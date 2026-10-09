//! Ranking weights (anti-botting, shadow mode).
//!
//! Every scrobble keeps its classification label; for global rankings it
//! also gets a weight from 0 to 1 (in thousandths), the product of:
//!
//! - **Label:** `counted` weighs in full, `suspect` and `duplicate` not at
//!   all, `no_data` (listening time unchecked) [`RankingParams::no_data`].
//!   A scrobble not classified yet weighs nothing until it is.
//! - **Import:** imported history weighs [`RankingParams::imported`] and
//!   nothing below applies to it: it has no client, no listened time, and
//!   predates the account.
//! - **Account age:** plays made before the account was
//!   [`RankingParams::min_account_days`] old weigh nothing (fresh accounts
//!   are what a farm is made of; the server sets `created_at`, so plays
//!   can't be backdated past it).
//! - **Listened time:** a play declaring less than its scrobble point
//!   ([`listen_point_ms`]) weighs nothing; one declaring none weighs
//!   [`RankingParams::no_listened`].
//! - **Client:** verified and known clients weigh in full, unknown ones
//!   [`RankingParams::unknown_client`].
//!
//! Then the per-user caps, per UTC day: only the first
//! [`RankingParams::track_daily_cap`] weighted plays of a track and
//! [`RankingParams::artist_daily_cap`] of an artist keep their weight, so a
//! loop all night (or a bot within the listening budget) counts like an
//! ordinary fan's day and nobody needs to be flagged for it.
//!
//! Pure, like the classifier: [`weigh`] sees one user's UTC day and its
//! result depends only on those plays, the account's age and the params.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::classification::{Status, scrobble_point_ms};
use crate::scrobble::MIN_LISTEN_MS;

/// Bump when the weighting logic changes, so stored weights are recomputed.
pub const WEIGHTS_VERSION: u32 = 1;

/// A full weight, in thousandths.
pub const FULL: i64 = 1000;

/// Clients' timers and rounding put a listen right at its threshold a
/// little either side.
const LISTEN_TOLERANCE_MS: i64 = 1000;

/// Clients recognized out of the box: this project's own (the extension
/// and the mobile app name their source) and scrobblers whose requests the
/// compatibility tests replay. Native and ListenBrainz names are claims.
pub const DEFAULT_KNOWN_CLIENTS: &[&str] = &[
    "scrobblr:ytmusic",
    "scrobblr:android",
    "scrobblr:spotify",
    "scrobblr:youtube-music",
    "scrobblr:youtube",
    "scrobblr:tidal",
    "scrobblr:deezer",
    "scrobblr:apple-music",
    "scrobblr:amazon-music",
    "scrobblr:vlc",
    "scrobblr:poweramp",
    "listenbrainz:web scrobbler",
    "listenbrainz:pano scrobbler",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientClass {
    /// The server checked who sent it: its own Spotify poller, or a
    /// request signed with a secret it was given.
    Verified,
    Known,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankingParams {
    /// Weights in thousandths.
    pub unknown_client: i64,
    pub no_listened: i64,
    pub no_data: i64,
    pub imported: i64,
    pub min_account_days: i64,
    pub track_daily_cap: i64,
    pub artist_daily_cap: i64,
    /// `protocol:name`, lowercase. A name matches with or without a
    /// version after it ("web scrobbler" matches "Web Scrobbler 3.14.0").
    pub known_clients: BTreeSet<String>,
}

#[derive(Debug, Error)]
pub enum ParamsError {
    #[error("{0} must be between 0 and 1")]
    Weight(&'static str),
    #[error("minimum account age must be between 0 and 365 days")]
    AccountDays,
    #[error("{0} must be between 1 and 10000")]
    Cap(&'static str),
    #[error("known client `{0}` must be protocol:name")]
    Client(String),
}

impl Default for RankingParams {
    fn default() -> Self {
        Self {
            unknown_client: 500,
            no_listened: 250,
            no_data: 500,
            imported: 0,
            min_account_days: 7,
            track_daily_cap: 4,
            artist_daily_cap: 30,
            known_clients: DEFAULT_KNOWN_CLIENTS
                .iter()
                .map(|c| c.to_string())
                .collect(),
        }
    }
}

/// Settings as given, before validation.
#[derive(Debug, Clone)]
pub struct RankingSettings {
    pub unknown_client: f64,
    pub no_listened: f64,
    pub no_data: f64,
    pub imported: f64,
    pub min_account_days: i64,
    pub track_daily_cap: i64,
    pub artist_daily_cap: i64,
    pub known_clients: Vec<String>,
}

impl Default for RankingSettings {
    fn default() -> Self {
        let p = RankingParams::default();
        let weight = |permille: i64| permille as f64 / FULL as f64;
        Self {
            unknown_client: weight(p.unknown_client),
            no_listened: weight(p.no_listened),
            no_data: weight(p.no_data),
            imported: weight(p.imported),
            min_account_days: p.min_account_days,
            track_daily_cap: p.track_daily_cap,
            artist_daily_cap: p.artist_daily_cap,
            known_clients: p.known_clients.into_iter().collect(),
        }
    }
}

impl RankingParams {
    pub fn new(s: &RankingSettings) -> Result<Self, ParamsError> {
        let weight = |value: f64, name: &'static str| {
            if (0.0..=1.0).contains(&value) {
                Ok((value * FULL as f64).round() as i64)
            } else {
                Err(ParamsError::Weight(name))
            }
        };
        let cap = |value: i64, name: &'static str| {
            if (1..=10_000).contains(&value) {
                Ok(value)
            } else {
                Err(ParamsError::Cap(name))
            }
        };
        if !(0..=365).contains(&s.min_account_days) {
            return Err(ParamsError::AccountDays);
        }
        let mut known_clients = BTreeSet::new();
        for client in &s.known_clients {
            let client = client.trim().to_lowercase();
            match client.split_once(':') {
                Some((protocol, name)) if !protocol.is_empty() && !name.trim().is_empty() => {
                    known_clients.insert(format!("{protocol}:{}", name.trim()));
                }
                _ => return Err(ParamsError::Client(client)),
            }
        }
        Ok(Self {
            unknown_client: weight(s.unknown_client, "unknown client weight")?,
            no_listened: weight(s.no_listened, "no listened time weight")?,
            no_data: weight(s.no_data, "no data weight")?,
            imported: weight(s.imported, "import weight")?,
            min_account_days: s.min_account_days,
            track_daily_cap: cap(s.track_daily_cap, "track daily cap")?,
            artist_daily_cap: cap(s.artist_daily_cap, "artist daily cap")?,
            known_clients,
        })
    }

    /// Identifies the logic version and params; weights stored under
    /// another fingerprint are stale.
    pub fn fingerprint(&self) -> String {
        format!(
            "weights/v{WEIGHTS_VERSION} unknown_client={} no_listened={} no_data={} imported={} \
             min_account_days={} track_daily_cap={} artist_daily_cap={} known_clients={}",
            self.unknown_client,
            self.no_listened,
            self.no_data,
            self.imported,
            self.min_account_days,
            self.track_daily_cap,
            self.artist_daily_cap,
            self.known_clients
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(","),
        )
    }

    pub fn client_class(&self, protocol: &str, name: &str, verified: bool) -> ClientClass {
        if verified {
            return ClientClass::Verified;
        }
        let name = name.trim().to_lowercase();
        let known = self.known_clients.iter().any(|known| {
            known
                .strip_prefix(protocol)
                .and_then(|rest| rest.strip_prefix(':'))
                .is_some_and(|known| {
                    name == known
                        || name
                            .strip_prefix(known)
                            .is_some_and(|rest| rest.starts_with(' '))
                })
        });
        if known {
            ClientClass::Known
        } else {
            ClientClass::Unknown
        }
    }

    /// The weight that makes a full listener: one play from a known client
    /// without listened time. Less trusted plays add up to it.
    pub fn listener_weight(&self) -> i64 {
        self.no_listened.max(1)
    }

    /// A user's listener credit for an entity, in thousandths, from the sum
    /// of their weights for it.
    pub fn listener_credit(&self, weight: i64) -> i64 {
        (weight * FULL / self.listener_weight()).min(FULL)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Reason {
    Unclassified,
    Suspect,
    Duplicate,
    NoData,
    Imported,
    NewAccount,
    ShortListen,
    NoListened,
    UnknownClient,
    TrackCap,
    ArtistCap,
}

impl Reason {
    pub const ALL: [Reason; 11] = [
        Reason::Unclassified,
        Reason::Suspect,
        Reason::Duplicate,
        Reason::NoData,
        Reason::Imported,
        Reason::NewAccount,
        Reason::ShortListen,
        Reason::NoListened,
        Reason::UnknownClient,
        Reason::TrackCap,
        Reason::ArtistCap,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Unclassified => "unclassified",
            Reason::Suspect => "suspect",
            Reason::Duplicate => "duplicate",
            Reason::NoData => "no_data",
            Reason::Imported => "imported",
            Reason::NewAccount => "new_account",
            Reason::ShortListen => "short_listen",
            Reason::NoListened => "no_listened",
            Reason::UnknownClient => "unknown_client",
            Reason::TrackCap => "track_cap",
            Reason::ArtistCap => "artist_cap",
        }
    }

    fn bit(self) -> u16 {
        1 << self as u16
    }
}

/// The reasons a play weighs less than in full.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reasons(u16);

impl Reasons {
    pub fn insert(&mut self, reason: Reason) {
        self.0 |= reason.bit();
    }

    pub fn contains(self, reason: Reason) -> bool {
        self.0 & reason.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Reason> {
        Reason::ALL.into_iter().filter(move |r| self.contains(*r))
    }
}

/// One scrobble as the weighting sees it.
#[derive(Debug, Clone)]
pub struct Play {
    pub id: i64,
    pub track_id: i64,
    pub artist_id: i64,
    pub played_at: DateTime<Utc>,
    /// `None` until the scrobble's day is classified with it.
    pub status: Option<Status>,
    pub client: ClientClass,
    pub imported: bool,
    pub listened_ms: Option<i32>,
    pub mb_duration_ms: Option<i32>,
    pub catalog_duration_ms: Option<i32>,
    pub reported_duration_ms: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weight {
    pub id: i64,
    /// 0 to [`FULL`].
    pub permille: i64,
    /// Every reason that applied, including on plays already at 0. The caps
    /// are named only when they took weight away.
    pub reasons: Reasons,
}

/// The listened time a play must declare: the scrobble point (half the
/// track, at most 4 minutes) of its shortest plausible length. The length
/// the client reported counts, since that is what it timed the listen
/// against, unless it is under half of MusicBrainz's or the catalog's, the
/// signature of a client claiming short tracks. Without any length, the
/// 30 s every valid scrobble needs.
pub fn listen_point_ms(play: &Play) -> i64 {
    let positive = |ms: Option<i32>| ms.filter(|d| *d > 0).map(i64::from);
    let others = [
        positive(play.mb_duration_ms),
        positive(play.catalog_duration_ms),
    ];
    let longest_other = others.iter().flatten().copied().max();
    let reported = positive(play.reported_duration_ms)
        .filter(|r| longest_other.is_none_or(|other| r * 2 >= other));
    others
        .into_iter()
        .chain([reported])
        .flatten()
        .min()
        .map_or(i64::from(MIN_LISTEN_MS), scrobble_point_ms)
}

/// Weighs one user's plays of one UTC day. `account_age_days` is the day's
/// age of the account (negative for plays dated before it existed). Input
/// order doesn't matter: the caps go by `(played_at, id)`.
pub fn weigh(plays: &[Play], account_age_days: i64, params: &RankingParams) -> Vec<Weight> {
    let mut plays: Vec<&Play> = plays.iter().collect();
    plays.sort_by_key(|p| (p.played_at, p.id));

    let mut per_track: HashMap<i64, i64> = HashMap::new();
    let mut per_artist: HashMap<i64, i64> = HashMap::new();
    plays
        .into_iter()
        .map(|play| {
            let (mut permille, mut reasons) = base_weight(play, account_age_days, params);
            if permille > 0 {
                let track = per_track.entry(play.track_id).or_default();
                *track += 1;
                if *track > params.track_daily_cap {
                    permille = 0;
                    reasons.insert(Reason::TrackCap);
                } else {
                    let artist = per_artist.entry(play.artist_id).or_default();
                    *artist += 1;
                    if *artist > params.artist_daily_cap {
                        permille = 0;
                        reasons.insert(Reason::ArtistCap);
                    }
                }
            }
            Weight {
                id: play.id,
                permille,
                reasons,
            }
        })
        .collect()
}

fn base_weight(play: &Play, account_age_days: i64, params: &RankingParams) -> (i64, Reasons) {
    let mut permille = FULL;
    let mut reasons = Reasons::default();
    let mut apply = |factor: i64, reason: Reason| {
        if factor < FULL {
            permille = permille * factor / FULL;
            reasons.insert(reason);
        }
    };

    match play.status {
        None => apply(0, Reason::Unclassified),
        Some(Status::Counted) => {}
        Some(Status::Suspect) => apply(0, Reason::Suspect),
        Some(Status::Duplicate) => apply(0, Reason::Duplicate),
        Some(Status::NoData) => apply(params.no_data, Reason::NoData),
    }
    if play.imported {
        apply(params.imported, Reason::Imported);
        return (permille, reasons);
    }
    if account_age_days < params.min_account_days {
        apply(0, Reason::NewAccount);
    }
    match play.listened_ms {
        Some(listened) if i64::from(listened) + LISTEN_TOLERANCE_MS < listen_point_ms(play) => {
            apply(0, Reason::ShortListen)
        }
        Some(_) => {}
        None => apply(params.no_listened, Reason::NoListened),
    }
    if play.client == ClientClass::Unknown {
        apply(params.unknown_client, Reason::UnknownClient);
    }
    (permille, reasons)
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;

    fn t0() -> DateTime<Utc> {
        "2026-09-01T00:00:00Z".parse().unwrap()
    }

    /// A counted play of its own track by artist 1, from a known client
    /// that listened to the whole 200 s track.
    fn play(id: i64, at_secs: i64) -> Play {
        Play {
            id,
            track_id: id,
            artist_id: 1,
            played_at: t0() + TimeDelta::seconds(at_secs),
            status: Some(Status::Counted),
            client: ClientClass::Known,
            imported: false,
            listened_ms: Some(200_000),
            mb_duration_ms: Some(200_000),
            catalog_duration_ms: Some(200_000),
            reported_duration_ms: Some(200_000),
        }
    }

    fn one(play: Play) -> Weight {
        weigh(&[play], 30, &RankingParams::default())[0]
    }

    fn total(weights: &[Weight]) -> i64 {
        weights.iter().map(|w| w.permille).sum()
    }

    #[test]
    fn an_honest_play_weighs_in_full() {
        let w = one(play(1, 0));
        assert_eq!(w.permille, FULL);
        assert!(w.reasons.is_empty());
        let spotify = Play {
            client: ClientClass::Verified,
            ..play(1, 0)
        };
        assert_eq!(one(spotify).permille, FULL);
    }

    #[test]
    fn our_clients_pass_at_the_point_they_scrobble() {
        // The extension and the mobile app submit once the listened time
        // reaches half their own reported length (at most 4 minutes).
        for (length, listened) in [(213_400, 106_700), (600_000, 240_000), (45_000, 30_000)] {
            let p = Play {
                mb_duration_ms: Some(length),
                catalog_duration_ms: Some(length),
                reported_duration_ms: Some(length),
                listened_ms: Some(listened),
                ..play(1, 0)
            };
            assert_eq!(one(p).permille, FULL, "{length} {listened}");
        }
        // A shorter catalog or MusicBrainz length only lowers the point.
        let p = Play {
            mb_duration_ms: Some(195_000),
            reported_duration_ms: Some(210_000),
            listened_ms: Some(105_000),
            ..play(1, 0)
        };
        assert_eq!(one(p).permille, FULL);
        // The mobile app without a length waits 4 minutes.
        let p = Play {
            reported_duration_ms: None,
            listened_ms: Some(240_000),
            mb_duration_ms: Some(540_000),
            catalog_duration_ms: None,
            ..play(1, 0)
        };
        assert_eq!(one(p).permille, FULL);
    }

    #[test]
    fn short_listens_weigh_nothing() {
        let p = Play {
            listened_ms: Some(30_000),
            mb_duration_ms: Some(240_000),
            catalog_duration_ms: Some(240_000),
            reported_duration_ms: Some(240_000),
            ..play(1, 0)
        };
        let w = one(p.clone());
        assert_eq!(w.permille, 0);
        assert!(w.reasons.contains(Reason::ShortListen));
        // Claiming a 60 s track doesn't lower a 4-minute track's point...
        let claimed = Play {
            reported_duration_ms: Some(60_000),
            ..p.clone()
        };
        assert_eq!(one(claimed).permille, 0);
        // ...but a track only the client knows a length for is its word.
        let unknown = Play {
            mb_duration_ms: None,
            catalog_duration_ms: None,
            reported_duration_ms: Some(60_000),
            ..p
        };
        assert_eq!(one(unknown).permille, FULL);
    }

    #[test]
    fn a_wrong_long_length_from_others_keeps_the_clients() {
        // MusicBrainz matched an 8:30 live take of a 2:27 song.
        let p = Play {
            mb_duration_ms: Some(510_000),
            catalog_duration_ms: Some(147_000),
            reported_duration_ms: Some(147_000),
            listened_ms: Some(73_500),
            ..play(1, 0)
        };
        assert_eq!(listen_point_ms(&p), 73_500);
        assert_eq!(one(p).permille, FULL);
    }

    #[test]
    fn plays_without_listened_time_and_unknown_clients_weigh_less() {
        let compat = Play {
            listened_ms: None,
            ..play(1, 0)
        };
        let w = one(compat.clone());
        assert_eq!(w.permille, 250);
        assert!(w.reasons.contains(Reason::NoListened));
        let unknown = Play {
            client: ClientClass::Unknown,
            ..compat
        };
        let w = one(unknown);
        assert_eq!(w.permille, 125);
        assert!(w.reasons.contains(Reason::UnknownClient));
        let native_unknown = Play {
            client: ClientClass::Unknown,
            ..play(1, 0)
        };
        assert_eq!(one(native_unknown).permille, 500);
    }

    #[test]
    fn labels_set_the_base() {
        for (status, expected) in [
            (Some(Status::Counted), FULL),
            (Some(Status::NoData), 500),
            (Some(Status::Suspect), 0),
            (Some(Status::Duplicate), 0),
            (None, 0),
        ] {
            let p = Play {
                status,
                ..play(1, 0)
            };
            assert_eq!(one(p).permille, expected, "{status:?}");
        }
    }

    #[test]
    fn imports_weigh_only_their_own_factor() {
        let imported = Play {
            imported: true,
            client: ClientClass::Unknown,
            listened_ms: None,
            ..play(1, 0)
        };
        let w = weigh(
            std::slice::from_ref(&imported),
            -400,
            &RankingParams::default(),
        )[0];
        assert_eq!(w.permille, 0);
        assert_eq!(w.reasons.iter().collect::<Vec<_>>(), [Reason::Imported]);
        let counted = RankingParams {
            imported: FULL,
            ..Default::default()
        };
        assert_eq!(weigh(&[imported], -400, &counted)[0].permille, FULL);
    }

    #[test]
    fn a_new_accounts_plays_weigh_nothing() {
        let params = RankingParams::default();
        for (age, expected) in [(-3, 0), (0, 0), (6, 0), (7, FULL), (400, FULL)] {
            assert_eq!(weigh(&[play(1, 0)], age, &params)[0].permille, expected);
        }
        let w = weigh(&[play(1, 0)], 2, &params)[0];
        assert!(w.reasons.contains(Reason::NewAccount));
    }

    #[test]
    fn a_track_looped_all_night_counts_like_a_fans_day() {
        // 160 plays of a 3-minute track, back to back.
        let plays: Vec<Play> = (0..160)
            .map(|n| Play {
                track_id: 7,
                mb_duration_ms: Some(180_000),
                catalog_duration_ms: Some(180_000),
                reported_duration_ms: Some(180_000),
                listened_ms: Some(180_000),
                ..play(n, n * 180)
            })
            .collect();
        let weights = weigh(&plays, 30, &RankingParams::default());
        assert_eq!(total(&weights), 4 * FULL);
        assert!(weights[..4].iter().all(|w| w.permille == FULL));
        assert!(
            weights[4..]
                .iter()
                .all(|w| w.permille == 0 && w.reasons.contains(Reason::TrackCap))
        );
    }

    #[test]
    fn an_artist_counts_at_most_its_daily_cap() {
        let plays: Vec<Play> = (0..50).map(|n| play(n, n * 200)).collect();
        let weights = weigh(&plays, 30, &RankingParams::default());
        assert_eq!(total(&weights), 30 * FULL);
        assert_eq!(
            weights
                .iter()
                .filter(|w| w.reasons.contains(Reason::ArtistCap))
                .count(),
            20
        );
        // Other artists keep their own allowance.
        let mut mixed = plays.clone();
        mixed.extend((100..110).map(|n| Play {
            artist_id: 2,
            ..play(n, n * 200)
        }));
        assert_eq!(
            total(&weigh(&mixed, 30, &RankingParams::default())),
            40 * FULL
        );
    }

    #[test]
    fn plays_that_weigh_nothing_use_no_allowance() {
        let mut plays: Vec<Play> = (0..10)
            .map(|n| Play {
                track_id: 7,
                status: Some(Status::Duplicate),
                ..play(n, n)
            })
            .collect();
        plays.extend((10..14).map(|n| Play {
            track_id: 7,
            ..play(n, 100 + n * 200)
        }));
        let weights = weigh(&plays, 30, &RankingParams::default());
        assert_eq!(total(&weights), 4 * FULL);
        assert!(
            weights
                .iter()
                .all(|w| !w.reasons.contains(Reason::TrackCap))
        );
    }

    #[test]
    fn caps_count_plays_whatever_they_weigh() {
        // A looper without listened time reaches the cap as fast as one
        // with it, so trust can't be traded for volume.
        let plays: Vec<Play> = (0..100)
            .map(|n| Play {
                track_id: 7,
                listened_ms: None,
                ..play(n, n * 200)
            })
            .collect();
        assert_eq!(
            total(&weigh(&plays, 30, &RankingParams::default())),
            4 * 250
        );
    }

    #[test]
    fn weighing_is_deterministic_and_order_independent() {
        let mut plays: Vec<Play> = (0..300)
            .map(|n| Play {
                track_id: n % 13,
                artist_id: n % 3,
                listened_ms: (n % 4 != 0).then_some(150_000),
                ..play(n, n * 37)
            })
            .collect();
        let params = RankingParams::default();
        let first = weigh(&plays, 30, &params);
        plays.reverse();
        let mut second = weigh(&plays, 30, &params);
        second.sort_by_key(|w| first.iter().position(|f| f.id == w.id));
        assert_eq!(first, second);
    }

    #[test]
    fn clients_are_classed_by_protocol_and_name() {
        let params = RankingParams::default();
        assert_eq!(
            params.client_class("scrobblr", "ytmusic", false),
            ClientClass::Known
        );
        assert_eq!(
            params.client_class("listenbrainz", "Web Scrobbler 3.14.0", false),
            ClientClass::Known
        );
        assert_eq!(
            params.client_class("listenbrainz", "Web Scrobblerx", false),
            ClientClass::Unknown
        );
        assert_eq!(
            params.client_class("lastfm", "ytmusic", false),
            ClientClass::Unknown
        );
        assert_eq!(
            params.client_class("lastfm", "0123abcd", true),
            ClientClass::Verified
        );
        assert_eq!(
            params.client_class("scrobblr", "farmbot", false),
            ClientClass::Unknown
        );
    }

    #[test]
    fn one_ordinary_play_makes_a_full_listener() {
        let params = RankingParams::default();
        assert_eq!(params.listener_credit(250), FULL);
        assert_eq!(params.listener_credit(4000), FULL);
        assert_eq!(params.listener_credit(125), 500);
        assert_eq!(params.listener_credit(0), 0);
    }

    #[test]
    fn params_are_validated_and_fingerprinted() {
        let defaults = RankingSettings::default();
        let params = RankingParams::new(&defaults).unwrap();
        assert_eq!(params, RankingParams::default());
        assert!(
            RankingParams::new(&RankingSettings {
                no_listened: 1.5,
                ..defaults.clone()
            })
            .is_err()
        );
        assert!(
            RankingParams::new(&RankingSettings {
                track_daily_cap: 0,
                ..defaults.clone()
            })
            .is_err()
        );
        assert!(
            RankingParams::new(&RankingSettings {
                known_clients: vec!["ytmusic".into()],
                ..defaults.clone()
            })
            .is_err()
        );
        let other = RankingParams::new(&RankingSettings {
            no_listened: 0.3,
            ..defaults
        })
        .unwrap();
        assert_ne!(params.fingerprint(), other.fingerprint());
        assert!(params.fingerprint().starts_with("weights/v1 "));
    }
}
