//! Discord REST v9 client.

pub mod cdn;
pub mod client;
pub mod error;
mod rate_limit;

pub use client::{Gif, GifCategory, HttpClient, Outgoing, SearchPage};
pub use error::{ApiError, Error, Result};

pub const API_BASE: &str = "https://discord.com/api/v9";
