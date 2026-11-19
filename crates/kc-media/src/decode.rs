//! Decoding received H.264 video for display.
//!
//! Frames arrive on the network thread before anything exists to show
//! them: the picture a decoder draws into belongs to the interface, which
//! builds it on its own thread a moment later. A [`VideoFeed`] bridges the
//! two. The network side pushes frames into it from the first one on; the
//! interface attaches a [`VideoDecoder`] when it is ready, and the frames
//! that arrived in between (from the latest keyframe) are decoded first.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::error::{Error, Result};

/// Frames kept while no decoder is attached. More than this without a
/// keyframe and the backlog is not worth decoding.
const MAX_PENDING_FRAMES: usize = 600;

#[derive(Default)]
struct Feed {
    decoder: Option<gst_app::AppSrc>,
    /// Frames since the latest keyframe, kept until a decoder attaches.
    pending: Vec<Vec<u8>>,
}

/// Where received video frames go. Cheap to share between threads.
#[derive(Default)]
pub struct VideoFeed {
    feed: Mutex<Feed>,
    pushed: AtomicU64,
    bytes: AtomicU64,
    keyframes: AtomicU64,
    /// The value of `pushed` at the latest keyframe.
    keyframe_at: AtomicU64,
}

/// What has gone into a feed so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeedStats {
    pub frames: u64,
    pub bytes: u64,
    pub keyframes: u64,
    /// Frames since the latest keyframe.
    pub since_keyframe: u64,
}

impl VideoFeed {
    /// Takes one complete Annex-B frame. `keyframe` marks a frame that
    /// decodes without the ones before it.
    pub fn push(&self, frame: Vec<u8>, keyframe: bool) {
        let count = self.pushed.fetch_add(1, Ordering::Relaxed) + 1;
        self.bytes.fetch_add(frame.len() as u64, Ordering::Relaxed);
        if keyframe {
            self.keyframes.fetch_add(1, Ordering::Relaxed);
            self.keyframe_at.store(count, Ordering::Relaxed);
        }
        let Ok(mut feed) = self.feed.lock() else {
            return;
        };
        match &feed.decoder {
            Some(decoder) => {
                let _ = decoder.push_buffer(gst::Buffer::from_mut_slice(frame));
            }
            None => {
                if keyframe || feed.pending.len() >= MAX_PENDING_FRAMES {
                    feed.pending.clear();
                }
                // Without a keyframe to start from there is nothing to keep.
                if keyframe || !feed.pending.is_empty() {
                    feed.pending.push(frame);
                }
            }
        }
    }

    /// Frames pushed so far.
    pub fn pushed(&self) -> u64 {
        self.pushed.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> FeedStats {
        let frames = self.pushed.load(Ordering::Relaxed);
        FeedStats {
            frames,
            bytes: self.bytes.load(Ordering::Relaxed),
            keyframes: self.keyframes.load(Ordering::Relaxed),
            since_keyframe: frames.saturating_sub(self.keyframe_at.load(Ordering::Relaxed)),
        }
    }

    fn attach(&self, decoder: gst_app::AppSrc) {
        let Ok(mut feed) = self.feed.lock() else {
            return;
        };
        for frame in feed.pending.drain(..) {
            let _ = decoder.push_buffer(gst::Buffer::from_mut_slice(frame));
        }
        feed.decoder = Some(decoder);
    }

    fn detach(&self) {
        if let Ok(mut feed) = self.feed.lock() {
            feed.decoder = None;
        }
    }
}

/// A running decoder drawing a feed's frames into a video sink.
pub struct VideoDecoder {
    pipeline: gst::Pipeline,
    feed: Arc<VideoFeed>,
    decoded: Arc<AtomicU64>,
}

impl VideoDecoder {
    /// Starts decoding `feed` into `sink`, an element that accepts raw
    /// video. Frames are shown as they arrive, not held to a clock.
    pub fn start(feed: Arc<VideoFeed>, sink: &gst::Element) -> Result<Self> {
        gst::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))?;
        let make = |factory: &str| {
            gst::ElementFactory::make(factory)
                .build()
                .map_err(|_| Error::Message(format!("missing GStreamer element {factory}")))
        };
        let caps = gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        let source = gst_app::AppSrc::builder()
            .caps(&caps)
            .format(gst::Format::Time)
            .is_live(true)
            .do_timestamp(true)
            .build();
        let parse = make("h264parse")?;
        let decode = make("avdec_h264")?;
        // Frame threading delays output by one frame per thread; slice
        // threading adds no delay.
        decode.set_property_from_str("thread-type", "slice");

        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([source.upcast_ref(), &parse, &decode, sink])
            .map_err(|error| Error::Message(format!("video decoder: {error}")))?;
        gst::Element::link_many([source.upcast_ref(), &parse, &decode, sink])
            .map_err(|error| Error::Message(format!("video decoder: {error}")))?;

        let decoded = Arc::new(AtomicU64::new(0));
        if let Some(pad) = decode.static_pad("src") {
            let decoded = decoded.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                decoded.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        }
        if let Some(bus) = pipeline.bus() {
            // Nobody polls this bus; handle messages as they are posted.
            bus.set_sync_handler(|_, message| {
                match message.view() {
                    gst::MessageView::Error(error) => {
                        tracing::warn!(error = %error.error(), debug = ?error.debug(), "video decoder error");
                    }
                    gst::MessageView::Warning(warning) => {
                        tracing::debug!(warning = %warning.error(), "video decoder warning");
                    }
                    _ => {}
                }
                gst::BusSyncReply::Drop
            });
        }

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| Error::Message(format!("video decoder start: {error}")))?;
        feed.attach(source);
        Ok(Self {
            pipeline,
            feed,
            decoded,
        })
    }

    /// Decodes without showing anything, for diagnostics.
    pub fn start_headless(feed: Arc<VideoFeed>) -> Result<Self> {
        gst::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))?;
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .map_err(|_| Error::Message("missing GStreamer element fakesink".into()))?;
        Self::start(feed, &sink)
    }

    /// Frames the decoder has produced so far.
    pub fn decoded(&self) -> u64 {
        self.decoded.load(Ordering::Relaxed)
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        self.feed.detach();
        if let Some(bus) = self.pipeline.bus() {
            bus.unset_sync_handler();
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Encodes a moving test picture into H.264 frames (Annex-B, one access
/// unit each, a keyframe every `keyframe_interval` frames). For diagnostics.
pub fn test_pattern(frames: u32, keyframe_interval: u32) -> Result<Vec<Vec<u8>>> {
    gst::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))?;
    let description = format!(
        "videotestsrc num-buffers={frames} pattern=ball ! \
         video/x-raw,width=1280,height=720,framerate=30/1 ! \
         x264enc tune=zerolatency speed-preset=veryfast bitrate=3000 key-int-max={keyframe_interval} ! \
         video/x-h264,stream-format=byte-stream,alignment=au ! appsink name=out sync=false"
    );
    let pipeline = gst::parse::launch(&description)
        .map_err(|error| Error::Message(format!("test pattern: {error}")))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| Error::Message("test pattern is not a pipeline".into()))?;
    let sink = pipeline
        .by_name("out")
        .and_then(|element| element.downcast::<gst_app::AppSink>().ok())
        .ok_or_else(|| Error::Message("test pattern sink missing".into()))?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| Error::Message(format!("test pattern start: {error}")))?;
    let mut out = Vec::new();
    while let Ok(sample) = sink.pull_sample() {
        if let Some(buffer) = sample.buffer()
            && let Ok(map) = buffer.map_readable()
        {
            out.push(map.as_slice().to_vec());
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_before_a_decoder_are_kept_from_the_latest_keyframe() {
        let feed = VideoFeed::default();
        // Nothing to start from yet.
        feed.push(vec![1], false);
        assert!(feed.feed.lock().unwrap().pending.is_empty());
        feed.push(vec![2], true);
        feed.push(vec![3], false);
        feed.push(vec![4], true);
        feed.push(vec![5], false);
        assert_eq!(feed.feed.lock().unwrap().pending, vec![vec![4], vec![5]]);
        assert_eq!(feed.pushed(), 5);
    }
}
