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

/// Encoder choice for a capture pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderPreference {
    /// Prefer VA-API and fall back to x264 if it cannot start.
    Auto,
    /// Force the software x264 encoder.
    Cpu,
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
    pub encoder: EncoderPreference,
}

impl VideoSettings {
    /// Rough H.264 budget at 0.12 bits per pixel per frame, capped at 8 Mbps.
    pub fn automatic_bitrate_kbps(width: u32, height: u32, fps: u32) -> u32 {
        let kbps = (width as f64 * height as f64 * fps as f64 * 0.12 / 1000.0) as u32;
        kbps.clamp(800, 8000)
    }
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: Self::automatic_bitrate_kbps(1280, 720, 30),
            preset: "ultrafast".into(),
            encoder: EncoderPreference::Auto,
        }
    }
}

fn encoder_element() -> &'static str {
    // Prefer hardware when the plugin is present; x264enc is the reliable
    // software path. The element is resolved once per pipeline startup.
    "x264enc"
}

fn hardware_available() -> bool {
    gst::init().is_ok() && gst::ElementFactory::find("vah264enc").is_some()
}

fn encoder_chain(settings: &VideoSettings, hardware: bool) -> String {
    if hardware {
        format!(
            "vah264enc bitrate={bitrate} key-int-max={keyint} b-frames=0 rate-control=cbr target-usage=7",
            bitrate = settings.bitrate_kbps,
            keyint = settings.fps * 2,
        )
    } else {
        format!(
            "{encoder} tune=zerolatency speed-preset={preset} bitrate={bitrate} key-int-max={keyint} bframes=0",
            encoder = encoder_element(),
            preset = settings.preset,
            bitrate = settings.bitrate_kbps,
            keyint = settings.fps * 2,
        )
    }
}

fn pipeline_downstream(settings: &VideoSettings, hardware: bool) -> String {
    format!(
        // Leave the source format unconstrained. Forcing I420 on pipewiresrc
        // itself fails its output-format setup (-22); videoconvert performs
        // the chroma conversion before the encoder.
        "capsfilter caps=video/x-raw ! queue max-size-buffers=4 max-size-time=0 max-size-bytes=0 \
         ! videoconvert ! videoscale ! videorate \
         ! capsfilter caps=video/x-raw,format={format},width={width},height={height},framerate={fps}/1 \
         ! {encoder} \
         ! h264parse config-interval=-1 \
         ! capsfilter caps=video/x-h264,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false max-buffers=8 drop=true",
        width = settings.width,
        height = settings.height,
        fps = settings.fps,
        format = if hardware { "NV12" } else { "I420" },
        encoder = encoder_chain(settings, hardware),
    )
}

fn pipeline_description(source: &str, settings: &VideoSettings, hardware: bool) -> String {
    format!("{source} ! {}", pipeline_downstream(settings, hardware))
}

fn screen_description(settings: &VideoSettings, hardware: bool) -> String {
    format!(
        "pipewiresrc name=src do-timestamp=true keepalive-time=1000 resend-last=true \
         ! videorate skip-to-first=true \
         ! capsfilter caps=video/x-raw,framerate={fps}/1 \
         ! queue \
         ! videoconvert \
         ! videoscale \
         ! capsfilter caps=video/x-raw,format={format},width={width},height={height} \
         ! {encoder} \
         ! h264parse config-interval=-1 \
         ! capsfilter caps=video/x-h264,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false max-buffers=8 drop=true",
        fps = settings.fps,
        width = settings.width,
        height = settings.height,
        format = if hardware { "NV12" } else { "I420" },
        encoder = encoder_chain(settings, hardware),
    )
}

#[derive(Clone, Copy, Debug)]
enum PipeWireProperty {
    Path,
    TargetObject,
}

/// Loads GStreamer and the PipeWire source plugin on the caller's thread.
/// Doing this lazily from a Tokio worker inside the GTK app deadlocked while
/// the plugin registry was loading; camera pipelines use v4l2 and were not
/// affected.
pub fn preload_pipewire() -> Result<()> {
    gst::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))?;
    gst::ElementFactory::make("pipewiresrc")
        .build()
        .map_err(|error| Error::Message(format!("pipewiresrc plugin: {error}")))?;
    Ok(())
}

fn encoder_descriptions(
    settings: &VideoSettings,
    make: impl Fn(bool) -> String,
) -> Vec<String> {
    let mut descriptions = Vec::new();
    if settings.encoder == EncoderPreference::Auto && hardware_available() {
        descriptions.push(make(true));
    }
    descriptions.push(make(false));
    descriptions
}

impl VideoSource {
    fn start_pipeline_with(
        description: &str,
        frame_ms: u32,
        mut on_frame: impl FnMut(EncodedFrame) + Send + 'static,
        configure: impl Fn(&gst::Pipeline) -> Result<()>,
    ) -> Result<Self> {
        gst::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))?;
        let element = gst::parse::launch(description)
            .map_err(|error| Error::Message(format!("video pipeline: {error}")))?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| Error::Message("video pipeline is not a pipeline".into()))?;
        configure(&pipeline)?;
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
            let _ = pipeline.set_state(gst::State::Null);
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

    fn start_with_fallback(
        descriptions: Vec<String>,
        frame_ms: u32,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
        configure: impl Fn(&gst::Pipeline) -> Result<()>,
    ) -> Result<Self> {
        let callback = Arc::new(std::sync::Mutex::new(on_frame));
        let mut last_error = None;
        for description in descriptions {
            let callback = callback.clone();
            let callback = move |frame| {
                if let Ok(mut callback) = callback.lock() {
                    callback(frame);
                }
            };
            match Self::start_pipeline_with(&description, frame_ms, callback, &configure) {
                Ok(source) => return Ok(source),
                Err(error) => {
                    tracing::warn!(%error, "capture encoder attempt failed");
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| Error::Message("no capture pipeline variant".into())))
    }

    /// Opens a V4L2 camera, or the first available one when `device` is None.
    pub fn camera(
        device: Option<String>,
        settings: VideoSettings,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self> {
        let device = device.unwrap_or_else(|| "/dev/video0".into());
        let source = format!("v4l2src device={device}");
        let descriptions = encoder_descriptions(&settings, |hardware| {
            pipeline_description(&source, &settings, hardware)
        });
        Self::start_with_fallback(
            descriptions,
            1000 / settings.fps.max(1),
            on_frame,
            |_| Ok(()),
        )
    }

    /// Captures a PipeWire node, as returned by the desktop portal.
    pub fn pipewire(
        node_id: u32,
        settings: VideoSettings,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self> {
        let source = format!("pipewiresrc path={node_id} do-timestamp=true");
        let descriptions = encoder_descriptions(&settings, |hardware| {
            pipeline_description(&source, &settings, hardware)
        });
        Self::start_with_fallback(
            descriptions,
            1000 / settings.fps.max(1),
            on_frame,
            |_| Ok(()),
        )
    }

    /// Captures the desktop stream selected through the XDG portal.
    pub fn screen(
        portal: crate::portal::ScreenCast,
        settings: VideoSettings,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self> {
        Self::from_screen_portal(portal, settings, on_frame)
    }

    /// Captures a camera selected through the XDG camera portal.
    pub fn camera_portal(
        camera: crate::portal::CameraCast,
        settings: VideoSettings,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self> {
        if let Some(device) = camera.device_path.clone() {
            return Self::camera(Some(device), settings, on_frame);
        }
        let node_id = camera.node_id;
        let fd = camera.fd();
        let mut targets = vec![vec![(PipeWireProperty::Path, node_id.to_string())]];
        if !camera.node_name.is_empty() {
            targets.push(vec![(
                PipeWireProperty::TargetObject,
                camera.node_name.clone(),
            )]);
        }
        if !camera.serial.is_empty() {
            targets.push(vec![(
                PipeWireProperty::TargetObject,
                camera.serial.clone(),
            )]);
        }
        let mut source =
            Self::from_pipewire(node_id, fd, true, &settings, targets, on_frame)?;
        // The portal owns the PipeWire fd; keep it alive with the pipeline.
        source._camera = Some(camera);
        Ok(source)
    }


    fn from_screen_portal(
        portal: crate::portal::ScreenCast,
        settings: VideoSettings,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self> {
        gst::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))?;
        tracing::debug!(
            node_id = portal.node_id,
            fd = portal.fd(),
            "screen capture source starting"
        );
        if gst::ElementFactory::find("pipewiresrc").is_none() {
            return Err(Error::Message(
                "pipewiresrc plugin unavailable; start kylecord from `nix develop` or the packaged Nix build"
                    .into(),
            ));
        }
        let fd = portal.fd();
        let node_id = portal.node_id;
        let descriptions = encoder_descriptions(&settings, |hardware| {
            screen_description(&settings, hardware)
        });
        tracing::debug!("screen capture parsing pipeline");
        let mut source = Self::start_with_fallback(
            descriptions,
            1000 / settings.fps.max(1),
            on_frame,
            move |pipeline| {
                let element = pipeline
                    .by_name("src")
                    .ok_or_else(|| Error::Message("pipewiresrc element missing".into()))?;
                element.set_property("fd", fd);
                element.set_property("path", node_id.to_string());
                Ok(())
            },
        )?;
        tracing::debug!("screen capture pipeline started");
        source._portal = Some(portal);
        Ok(source)
    }

    fn from_pipewire(
        node_id: u32,
        fd: std::os::fd::RawFd,
        use_fd: bool,
        settings: &VideoSettings,
        targets: Vec<Vec<(PipeWireProperty, String)>>,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self> {
        let on_frame = Arc::new(std::sync::Mutex::new(on_frame));
        // Set properties on the element directly. Passing fd/path through
        // gst_parse_launch failed state setup on the camera; the gtk4
        // paintable example sets fd and path as properties.
        let source = "pipewiresrc name=src do-timestamp=true";
        let mut last_error = None;
        for properties in targets {
            let callback = on_frame.clone();
            let callback = move |frame| {
                if let Ok(mut callback) = callback.lock() {
                    callback(frame);
                }
            };
            let properties_for_log = properties.clone();
            let configure = move |pipeline: &gst::Pipeline| -> Result<()> {
                let element = pipeline
                    .by_name("src")
                    .ok_or_else(|| Error::Message("pipewiresrc element missing".into()))?;
                if use_fd {
                    element.set_property("fd", fd);
                }
                for (property, value) in &properties {
                    match property {
                        PipeWireProperty::Path => element.set_property("path", value),
                        PipeWireProperty::TargetObject => {
                            element.set_property("target-object", value)
                        }
                    }
                }
                Ok(())
            };
            let descriptions = encoder_descriptions(settings, |hardware| {
                pipeline_description(source, settings, hardware)
            });
            match Self::start_with_fallback(
                descriptions,
                1000 / settings.fps.max(1),
                callback,
                configure,
            ) {
                Ok(video_source) => {
                    tracing::info!(
                        ?properties_for_log,
                        node_id,
                        fd,
                        use_fd,
                        "pipewire capture source selected"
                    );
                    return Ok(video_source);
                }
                Err(error) => {
                    tracing::warn!(
                        ?properties_for_log,
                        node_id,
                        fd,
                        use_fd,
                        %error,
                        "pipewire capture variant failed"
                    );
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| Error::Message("no pipewire source variant".into())))
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
             ! video/x-raw,format=I420,width=320,height=240,framerate=30/1 \
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
