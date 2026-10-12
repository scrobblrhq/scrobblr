//! Keyset cursors for lists paged newest first: where a page ended, as an
//! opaque string for the next request.

use chrono::{DateTime, Utc};

/// The last entry of a page by (time, id); the next page starts below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub at: DateTime<Utc>,
    pub id: i64,
}

impl Cursor {
    pub fn encode(&self) -> String {
        format!("{}.{}", self.at.timestamp_micros(), self.id)
    }

    pub fn parse(s: &str) -> Option<Cursor> {
        let (micros, id) = s.split_once('.')?;
        Some(Cursor {
            at: DateTime::from_timestamp_micros(micros.parse().ok()?)?,
            id: id.parse().ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_to_the_microsecond() {
        let at = DateTime::from_timestamp_micros(1_760_300_000_123_456).unwrap();
        let cursor = Cursor { at, id: 42 };
        assert_eq!(cursor.encode(), "1760300000123456.42");
        assert_eq!(Cursor::parse(&cursor.encode()), Some(cursor));
        for bad in [
            "",
            "42",
            "x.1",
            "1.x",
            "1760300000123456",
            "9999999999999999999.1",
        ] {
            assert_eq!(Cursor::parse(bad), None, "{bad}");
        }
    }
}
