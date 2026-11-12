//! Discord model types and the event reducer.
//!
//! The gateway emits raw dispatch payloads; this crate turns them into the
//! subset of Discord's object graph the client actually renders.

pub mod channel;
pub mod guild;
pub mod message;
pub mod state;
pub mod user;

pub use channel::{Channel, ChannelType};
pub use guild::Guild;
pub use message::{
    Attachment, Embed, EmbedField, Message, MessageReference, Reaction, ReactionEmoji,
};
pub use state::{MessageBuffer, State, Store, StoreEvent};
pub use user::User;