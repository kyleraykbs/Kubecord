use kc_core::Snowflake;
use serde::{Deserialize, Serialize};

use crate::user::User;

/// Channel types, including the ones Discord adds without warning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChannelType {
    GuildText,
    Dm,
    GuildVoice,
    GroupDm,
    GuildCategory,
    GuildAnnouncement,
    AnnouncementThread,
    PublicThread,
    PrivateThread,
    GuildStageVoice,
    GuildDirectory,
    GuildForum,
    GuildMedia,
    Unknown(u8),
}

impl ChannelType {
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::GuildText,
            1 => Self::Dm,
            2 => Self::GuildVoice,
            3 => Self::GroupDm,
            4 => Self::GuildCategory,
            5 => Self::GuildAnnouncement,
            10 => Self::AnnouncementThread,
            11 => Self::PublicThread,
            12 => Self::PrivateThread,
            13 => Self::GuildStageVoice,
            14 => Self::GuildDirectory,
            15 => Self::GuildForum,
            16 => Self::GuildMedia,
            other => Self::Unknown(other),
        }
    }

    pub const fn as_u8(self) -> u8 {
        match self {
            Self::GuildText => 0,
            Self::Dm => 1,
            Self::GuildVoice => 2,
            Self::GroupDm => 3,
            Self::GuildCategory => 4,
            Self::GuildAnnouncement => 5,
            Self::AnnouncementThread => 10,
            Self::PublicThread => 11,
            Self::PrivateThread => 12,
            Self::GuildStageVoice => 13,
            Self::GuildDirectory => 14,
            Self::GuildForum => 15,
            Self::GuildMedia => 16,
            Self::Unknown(value) => value,
        }
    }

    /// Channels that can carry a text history.
    pub const fn is_text(self) -> bool {
        matches!(
            self,
            Self::GuildText
                | Self::Dm
                | Self::GroupDm
                | Self::GuildAnnouncement
                | Self::AnnouncementThread
                | Self::PublicThread
                | Self::PrivateThread
                | Self::GuildForum
                | Self::GuildMedia
        )
    }

    /// Channels that can be joined for voice/video.
    pub const fn is_voice(self) -> bool {
        matches!(
            self,
            Self::GuildVoice | Self::Dm | Self::GroupDm | Self::GuildStageVoice
        )
    }

    pub const fn is_thread(self) -> bool {
        matches!(
            self,
            Self::AnnouncementThread | Self::PublicThread | Self::PrivateThread
        )
    }

    pub const fn is_category(self) -> bool {
        matches!(self, Self::GuildCategory)
    }
}

impl Serialize for ChannelType {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.as_u8())
    }
}

impl<'de> Deserialize<'de> for ChannelType {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        u8::deserialize(deserializer).map(Self::from_u8)
    }
}

/// One permission overwrite of a guild channel.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overwrite {
    pub id: Snowflake,
    /// 0 for a role, 1 for a member.
    #[serde(rename = "type", default)]
    pub kind: u8,
    #[serde(default, deserialize_with = "crate::serde_util::u64_string")]
    pub allow: u64,
    #[serde(default, deserialize_with = "crate::serde_util::u64_string")]
    pub deny: u64,
}

impl Overwrite {
    pub const ROLE: u8 = 0;
    pub const MEMBER: u8 = 1;
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ThreadMetadata {
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub locked: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Channel {
    pub id: Snowflake,
    #[serde(rename = "type")]
    pub kind: ChannelType,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub position: Option<i32>,
    #[serde(default)]
    pub parent_id: Option<Snowflake>,
    #[serde(default)]
    pub recipients: Option<Vec<User>>,
    #[serde(default)]
    pub last_message_id: Option<Snowflake>,
    #[serde(default)]
    pub nsfw: Option<bool>,
    /// DMs under the `DEDUPE_USER_OBJECTS` capability list recipient ids
    /// here; the reducer resolves them into `recipients`.
    #[serde(default)]
    pub recipient_ids: Option<Vec<Snowflake>>,
    #[serde(default, deserialize_with = "crate::serde_util::null_default")]
    pub permission_overwrites: Vec<Overwrite>,
    /// Group DM icon hash.
    #[serde(default)]
    pub icon: Option<String>,
    /// Group DM owner, or the creator of a thread.
    #[serde(default)]
    pub owner_id: Option<Snowflake>,
    /// Slowmode, in seconds.
    #[serde(default)]
    pub rate_limit_per_user: Option<u32>,
    #[serde(default)]
    pub thread_metadata: Option<ThreadMetadata>,
    /// Voice channels: bits per second.
    #[serde(default)]
    pub bitrate: Option<u32>,
    /// Voice channels: most people at once; 0 is no limit.
    #[serde(default)]
    pub user_limit: Option<u32>,
    /// Present on a thread the current user has joined.
    #[serde(default, skip_serializing)]
    pub member: Option<serde_json::Value>,
}

impl Channel {
    pub fn display_name(&self) -> String {
        match (&self.name, &self.recipients) {
            (Some(name), _) => name.clone(),
            (None, Some(recipients)) if recipients.len() == 1 => {
                recipients[0].display_name().to_owned()
            }
            (None, Some(recipients)) => recipients
                .iter()
                .map(User::display_name)
                .collect::<Vec<_>>()
                .join(", "),
            (None, None) => self.id.to_string(),
        }
    }

    pub fn is_private(&self) -> bool {
        matches!(self.kind, ChannelType::Dm | ChannelType::GroupDm)
    }

    /// The other party of a one-to-one DM.
    pub fn dm_recipient(&self) -> Option<&User> {
        match (self.kind, self.recipients.as_deref()) {
            (ChannelType::Dm, Some([user])) => Some(user),
            _ => None,
        }
    }

    /// Sort key: categories and channels interleave by Discord's `position`.
    pub fn sort_key(&self) -> (i32, u64) {
        (self.position.unwrap_or(0), self.id.get())
    }
}
