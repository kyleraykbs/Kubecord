use kc_core::Snowflake;
use serde::{Deserialize, Serialize};

use crate::user::User;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attachment {
    pub id: Snowflake,
    pub filename: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub proxy_url: String,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    /// Alt text.
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub flags: u64,
}

impl Attachment {
    pub fn is_spoiler(&self) -> bool {
        self.filename.starts_with("SPOILER_") || self.flags & (1 << 3) != 0
    }

    fn content_type_is(&self, prefix: &str) -> bool {
        self.content_type
            .as_deref()
            .is_some_and(|kind| kind.starts_with(prefix))
    }

    pub fn is_image(&self) -> bool {
        self.content_type_is("image/") && self.width.is_some()
    }

    pub fn is_video(&self) -> bool {
        self.content_type_is("video/")
    }

    pub fn is_audio(&self) -> bool {
        self.content_type_is("audio/")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub inline: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EmbedMedia {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbedAuthor {
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub proxy_icon_url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbedFooter {
    pub text: String,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub proxy_icon_url: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EmbedProvider {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Embed {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub color: Option<u32>,
    #[serde(default)]
    pub fields: Vec<EmbedField>,
    #[serde(default)]
    pub image: Option<EmbedMedia>,
    #[serde(default)]
    pub thumbnail: Option<EmbedMedia>,
    #[serde(default)]
    pub author: Option<EmbedAuthor>,
    #[serde(default)]
    pub footer: Option<EmbedFooter>,
    /// `rich`, `image`, `video`, `gifv`, `article`, `link`, ...
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub video: Option<EmbedMedia>,
    #[serde(default)]
    pub provider: Option<EmbedProvider>,
    #[serde(default)]
    pub timestamp: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReactionEmoji {
    #[serde(default)]
    pub id: Option<Snowflake>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub animated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reaction {
    pub count: u32,
    #[serde(default)]
    pub me: bool,
    pub emoji: ReactionEmoji,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MessageReference {
    #[serde(default)]
    pub message_id: Option<Snowflake>,
    #[serde(default)]
    pub channel_id: Option<Snowflake>,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: Snowflake,
    pub channel_id: Snowflake,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    pub author: User,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default)]
    pub tts: bool,
    #[serde(default)]
    pub mention_everyone: bool,
    #[serde(default)]
    pub mentions: Vec<User>,
    #[serde(default)]
    pub mention_roles: Vec<Snowflake>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub embeds: Vec<Embed>,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(rename = "type", default)]
    pub kind: u8,
    #[serde(default)]
    pub message_reference: Option<MessageReference>,
    #[serde(default)]
    pub referenced_message: Option<Box<Message>>,
    #[serde(default)]
    pub sticker_items: Vec<StickerItem>,
    #[serde(default)]
    pub flags: u64,
    #[serde(default)]
    pub webhook_id: Option<Snowflake>,
    /// Echo of the nonce the client sent, used to match a pending message.
    #[serde(default)]
    pub nonce: Option<serde_json::Value>,
    /// Forwarded messages carry their content here.
    #[serde(default)]
    pub message_snapshots: Vec<MessageSnapshot>,
}

/// A sticker reference on a message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StickerItem {
    pub id: Snowflake,
    #[serde(default)]
    pub name: String,
    /// 1 PNG, 2 APNG, 3 Lottie, 4 GIF.
    #[serde(default)]
    pub format_type: u8,
}

/// The copied content of a forwarded message.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SnapshotMessage {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub embeds: Vec<Embed>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MessageSnapshot {
    #[serde(default)]
    pub message: SnapshotMessage,
}

impl Message {
    /// Discord message type 19 is a reply.
    pub const REPLY: u8 = 19;
    pub const CHAT_INPUT_COMMAND: u8 = 20;
    pub const CONTEXT_MENU_COMMAND: u8 = 23;
    pub const FLAG_SUPPRESS_EMBEDS: u64 = 1 << 2;
    pub const DEFAULT: u8 = 0;

    pub fn is_reply(&self) -> bool {
        self.kind == Self::REPLY || self.message_reference.is_some()
    }

    pub fn is_system(&self) -> bool {
        !matches!(
            self.kind,
            Self::DEFAULT | Self::REPLY | Self::CHAT_INPUT_COMMAND | Self::CONTEXT_MENU_COMMAND
        )
    }
}
