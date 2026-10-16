//! The client guides in `docs/api/`, built into the OpenAPI spec so the
//! docs page shows them: the introduction as the spec's description, every
//! other guide as its tag's, and the tags grouped for the sidebar
//! (`x-tagGroups`).

use aide::openapi::{OpenApi, Tag};
use serde_json::json;

macro_rules! guide {
    ($file:literal) => {
        Guide {
            file: $file,
            text: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../docs/api/",
                $file
            )),
        }
    };
}

#[derive(Clone, Copy)]
struct Guide {
    #[cfg_attr(not(test), allow(dead_code))]
    file: &'static str,
    text: &'static str,
}

impl Guide {
    /// The text without its title, which the page shows already.
    fn body(&self) -> &'static str {
        match self.text.strip_prefix("# ") {
            Some(rest) => rest
                .split_once('\n')
                .map_or("", |(_, body)| body.trim_start()),
            None => self.text,
        }
    }
}

const INTRODUCTION: Guide = guide!("introduction.md");

/// Every tag an operation has, by sidebar group, with its guide.
const GROUPS: &[(&str, &[(&str, Guide)])] = &[
    (
        "Accounts",
        &[
            ("Auth", guide!("auth.md")),
            ("Users", guide!("users.md")),
            ("Account", guide!("account.md")),
            ("Connected accounts", guide!("connected-accounts.md")),
            ("Imports", guide!("imports.md")),
        ],
    ),
    (
        "Scrobbling",
        &[
            ("Scrobbling", guide!("scrobbling.md")),
            ("Scrobblers", guide!("scrobblers.md")),
            ("Scrobbler protocols", guide!("scrobbler-protocols.md")),
        ],
    ),
    (
        "Listening",
        &[
            ("Scrobbles", guide!("scrobbles.md")),
            ("Charts", guide!("charts.md")),
            ("Loved tracks", guide!("loved-tracks.md")),
        ],
    ),
    (
        "People",
        &[
            ("Social", guide!("social.md")),
            ("Recommendations", guide!("recommendations.md")),
        ],
    ),
    (
        "Catalog",
        &[
            ("Catalog", guide!("catalog.md")),
            ("Images", guide!("images.md")),
            ("Bios", guide!("bios.md")),
            ("Tags", guide!("tags.md")),
            ("Rankings", guide!("rankings.md")),
        ],
    ),
];

/// Puts the guides into `api`.
pub fn document(api: &mut OpenApi) {
    api.info.description = Some(INTRODUCTION.body().into());
    api.tags = GROUPS
        .iter()
        .flat_map(|(_, tags)| tags.iter())
        .map(|(name, guide)| Tag {
            name: (*name).into(),
            description: Some(guide.body().into()),
            ..Default::default()
        })
        .collect();
    let groups: Vec<_> = GROUPS
        .iter()
        .map(|(group, tags)| json!({ "name": group, "tags": tags.iter().map(|(name, _)| name).collect::<Vec<_>>() }))
        .collect();
    api.extensions.insert("x-tagGroups".into(), json!(groups));
}

/// Every guide's file name and text, the introduction first.
#[cfg(test)]
pub fn texts() -> impl Iterator<Item = (&'static str, &'static str)> {
    std::iter::once(INTRODUCTION)
        .chain(
            GROUPS
                .iter()
                .flat_map(|(_, tags)| tags.iter().map(|(_, guide)| *guide)),
        )
        .map(|guide| (guide.file, guide.text))
}

/// The tags `document` describes, in order.
#[cfg(test)]
pub fn tags() -> impl Iterator<Item = &'static str> {
    GROUPS
        .iter()
        .flat_map(|(_, tags)| tags.iter().map(|(name, _)| *name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_guide_in_docs_api_is_built_in_once() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/api");
        let mut files: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.ends_with(".md"))
            .collect();
        files.sort();
        let mut built: Vec<String> = texts().map(|(file, _)| file.to_string()).collect();
        built.sort();
        assert_eq!(files, built, "docs/api and guides::GROUPS disagree");
    }

    /// A relative link works on GitHub, not on the docs page.
    #[test]
    fn guides_link_only_to_absolute_urls() {
        for (file, text) in texts() {
            for (at, _) in text.match_indices("](") {
                let target = &text[at + 2..];
                let target = &target[..target.find(')').unwrap_or(target.len())];
                assert!(
                    target.starts_with("https://") || target.starts_with("http://"),
                    "{file}: a link to {target}"
                );
            }
        }
    }

    #[test]
    fn json_examples_parse() {
        for (file, text) in texts() {
            for block in text.split("```json\n").skip(1) {
                let json = block.split("\n```").next().unwrap();
                if let Err(e) = serde_json::from_str::<serde_json::Value>(json) {
                    panic!("{file}: {e}\n{json}");
                }
            }
        }
    }
}
