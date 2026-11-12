use kc_core::Snowflake;
use serde::{Deserialize, Serialize};

use crate::channel::Channel;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Guild {
    pub id: Snowflake,
    pub name: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub owner_id: Option<Snowflake>,
    #[serde(default)]
    pub unavailable: bool,
    /// Present on `GUILD_CREATE`, absent on the `READY` guild stubs.
    #[serde(default)]
    pub channels: Option<Vec<Channel>>,
}

impl Guild {
    pub fn icon_url(&self, size: u32) -> Option<String> {
        let hash = self.icon.as_deref()?;
        let ext = if hash.starts_with("a_") { "gif" } else { "png" };
        Some(format!(
            "https://cdn.discordapp.com/icons/{}/{hash}.{ext}?size={size}",
            self.id
        ))
    }

    /// Initials used when the guild has no icon.
    pub fn initials(&self) -> String {
        self.name
            .split_whitespace()
            .filter_map(|word| word.chars().next())
            .take(2)
            .collect::<String>()
            .to_uppercase()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VoiceState {
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    #[serde(default)]
    pub channel_id: Option<Snowflake>,
    pub user_id: Snowflake,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub deaf: bool,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub self_deaf: bool,
    #[serde(default)]
    pub self_mute: bool,
    #[serde(default)]
    pub self_video: bool,
    #[serde(default)]
    pub self_stream: bool,
    #[serde(default)]
    pub suppress: bool,
}

impl VoiceState {
    pub fn key(&self) -> (Option<Snowflake>, Snowflake) {
        (self.guild_id, self.user_id)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Presence {
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    pub user_id: Snowflake,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub activities: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Relationship {
    pub id: Snowflake,
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(default)]
    pub nickname: Option<String>,
}

/// A Go Live stream, from `STREAM_CREATE`/`STREAM_UPDATE`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stream {
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    #[serde(default)]
    pub channel_id: Snowflake,
    #[serde(default)]
    pub owner_id: Option<Snowflake>,
    #[serde(default)]
    pub server_id: Option<Snowflake>,
    #[serde(default)]
    pub rtc_server_id: Option<Snowflake>,
    #[serde(default)]
    pub stream_key: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub paused: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guild_initials() {
        let guild = Guild {
            id: Snowflake(1),
            name: "kyle's cool server".into(),
            ..Guild::default()
        };
        assert_eq!(guild.initials(), "KC");
    }
}
