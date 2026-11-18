use std::fmt;

use kc_core::Snowflake;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: Snowflake,
    pub username: String,
    #[serde(default)]
    pub discriminator: Option<String>,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub bot: Option<bool>,
    #[serde(default)]
    pub system: Option<bool>,
    /// 0 none, 1 Nitro Classic, 2 Nitro, 3 Nitro Basic. Only sent for the
    /// current user and in profiles.
    #[serde(default)]
    pub premium_type: Option<u8>,
}

impl User {
    /// Whether the account can use custom emoji from other guilds and
    /// animated emoji.
    pub fn has_nitro(&self) -> bool {
        matches!(self.premium_type, Some(1..=3))
    }
}

impl User {
    /// What Discord shows next to a message: the global display name when
    /// present, otherwise the unique username.
    pub fn display_name(&self) -> &str {
        self.global_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.username)
    }

    /// Avatar URL, or `None` when the user keeps the default avatar.
    pub fn avatar_url(&self, size: u32) -> Option<String> {
        let hash = self.avatar.as_deref()?;
        let ext = if hash.starts_with("a_") { "gif" } else { "png" };
        Some(format!(
            "https://cdn.discordapp.com/avatars/{}/{hash}.{ext}?size={size}",
            self.id
        ))
    }
}

impl fmt::Display for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}
