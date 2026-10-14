//! Artist similarity, recommendations and taste neighbours.
//!
//! **Input.** Each account's listening over a window of days, as global
//! rankings weigh it ([`crate::ranking`]): imports, plays before an account
//! is a week old, suspect plays and duplicates weigh nothing, and the daily
//! caps hold loops to an ordinary day. An account's **credit** for an artist
//! is its weight for it against [`SimilarityParams::full_plays`] full plays,
//! at most 1, times its coordination factor ([`crate::coordination`]); an
//! account under [`SimilarityParams::min_factor`] adds nothing. A credit is
//! at most one account, so everything below counts accounts, never plays.
//!
//! **Artist neighbours.** For an artist `a` with audience `L_a` (the sum of
//! its credits) and a candidate `b`, `co` is the credit mass of the accounts
//! listening to both. A candidate needs [`SimilarityParams::min_support`]
//! such accounts, `co` at least [`SimilarityParams::min_share`] of `L_a`,
//! and more than chance: `E = L_a × L_b / N` over the `N` contributing
//! accounts. Its score is the excess over chance against the geometric mean
//! of the audiences, shrunk while the evidence is small:
//!
//! ```text
//! score = (co − E) / √(L_a × L_b) × co / (co + shrink)
//! ```
//!
//! Lists are directional: the share floor is the source's, so putting an
//! artist on a popular artist's list takes that share of its audience.
//!
//! **Recommendations** (item-based): a user's artists, by plays decayed
//! with a half-life, each weighed `ln(1 + plays)`, vote for their
//! neighbours by score; artists the user ever played or skipped are left
//! out, and each recommendation names the artist that voted most for it.
//! The user's own history (imports included) only steers their own list.
//!
//! **Taste neighbours**: cosine between a user's history and the credits of
//! accounts that chose to be listed, each artist weighed by its rarity
//! (`ln(N / L)`), through an inverted index whose postings keep each
//! artist's [`SimilarityParams::max_postings`] strongest accounts, then
//! scored exactly; shrunk by the artists shared and times the candidate's
//! coordination factor. Linear in users for a bounded history and postings:
//! no pair of users is compared outside a shared artist.
//!
//! Pure and deterministic: results depend only on the input and params,
//! whatever their order.

use std::collections::{BTreeMap, HashMap, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ts_rs::TS;

pub use crate::ranking::FULL;

/// Bump when the logic changes, so stored results are recomputed.
pub const SIMILARITY_VERSION: u32 = 1;

/// How long a skipped recommendation stays hidden.
pub const SKIP_DAYS: i64 = 90;

/// Top shared artists kept to explain a taste neighbour.
pub const TOP_SHARED: usize = 3;

/// Whether an account's listening is used for similarity, and whether it
/// may be shown as someone's taste neighbour.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS,
)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum SimilarityMode {
    /// Counted anonymously, and shown as a taste neighbour of others (while
    /// the profile is public).
    Listed,
    /// Counted anonymously; never shown as anyone's taste neighbour.
    #[default]
    Anonymous,
    /// Not used at all: nothing is computed from or for the account.
    Off,
}

impl SimilarityMode {
    pub const ALL: [SimilarityMode; 3] = [
        SimilarityMode::Listed,
        SimilarityMode::Anonymous,
        SimilarityMode::Off,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SimilarityMode::Listed => "listed",
            SimilarityMode::Anonymous => "anonymous",
            SimilarityMode::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Option<SimilarityMode> {
        SimilarityMode::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimilarityParams {
    /// Days of weights read, ending today.
    pub window_days: i64,
    /// Full-weight plays that make a full credit.
    pub full_plays: i64,
    /// Coordination factor (thousandths) under which an account adds
    /// nothing.
    pub min_factor: i64,
    /// Audience (accounts) an artist needs to have neighbours or be one.
    pub min_listeners: i64,
    /// Accounts listening to both that a pair needs.
    pub min_support: i64,
    /// Share (thousandths) of the source's audience a pair needs.
    pub min_share: i64,
    /// Accounts of evidence at which a score keeps half its value.
    pub shrink: i64,
    /// Neighbours kept per artist.
    pub artist_neighbours: i64,
    /// Artists of an account read, its strongest.
    pub user_artists: i64,
    /// Distinct artists a user needs for recommendations and neighbours.
    pub min_user_artists: i64,
    /// Recommendations kept per user.
    pub recommendations: i64,
    /// Taste neighbours kept per user.
    pub taste_neighbours: i64,
    /// Artists two users need in common.
    pub min_shared: i64,
    /// Half-life of a user's own plays, in days.
    pub half_life_days: i64,
    /// Accounts kept per artist in the taste index.
    pub max_postings: i64,
    /// How often the results are recomputed.
    pub refresh_hours: i64,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ParamsError(String);

/// Settings as given, before validation.
#[derive(Debug, Clone)]
pub struct SimilaritySettings {
    pub window_days: i64,
    pub full_plays: i64,
    pub min_factor: f64,
    pub min_listeners: i64,
    pub min_support: i64,
    pub min_share: f64,
    pub shrink: i64,
    pub artist_neighbours: i64,
    pub user_artists: i64,
    pub min_user_artists: i64,
    pub recommendations: i64,
    pub taste_neighbours: i64,
    pub min_shared: i64,
    pub half_life_days: i64,
    pub max_postings: i64,
    pub refresh_hours: i64,
}

impl Default for SimilarityParams {
    fn default() -> Self {
        Self {
            window_days: 180,
            full_plays: 4,
            min_factor: 950,
            min_listeners: 10,
            min_support: 10,
            min_share: 100,
            shrink: 10,
            artist_neighbours: 50,
            user_artists: 200,
            min_user_artists: 5,
            recommendations: 100,
            taste_neighbours: 50,
            min_shared: 3,
            half_life_days: 180,
            max_postings: 1000,
            refresh_hours: 24,
        }
    }
}

impl Default for SimilaritySettings {
    fn default() -> Self {
        let p = SimilarityParams::default();
        let f = |permille: i64| permille as f64 / FULL as f64;
        Self {
            window_days: p.window_days,
            full_plays: p.full_plays,
            min_factor: f(p.min_factor),
            min_listeners: p.min_listeners,
            min_support: p.min_support,
            min_share: f(p.min_share),
            shrink: p.shrink,
            artist_neighbours: p.artist_neighbours,
            user_artists: p.user_artists,
            min_user_artists: p.min_user_artists,
            recommendations: p.recommendations,
            taste_neighbours: p.taste_neighbours,
            min_shared: p.min_shared,
            half_life_days: p.half_life_days,
            max_postings: p.max_postings,
            refresh_hours: p.refresh_hours,
        }
    }
}

impl SimilarityParams {
    pub fn new(s: &SimilaritySettings) -> Result<Self, ParamsError> {
        let fraction = |value: f64, name: &str| {
            if (0.0..=1.0).contains(&value) {
                Ok((value * FULL as f64).round() as i64)
            } else {
                Err(ParamsError(format!("{name} must be between 0 and 1")))
            }
        };
        let range = |value: i64, min: i64, max: i64, name: &str| {
            if (min..=max).contains(&value) {
                Ok(value)
            } else {
                Err(ParamsError(format!(
                    "{name} must be between {min} and {max}"
                )))
            }
        };
        Ok(Self {
            window_days: range(s.window_days, 7, 365, "window days")?,
            full_plays: range(s.full_plays, 1, 100, "full plays")?,
            min_factor: fraction(s.min_factor, "minimum factor")?,
            min_listeners: range(s.min_listeners, 2, 1_000_000, "minimum listeners")?,
            min_support: range(s.min_support, 2, 1_000_000, "minimum support")?,
            min_share: fraction(s.min_share, "minimum share")?,
            shrink: range(s.shrink, 0, 1_000_000, "shrink")?,
            artist_neighbours: range(s.artist_neighbours, 1, 200, "artist neighbours")?,
            user_artists: range(s.user_artists, 10, 2000, "user artists")?,
            min_user_artists: range(s.min_user_artists, 1, 1000, "minimum user artists")?,
            recommendations: range(s.recommendations, 1, 500, "recommendations")?,
            taste_neighbours: range(s.taste_neighbours, 1, 200, "taste neighbours")?,
            min_shared: range(s.min_shared, 1, 1000, "minimum shared artists")?,
            half_life_days: range(s.half_life_days, 1, 3650, "half-life days")?,
            max_postings: range(s.max_postings, 10, 1_000_000, "maximum postings")?,
            refresh_hours: range(s.refresh_hours, 1, 168, "refresh hours")?,
        })
    }

    /// Identifies the logic version and params; results stored under
    /// another fingerprint are redone.
    pub fn fingerprint(&self) -> String {
        format!(
            "similarity/v{SIMILARITY_VERSION} window_days={} full_plays={} min_factor={} \
             min_listeners={} min_support={} min_share={} shrink={} artist_neighbours={} \
             user_artists={} min_user_artists={} recommendations={} taste_neighbours={} \
             min_shared={} half_life_days={} max_postings={} refresh_hours={}",
            self.window_days,
            self.full_plays,
            self.min_factor,
            self.min_listeners,
            self.min_support,
            self.min_share,
            self.shrink,
            self.artist_neighbours,
            self.user_artists,
            self.min_user_artists,
            self.recommendations,
            self.taste_neighbours,
            self.min_shared,
            self.half_life_days,
            self.max_postings,
            self.refresh_hours,
        )
    }
}

/// An account whose listening may count: not opted out, not waiting to be
/// erased.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contributor {
    pub user_id: i64,
    /// Its coordination factor for the window, thousandths.
    pub factor: i64,
    /// Opted in to be shown as a taste neighbour, with a public profile.
    pub listed: bool,
}

/// An account's weight for an artist over the window, thousandths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Credit {
    pub user_id: i64,
    pub artist_id: i64,
    pub weight: i64,
}

/// A user to serve: every artist they ever played (imports included), with
/// its plays decayed by age.
#[derive(Debug, Clone, PartialEq)]
pub struct History {
    pub user_id: i64,
    pub artists: Vec<(i64, f64)>,
}

#[derive(Debug, Clone, Default)]
pub struct Input {
    pub contributors: Vec<Contributor>,
    pub credits: Vec<Credit>,
    pub histories: Vec<History>,
    /// (user, artist) recommendations the user skipped.
    pub skips: Vec<(i64, i64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtistNeighbour {
    pub artist_id: i64,
    /// Thousandths.
    pub score: i64,
    /// Accounts listening to both.
    pub listeners: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recommendation {
    pub artist_id: i64,
    /// Thousandths; comparable within one user's list only.
    pub score: i64,
    /// The user's artist that voted most for it.
    pub because: i64,
    /// The user's artists that voted for it.
    pub because_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TasteNeighbour {
    pub user_id: i64,
    /// Thousandths.
    pub score: i64,
    /// Artists in common.
    pub shared: i64,
    /// The ones adding most to the score, at most [`TOP_SHARED`].
    pub top_shared: Vec<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// Accounts with some credit.
    pub contributors: i64,
    /// Listed accounts with enough artists to be taste neighbours.
    pub candidates: i64,
    pub artists: BTreeMap<i64, Vec<ArtistNeighbour>>,
    pub recommendations: BTreeMap<i64, Vec<Recommendation>>,
    pub neighbours: BTreeMap<i64, Vec<TasteNeighbour>>,
}

pub fn compute(input: &Input, params: &SimilarityParams) -> Output {
    let matrix = Matrix::new(&input.contributors, &input.credits, params);
    let artists = artist_neighbours(&matrix, params);
    let recommendations = recommend(&artists, &input.histories, &input.skips, params);
    let neighbours = taste_neighbours(&matrix, &input.histories, params);
    Output {
        contributors: matrix.users.len() as i64,
        candidates: matrix.candidates().count() as i64,
        artists,
        recommendations,
        neighbours,
    }
}

/// Credits by account and by artist, dense indices in id order.
#[derive(Debug, Clone, Default)]
pub struct Matrix {
    users: Vec<i64>,
    factors: Vec<f64>,
    listed: Vec<bool>,
    artists: Vec<i64>,
    artist_index: HashMap<i64, u32>,
    by_user: Vec<Vec<(u32, f64)>>,
    by_artist: Vec<Vec<(u32, f64)>>,
    audience: Vec<f64>,
}

impl Matrix {
    pub fn new(contributors: &[Contributor], credits: &[Credit], p: &SimilarityParams) -> Self {
        let known: HashMap<i64, Contributor> = contributors
            .iter()
            .filter(|c| c.factor >= p.min_factor)
            .map(|c| (c.user_id, *c))
            .collect();
        let mut weights: BTreeMap<i64, BTreeMap<i64, i64>> = BTreeMap::new();
        for c in credits {
            if c.weight > 0 && known.contains_key(&c.user_id) {
                *weights
                    .entry(c.user_id)
                    .or_default()
                    .entry(c.artist_id)
                    .or_default() += c.weight;
            }
        }
        let full = (p.full_plays * FULL) as f64;
        let mut per_user: Vec<(i64, Vec<(i64, f64)>)> = Vec::with_capacity(weights.len());
        for (user, artists) in weights {
            let factor = known[&user].factor as f64 / FULL as f64;
            let mut credits: Vec<(i64, f64)> = artists
                .into_iter()
                .map(|(artist, w)| (artist, (w as f64 / full).min(1.0) * factor))
                .filter(|(_, c)| *c > 0.0)
                .collect();
            credits.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            credits.truncate(p.user_artists as usize);
            if !credits.is_empty() {
                per_user.push((user, credits));
            }
        }
        let mut artists: Vec<i64> = per_user
            .iter()
            .flat_map(|(_, c)| c.iter().map(|(a, _)| *a))
            .collect();
        artists.sort_unstable();
        artists.dedup();
        let artist_index: HashMap<i64, u32> = artists
            .iter()
            .enumerate()
            .map(|(i, a)| (*a, i as u32))
            .collect();
        let mut m = Matrix {
            by_artist: vec![Vec::new(); artists.len()],
            audience: vec![0.0; artists.len()],
            artists,
            artist_index,
            ..Default::default()
        };
        for (u, (user, credits)) in per_user.into_iter().enumerate() {
            let c = known[&user];
            m.users.push(user);
            m.factors.push(c.factor as f64 / FULL as f64);
            m.listed.push(c.listed);
            let mut row: Vec<(u32, f64)> = credits
                .into_iter()
                .map(|(a, credit)| (m.artist_index[&a], credit))
                .collect();
            row.sort_by_key(|(a, _)| *a);
            for &(a, credit) in &row {
                m.by_artist[a as usize].push((u as u32, credit));
                m.audience[a as usize] += credit;
            }
            m.by_user.push(row);
        }
        m
    }

    /// Accounts with some credit.
    pub fn accounts(&self) -> usize {
        self.users.len()
    }

    /// An artist's audience: the sum of its credits.
    pub fn audience(&self, artist_id: i64) -> f64 {
        self.artist_index
            .get(&artist_id)
            .map_or(0.0, |&a| self.audience[a as usize])
    }

    /// Artists with some credit, the largest audience first.
    pub fn by_popularity(&self) -> Vec<i64> {
        let mut order: Vec<usize> = (0..self.artists.len()).collect();
        order.sort_by(|&a, &b| {
            self.audience[b]
                .total_cmp(&self.audience[a])
                .then(self.artists[a].cmp(&self.artists[b]))
        });
        order.into_iter().map(|a| self.artists[a]).collect()
    }

    fn candidates(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.users.len()).filter(|&u| self.listed[u])
    }

    fn idf(&self) -> Vec<f64> {
        let n = self.users.len() as f64;
        self.audience
            .iter()
            .map(|&l| if l > 0.0 { (n / l).ln().max(0.0) } else { 0.0 })
            .collect()
    }
}

pub fn artist_neighbours(m: &Matrix, p: &SimilarityParams) -> BTreeMap<i64, Vec<ArtistNeighbour>> {
    let n = m.users.len() as f64;
    let min_listeners = p.min_listeners as f64;
    let share = p.min_share as f64 / FULL as f64;
    let shrink = p.shrink as f64;
    let mut co = vec![0.0f64; m.artists.len()];
    let mut count = vec![0u32; m.artists.len()];
    let mut touched: Vec<u32> = Vec::new();
    let mut lists = BTreeMap::new();
    for a in 0..m.artists.len() {
        let la = m.audience[a];
        if la < min_listeners {
            continue;
        }
        for &(u, ca) in &m.by_artist[a] {
            for &(b, cb) in &m.by_user[u as usize] {
                if b as usize == a {
                    continue;
                }
                if count[b as usize] == 0 {
                    touched.push(b);
                }
                co[b as usize] += ca * cb;
                count[b as usize] += 1;
            }
        }
        let mut list = Vec::new();
        for &b in &touched {
            let (b, both, accounts) = (b as usize, co[b as usize], count[b as usize]);
            co[b] = 0.0;
            count[b] = 0;
            let lb = m.audience[b];
            let expected = la * lb / n;
            if i64::from(accounts) < p.min_support
                || both < share * la
                || lb < min_listeners
                || both <= expected
            {
                continue;
            }
            let score = (both - expected) / (la * lb).sqrt() * both / (both + shrink);
            let score = (score * FULL as f64).round() as i64;
            if score > 0 {
                list.push(ArtistNeighbour {
                    artist_id: m.artists[b],
                    score,
                    listeners: i64::from(accounts),
                });
            }
        }
        touched.clear();
        if list.is_empty() {
            continue;
        }
        list.sort_by(|x, y| y.score.cmp(&x.score).then(x.artist_id.cmp(&y.artist_id)));
        list.truncate(p.artist_neighbours as usize);
        lists.insert(m.artists[a], list);
    }
    lists
}

/// A history's strongest artists, `ln(1 + plays)` each.
fn strongest(history: &History, p: &SimilarityParams) -> Vec<(i64, f64)> {
    let mut artists: Vec<(i64, f64)> = history
        .artists
        .iter()
        .filter(|(_, plays)| *plays > 0.0)
        .map(|&(a, plays)| (a, plays.ln_1p()))
        .collect();
    artists.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    artists.dedup_by_key(|(a, _)| *a);
    artists.truncate(p.user_artists as usize);
    artists
}

fn distinct_artists(history: &History) -> usize {
    history
        .artists
        .iter()
        .map(|(a, _)| *a)
        .collect::<HashSet<_>>()
        .len()
}

pub fn recommend(
    lists: &BTreeMap<i64, Vec<ArtistNeighbour>>,
    histories: &[History],
    skips: &[(i64, i64)],
    p: &SimilarityParams,
) -> BTreeMap<i64, Vec<Recommendation>> {
    let mut skipped: HashMap<i64, HashSet<i64>> = HashMap::new();
    for &(user, artist) in skips {
        skipped.entry(user).or_default().insert(artist);
    }
    let none = HashSet::new();
    let mut out = BTreeMap::new();
    for h in histories {
        if (distinct_artists(h) as i64) < p.min_user_artists {
            continue;
        }
        let listened: HashSet<i64> = h.artists.iter().map(|(a, _)| *a).collect();
        let skipped = skipped.get(&h.user_id).unwrap_or(&none);
        // artist -> (score, best vote, its artist, votes)
        let mut votes: HashMap<i64, (f64, f64, i64, i64)> = HashMap::new();
        for (a, strength) in strongest(h, p) {
            let Some(list) = lists.get(&a) else { continue };
            for nb in list {
                if listened.contains(&nb.artist_id) || skipped.contains(&nb.artist_id) {
                    continue;
                }
                let vote = strength * nb.score as f64 / FULL as f64;
                let v = votes.entry(nb.artist_id).or_insert((0.0, -1.0, a, 0));
                v.0 += vote;
                v.3 += 1;
                if vote > v.1 || (vote == v.1 && a < v.2) {
                    v.1 = vote;
                    v.2 = a;
                }
            }
        }
        let mut recs: Vec<Recommendation> = votes
            .into_iter()
            .map(|(artist_id, (score, _, because, count))| Recommendation {
                artist_id,
                score: (score * FULL as f64).round() as i64,
                because,
                because_count: count,
            })
            .filter(|r| r.score > 0)
            .collect();
        if recs.is_empty() {
            continue;
        }
        recs.sort_by(|x, y| y.score.cmp(&x.score).then(x.artist_id.cmp(&y.artist_id)));
        recs.truncate(p.recommendations as usize);
        out.insert(h.user_id, recs);
    }
    out
}

/// A sparse vector, by artist index, of unit length.
fn unit(mut v: Vec<(u32, f64)>) -> Vec<(u32, f64)> {
    v.retain(|(_, w)| *w > 0.0);
    v.sort_by_key(|(a, _)| *a);
    let norm = v.iter().map(|(_, w)| w * w).sum::<f64>().sqrt();
    if norm > 0.0 {
        for (_, w) in &mut v {
            *w /= norm;
        }
    }
    v
}

/// The dot product, the artists in common and their contributions.
fn overlap(q: &[(u32, f64)], x: &[(u32, f64)]) -> (f64, Vec<(u32, f64)>) {
    let (mut i, mut j) = (0, 0);
    let mut dot = 0.0;
    let mut shared = Vec::new();
    while i < q.len() && j < x.len() {
        match q[i].0.cmp(&x[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                let c = q[i].1 * x[j].1;
                dot += c;
                shared.push((q[i].0, c));
                i += 1;
                j += 1;
            }
        }
    }
    (dot, shared)
}

pub fn taste_neighbours(
    m: &Matrix,
    histories: &[History],
    p: &SimilarityParams,
) -> BTreeMap<i64, Vec<TasteNeighbour>> {
    let idf = m.idf();
    let candidates: Vec<usize> = m
        .candidates()
        .filter(|&u| m.by_user[u].len() as i64 >= p.min_user_artists)
        .collect();
    let vectors: Vec<Vec<(u32, f64)>> = candidates
        .iter()
        .map(|&u| {
            unit(
                m.by_user[u]
                    .iter()
                    .map(|&(a, c)| (a, c * idf[a as usize]))
                    .collect(),
            )
        })
        .collect();
    let mut postings: Vec<Vec<(u32, f64)>> = vec![Vec::new(); m.artists.len()];
    for (c, v) in vectors.iter().enumerate() {
        for &(a, w) in v {
            postings[a as usize].push((c as u32, w));
        }
    }
    for list in &mut postings {
        list.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.cmp(&y.0)));
        list.truncate(p.max_postings as usize);
    }

    let mut dot = vec![0.0f64; candidates.len()];
    let mut seen = vec![false; candidates.len()];
    let mut touched: Vec<u32> = Vec::new();
    let mut out = BTreeMap::new();
    let shrink = p.min_shared as f64;
    let score = |c: usize, dot: f64, shared: usize| {
        dot * shared as f64 / (shared as f64 + shrink) * m.factors[candidates[c]]
    };
    for h in histories {
        if (distinct_artists(h) as i64) < p.min_user_artists {
            continue;
        }
        let q = unit(
            strongest(h, p)
                .into_iter()
                .filter_map(|(a, s)| {
                    let a = *m.artist_index.get(&a)?;
                    Some((a, s * idf[a as usize]))
                })
                .collect(),
        );
        for &(a, w) in &q {
            for &(c, x) in &postings[a as usize] {
                if !seen[c as usize] {
                    seen[c as usize] = true;
                    touched.push(c);
                }
                dot[c as usize] += w * x;
            }
        }
        let mut found: Vec<(f64, usize)> = touched
            .iter()
            .map(|&c| (dot[c as usize], c as usize))
            .filter(|&(_, c)| m.users[candidates[c]] != h.user_id)
            .collect();
        for &c in &touched {
            dot[c as usize] = 0.0;
            seen[c as usize] = false;
        }
        touched.clear();
        found.sort_by(|x, y| {
            y.0.total_cmp(&x.0)
                .then(m.users[candidates[x.1]].cmp(&m.users[candidates[y.1]]))
        });
        found.truncate(2 * p.taste_neighbours as usize);
        let mut list: Vec<TasteNeighbour> = found
            .into_iter()
            .filter_map(|(_, c)| {
                let (dot, mut shared) = overlap(&q, &vectors[c]);
                if (shared.len() as i64) < p.min_shared {
                    return None;
                }
                let s = (score(c, dot, shared.len()) * FULL as f64).round() as i64;
                shared.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.cmp(&y.0)));
                Some(TasteNeighbour {
                    user_id: m.users[candidates[c]],
                    score: s,
                    shared: shared.len() as i64,
                    top_shared: shared
                        .iter()
                        .take(TOP_SHARED)
                        .map(|(a, _)| m.artists[*a as usize])
                        .collect(),
                })
            })
            .filter(|n| n.score > 0)
            .collect();
        if list.is_empty() {
            continue;
        }
        list.sort_by(|x, y| y.score.cmp(&x.score).then(x.user_id.cmp(&y.user_id)));
        list.truncate(p.taste_neighbours as usize);
        out.insert(h.user_id, list);
    }
    out
}

/// Offline evaluation: recommendations made from history up to a day,
/// against the artists each user first played after it.
pub mod eval {
    use std::collections::{HashMap, HashSet};

    #[derive(Debug, Clone, Default)]
    pub struct Case {
        /// Best first.
        pub recommended: Vec<i64>,
        /// Artists new to the user that they then played.
        pub targets: HashSet<i64>,
    }

    #[derive(Debug, Clone, Copy, Default, PartialEq)]
    pub struct Metrics {
        /// Users with targets and recommendations.
        pub users: usize,
        /// Users with a target in their first `n`.
        pub hit_rate: f64,
        /// Targets found in the first `n`, against at most `n` per user.
        pub recall: f64,
        pub ndcg: f64,
        /// Distinct artists recommended in the first `n`, over all users.
        pub coverage: usize,
        /// Mean popularity percentile of what was recommended (0 the most
        /// popular artist, 100 the least) and of the targets.
        pub popularity: f64,
        pub target_popularity: f64,
    }

    /// `popularity` ranks artists from the most popular (0); `ranked` is how
    /// many it ranks, and any artist it doesn't rank counts as the least.
    pub fn evaluate(
        cases: &[Case],
        n: usize,
        popularity: &HashMap<i64, usize>,
        ranked: usize,
    ) -> Metrics {
        let percentile = |a: &i64| {
            popularity
                .get(a)
                .map_or(100.0, |&r| 100.0 * r as f64 / ranked.max(1) as f64)
        };
        let mut m = Metrics::default();
        let mut recommended = HashSet::new();
        let (mut pop, mut pops, mut target_pop, mut target_pops) = (0.0, 0usize, 0.0, 0usize);
        for case in cases {
            let top: Vec<i64> = case.recommended.iter().take(n).copied().collect();
            if case.targets.is_empty() || top.is_empty() {
                continue;
            }
            m.users += 1;
            let hits: Vec<bool> = top.iter().map(|a| case.targets.contains(a)).collect();
            let found = hits.iter().filter(|h| **h).count();
            let wanted = case.targets.len().min(n);
            if found > 0 {
                m.hit_rate += 1.0;
            }
            m.recall += found as f64 / wanted as f64;
            let dcg: f64 = hits
                .iter()
                .enumerate()
                .filter(|(_, h)| **h)
                .map(|(i, _)| 1.0 / (i as f64 + 2.0).log2())
                .sum();
            let ideal: f64 = (0..wanted).map(|i| 1.0 / (i as f64 + 2.0).log2()).sum();
            m.ndcg += dcg / ideal;
            for a in &top {
                recommended.insert(*a);
                pop += percentile(a);
                pops += 1;
            }
            for a in &case.targets {
                target_pop += percentile(a);
                target_pops += 1;
            }
        }
        if m.users > 0 {
            let users = m.users as f64;
            m.hit_rate /= users;
            m.recall /= users;
            m.ndcg /= users;
        }
        m.coverage = recommended.len();
        m.popularity = if pops > 0 { pop / pops as f64 } else { 0.0 };
        m.target_popularity = if target_pops > 0 {
            target_pop / target_pops as f64
        } else {
            0.0
        };
        m
    }

    /// The popularity baseline: the most popular artists the user hasn't
    /// played, best first.
    pub fn most_popular(by_popularity: &[i64], listened: &HashSet<i64>, n: usize) -> Vec<i64> {
        by_popularity
            .iter()
            .filter(|a| !listened.contains(a))
            .take(n)
            .copied()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> SimilarityParams {
        SimilarityParams {
            min_listeners: 3,
            min_support: 3,
            min_share: 100,
            shrink: 2,
            min_user_artists: 2,
            min_shared: 2,
            ..Default::default()
        }
    }

    /// `n` accounts, user ids from `first`, each playing every artist of
    /// `artists` `plays` full plays.
    fn fans(first: i64, n: i64, artists: &[i64], plays: i64) -> Vec<Credit> {
        (first..first + n)
            .flat_map(|u| {
                artists.iter().map(move |&a| Credit {
                    user_id: u,
                    artist_id: a,
                    weight: plays * FULL,
                })
            })
            .collect()
    }

    fn contributors(credits: &[Credit], listed: bool) -> Vec<Contributor> {
        let mut ids: Vec<i64> = credits.iter().map(|c| c.user_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids.into_iter()
            .map(|user_id| Contributor {
                user_id,
                factor: FULL,
                listed,
            })
            .collect()
    }

    fn list(lists: &BTreeMap<i64, Vec<ArtistNeighbour>>, artist: i64) -> Vec<i64> {
        lists
            .get(&artist)
            .map(|l| l.iter().map(|n| n.artist_id).collect())
            .unwrap_or_default()
    }

    /// A population: two genres of 3 artists each, with 40 fans each, and
    /// 100 accounts listening to a popular artist (100) and a few others.
    fn population() -> Vec<Credit> {
        let mut credits = fans(1, 40, &[1, 2, 3, 100], 5);
        credits.extend(fans(101, 40, &[11, 12, 13, 100], 5));
        credits.extend(fans(201, 100, &[100, 50], 5));
        credits
    }

    #[test]
    fn settings_validate_and_change_the_fingerprint() {
        let p = SimilarityParams::new(&SimilaritySettings::default()).unwrap();
        assert_eq!(p, SimilarityParams::default());
        let other = SimilarityParams::new(&SimilaritySettings {
            min_share: 0.05,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(other.min_share, 50);
        assert_ne!(p.fingerprint(), other.fingerprint());
        for bad in [
            SimilaritySettings {
                min_share: 1.5,
                ..Default::default()
            },
            SimilaritySettings {
                window_days: 400,
                ..Default::default()
            },
            SimilaritySettings {
                min_support: 1,
                ..Default::default()
            },
        ] {
            assert!(SimilarityParams::new(&bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            SimilarityMode::parse("listed"),
            Some(SimilarityMode::Listed)
        );
        assert_eq!(SimilarityMode::parse("Listed"), None);
        assert_eq!(SimilarityMode::default(), SimilarityMode::Anonymous);
    }

    #[test]
    fn artists_of_a_genre_are_neighbours_and_a_popular_artist_is_nobodys() {
        let credits = population();
        let p = params();
        let m = Matrix::new(&contributors(&credits, false), &credits, &p);
        let lists = artist_neighbours(&m, &p);
        assert_eq!(list(&lists, 1), vec![2, 3]);
        assert_eq!(list(&lists, 12), vec![11, 13]);
        // Everyone plays 100: it goes with nothing beyond chance.
        assert!(!list(&lists, 1).contains(&100));
        assert_eq!(list(&lists, 100), Vec::<i64>::new());
        let first = lists[&1][0];
        assert_eq!(first.listeners, 40);
        assert!(first.score > 0 && first.score <= FULL);
    }

    #[test]
    fn a_credit_is_one_account_however_many_plays() {
        let mut credits = fans(1, 40, &[1, 2], 5);
        credits.extend(fans(41, 40, &[3, 4], 5));
        // Two accounts playing 1 and 3 ten thousand times each.
        credits.extend(fans(81, 2, &[1, 3], 10_000));
        let p = params();
        let m = Matrix::new(&contributors(&credits, false), &credits, &p);
        assert!((m.audience(1) - 42.0).abs() < 1e-9);
        let lists = artist_neighbours(&m, &p);
        assert_eq!(list(&lists, 1), vec![2], "2 accounts are under the support");
    }

    #[test]
    fn credits_count_weighted_plays_against_full_plays() {
        let credits = vec![
            Credit {
                user_id: 1,
                artist_id: 1,
                weight: 2 * FULL,
            },
            Credit {
                user_id: 1,
                artist_id: 2,
                weight: 0,
            },
            Credit {
                user_id: 2,
                artist_id: 1,
                weight: 40 * FULL,
            },
        ];
        let mut c = contributors(&credits, false);
        c[1].factor = 960;
        let p = params();
        let m = Matrix::new(&c, &credits, &p);
        // 2 of 4 full plays, and 1 at a factor of 0.96.
        assert!((m.audience(1) - 1.46).abs() < 1e-9);
        assert_eq!(m.audience(2), 0.0);
        // Under the minimum factor, nothing.
        c[1].factor = 900;
        let m = Matrix::new(&c, &credits, &p);
        assert!((m.audience(1) - 0.5).abs() < 1e-9);
        assert_eq!(m.accounts(), 1);
    }

    /// A farm playing its target T with a popular artist P: T joins P's list
    /// only once the farm is the minimum share of P's audience.
    #[test]
    fn a_farm_needs_the_minimum_share_of_the_popular_artists_audience() {
        let p = params();
        let farm_reaches = |farm: i64| {
            let mut credits = population();
            credits.extend(fans(1001, farm, &[999, 1], 5));
            let m = Matrix::new(&contributors(&credits, false), &credits, &p);
            let lists = artist_neighbours(&m, &p);
            (
                list(&lists, 1).contains(&999),
                list(&lists, 999).contains(&1),
            )
        };
        // Artist 1 has 40 listeners; with the farm's, 40 + farm. 10 % of
        // that is 4.4 for 4 accounts: under it.
        assert_eq!(farm_reaches(4), (false, true));
        assert_eq!(farm_reaches(5), (true, true));
    }

    #[test]
    fn recommendations_leave_out_what_was_played_or_skipped_and_say_why() {
        let credits = population();
        let p = params();
        let m = Matrix::new(&contributors(&credits, false), &credits, &p);
        let lists = artist_neighbours(&m, &p);
        let history = |user_id, artists: &[(i64, f64)]| History {
            user_id,
            artists: artists.to_vec(),
        };
        let histories = vec![
            history(7, &[(1, 30.0), (100, 2.0)]),
            history(8, &[(1, 30.0), (2, 1.0)]),
            history(9, &[(1, 30.0)]),
        ];
        let recs = recommend(&lists, &histories, &[(8, 3)], &p);
        let artists = |u: i64| -> Vec<i64> { recs[&u].iter().map(|r| r.artist_id).collect() };
        assert_eq!(artists(7), vec![2, 3]);
        assert_eq!(recs[&7][0].because, 1);
        assert_eq!(recs[&7][0].because_count, 1);
        // 2 played and 3 skipped: nothing left.
        assert!(!recs.contains_key(&8));
        // One artist: too little to go on.
        assert!(!recs.contains_key(&9));
    }

    #[test]
    fn taste_neighbours_are_listed_accounts_sharing_rare_artists() {
        let mut credits = population();
        let p = params();
        let mut c = contributors(&credits, true);
        // The second genre isn't listed, but for one account.
        for x in &mut c {
            if (102..=140).contains(&x.user_id) {
                x.listed = false;
            }
        }
        credits.push(Credit {
            user_id: 3,
            artist_id: 12,
            weight: FULL,
        });
        let m = Matrix::new(&c, &credits, &p);
        let histories = vec![
            History {
                user_id: 1,
                artists: vec![(1, 10.0), (2, 10.0), (3, 4.0), (100, 50.0)],
            },
            History {
                user_id: 105,
                artists: vec![(11, 10.0), (12, 10.0), (13, 10.0)],
            },
            History {
                user_id: 500,
                artists: vec![(1, 10.0), (9999, 3.0)],
            },
        ];
        let neighbours = taste_neighbours(&m, &histories, &p);
        let first = &neighbours[&1];
        assert_eq!(first.len(), 39);
        assert!(first.iter().all(|n| n.user_id != 1 && n.user_id <= 40));
        // 100 is everyone's: it tells nobody apart.
        assert_eq!(first[0].shared, 3);
        assert_eq!(first[0].top_shared, vec![1, 2, 3]);
        // Only one of its genre is listed (101), and 3 shares 12 only.
        let second: Vec<i64> = neighbours[&105].iter().map(|n| n.user_id).collect();
        assert_eq!(second, vec![101]);
        // Shares one artist the index knows with anyone: under the minimum.
        assert!(!neighbours.contains_key(&500));
    }

    #[test]
    fn results_dont_depend_on_input_order() {
        let mut credits = population();
        let p = params();
        let input = Input {
            contributors: contributors(&credits, true),
            credits: credits.clone(),
            histories: vec![History {
                user_id: 1,
                artists: vec![(1, 3.0), (2, 3.0), (3, 1.0), (100, 3.0)],
            }],
            skips: vec![],
        };
        let a = compute(&input, &p);
        credits.reverse();
        let mut contributors = input.contributors.clone();
        contributors.reverse();
        let b = compute(
            &Input {
                contributors,
                credits,
                ..input
            },
            &p,
        );
        assert_eq!(a, b);
        assert!(!a.artists.is_empty() && !a.neighbours.is_empty());
    }

    #[test]
    fn evaluation_counts_hits_recall_ndcg_coverage_and_popularity() {
        use eval::*;
        let popularity: HashMap<i64, usize> = (1..=10).map(|a| (a, a as usize - 1)).collect();
        let cases = vec![
            Case {
                recommended: vec![1, 2, 3],
                targets: [2].into(),
            },
            Case {
                recommended: vec![4, 5, 6],
                targets: [9, 10].into(),
            },
            Case {
                recommended: vec![1],
                targets: HashSet::new(),
            },
        ];
        let m = evaluate(&cases, 2, &popularity, 10);
        assert_eq!(m.users, 2);
        assert!((m.hit_rate - 0.5).abs() < 1e-9);
        assert!((m.recall - 0.5).abs() < 1e-9);
        assert!((m.ndcg - 0.5 * (1.0 / 3f64.log2())).abs() < 1e-9);
        assert_eq!(m.coverage, 4);
        assert!((m.popularity - 20.0).abs() < 1e-9);
        assert!((m.target_popularity - 60.0).abs() < 1e-9);
        let listened: HashSet<i64> = [1, 3].into();
        assert_eq!(most_popular(&[1, 2, 3, 4], &listened, 2), vec![2, 4]);
    }
}
