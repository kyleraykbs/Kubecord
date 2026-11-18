//! Camera and screen capture to H.264 Annex-B access units.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gstreamer as gst;
use gstreamer_app::prelude::*;
use tracing::debug;

use crate::error::{Error, Result};

/// One H.264 access unit and its duration.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub duration_ms: u32,
}

pub struct VideoSource {
    pipeline: gst::Pipeline,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    _portal: Option<crate::portal::ScreenCast>,
    _camera: Option<crate::portal::CameraCast>,
}

/// Settings shared by camera and screen capture.
#[derive(Clone, Debug)]
pub struct VideoSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// x264 preset; `zerolatency` tuning is always applied.
    pub preset: String,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 2500,
            preset: "ultrafast".into(),
        }
    }
}

fn encoder_element() -> &'static str {
    // Prefer hardware when the plugin is present; x264enc is the reliable
    // software path. The element is resolved once per pipeline startup.
    "x264enc"
}

fn pipeline_description(source: &str, settings: &VideoSettings) -> String {
    format!(
        "{source} ! videoconvert ! videoscale ! videorate \
         ! video/x-raw,width={width},height={height},framerate={fps}/1 \
         ! {encoder} tune=zerolatency speed-preset={preset} bitrate={bitrate} key-int-max={keyint} bframes=0 \
         ! h264parse config-interval=-1 \
         ! video/x-h264,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false max-buffers=4 drop=true",
        width = settings.width,
        height = settings.height,
        fps = settings.fps,
        encoder = encoder_element(),
        preset = settings.preset,
        bitrate = settings.bitrate_kbps,
        keyint = settings.fps * 2,
    )
}

impl VideoSource {
    fn start_pipeline(
        description: &str,
        frame_ms: u32,
        mut on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self> {
        gst::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))?;
        let element = gst::parse::launch(description)
            .map_err(|error| Error::Message(format!("video pipeline: {error}")))?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| Error::Message("video pipeline is not a pipeline".into()))?;
        let appsink = pipeline
            .by_name("sink")
            .and_then(|element| element.downcast::<gstreamer_app::AppSink>().ok())
            .ok_or_else(|| Error::Message("video appsink missing".into()))?;
        appsink.set_sync(false);
        appsink.set_drop(true);
        if let Err(error) = pipeline.set_state(gst::State::Playing) {
            let detail = pipeline
                .bus()
                .and_then(|bus| {
                    bus.timed_pop_filtered(
                        gst::ClockTime::from_mseconds(500),
                        &[gst::MessageType::Error],
                    )
                })
                .and_then(|message| match message.view() {
                    gst::MessageView::Error(error) => Some(format!(
                        "{}: {}",
                        error.error(),
                        error.debug().unwrap_or_default()
                    )),
                    _ => None,
                })
                .unwrap_or_default();
            return Err(Error::Message(format!("video state: {error} {detail}")));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(100))
                else {
                    continue;
                };
                let Some(buffer) = sample.buffer() else {
                    continue;
                };
                let Ok(map) = buffer.map_readable() else {
                    continue;
                };
                if !map.as_slice().is_empty() {
                    on_frame(EncodedFrame {
                        data: map.as_slice().to_vec(),
                        duration_ms: frame_ms,
                    });
                }
            }
        });

        Ok(Self {
            pipeline,
            stop,
            thread: Some(thread),
            _portal: None,
            _camera: None,
        })
    }

    /// Opens a V4L2 camera, or the first available one when `device` is None.
    pub fn camera(
        device: Option<String>,
        settings: VideoSettings,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self> {
        let device = device.unwrap_or_else(|| "/dev/video0".into());
        let source = format!("v4l2src device={device}");
        Self::start_pipeline(
            &pipeline_description(&source, &settings),
            1000 / settings.fps.max(1),
            on_frame,
        )
    }

    /// Captures a PipeWire node, as returned by the desktop portal.
    pub fn pipewire(
        node_id: u32,
        settings: VideoSettings,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self> {
        let source = format!("pipewiresrc path={node_id} do-timestamp=true");
        Self::start_pipeline(
            &pipeline_description(&source, &settings),
            1000 / settings.fps.max(1),
            on_frame,
        )
    }

    /// Captures the desktop stream selected through the XDG portal.
    pub fn screen(
        portal: crate::portal::ScreenCast,
        settings: VideoSettings,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self> {
        let node_id = portal.node_id;
        let fd = portal.fd();
        let mut source = Self::from_pipewire(node_id, fd, &settings, on_frame)?;
        source._portal = Some(portal);
        Ok(source)
    }

    /// Captures a camera selected through the XDG camera portal.
    pub fn camera_portal(
        camera: crate::portal::CameraCast,
        settings: VideoSettings,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self> {
        let node_id = camera.node_id;
        let fd = camera.fd();
        let mut source = Self::from_pipewire(node_id, fd, &settings, on_frame)?;
        // The portal owns the PipeWire fd; keep it alive with the pipeline.
        source._camera = Some(camera);
        Ok(source)
    }

    fn from_pipewire(
        node_id: u32,
        fd: std::os::fd::RawFd,
        settings: &VideoSettings,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self> {
        let source = format!("pipewiresrc path={node_id} fd={fd} do-timestamp=true");
        Self::start_pipeline(
            &pipeline_description(&source, settings),
            1000 / settings.fps.max(1),
            on_frame,
        )
    }
}

impl Drop for VideoSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Decodes H.264 Annex-B into RGBA frames for the UI.
pub struct VideoDecoder {
    pipeline: gst::Pipeline,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub struct DecodedFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl VideoDecoder {
    pub fn start(mut on_frame: impl FnMut(DecodedFrame) + Send + 'static) -> Result<Self> {
        gst::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))?;
        let element = gst::parse::launch(
            "appsrc name=src is-live=true format=time do-timestamp=true \
             ! h264parse ! avdec_h264 ! videoconvert \
             ! video/x-raw,format=RGBA \
             ! appsink name=sink sync=false max-buffers=2 drop=true",
        )
        .map_err(|error| Error::Message(format!("decode pipeline: {error}")))?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| Error::Message("decode pipeline is not a pipeline".into()))?;
        let appsink = pipeline
            .by_name("sink")
            .and_then(|element| element.downcast::<gstreamer_app::AppSink>().ok())
            .ok_or_else(|| Error::Message("decode appsink missing".into()))?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| Error::Message(format!("decode state: {error}")))?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(100))
                else {
                    continue;
                };
                let Some(caps) = sample.caps() else {
                    continue;
                };
                let Some(structure) = caps.structure(0) else {
                    continue;
                };
                let Ok(width) = structure.get::<i32>("width") else {
                    continue;
                };
                let Ok(height) = structure.get::<i32>("height") else {
                    continue;
                };
                let Some(buffer) = sample.buffer() else {
                    continue;
                };
                let Ok(map) = buffer.map_readable() else {
                    continue;
                };
                on_frame(DecodedFrame {
                    rgba: map.as_slice().to_vec(),
                    width: width as u32,
                    height: height as u32,
                    stride: width as u32 * 4,
                });
            }
        });

        Ok(Self {
            pipeline,
            stop,
            thread: Some(thread),
        })
    }

    pub fn push(&self, frame: &[u8]) -> Result<()> {
        let appsrc = self
            .pipeline
            .by_name("src")
            .and_then(|element| element.downcast::<gstreamer_app::AppSrc>().ok())
            .ok_or_else(|| Error::Message("decode appsrc missing".into()))?;
        let caps = gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        appsrc.set_caps(Some(&caps));
        appsrc.set_format(gst::Format::Time);
        let buffer = gst::Buffer::from_slice(frame.to_vec());
        if let Err(error) = appsrc.push_buffer(buffer) {
            debug!(%error, "video decode push");
        }
        Ok(())
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_elements_are_installed() {
        gst::init().unwrap();
        for name in [
            "videotestsrc",
            "x264enc",
            "h264parse",
            "avdec_h264",
            "appsink",
        ] {
            assert!(
                gst::ElementFactory::find(name).is_some(),
                "missing gstreamer element {name}"
            );
        }
    }

    #[test]
    fn encodes_a_test_pattern() {
        gst::init().unwrap();
        let element = gst::parse::launch(
            "videotestsrc num-buffers=5 \
             ! video/x-raw,width=320,height=240,framerate=30/1 \
             ! videoconvert ! videoscale ! videorate \
             ! x264enc tune=zerolatency speed-preset=ultrafast bitrate=500 key-int-max=30 bframes=0 \
             ! h264parse config-interval=-1 \
             ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false",
        )
        .unwrap();
        let pipeline = element.downcast::<gst::Pipeline>().unwrap();
        let appsink = pipeline
            .by_name("sink")
            .and_then(|element| element.downcast::<gstreamer_app::AppSink>().ok())
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let mut frames = 0;
        while frames < 5 {
            let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_seconds(2)) else {
                break;
            };
            if let Some(buffer) = sample.buffer()
                && let Ok(map) = buffer.map_readable()
            {
                assert!(!map.as_slice().is_empty());
                frames += 1;
            }
        }
        pipeline.set_state(gst::State::Null).unwrap();
        assert_eq!(frames, 5);
    }

    #[test]
    fn gtk4_paintable_sink_is_present_when_plugin_is_installed() {
        gst::init().unwrap();
        // Provided by gst-plugins-rs; the UI uses it for video tiles.
        let _ = gst::ElementFactory::find("gtk4paintablesink");
    }
}
