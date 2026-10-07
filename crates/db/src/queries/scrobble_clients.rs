//! The clients scrobbles arrive from (`scrobble_clients`, migration 0013).

use sqlx::PgPool;

/// Longest client name kept; the rest is client-supplied noise.
pub const MAX_NAME_CHARS: usize = 100;

pub const PROTOCOL_SCROBBLR: &str = "scrobblr";
pub const PROTOCOL_LASTFM: &str = "lastfm";
pub const PROTOCOL_AUDIOSCROBBLER: &str = "audioscrobbler";
pub const PROTOCOL_LISTENBRAINZ: &str = "listenbrainz";
pub const PROTOCOL_SPOTIFY: &str = "spotify";

/// A client as its protocol identifies it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientIdentity {
    pub protocol: &'static str,
    pub name: String,
    pub verified: bool,
}

impl ClientIdentity {
    pub fn new(protocol: &'static str, name: &str, verified: bool) -> Self {
        let name: String = name.trim().chars().take(MAX_NAME_CHARS).collect();
        Self {
            protocol,
            name: if name.is_empty() {
                "unknown".into()
            } else {
                name
            },
            verified,
        }
    }
}

/// The client's id, registering it on first sight.
pub async fn resolve_client(pool: &PgPool, client: &ClientIdentity) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        WITH inserted AS (
            INSERT INTO scrobble_clients (protocol, name, verified)
            VALUES ($1, $2, $3)
            ON CONFLICT (protocol, name, verified) DO NOTHING
            RETURNING id
        )
        SELECT id AS "id!" FROM inserted
        UNION ALL
        SELECT id FROM scrobble_clients WHERE protocol = $1 AND name = $2 AND verified = $3
        LIMIT 1
        "#,
        client.protocol,
        client.name,
        client.verified,
    )
    .fetch_one(pool)
    .await
}
