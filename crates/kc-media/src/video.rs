//! Camera and screen capture to H.264 Annex-B access units.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer_app::prelude::*;
use tracing::debug;

use crate::error::{Error, Result};

/// One H.264 access unit and its duration.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub duration_ms: u32,
}

/// Where a running capture sends its encoded frames. It can be pointed
/// somewhere else while the capture runs, so a preview becomes a stream
/// without restarting: reconnecting to a screen-cast node is not reliable,
/// as some portals remove the node when its first consumer leaves.
#[derive(Clone, Default)]
pub struct FrameTap(Arc<Mutex<Option<FrameSink>>>);

type FrameSink = Box<dyn FnMut(EncodedFrame) + Send>;

impl FrameTap {
    /// Sends frames to `sink` from now on.
    pub fn set(&self, sink: impl FnMut(EncodedFrame) + Send + 'static) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(Box::new(sink));
        }
    }

    /// Drops frames until the next [`FrameTap::set`].
    pub fn clear(&self) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = None;
        }
    }

    /// The callback to hand the capture.
    pub fn sink(&self) -> impl FnMut(EncodedFrame) + Send + 'static {
        let tap = self.clone();
        move |frame| {
            if let Ok(mut slot) = tap.0.lock()
                && let Some(sink) = slot.as_mut()
            {
                sink(frame);
            }
        }
    }
}

/// A capture that is already running, with the tap its frames go through.
pub struct PreparedCapture {
    pub source: VideoSource,
    pub tap: FrameTap,
}

pub struct VideoSource {
    pipeline: gst::Pipeline,
    stop: Arc<AtomicBool>,
    /// Set once the pipeline has produced an encoded frame.
    produced: Arc<AtomicBool>,
    /// The pipeline's latest error, as read off its bus by the capture
    /// thread.
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
    _portal: Option<crate::portal::ScreenCast>,
    _camera: Option<crate::portal::CameraCast>,
    keyframes: Arc<Mutex<KeyframeRequests>>,
}

/// The least time between forced keyframes. Viewers repeat their requests
/// while frozen, and a keyframe per request would flood the stream.
const KEYFRAME_SPACING: Duration = Duration::from_millis(750);

#[derive(Default)]
struct KeyframeRequests {
    last: Option<Instant>,
    /// A request is waiting for the spacing to pass.
    deferred: bool,
}

/// Sends the force-key-unit event up the pipeline to the encoder.
/// `all_headers` re-sends SPS/PPS with the IDR so a viewer that lost the
/// stream headers can decode again. A bin routes an upstream event to its
/// sinks, which forward it to the encoder; sending on the appsink's own pad
/// is the fallback.
fn send_keyframe_request(pipeline: &gst::Pipeline) {
    let build_event = || {
        gstreamer_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build()
    };
    if pipeline.send_event(build_event()) {
        tracing::info!("forced encoder keyframe");
        return;
    }
    if let Some(sink) = pipeline.by_name("sink")
        && let Some(pad) = sink.static_pad("sink")
        && pad.send_event(build_event())
    {
        tracing::info!("forced encoder keyframe (pad fallback)");
        return;
    }
    tracing::warn!("could not send force-key-unit event upstream");
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
    /// Seconds between IDR keyframes.
    /// Zero sends keyframes only when asked for (a viewer's request, or
    /// lost packets), as WebRTC senders do.
    pub keyframe_secs: u32,
    /// Maximum extra delay before a captured frame is dropped instead of sent.
    /// `0` disables dropping.
    pub drop_frames_after_ms: u32,
    /// Drop old buffers in the capture pipeline instead of building latency.
    pub leaky_queue: bool,
    /// How long sent packets stay available for retransmission after a
    /// network dropout. `0` disables retransmission.
    pub retransmit_ms: u32,
    /// x264 preset; `zerolatency` tuning is always applied.
    pub preset: String,
    pub encoder: EncoderPreference,
    /// Which of a camera's formats to capture.
    pub camera_format: CameraFormat,
}

/// The format a camera is asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CameraFormat {
    /// MJPEG at the wanted size and framerate, else raw video.
    #[default]
    Auto,
    /// Compressed in the camera: full framerate at large sizes.
    Mjpeg,
    /// Uncompressed: no decoding, but most webcams slow down above 480p.
    Raw,
}

impl VideoSettings {
    /// Rough H.264 budget at 0.12 bits per pixel per frame, capped at 8 Mbps.
    pub fn automatic_bitrate_kbps(width: u32, height: u32, fps: u32) -> u32 {
        // About 2 Mbit/s for 720p30 and 4.4 for 1080p30. Discord's server
        // has estimated 4.2 Mbit/s of room for a stream; aiming above what
        // it will carry only gets packets dropped.
        let kbps = (width as f64 * height as f64 * fps as f64 * 0.07 / 1000.0) as u32;
        kbps.clamp(800, 6000)
    }
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: Self::automatic_bitrate_kbps(1280, 720, 30),
            keyframe_secs: 0,
            drop_frames_after_ms: 250,
            leaky_queue: true,
            retransmit_ms: 2000,
            preset: "ultrafast".into(),
            encoder: EncoderPreference::Auto,
            camera_format: CameraFormat::Auto,
        }
    }
}

/// True when an Annex-B access unit contains an IDR or SPS NAL.
pub fn is_keyframe(annexb: &[u8]) -> bool {
    let mut index = 0;
    while index + 3 <= annexb.len() {
        if annexb[index] == 0 && annexb[index + 1] == 0 && annexb[index + 2] == 1 {
            if let Some(kind) = annexb.get(index + 3).map(|byte| byte & 0x1F)
                && (kind == 5 || kind == 7)
            {
                return true;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    false
}

fn encoder_element() -> &'static str {
    // Prefer hardware when the plugin is present; x264enc is the reliable
    // software path. The element is resolved once per pipeline startup.
    "x264enc"
}

/// Frames between scheduled keyframes. "Only when asked" is an hour: the
/// encoders have no setting for never.
fn keyframe_distance(settings: &VideoSettings) -> u32 {
    match settings.keyframe_secs {
        0 => settings.fps.max(1) * 3600,
        seconds => settings.fps.max(1) * seconds,
    }
}

fn hardware_available() -> bool {
    gst::init().is_ok() && gst::ElementFactory::find("vah264enc").is_some()
}

fn encoder_chain(settings: &VideoSettings, hardware: bool) -> String {
    if hardware {
        format!(
            "vah264enc bitrate={bitrate} key-int-max={keyint} b-frames=0 rate-control=cbr target-usage=7 num-slices=32",
            bitrate = settings.bitrate_kbps,
            keyint = keyframe_distance(settings),
        )
    } else {
        format!(
            // x264enc's default 600 ms VBV lets a single IDR take 0.6 s of
            // bitrate (150 KB at 2.5 Mbit/s, ~130 RTP packets). Bound any one
            // frame to 150 ms of bitrate so keyframes stay small enough to
            // pace out within a few frame intervals.
            "{encoder} tune=zerolatency speed-preset={preset} bitrate={bitrate} vbv-buf-capacity=150 key-int-max={keyint} bframes=0",
            encoder = encoder_element(),
            preset = settings.preset,
            bitrate = settings.bitrate_kbps,
            keyint = keyframe_distance(settings),
        )
    }
}

fn queue_properties(settings: &VideoSettings) -> &'static str {
    if settings.leaky_queue {
        " leaky=downstream"
    } else {
        ""
    }
}

fn pipeline_downstream(settings: &VideoSettings, hardware: bool) -> String {
    format!(
        // Leave the source format unconstrained. Forcing I420 on pipewiresrc
        // itself fails its output-format setup (-22); videoconvert performs
        // the chroma conversion before the encoder.
        "capsfilter caps=video/x-raw ! queue max-size-buffers=8 max-size-time=0 max-size-bytes=0{leaky} \
         ! videoconvert ! videoscale ! videorate skip-to-first=true \
         ! capsfilter caps=video/x-raw,format={format},width={width},height={height},framerate={fps}/1,colorimetry={colorimetry} \
         ! {encoder} \
         ! h264parse config-interval=-1 \
         ! capsfilter caps=video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false max-buffers=8 drop=true",
        width = settings.width,
        height = settings.height,
        fps = settings.fps,
        format = if hardware { "NV12" } else { "I420" },
        colorimetry = limited_range(settings.height),
        encoder = encoder_chain(settings, hardware),
        leaky = queue_properties(settings),
    )
}

/// The colorimetry video is encoded in: limited ("TV") range. Cameras that
/// send MJPEG deliver full range (0-255); encoded as is, decoders that
/// assume limited range, Discord's among them, stretch it and crush the
/// shadows, so the picture looks darker to viewers.
fn limited_range(height: u32) -> &'static str {
    if height >= 720 { "bt709" } else { "bt601" }
}

/// Counts of what the MJPEG check did to a camera's frames.
#[derive(Default)]
struct MjpegStats {
    checked: u64,
    damaged: u64,
    /// Damaged frames among the latest `MJPEG_WINDOW`.
    recent: std::collections::VecDeque<bool>,
    /// The check rejected so much that it is more likely wrong about this
    /// camera than the camera is broken; it has been switched off.
    distrusted: bool,
}

const MJPEG_WINDOW: usize = 60;

/// Drops damaged MJPEG frames before they reach the decoder (see
/// [`crate::mjpeg`]); the previous picture then stays up for a frame.
fn guard_mjpeg(pipeline: &gst::Pipeline) {
    let Some(pad) = pipeline
        .by_name("mjpeg")
        .and_then(|decoder| decoder.static_pad("sink"))
    else {
        return;
    };
    let stats = Mutex::new(MjpegStats::default());
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(buffer) = info.buffer() else {
            return gst::PadProbeReturn::Ok;
        };
        let Ok(mut stats) = stats.lock() else {
            return gst::PadProbeReturn::Ok;
        };
        if stats.distrusted {
            return gst::PadProbeReturn::Ok;
        }
        let Ok(map) = buffer.map_readable() else {
            return gst::PadProbeReturn::Ok;
        };
        let verdict = crate::mjpeg::check(map.as_slice());
        let damaged = matches!(verdict, crate::mjpeg::Verdict::Damaged(_));
        if stats.checked == 0 {
            // Says whether this camera's frames can be checked at all.
            tracing::info!(?verdict, bytes = map.len(), "first camera MJPEG frame");
        }
        stats.checked += 1;
        stats.recent.push_back(damaged);
        if stats.recent.len() > MJPEG_WINDOW {
            stats.recent.pop_front();
        }
        if !damaged {
            return gst::PadProbeReturn::Ok;
        }
        stats.damaged += 1;
        let recent = stats.recent.iter().filter(|damaged| **damaged).count();
        if stats.recent.len() == MJPEG_WINDOW && recent > MJPEG_WINDOW / 2 {
            stats.distrusted = true;
            tracing::warn!(
                ?verdict,
                "most camera frames fail the MJPEG check; passing them unchecked"
            );
            return gst::PadProbeReturn::Ok;
        }
        tracing::info!(
            ?verdict,
            bytes = map.len(),
            damaged = stats.damaged,
            checked = stats.checked,
            "dropped a damaged camera frame"
        );
        gst::PadProbeReturn::Drop
    });
}

/// A camera pipeline; `source` ends in raw video.
fn camera_description(source: &str, settings: &VideoSettings, hardware: bool) -> String {
    format!("{source} ! {}", pipeline_downstream(settings, hardware))
}

fn pipeline_description(source: &str, settings: &VideoSettings, hardware: bool) -> String {
    format!("{source} ! {}", pipeline_downstream(settings, hardware))
}

fn screen_description(settings: &VideoSettings, hardware: bool) -> String {
    format!(
        "pipewiresrc name=src do-timestamp=true keepalive-time=1000 resend-last=true \
         ! videorate skip-to-first=true \
         ! capsfilter caps=video/x-raw,framerate={fps}/1 \
         ! queue max-size-buffers=8{leaky} \
         ! videoconvert \
         ! videoscale \
         ! capsfilter caps=video/x-raw,format={format},width={width},height={height} \
         ! {encoder} \
         ! h264parse config-interval=-1 \
         ! capsfilter caps=video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false max-buffers=8 drop=true",
        fps = settings.fps,
        width = settings.width,
        height = settings.height,
        format = if hardware { "NV12" } else { "I420" },
        encoder = encoder_chain(settings, hardware),
        leaky = queue_properties(settings),
    )
}

/// What to ask a camera for, best first. Webcams deliver their full
/// framerate only as MJPEG above small sizes (a C920 gives raw 720p at
/// 10 fps but MJPEG 720p at 30), so MJPEG at the wanted size and rate comes
/// first; plain raw video, whatever the camera prefers, is the last resort.
/// Each entry is the pipeline between the source and the scaler.
fn camera_inputs(settings: &VideoSettings) -> Vec<String> {
    let (width, height, fps) = (settings.width, settings.height, settings.fps);
    let mjpeg = [
        format!(
            "capsfilter caps=\"image/jpeg,width={width},height={height},framerate={fps}/1\" ! jpegdec name=mjpeg"
        ),
        format!(
            "capsfilter caps=\"image/jpeg,width=[{width},4096],height=[{height},4096],framerate={fps}/1\" ! jpegdec name=mjpeg"
        ),
        format!(
            "capsfilter caps=\"image/jpeg,width={width},height={height}\" ! jpegdec name=mjpeg"
        ),
    ];
    let raw = [
        format!("capsfilter caps=\"video/x-raw,width={width},height={height},framerate={fps}/1\""),
        format!(
            "capsfilter caps=\"video/x-raw,width=[640,4096],height=[360,4096],framerate={fps}/1\""
        ),
        "capsfilter caps=video/x-raw".to_owned(),
    ];
    match settings.camera_format {
        CameraFormat::Auto => mjpeg.into_iter().chain(raw).collect(),
        // Any MJPEG the camera has before giving up on the choice.
        CameraFormat::Mjpeg => mjpeg
            .into_iter()
            .chain(["capsfilter caps=image/jpeg ! jpegdec name=mjpeg".to_owned()])
            .collect(),
        CameraFormat::Raw => raw.into(),
    }
}

/// How long a format the camera accepted gets to produce its first frame. A
/// format the camera lacks is refused at once, so silence means a camera
/// that is slow to start (some take ten seconds), not a wrong format.
const CAMERA_FORMAT_WAIT: Duration = Duration::from_secs(15);

/// Why a camera format was not kept.
enum StartFailure {
    /// The pipeline would not start, or reported an error.
    Refused(Error),
    /// It started but no frame came in time.
    Silent,
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

fn encoder_descriptions(settings: &VideoSettings, make: impl Fn(bool) -> String) -> Vec<String> {
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
        let produced = Arc::new(AtomicBool::new(false));
        let thread_produced = produced.clone();
        let failure = Arc::new(Mutex::new(None));
        let thread_failure = failure.clone();
        let bus = pipeline.bus();
        let started = Instant::now();
        let mut silence_reported = false;
        let thread = std::thread::spawn(move || {
            let mut last_arrival: Option<Instant> = None;
            while !thread_stop.load(Ordering::Relaxed) {
                // Errors are otherwise invisible: a pipeline that fails after
                // starting just stops delivering.
                while let Some(message) = bus.as_ref().and_then(|bus| {
                    bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Warning])
                }) {
                    match message.view() {
                        gst::MessageView::Error(error) => {
                            let text =
                                format!("{}: {}", error.error(), error.debug().unwrap_or_default());
                            tracing::warn!(error = %text, "capture pipeline error");
                            if let Ok(mut failure) = thread_failure.lock() {
                                *failure = Some(text);
                            }
                        }
                        gst::MessageView::Warning(warning) => tracing::warn!(
                            warning = %warning.error(),
                            detail = warning.debug().unwrap_or_default().as_str(),
                            "capture pipeline warning"
                        ),
                        _ => {}
                    }
                }
                if !silence_reported
                    && !thread_produced.load(Ordering::Relaxed)
                    && started.elapsed() > Duration::from_secs(4)
                {
                    silence_reported = true;
                    tracing::warn!("capture pipeline has produced no frame in 4 s");
                }
                let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(100))
                else {
                    if last_arrival.is_some_and(|last| last.elapsed() > Duration::from_millis(100))
                    {
                        tracing::debug!(
                            silent_ms = last_arrival.unwrap().elapsed().as_millis(),
                            "video capture starved"
                        );
                        last_arrival = Some(Instant::now());
                    }
                    continue;
                };
                let Some(buffer) = sample.buffer() else {
                    continue;
                };
                let Ok(map) = buffer.map_readable() else {
                    continue;
                };
                if !map.as_slice().is_empty() {
                    let now = Instant::now();
                    if let Some(last) = last_arrival {
                        let gap = now.saturating_duration_since(last);
                        if gap > Duration::from_millis(frame_ms as u64 * 2) {
                            tracing::debug!(
                                gap_ms = gap.as_millis(),
                                bytes = map.as_slice().len(),
                                "video encoder frame gap"
                            );
                        }
                    }
                    last_arrival = Some(now);
                    thread_produced.store(true, Ordering::Relaxed);
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
            produced,
            failure,
            thread: Some(thread),
            _portal: None,
            _camera: None,
            keyframes: Arc::default(),
        })
    }

    /// Waits until the pipeline has produced a frame; an error from the
    /// pipeline, or `timeout` passing first, fails.
    fn wait_for_first_frame(&self, timeout: Duration) -> std::result::Result<(), StartFailure> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.produced.load(Ordering::Relaxed) {
                return Ok(());
            }
            if let Some(error) = self.failure.lock().ok().and_then(|failure| failure.clone()) {
                return Err(StartFailure::Refused(Error::Message(error)));
            }
            if Instant::now() >= deadline {
                return Err(StartFailure::Silent);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
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
                    tracing::debug!(%error, "capture pipeline attempt failed");
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| Error::Message("no capture pipeline variant".into())))
    }

    /// Asks the encoder for a keyframe: a viewer cannot decode (PLI/FIR), or
    /// packets were lost on the way. At most one goes out per
    /// `KEYFRAME_SPACING`; a request inside that time is not dropped but
    /// served when it ends, so a loss just after a keyframe is still
    /// repaired without waiting for the next scheduled one.
    pub fn force_keyframe(&self) {
        let wait = {
            let Ok(mut state) = self.keyframes.lock() else {
                return;
            };
            match state.last {
                Some(last) if last.elapsed() < KEYFRAME_SPACING => {
                    if state.deferred {
                        return;
                    }
                    state.deferred = true;
                    Some(KEYFRAME_SPACING - last.elapsed())
                }
                _ => {
                    state.last = Some(Instant::now());
                    None
                }
            }
        };
        match wait {
            None => send_keyframe_request(&self.pipeline),
            Some(wait) => {
                let pipeline = self.pipeline.downgrade();
                let keyframes = self.keyframes.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(wait);
                    if let Ok(mut state) = keyframes.lock() {
                        state.deferred = false;
                        state.last = Some(Instant::now());
                    }
                    if let Some(pipeline) = pipeline.upgrade() {
                        send_keyframe_request(&pipeline);
                    }
                });
            }
        }
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
        Self::start_with_fallback(descriptions, 1000 / settings.fps.max(1), on_frame, |_| {
            Ok(())
        })
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
        Self::start_with_fallback(descriptions, 1000 / settings.fps.max(1), on_frame, |_| {
            Ok(())
        })
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
        let mut source = Self::from_pipewire(node_id, fd, true, &settings, targets, on_frame)?;
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
                // Only these two. This source is temperamental: with stream
                // properties added (node.dont-fallback, node.dont-reconnect)
                // it started and never delivered a frame.
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
        let inputs = camera_inputs(settings);
        let last_input = inputs.len() - 1;
        let mut last_error = None;
        // A format the camera lacks is refused at once; one it accepts but
        // never delivers costs the whole wait. After that the camera is
        // taken as it comes, so a dead camera holds the start up only once.
        let mut silent = 0;
        for (index, input) in inputs.iter().enumerate() {
            if silent >= 1 && index < last_input {
                continue;
            }
            let source = format!("pipewiresrc name=src do-timestamp=true ! {input}");
            for properties in &targets {
                let properties = properties.clone();
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
                    guard_mjpeg(pipeline);
                    Ok(())
                };
                let descriptions = encoder_descriptions(settings, |hardware| {
                    camera_description(&source, settings, hardware)
                });
                let started = Self::start_with_fallback(
                    descriptions,
                    1000 / settings.fps.max(1),
                    callback,
                    configure,
                )
                .map_err(StartFailure::Refused)
                .and_then(|video_source| {
                    // The last resort is taken as it comes; the others must
                    // show they deliver before they are kept.
                    if index < last_input {
                        video_source.wait_for_first_frame(CAMERA_FORMAT_WAIT)?;
                    }
                    Ok(video_source)
                });
                let started = match started {
                    Ok(video_source) => Ok(video_source),
                    Err(StartFailure::Refused(error)) => Err(error),
                    Err(StartFailure::Silent) => {
                        // The other ways of naming the node would be just as
                        // silent with this format.
                        silent += 1;
                        tracing::debug!(node_id, input, "camera format produced no frame in time");
                        last_error = Some(Error::Message("no frame in time".into()));
                        break;
                    }
                };
                match started {
                    Ok(video_source) => {
                        tracing::info!(
                            ?properties_for_log,
                            node_id,
                            input,
                            "pipewire capture source selected"
                        );
                        return Ok(video_source);
                    }
                    Err(error) => {
                        tracing::debug!(
                            ?properties_for_log,
                            node_id,
                            input,
                            %error,
                            "pipewire capture variant failed"
                        );
                        last_error = Some(error);
                    }
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
            let mut last_arrival: Option<Instant> = None;
            while !thread_stop.load(Ordering::Relaxed) {
                let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(100))
                else {
                    if last_arrival.is_some_and(|last| last.elapsed() > Duration::from_millis(100))
                    {
                        tracing::debug!(
                            silent_ms = last_arrival.unwrap().elapsed().as_millis(),
                            "video capture starved"
                        );
                        last_arrival = Some(Instant::now());
                    }
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
    fn full_range_video_is_converted_to_limited_range() {
        // Black is 0 in full range (what MJPEG decodes to) and 16 in the
        // limited range decoders expect.
        gst::init().unwrap();
        let pipeline = gst::parse::launch(&format!(
            "videotestsrc num-buffers=1 pattern=black \
             ! video/x-raw,format=I420,width=64,height=48,colorimetry=1:4:0:0 \
             ! videoconvert ! video/x-raw,format=I420,colorimetry={} \
             ! appsink name=sink sync=false",
            limited_range(48)
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let sink = pipeline
            .by_name("sink")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let sample = sink.pull_sample().unwrap();
        let luma = sample.buffer().unwrap().map_readable().unwrap().as_slice()[0];
        pipeline.set_state(gst::State::Null).unwrap();
        assert_eq!(luma, 16);
    }

    /// The screen pipeline with a test picture in place of the screen
    /// produces frames, with keyframes scheduled and on demand.
    #[test]
    fn the_screen_pipeline_produces_frames() {
        gst::init().unwrap();
        for keyframe_secs in [2, 0] {
            let settings = VideoSettings {
                keyframe_secs,
                encoder: EncoderPreference::Cpu,
                ..VideoSettings::default()
            };
            let description = screen_description(&settings, false).replace(
                "pipewiresrc name=src do-timestamp=true keepalive-time=1000 resend-last=true",
                "videotestsrc is-live=true name=src",
            );
            assert!(description.contains("videotestsrc"), "{description}");
            let (tx, rx) = std::sync::mpsc::channel();
            let source = VideoSource::start_pipeline_with(
                &description,
                33,
                move |frame: EncodedFrame| {
                    let _ = tx.send(frame.data.len());
                },
                |_| Ok(()),
            )
            .unwrap();
            let first = rx.recv_timeout(Duration::from_secs(5));
            drop(source);
            assert!(
                first.is_ok(),
                "no frame with keyframe_secs={keyframe_secs}: {description}"
            );
        }
    }

    #[test]
    fn camera_formats_are_tried_best_first() {
        let mut settings = VideoSettings::default();
        let auto = camera_inputs(&settings);
        assert!(auto[0].contains("image/jpeg,width=1280,height=720,framerate=30/1"));
        assert_eq!(auto.last().unwrap(), "capsfilter caps=video/x-raw");
        settings.camera_format = CameraFormat::Mjpeg;
        assert!(
            camera_inputs(&settings)
                .iter()
                .all(|input| input.contains("image/jpeg"))
        );
        settings.camera_format = CameraFormat::Raw;
        assert!(
            camera_inputs(&settings)
                .iter()
                .all(|input| input.contains("video/x-raw"))
        );
    }

    #[test]
    fn a_frame_tap_can_be_redirected_while_running() {
        let frame = || EncodedFrame {
            data: vec![0, 0, 0, 1],
            duration_ms: 33,
        };
        let tap = FrameTap::default();
        let mut capture = tap.sink();
        let preview = Arc::new(Mutex::new(0));
        let stream = Arc::new(Mutex::new(0));

        let seen = preview.clone();
        tap.set(move |_| *seen.lock().unwrap() += 1);
        capture(frame());
        // Handed over: nothing goes anywhere until the stream takes it.
        tap.clear();
        capture(frame());
        let seen = stream.clone();
        tap.set(move |_| *seen.lock().unwrap() += 1);
        capture(frame());
        capture(frame());

        assert_eq!(*preview.lock().unwrap(), 1);
        assert_eq!(*stream.lock().unwrap(), 2);
    }

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
             ! videoconvert ! videoscale ! videorate skip-to-first=true \
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
