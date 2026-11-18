//! GStreamer capture, codecs and rendering for kylecord.

pub mod audio;
pub mod decode;
pub mod delay;
pub mod devices;
pub mod error;
pub mod portal;
pub mod receive;
pub mod video;

pub use error::{Error, Result};

/// Initialises GStreamer once for the process.
pub fn init() -> Result<()> {
    gstreamer::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))
}
