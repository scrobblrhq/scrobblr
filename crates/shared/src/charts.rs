//! Chart periods and date ranges, for personal charts (any span of a
//! user's history) and community charts (the days whose ranking weights
//! are kept).

use chrono::{NaiveDate, TimeDelta};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ts_rs::TS;

/// A rolling window of UTC days ending today, today included, or all of
/// history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub enum ChartPeriod {
    #[serde(rename = "7d")]
    Days7,
    #[serde(rename = "30d")]
    Days30,
    #[serde(rename = "90d")]
    Days90,
    /// 365 days.
    #[serde(rename = "12m")]
    Months12,
    #[serde(rename = "all")]
    All,
}

impl ChartPeriod {
    pub const ALL: [ChartPeriod; 5] = [
        ChartPeriod::Days7,
        ChartPeriod::Days30,
        ChartPeriod::Days90,
        ChartPeriod::Months12,
        ChartPeriod::All,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ChartPeriod::Days7 => "7d",
            ChartPeriod::Days30 => "30d",
            ChartPeriod::Days90 => "90d",
            ChartPeriod::Months12 => "12m",
            ChartPeriod::All => "all",
        }
    }

    pub fn parse(s: &str) -> Option<ChartPeriod> {
        ChartPeriod::ALL.into_iter().find(|p| p.as_str() == s)
    }

    /// `None` for all of history.
    pub fn days(self) -> Option<i64> {
        match self {
            ChartPeriod::Days7 => Some(7),
            ChartPeriod::Days30 => Some(30),
            ChartPeriod::Days90 => Some(90),
            ChartPeriod::Months12 => Some(365),
            ChartPeriod::All => None,
        }
    }
}

/// The longest custom range: a calendar year, leap or not. Longer views
/// are what `all` is for.
pub const MAX_CUSTOM_DAYS: i64 = 366;

/// The UTC days a chart covers, both included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChartRange {
    /// `None` for a custom range.
    pub period: Option<ChartPeriod>,
    /// `None` for all of history.
    pub from: Option<NaiveDate>,
    pub to: NaiveDate,
}

impl ChartRange {
    /// Days covered; `None` for all of history.
    pub fn days(&self) -> Option<i64> {
        self.from.map(|from| (self.to - from).num_days() + 1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RangeError {
    #[error("unknown period `{0}`: 7d, 30d, 90d, 12m or all")]
    UnknownPeriod(String),
    #[error("`{0}` isn't a date: YYYY-MM-DD")]
    BadDate(String),
    #[error("give a period or a range (from and to), not both")]
    PeriodAndRange,
    #[error("a range needs both from and to")]
    HalfRange,
    #[error("from is after to")]
    Reversed,
    #[error("to is after today (UTC)")]
    Future,
    #[error("a range covers at most {MAX_CUSTOM_DAYS} days")]
    TooLong,
    #[error("this chart covers at most the days from {0} on")]
    TooOld(NaiveDate),
    #[error("this chart has no `all` period")]
    NoAllTime,
}

/// What a chart request asked for, before validation.
#[derive(Debug, Clone, Copy, Default)]
pub struct RangeQuery<'a> {
    pub period: Option<&'a str>,
    pub from: Option<&'a str>,
    pub to: Option<&'a str>,
}

/// What a kind of chart can cover.
#[derive(Debug, Clone, Copy)]
pub struct RangeRules {
    pub default: ChartPeriod,
    /// The first day with data to chart, when older days have none.
    pub earliest: Option<NaiveDate>,
}

/// The range asked for on `today`: a period (rules' default when neither a
/// period nor dates are given), or `from` and `to` dates.
pub fn resolve(
    q: RangeQuery<'_>,
    today: NaiveDate,
    rules: RangeRules,
) -> Result<ChartRange, RangeError> {
    let date = |s: &str| {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| RangeError::BadDate(s.to_owned()))
    };
    let range = match (q.period, q.from, q.to) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => return Err(RangeError::PeriodAndRange),
        (None, Some(from), Some(to)) => {
            let (from, to) = (date(from)?, date(to)?);
            if from > to {
                return Err(RangeError::Reversed);
            }
            if to > today {
                return Err(RangeError::Future);
            }
            if (to - from).num_days() + 1 > MAX_CUSTOM_DAYS {
                return Err(RangeError::TooLong);
            }
            ChartRange {
                period: None,
                from: Some(from),
                to,
            }
        }
        (None, Some(_), None) | (None, None, Some(_)) => return Err(RangeError::HalfRange),
        (period, None, None) => {
            let period = match period {
                None => rules.default,
                Some(p) => {
                    ChartPeriod::parse(p).ok_or_else(|| RangeError::UnknownPeriod(p.to_owned()))?
                }
            };
            ChartRange {
                period: Some(period),
                from: period.days().map(|days| today - TimeDelta::days(days - 1)),
                to: today,
            }
        }
    };
    if let Some(earliest) = rules.earliest {
        match range.from {
            None => return Err(RangeError::NoAllTime),
            Some(from) if from < earliest => return Err(RangeError::TooOld(earliest)),
            Some(_) => {}
        }
    }
    Ok(range)
}

/// Community charts read at most this many (account, day) pairs of
/// weights: the accounts followed, up to this over the days covered.
pub const COMMUNITY_USER_DAYS: i64 = 36_500;

/// How many accounts a community chart over `days` days covers.
pub fn community_accounts(days: i64, max_following: i64) -> i64 {
    (COMMUNITY_USER_DAYS / days.max(1)).min(max_following)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    const TODAY: &str = "2026-10-12";

    fn personal(q: RangeQuery<'_>) -> Result<ChartRange, RangeError> {
        resolve(
            q,
            day(TODAY),
            RangeRules {
                default: ChartPeriod::Days7,
                earliest: None,
            },
        )
    }

    #[test]
    fn periods_end_today_and_include_it() {
        let range = personal(RangeQuery::default()).unwrap();
        assert_eq!(range.period, Some(ChartPeriod::Days7));
        assert_eq!(
            (range.from, range.to),
            (Some(day("2026-10-06")), day(TODAY))
        );
        assert_eq!(range.days(), Some(7));
        for (name, days) in [("30d", 30), ("90d", 90), ("12m", 365)] {
            let range = personal(RangeQuery {
                period: Some(name),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(range.days(), Some(days), "{name}");
            assert_eq!(range.to, day(TODAY));
        }
        let all = personal(RangeQuery {
            period: Some("all"),
            ..Default::default()
        })
        .unwrap();
        assert_eq!((all.from, all.days()), (None, None));
        for p in ChartPeriod::ALL {
            assert_eq!(ChartPeriod::parse(p.as_str()), Some(p));
            assert_eq!(serde_json::to_value(p).unwrap(), p.as_str());
        }
    }

    #[test]
    fn ranges_are_validated() {
        let range = |from, to| {
            personal(RangeQuery {
                period: None,
                from,
                to,
            })
        };
        let ok = range(Some("2025-10-12"), Some("2026-10-12")).unwrap();
        assert_eq!((ok.period, ok.days()), (None, Some(366)));
        assert_eq!(
            range(Some("2026-10-12"), Some("2026-10-12"))
                .unwrap()
                .days(),
            Some(1)
        );
        assert_eq!(
            range(Some("2025-10-11"), Some("2026-10-12")),
            Err(RangeError::TooLong)
        );
        assert_eq!(
            range(Some("2026-10-02"), Some("2026-10-01")),
            Err(RangeError::Reversed)
        );
        assert_eq!(
            range(Some("2026-10-02"), Some("2026-10-13")),
            Err(RangeError::Future)
        );
        assert_eq!(range(Some("2026-10-02"), None), Err(RangeError::HalfRange));
        assert_eq!(range(None, Some("2026-10-02")), Err(RangeError::HalfRange));
        assert!(matches!(
            range(Some("2026-13-01"), Some("2026-10-02")),
            Err(RangeError::BadDate(_))
        ));
        assert!(matches!(
            range(Some("yesterday"), Some("2026-10-02")),
            Err(RangeError::BadDate(_))
        ));
        assert_eq!(
            personal(RangeQuery {
                period: Some("7d"),
                from: Some("2026-10-01"),
                to: Some("2026-10-02"),
            }),
            Err(RangeError::PeriodAndRange)
        );
        assert!(matches!(
            personal(RangeQuery {
                period: Some("week"),
                ..Default::default()
            }),
            Err(RangeError::UnknownPeriod(_))
        ));
    }

    #[test]
    fn community_ranges_stay_within_the_kept_weights() {
        let earliest = day("2025-10-06");
        let community = |q| {
            resolve(
                q,
                day(TODAY),
                RangeRules {
                    default: ChartPeriod::Days7,
                    earliest: Some(earliest),
                },
            )
        };
        let year = community(RangeQuery {
            period: Some("12m"),
            ..Default::default()
        })
        .unwrap();
        assert!(year.from.unwrap() >= earliest);
        assert_eq!(
            community(RangeQuery {
                period: Some("all"),
                ..Default::default()
            }),
            Err(RangeError::NoAllTime)
        );
        assert_eq!(
            community(RangeQuery {
                period: None,
                from: Some("2025-10-05"),
                to: Some("2025-12-31"),
            }),
            Err(RangeError::TooOld(earliest))
        );
    }

    #[test]
    fn community_charts_read_a_bounded_number_of_user_days() {
        assert_eq!(community_accounts(7, 2000), 2000);
        assert_eq!(community_accounts(30, 2000), 1216);
        assert_eq!(community_accounts(365, 2000), 100);
        for days in [1, 7, 30, 90, 365, 366] {
            assert!(community_accounts(days, 2000) * days <= COMMUNITY_USER_DAYS);
        }
    }
}
