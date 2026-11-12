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

    /// Sort key: categories and channels interleave by Discord's `position`.
    pub fn sort_key(&self) -> (i32, u64) {
        (self.position.unwrap_or(0), self.id.get())
    }
}
