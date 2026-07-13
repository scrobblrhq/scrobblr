use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

/// User
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, JsonSchema, TS)]
#[ts(export)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub image_url: Option<String>,
    pub website_url: Option<String>,
    pub country: Option<String>,
    pub scrobble_count: i64,
    pub is_private: bool,
    pub is_verified: bool,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Public-facing profile (no sensitive fields)
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct UserProfile {
    pub id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub image_url: Option<String>,
    pub website_url: Option<String>,
    pub country: Option<String>,
    pub scrobble_count: i64,
    pub is_private: bool,
    pub is_verified: bool,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl From<User> for UserProfile {
    fn from(u: User) -> Self {
        Self {
            id: u.id,
            username: u.username,
            display_name: u.display_name,
            bio: u.bio,
            image_url: u.image_url,
            website_url: u.website_url,
            country: u.country,
            scrobble_count: u.scrobble_count,
            is_private: u.is_private,
            is_verified: u.is_verified,
            last_seen_at: u.last_seen_at,
            created_at: u.created_at,
        }
    }
}

/// Artist
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, JsonSchema, TS)]
#[ts(export)]
pub struct Artist {
    pub id: i64,
    pub name: String,
    pub name_normalized: String,
    pub mbid: Option<Uuid>,
    pub image_url: Option<String>,
    pub bio: Option<String>,
    pub scrobble_count: i64,
    pub listener_count: i64,
    pub created_at: DateTime<Utc>,
}

/// Album
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, JsonSchema, TS)]
#[ts(export)]
pub struct Album {
    pub id: i64,
    pub artist_id: i64,
    pub title: String,
    pub title_normalized: String,
    pub mbid: Option<Uuid>,
    pub image_url: Option<String>,
    pub release_date: Option<chrono::NaiveDate>,
    pub scrobble_count: i64,
    pub created_at: DateTime<Utc>,
}

/// Track
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, JsonSchema, TS)]
#[ts(export)]
pub struct Track {
    pub id: i64,
    pub artist_id: i64,
    pub album_id: Option<i64>,
    pub title: String,
    pub title_normalized: String,
    pub mbid: Option<Uuid>,
    pub duration_ms: Option<i32>,
    pub scrobble_count: i64,
    pub created_at: DateTime<Utc>,
}

///  Scrobble
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, JsonSchema, TS)]
#[ts(export)]
pub struct Scrobble {
    pub id: i64,
    pub user_id: i64,
    pub track_id: i64,
    pub artist_id: i64,
    pub album_id: Option<i64>,
    pub played_at: DateTime<Utc>,
    pub source: String,
    pub duration_ms: Option<i32>,
}

/// Rich scrobble for API responses (joined data)
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, JsonSchema, TS)]
#[ts(export)]
pub struct ScrobbleRich {
    pub id: i64,
    pub played_at: DateTime<Utc>,
    pub source: String,
    pub track_id: i64,
    pub track_title: String,
    pub artist_id: i64,
    pub artist_name: String,
    pub album_id: Option<i64>,
    pub album_title: Option<String>,
    pub album_image: Option<String>,
    pub duration_ms: Option<i32>,
}

/// NowPlaying
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, JsonSchema, TS)]
#[ts(export)]
pub struct NowPlaying {
    pub user_id: i64,
    pub track_id: i64,
    pub artist_id: i64,
    pub album_id: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub source: String,
}

/*
    Authentication
*/

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, JsonSchema, TS)]
#[ts(export)]
pub struct UserSession {
    pub id: Uuid,
    pub user_id: i64,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, JsonSchema, TS)]
#[ts(export)]
pub struct ApiToken {
    pub id: Uuid,
    pub user_id: i64,
    pub name: String,
    #[serde(skip_serializing)]
    pub token_hash: String,
    pub scopes: Vec<String>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/*
    Stats
*/

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct TopArtist {
    pub artist_id: i64,
    pub artist_name: String,
    pub image_url: Option<String>,
    pub play_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct TopTrack {
    pub track_id: i64,
    pub track_title: String,
    pub artist_id: i64,
    pub artist_name: String,
    pub album_image: Option<String>,
    pub play_count: i64,
}

/// A user who listens to a given artist or track, with their play count.
/// Only public profiles are ever returned.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct TopListener {
    pub user_id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub image_url: Option<String>,
    pub play_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct ActivityDay {
    pub day: DateTime<Utc>,
    pub scrobble_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct NowPlayingRich {
    pub track_title: String,
    pub artist_name: String,
    pub album_title: Option<String>,
    pub album_image: Option<String>,
    /// Fallback artwork for clients when the album has no cover (tracks have
    /// no artwork of their own — the album cover is the track's image).
    pub artist_image: Option<String>,
    pub started_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub source: String,
}

/// A community-uploaded image candidate for an artist or album. Uploads add
/// candidates rather than replacing the image; the most-liked candidate
/// (once it clears the vote threshold) becomes the entity's displayed image.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct ImageCandidate {
    pub id: i64,
    pub url: String,
    pub uploaded_by: String,
    pub vote_count: i64,
    /// Whether the requesting user has liked this candidate (false when
    /// unauthenticated).
    pub has_voted: bool,
    /// Whether this candidate is the entity's current displayed image.
    pub is_default: bool,
    pub created_at: DateTime<Utc>,
}

/// A user comment on a catalog entity, joined with the commenter's identity.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct Comment {
    pub id: i64,
    pub user_id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub image_url: Option<String>,
    pub body: String,
    pub created_at: DateTime<Utc>,
}
