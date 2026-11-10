//! UTC timestamps on the wire: always `YYYY-MM-DDTHH:MM:SSZ`, with no fractions or offsets, so a
//! microcontroller can parse them with a fixed-width scan.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::format_description::FormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

const WIRE: &[FormatItem<'static>] = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

/// A UTC instant with whole-second precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(OffsetDateTime);

impl Timestamp {
    /// Truncates to the second and converts to UTC.
    pub fn new(t: OffsetDateTime) -> Self {
        let utc = t.to_offset(UtcOffset::UTC);
        Self(
            utc.replace_nanosecond(0)
                .expect("zero nanoseconds is always valid"),
        )
    }

    pub fn from_unix(seconds: i64) -> Option<Self> {
        OffsetDateTime::from_unix_timestamp(seconds).ok().map(Self::new)
    }

    pub fn unix(self) -> i64 {
        self.0.unix_timestamp()
    }

    pub fn get(self) -> OffsetDateTime {
        self.0
    }

    /// Parses only the exact wire format.
    pub fn parse(s: &str) -> Result<Self, TimestampError> {
        if s.len() != 20 {
            return Err(TimestampError(s.chars().take(40).collect()));
        }
        PrimitiveDateTime::parse(s, WIRE)
            .map(|p| Self(p.assume_utc()))
            .map_err(|_| TimestampError(s.to_string()))
    }

    /// Seconds from `self` to `later` (negative if `later` is earlier).
    pub fn seconds_until(self, later: Timestamp) -> i64 {
        later.unix() - self.unix()
    }

    pub fn plus_seconds(self, s: i64) -> Self {
        Self(self.0 + time::Duration::seconds(s))
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.format(WIRE).expect("UTC instants always format"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a UTC timestamp in YYYY-MM-DDTHH:MM:SSZ form: {0:?}")]
pub struct TimestampError(String);

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Timestamp::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_the_wire_format() {
        let t = Timestamp::parse("2026-11-10T20:00:00Z").unwrap();
        assert_eq!(t.to_string(), "2026-11-10T20:00:00Z");
        assert_eq!(Timestamp::from_unix(t.unix()).unwrap(), t);
    }

    #[test]
    fn rejects_other_forms() {
        for bad in [
            "2026-11-10T20:00:00.5Z",
            "2026-11-10T20:00:00+00:00",
            "2026-11-10 20:00:00Z",
            "2026-11-10T20:00:00z",
            "2026-13-10T20:00:00Z",
            "",
        ] {
            assert!(Timestamp::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn truncates_fractions_and_offsets() {
        let t = OffsetDateTime::from_unix_timestamp_nanos(1_794_355_200_750_000_000).unwrap();
        assert_eq!(Timestamp::new(t).to_string(), "2026-11-11T00:00:00Z");
    }
}
