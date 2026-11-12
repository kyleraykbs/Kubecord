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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbedFooter {
    pub text: String,
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
}

impl Message {
    /// Discord message type 19 is a reply.
    pub const REPLY: u8 = 19;
    pub const DEFAULT: u8 = 0;

    pub fn is_reply(&self) -> bool {
        self.kind == Self::REPLY || self.message_reference.is_some()
    }

    pub fn is_system(&self) -> bool {
        !matches!(self.kind, Self::DEFAULT | Self::REPLY)
    }
}