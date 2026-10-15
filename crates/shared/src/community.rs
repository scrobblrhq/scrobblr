//! Community bios and tags: who may contribute, and the rules their text
//! follows (docs/social.md, Bios and Tags). Pure.

use thiserror::Error;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::validation::is_unsafe_text_char;

/// How old an account must be to edit a bio, and before its tags weigh in
/// the tag aggregates: the rankings' default.
pub const MIN_ACCOUNT_DAYS: i64 = 7;
/// The coordination factors that count: the detection's default window.
pub const FACTOR_WINDOW_DAYS: i64 = 28;
/// The lowest coordination factor over [`FACTOR_WINDOW_DAYS`], in
/// thousandths, an account may edit bios with: below it the detection is
/// more sure than not that the account is part of a farm.
pub const MIN_EDITOR_FACTOR: i64 = 500;

pub const BIO_MAX_CHARS: usize = 10_000;
pub const SUMMARY_MAX_CHARS: usize = 200;
pub const NOTE_MAX_CHARS: usize = 500;
/// Links (`http://`, `https://` or `www.`) a bio may hold.
pub const MAX_LINKS: usize = 5;
/// A link host appearing more often than this in one bio holds the edit.
pub const MAX_SAME_HOST: usize = 2;
/// Shorter texts are too often alike (one-line stubs) to call a copy spam.
pub const DUPLICATE_MIN_CHARS: usize = 200;

/// Revisions an account may write in 24 hours, and on one bio.
pub const EDITS_PER_DAY: i64 = 50;
pub const EDITS_PER_BIO_PER_DAY: i64 = 3;

pub const TAG_MAX_CHARS: usize = 40;
/// Tags an account may put on one artist, album or track.
pub const MAX_TAGS_PER_ENTITY: i64 = 10;
/// Distinct tags one artist, album or track may carry.
pub const MAX_ENTITY_TAGS: i64 = 500;
/// Taggings an account may hold.
pub const MAX_TAGGINGS: i64 = 50_000;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TextError {
    #[error("the text is empty")]
    Empty,
    #[error("the text may be at most {0} characters")]
    TooLong(usize),
    #[error("the text holds control, invisible or bidirectional formatting characters")]
    Unsafe,
    #[error("a bio may hold at most {MAX_LINKS} links")]
    TooManyLinks,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TagError {
    #[error("a tag may be 1 to {TAG_MAX_CHARS} characters")]
    Length,
    #[error(
        "a tag holds letters, digits, spaces and - & ' . + ! _ only, and at least one letter or digit"
    )]
    Charset,
    #[error("the tag `{0}` isn't allowed")]
    Blocked(String),
}

fn unsafe_in_bio(c: char) -> bool {
    // Joiners shape many scripts and emoji; bidi marks and isolates don't.
    (is_unsafe_text_char(c) && !matches!(c, '\n' | '\u{200C}' | '\u{200D}'))
        || matches!(
            c,
            '\u{061C}' | '\u{180E}' | '\u{2028}' | '\u{2029}' | '\u{FFF9}'
                ..='\u{FFFB}' | '\u{FFFE}' | '\u{FFFF}'
        )
}

/// A bio as stored: line breaks as `\n`, tabs as spaces, no trailing space
/// on a line, at most one blank line in a row, trimmed. Plain text: no
/// markup is interpreted, and clients show it as text, with paragraphs at
/// blank lines and `http(s)://` URLs as links.
pub fn bio_text(input: &str) -> Result<String, TextError> {
    let text = input.replace("\r\n", "\n").replace(['\r', '\t'], " ");
    let mut lines: Vec<&str> = Vec::new();
    for line in text.split('\n').map(str::trim_end) {
        if line.is_empty() && lines.last().is_some_and(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    let text = lines.join("\n").trim().to_string();
    if text.is_empty() {
        return Err(TextError::Empty);
    }
    if text.chars().count() > BIO_MAX_CHARS {
        return Err(TextError::TooLong(BIO_MAX_CHARS));
    }
    if text.chars().any(unsafe_in_bio) {
        return Err(TextError::Unsafe);
    }
    if link_hosts(&text).len() > MAX_LINKS {
        return Err(TextError::TooManyLinks);
    }
    Ok(text)
}

/// An edit summary or a report's note: one line, whitespace collapsed,
/// possibly empty.
pub fn line(input: &str, max: usize) -> Result<String, TextError> {
    let text = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > max {
        return Err(TextError::TooLong(max));
    }
    if text.chars().any(unsafe_in_bio) {
        return Err(TextError::Unsafe);
    }
    Ok(text)
}

/// The host of every link in `text`, lowercased and without `www.`, once
/// per link: `http://` and `https://` URLs, and `www.` names without a
/// scheme, which clients don't make links of but spam still uses.
pub fn link_hosts(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut hosts = Vec::new();
    let mut rest = lower.as_str();
    while let Some((start, scheme)) = ["http://", "https://", "www."]
        .iter()
        .filter_map(|p| rest.find(p).map(|i| (i, *p)))
        .min()
    {
        let bare = scheme == "www.";
        let starts_word = rest[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '.');
        let from = if bare { start } else { start + scheme.len() };
        let len = rest[from..]
            .find(|c: char| !(c.is_alphanumeric() || matches!(c, '.' | '-' | '_')))
            .unwrap_or(rest.len() - from);
        let host = rest[from..from + len]
            .trim_start_matches("www.")
            .trim_end_matches('.');
        if (!bare || starts_word) && host.contains('.') {
            hosts.push(host.to_string());
        }
        rest = &rest[from + len..];
    }
    hosts
}

/// The host linked more than [`MAX_SAME_HOST`] times, if any.
pub fn repeated_host(hosts: &[String]) -> Option<&str> {
    hosts
        .iter()
        .find(|h| hosts.iter().filter(|o| o == h).count() > MAX_SAME_HOST)
        .map(String::as_str)
}

/// A tag as stored and shown: compatibility-normalized (NFKC), lowercase,
/// whitespace collapsed to single spaces.
pub fn tag_name(input: &str) -> Result<String, TagError> {
    let name = input
        .nfkc()
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let chars = name.chars().count();
    if chars == 0 || chars > TAG_MAX_CHARS {
        return Err(TagError::Length);
    }
    let allowed = |c: char| {
        c.is_alphanumeric()
            || is_combining_mark(c)
            || matches!(c, ' ' | '-' | '&' | '\'' | '.' | '+' | '!' | '_')
    };
    if !name.chars().all(allowed) || !name.chars().any(char::is_alphanumeric) {
        return Err(TagError::Charset);
    }
    Ok(name)
}

fn words(name: &str) -> Vec<&str> {
    name.split(|c: char| !c.is_alphanumeric() && !is_combining_mark(c))
        .filter(|w| !w.is_empty())
        .collect()
}

/// Whether a blocklist term (normalized as a tag) matches `tag`: the same
/// words in a row, so `bad` blocks `bad`, `so-bad` and `bad tag` but not
/// `badminton`.
pub fn is_blocked(tag: &str, terms: &[String]) -> bool {
    let tag = words(tag);
    terms.iter().any(|term| {
        let term = words(term);
        !term.is_empty() && tag.windows(term.len()).any(|w| w == term.as_slice())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bio_text_is_tidied() {
        assert_eq!(
            bio_text("  First line.  \r\n\r\n\r\n\tSecond\r\nthird  \n\n").unwrap(),
            "First line.\n\n Second\nthird"
        );
        assert_eq!(bio_text(" \n\t ").unwrap_err(), TextError::Empty);
        assert!(bio_text(&"a".repeat(BIO_MAX_CHARS)).is_ok());
        assert_eq!(
            bio_text(&"é".repeat(BIO_MAX_CHARS + 1)).unwrap_err(),
            TextError::TooLong(BIO_MAX_CHARS)
        );
    }

    #[test]
    fn bio_text_refuses_control_and_bidi_characters() {
        for bad in [
            "nul\u{0}",
            "bell\u{7}",
            "esc\u{1b}[31m",
            "c1\u{85}x",
            "evil\u{202E}txt.exe",
            "isolate\u{2066}x\u{2069}",
            "mark\u{200F}",
            "arabic mark\u{061C}",
            "zero\u{200B}width",
            "bom\u{FEFF}",
            "line\u{2028}sep",
        ] {
            assert_eq!(bio_text(bad).unwrap_err(), TextError::Unsafe, "{bad:?}");
        }
        // Joiners are part of scripts and emoji sequences.
        for good in [
            "می\u{200C}خواهم",
            "family 👨\u{200D}👩\u{200D}👧",
            "Björk — Ísland",
        ] {
            assert!(bio_text(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn links_are_counted_by_host() {
        let text = "See https://Example.com/a?b=1, http://www.example.com and \
                    www.other.org/x; not mail@www.nope or nowww.fake.com or https://localhost";
        assert_eq!(
            link_hosts(text),
            ["example.com", "example.com", "other.org"]
        );
        assert_eq!(repeated_host(&link_hosts(text)), None);
        let spam = "https://a.example x https://a.example y https://a.example";
        assert_eq!(repeated_host(&link_hosts(spam)), Some("a.example"));
        let many = (0..=MAX_LINKS)
            .map(|i| format!("https://site{i}.example"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(bio_text(&many).unwrap_err(), TextError::TooManyLinks);
    }

    #[test]
    fn lines_collapse_whitespace() {
        assert_eq!(
            line("  fixed\n the   dates ", 200).unwrap(),
            "fixed the dates"
        );
        assert_eq!(line("", 200).unwrap(), "");
        assert_eq!(line("abc", 2).unwrap_err(), TextError::TooLong(2));
        assert_eq!(line("x\u{202E}", 200).unwrap_err(), TextError::Unsafe);
    }

    #[test]
    fn tags_are_normalized() {
        for (input, expected) in [
            ("  Post \n  Rock ", "post rock"),
            ("HIP-HOP", "hip-hop"),
            ("Ｊ－ｐｏｐ", "j-pop"),
            ("ﬁlm score", "film score"),
            ("Rock 'n' Roll", "rock 'n' roll"),
            ("R&B", "r&b"),
            ("ÉLECTRO", "électro"),
            ("E\u{0301}lectro", "électro"),
            ("हिंदी", "हिंदी"),
            ("80s", "80s"),
            ("k-pop!", "k-pop!"),
        ] {
            assert_eq!(tag_name(input).unwrap(), expected, "{input}");
        }
        assert_eq!(tag_name("ÉLECTRO"), tag_name("E\u{0301}lectro"));
    }

    #[test]
    fn tags_refuse_bad_input() {
        let long = "a".repeat(TAG_MAX_CHARS + 1);
        for (input, err) in [
            ("", TagError::Length),
            ("   ", TagError::Length),
            (long.as_str(), TagError::Length),
            ("rock, pop", TagError::Charset),
            ("<b>", TagError::Charset),
            ("a/b", TagError::Charset),
            ("--", TagError::Charset),
            ("zero\u{200B}width", TagError::Charset),
            ("bidi\u{202E}", TagError::Charset),
            ("nul\u{0}", TagError::Charset),
            ("emoji 🎸", TagError::Charset),
        ] {
            assert_eq!(tag_name(input).unwrap_err(), err, "{input:?}");
        }
        assert!(tag_name(&"é".repeat(TAG_MAX_CHARS)).is_ok());
    }

    #[test]
    fn blocklist_matches_whole_words() {
        let terms = vec!["bad".to_string(), "buy pills".to_string()];
        for tag in ["bad", "so-bad", "bad tag", "cheap buy pills now"] {
            assert!(is_blocked(tag, &terms), "{tag}");
        }
        for tag in ["badminton", "buy", "pills buy"] {
            assert!(!is_blocked(tag, &terms), "{tag}");
        }
        assert!(!is_blocked("bad", &[String::new()]));
    }
}
