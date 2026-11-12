//! Discord REST v9 client.

pub mod client;
pub mod error;
mod rate_limit;

pub use client::HttpClient;
pub use error::{ApiError, Error, Result};

pub const API_BASE: &str = "https://discord.com/api/v9";
