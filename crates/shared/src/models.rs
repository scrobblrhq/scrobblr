use chrono::{DateTime, NaiveDate, Utc};
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
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub image_url: Option<String>,
    pub website_url: Option<String>,
    pub country: Option<String>,
    pub scrobble_count: i64,
    pub is_private: bool,
    pub is_verified: bool,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Set while the account waits to be erased; it is hidden meanwhile.
    #[serde(skip_serializing)]
    #[ts(skip)]
    pub delete_after: Option<DateTime<Utc>>,
}

/// Public-facing profile (no sensitive fields)
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct UserProfile {
    pub id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub image_url: Option<String>,
    pub website_url: Option<String>,
    pub country: Option<String>,
    pub scrobble_count: i64,
    pub is_private: bool,
    pub is_verified: bool,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// How a profile's `scrobble_count` divides by what the classifier made of
/// it: plays it found plausible, and plays it couldn't vouch for, so a flood
/// of botted plays doesn't pass for listening. The duplicates it found (the
/// same listen reported again) are in no count but their own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct ScrobbleBreakdown {
    /// Labelled `counted`.
    pub verified: i64,
    /// `suspect + no_data + pending`; `verified + unverified` is
    /// `scrobble_count`.
    pub unverified: i64,
    /// Over the listening time real time allows.
    pub suspect: i64,
    /// Of tracks with no known length, so unchecked.
    pub no_data: i64,
    /// Not classified yet (usually the last minute's).
    pub pending: i64,
    pub duplicates: i64,
}

/// An account in someone's followers or following.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct FollowEntry {
    #[serde(flatten)]
    #[ts(flatten)]
    pub user: UserProfile,
    /// When the follow began.
    pub followed_at: DateTime<Utc>,
    /// Whether the signed-in viewer follows this account; `null` when
    /// signed out, or for the viewer's own entry.
    pub is_following: Option<bool>,
    /// Whether this account follows the signed-in viewer; `null` likewise.
    pub follows_you: Option<bool>,
}

/// A page of followers or following, newest follow first.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct FollowPage {
    pub entries: Vec<FollowEntry>,
    /// `before` for the next page; `null` on the last one.
    pub next: Option<String>,
}

/// An account the user blocks. A private account shows its username only.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct BlockEntry {
    #[serde(flatten)]
    #[ts(flatten)]
    pub user: UserSummary,
    pub blocked_at: DateTime<Utc>,
}

/// A page of the accounts the user blocks, newest block first.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct BlockPage {
    pub entries: Vec<BlockEntry>,
    /// `before` for the next page; `null` on the last one.
    pub next: Option<String>,
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
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub image_url: Option<String>,
    pub bio: Option<String>,
    /// `null` unless at least `RANKING_MIN_LISTENERS` (5) users played it.
    pub scrobble_count: Option<i64>,
    /// Never computed: always `null`.
    pub listener_count: Option<i64>,
    pub created_at: DateTime<Utc>,
}

/// Album. Its tracks are those the catalog files under it (each track has
/// one album, the one the accounts playing it named, decided as
/// docs/social.md says), and their plays are its plays.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, JsonSchema, TS)]
#[ts(export)]
pub struct Album {
    pub id: i64,
    pub artist_id: i64,
    pub artist_name: String,
    pub title: String,
    pub title_normalized: String,
    pub mbid: Option<Uuid>,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub image_url: Option<String>,
    pub release_date: Option<chrono::NaiveDate>,
    /// Plays of its tracks; `null` unless at least `RANKING_MIN_LISTENERS`
    /// (5) users played them.
    pub scrobble_count: Option<i64>,
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
    /// `null` unless at least `RANKING_MIN_LISTENERS` (5) users played it.
    pub scrobble_count: Option<i64>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS, sqlx::Type)]
#[sqlx(type_name = "track_artist_role", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum TrackArtistRole {
    Primary,
    Featured,
}

/// One artist credited on a track. The `primary` credit always mirrors
/// `tracks.artist_id`; everything else is a featured collaborator.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, JsonSchema, TS)]
#[ts(export)]
pub struct TrackCredit {
    pub artist_id: i64,
    pub name: String,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub image_url: Option<String>,
    pub role: TrackArtistRole,
    pub position: i32,
}

/// A track together with its full credit list.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct TrackWithCredits {
    #[serde(flatten)]
    #[ts(flatten)]
    pub track: Track,
    pub artists: Vec<TrackCredit>,
    /// Whether the signed-in viewer loves the track; `null` when signed out.
    pub loved: Option<bool>,
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
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
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub album_image: Option<String>,
    pub duration_ms: Option<i32>,
    /// What the classifier made of it; `null` until it is classified.
    pub status: Option<ScrobbleLabel>,
    /// Whether the user who scrobbled it loves the track (now, not when it
    /// was played).
    pub loved: bool,
}

/// A scrobble's label as profiles show it. The classifier's duplicates (the
/// same listen reported again) are left out of profiles altogether.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ScrobbleLabel {
    /// A plausible listen.
    Counted,
    /// Over the listening time real time allows (the classifier's budget).
    Suspect,
    /// Of a track with no known length, so its listening couldn't be checked.
    NoData,
}

impl ScrobbleLabel {
    /// From a stored label; `None` for `duplicate` and anything unknown.
    pub fn from_status(status: &str) -> Option<Self> {
        match status {
            "counted" => Some(Self::Counted),
            "suspect" => Some(Self::Suspect),
            "no_data" => Some(Self::NoData),
            _ => None,
        }
    }
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
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub image_url: Option<String>,
    /// Plays in the period, duplicates left out.
    pub play_count: i64,
    /// Of `play_count`, the plays the classifier labelled `suspect` or
    /// `no_data`; plays not classified yet are in neither.
    pub unverified_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct TopTrack {
    pub track_id: i64,
    pub track_title: String,
    pub artist_id: i64,
    pub artist_name: String,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub album_image: Option<String>,
    /// Plays in the period, duplicates left out.
    pub play_count: i64,
    /// In a user's top tracks, as for [`TopArtist`]; `null` elsewhere.
    pub unverified_count: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct TopAlbum {
    pub album_id: i64,
    pub album_title: String,
    pub artist_id: i64,
    pub artist_name: String,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub image_url: Option<String>,
    /// Plays of its tracks in the period, duplicates left out.
    pub play_count: i64,
    /// As for [`TopArtist`].
    pub unverified_count: i64,
}

/// What a page of a user's chart covers.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
pub struct ChartInfo {
    /// The period asked for; `null` for a custom range.
    pub period: Option<crate::charts::ChartPeriod>,
    /// The first UTC day covered; `null` for `all`.
    pub from: Option<NaiveDate>,
    /// The last UTC day covered, included: today for a period.
    pub to: NaiveDate,
    /// Entries the chart has; a page past them is empty.
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

/// A page of a user's top artists.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct ArtistChart {
    #[serde(flatten)]
    #[ts(flatten)]
    pub info: ChartInfo,
    pub entries: Vec<TopArtist>,
}

/// A page of a user's top tracks.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct TrackChart {
    #[serde(flatten)]
    #[ts(flatten)]
    pub info: ChartInfo,
    pub entries: Vec<TopTrack>,
}

/// A page of a user's top albums.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct AlbumChart {
    #[serde(flatten)]
    #[ts(flatten)]
    pub info: ChartInfo,
    pub entries: Vec<TopAlbum>,
}

/// Who someone is, as lists of others' activity name them.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct UserSummary {
    pub id: i64,
    pub username: String,
    pub display_name: Option<String>,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub image_url: Option<String>,
}

/// A scrobble in the feed of the accounts a user follows.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct FeedEntry {
    /// The scrobble's id: an entry seen on two pages (when the client polls
    /// the first page again) is the same entry.
    pub id: i64,
    pub played_at: DateTime<Utc>,
    pub user: UserSummary,
    pub track_id: i64,
    pub track_title: String,
    pub artist_id: i64,
    pub artist_name: String,
    /// The album the play named, else the one the catalog files the track
    /// under.
    pub album_id: Option<i64>,
    pub album_title: Option<String>,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub album_image: Option<String>,
    /// For when the album has no cover, or there is no album.
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub artist_image: Option<String>,
    /// `counted`, `no_data` (a track without a known length), or `null`
    /// until it is classified; suspect plays and duplicates aren't in the
    /// feed.
    pub status: Option<ScrobbleLabel>,
    /// Whether the account that played it loves the track.
    pub loved: bool,
}

/// A page of the feed, newest first.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct FeedPage {
    pub entries: Vec<FeedEntry>,
    /// `before` for the next, older page; `null` once the feed's oldest day
    /// is reached. A page may hold fewer entries than asked for and still
    /// have a next one.
    pub next: Option<String>,
}

/// A track a user loves.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct LovedTrack {
    pub track_id: i64,
    pub track_title: String,
    pub artist_id: i64,
    pub artist_name: String,
    /// The album the catalog files the track under (the one its listeners
    /// named); `null` without one.
    pub album_id: Option<i64>,
    pub album_title: Option<String>,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub album_image: Option<String>,
    /// For when the album has no cover, or there is no album.
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub artist_image: Option<String>,
    pub loved_at: DateTime<Utc>,
}

/// A page of a user's loved tracks, newest love first.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct LovedTrackPage {
    pub entries: Vec<LovedTrack>,
    /// Every track the user loves.
    pub total: i64,
    /// `before` for the next page; `null` on the last one.
    pub next: Option<String>,
}

/// A user who listens to a given artist or track, with their play count.
/// Only public profiles are ever returned.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct TopListener {
    pub user_id: i64,
    pub username: String,
    pub display_name: Option<String>,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub image_url: Option<String>,
    pub play_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct ActivityDay {
    pub day: DateTime<Utc>,
    /// Duplicates left out.
    pub scrobble_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct NowPlayingRich {
    /// For loving it, or opening its page.
    pub track_id: i64,
    pub track_title: String,
    pub artist_name: String,
    pub album_title: Option<String>,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub album_image: Option<String>,
    /// Fallback artwork for clients when the album has no cover (tracks have
    /// no artwork of their own — the album cover is the track's image).
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub artist_image: Option<String>,
    pub started_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub source: String,
}

/*
    Global rankings
*/

/// What every page of a global ranking says about the stored ranking it
/// comes from.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
pub struct RankingInfo {
    pub period: crate::ranking::Period,
    /// The first UTC day the ranking covers.
    pub from: NaiveDate,
    /// The last UTC day it covers (included): the day it was computed.
    pub to: NaiveDate,
    /// When the server computed it. Week and month rankings are recomputed
    /// every hour, the year's every day, and each when the UTC day changes.
    pub computed_at: DateTime<Utc>,
    /// Its recomputation is overdue by more than 15 minutes: this is the
    /// last ranking the server managed to compute.
    pub stale: bool,
    /// Positions this ranking has; a page past them is empty.
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

/// An artist's place in a global ranking.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct RankedArtist {
    /// 1 for the first.
    pub position: i32,
    pub artist_id: i64,
    pub artist_name: String,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub image_url: Option<String>,
    /// Listeners in the period, rounded, the ranking's order: each account
    /// counts once at most, and less when its plays don't look like real
    /// listening.
    pub listeners: i64,
    /// Plays in the period, rounded, each weighed by how surely it was a
    /// real listen.
    pub plays: i64,
}

/// A track's place in a global ranking.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct RankedTrack {
    /// 1 for the first.
    pub position: i32,
    pub track_id: i64,
    pub track_title: String,
    pub artist_id: i64,
    pub artist_name: String,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub album_image: Option<String>,
    /// As in [`RankedArtist`].
    pub listeners: i64,
    /// As in [`RankedArtist`].
    pub plays: i64,
}

/// A page of the global artist ranking.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct ArtistRanking {
    #[serde(flatten)]
    #[ts(flatten)]
    pub info: RankingInfo,
    pub entries: Vec<RankedArtist>,
}

/// A page of the global track ranking.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct TrackRanking {
    #[serde(flatten)]
    #[ts(flatten)]
    pub info: RankingInfo,
    pub entries: Vec<RankedTrack>,
}

/// An album's place in a ranking.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct RankedAlbum {
    /// 1 for the first.
    pub position: i32,
    pub album_id: i64,
    pub album_title: String,
    pub artist_id: i64,
    pub artist_name: String,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub image_url: Option<String>,
    /// As in [`RankedArtist`].
    pub listeners: i64,
    /// As in [`RankedArtist`].
    pub plays: i64,
}

/// What a page of a community chart covers: the accounts the viewer
/// follows.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
pub struct CommunityChartInfo {
    /// The period asked for; `null` for a custom range.
    pub period: Option<crate::charts::ChartPeriod>,
    /// The first UTC day covered.
    pub from: NaiveDate,
    /// The last UTC day covered, included: today for a period.
    pub to: NaiveDate,
    /// Accounts the viewer follows that the chart may cover: public ones
    /// not waiting to be deleted.
    pub following: i64,
    /// Those the chart covers: the most recently active, as many as the
    /// range allows (every one over a week, 40 over a year).
    pub accounts: i64,
    /// Listeners an entry needs: entities fewer of these accounts played
    /// are left out, so no entry stands for one or two people's listening.
    pub min_listeners: i64,
    /// Entries the chart has; a page past them is empty.
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

/// A page of the artists the accounts a user follows listen to.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct CommunityArtistChart {
    #[serde(flatten)]
    #[ts(flatten)]
    pub info: CommunityChartInfo,
    pub entries: Vec<RankedArtist>,
}

/// A page of the tracks the accounts a user follows listen to.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct CommunityTrackChart {
    #[serde(flatten)]
    #[ts(flatten)]
    pub info: CommunityChartInfo,
    pub entries: Vec<RankedTrack>,
}

/// A page of the albums the accounts a user follows listen to.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct CommunityAlbumChart {
    #[serde(flatten)]
    #[ts(flatten)]
    pub info: CommunityChartInfo,
    pub entries: Vec<RankedAlbum>,
}

/*
    Similarity and recommendations
*/

/// An artist, as an explanation names it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct ArtistRef {
    pub artist_id: i64,
    pub artist_name: String,
}

/// An artist whose listeners also play the one asked about.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct SimilarArtist {
    /// 1 for the most similar.
    pub position: i32,
    pub artist_id: i64,
    pub artist_name: String,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub image_url: Option<String>,
    /// 0 to 1, three decimals: how much more the two audiences overlap than
    /// chance would have them, against their sizes.
    pub score: f64,
    /// A lower bound of the accounts listening to both, from 10, 20, 50,
    /// 100, 200, 500, 1,000 and so on, never the exact count.
    pub common_listeners: i64,
}

/// A page of an artist's similar artists.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct SimilarArtists {
    pub artist_id: i64,
    /// When the server computed them; `null` before it first did.
    pub computed_at: Option<DateTime<Utc>>,
    /// Their daily recomputation is overdue: these are the last the server
    /// managed to compute.
    pub stale: bool,
    /// Similar artists this artist has; none when too few accounts listen
    /// to it, or to anything with it.
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
    pub entries: Vec<SimilarArtist>,
}

/// Why a personal list holds what it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum SimilarityStatus {
    /// Computed, entries as they are (maybe none left after skips and new
    /// plays).
    Ready,
    /// Nothing for this user yet: too little listening, or none of it
    /// shared with enough others. Computed again daily.
    None,
    /// The user turned similarity off.
    Off,
    /// The server hasn't computed anything yet.
    Pending,
}

/// An artist recommended to the signed-in user.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct RecommendedArtist {
    /// 1 for the best.
    pub position: i32,
    pub artist_id: i64,
    pub artist_name: String,
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub image_url: Option<String>,
    /// 0 to 1 against the first recommendation, three decimals.
    pub score: f64,
    /// One of the user's own artists whose listeners also play this one,
    /// the one that counted most: "listeners of {because} also play this".
    pub because: ArtistRef,
    /// How many of the user's artists led to it, `because` included.
    pub because_count: i64,
}

/// A page of the artists recommended to the signed-in user.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct ArtistRecommendations {
    pub status: SimilarityStatus,
    /// When the server computed them; `null` before it first did.
    pub computed_at: Option<DateTime<Utc>>,
    /// The daily recomputation is overdue.
    pub stale: bool,
    /// Recommendations left after skips and plays since they were computed.
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
    pub entries: Vec<RecommendedArtist>,
}

/// An account whose listening is close to a user's, among those that chose
/// to be shown.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct SimilarUser {
    pub user: UserSummary,
    /// 0 to 1, three decimals.
    pub score: f64,
    /// Artists both listen to.
    pub shared_artists: i64,
    /// The rarest of them, at most three.
    pub top_shared: Vec<ArtistRef>,
}

/// A user's taste neighbours.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct SimilarUsers {
    pub status: SimilarityStatus,
    pub computed_at: Option<DateTime<Utc>>,
    pub stale: bool,
    pub entries: Vec<SimilarUser>,
}

/// Whether the account's listening is used for similarity, and shown.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct SimilaritySettings {
    pub mode: crate::similarity::SimilarityMode,
    /// Others see the account as a taste neighbour now: `listed`, and the
    /// profile public.
    pub shown: bool,
    /// When the mode last changed; `null` if never.
    pub changed_at: Option<DateTime<Utc>>,
}

/// A community-uploaded image candidate for an artist or album. Uploads add
/// candidates rather than replacing the image; the most-liked candidate
/// (once it clears the vote threshold) becomes the entity's displayed image.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct ImageCandidate {
    pub id: i64,
    #[serde(serialize_with = "crate::media::serialize_url")]
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
    #[serde(serialize_with = "crate::media::serialize_opt_url")]
    pub image_url: Option<String>,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

/// What a bio or a tag is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum CatalogKind {
    Artist,
    Album,
    Track,
}

impl CatalogKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Artist => "artist",
            Self::Album => "album",
            Self::Track => "track",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "artist" => Self::Artist,
            "album" => Self::Album,
            "track" => Self::Track,
            _ => return None,
        })
    }
}

/// How a bio revision came to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum RevisionKind {
    /// Written by an account.
    Edit,
    /// An account restored an earlier revision's text.
    Revert,
    /// Fetched from Last.fm before anyone edited the bio.
    Import,
    /// A moderator's revert.
    Moderation,
}

impl RevisionKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "edit" => Self::Edit,
            "revert" => Self::Revert,
            "import" => Self::Import,
            "moderation" => Self::Moderation,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum RevisionStatus {
    Published,
    /// Waiting for a moderator: not shown until approved.
    Held,
    /// A held edit a moderator turned down.
    Rejected,
}

impl RevisionStatus {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "published" => Self::Published,
            "held" => Self::Held,
            "rejected" => Self::Rejected,
            _ => return None,
        })
    }
}

/// An artist's or album's bio as it stands.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct Bio {
    pub entity_type: CatalogKind,
    pub entity_id: i64,
    /// The current revision; `null` when there is no bio.
    pub revision_id: Option<i64>,
    /// Plain text: show it as text, a paragraph at each blank line, and
    /// `http://` and `https://` URLs as links (`rel="nofollow ugc"`), never
    /// embedded or previewed.
    pub body: Option<String>,
    pub kind: Option<RevisionKind>,
    /// Who wrote the current revision; `null` for imported text, a
    /// moderator's revert, and an account deleted or waiting to be, or
    /// blocking the viewer or blocked by it.
    pub author: Option<UserSummary>,
    pub updated_at: Option<DateTime<Utc>>,
    /// Locked by a moderator: nobody may edit or revert it.
    pub locked: bool,
    /// Published revisions in its history.
    pub revisions: i64,
}

/// A revision of a bio.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct BioRevision {
    pub id: i64,
    pub entity_type: CatalogKind,
    pub entity_id: i64,
    /// The revision that was current when it was written; `null` for the
    /// first.
    pub parent_id: Option<i64>,
    pub kind: RevisionKind,
    /// As [`Bio::author`].
    pub author: Option<UserSummary>,
    /// The edit summary (maybe empty); `null` when a moderator hid the
    /// revision.
    pub summary: Option<String>,
    /// The text; `null` in history pages (fetch the revision for it) and
    /// when a moderator hid the revision.
    pub body: Option<String>,
    /// Characters in the text.
    pub length: i64,
    /// For a revert, the revision whose text it restored.
    pub reverted_to: Option<i64>,
    pub status: RevisionStatus,
    /// Why a held edit waits for review: `repeated_link`,
    /// `link_across_bios` or `duplicate_text`.
    pub held_reason: Option<String>,
    /// A moderator hid its text and summary.
    pub hidden: bool,
    /// It is the bio's current text.
    pub current: bool,
    pub created_at: DateTime<Utc>,
}

/// A page of a bio's history, newest revision first.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct BioHistoryPage {
    pub entries: Vec<BioRevision>,
    /// `before` for the next page; `null` on the last one.
    pub next: Option<String>,
}

/// A user's OAuth connection to an external streaming service (currently
/// Spotify only), polled by the worker to auto-scrobble listening activity
/// reported by the provider's own official API.
///
/// Server-internal: it carries live token material, so it is never returned
/// from a handler — `GET /v1/connect` returns [`ConnectedAccountSummary`],
/// which has no token fields at all. The tokens here are plaintext; they
/// are encrypted going into the database and decrypted coming out, by
/// `db::queries::connected_accounts`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ConnectedAccount {
    pub id: i64,
    pub user_id: i64,
    pub provider: String,
    pub provider_user_id: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub scope: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    /// When the worker last attempted a poll — scheduling only.
    pub last_polled_at: Option<DateTime<Utc>>,
    /// `played_at` of the newest play ingested so far: the provider history
    /// cursor. Tracked separately from `last_polled_at` — see
    /// `migrations/0006_connected_accounts.sql` for why.
    pub history_cursor_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What `GET /v1/connect` returns: a connection stripped of token material.
/// A separate type rather than `#[serde(skip_serializing)]` on
/// [`ConnectedAccount`], so the user-facing query never selects — and never
/// decrypts — the token columns in the first place.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, JsonSchema, TS)]
#[ts(export)]
pub struct ConnectedAccountSummary {
    pub id: i64,
    pub user_id: i64,
    pub provider: String,
    pub provider_user_id: String,
    pub token_type: String,
    pub scope: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_polled_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum ImportStatus {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl ImportStatus {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }
}

/// A listening-history import (currently from Last.fm) and its progress.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct ScrobbleImport {
    pub id: i64,
    pub provider: String,
    pub external_user: String,
    /// Ownership of the external account was proven through its auth flow.
    pub verified: bool,
    pub status: ImportStatus,
    /// Set when `status` is `failed`: `user_not_found`, `history_hidden`,
    /// `cap_reached`, `lastfm_unavailable` or `lastfm_error`.
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    /// Imports cover scrobbles after `window_from` (re-imports only) up to
    /// `window_to`, fixed when the import starts.
    pub window_from: Option<DateTime<Utc>>,
    pub window_to: Option<DateTime<Utc>>,
    /// Scrobbles in the window according to the provider, known once the
    /// first page is in.
    pub total_expected: Option<i64>,
    /// Scrobbles read so far; can end slightly above `total_expected`, as
    /// pages at segment boundaries overlap.
    pub fetched: i64,
    pub imported: i64,
    pub duplicates: i64,
    pub skipped: i64,
    /// How far back in time the import has reached.
    pub oldest_played_at: Option<DateTime<Utc>>,
    /// Loved tracks the provider lists for the account, known once their
    /// first page is read; `null` before, and when they couldn't be read.
    pub loved_total: Option<i64>,
    /// Loves added; tracks already loved here aren't counted.
    pub loved_imported: i64,
    /// The loves are read (before the scrobbles).
    pub loved_done: bool,
    /// Why the loves stopped short: `history_hidden` (the provider wouldn't
    /// show them), `cap_reached` (the account loves the most tracks
    /// allowed) or `lastfm_error`. The import goes on with the scrobbles.
    pub loved_error: Option<String>,
    /// Set while a running import waits out a provider error.
    pub retrying_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// A credential a third-party scrobbler holds (Last.fm-, Audioscrobbler-
/// or ListenBrainz-compatible). It authenticates those endpoints only,
/// never the native API. The secret is shown once, when created.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct ScrobblerCredential {
    pub id: Uuid,
    /// `token`: made by the user to paste into a client. `session`: a
    /// Last.fm session key a client obtained by logging in or through the
    /// browser authorization.
    pub kind: String,
    pub name: String,
    /// The Last.fm API key a session is bound to.
    pub api_key: Option<String>,
    /// Usable as an Audioscrobbler 1.2 password (possible only when the
    /// server could store it encrypted).
    pub legacy_auth: bool,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// A new scrobbler token and its secret, which is never shown again.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct CreatedScrobblerToken {
    pub credential: ScrobblerCredential,
    pub token: String,
}

/// A Last.fm-API client asking to scrobble for the user, as the browser
/// authorization page shows it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct ScrobblerAuthorization {
    pub api_key: String,
    /// `pending`, `approved` or `expired`.
    pub status: String,
    pub expires_at: DateTime<Utc>,
}

/// Where the browser goes after approving a web-flow client: the client's
/// callback, carrying the authorized token.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct ScrobblerAuthorizationRedirect {
    pub token: String,
    pub redirect_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const KEY: &str = "avatars/ab/abcd.jpg";

    /// Every string in `value`.
    fn strings(value: &Value) -> Vec<&str> {
        match value {
            Value::String(s) => vec![s],
            Value::Array(items) => items.iter().flat_map(strings).collect(),
            Value::Object(fields) => fields.values().flat_map(strings).collect(),
            _ => vec![],
        }
    }

    #[test]
    fn every_image_field_serializes_keys_as_urls() {
        let now = Utc::now();
        let image = || Some(KEY.to_owned());
        let user = User {
            id: 1,
            username: "u".into(),
            email: "u@example.com".into(),
            password_hash: String::new(),
            display_name: None,
            bio: None,
            image_url: image(),
            website_url: None,
            country: None,
            scrobble_count: 0,
            is_private: false,
            is_verified: false,
            last_seen_at: None,
            created_at: now,
            updated_at: now,
            delete_after: None,
        };
        let models = [
            serde_json::to_value(&user).unwrap(),
            serde_json::to_value(UserProfile::from(user)).unwrap(),
            serde_json::to_value(Artist {
                id: 1,
                name: "a".into(),
                name_normalized: "a".into(),
                mbid: None,
                image_url: image(),
                bio: None,
                scrobble_count: None,
                listener_count: None,
                created_at: now,
            })
            .unwrap(),
            serde_json::to_value(Album {
                id: 1,
                artist_id: 1,
                artist_name: "a".into(),
                title: "a".into(),
                title_normalized: "a".into(),
                mbid: None,
                image_url: image(),
                release_date: None,
                scrobble_count: None,
                created_at: now,
            })
            .unwrap(),
            serde_json::to_value(TrackCredit {
                artist_id: 1,
                name: "a".into(),
                image_url: image(),
                role: TrackArtistRole::Primary,
                position: 0,
            })
            .unwrap(),
            serde_json::to_value(ScrobbleRich {
                id: 1,
                played_at: now,
                source: "s".into(),
                track_id: 1,
                track_title: "t".into(),
                artist_id: 1,
                artist_name: "a".into(),
                album_id: None,
                album_title: None,
                album_image: image(),
                duration_ms: None,
                status: Some(ScrobbleLabel::Counted),
                loved: false,
            })
            .unwrap(),
            serde_json::to_value(LovedTrack {
                track_id: 1,
                track_title: "t".into(),
                artist_id: 1,
                artist_name: "a".into(),
                album_id: None,
                album_title: None,
                album_image: image(),
                artist_image: image(),
                loved_at: now,
            })
            .unwrap(),
            serde_json::to_value(TopArtist {
                artist_id: 1,
                artist_name: "a".into(),
                image_url: image(),
                play_count: 1,
                unverified_count: 0,
            })
            .unwrap(),
            serde_json::to_value(TopTrack {
                track_id: 1,
                track_title: "t".into(),
                artist_id: 1,
                artist_name: "a".into(),
                album_image: image(),
                play_count: 1,
                unverified_count: None,
            })
            .unwrap(),
            serde_json::to_value(TopAlbum {
                album_id: 1,
                album_title: "a".into(),
                artist_id: 1,
                artist_name: "a".into(),
                image_url: image(),
                play_count: 1,
                unverified_count: 0,
            })
            .unwrap(),
            serde_json::to_value(RankedArtist {
                position: 1,
                artist_id: 1,
                artist_name: "a".into(),
                image_url: image(),
                listeners: 5,
                plays: 9,
            })
            .unwrap(),
            serde_json::to_value(RankedTrack {
                position: 1,
                track_id: 1,
                track_title: "t".into(),
                artist_id: 1,
                artist_name: "a".into(),
                album_image: image(),
                listeners: 5,
                plays: 9,
            })
            .unwrap(),
            serde_json::to_value(RankedAlbum {
                position: 1,
                album_id: 1,
                album_title: "a".into(),
                artist_id: 1,
                artist_name: "a".into(),
                image_url: image(),
                listeners: 5,
                plays: 9,
            })
            .unwrap(),
            serde_json::to_value(TopListener {
                user_id: 1,
                username: "u".into(),
                display_name: None,
                image_url: image(),
                play_count: 1,
            })
            .unwrap(),
            serde_json::to_value(FeedEntry {
                id: 1,
                played_at: now,
                user: UserSummary {
                    id: 1,
                    username: "u".into(),
                    display_name: None,
                    image_url: image(),
                },
                track_id: 1,
                track_title: "t".into(),
                artist_id: 1,
                artist_name: "a".into(),
                album_id: None,
                album_title: None,
                album_image: image(),
                artist_image: image(),
                status: None,
                loved: false,
            })
            .unwrap(),
            serde_json::to_value(NowPlayingRich {
                track_id: 1,
                track_title: "t".into(),
                artist_name: "a".into(),
                album_title: None,
                album_image: image(),
                artist_image: image(),
                started_at: now,
                expires_at: now,
                source: "s".into(),
            })
            .unwrap(),
            serde_json::to_value(ImageCandidate {
                id: 1,
                url: KEY.into(),
                uploaded_by: "u".into(),
                vote_count: 1,
                has_voted: false,
                is_default: false,
                created_at: now,
            })
            .unwrap(),
            serde_json::to_value(Comment {
                id: 1,
                user_id: 1,
                username: "u".into(),
                display_name: None,
                image_url: image(),
                body: "b".into(),
                created_at: now,
            })
            .unwrap(),
            serde_json::to_value(SimilarArtist {
                position: 1,
                artist_id: 1,
                artist_name: "a".into(),
                image_url: image(),
                score: 0.5,
                common_listeners: 10,
            })
            .unwrap(),
            serde_json::to_value(RecommendedArtist {
                position: 1,
                artist_id: 1,
                artist_name: "a".into(),
                image_url: image(),
                score: 1.0,
                because: ArtistRef {
                    artist_id: 2,
                    artist_name: "b".into(),
                },
                because_count: 1,
            })
            .unwrap(),
            serde_json::to_value(SimilarUser {
                user: UserSummary {
                    id: 1,
                    username: "u".into(),
                    display_name: None,
                    image_url: image(),
                },
                score: 0.5,
                shared_artists: 3,
                top_shared: vec![],
            })
            .unwrap(),
        ];
        let url = format!("{}/{KEY}", crate::media::public_base());
        for model in &models {
            let strings = strings(model);
            assert!(!strings.contains(&KEY), "bare key in {model}");
            assert!(strings.contains(&url.as_str()), "no image URL in {model}");
        }
    }
}
