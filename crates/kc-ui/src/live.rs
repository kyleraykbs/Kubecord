//! Live video from a call: a decoder drawing into something GTK can show.

use std::sync::Arc;

use gstreamer as gst;
use gstreamer::prelude::*;
use gtk::gdk;

use kc_media::decode::{VideoDecoder, VideoFeed};

/// One person's camera or stream being decoded for display.
pub(crate) struct LiveVideo {
    pub feed: Arc<VideoFeed>,
    pub paintable: gdk::Paintable,
    decoder: VideoDecoder,
}

impl LiveVideo {
    /// Starts decoding `feed`. Must run on the GTK thread: the picture the
    /// decoder draws into belongs to it.
    pub(crate) fn start(feed: Arc<VideoFeed>) -> Option<Self> {
        if let Err(error) = gst::init() {
            tracing::warn!(%error, "gstreamer unavailable for live video");
            return None;
        }
        let picture = gst::ElementFactory::make("gtk4paintablesink")
            // Show each frame when it arrives rather than on a clock.
            .property("sync", false)
            .build()
            .ok()?;
        let paintable = picture.property::<gdk::Paintable>("paintable");
        // With OpenGL the colour conversion runs on the GPU; without it, a
        // software converter sits in front of the picture.
        let has_gl = paintable.find_property("gl-context").is_some()
            && paintable
                .property::<Option<gdk::GLContext>>("gl-context")
                .is_some();
        tracing::debug!(has_gl, "starting live video");
        let sink = if has_gl {
            gst::ElementFactory::make("glsinkbin")
                .property("sink", &picture)
                .build()
                .ok()?
        } else {
            let convert = gst::ElementFactory::make("videoconvert").build().ok()?;
            let bin = gst::Bin::new();
            bin.add_many([&convert, &picture]).ok()?;
            convert.link(&picture).ok()?;
            let pad = gst::GhostPad::with_target(&convert.static_pad("sink")?).ok()?;
            bin.add_pad(&pad).ok()?;
            bin.upcast()
        };
        match VideoDecoder::start(feed.clone(), &sink) {
            Ok(decoder) => Some(Self {
                feed,
                paintable,
                decoder,
            }),
            Err(error) => {
                tracing::warn!(%error, "could not start live video decoder");
                None
            }
        }
    }

    /// Frames received and frames decoded so far.
    pub(crate) fn progress(&self) -> (u64, u64) {
        (self.feed.pushed(), self.decoder.decoded())
    }
}
