//! Per-account settings that shape the sidebar: read states, notification
//! settings and the guild folder layout.

use std::collections::HashMap;

use kc_core::Snowflake;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What the account has read in one channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadState {
    pub last_message_id: Option<Snowflake>,
    pub mention_count: u32,
}

/// One entry of the legacy `guild_folders` user setting. An entry without an
/// id that holds a single guild is not a folder: it only records that guild's
/// position in the sidebar.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildFolder {
    #[serde(default, deserialize_with = "crate::serde_util::opt_i64_lenient")]
    pub id: Option<i64>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub color: Option<u32>,
    #[serde(default, deserialize_with = "crate::serde_util::null_default")]
    pub guild_ids: Vec<Snowflake>,
}

impl GuildFolder {
    pub fn is_folder(&self) -> bool {
        self.id.is_some_and(|id| id != 0) || self.guild_ids.len() > 1
    }
}

/// One slot of the server rail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuildEntry {
    Guild(Snowflake),
    Folder {
        id: i64,
        name: Option<String>,
        color: Option<u32>,
        guild_ids: Vec<Snowflake>,
    },
}

/// A mute with an optional expiry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mute {
    pub muted: bool,
    /// ISO 8601 end time; `None` means until unmuted.
    pub end_time: Option<String>,
}

impl Mute {
    fn parse(value: &Value) -> Self {
        Self {
            muted: value["muted"].as_bool().unwrap_or(false),
            end_time: value
                .pointer("/mute_config/end_time")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }
    }

    /// `now` is an ISO 8601 UTC timestamp; ISO timestamps in the same zone
    /// compare correctly as strings.
    pub fn active(&self, now: &str) -> bool {
        self.muted
            && self
                .end_time
                .as_deref()
                .is_none_or(|end| comparable(end) > comparable(now))
    }
}

/// The date-time prefix shared by every ISO 8601 form Discord emits.
fn comparable(timestamp: &str) -> &str {
    timestamp.get(..19).unwrap_or(timestamp)
}

/// Message notification levels, as Discord numbers them.
pub mod level {
    pub const ALL_MESSAGES: u8 = 0;
    pub const ONLY_MENTIONS: u8 = 1;
    pub const NO_MESSAGES: u8 = 2;
    /// A channel follows its category, then the server.
    pub const INHERIT: u8 = 3;
}

/// A channel's own notification settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelOverride {
    pub mute: Mute,
    /// One of [`level`]; [`level::INHERIT`] when the channel has none.
    pub message_notifications: u8,
}

impl Default for ChannelOverride {
    fn default() -> Self {
        Self {
            mute: Mute::default(),
            message_notifications: level::INHERIT,
        }
    }
}

impl ChannelOverride {
    fn parse(value: &Value) -> Self {
        Self {
            mute: Mute::parse(value),
            message_notifications: value["message_notifications"]
                .as_u64()
                .map_or(level::INHERIT, |level| level as u8),
        }
    }
}

/// Notification settings for one guild (or for DMs when the key is `None`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuildSettings {
    pub mute: Mute,
    pub suppress_everyone: bool,
    pub suppress_roles: bool,
    /// One of [`level`]: 0 all messages, 1 only mentions, 2 nothing (3,
    /// or a guild without settings: the server's default).
    pub message_notifications: u8,
    pub channel_overrides: HashMap<Snowflake, ChannelOverride>,
}

impl GuildSettings {
    /// Parses one `user_guild_settings` entry; returns the guild id (`None`
    /// for the DM settings) with the settings.
    pub fn parse(value: &Value) -> (Option<Snowflake>, Self) {
        let guild_id = value["guild_id"].as_str().and_then(|id| id.parse().ok());
        let mut channel_overrides = HashMap::new();
        // READY sends an array; the PATCH response and some updates use a map
        // keyed by channel id.
        match &value["channel_overrides"] {
            Value::Array(overrides) => {
                for entry in overrides {
                    if let Some(id) = entry["channel_id"].as_str().and_then(|id| id.parse().ok()) {
                        channel_overrides.insert(id, ChannelOverride::parse(entry));
                    }
                }
            }
            Value::Object(overrides) => {
                for (id, entry) in overrides {
                    if let Ok(id) = id.parse() {
                        channel_overrides.insert(id, ChannelOverride::parse(entry));
                    }
                }
            }
            _ => {}
        }
        (
            guild_id,
            Self {
                mute: Mute::parse(value),
                suppress_everyone: value["suppress_everyone"].as_bool().unwrap_or(false),
                suppress_roles: value["suppress_roles"].as_bool().unwrap_or(false),
                message_notifications: value["message_notifications"]
                    .as_u64()
                    .map_or(level::INHERIT, |level| level as u8),
                channel_overrides,
            },
        )
    }
}

/// The account's own custom status.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CustomStatus {
    pub text: Option<String>,
    pub emoji_name: Option<String>,
    pub emoji_id: Option<Snowflake>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn single_guild_entries_are_not_folders() {
        let position: GuildFolder =
            serde_json::from_value(json!({"id": null, "guild_ids": ["1"]})).unwrap();
        assert!(!position.is_folder());
        let folder: GuildFolder =
            serde_json::from_value(json!({"id": "123", "name": "x", "guild_ids": ["1"]})).unwrap();
        assert!(folder.is_folder());
        assert_eq!(folder.id, Some(123));
    }

    #[test]
    fn mutes_expire() {
        let (_, settings) = GuildSettings::parse(&json!({
            "guild_id": "1",
            "muted": true,
            "mute_config": {"end_time": "2026-01-01T00:00:00+00:00", "selected_time_window": 3600},
            "channel_overrides": [{"channel_id": "5", "muted": true, "mute_config": null}]
        }));
        assert!(settings.mute.active("2025-12-31T23:00:00.000Z"));
        assert!(!settings.mute.active("2026-06-01T00:00:00.000Z"));
        assert!(
            settings.channel_overrides[&Snowflake(5)]
                .mute
                .active("2030-01-01T00:00:00Z")
        );
    }

    #[test]
    fn notification_levels_are_read_for_guilds_and_channels() {
        let (guild, settings) = GuildSettings::parse(&json!({
            "guild_id": "1",
            "message_notifications": 1,
            "suppress_everyone": true,
            "channel_overrides": [
                {"channel_id": "5", "message_notifications": 0, "muted": false},
                {"channel_id": "6", "message_notifications": 3, "muted": true},
                {"channel_id": "7", "muted": false}
            ]
        }));
        assert_eq!(guild, Some(Snowflake(1)));
        assert_eq!(settings.message_notifications, level::ONLY_MENTIONS);
        assert!(settings.suppress_everyone && !settings.suppress_roles);
        let of = |id| settings.channel_overrides[&Snowflake(id)].message_notifications;
        assert_eq!(of(5), level::ALL_MESSAGES);
        assert_eq!(of(6), level::INHERIT);
        assert_eq!(of(7), level::INHERIT);

        // The update and PATCH response form: overrides keyed by channel.
        let (dms, settings) = GuildSettings::parse(&json!({
            "guild_id": null,
            "channel_overrides": {"9": {"muted": true, "message_notifications": 2}}
        }));
        assert_eq!(dms, None);
        assert_eq!(settings.message_notifications, level::INHERIT);
        let dm = &settings.channel_overrides[&Snowflake(9)];
        assert!(dm.mute.muted);
        assert_eq!(dm.message_notifications, level::NO_MESSAGES);
    }
}
