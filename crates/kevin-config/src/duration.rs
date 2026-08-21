//! `humantime` serde representation for [`std::time::Duration`] (`"30s"`, `"2h"`, `"15m"`).
//!
//! Use with `#[serde(with = "crate::duration")]`. The string form is the only
//! accepted form: numbers without a unit are rejected so a config never silently
//! changes meaning between seconds and milliseconds.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Serializes `d` as a humantime string (`30s`, `2h`, `1h 30m`).
pub fn serialize<S: Serializer>(d: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
    humantime::format_duration(*d)
        .to_string()
        .serialize(serializer)
}

/// Deserializes a humantime string into a [`Duration`].
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    let raw = String::deserialize(deserializer)?;
    parse(&raw).map_err(serde::de::Error::custom)
}

/// Parses a humantime duration string (`"30s"`, `"2h"`, `"1h 30m"`).
pub fn parse(raw: &str) -> Result<Duration, String> {
    humantime::parse_duration(raw.trim()).map_err(|e| {
        format!("invalid duration {raw:?} (expected e.g. \"30s\", \"15m\", \"2h\"): {e}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Holder {
        #[serde(with = "super")]
        d: Duration,
    }

    #[test]
    fn round_trips_through_toml() {
        for (text, secs) in [("30s", 30), ("15m", 900), ("2h", 7200), ("10m", 600)] {
            let h: Holder = toml::from_str(&format!("d = \"{text}\"")).unwrap();
            assert_eq!(h.d, Duration::from_secs(secs));
            assert_eq!(toml::to_string(&h).unwrap(), format!("d = \"{text}\"\n"));
        }
    }

    #[test]
    fn rejects_bare_numbers_and_garbage() {
        assert!(toml::from_str::<Holder>("d = 30").is_err());
        assert!(toml::from_str::<Holder>("d = \"soon\"").is_err());
        assert!(toml::from_str::<Holder>("d = \"\"").is_err());
    }
}
