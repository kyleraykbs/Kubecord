//! GStreamer capture, codecs and rendering for kylecord.

pub mod audio;
pub mod decode;
pub mod delay;
pub mod devices;
pub mod error;
pub mod h264;
pub mod mjpeg;
pub mod portal;
pub mod processing;
pub mod receive;
pub mod share_audio;
pub mod sound;
pub mod video;

pub use error::{Error, Result};

/// Initialises GStreamer once for the process.
pub fn init() -> Result<()> {
    gstreamer::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))
}
