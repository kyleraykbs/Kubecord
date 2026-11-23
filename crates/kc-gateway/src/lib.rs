//! Discord user gateway: one WebSocket, heartbeats, identify, resume.

pub mod compress;
pub mod session;

pub use session::{Gateway, GatewayEvent, Outcome};

/// Gateway protocol version. Discord still serves v9 to user clients.
pub const GATEWAY_VERSION: u8 = 9;
pub const DEFAULT_GATEWAY: &str = "wss://gateway.discord.gg";
