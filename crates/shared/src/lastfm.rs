//! Last.fm Web Services client: listening-history import
//! (`user.getRecentTracks`), track lengths (`track.getInfo`) and the web-auth
//! flow that proves a Scrobblr user owns a Last.fm account
//! (`auth.getSession`). Shared by `crates/api` (auth) and `crates/worker`
//! (import, lengths). Rate limiting is the caller's job.

use md5::{Digest, Md5};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use uuid::Uuid;

use crate::scrobble::MAX_NAME_LEN;

pub const API_URL: &str = "https://ws.audioscrobbler.com/2.0/";
const AUTH_URL: &str = "https://www.last.fm/api/auth/";

/// `user.getRecentTracks` page size (the API maximum).
pub const PAGE_LIMIT: u32 = 200;
/// Pages fetched before the walk restarts at page 1 further back in time.
pub const PAGES_PER_SEGMENT: u32 = 20;
/// Scrobbles before Audioscrobbler existed come from broken clients.
pub const MIN_TIMESTAMP: i64 = 1_009_843_200; // 2002-01-01

// https://www.last.fm/api/errorcodes
pub const ERROR_INVALID_PARAMETERS: i32 = 6; // also "User not found" / "Track not found"
pub const ERROR_OPERATION_FAILED: i32 = 8;
pub const ERROR_INVALID_SESSION: i32 = 9;
pub const ERROR_SERVICE_OFFLINE: i32 = 11;
pub const ERROR_TEMPORARILY_UNAVAILABLE: i32 = 16;
/// Returned for profiles that hide their recent listening.
pub const ERROR_LOGIN_REQUIRED: i32 = 17;
pub const ERROR_RATE_LIMITED: i32 = 29;

#[derive(Debug, Error)]
pub enum LastfmError {
    #[error("last.fm error {code}: {message}")]
    Api { code: i32, message: String },
    #[error("last.fm request failed: {0}")]
    Http(String),
    #[error("last.fm returned HTTP {0}")]
    Status(u16),
    #[error("unexpected last.fm response: {0}")]
    Parse(String),
    #[error("LASTFM_SHARED_SECRET is not set")]
    MissingSecret,
}

impl LastfmError {
    pub fn code(&self) -> Option<i32> {
        match self {
            LastfmError::Api { code, .. } => Some(*code),
            _ => None,
        }
    }

    pub fn is_rate_limited(&self) -> bool {
        self.code() == Some(ERROR_RATE_LIMITED) || matches!(self, LastfmError::Status(429))
    }

    /// Worth retrying later. A garbled body counts: Last.fm answers some
    /// outages with an HTML page.
    pub fn is_transient(&self) -> bool {
        match self {
            LastfmError::Api { code, .. } => matches!(
                *code,
                ERROR_OPERATION_FAILED
                    | ERROR_SERVICE_OFFLINE
                    | ERROR_TEMPORARILY_UNAVAILABLE
                    | ERROR_RATE_LIMITED
            ),
            LastfmError::Http(_) | LastfmError::Parse(_) => true,
            LastfmError::Status(status) => *status == 429 || *status >= 500,
            LastfmError::MissingSecret => false,
        }
    }
}

/// One scrobble from a user's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastfmPlay {
    /// Unix seconds.
    pub uts: i64,
    pub artist: String,
    pub track: String,
    pub album: Option<String>,
    /// MusicBrainz recording id as Last.fm knows it; may be wrong or stale.
    pub track_mbid: Option<Uuid>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecentTracksPage {
    pub plays: Vec<LastfmPlay>,
    /// Dated scrobbles rejected by [`LastfmPlay`] validation.
    pub invalid: u32,
    /// Oldest timestamp on the page, invalid scrobbles included.
    pub oldest_uts: Option<i64>,
    pub page: u32,
    pub total_pages: u32,
    /// Scrobbles in the requested range, across all pages.
    pub total: u64,
}

impl RecentTracksPage {
    /// Scrobbles on the page, excluding the now-playing entry.
    pub fn dated(&self) -> usize {
        self.plays.len() + self.invalid as usize
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Canonical Last.fm username.
    pub name: String,
    pub key: String,
}

/// Resumable position in a user's history, walked newest to oldest.
///
/// Deep `page` offsets are slow on Last.fm's side, so the walk is cut into
/// segments of [`PAGES_PER_SEGMENT`] pages; each new segment restarts at
/// page 1 with `to` one second above the oldest scrobble seen so far. The
/// extra second keeps that timestamp inside the next segment whether Last.fm
/// treats `to` as inclusive or exclusive, and the overlap is deduplicated on
/// insert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// `to` parameter of the current segment.
    pub segment_to: i64,
    /// Next page to fetch within the segment.
    pub page: u32,
    pub segment_oldest: Option<i64>,
}

impl Cursor {
    pub fn start(window_to: i64) -> Self {
        Cursor {
            segment_to: window_to + 1,
            page: 1,
            segment_oldest: None,
        }
    }

    /// The position after `fetched` was read at `self`, or `None` once the
    /// history is exhausted. Always moves: either one page deeper, or to a
    /// strictly earlier segment.
    pub fn advance(&self, fetched: &RecentTracksPage) -> Option<Cursor> {
        if fetched.dated() == 0 || fetched.page >= fetched.total_pages {
            return None;
        }
        let oldest = [self.segment_oldest, fetched.oldest_uts]
            .into_iter()
            .flatten()
            .min();
        if self.page >= PAGES_PER_SEGMENT
            && let Some(oldest) = oldest
            && oldest + 1 < self.segment_to
        {
            return Some(Cursor {
                segment_to: oldest + 1,
                page: 1,
                segment_oldest: None,
            });
        }
        Some(Cursor {
            segment_to: self.segment_to,
            page: self.page + 1,
            segment_oldest: oldest,
        })
    }
}

/// `api_sig`: every parameter except `format` and `callback`, sorted by
/// name, concatenated as name+value, followed by the shared secret, MD5'd.
pub fn signature(params: &[(&str, &str)], secret: &str) -> String {
    let mut signed: Vec<&(&str, &str)> = params
        .iter()
        .filter(|(name, _)| *name != "format" && *name != "callback")
        .collect();
    signed.sort_by(|a, b| a.0.cmp(b.0));

    let mut hasher = Md5::new();
    for (name, value) in signed {
        hasher.update(name.as_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.update(secret.as_bytes());
    hex(&hasher.finalize())
}

/// Lowercase hex MD5, as Audioscrobbler's auth tokens use it.
pub fn md5_hex(value: &str) -> String {
    hex(&Md5::digest(value.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub struct RecentTracksQuery<'a> {
    pub user: &'a str,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub page: u32,
    /// Signs the request as the user, which reaches a hidden history.
    pub session_key: Option<&'a str>,
}

#[derive(Clone)]
pub struct LastfmClient {
    http: reqwest::Client,
    api_url: String,
    api_key: String,
    shared_secret: Option<String>,
}

impl LastfmClient {
    pub fn new(
        http: reqwest::Client,
        api_url: impl Into<String>,
        api_key: impl Into<String>,
        shared_secret: Option<String>,
    ) -> Self {
        Self {
            http,
            api_url: api_url.into(),
            api_key: api_key.into(),
            shared_secret,
        }
    }

    /// `None` unless `LASTFM_API_KEY` is set. `LASTFM_SHARED_SECRET` is only
    /// needed for the auth flow and signed requests.
    pub fn from_env(http: reqwest::Client) -> Option<Self> {
        let non_empty = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let api_key = non_empty("LASTFM_API_KEY")?;
        Some(Self::new(
            http,
            API_URL,
            api_key,
            non_empty("LASTFM_SHARED_SECRET"),
        ))
    }

    pub fn can_sign(&self) -> bool {
        self.shared_secret.is_some()
    }

    /// Where to send the user to grant access; Last.fm appends `token` to
    /// `callback` (keeping its own query string).
    pub fn auth_url(&self, callback: &str) -> String {
        let mut url = reqwest::Url::parse(AUTH_URL).expect("AUTH_URL is a valid static URL");
        url.query_pairs_mut()
            .append_pair("api_key", &self.api_key)
            .append_pair("cb", callback);
        url.to_string()
    }

    pub async fn recent_tracks(
        &self,
        query: &RecentTracksQuery<'_>,
    ) -> Result<RecentTracksPage, LastfmError> {
        let page = query.page.to_string();
        let limit = PAGE_LIMIT.to_string();
        let from = query.from.map(|t| t.to_string());
        let to = query.to.map(|t| t.to_string());
        let mut params = vec![
            ("method", "user.getrecenttracks"),
            ("user", query.user),
            ("limit", limit.as_str()),
            ("page", page.as_str()),
            ("extended", "0"),
        ];
        if let Some(from) = &from {
            params.push(("from", from.as_str()));
        }
        if let Some(to) = &to {
            params.push(("to", to.as_str()));
        }
        let session_key = query.session_key.filter(|_| self.can_sign());
        let body = self.get(params, session_key).await?;
        parse_recent_tracks(&body)
    }

    /// The track's length in ms, or `None` when Last.fm doesn't know it.
    pub async fn track_duration(
        &self,
        artist: &str,
        track: &str,
    ) -> Result<Option<i32>, LastfmError> {
        let params = vec![
            ("method", "track.getinfo"),
            ("artist", artist),
            ("track", track),
            ("autocorrect", "0"),
        ];
        match self.get(params, None).await {
            Ok(body) => parse_track_duration(&body),
            Err(e) if e.code() == Some(ERROR_INVALID_PARAMETERS) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Trades the token Last.fm appended to the auth callback for a session,
    /// whose `name` is the account the user just authorized.
    pub async fn get_session(&self, token: &str) -> Result<Session, LastfmError> {
        let secret = self
            .shared_secret
            .as_deref()
            .ok_or(LastfmError::MissingSecret)?;
        let mut params = vec![
            ("method", "auth.getsession"),
            ("api_key", self.api_key.as_str()),
            ("token", token),
        ];
        let sig = signature(&params, secret);
        params.push(("api_sig", &sig));
        params.push(("format", "json"));
        let body = self.send(&params).await?;
        parse_session(&body)
    }

    async fn get(
        &self,
        mut params: Vec<(&str, &str)>,
        session_key: Option<&str>,
    ) -> Result<String, LastfmError> {
        params.push(("api_key", &self.api_key));
        let sig;
        if let (Some(sk), Some(secret)) = (session_key, &self.shared_secret) {
            params.push(("sk", sk));
            sig = signature(&params, secret);
            params.push(("api_sig", &sig));
        }
        params.push(("format", "json"));
        self.send(&params).await
    }

    async fn send(&self, params: &[(&str, &str)]) -> Result<String, LastfmError> {
        let response = self
            .http
            .get(&self.api_url)
            .query(params)
            .send()
            .await
            .map_err(|e| LastfmError::Http(e.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| LastfmError::Http(e.to_string()))?;

        // Errors carry a JSON body whatever the status code.
        if let Ok(error) = serde_json::from_str::<ErrorBody>(&body) {
            return Err(LastfmError::Api {
                code: error.error,
                message: error.message.unwrap_or_default(),
            });
        }
        if !status.is_success() {
            return Err(LastfmError::Status(status.as_u16()));
        }
        Ok(body)
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    error: i32,
    message: Option<String>,
}

/// Last.fm's JSON is converted from XML: numbers arrive as strings, and a
/// one-element list arrives as a bare object.
#[derive(Deserialize)]
#[serde(untagged)]
enum Number {
    Int(i64),
    Text(String),
}

impl Number {
    fn value(&self) -> Option<i64> {
        match self {
            Number::Int(n) => Some(*n),
            Number::Text(s) => s.trim().parse().ok(),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    Many(Vec<T>),
    One(T),
}

impl<T> OneOrMany<T> {
    fn into_vec(self) -> Vec<T> {
        match self {
            OneOrMany::Many(items) => items,
            OneOrMany::One(item) => vec![item],
        }
    }
}

#[derive(Deserialize)]
struct RecentTracksBody {
    recenttracks: RecentTracks,
}

#[derive(Deserialize)]
struct RecentTracks {
    track: Option<OneOrMany<RawTrack>>,
    #[serde(rename = "@attr")]
    attr: PageAttr,
}

#[derive(Deserialize)]
struct PageAttr {
    page: Option<Number>,
    #[serde(rename = "totalPages")]
    total_pages: Option<Number>,
    total: Option<Number>,
}

#[derive(Deserialize)]
struct RawTrack {
    name: Option<String>,
    mbid: Option<String>,
    artist: Option<Text>,
    album: Option<Text>,
    date: Option<RawDate>,
}

#[derive(Deserialize)]
struct Text {
    #[serde(rename = "#text")]
    text: Option<String>,
}

#[derive(Deserialize)]
struct RawDate {
    uts: Number,
}

fn parse_json<T: DeserializeOwned>(body: &str) -> Result<T, LastfmError> {
    serde_json::from_str(body).map_err(|e| LastfmError::Parse(e.to_string()))
}

fn valid_name(name: Option<String>) -> Option<String> {
    let name = name?.trim().to_string();
    (!name.is_empty() && name.chars().count() <= MAX_NAME_LEN).then_some(name)
}

pub fn parse_recent_tracks(body: &str) -> Result<RecentTracksPage, LastfmError> {
    let parsed: RecentTracksBody = parse_json(body)?;
    let attr = parsed.recenttracks.attr;
    let mut page = RecentTracksPage {
        page: attr.page.and_then(|n| n.value()).unwrap_or(1) as u32,
        total_pages: attr.total_pages.and_then(|n| n.value()).unwrap_or(0) as u32,
        total: attr.total.and_then(|n| n.value()).unwrap_or(0) as u64,
        ..Default::default()
    };

    for raw in parsed
        .recenttracks
        .track
        .map(OneOrMany::into_vec)
        .unwrap_or_default()
    {
        // No date: the now-playing entry.
        let Some(uts) = raw.date.and_then(|d| d.uts.value()) else {
            continue;
        };
        page.oldest_uts = Some(page.oldest_uts.map_or(uts, |o| o.min(uts)));

        let artist = valid_name(raw.artist.and_then(|a| a.text));
        let track = valid_name(raw.name);
        match (artist, track) {
            (Some(artist), Some(track)) if uts >= MIN_TIMESTAMP => page.plays.push(LastfmPlay {
                uts,
                artist,
                track,
                album: valid_name(raw.album.and_then(|a| a.text)),
                track_mbid: raw.mbid.and_then(|m| m.trim().parse().ok()),
            }),
            _ => page.invalid += 1,
        }
    }
    Ok(page)
}

#[derive(Deserialize)]
struct TrackInfoBody {
    track: TrackInfo,
}

#[derive(Deserialize)]
struct TrackInfo {
    duration: Option<Number>,
}

pub fn parse_track_duration(body: &str) -> Result<Option<i32>, LastfmError> {
    let parsed: TrackInfoBody = parse_json(body)?;
    Ok(parsed
        .track
        .duration
        .and_then(|d| d.value())
        .and_then(|ms| i32::try_from(ms).ok())
        .filter(|ms| *ms > 0))
}

#[derive(Deserialize)]
struct SessionBody {
    session: RawSession,
}

#[derive(Deserialize)]
struct RawSession {
    name: String,
    key: String,
}

pub fn parse_session(body: &str) -> Result<Session, LastfmError> {
    let parsed: SessionBody = parse_json(body)?;
    Ok(Session {
        name: parsed.session.name,
        key: parsed.session.key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r##"{"recenttracks":{"track":[
        {"artist":{"mbid":"","#text":"Sigur Rós"},"name":"Hoppípolla","mbid":"",
         "album":{"mbid":"","#text":"Takk..."},"@attr":{"nowplaying":"true"}},
        {"artist":{"mbid":"","#text":"Radiohead"},"name":"Airbag","mbid":"8f2bc1b0-9c33-4f25-8e65-d2dbd1c9a5b1",
         "album":{"mbid":"","#text":"OK Computer"},"date":{"uts":"1696440000","#text":"04 Oct 2023, 17:20"}},
        {"artist":{"mbid":"","#text":"  Radiohead "},"name":"Lucky","mbid":"not-a-uuid",
         "album":{"mbid":"","#text":""},"date":{"uts":"1696439000","#text":""}},
        {"artist":{"mbid":"","#text":""},"name":"Nameless","date":{"uts":"1696438000"}},
        {"artist":{"mbid":"","#text":"Broken Clock"},"name":"Epoch","date":{"uts":"0"}}
      ],"@attr":{"user":"rj","totalPages":"7","page":"2","perPage":"200","total":"1203"}}}"##;

    #[test]
    fn parses_a_page_and_skips_now_playing() {
        let page = parse_recent_tracks(PAGE).unwrap();
        assert_eq!((page.page, page.total_pages, page.total), (2, 7, 1203));
        assert_eq!(page.plays.len(), 2);
        assert_eq!(page.invalid, 2);
        assert_eq!(page.dated(), 4);
        assert_eq!(page.oldest_uts, Some(0));
        assert_eq!(
            page.plays[0],
            LastfmPlay {
                uts: 1_696_440_000,
                artist: "Radiohead".into(),
                track: "Airbag".into(),
                album: Some("OK Computer".into()),
                track_mbid: Some("8f2bc1b0-9c33-4f25-8e65-d2dbd1c9a5b1".parse().unwrap()),
            }
        );
        assert_eq!(page.plays[1].artist, "Radiohead");
        assert_eq!(page.plays[1].album, None);
        assert_eq!(page.plays[1].track_mbid, None);
    }

    #[test]
    fn parses_a_single_track_object_and_numeric_attrs() {
        let body = r##"{"recenttracks":{"track":{"artist":{"#text":"A"},"name":"T","date":{"uts":1696440000}},
            "@attr":{"page":1,"totalPages":1,"total":1}}}"##;
        let page = parse_recent_tracks(body).unwrap();
        assert_eq!(page.plays.len(), 1);
        assert_eq!(page.total_pages, 1);
    }

    #[test]
    fn parses_an_empty_history() {
        let body = r#"{"recenttracks":{"track":[],"@attr":{"user":"x","totalPages":"0","page":"1","perPage":"200","total":"0"}}}"#;
        let page = parse_recent_tracks(body).unwrap();
        assert_eq!(page.dated(), 0);
        assert_eq!(Cursor::start(100).advance(&page), None);
    }

    #[test]
    fn rejects_names_over_the_ingest_limit() {
        let long = "x".repeat(MAX_NAME_LEN + 1);
        let body = format!(
            r##"{{"recenttracks":{{"track":[{{"artist":{{"#text":"A"}},"name":"{long}","date":{{"uts":"1696440000"}}}}],"@attr":{{"totalPages":"1","page":"1","total":"1"}}}}}}"##
        );
        let page = parse_recent_tracks(&body).unwrap();
        assert_eq!((page.plays.len(), page.invalid), (0, 1));
    }

    #[test]
    fn classifies_errors() {
        let hidden = LastfmError::Api {
            code: ERROR_LOGIN_REQUIRED,
            message: String::new(),
        };
        assert!(!hidden.is_transient());
        assert!(
            LastfmError::Api {
                code: ERROR_RATE_LIMITED,
                message: String::new()
            }
            .is_rate_limited()
        );
        assert!(LastfmError::Status(503).is_transient());
        assert!(!LastfmError::Status(403).is_transient());
        assert!(LastfmError::Parse("html".into()).is_transient());
    }

    #[test]
    fn parses_track_lengths_and_sessions() {
        let info = |d: &str| format!(r#"{{"track":{{"name":"Believe","duration":"{d}"}}}}"#);
        assert_eq!(
            parse_track_duration(&info("240000")).unwrap(),
            Some(240_000)
        );
        assert_eq!(parse_track_duration(&info("0")).unwrap(), None);
        assert_eq!(
            parse_track_duration(r#"{"track":{"name":"x"}}"#).unwrap(),
            None
        );

        let session =
            parse_session(r#"{"session":{"name":"RJ","key":"d580d57f","subscriber":0}}"#).unwrap();
        assert_eq!(
            session,
            Session {
                name: "RJ".into(),
                key: "d580d57f".into()
            }
        );
    }

    #[test]
    fn signs_like_the_documented_example() {
        let params = [
            ("method", "auth.getSession"),
            ("token", "yyyyyyy"),
            ("api_key", "xxxxxxxx"),
            ("format", "json"),
        ];
        assert_eq!(
            signature(&params, "mysecret"),
            "a5a32f5a2fef8690ff405269a532a1c3"
        );
        let unicode = [
            ("sk", "tok"),
            ("artist", "Sigur Rós"),
            ("method", "track.getInfo"),
            ("api_key", "K"),
        ];
        assert_eq!(
            signature(&unicode, "sec"),
            "18aa6f68cdd93855b9b4746cce72f5ab"
        );
    }

    /// A fake history served the way Last.fm pages it: newest first, `to`
    /// bounding the range either inclusively or exclusively.
    fn serve(history: &[i64], cursor: &Cursor, to_inclusive: bool) -> RecentTracksPage {
        let mut in_range: Vec<i64> = history
            .iter()
            .copied()
            .filter(|t| {
                if to_inclusive {
                    *t <= cursor.segment_to
                } else {
                    *t < cursor.segment_to
                }
            })
            .collect();
        in_range.sort_by(|a, b| b.cmp(a));
        let total_pages = in_range.len().div_ceil(PAGE_LIMIT as usize) as u32;
        let start = (cursor.page as usize - 1) * PAGE_LIMIT as usize;
        let plays: Vec<LastfmPlay> = in_range
            .iter()
            .skip(start)
            .take(PAGE_LIMIT as usize)
            .map(|&uts| LastfmPlay {
                uts,
                artist: "A".into(),
                track: uts.to_string(),
                album: None,
                track_mbid: None,
            })
            .collect();
        RecentTracksPage {
            oldest_uts: plays.iter().map(|p| p.uts).min(),
            plays,
            invalid: 0,
            page: cursor.page,
            total_pages,
            total: in_range.len() as u64,
        }
    }

    /// Walks the whole history from `cursor`, returning every timestamp
    /// fetched (with repeats) and the number of requests made.
    fn walk(history: &[i64], mut cursor: Cursor, to_inclusive: bool) -> (Vec<i64>, usize) {
        let mut fetched = Vec::new();
        for requests in 1.. {
            let page = serve(history, &cursor, to_inclusive);
            fetched.extend(page.plays.iter().map(|p| p.uts));
            match cursor.advance(&page) {
                Some(next) => {
                    assert!(next.segment_to < cursor.segment_to || next.page == cursor.page + 1);
                    cursor = next;
                }
                None => return (fetched, requests),
            }
            assert!(requests < 100_000, "cursor did not terminate");
        }
        unreachable!()
    }

    fn history() -> Vec<i64> {
        let mut history: Vec<i64> = (0..23_000).map(|n| 1_600_000_000 + n * 180).collect();
        // Batch-submitted scrobbles sharing one second, larger than a page and
        // straddling segment boundaries.
        history.extend(std::iter::repeat_n(1_600_000_000 + 4000 * 180, 450));
        history.extend(std::iter::repeat_n(1_600_000_000 + 9000 * 180, 70));
        history
    }

    #[test]
    fn walks_every_scrobble_whether_to_is_inclusive_or_not() {
        let history = history();
        let window_to = *history.iter().max().unwrap();
        for to_inclusive in [true, false] {
            let (fetched, requests) = walk(&history, Cursor::start(window_to), to_inclusive);
            let mut expected = history.clone();
            expected.sort();
            let mut unique = fetched.clone();
            unique.sort();
            unique.dedup();
            expected.dedup();
            assert_eq!(unique, expected, "to_inclusive={to_inclusive}");
            // Repeats come only from segment overlaps.
            assert!(fetched.len() < history.len() + 2000, "{}", fetched.len());
            assert!(requests < history.len() / PAGE_LIMIT as usize + 20);
        }
    }

    #[test]
    fn resuming_from_a_saved_cursor_finishes_the_walk() {
        let history = history();
        let window_to = *history.iter().max().unwrap();
        let mut cursor = Cursor::start(window_to);
        let mut fetched = Vec::new();
        for _ in 0..37 {
            let page = serve(&history, &cursor, false);
            fetched.extend(page.plays.iter().map(|p| p.uts));
            cursor = cursor.advance(&page).unwrap();
        }
        // A restart reloads `cursor` from the database and carries on.
        let (rest, _) = walk(&history, cursor, false);
        fetched.extend(rest);
        fetched.sort();
        fetched.dedup();
        let mut expected = history;
        expected.sort();
        expected.dedup();
        assert_eq!(fetched, expected);
    }

    #[test]
    fn a_page_of_one_second_never_stalls_the_cursor() {
        let history = vec![1_700_000_000; (PAGES_PER_SEGMENT * PAGE_LIMIT) as usize + 10];
        let (fetched, _) = walk(&history, Cursor::start(1_700_000_000), true);
        assert!(fetched.len() >= history.len());
    }
}
