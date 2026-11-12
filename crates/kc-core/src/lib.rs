//! Shared types, configuration and credentials for kylecord.

pub mod account;
pub mod error;
pub mod id;
pub mod paths;
pub mod super_properties;
pub mod token;

pub use account::{Account, AccountStore};
pub use error::{Error, Result};
pub use id::Snowflake;
pub use token::Token;