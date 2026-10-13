//! Conditional requests: weak `ETag`s compared against `If-None-Match`,
//! and how routes that send them document it.

use aide::openapi::{Header, ParameterSchemaOrContent, ReferenceOr, SchemaObject};
use aide::transform::{TransformOperation, TransformResponse};
use axum::http::HeaderValue;

/// `If-None-Match` holds `*` or a list of tags, compared weakly.
pub fn matches(tags: &HeaderValue, etag: &str) -> bool {
    let Ok(tags) = tags.to_str() else {
        return false;
    };
    let opaque = |tag: &str| tag.trim().trim_start_matches("W/").to_owned();
    tags.trim() == "*" || tags.split(',').any(|tag| opaque(tag) == opaque(etag))
}

/// Documents the `If-None-Match` request header.
pub fn if_none_match<'t>(
    mut op: TransformOperation<'t>,
    description: &str,
) -> TransformOperation<'t> {
    op.inner_mut()
        .parameters
        .push(ReferenceOr::Item(aide::openapi::Parameter::Header {
            parameter_data: aide::openapi::ParameterData {
                name: "If-None-Match".into(),
                description: Some(description.into()),
                required: false,
                deprecated: None,
                format: string_schema(),
                example: None,
                examples: Default::default(),
                explode: None,
                extensions: Default::default(),
            },
            style: Default::default(),
        }));
    op
}

/// Documents a response's `ETag` and `Cache-Control` headers.
pub fn headers<'t, T>(
    r: TransformResponse<'t, T>,
    etag: &str,
    cache_control: &str,
) -> TransformResponse<'t, T> {
    let r = header(r, "ETag", etag.to_owned());
    header(r, "Cache-Control", format!("`{cache_control}`"))
}

/// Documents a response's `Cache-Control` header.
pub fn cache_control<'t, T>(r: TransformResponse<'t, T>, value: &str) -> TransformResponse<'t, T> {
    header(r, "Cache-Control", format!("`{value}`"))
}

fn header<'t, T>(
    mut r: TransformResponse<'t, T>,
    name: &str,
    description: String,
) -> TransformResponse<'t, T> {
    r.inner().headers.insert(
        name.into(),
        ReferenceOr::Item(Header {
            description: Some(description),
            style: Default::default(),
            required: true,
            deprecated: None,
            format: string_schema(),
            example: None,
            examples: Default::default(),
            extensions: Default::default(),
        }),
    );
    r
}

fn string_schema() -> ParameterSchemaOrContent {
    ParameterSchemaOrContent::Schema(SchemaObject {
        json_schema: schemars::json_schema!({ "type": "string" }),
        external_docs: None,
        example: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_none_match_compares_tags_weakly() {
        let etag = "W/\"r1-1-5-f\"";
        for header in [
            "W/\"r1-1-5-f\"",
            "\"r1-1-5-f\"",
            "\"x\", W/\"r1-1-5-f\"",
            "*",
        ] {
            assert!(matches(&HeaderValue::from_static(header), etag), "{header}");
        }
        for header in ["W/\"r1-1-5-s\"", "\"r1-1-5\"", ""] {
            assert!(
                !matches(&HeaderValue::from_static(header), etag),
                "{header}"
            );
        }
    }
}
