use thiserror::Error;

pub const USERNAME_MIN_LEN: usize = 2;
pub const USERNAME_MAX_LEN: usize = 30;
pub const EMAIL_MAX_LEN: usize = 254;
pub const PASSWORD_MIN_LEN: usize = 12;
pub const PASSWORD_MAX_LEN: usize = 128;
pub const DISPLAY_NAME_MAX_LEN: usize = 40;

/// Upper bound on a password accepted at *login*. Far above
/// [`PASSWORD_MAX_LEN`] so nobody who registered before that cap is locked
/// out, but low enough that Argon2 can't be fed a multi-megabyte body.
pub const PASSWORD_LOGIN_MAX_LEN: usize = 1024;

const USERNAME_SEPARATORS: [char; 3] = ['_', '-', '.'];
const LOCAL_PART_SPECIALS: &str = "!#$%&'*+/=?^_`{|}~-";

const COMMON_PASSWORDS: &[&str] = &[
    "password",
    "passw0rd",
    "password1",
    "qwertyuiop",
    "1234567890",
    "12345678910",
    "letmein123",
    "iloveyou123",
    "administrator",
    "welcome123",
    "changeme123",
    "scrobblr123",
];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("username must be between {USERNAME_MIN_LEN} and {USERNAME_MAX_LEN} characters")]
    UsernameLength,
    #[error("username may only contain letters, digits, and the separators _ - .")]
    UsernameCharset,
    #[error("username must start and end with a letter or digit")]
    UsernameBoundary,
    #[error("username may not contain repeated separators")]
    UsernameRepeatedSeparator,
    #[error("username is reserved")]
    UsernameReserved,

    #[error("email address is not valid")]
    EmailInvalid,
    #[error("email address may be at most {EMAIL_MAX_LEN} characters")]
    EmailLength,

    #[error("password must be at least {PASSWORD_MIN_LEN} characters")]
    PasswordTooShort,
    #[error("password may be at most {PASSWORD_MAX_LEN} characters")]
    PasswordTooLong,
    #[error("password must combine at least three of: lowercase, uppercase, digits, symbols")]
    PasswordTooSimple,
    #[error("password may not contain control characters")]
    PasswordControlChars,
    #[error("password may not contain your username or email address")]
    PasswordContainsIdentity,
    #[error("password is too common")]
    PasswordCommon,

    #[error("display name may be at most {DISPLAY_NAME_MAX_LEN} characters")]
    DisplayNameLength,
    #[error("display name contains disallowed characters")]
    DisplayNameCharset,
}

/// Rejects anything that renders ambiguously or can break a client: C0/C1
/// controls, zero-width joiners and the bidirectional overrides that let a
/// name display as something other than what is stored.
fn is_unsafe_text_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}')
}

pub fn normalize_username(raw: &str) -> &str {
    raw.trim()
}

pub fn validate_username(raw: &str, reserved: &[&str]) -> Result<(), ValidationError> {
    let username = normalize_username(raw);
    let len = username.chars().count();

    if !(USERNAME_MIN_LEN..=USERNAME_MAX_LEN).contains(&len) {
        return Err(ValidationError::UsernameLength);
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || USERNAME_SEPARATORS.contains(&c))
    {
        return Err(ValidationError::UsernameCharset);
    }

    let first = username.chars().next().unwrap_or_default();
    let last = username.chars().next_back().unwrap_or_default();
    if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
        return Err(ValidationError::UsernameBoundary);
    }

    let is_separator = |b: &u8| USERNAME_SEPARATORS.contains(&(*b as char));
    if username
        .as_bytes()
        .windows(2)
        .any(|w| is_separator(&w[0]) && is_separator(&w[1]))
    {
        return Err(ValidationError::UsernameRepeatedSeparator);
    }

    let lowered = username.to_ascii_lowercase();
    if reserved.iter().any(|r| r.eq_ignore_ascii_case(&lowered)) {
        return Err(ValidationError::UsernameReserved);
    }

    Ok(())
}

pub fn normalize_email(raw: &str) -> String {
    raw.trim().to_lowercase()
}

pub fn validate_email(raw: &str) -> Result<String, ValidationError> {
    let email = normalize_email(raw);

    if email.len() > EMAIL_MAX_LEN {
        return Err(ValidationError::EmailLength);
    }
    if email.chars().any(is_unsafe_text_char) {
        return Err(ValidationError::EmailInvalid);
    }

    let (local, domain) = email.split_once('@').ok_or(ValidationError::EmailInvalid)?;
    if domain.contains('@') {
        return Err(ValidationError::EmailInvalid);
    }

    validate_email_local(local)?;
    validate_email_domain(domain)?;

    Ok(email)
}

fn validate_email_local(local: &str) -> Result<(), ValidationError> {
    if local.is_empty() || local.len() > 64 {
        return Err(ValidationError::EmailInvalid);
    }
    if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return Err(ValidationError::EmailInvalid);
    }
    if !local
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || LOCAL_PART_SPECIALS.contains(c))
    {
        return Err(ValidationError::EmailInvalid);
    }
    Ok(())
}

fn validate_email_domain(domain: &str) -> Result<(), ValidationError> {
    if domain.is_empty() || domain.len() > 253 {
        return Err(ValidationError::EmailInvalid);
    }

    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return Err(ValidationError::EmailInvalid);
    }

    for label in &labels {
        if label.is_empty() || label.len() > 63 {
            return Err(ValidationError::EmailInvalid);
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(ValidationError::EmailInvalid);
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(ValidationError::EmailInvalid);
        }
    }

    let tld = labels[labels.len() - 1];
    if tld.len() < 2 || !tld.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(ValidationError::EmailInvalid);
    }

    Ok(())
}

pub fn validate_password(
    password: &str,
    username: &str,
    email: &str,
) -> Result<(), ValidationError> {
    let len = password.chars().count();
    if len < PASSWORD_MIN_LEN {
        return Err(ValidationError::PasswordTooShort);
    }
    if len > PASSWORD_MAX_LEN {
        return Err(ValidationError::PasswordTooLong);
    }
    if password.chars().any(char::is_control) {
        return Err(ValidationError::PasswordControlChars);
    }

    let classes = [
        password.chars().any(char::is_lowercase),
        password.chars().any(char::is_uppercase),
        password.chars().any(|c| c.is_ascii_digit()),
        password
            .chars()
            .any(|c| !c.is_alphanumeric() && !c.is_whitespace()),
    ];
    if classes.iter().filter(|present| **present).count() < 3 {
        return Err(ValidationError::PasswordTooSimple);
    }

    let lowered = password.to_lowercase();
    if COMMON_PASSWORDS.contains(&lowered.as_str()) {
        return Err(ValidationError::PasswordCommon);
    }

    let username = username.trim().to_lowercase();
    if username.len() >= 3 && lowered.contains(&username) {
        return Err(ValidationError::PasswordContainsIdentity);
    }
    let email_local = email.split('@').next().unwrap_or_default().to_lowercase();
    if email_local.len() >= 3 && lowered.contains(&email_local) {
        return Err(ValidationError::PasswordContainsIdentity);
    }

    Ok(())
}

/// Trims and rejects unrenderable input, collapsing a blank result to
/// `None` so the column stays NULL instead of holding an empty string.
pub fn sanitize_display_name(raw: Option<&str>) -> Result<Option<String>, ValidationError> {
    let Some(value) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };

    if value.chars().count() > DISPLAY_NAME_MAX_LEN {
        return Err(ValidationError::DisplayNameLength);
    }
    if value.chars().any(is_unsafe_text_char) {
        return Err(ValidationError::DisplayNameCharset);
    }

    Ok(Some(value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESERVED: &[&str] = &["me", "admin"];

    #[test]
    fn accepts_ordinary_usernames() {
        for name in ["ab", "chewawi", "user_name", "a-b.c", "  padded  ", "X9"] {
            assert!(validate_username(name, RESERVED).is_ok(), "{name}");
        }
    }

    #[test]
    fn rejects_malformed_usernames() {
        let cases = [
            ("a", ValidationError::UsernameLength),
            ("", ValidationError::UsernameLength),
            (&"a".repeat(31), ValidationError::UsernameLength),
            ("usuário", ValidationError::UsernameCharset),
            ("with space", ValidationError::UsernameCharset),
            ("emoji🎵", ValidationError::UsernameCharset),
            ("path/traversal", ValidationError::UsernameCharset),
            ("_leading", ValidationError::UsernameBoundary),
            ("trailing.", ValidationError::UsernameBoundary),
            ("double__sep", ValidationError::UsernameRepeatedSeparator),
            ("ME", ValidationError::UsernameReserved),
        ];
        for (input, expected) in cases {
            assert_eq!(
                validate_username(input, RESERVED).unwrap_err(),
                expected,
                "{input}"
            );
        }
    }

    /// Zero-width and bidi characters are the ones that make a stored name
    /// render as something else entirely in a client.
    #[test]
    fn rejects_invisible_characters_in_usernames() {
        for name in ["ad\u{200B}min", "user\u{202E}eman", "a\u{FEFF}b"] {
            assert_eq!(
                validate_username(name, RESERVED).unwrap_err(),
                ValidationError::UsernameCharset
            );
        }
    }

    #[test]
    fn accepts_and_lowercases_valid_emails() {
        assert_eq!(
            validate_email("  User.Name+tag@Example.CO.UK  ").unwrap(),
            "user.name+tag@example.co.uk"
        );
        for email in ["a@b.io", "x_y@sub.domain.com", "n!#$%&'*+-/=?^_`{|}~@d.dev"] {
            assert!(validate_email(email).is_ok(), "{email}");
        }
    }

    #[test]
    fn rejects_malformed_emails() {
        for email in [
            "plainstring",
            "@nolocal.com",
            "nodomain@",
            "two@at@signs.com",
            "trailing.dot.@x.com",
            ".leading@x.com",
            "double..dot@x.com",
            "no@tld",
            "bad@-hyphen.com",
            "bad@hyphen-.com",
            "bad@domain..com",
            "short@tld.x",
            "digits@tld.12",
            "spaced out@x.com",
            "nul\u{0}byte@x.com",
        ] {
            assert!(validate_email(email).is_err(), "{email}");
        }
        assert_eq!(
            validate_email(&format!("{}@x.com", "a".repeat(300))).unwrap_err(),
            ValidationError::EmailLength
        );
    }

    #[test]
    fn accepts_strong_passwords() {
        assert!(validate_password("Tr0ub4dor&3xyz", "chewawi", "c@x.com").is_ok());
        assert!(validate_password("correct horse Battery 9", "chewawi", "c@x.com").is_ok());
    }

    #[test]
    fn rejects_weak_passwords() {
        let cases = [
            ("Sh0rt!", ValidationError::PasswordTooShort),
            ("alllowercaseletters", ValidationError::PasswordTooSimple),
            ("PASSWORD1234567890", ValidationError::PasswordTooSimple),
            ("password", ValidationError::PasswordTooShort),
            ("Password1234\u{0}", ValidationError::PasswordControlChars),
        ];
        for (input, expected) in cases {
            assert_eq!(
                validate_password(input, "chewawi", "c@x.com").unwrap_err(),
                expected,
                "{input}"
            );
        }

        assert_eq!(
            validate_password(&format!("Aa1!{}", "x".repeat(200)), "chewawi", "c@x.com")
                .unwrap_err(),
            ValidationError::PasswordTooLong
        );
        assert_eq!(
            validate_password("Passw0rd", "chewawi", "c@x.com").unwrap_err(),
            ValidationError::PasswordTooShort
        );
        assert_eq!(
            validate_password("Chewawi_2026!", "chewawi", "c@x.com").unwrap_err(),
            ValidationError::PasswordContainsIdentity
        );
        assert_eq!(
            validate_password("Rafael_2026!x", "user", "rafael@x.com").unwrap_err(),
            ValidationError::PasswordContainsIdentity
        );
        assert_eq!(
            validate_password("Password1!", "user", "u@x.com").unwrap_err(),
            ValidationError::PasswordTooShort
        );
    }

    #[test]
    fn sanitizes_display_names() {
        assert_eq!(sanitize_display_name(None).unwrap(), None);
        assert_eq!(sanitize_display_name(Some("   ")).unwrap(), None);
        assert_eq!(
            sanitize_display_name(Some("  Chewawi  ")).unwrap(),
            Some("Chewawi".into())
        );
        // Unicode is fine in a display name; only unrenderable input is not.
        assert!(sanitize_display_name(Some("Sébastien 🎵")).is_ok());
        assert_eq!(
            sanitize_display_name(Some("spoof\u{202E}eman")).unwrap_err(),
            ValidationError::DisplayNameCharset
        );
        assert_eq!(
            sanitize_display_name(Some(&"a".repeat(41))).unwrap_err(),
            ValidationError::DisplayNameLength
        );
    }
}
