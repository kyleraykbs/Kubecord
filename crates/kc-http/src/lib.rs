//! Discord REST v9 client.

pub mod auth;
pub mod cdn;
pub mod client;
pub mod error;
mod rate_limit;

pub use client::{
    Ban, CaptchaAnswer, ChannelEdit, Gif, GifCategory, HttpClient, Invite, InviteChannel,
    MemberEdit, Outgoing, RoleEdit, SearchPage, image_data_uri,
};
pub use error::{ApiError, Error, Result};

pub const API_BASE: &str = "https://discord.com/api/v9";
