//! Discord REST v9 client.

pub mod auth;
pub mod browser;
pub mod cdn;
pub mod client;
pub mod commands;
pub mod error;
mod favorites;
mod rate_limit;

pub use client::{
    AvatarEdit, Ban, CaptchaAnswer, ChannelEdit, FAVORITE_GIF_FORMAT_IMAGE,
    FAVORITE_GIF_FORMAT_VIDEO, Gif, GifCategory, HttpClient, Invite, InviteChannel, MemberEdit,
    Outgoing, RoleEdit, SearchPage, ServerProfileEdit, image_data_uri,
};
pub use commands::{
    Command, CommandApp, CommandChoice, CommandIndex, CommandOption, CommandScope,
    InteractionPlace,
};
pub use error::{ApiError, Error, Result};

pub const API_BASE: &str = "https://discord.com/api/v9";
