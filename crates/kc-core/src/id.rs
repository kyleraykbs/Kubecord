use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};

/// A Discord snowflake.
///
/// Discord serialises these as decimal strings. Accepts either a JSON string or
/// a JSON number on the way in, always serialises as a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Snowflake(pub u64);

impl Snowflake {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// Milliseconds since the Discord epoch (2015-01-01).
    pub const fn timestamp(self) -> u64 {
        (self.0 >> 22) + Self::DISCORD_EPOCH
    }

    const DISCORD_EPOCH: u64 = 1_420_070_400_000;
}

impl fmt::Display for Snowflake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for Snowflake {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        s.parse::<u64>()
            .map(Self)
            .map_err(|_| Error::Snowflake(s.to_owned()))
    }
}

impl From<u64> for Snowflake {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl Serialize for Snowflake {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Snowflake {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = Snowflake;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a snowflake as a decimal string or an integer")
            }

            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<Self::Value, E> {
                value
                    .parse::<u64>()
                    .map(Snowflake)
                    .map_err(|_| E::custom(format!("invalid snowflake {value:?}")))
            }

            fn visit_u64<E: serde::de::Error>(
                self,
                value: u64,
            ) -> std::result::Result<Self::Value, E> {
                Ok(Snowflake(value))
            }

            fn visit_i64<E: serde::de::Error>(
                self,
                value: i64,
            ) -> std::result::Result<Self::Value, E> {
                u64::try_from(value)
                    .map(Snowflake)
                    .map_err(|_| E::custom(format!("negative snowflake {value}")))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_serialises_as_string() {
        let id: Snowflake = serde_json::from_str(r#""175928847299117063""#).unwrap();
        assert_eq!(id, Snowflake(175_928_847_299_117_063));
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            r#""175928847299117063""#
        );
    }

    #[test]
    fn accepts_numeric_encoding() {
        let id: Snowflake = serde_json::from_str("175928847299117063").unwrap();
        assert_eq!(id.0, 175_928_847_299_117_063);
    }

    #[test]
    fn round_trips_display() {
        let id = Snowflake(42);
        assert_eq!(id.to_string().parse::<Snowflake>().unwrap(), id);
    }
}
