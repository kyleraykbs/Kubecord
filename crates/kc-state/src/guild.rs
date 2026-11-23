use kc_core::Snowflake;
use serde::{Deserialize, Serialize};

use crate::channel::Channel;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub id: Snowflake,
    #[serde(default)]
    pub name: String,
    /// RGB colour; zero means the role does not colour names.
    #[serde(default)]
    pub color: u32,
    /// Shown as its own group in the member list.
    #[serde(default)]
    pub hoist: bool,
    #[serde(default)]
    pub position: i32,
    #[serde(default, deserialize_with = "crate::serde_util::u64_string")]
    pub permissions: u64,
    #[serde(default)]
    pub mentionable: bool,
    /// Owned by an integration (a bot's own role, the booster role): it
    /// cannot be assigned or removed by hand.
    #[serde(default)]
    pub managed: bool,
}

/// A custom guild emoji.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Emoji {
    pub id: Snowflake,
    #[serde(default, deserialize_with = "crate::serde_util::null_default")]
    pub name: String,
    #[serde(default)]
    pub animated: bool,
    /// False when the guild lost the boost level that provided the slot.
    #[serde(default = "default_true")]
    pub available: bool,
    /// When non-empty, only members with one of these roles may use it.
    #[serde(default, deserialize_with = "crate::serde_util::null_default")]
    pub roles: Vec<Snowflake>,
}

/// A sticker: a server's own, or one of Discord's packs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sticker {
    pub id: Snowflake,
    #[serde(default, deserialize_with = "crate::serde_util::null_default")]
    pub name: String,
    #[serde(default, deserialize_with = "crate::serde_util::null_default")]
    pub description: String,
    /// Words it is found by (for a server's own, an emoji's name).
    #[serde(default, deserialize_with = "crate::serde_util::null_default")]
    pub tags: String,
    /// 1 PNG, 2 APNG, 3 Lottie, 4 GIF.
    #[serde(default)]
    pub format_type: u8,
    #[serde(default = "default_true")]
    pub available: bool,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    #[serde(default)]
    pub pack_id: Option<Snowflake>,
}

/// A soundboard sound: Discord's own (no guild) or a server's.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SoundboardSound {
    pub sound_id: Snowflake,
    #[serde(default)]
    pub name: String,
    /// 0.0 to 1.0.
    #[serde(default = "full_volume")]
    pub volume: f64,
    #[serde(default)]
    pub emoji_id: Option<Snowflake>,
    #[serde(default)]
    pub emoji_name: Option<String>,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    #[serde(default = "default_true")]
    pub available: bool,
}

fn full_volume() -> f64 {
    1.0
}

/// A stage that is live: its channel and topic.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageInstance {
    pub id: Snowflake,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    pub channel_id: Snowflake,
    #[serde(default)]
    pub topic: String,
    /// 1 public, 2 server members only.
    #[serde(default)]
    pub privacy_level: u8,
}

/// One of Discord's sticker packs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StickerPack {
    pub id: Snowflake,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub stickers: Vec<Sticker>,
}

fn default_true() -> bool {
    true
}

impl Emoji {
    /// The text that represents this emoji in message content.
    pub fn mention(&self) -> String {
        if self.animated {
            format!("<a:{}:{}>", self.name, self.id)
        } else {
            format!("<:{}:{}>", self.name, self.id)
        }
    }
}

/// A user's membership of one guild.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    #[serde(default)]
    pub nick: Option<String>,
    /// Per-guild avatar hash.
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default, deserialize_with = "crate::serde_util::null_default")]
    pub roles: Vec<Snowflake>,
    #[serde(default)]
    pub joined_at: Option<String>,
    /// While in the future, the member is timed out (ISO 8601).
    #[serde(default)]
    pub communication_disabled_until: Option<String>,
}

impl Member {
    /// Whether a timeout is running at `now_millis` (Unix milliseconds).
    pub fn timed_out_at(&self, now_millis: u64) -> bool {
        self.communication_disabled_until
            .as_deref()
            .and_then(crate::time::iso_to_unix_millis)
            .is_some_and(|until| until > now_millis)
    }
}

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
    #[serde(default, deserialize_with = "crate::serde_util::null_default")]
    pub roles: Vec<Role>,
    #[serde(default, deserialize_with = "crate::serde_util::null_default")]
    pub emojis: Vec<Emoji>,
    #[serde(default, deserialize_with = "crate::serde_util::null_default")]
    pub stickers: Vec<Sticker>,
    /// The server's soundboard (arrives on request: gateway op 31).
    #[serde(default, deserialize_with = "crate::serde_util::null_default")]
    pub soundboard_sounds: Vec<SoundboardSound>,
    /// Stages live now, one per stage channel at most.
    #[serde(default, deserialize_with = "crate::serde_util::null_default")]
    pub stage_instances: Vec<StageInstance>,
    #[serde(default)]
    pub banner: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub member_count: Option<u64>,
    /// When the current user joined, as an ISO 8601 timestamp.
    #[serde(default)]
    pub joined_at: Option<String>,
    #[serde(default)]
    pub afk_channel_id: Option<Snowflake>,
    /// Seconds of silence before a member is moved to the AFK channel.
    #[serde(default)]
    pub afk_timeout: Option<u32>,
    /// Where join and boost messages are posted.
    #[serde(default)]
    pub system_channel_id: Option<Snowflake>,
    /// 0: all messages, 1: only mentions.
    #[serde(default)]
    pub default_message_notifications: Option<u8>,
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
    /// In a stage's audience (only speakers are heard).
    #[serde(default)]
    pub suppress: bool,
    /// In a stage's audience, asking to speak since then.
    #[serde(default)]
    pub request_to_speak_timestamp: Option<String>,
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

impl Presence {
    /// Parses a presence from READY (`user_id`) or PRESENCE_UPDATE (`user.id`).
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        let user_id = value["user_id"]
            .as_str()
            .or_else(|| {
                value
                    .pointer("/user/id")
                    .and_then(serde_json::Value::as_str)
            })?
            .parse()
            .ok()?;
        Some(Self {
            guild_id: value["guild_id"].as_str().and_then(|id| id.parse().ok()),
            user_id,
            status: value["status"].as_str().map(str::to_owned),
            activities: value["activities"].as_array().cloned().unwrap_or_default(),
        })
    }

    /// `online`, `idle`, `dnd` or `offline`.
    pub fn status(&self) -> &str {
        self.status.as_deref().unwrap_or("offline")
    }

    /// The custom status text (activity type 4), with its emoji when unicode
    /// (a server emoji has no text form; see [`Presence::status_line`]).
    pub fn custom_status(&self) -> Option<String> {
        self.custom_status_line().map(|line| line.plain()).filter(|text| !text.is_empty())
    }

    /// The custom status (activity type 4): its emoji, Unicode or a server
    /// emoji, and its text.
    pub fn custom_status_line(&self) -> Option<StatusLine> {
        let activity = self
            .activities
            .iter()
            .find(|activity| activity["type"] == 4)?;
        let text = activity["state"].as_str().unwrap_or("").trim().to_owned();
        let emoji = activity.get("emoji").and_then(StatusEmoji::from_json);
        (emoji.is_some() || !text.is_empty()).then_some(StatusLine { emoji, text })
    }

    /// What to show under a name: the custom status, else what the user is
    /// doing ("Playing ..."), with the custom status's emoji when it has one.
    pub fn status_line(&self) -> Option<StatusLine> {
        if let Some(custom) = self.custom_status_line() {
            return Some(custom);
        }
        let activity = self
            .activities
            .iter()
            .find(|activity| activity["type"] != 4)?;
        let name = activity["name"].as_str()?;
        let verb = match activity["type"].as_u64().unwrap_or(0) {
            1 => "Streaming",
            2 => "Listening to",
            3 => "Watching",
            5 => "Competing in",
            _ => "Playing",
        };
        Some(StatusLine {
            emoji: None,
            text: format!("{verb} {name}"),
        })
    }

    /// A one-line description of what the user is doing: the custom status,
    /// otherwise "Playing X" / "Listening to X" and so on.
    pub fn summary(&self) -> Option<String> {
        self.status_line()
            .map(|line| line.plain())
            .filter(|text| !text.is_empty())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Relationship {
    /// The other user's id.
    pub id: Snowflake,
    #[serde(rename = "type", default)]
    pub kind: u8,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub since: Option<String>,
}

impl Relationship {
    pub const FRIEND: u8 = 1;
    pub const BLOCKED: u8 = 2;
    pub const INCOMING_REQUEST: u8 = 3;
    pub const OUTGOING_REQUEST: u8 = 4;
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
    /// Who is watching (STREAM_UPDATE).
    #[serde(default)]
    pub viewer_ids: Vec<Snowflake>,
}

/// The emoji beside a status: a Unicode emoji, or a server (custom) emoji.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StatusEmoji {
    Unicode(String),
    Custom {
        id: Snowflake,
        name: String,
        animated: bool,
    },
}

impl StatusEmoji {
    /// From an activity's or setting's `{"name", "id", "animated"}`.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let name = value["name"].as_str().unwrap_or("").to_owned();
        let id = value["id"].as_str().and_then(|id| id.parse().ok());
        match id {
            Some(id) => Some(Self::Custom {
                id,
                name,
                animated: value["animated"].as_bool().unwrap_or(false),
            }),
            None => (!name.is_empty()).then_some(Self::Unicode(name)),
        }
    }
}

/// A status as shown under a name: an optional emoji, and text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct StatusLine {
    pub emoji: Option<StatusEmoji>,
    pub text: String,
}

impl StatusLine {
    /// The line as plain text: a Unicode emoji is kept, a server emoji (which
    /// has no text form) is left out.
    pub fn plain(&self) -> String {
        match &self.emoji {
            Some(StatusEmoji::Unicode(emoji)) if self.text.is_empty() => emoji.clone(),
            Some(StatusEmoji::Unicode(emoji)) => format!("{emoji} {}", self.text),
            _ => self.text.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn custom_status_emoji_is_read_whether_unicode_or_a_server_emoji() {
        let presence = |activities: serde_json::Value| Presence {
            guild_id: None,
            user_id: Snowflake(1),
            status: Some("online".to_owned()),
            activities: activities.as_array().cloned().unwrap_or_default(),
        };
        // A server emoji keeps its id; it has no text form.
        let custom = presence(serde_json::json!([{"type": 4, "state": "in the lab",
            "emoji": {"id": "236622542490566658", "name": "tanooki", "animated": true}}]));
        let line = custom.status_line().expect("a status line");
        assert_eq!(
            line.emoji,
            Some(StatusEmoji::Custom { id: Snowflake(236622542490566658), name: "tanooki".to_owned(), animated: true })
        );
        assert_eq!(line.plain(), "in the lab");
        // A Unicode emoji is part of the plain text; an emoji alone counts.
        let unicode = presence(serde_json::json!([{"type": 4, "emoji": {"name": "🚀"}}]));
        assert_eq!(unicode.summary().as_deref(), Some("🚀"));
        // Without a custom status, the activity.
        let playing = presence(serde_json::json!([{"type": 0, "name": "Celeste"}]));
        assert_eq!(playing.status_line().unwrap().text, "Playing Celeste");
    }

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
