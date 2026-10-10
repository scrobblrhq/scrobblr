//! Cross-account coordination (anti-botting, shadow mode).
//!
//! The per-play weights ([`crate::ranking`]) judge one account at a time,
//! so a farm of accounts older than the minimum age, each playing like a
//! plausible fan, weighs like fans. What gives such a farm away is what its
//! accounts share. Detection runs over a window of days and gives each
//! account a factor from 0 to 1 that global rankings multiply its listener
//! credits and weights by; profiles never read it.
//!
//! **Groups.** Every artist and track is a candidate group: the accounts
//! that weighed something for it in the window. A farm is defined by what
//! it pushes, so it sits inside its targets' groups, and building them is
//! linear in the (account, entity) pairs: no account is ever compared with
//! every other.
//!
//! **Signals.** Within a group, a signal finds the members holding a trait
//! far more often than the active population explains:
//!
//! - [`Signal::Age`]: registered within the same few days, or the same
//!   weeks, far more often than the population did.
//! - [`Signal::Origin`]: registered from the same address or network
//!   ([`crate::origin`]).
//! - [`Signal::Client`]: scrobbled the group's entity through the same
//!   client, one few accounts use.
//! - [`Signal::Sync`]: played the group's tracks in crowds chance doesn't
//!   explain, on several days.
//!
//! Each needs a minimum excess of accounts and a z-score against the
//! population's expectation, and gives its members the share of their cell
//! the excess explains (its purity), so a cell holding a few honest accounts
//! beside a farm dents them only as much as it is impure. Two behavioural
//! signals add to them but can't act alone: [`Signal::Shape`] (listening
//! concentrated on three artists) and [`Signal::Burst`] (new to the entity
//! in a group mostly new to it).
//!
//! **Suspicion and factor.** A member's suspicion for a group is the
//! weighted sum of its evidence, capped at 1; an account's is its highest
//! over the groups it is in. One primary signal at full strength reaches
//! 0.5, so nothing short of two independent signals reaches certainty: a
//! fandom, a release, a household or a campus each share at most one trait.
//! [`CoordinationParams::factor`] turns suspicion into the factor with a
//! smoothstep between two bounds.
//!
//! Pure and deterministic, like the classifier: [`detect`] depends only on
//! its input and the params, whatever their order.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;

use chrono::{Datelike, NaiveDate};
use serde::Serialize;
use thiserror::Error;

pub use crate::ranking::{FULL, Kind};

/// Bump when the detection logic changes, so stored results are redone.
pub const COORDINATION_VERSION: u32 = 1;

/// A value of a trait counts only when this many members hold it, so pairs
/// (a couple's accounts) never make a cell.
const MIN_VALUE_MEMBERS: i64 = 3;
/// A cohort gathers around the group of its members' strongest primary
/// evidence; closer than this (thousandths of suspicion), the larger group
/// wins, so a farm meets on its main target rather than scattering over
/// groups that differ by rounding.
const HOME_STEP: i64 = 50;
/// Others needed around a play for it to be a crowd.
const MIN_CROWD: i64 = 3;
const DAY_SECS: i64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    Age,
    Origin,
    Client,
    Sync,
    Shape,
    Burst,
}

impl Signal {
    pub const ALL: [Signal; 6] = [
        Signal::Age,
        Signal::Origin,
        Signal::Client,
        Signal::Sync,
        Signal::Shape,
        Signal::Burst,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Signal::Age => "age",
            Signal::Origin => "origin",
            Signal::Client => "client",
            Signal::Sync => "sync",
            Signal::Shape => "shape",
            Signal::Burst => "burst",
        }
    }

    pub fn parse(s: &str) -> Option<Signal> {
        Signal::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// Thousandths per [`Signal`], indexed like [`Signal::ALL`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Evidence(pub [i64; 6]);

impl Evidence {
    pub fn get(&self, signal: Signal) -> i64 {
        self.0[signal as usize]
    }

    pub fn set(&mut self, signal: Signal, value: i64) {
        self.0[signal as usize] = value;
    }

    /// The signals that compare a group with the population, without the
    /// account's own behaviour.
    fn primary(&self) -> Evidence {
        let mut primary = *self;
        primary.set(Signal::Shape, 0);
        primary.set(Signal::Burst, 0);
        primary
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Not run.
    Off,
    /// Run and list cohorts; every factor stays 1.
    Report,
    /// Run, and global rankings apply the factors.
    Apply,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Report => "report",
            Mode::Apply => "apply",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        [Mode::Off, Mode::Report, Mode::Apply]
            .into_iter()
            .find(|m| m.as_str() == s.trim().to_lowercase())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinationParams {
    pub mode: Mode,
    /// Suspicion (thousandths) up to which the factor stays 1, and from
    /// which it is 0.
    pub from: i64,
    pub to: i64,
    /// Active accounts below which nothing is judged: the population is the
    /// baseline every signal compares against.
    pub min_accounts: i64,
    /// Excess accounts a signal needs in a group.
    pub min_cohort: i64,
    /// z-score a signal's excess needs against its expectation.
    pub min_z: i64,
    /// Days the groups are made over, ending today.
    pub window_days: i64,
    /// Width of a registration cohort, in days.
    pub age_days: i64,
    /// Days of plays [`Signal::Sync`] and [`Signal::Client`] read, ending
    /// today; at most 64.
    pub sync_window_days: i64,
    /// Days with crowds that make a member's listening lockstep.
    pub sync_days: i64,
    /// A crowd: others playing the same track within this many seconds...
    pub sync_secs: i64,
    /// ...at least this many times what the others playing it within this
    /// many seconds, spread evenly, would put there. Habits vary over the
    /// day, hardly within an hour.
    pub sync_context_secs: i64,
    pub sync_ratio: i64,
    /// A client is rare below this share of the accounts read (thousandths).
    pub rare_client: i64,
    /// [`Signal::Shape`] grows from 0 to full as the share of an account's
    /// weight on its top three artists goes from one bound to the other.
    pub shape_from: i64,
    pub shape_to: i64,
    /// Thousandths of suspicion each signal at full strength adds.
    pub weights: Evidence,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParamsError {
    #[error("{0}")]
    Invalid(String),
}

/// Settings as given, before validation.
#[derive(Debug, Clone)]
pub struct CoordinationSettings {
    pub mode: Mode,
    pub from: f64,
    pub to: f64,
    pub min_accounts: i64,
    pub min_cohort: i64,
    pub min_z: i64,
    pub window_days: i64,
    pub age_days: i64,
    pub sync_window_days: i64,
    pub sync_days: i64,
    pub sync_secs: i64,
    pub sync_context_secs: i64,
    pub sync_ratio: i64,
    pub rare_client: f64,
    pub shape_from: f64,
    pub shape_to: f64,
    pub weights: [f64; 6],
}

impl Default for CoordinationParams {
    fn default() -> Self {
        Self {
            mode: Mode::Apply,
            from: 400,
            to: 900,
            min_accounts: 200,
            min_cohort: 10,
            min_z: 4,
            window_days: 28,
            age_days: 7,
            sync_window_days: 14,
            sync_days: 4,
            sync_secs: 120,
            sync_context_secs: 3600,
            sync_ratio: 10,
            rare_client: 20,
            shape_from: 450,
            shape_to: 850,
            weights: Evidence([500, 500, 350, 500, 125, 125]),
        }
    }
}

impl Default for CoordinationSettings {
    fn default() -> Self {
        let p = CoordinationParams::default();
        let f = |permille: i64| permille as f64 / FULL as f64;
        Self {
            mode: p.mode,
            from: f(p.from),
            to: f(p.to),
            min_accounts: p.min_accounts,
            min_cohort: p.min_cohort,
            min_z: p.min_z,
            window_days: p.window_days,
            age_days: p.age_days,
            sync_window_days: p.sync_window_days,
            sync_days: p.sync_days,
            sync_secs: p.sync_secs,
            sync_context_secs: p.sync_context_secs,
            sync_ratio: p.sync_ratio,
            rare_client: f(p.rare_client),
            shape_from: f(p.shape_from),
            shape_to: f(p.shape_to),
            weights: p.weights.0.map(f),
        }
    }
}

impl CoordinationParams {
    pub fn new(s: &CoordinationSettings) -> Result<Self, ParamsError> {
        let invalid = |m: String| Err(ParamsError::Invalid(m));
        let fraction = |value: f64, name: &str| {
            if (0.0..=1.0).contains(&value) {
                Ok((value * FULL as f64).round() as i64)
            } else {
                Err(ParamsError::Invalid(format!(
                    "{name} must be between 0 and 1"
                )))
            }
        };
        let range = |value: i64, min: i64, max: i64, name: &str| {
            if (min..=max).contains(&value) {
                Ok(value)
            } else {
                Err(ParamsError::Invalid(format!(
                    "{name} must be between {min} and {max}"
                )))
            }
        };
        let from = fraction(s.from, "coordination from")?;
        let to = fraction(s.to, "coordination to")?;
        if to <= from {
            return invalid("coordination to must be above from".into());
        }
        let shape_from = fraction(s.shape_from, "shape from")?;
        let shape_to = fraction(s.shape_to, "shape to")?;
        if shape_to <= shape_from {
            return invalid("shape to must be above shape from".into());
        }
        let window_days = range(s.window_days, 7, 90, "window days")?;
        let sync_window_days = range(s.sync_window_days, 1, window_days.min(64), "sync window")?;
        let sync_secs = range(s.sync_secs, 10, 3600, "sync seconds")?;
        let sync_context_secs = range(s.sync_context_secs, 60, 86_400, "sync context seconds")?;
        if sync_context_secs <= sync_secs {
            return invalid("sync context seconds must be above sync seconds".into());
        }
        let mut weights = Evidence::default();
        for (signal, weight) in Signal::ALL.into_iter().zip(s.weights) {
            weights.set(signal, fraction(weight, signal.as_str())?);
        }
        Ok(Self {
            mode: s.mode,
            from,
            to,
            min_accounts: range(s.min_accounts, 1, 10_000_000, "minimum accounts")?,
            min_cohort: range(s.min_cohort, 2, 1_000_000, "minimum cohort")?,
            min_z: range(s.min_z, 1, 50, "minimum z")?,
            window_days,
            age_days: range(s.age_days, 1, 90, "age days")?,
            sync_window_days,
            sync_days: range(s.sync_days, 1, sync_window_days, "sync days")?,
            sync_secs,
            sync_context_secs,
            sync_ratio: range(s.sync_ratio, 1, 10_000, "sync ratio")?,
            rare_client: fraction(s.rare_client, "rare client")?,
            shape_from,
            shape_to,
            weights,
        })
    }

    /// Identifies the logic version and params; results stored under
    /// another fingerprint are redone.
    pub fn fingerprint(&self) -> String {
        let weights: Vec<String> = Signal::ALL
            .into_iter()
            .map(|s| format!("{}={}", s.as_str(), self.weights.get(s)))
            .collect();
        format!(
            "coordination/v{COORDINATION_VERSION} mode={} from={} to={} min_accounts={} \
             min_cohort={} min_z={} window_days={} age_days={} sync_window_days={} sync_days={} \
             sync_secs={} sync_context_secs={} sync_ratio={} rare_client={} shape={}..{} weights={}",
            self.mode.as_str(),
            self.from,
            self.to,
            self.min_accounts,
            self.min_cohort,
            self.min_z,
            self.window_days,
            self.age_days,
            self.sync_window_days,
            self.sync_days,
            self.sync_secs,
            self.sync_context_secs,
            self.sync_ratio,
            self.rare_client,
            self.shape_from,
            self.shape_to,
            weights.join(","),
        )
    }

    /// The factor (thousandths) for a suspicion: 1 up to `from`, 0 from
    /// `to`, a smoothstep between.
    pub fn factor(&self, suspicion: i64) -> i64 {
        if suspicion <= self.from {
            FULL
        } else if suspicion >= self.to {
            0
        } else {
            let t = (suspicion - self.from) * FULL / (self.to - self.from);
            FULL - t * t * (3 * FULL - 2 * t) / (FULL * FULL)
        }
    }

    /// The weighted sum of the evidence; suspicion is this capped at 1.
    fn weighted(&self, evidence: &Evidence) -> i64 {
        Signal::ALL
            .into_iter()
            .map(|s| self.weights.get(s) * evidence.get(s))
            .sum::<i64>()
            / FULL
    }
}

/// `protocol:name` without a trailing version, lowercase, so the releases
/// of one client are one client ("Pano Scrobbler 3.12" and "3.13").
pub fn client_family(protocol: &str, name: &str) -> String {
    let name = name.trim().to_lowercase();
    let is_version = |t: &str| {
        let t = t.strip_prefix('v').unwrap_or(t);
        t.starts_with(|c: char| c.is_ascii_digit())
    };
    let tokens: Vec<&str> = name
        .split_whitespace()
        .take_while(|t| !is_version(t))
        .collect();
    if tokens.is_empty() {
        format!("{protocol}:{name}")
    } else {
        format!("{protocol}:{}", tokens.join(" "))
    }
}

/// An account active in the window: some weight for global rankings.
#[derive(Debug, Clone)]
pub struct Account {
    pub user_id: i64,
    /// The UTC day it was created.
    pub registered: NaiveDate,
    pub origin: Option<crate::origin::Origin>,
    /// Its weight in the window, and the part on its top three artists.
    pub weight: i64,
    pub top3_weight: i64,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub user_id: i64,
    /// Its weight for the entity in the window.
    pub weight: i64,
    /// It weighed nothing for the entity in the period before the window.
    pub new: bool,
}

/// The accounts that weighed something for an artist or track.
#[derive(Debug, Clone)]
pub struct Group {
    pub kind: Kind,
    pub entity_id: i64,
    pub members: Vec<Member>,
}

/// A scrobble of the sync window.
#[derive(Debug, Clone)]
pub struct Play {
    pub user_id: i64,
    pub track_id: i64,
    pub artist_id: i64,
    /// Unix seconds.
    pub at: i64,
    /// An index into [`Input::clients`].
    pub client: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct Input {
    pub accounts: Vec<Account>,
    pub groups: Vec<Group>,
    pub plays: Vec<Play>,
    /// Client families ([`client_family`]) plays refer to.
    pub clients: Vec<String>,
    /// The first day of the sync window.
    pub sync_from: Option<NaiveDate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub user_id: i64,
    /// An index into [`Detection::cohorts`].
    pub cohort: usize,
    pub suspicion: i64,
    pub factor: i64,
    pub evidence: Evidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgeCell {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub members: i64,
    pub purity: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValueCell {
    /// For origins, `address` or `network`; for clients, the family.
    pub value: String,
    /// Distinct values in the cell.
    pub values: i64,
    pub members: i64,
    /// The cell's purity as a whole.
    pub purity: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncCell {
    pub members: i64,
    pub median_days: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Target {
    pub kind: Kind,
    pub entity_id: i64,
    /// The cohort's members among its listeners, and all of them.
    pub members: i64,
    pub listeners: i64,
    /// The cohort's share of its weight, thousandths.
    pub weight_share: i64,
}

/// What fired in the group a cohort is named after.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CohortDetail {
    pub age: Vec<AgeCell>,
    pub origin: Option<ValueCell>,
    pub client: Option<ValueCell>,
    pub sync: Option<SyncCell>,
    /// New listeners' share of the group, thousandths.
    pub new_share: i64,
    pub targets: Vec<Target>,
}

/// Flagged accounts whose strongest primary evidence is in the same group,
/// [`HOME_STEP`] apart counting as equal and the larger group winning, so a
/// farm's accounts meet on one of its targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cohort {
    pub kind: Kind,
    pub entity_id: i64,
    pub members: i64,
    /// Means over the members.
    pub score: i64,
    pub factor: i64,
    pub evidence: Evidence,
    pub detail: CohortDetail,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detection {
    pub active: i64,
    /// Groups large enough to examine.
    pub groups: i64,
    /// Fewer active accounts than [`CoordinationParams::min_accounts`]:
    /// nothing was judged.
    pub too_small: bool,
    /// Accounts with a factor below 1, by user id.
    pub accounts: Vec<Assessment>,
    /// Largest first.
    pub cohorts: Vec<Cohort>,
}

pub fn detect(input: &Input, params: &CoordinationParams) -> Detection {
    let mut accounts: Vec<&Account> = input.accounts.iter().collect();
    accounts.sort_by_key(|a| a.user_id);
    accounts.dedup_by_key(|a| a.user_id);
    let mut detection = Detection {
        active: accounts.len() as i64,
        ..Default::default()
    };
    if detection.active < params.min_accounts {
        detection.too_small = true;
        return detection;
    }
    let index: HashMap<i64, usize> = accounts
        .iter()
        .enumerate()
        .map(|(i, a)| (a.user_id, i))
        .collect();
    let population = Population::new(&accounts, params);
    let plays = Plays::new(input, &index, params);

    let mut groups: Vec<&Group> = input.groups.iter().collect();
    groups.sort_by_key(|g| (g.kind, g.entity_id));
    let mut best: Vec<Option<Best>> = vec![None; accounts.len()];
    let mut home: Vec<Option<(i64, usize, usize)>> = vec![None; accounts.len()];
    let mut details: HashMap<usize, CohortDetail> = HashMap::new();
    let mut examined: Vec<(usize, Vec<(usize, i64)>)> = Vec::new();
    for (g, group) in groups.iter().enumerate() {
        let mut members: Vec<(usize, &Member)> = group
            .members
            .iter()
            .filter_map(|m| index.get(&m.user_id).map(|i| (*i, m)))
            .collect();
        members.sort_by_key(|(i, _)| *i);
        members.dedup_by_key(|(i, _)| *i);
        if (members.len() as i64) < params.min_cohort {
            continue;
        }
        detection.groups += 1;
        let (evidence, detail) = evaluate(group, &members, &accounts, &population, &plays, params);
        for ((i, _), e) in members.iter().zip(&evidence) {
            let raw = params.weighted(e);
            let candidate = Best {
                suspicion: raw.min(FULL),
                raw,
                size: members.len(),
                group: g,
                evidence: *e,
            };
            if best[*i].as_ref().is_none_or(|b| candidate.beats(b)) {
                best[*i] = Some(candidate);
            }
            let primary = params.weighted(&e.primary());
            let step = primary / HOME_STEP;
            if primary > 0 && home[*i].is_none_or(|(s, n, _)| (step, members.len()) > (s, n)) {
                home[*i] = Some((step, members.len(), g));
            }
        }
        details.insert(g, detail);
        examined.push((g, members.iter().map(|(i, m)| (*i, m.weight)).collect()));
    }

    // Cohorts: flagged accounts by the group of their strongest primary
    // evidence.
    let mut by_group: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, b) in best.iter().enumerate() {
        if let Some(b) = b
            && params.factor(b.suspicion) < FULL
        {
            let group = home[i].map_or(b.group, |(_, _, g)| g);
            by_group.entry(group).or_default().push(i);
        }
    }
    let components: Vec<(usize, Vec<usize>)> = by_group.into_iter().collect();
    let cohort_of: HashMap<usize, usize> = components
        .iter()
        .enumerate()
        .flat_map(|(c, (_, members))| members.iter().map(move |i| (*i, c)))
        .collect();

    // Each examined group's listeners and weight per cohort among them.
    let mut targets: Vec<Vec<Target>> = vec![Vec::new(); components.len()];
    for (g, members) in &examined {
        let total: i64 = members.iter().map(|(_, w)| w).sum();
        let mut per_cohort: BTreeMap<usize, (i64, i64)> = BTreeMap::new();
        for (i, w) in members {
            if let Some(c) = cohort_of.get(i) {
                let e = per_cohort.entry(*c).or_default();
                e.0 += 1;
                e.1 += w;
            }
        }
        for (c, (n, w)) in per_cohort {
            targets[c].push(Target {
                kind: groups[*g].kind,
                entity_id: groups[*g].entity_id,
                members: n,
                listeners: members.len() as i64,
                weight_share: if total > 0 { w * FULL / total } else { 0 },
            });
        }
    }

    let mut cohorts: Vec<(Cohort, Vec<usize>)> = Vec::new();
    for ((main, members), mut tops) in components.into_iter().zip(targets) {
        tops.sort_by(|a, b| {
            (b.members, b.weight_share)
                .cmp(&(a.members, a.weight_share))
                .then((a.kind, a.entity_id).cmp(&(b.kind, b.entity_id)))
        });
        tops.truncate(5);
        let n = members.len() as i64;
        let mut evidence = Evidence::default();
        let (mut score, mut factor) = (0, 0);
        for i in &members {
            let b = best[*i].as_ref().expect("flagged");
            score += b.suspicion;
            factor += params.factor(b.suspicion);
            for signal in Signal::ALL {
                evidence.0[signal as usize] += b.evidence.get(signal);
            }
        }
        for v in evidence.0.iter_mut() {
            *v /= n;
        }
        let mut detail = details.remove(&main).unwrap_or_default();
        detail.targets = tops;
        cohorts.push((
            Cohort {
                kind: groups[main].kind,
                entity_id: groups[main].entity_id,
                members: n,
                score: score / n,
                factor: factor / n,
                evidence,
                detail,
            },
            members,
        ));
    }
    cohorts.sort_by(|(a, _), (b, _)| {
        (b.members, b.score)
            .cmp(&(a.members, a.score))
            .then((a.kind, a.entity_id).cmp(&(b.kind, b.entity_id)))
    });
    for (c, (cohort, members)) in cohorts.into_iter().enumerate() {
        for i in members {
            let b = best[i].as_ref().expect("flagged");
            detection.accounts.push(Assessment {
                user_id: accounts[i].user_id,
                cohort: c,
                suspicion: b.suspicion,
                factor: params.factor(b.suspicion),
                evidence: b.evidence,
            });
        }
        detection.cohorts.push(cohort);
    }
    detection.accounts.sort_by_key(|a| a.user_id);
    detection
}

/// An account's highest suspicion so far, and where.
#[derive(Debug, Clone)]
struct Best {
    suspicion: i64,
    /// The evidence uncapped, then the group's size, break ties at the cap.
    raw: i64,
    size: usize,
    group: usize,
    evidence: Evidence,
}

impl Best {
    fn beats(&self, other: &Best) -> bool {
        (self.suspicion, self.raw, self.size) > (other.suspicion, other.raw, other.size)
    }
}

/// What every group is compared with.
struct Population {
    total: i64,
    /// Registrations up to and including each day, by day.
    registered: Vec<(i32, i64)>,
    with_origin: i64,
    addresses: HashMap<(i32, i64), i64>,
    networks: HashMap<(i32, i64), i64>,
    shape: Vec<i64>,
}

impl Population {
    fn new(accounts: &[&Account], params: &CoordinationParams) -> Self {
        let mut days: BTreeMap<i32, i64> = BTreeMap::new();
        let mut addresses = HashMap::new();
        let mut networks = HashMap::new();
        let mut with_origin = 0;
        for a in accounts {
            *days.entry(a.registered.num_days_from_ce()).or_default() += 1;
            if let Some(o) = a.origin {
                with_origin += 1;
                *addresses.entry((o.key_id, o.address)).or_default() += 1;
                *networks.entry((o.key_id, o.network)).or_default() += 1;
            }
        }
        let mut cumulative = 0;
        let registered = days
            .into_iter()
            .map(|(d, n)| {
                cumulative += n;
                (d, cumulative)
            })
            .collect();
        let shape = accounts
            .iter()
            .map(|a| {
                if a.weight <= 0 {
                    return 0;
                }
                let share = a.top3_weight.clamp(0, a.weight) * FULL / a.weight;
                ((share - params.shape_from) * FULL / (params.shape_to - params.shape_from))
                    .clamp(0, FULL)
            })
            .collect();
        Self {
            total: accounts.len() as i64,
            registered,
            with_origin,
            addresses,
            networks,
            shape,
        }
    }

    /// Accounts registered on the days `[from, to]`.
    fn registered_between(&self, from: i32, to: i32) -> i64 {
        let upto = |day: i32| {
            let i = self.registered.partition_point(|(d, _)| *d <= day);
            if i == 0 { 0 } else { self.registered[i - 1].1 }
        };
        upto(to) - upto(from - 1)
    }
}

/// The sync window's plays, with what each group needs from them.
struct Plays {
    /// Per play: the account's index, its day in the window, whether it was
    /// in a crowd, its client.
    rows: Vec<(usize, u8, bool, Option<u32>)>,
    by_artist: HashMap<i64, Vec<u32>>,
    by_track: HashMap<i64, Vec<u32>>,
    /// Accounts read, and how many used each client.
    readers: i64,
    client_users: HashMap<u32, i64>,
    clients: Vec<String>,
}

impl Plays {
    fn new(input: &Input, index: &HashMap<i64, usize>, params: &CoordinationParams) -> Self {
        let first_day = input
            .sync_from
            .map(|d| i64::from(d.num_days_from_ce() - EPOCH_CE_DAYS));
        let mut plays: Vec<(&Play, usize, u8)> = input
            .plays
            .iter()
            .filter_map(|p| {
                let i = *index.get(&p.user_id)?;
                let day = p.at.div_euclid(DAY_SECS) - first_day?;
                (0..params.sync_window_days)
                    .contains(&day)
                    .then_some((p, i, day as u8))
            })
            .collect();
        plays.sort_by_key(|(p, i, _)| (p.track_id, p.at, *i));

        let mut crowded = vec![false; plays.len()];
        let mut start = 0;
        while start < plays.len() {
            let track = plays[start].0.track_id;
            let mut end = start;
            while end < plays.len() && plays[end].0.track_id == track {
                end += 1;
            }
            mark_crowds(&plays[start..end], &mut crowded[start..end], params);
            start = end;
        }

        let mut rows = Vec::with_capacity(plays.len());
        let mut by_artist: HashMap<i64, Vec<u32>> = HashMap::new();
        let mut by_track: HashMap<i64, Vec<u32>> = HashMap::new();
        let mut readers = HashSet::new();
        let mut client_users: HashMap<u32, HashSet<usize>> = HashMap::new();
        for (k, (p, i, day)) in plays.iter().enumerate() {
            rows.push((*i, *day, crowded[k], p.client));
            by_artist.entry(p.artist_id).or_default().push(k as u32);
            by_track.entry(p.track_id).or_default().push(k as u32);
            readers.insert(*i);
            if let Some(c) = p.client {
                client_users.entry(c).or_default().insert(*i);
            }
        }
        Self {
            rows,
            by_artist,
            by_track,
            readers: readers.len() as i64,
            client_users: client_users
                .into_iter()
                .map(|(c, users)| (c, users.len() as i64))
                .collect(),
            clients: input.clients.clone(),
        }
    }

    fn of(&self, group: &Group) -> &[u32] {
        let by = match group.kind {
            Kind::Artist => &self.by_artist,
            Kind::Track => &self.by_track,
        };
        by.get(&group.entity_id).map_or(&[], Vec::as_slice)
    }
}

/// Days from 0001-01-01 to 1970-01-01.
const EPOCH_CE_DAYS: i32 = 719_163;

/// Marks the plays of one track (sorted by time) made with at
/// least [`MIN_CROWD`] other accounts within `sync_secs`, and `sync_ratio`
/// times what the other accounts playing it within `sync_context_secs`,
/// spread evenly, would put there.
fn mark_crowds(plays: &[(&Play, usize, u8)], crowded: &mut [bool], params: &CoordinationParams) {
    if plays.len() <= MIN_CROWD as usize {
        return;
    }
    let mut near = Window::default();
    let mut context = Window::default();
    for k in 0..plays.len() {
        let at = plays[k].0.at;
        near.slide(plays, at, params.sync_secs);
        context.slide(plays, at, params.sync_context_secs);
        let others = near.users.len() as i64 - 1;
        let around = context.users.len() as i64 - 1;
        crowded[k] = others >= MIN_CROWD
            && others * params.sync_context_secs >= params.sync_ratio * around * params.sync_secs;
    }
}

/// The distinct accounts among the plays within `radius` of a time that
/// only moves forward.
#[derive(Default)]
struct Window {
    users: HashMap<usize, i64>,
    left: usize,
    right: usize,
}

impl Window {
    fn slide(&mut self, plays: &[(&Play, usize, u8)], at: i64, radius: i64) {
        while self.right < plays.len() && plays[self.right].0.at <= at + radius {
            *self.users.entry(plays[self.right].1).or_default() += 1;
            self.right += 1;
        }
        while plays[self.left].0.at < at - radius {
            let user = plays[self.left].1;
            let n = self.users.get_mut(&user).expect("in the window");
            *n -= 1;
            if *n == 0 {
                self.users.remove(&user);
            }
            self.left += 1;
        }
    }
}

/// Evidence for each member (in `members`' order) and what fired.
fn evaluate(
    group: &Group,
    members: &[(usize, &Member)],
    accounts: &[&Account],
    population: &Population,
    plays: &Plays,
    params: &CoordinationParams,
) -> (Vec<Evidence>, CohortDetail) {
    let n = members.len() as i64;
    let mut evidence = vec![Evidence::default(); members.len()];
    let mut detail = CohortDetail::default();
    let position: HashMap<usize, usize> = members
        .iter()
        .enumerate()
        .map(|(k, (i, _))| (*i, k))
        .collect();

    // Age: the registration windows the group is over-represented in.
    let days: Vec<(i32, usize)> = members
        .iter()
        .enumerate()
        .map(|(k, (i, _))| (accounts[*i].registered.num_days_from_ce(), k))
        .collect();
    for cell in age_cells(days, population, n, params) {
        for k in &cell.members {
            evidence[*k].set(Signal::Age, cell.purity);
        }
        detail.age.push(AgeCell {
            from: NaiveDate::from_num_days_from_ce_opt(cell.from).unwrap_or_default(),
            to: NaiveDate::from_num_days_from_ce_opt(cell.to).unwrap_or_default(),
            members: cell.members.len() as i64,
            purity: cell.purity,
        });
    }

    // Origin: the address or the network, whichever explains more.
    let origins: Vec<(usize, crate::origin::Origin)> = members
        .iter()
        .enumerate()
        .filter_map(|(k, (i, _))| accounts[*i].origin.map(|o| (k, o)))
        .collect();
    let with_origin = origins.len() as i64;
    let address = value_cells(
        origins.iter().map(|(k, o)| (*k, (o.key_id, o.address))),
        &population.addresses,
        with_origin,
        population.with_origin,
        params,
    );
    let network = value_cells(
        origins.iter().map(|(k, o)| (*k, (o.key_id, o.network))),
        &population.networks,
        with_origin,
        population.with_origin,
        params,
    );
    let origin = match (address, network) {
        (Some(a), Some(n)) if n.excess > a.excess => Some(("network", n)),
        (Some(a), _) => Some(("address", a)),
        (None, Some(n)) => Some(("network", n)),
        (None, None) => None,
    };
    if let Some((what, cells)) = origin {
        for (k, purity) in &cells.members {
            evidence[*k].set(Signal::Origin, *purity);
        }
        detail.origin = Some(ValueCell {
            value: what.into(),
            values: cells.values,
            members: cells.members.len() as i64,
            purity: cells.purity,
        });
    }

    // Client and sync, from the group's plays in the sync window.
    let mut listened: BTreeMap<usize, (u64, BTreeMap<u32, i64>)> = BTreeMap::new();
    for p in plays.of(group) {
        let (i, day, crowded, client) = plays.rows[*p as usize];
        let Some(k) = position.get(&i) else { continue };
        let entry = listened.entry(*k).or_default();
        if crowded {
            entry.0 |= 1 << day;
        }
        if let Some(c) = client {
            *entry.1.entry(c).or_default() += 1;
        }
    }
    let dominant: Vec<(usize, u32)> = listened
        .iter()
        .filter_map(|(k, (_, clients))| {
            clients
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
                .map(|(c, _)| (*k, *c))
        })
        .collect();
    // A client the group's members use and hardly any other account does.
    let on_clients = dominant.len() as i64;
    let mut holders: BTreeMap<u32, i64> = BTreeMap::new();
    for (_, c) in &dominant {
        *holders.entry(*c).or_default() += 1;
    }
    let outside_readers = (plays.readers - on_clients).max(1);
    let rare: HashMap<u32, i64> = holders
        .iter()
        .filter_map(|(c, held)| {
            let users = plays.client_users.get(c).copied().unwrap_or(0);
            ((users - held).max(0) * FULL <= params.rare_client * outside_readers)
                .then_some((*c, users))
        })
        .collect();
    let client = value_cells(
        dominant
            .iter()
            .filter(|(_, c)| rare.contains_key(c))
            .map(|(k, c)| (*k, *c)),
        &rare,
        on_clients,
        plays.readers,
        params,
    );
    if let Some(cells) = client {
        for (k, purity) in &cells.members {
            evidence[*k].set(Signal::Client, *purity);
        }
        detail.client = cells.largest.map(|c| ValueCell {
            value: plays
                .clients
                .get(c as usize)
                .cloned()
                .unwrap_or_else(|| format!("#{c}")),
            values: cells.values,
            members: cells.members.len() as i64,
            purity: cells.purity,
        });
    }

    let lockstep: Vec<(usize, i64)> = listened
        .iter()
        .map(|(k, (days, _))| (*k, i64::from(days.count_ones())))
        .filter(|(_, days)| *days >= params.sync_days)
        .collect();
    if lockstep.len() as i64 >= params.min_cohort {
        let mut days: Vec<i64> = lockstep.iter().map(|(_, d)| *d).collect();
        days.sort();
        for (k, d) in &lockstep {
            evidence[*k].set(Signal::Sync, (d * FULL / (2 * params.sync_days)).min(FULL));
        }
        detail.sync = Some(SyncCell {
            members: lockstep.len() as i64,
            median_days: days[days.len() / 2],
        });
    }

    // Behaviour: concentrated listening, and new listeners of the entity.
    let new = members.iter().filter(|(_, m)| m.new).count() as i64;
    detail.new_share = new * FULL / n;
    for (k, (i, m)) in members.iter().enumerate() {
        evidence[k].set(Signal::Shape, population.shape[*i]);
        if m.new {
            evidence[k].set(Signal::Burst, detail.new_share);
        }
    }
    (evidence, detail)
}

/// An excess against the expectation `expected / scale` passes when it is
/// at least `min_cohort` accounts and `min_z` standard deviations (Poisson).
/// All terms are scaled by `scale`.
fn significant(observed: i64, expected: i64, scale: i64, params: &CoordinationParams) -> bool {
    let excess = i128::from(observed) * i128::from(scale) - i128::from(expected);
    let z = i128::from(params.min_z);
    excess >= i128::from(params.min_cohort) * i128::from(scale)
        && excess * excess >= z * z * i128::from(expected.max(scale)) * i128::from(scale)
}

struct AgeWindow {
    from: i32,
    to: i32,
    members: Vec<usize>,
    purity: i64,
}

/// Picks registration windows of `age_days`, the largest excess first,
/// while the excess is significant; the windows never overlap. A farm
/// registered in batches, or steadily over months, is over-represented in
/// every week it registered in; each window passes on its own.
fn age_cells(
    mut days: Vec<(i32, usize)>,
    population: &Population,
    n: i64,
    params: &CoordinationParams,
) -> Vec<AgeWindow> {
    days.sort();
    let width = params.age_days as i32;
    let total = population.total;
    let mut used = vec![false; days.len()];
    let mut taken: Vec<(i32, i32)> = Vec::new();
    let mut cells = Vec::new();
    loop {
        let mut unused = vec![0i64; days.len() + 1];
        for k in 0..days.len() {
            unused[k + 1] = unused[k] + i64::from(!used[k]);
        }
        let mut best: Option<(i64, i32, i64)> = None;
        let mut right = 0;
        for left in 0..days.len() {
            if left > 0 && days[left].0 == days[left - 1].0 {
                continue;
            }
            let from = days[left].0;
            let to = from + width - 1;
            right = right.max(left);
            while right < days.len() && days[right].0 <= to {
                right += 1;
            }
            if taken.iter().any(|(a, b)| from <= *b && *a <= to) {
                continue;
            }
            let inside = unused[right] - unused[left];
            let expected = n * population.registered_between(from, to);
            let excess = inside * total - expected;
            if significant(inside, expected, total, params)
                && best.is_none_or(|(e, _, _)| excess > e)
            {
                best = Some((excess, from, inside));
            }
        }
        let Some((excess, from, inside)) = best else {
            break;
        };
        let to = from + width - 1;
        let mut members = Vec::new();
        for (k, (d, m)) in days.iter().enumerate() {
            if !used[k] && (from..=to).contains(d) {
                used[k] = true;
                members.push(*m);
            }
        }
        taken.push((from, to));
        cells.push(AgeWindow {
            from,
            to,
            members,
            purity: excess * FULL / (inside * total),
        });
    }
    cells
}

struct ValueCells<K> {
    /// Members in the cell, with the purity of their value.
    members: Vec<(usize, i64)>,
    values: i64,
    purity: i64,
    /// Total excess, scaled.
    excess: i64,
    largest: Option<K>,
}

/// Members whose value (an origin, a client) is held by at least
/// [`MIN_VALUE_MEMBERS`] of the group and more than both one member and
/// what the value's share of the population (`counts` of `total`)
/// predicts for `n` members. `None` unless the cell's excess is
/// significant.
fn value_cells<K: Hash + Eq + Ord + Copy>(
    values: impl Iterator<Item = (usize, K)>,
    counts: &HashMap<K, i64>,
    n: i64,
    total: i64,
    params: &CoordinationParams,
) -> Option<ValueCells<K>> {
    if total == 0 {
        return None;
    }
    let mut by_value: BTreeMap<K, Vec<usize>> = BTreeMap::new();
    for (k, v) in values {
        by_value.entry(v).or_default().push(k);
    }
    let (mut observed, mut expected) = (0, 0);
    let mut cells: Vec<(K, Vec<usize>, i64)> = Vec::new();
    for (value, holders) in by_value {
        let obs = holders.len() as i64;
        let exp = (counts.get(&value).copied().unwrap_or(0) * n).max(total);
        if obs < MIN_VALUE_MEMBERS || obs * total <= exp {
            continue;
        }
        observed += obs;
        expected += exp;
        let purity = (obs * total - exp) * FULL / (obs * total);
        cells.push((value, holders, purity));
    }
    if !significant(observed, expected, total, params) {
        return None;
    }
    let largest = cells
        .iter()
        .max_by(|a, b| a.1.len().cmp(&b.1.len()).then(b.0.cmp(&a.0)))
        .map(|c| c.0);
    Some(ValueCells {
        values: cells.len() as i64,
        purity: (observed * total - expected) * FULL / (observed * total),
        excess: observed * total - expected,
        members: cells
            .into_iter()
            .flat_map(|(_, holders, purity)| holders.into_iter().map(move |k| (k, purity)))
            .collect(),
        largest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::Origin;

    /// A deterministic generator, so test populations are the same on
    /// every run.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> i64 {
            (self.next() % n) as i64
        }

        /// Zipf-like rank in `1..=n`: rank = floor(n ^ u).
        fn zipf(&mut self, n: u64) -> i64 {
            let u = (self.next() % 1_000_000) as f64 / 1_000_000.0;
            ((n as f64).powf(u).floor() as i64).clamp(1, n as i64)
        }
    }

    fn day(n: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 10).unwrap() - chrono::TimeDelta::days(n)
    }

    const TODAY: i64 = 1_791_590_400; // 2026-10-10T00:00:00Z

    /// Builds groups and plays from what each account listened to.
    struct World {
        rng: Rng,
        accounts: Vec<Account>,
        /// (user, artist, track, at, client)
        plays: Vec<(i64, i64, i64, i64, u32)>,
        clients: Vec<String>,
        new_to: HashSet<(i64, i64)>,
    }

    impl World {
        fn new(seed: u64) -> Self {
            Self {
                rng: Rng(seed),
                accounts: Vec::new(),
                plays: Vec::new(),
                clients: vec!["scrobblr:ytmusic".into(), "scrobblr:android".into()],
                new_to: HashSet::new(),
            }
        }

        fn account(&mut self, user_id: i64, registered: NaiveDate, origin: Option<Origin>) {
            self.accounts.push(Account {
                user_id,
                registered,
                origin,
                weight: 0,
                top3_weight: 0,
            });
        }

        /// An honest population: registered over two years, each from its
        /// own address (households of 3 every 50 accounts), 15 favourite
        /// artists of 400, listening at its own hours on half the days.
        fn honest(&mut self, from: i64, count: i64) {
            for user in from..from + count {
                let registered = day(30 + self.rng.below(700));
                let address = if user % 50 < 3 {
                    user / 50
                } else {
                    user + 1_000_000
                };
                let origin = Origin {
                    key_id: 1,
                    address,
                    network: self.rng.below(3000),
                };
                self.account(user, registered, Some(origin));
                let favourites: Vec<i64> = (0..15).map(|_| self.rng.zipf(400)).collect();
                let client = (user % 2) as u32;
                for d in 0..14 {
                    if self.rng.below(2) == 0 {
                        continue;
                    }
                    let mut at = TODAY - (14 - d) * DAY_SECS + (8 + self.rng.below(12)) * 3600;
                    for _ in 0..(5 + self.rng.below(15)) {
                        let artist = favourites[self.rng.below(15) as usize];
                        let track = artist * 10 + self.rng.below(8);
                        self.plays.push((user, artist, track, at, client));
                        at += 200 + self.rng.below(60);
                    }
                }
            }
        }

        /// Farm accounts playing `targets` (each 8 tracks) and popular
        /// filler, at their own hours (`calqued`: all at 20:00).
        #[allow(clippy::too_many_arguments)]
        fn farm(
            &mut self,
            from: i64,
            count: i64,
            registered: impl Fn(i64) -> NaiveDate,
            origin: impl Fn(i64) -> Option<Origin>,
            targets: &[i64],
            client: u32,
            calqued: bool,
        ) {
            for user in from..from + count {
                self.account(user, registered(user), origin(user));
                for t in targets {
                    self.new_to.insert((user, *t));
                }
                for d in 0..14 {
                    let start = if calqued {
                        20 * 3600 + self.rng.below(20)
                    } else {
                        (8 + self.rng.below(14)) * 3600 + self.rng.below(3600)
                    };
                    let mut at = TODAY - (14 - d) * DAY_SECS + start;
                    for seq in 0..20 {
                        let (artist, track) = if seq % 4 == 3 {
                            let a = 1 + self.rng.below(5);
                            (a, a * 10 + self.rng.below(8))
                        } else if calqued {
                            let a = targets[(seq as usize / 4) % targets.len()];
                            (a, a * 10 + (seq + d) % 8)
                        } else {
                            let a = targets[self.rng.below(targets.len() as u64) as usize];
                            (a, a * 10 + self.rng.below(8))
                        };
                        self.plays.push((user, artist, track, at, client));
                        at += 205;
                    }
                }
            }
        }

        fn input(&mut self) -> Input {
            let mut artists: BTreeMap<i64, BTreeMap<i64, i64>> = BTreeMap::new();
            let mut tracks: BTreeMap<i64, BTreeMap<i64, i64>> = BTreeMap::new();
            let mut per_user: HashMap<i64, HashMap<i64, i64>> = HashMap::new();
            for (user, artist, track, _, _) in &self.plays {
                *artists
                    .entry(*artist)
                    .or_default()
                    .entry(*user)
                    .or_default() += FULL;
                *tracks.entry(*track).or_default().entry(*user).or_default() += FULL;
                *per_user
                    .entry(*user)
                    .or_default()
                    .entry(*artist)
                    .or_default() += FULL;
            }
            for a in &mut self.accounts {
                let mut weights: Vec<i64> = per_user
                    .get(&a.user_id)
                    .map(|m| m.values().copied().collect())
                    .unwrap_or_default();
                weights.sort_by(|x, y| y.cmp(x));
                a.weight = weights.iter().sum();
                a.top3_weight = weights.iter().take(3).sum();
            }
            let group = |kind, id: i64, members: &BTreeMap<i64, i64>, new_to: &HashSet<_>| Group {
                kind,
                entity_id: id,
                members: members
                    .iter()
                    .map(|(u, w)| Member {
                        user_id: *u,
                        weight: *w,
                        new: new_to
                            .contains(&(*u, if kind == Kind::Artist { id } else { id / 10 })),
                    })
                    .collect(),
            };
            let mut groups: Vec<Group> = artists
                .iter()
                .map(|(id, m)| group(Kind::Artist, *id, m, &self.new_to))
                .collect();
            groups.extend(
                tracks
                    .iter()
                    .map(|(id, m)| group(Kind::Track, *id, m, &self.new_to)),
            );
            Input {
                accounts: self.accounts.clone(),
                groups,
                plays: self
                    .plays
                    .iter()
                    .map(|(user_id, artist_id, track_id, at, client)| Play {
                        user_id: *user_id,
                        track_id: *track_id,
                        artist_id: *artist_id,
                        at: *at,
                        client: Some(*client),
                    })
                    .collect(),
                clients: self.clients.clone(),
                sync_from: Some(day(14)),
            }
        }
    }

    fn params() -> CoordinationParams {
        CoordinationParams::default()
    }

    fn factor_of(d: &Detection, user: i64) -> i64 {
        d.accounts
            .iter()
            .find(|a| a.user_id == user)
            .map_or(FULL, |a| a.factor)
    }

    fn assessment(d: &Detection, user: i64) -> &Assessment {
        d.accounts.iter().find(|a| a.user_id == user).unwrap()
    }

    #[test]
    fn the_curve_dents_one_signal_and_zeroes_two() {
        let p = params();
        assert_eq!(p.factor(0), FULL);
        assert_eq!(p.factor(400), FULL);
        assert_eq!(p.factor(900), 0);
        assert_eq!(p.factor(1000), 0);
        let mut last = FULL;
        for s in (400..=900).step_by(10) {
            let f = p.factor(s);
            assert!(f <= last, "{s}");
            last = f;
        }
        // One primary signal at full strength alone keeps ~90 %.
        assert!(p.factor(500) >= 890);
        // Two at 0.8 purity keep ~10 %.
        assert!(p.factor(800) <= 110);
        assert_eq!(p.factor(650), 500);
    }

    #[test]
    fn an_honest_population_keeps_full_weight() {
        let mut w = World::new(7);
        w.honest(1, 600);
        let d = detect(&w.input(), &params());
        assert!(!d.too_small);
        assert!(d.groups > 100);
        assert!(d.accounts.is_empty(), "{:?}", d.accounts.first());
    }

    #[test]
    fn a_farm_registered_together_from_few_addresses_weighs_nothing() {
        let mut w = World::new(11);
        w.honest(1, 600);
        w.farm(
            10_001,
            40,
            |u| day(45 + u % 2),
            |u| {
                Some(Origin {
                    key_id: 1,
                    address: 77 + u % 2,
                    network: 77,
                })
            },
            &[9_001],
            0,
            false,
        );
        let d = detect(&w.input(), &params());
        for user in 10_001..10_041 {
            let a = assessment(&d, user);
            assert_eq!(a.factor, 0, "{a:?}");
            assert!(a.evidence.get(Signal::Age) > 900);
            assert!(a.evidence.get(Signal::Origin) > 900);
        }
        // An honest account registered the farm's week, in a group the
        // farm's filler joined, is dented as much as that cell is impure.
        assert!((1..=600).all(|u| factor_of(&d, u) >= 990));
        let cohort = &d.cohorts[0];
        assert_eq!(cohort.members, 40);
        assert_eq!(cohort.detail.targets[0].members, 40);
        assert!(cohort.detail.targets.iter().any(|t| t.entity_id == 9_001));
        assert_eq!(cohort.detail.origin.as_ref().unwrap().values, 2);
    }

    #[test]
    fn one_trait_alone_only_dents() {
        // Registered together, but from their own addresses, at their own
        // hours, through a common client: indistinguishable by anything
        // else from fans an artist brought over in one week.
        let mut w = World::new(13);
        w.honest(1, 600);
        w.farm(
            10_001,
            40,
            |_| day(45),
            |u| {
                Some(Origin {
                    key_id: 1,
                    address: 5_000_000 + u,
                    network: 5_000_000 + u,
                })
            },
            &[9_001, 9_002, 9_003],
            0,
            false,
        );
        let d = detect(&w.input(), &params());
        for user in 10_001..10_041 {
            let a = assessment(&d, user);
            assert_eq!(a.evidence.get(Signal::Origin), 0);
            assert_eq!(a.evidence.get(Signal::Sync), 0);
            assert!(a.evidence.get(Signal::Age) > 900);
            // Age, plus their listening's shape and newness.
            assert!(a.factor > 0 && a.factor < FULL, "{a:?}");
        }
        let without_behaviour = CoordinationParams {
            weights: Evidence([500, 500, 350, 500, 0, 0]),
            ..params()
        };
        let d = detect(&w.input(), &without_behaviour);
        assert!((10_001..10_041).all(|u| factor_of(&d, u) >= 890));
    }

    fn cohort_of(d: &Detection, user: i64) -> usize {
        assessment(d, user).cohort
    }

    #[test]
    fn a_farm_is_one_cohort_and_two_farms_sharing_filler_are_two() {
        let mut w = World::new(53);
        w.honest(1, 600);
        let origin = |base: i64| {
            move |u: i64| {
                Some(Origin {
                    key_id: 1,
                    address: base + u % 2,
                    network: base,
                })
            }
        };
        w.farm(
            10_001,
            40,
            |u| day(45 + u % 2),
            origin(77),
            &[9_001, 9_002, 9_003],
            0,
            false,
        );
        // Registered the same week, playing the same popular filler.
        w.farm(
            20_001,
            30,
            |_| day(47),
            origin(88),
            &[9_101, 9_102],
            0,
            false,
        );
        let d = detect(&w.input(), &params());
        let a = cohort_of(&d, 10_001);
        let b = cohort_of(&d, 20_001);
        assert_ne!(a, b);
        assert!((10_001..10_041).all(|u| cohort_of(&d, u) == a));
        assert!((20_001..20_031).all(|u| cohort_of(&d, u) == b));
        assert_eq!(d.cohorts[a].kind, Kind::Artist);
        assert!([9_001, 9_002, 9_003].contains(&d.cohorts[a].entity_id));
        assert!([9_101, 9_102].contains(&d.cohorts[b].entity_id));
    }

    #[test]
    fn a_farm_registered_steadily_for_weeks_is_over_represented_in_each() {
        let mut w = World::new(59);
        w.honest(1, 600);
        w.farm(
            10_001,
            200,
            |u| day(40 + u % 56),
            |_| None,
            &[9_001],
            0,
            false,
        );
        let d = detect(&w.input(), &params());
        let aged = (10_001..10_201)
            .filter(|u| {
                d.accounts
                    .iter()
                    .any(|a| a.user_id == *u && a.evidence.get(Signal::Age) > 0)
            })
            .count();
        assert!(aged >= 180, "{aged}");
        assert!(d.cohorts[0].detail.age.len() >= 7);
    }

    #[test]
    fn calqued_schedules_are_lockstep() {
        let mut w = World::new(17);
        w.honest(1, 600);
        w.farm(
            10_001,
            30,
            |u| day(30 + u * 7 % 700),
            |u| {
                Some(Origin {
                    key_id: 1,
                    address: 5_000_000 + u,
                    network: 5_000_000 + u,
                })
            },
            &[9_001],
            0,
            true,
        );
        let d = detect(&w.input(), &params());
        let a = assessment(&d, 10_001);
        assert_eq!(a.evidence.get(Signal::Sync), FULL);
        assert!(d.cohorts[0].detail.sync.as_ref().unwrap().median_days >= 13);
        assert!((1..=600).all(|u| factor_of(&d, u) == FULL));
    }

    #[test]
    fn a_crowd_on_one_day_is_not_lockstep() {
        // A release: 200 listeners play the album in the same minutes, once.
        let mut w = World::new(19);
        w.honest(1, 600);
        for user in 1..=200 {
            w.new_to.insert((user, 9_500));
            for seq in 0..10 {
                let at = TODAY - 3 * DAY_SECS + seq * 210 + w.rng.below(30);
                w.plays
                    .push((user, 9_500, 95_000 + seq, at, (user % 2) as u32));
            }
        }
        let d = detect(&w.input(), &params());
        assert!(d.accounts.is_empty(), "{:?}", d.accounts.first());
    }

    #[test]
    fn a_household_listening_together_is_no_crowd() {
        let mut w = World::new(23);
        w.honest(1, 300);
        let plays: Vec<Play> = (0..3)
            .flat_map(|u| {
                (0..20).map(move |seq| Play {
                    user_id: 1 + u,
                    track_id: 5,
                    artist_id: 1,
                    at: TODAY - DAY_SECS + seq * 200 + u,
                    client: None,
                })
            })
            .collect();
        let accounts: Vec<&Account> = w.accounts.iter().collect();
        let index: HashMap<i64, usize> = accounts
            .iter()
            .enumerate()
            .map(|(i, a)| (a.user_id, i))
            .collect();
        let input = Input {
            plays,
            sync_from: Some(day(14)),
            ..Default::default()
        };
        let p = Plays::new(&input, &index, &params());
        assert!(p.rows.iter().all(|(_, _, crowded, _)| !crowded));
    }

    #[test]
    fn a_campus_behind_one_address_is_no_evidence() {
        // 80 students share a NAT address and a semester's registration
        // week, and listen like everyone else.
        let mut w = World::new(29);
        w.honest(1, 600);
        for a in w.accounts.iter_mut().take(80) {
            a.origin = Some(Origin {
                key_id: 1,
                address: 424_242,
                network: 4_242,
            });
            a.registered = day(200 + a.user_id % 10);
        }
        let d = detect(&w.input(), &params());
        assert!(d.accounts.is_empty(), "{:?}", d.accounts.first());
    }

    #[test]
    fn a_genuine_fandom_is_left_alone() {
        // 40 established fans of a small artist, registered whenever, from
        // their own addresses, focused on it.
        let mut w = World::new(31);
        w.honest(1, 600);
        for user in (1..=600).step_by(15) {
            for d in 0..14 {
                let mut at = TODAY - (14 - d) * DAY_SECS + (20 + w.rng.below(3)) * 3600;
                for _ in 0..12 {
                    w.plays.push((user, 9_700, 97_000 + w.rng.below(8), at, 0));
                    at += 220;
                }
            }
        }
        let d = detect(&w.input(), &params());
        assert!(d.accounts.is_empty(), "{:?}", d.accounts.first());
    }

    #[test]
    fn a_rare_client_concentrated_on_a_target_is_evidence() {
        let mut w = World::new(37);
        w.honest(1, 600);
        w.clients.push("lastfm:0123abcd".into());
        w.farm(
            10_001,
            30,
            |u| day(30 + u * 7 % 700),
            |u| {
                Some(Origin {
                    key_id: 1,
                    address: 5_000_000 + u,
                    network: 5_000_000 + u,
                })
            },
            &[9_001],
            2,
            false,
        );
        let d = detect(&w.input(), &params());
        let a = assessment(&d, 10_001);
        assert!(a.evidence.get(Signal::Client) > 900, "{a:?}");
        assert!(a.factor > 0, "one trait only dents");
    }

    #[test]
    fn small_populations_are_not_judged() {
        let mut w = World::new(41);
        w.honest(1, 50);
        w.farm(10_001, 40, |_| day(45), |_| None, &[9_001], 0, true);
        let d = detect(&w.input(), &params());
        assert!(d.too_small);
        assert!(d.accounts.is_empty());
    }

    #[test]
    fn detection_is_deterministic_and_order_independent() {
        let mut w = World::new(43);
        w.honest(1, 400);
        w.farm(
            10_001,
            25,
            |u| day(45 + u % 3),
            |u| {
                Some(Origin {
                    key_id: 1,
                    address: 9 + u % 3,
                    network: 9,
                })
            },
            &[9_001, 9_002],
            0,
            false,
        );
        let input = w.input();
        let first = detect(&input, &params());
        let mut shuffled = input.clone();
        shuffled.accounts.reverse();
        shuffled.groups.reverse();
        shuffled.plays.reverse();
        for g in &mut shuffled.groups {
            g.members.reverse();
        }
        assert_eq!(detect(&shuffled, &params()), first);
        assert!(!first.accounts.is_empty());
    }

    #[test]
    fn origins_under_another_key_never_match() {
        let mut w = World::new(47);
        w.honest(1, 600);
        w.farm(
            10_001,
            40,
            |u| day(30 + u * 7 % 700),
            |u| {
                Some(Origin {
                    key_id: (u % 2) as i32 + 2,
                    address: 77,
                    network: 77,
                })
            },
            &[9_001],
            0,
            false,
        );
        let d = detect(&w.input(), &params());
        // Two keys, 20 accounts each: still a shared origin under each.
        assert!(assessment(&d, 10_001).evidence.get(Signal::Origin) > 900);
        let mut w = World::new(47);
        w.honest(1, 600);
        w.farm(
            10_001,
            40,
            |u| day(30 + u * 7 % 700),
            |u| {
                Some(Origin {
                    key_id: u as i32,
                    address: 77,
                    network: 77,
                })
            },
            &[9_001],
            0,
            false,
        );
        let d = detect(&w.input(), &params());
        assert!(
            d.accounts
                .iter()
                .all(|a| a.evidence.get(Signal::Origin) == 0)
        );
    }

    #[test]
    fn client_families_ignore_versions() {
        assert_eq!(
            client_family("listenbrainz", "Pano Scrobbler 3.12"),
            "listenbrainz:pano scrobbler"
        );
        assert_eq!(
            client_family("listenbrainz", "Web Scrobbler v3.14.0"),
            "listenbrainz:web scrobbler"
        );
        assert_eq!(client_family("lastfm", "0123abcd"), "lastfm:0123abcd");
        assert_eq!(client_family("scrobblr", " ytmusic "), "scrobblr:ytmusic");
    }

    #[test]
    fn params_are_validated_and_fingerprinted() {
        let defaults = CoordinationSettings::default();
        let params = CoordinationParams::new(&defaults).unwrap();
        assert_eq!(params, CoordinationParams::default());
        for bad in [
            CoordinationSettings {
                to: 0.3,
                ..defaults.clone()
            },
            CoordinationSettings {
                sync_window_days: 70,
                window_days: 90,
                ..defaults.clone()
            },
            CoordinationSettings {
                sync_days: 20,
                ..defaults.clone()
            },
            CoordinationSettings {
                weights: [0.5, 0.5, 2.0, 0.5, 0.1, 0.1],
                ..defaults.clone()
            },
        ] {
            assert!(CoordinationParams::new(&bad).is_err(), "{bad:?}");
        }
        let other = CoordinationParams::new(&CoordinationSettings {
            min_cohort: 20,
            ..defaults
        })
        .unwrap();
        assert_ne!(params.fingerprint(), other.fingerprint());
        assert!(
            params
                .fingerprint()
                .starts_with("coordination/v1 mode=apply")
        );
        assert_eq!(Mode::parse(" Report "), Some(Mode::Report));
    }
}
