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

    /// An animated-image attachment (`image/gif`): shown as a looping GIF
    /// rather than a still.
    pub fn is_gif(&self) -> bool {
        self.content_type_is("image/gif") || self.filename.to_lowercase().ends_with(".gif")
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
    /// The media type, e.g. `image/gif` (sent for image embeds).
    #[serde(default)]
    pub content_type: Option<String>,
    /// Discord's embed media flags; [`EmbedMedia::FLAG_ANIMATED`] marks an
    /// animated image.
    #[serde(default)]
    pub flags: u64,
}

impl EmbedMedia {
    /// `IS_ANIMATED`: the image is animated (a GIF, or an animated WebP).
    pub const FLAG_ANIMATED: u64 = 1 << 5;

    /// Whether this is an animated GIF, judged by its content type, the
    /// animated flag, or a `.gif` path on its original URL.
    pub fn is_animated_gif(&self) -> bool {
        let gif_path = self.url.as_deref().is_some_and(|url| {
            url.split(['?', '#'])
                .next()
                .is_some_and(|path| path.to_ascii_lowercase().ends_with(".gif"))
        });
        self.content_type.as_deref() == Some("image/gif")
            || gif_path
            || (self.flags & Self::FLAG_ANIMATED != 0
                && self.content_type.as_deref().is_none_or(|kind| kind == "image/gif"))
    }
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
    /// Bot buttons, select menus and (with [`Message::FLAG_COMPONENTS_V2`])
    /// layout, as Discord sends them; the interface reads them as it draws.
    #[serde(default)]
    pub components: Vec<serde_json::Value>,
    /// The app that sent this in answer to an interaction.
    #[serde(default)]
    pub application_id: Option<Snowflake>,
    /// The command (or button) this message answers, and who used it.
    #[serde(default)]
    pub interaction_metadata: Option<MessageInteraction>,
    /// The older form of `interaction_metadata`, which names the command.
    #[serde(default)]
    pub interaction: Option<MessageInteraction>,
    /// For a call message: who joined, and when it ended.
    #[serde(default)]
    pub call: Option<MessageCall>,
    /// Deleted on Discord, and shown from the message log.
    #[serde(skip)]
    pub deleted: bool,
    /// Local send progress for a message shown before Discord confirms it.
    /// Never sent by Discord; always [`SendState::Sent`] for its messages.
    #[serde(skip)]
    pub send_state: SendState,
}

/// A DM call a message records.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MessageCall {
    #[serde(default)]
    pub participants: Vec<Snowflake>,
    /// Unset while the call is still going.
    #[serde(default)]
    pub ended_timestamp: Option<String>,
}

/// What a bot's message answers: an interaction, and who started it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MessageInteraction {
    #[serde(default)]
    pub id: Option<Snowflake>,
    /// 2 = a command, 3 = a button or menu, 5 = a submitted form.
    #[serde(rename = "type", default)]
    pub kind: u8,
    /// The command as typed, subcommands included (`"music play"`).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub user: Option<crate::User>,
}

/// Where a message the user just sent is in its journey to Discord.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SendState {
    /// Confirmed by Discord (every message it sends us).
    #[default]
    Sent,
    /// Shown optimistically; the send request is in flight.
    Sending,
    /// The send request failed; the user can retry or discard it.
    Failed,
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
    /// A message shown optimistically, before Discord confirms it. `nonce`
    /// is echoed back by the gateway, matching this to its confirmation.
    pub fn pending(
        id: Snowflake,
        channel_id: Snowflake,
        author: User,
        content: String,
        nonce: String,
    ) -> Self {
        Message {
            id,
            channel_id,
            guild_id: None,
            author,
            content,
            timestamp: crate::time::now_iso(),
            edited_timestamp: None,
            tts: false,
            mention_everyone: false,
            mentions: Vec::new(),
            mention_roles: Vec::new(),
            attachments: Vec::new(),
            embeds: Vec::new(),
            reactions: Vec::new(),
            pinned: false,
            kind: Self::DEFAULT,
            message_reference: None,
            referenced_message: None,
            sticker_items: Vec::new(),
            flags: 0,
            webhook_id: None,
            nonce: Some(serde_json::Value::String(nonce)),
            message_snapshots: Vec::new(),
            components: Vec::new(),
            application_id: None,
            interaction_metadata: None,
            interaction: None,
            call: None,
            deleted: false,
            send_state: SendState::Sending,
        }
    }

    /// Discord message type 19 is a reply.
    pub const REPLY: u8 = 19;
    pub const CHAT_INPUT_COMMAND: u8 = 20;
    pub const CONTEXT_MENU_COMMAND: u8 = 23;
    pub const FLAG_SUPPRESS_EMBEDS: u64 = 1 << 2;
    /// Only the user who used the command sees it.
    pub const FLAG_EPHEMERAL: u64 = 1 << 6;
    /// The app is still working on its answer ("is thinking…").
    pub const FLAG_LOADING: u64 = 1 << 7;
    /// Laid out with components alone (text, sections, containers...).
    pub const FLAG_COMPONENTS_V2: u64 = 1 << 15;

    /// The command this message answers (`/music play`), and who used it.
    pub fn command_used(&self) -> Option<(String, Option<&crate::User>)> {
        let interaction = self.interaction.as_ref().or(self.interaction_metadata.as_ref())?;
        let name = self
            .interaction
            .as_ref()
            .and_then(|old| old.name.clone())
            .or_else(|| interaction.name.clone())?;
        let user = self
            .interaction_metadata
            .as_ref()
            .and_then(|meta| meta.user.as_ref())
            .or(interaction.user.as_ref());
        Some((name, user))
    }
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

#[cfg(test)]
mod tests {
    use super::EmbedMedia;

    fn media(url: &str, content_type: Option<&str>, flags: u64) -> EmbedMedia {
        EmbedMedia {
            url: Some(url.to_owned()),
            content_type: content_type.map(str::to_owned),
            flags,
            ..EmbedMedia::default()
        }
    }

    #[test]
    fn linked_gifs_are_recognised_as_animated() {
        // By path (signed CDN query ignored), content type, or animated flag.
        assert!(media("https://cdn.discordapp.com/a/b/x.GIF?ex=1&hm=2", None, 0).is_animated_gif());
        assert!(media("https://example.com/img", Some("image/gif"), 0).is_animated_gif());
        assert!(media("https://example.com/img", None, EmbedMedia::FLAG_ANIMATED).is_animated_gif());
        // A still, or an animated WebP, is not a GIF.
        assert!(!media("https://example.com/a.png", Some("image/png"), 0).is_animated_gif());
        assert!(
            !media("https://example.com/a.webp", Some("image/webp"), EmbedMedia::FLAG_ANIMATED)
                .is_animated_gif()
        );
    }
}
