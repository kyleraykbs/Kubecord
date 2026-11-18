//! Discord voice transport: Voice gateway, UDP, SRTP, RTP and DAVE.

pub mod connection;
pub mod dave;
pub mod error;
pub mod rtcp;
pub mod rtp;
pub mod srtp;
pub mod video_rx;

pub use connection::{VoiceEvent, VoiceHandle, VoiceParams, connect};
pub use error::{Error, Result};
