use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A Discord user token.
///
/// The `Debug` implementation never prints the token itself; anything that
/// formats a `Token` with `{:?}` stays safe to log.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    pub fn new(value: impl Into<String>) -> Self {
        let mut value = value.into();
        let trimmed = value.trim();
        if trimmed.len() != value.len() {
            value = trimmed.to_owned();
        }
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The part Discord uses for display/debugging: first six and last four
    /// characters. Never enough to reconstruct the token.
    pub fn redacted(&self) -> String {
        let chars: Vec<char> = self.0.chars().collect();
        if chars.len() <= 12 {
            return "<redacted>".to_owned();
        }
        let head: String = chars[..6].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}…{tail}")
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Token").field(&self.redacted()).finish()
    }
}

impl From<&str> for Token {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for Token {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl Serialize for Token {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Token {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        // A deliberately fake, non-token-shaped string (no dots, so secret
        // scanners do not mistake it for a real Discord token). Redaction keeps
        // only the first six and last four characters.
        let token = Token::new("MTA5ND_middle_part_stays_secret_and_hidden_TAIL");
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("middle_part"));
        assert!(!rendered.contains("secret"));
        assert!(rendered.contains("MTA5ND"));
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(Token::new("  abc  ").expose(), "abc");
    }
}
