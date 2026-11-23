//! Discord model types and the event reducer.
//!
//! The gateway emits raw dispatch payloads; this crate turns them into the
//! subset of Discord's object graph the client actually renders.

pub mod channel;
pub mod guild;
pub mod member_list;
pub mod message;
pub mod moderation;
pub mod notify;
pub mod permissions;
mod queries;
mod serde_util;
pub mod settings;
pub mod settings_proto;
pub mod state;
pub mod time;
pub mod user;

pub use channel::{Channel, ChannelType, Overwrite};
pub use guild::{
    Emoji, Guild, Member, Presence, Relationship, Role, StatusEmoji, StatusLine, VoiceState,
};
pub use member_list::{MemberList, MemberListItem};
pub use message::{
    Attachment, Embed, EmbedField, EmbedMedia, Message, MessageEdit, MessageReference, Poll, PollAnswer, PollCount, PollMedia, PollResults, Reaction, ReactionEmoji,
    SendState, StickerItem,
};
pub use queries::ChannelSection;
pub use settings::{CustomStatus, GuildEntry, GuildFolder, GuildSettings, ReadState};
pub use state::{MessageBuffer, State, Store, StoreEvent, set_unload_distant};
pub use user::User;
