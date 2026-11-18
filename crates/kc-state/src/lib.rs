//! Discord model types and the event reducer.
//!
//! The gateway emits raw dispatch payloads; this crate turns them into the
//! subset of Discord's object graph the client actually renders.

pub mod channel;
pub mod guild;
pub mod member_list;
pub mod message;
pub mod permissions;
mod queries;
mod serde_util;
pub mod settings;
pub mod state;
pub mod time;
pub mod user;

pub use channel::{Channel, ChannelType, Overwrite};
pub use guild::{Emoji, Guild, Member, Presence, Relationship, Role, VoiceState};
pub use member_list::{MemberList, MemberListItem};
pub use message::{
    Attachment, Embed, EmbedField, EmbedMedia, Message, MessageReference, Reaction, ReactionEmoji,
    StickerItem,
};
pub use queries::ChannelSection;
pub use settings::{CustomStatus, GuildEntry, GuildFolder, GuildSettings, ReadState};
pub use state::{MessageBuffer, State, Store, StoreEvent};
pub use user::User;
