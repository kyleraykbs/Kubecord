//! Camera and screen capture to H.264 Annex-B access units.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    /// The longest the stream goes without a keyframe, in milliseconds
    /// (zero: no limit). The capture thread asks for one when it passes.
    keyframe_gap_ms: Arc<AtomicU32>,
    /// Present when the bitrate or framerate adapts to the network.
    adaptation: Mutex<Option<Adaptation>>,
    /// The time each frame stands for, which follows the framerate.
    frame_ms: Arc<AtomicU32>,
    /// Renegotiation is being kept from the source (see
    /// `hold_source_format`).
    source_held: AtomicBool,
    /// When the capture began, and how long after that its latest frame
    /// came (in milliseconds; zero before the first).
    started: Instant,
    last_frame_ms: Arc<std::sync::atomic::AtomicU64>,
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

/// Logs the format a capture's picture arrives in and the one it is
/// encoded in, including the colour range: what to look at when a picture
/// comes out lighter or darker than it went in.
fn log_formats(pipeline: &gst::Pipeline) {
    let (captured, encoded) = capture_formats(pipeline);
    tracing::info!(captured, encoded, "capture formats");
}

/// A picture format as `I420 1280x720 colorimetry bt709`, the
/// colorimetry being what GStreamer takes it to be: caps often leave it
/// out, and then a default for the format and size applies. The range is
/// its first field: 1 full (0–255), 2 limited (16–235).
fn describe_video_caps(caps: &gst::CapsRef) -> String {
    match gstreamer_video::VideoInfo::from_caps(caps) {
        Ok(info) => format!(
            "{} {}x{} colorimetry {}",
            info.format().to_str(),
            info.width(),
            info.height(),
            info.colorimetry()
        ),
        Err(_) => caps.to_string(),
    }
}

/// The format entering the colour conversion, and the one the encoder
/// receives.
fn capture_formats(pipeline: &gst::Pipeline) -> (String, String) {
    let caps_at = |factory: &str, pad: &str| {
        pipeline
            .iterate_recurse()
            .into_iter()
            .flatten()
            .find(|element| {
                element
                    .factory()
                    .is_some_and(|found| found.name() == factory)
            })
            .and_then(|element| element.static_pad(pad))
            .and_then(|pad| pad.current_caps())
            .map(|caps| describe_video_caps(&caps))
            .unwrap_or_else(|| "unknown".to_owned())
    };
    let encoder = pipeline
        .by_name("enc")
        .and_then(|encoder| encoder.static_pad("sink"))
        .and_then(|pad| pad.current_caps())
        .map(|caps| describe_video_caps(&caps))
        .unwrap_or_else(|| "unknown".to_owned());
    (caps_at("videoconvert", "sink"), encoder)
}

/// Asks the encoder for a keyframe, at most one per `KEYFRAME_SPACING`; a
/// request inside that time is not dropped but served when it ends.
fn request_keyframe(pipeline: &gst::Pipeline, keyframes: &Arc<Mutex<KeyframeRequests>>) {
    let wait = {
        let Ok(mut state) = keyframes.lock() else {
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
        None => send_keyframe_request(pipeline),
        Some(wait) => {
            let pipeline = pipeline.downgrade();
            let keyframes = keyframes.clone();
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
    /// Lower the bitrate while packets are being lost and bring it back
    /// when the connection is clean; `bitrate_kbps` is then the most used.
    pub adaptive_bitrate: bool,
    /// Least time between keyframes sent because packets were lost.
    pub loss_keyframe_ms: u32,
    /// Send a keyframe when none has gone out for this many seconds, so a
    /// viewer who lost packets on their own connection recovers without
    /// having to ask. Zero: only when asked or on loss.
    pub keyframe_gap_secs: u32,
    /// Send every video packet a second time, 20 ms later.
    pub send_twice: bool,
    /// The least an adapting bitrate goes down to, in kbit/s. Zero: three
    /// tenths of `bitrate_kbps`.
    pub bitrate_floor_kbps: u32,
    /// Keep an adapting bitrate under the receiver's bandwidth estimate.
    /// Off when the top of the range was set by hand: Discord's estimate
    /// for a stream sits near 4 Mbit/s whatever the connection carries.
    pub bitrate_follows_estimate: bool,
    /// Let the framerate follow the network too, between this and `fps`.
    /// Zero (or `fps` and above) keeps the framerate fixed.
    pub fps_floor: u32,
    /// With an adapting framerate, the one used while the connection is
    /// clean; `fps` is then the most it climbs to after a long clean
    /// spell. Zero: `fps`. (The capture is built for `fps` and starts at
    /// this.)
    pub fps_target: u32,
    /// With an adapting bitrate, the most it climbs to after a long clean
    /// spell; `bitrate_kbps` is what it starts at and comes back to. Zero:
    /// `bitrate_kbps`.
    pub bitrate_top_kbps: u32,
    /// With both adapting, which is kept longest.
    pub adapt_priority: AdaptPriority,
}

/// When both the bitrate and the framerate adapt, which gives way first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AdaptPriority {
    /// Both go down together.
    #[default]
    Balanced,
    /// Keep the bitrate: the framerate goes down first, so each frame stays
    /// sharp.
    Bitrate,
    /// Keep the framerate: the bitrate goes down first, so motion stays
    /// smooth.
    Framerate,
}

/// Where the bitrate and the framerate stand in their ranges (1 at the
/// top, 0 at the bottom) for the connection's `level`. With a priority,
/// the other one takes the first half of the way down on its own.
pub fn split_level(level: f64, priority: AdaptPriority) -> (f64, f64) {
    let level = level.clamp(0.0, 1.0);
    let upper = ((level - 0.5) * 2.0).max(0.0);
    let lower = (level * 2.0).min(1.0);
    match priority {
        AdaptPriority::Balanced => (level, level),
        AdaptPriority::Bitrate => (lower, upper),
        AdaptPriority::Framerate => (upper, lower),
    }
}

/// Framerates an adapting capture moves between, besides the ends of its
/// own range.
const FRAMERATE_STEPS: [u32; 8] = [5, 10, 15, 20, 24, 30, 45, 60];
/// The least time between changes of framerate: each one restarts the
/// encoder's stream with a keyframe.
const FRAMERATE_CHANGE_SPACING: Duration = Duration::from_secs(10);
/// The least time between changes of a hardware encoder's bitrate. x264
/// takes a new bitrate in its stride; `vah264enc` starts its stream over
/// with a keyframe, and its keyframes are large (a third of a second of
/// bitrate), so steps are gathered up rather than taken one by one.
const HARDWARE_BITRATE_SPACING: Duration = Duration::from_secs(10);

/// Whether a change last made at `changed` may be followed by another.
fn spaced(changed: Option<Instant>, spacing: Duration) -> bool {
    changed.is_none_or(|changed| changed.elapsed() >= spacing)
}

/// The framerate for a `level` of the connection (1 good, 0 at its worst)
/// within `min..=max`: the highest step not above that share of the range.
pub fn framerate_for(level: f64, min: u32, max: u32) -> u32 {
    if min >= max {
        return max;
    }
    let wanted = f64::from(min) + level.clamp(0.0, 1.0) * f64::from(max - min);
    FRAMERATE_STEPS
        .iter()
        .copied()
        .chain([min, max])
        .filter(|step| (min..=max).contains(step) && f64::from(*step) <= wanted + 0.5)
        .max()
        .unwrap_or(min)
}

/// What a running capture adapts to the network, and where it stands.
struct Adaptation {
    controller: BitrateController,
    /// The encoder's bitrate follows the controller. (With a fixed bitrate
    /// the controller still runs, to steer the framerate.)
    bitrate: bool,
    follows_estimate: bool,
    /// The framerate's range, when it adapts: lowest, target, highest.
    fps: Option<(u32, u32, u32)>,
    current_fps: u32,
    fps_changed: Option<Instant>,
    /// With both adapting, which gives way first.
    priority: AdaptPriority,
    /// The bitrate the encoder was last given.
    applied_kbps: u32,
    bitrate_changed: Option<Instant>,
    /// The least time between changes of bitrate.
    bitrate_spacing: Duration,
}

/// Follows how the network treats the video and picks a bitrate for it:
/// down quickly when packets are lost in numbers, back up slowly when the
/// connection has been clean. The occasional small burst of loss that any
/// connection has does not count as trouble.
#[derive(Clone, Debug)]
pub struct BitrateController {
    /// Where it sits while the connection is clean.
    target: u32,
    floor: u32,
    /// The most it climbs to after a long clean spell.
    top: u32,
    current: u32,
    /// Recent reports, as (sent, lost).
    recent: std::collections::VecDeque<(u32, u32)>,
}

impl BitrateController {
    /// Loss in one report that means the connection is over its limit.
    const TROUBLE: f64 = 0.02;
    /// Loss over `CLEAN_REPORTS` reports below which it has room again.
    const CLEAN: f64 = 0.008;
    const CLEAN_REPORTS: usize = 5;
    /// Clean reports needed per step above the target: a long clean spell
    /// (30 s), so it only goes past what was asked for on a connection
    /// that has shown it has room.
    const CLEAN_REPORTS_ABOVE: usize = 15;

    pub fn new(target_kbps: u32) -> Self {
        Self::with_floor(target_kbps, 0)
    }

    /// Adapts between `floor_kbps` and `target_kbps`; a floor of zero is
    /// three tenths of the target.
    pub fn with_floor(target_kbps: u32, floor_kbps: u32) -> Self {
        Self::with_range(floor_kbps, target_kbps, target_kbps)
    }

    /// Starts at `target_kbps`, drops toward `floor_kbps` (zero: three
    /// tenths of the target) on loss, and climbs above the target toward
    /// `top_kbps` only after a long clean spell.
    pub fn with_range(floor_kbps: u32, target_kbps: u32, top_kbps: u32) -> Self {
        let floor = match floor_kbps {
            0 => (target_kbps * 3 / 10).max(300),
            floor => floor,
        };
        Self {
            target: target_kbps,
            floor: floor.min(target_kbps),
            top: top_kbps.max(target_kbps),
            current: target_kbps,
            recent: Default::default(),
        }
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    /// The range below the target: `(floor, target)`.
    pub fn range(&self) -> (u32, u32) {
        (self.floor, self.target)
    }

    /// Where it stands between the floor and the target: 1 at the target
    /// (or above), 0 at the floor.
    pub fn level(&self) -> f64 {
        if self.target <= self.floor {
            return 1.0;
        }
        (f64::from(self.current.saturating_sub(self.floor)) / f64::from(self.target - self.floor))
            .min(1.0)
    }

    /// How far above the target it has climbed: 0 at the target (or
    /// below), 1 at the top.
    pub fn headroom(&self) -> f64 {
        if self.top <= self.target {
            return 0.0;
        }
        f64::from(self.current.saturating_sub(self.target)) / f64::from(self.top - self.target)
    }

    /// Takes one report; returns the new bitrate when it changes.
    /// `ceiling_kbps` is the receiver's estimate of what fits (zero if
    /// unknown).
    pub fn report(&mut self, sent: u32, lost: u32, ceiling_kbps: u32) -> Option<u32> {
        if sent < 50 {
            // Too little sent to judge by.
            return None;
        }
        let before = self.current;
        let loss = f64::from(lost) / f64::from(sent);
        if loss >= Self::TROUBLE {
            self.current = (self.current * 85 / 100).max(self.floor);
            self.recent.clear();
        } else {
            self.recent.push_back((sent, lost));
            // Back to the target readily; past it only after a long spell.
            let (needed, limit) = if self.current < self.target {
                (Self::CLEAN_REPORTS, self.target)
            } else {
                (Self::CLEAN_REPORTS_ABOVE, self.top)
            };
            if self.recent.len() >= needed {
                let (sent, lost) = self
                    .recent
                    .iter()
                    .fold((0u64, 0u64), |(s, l), (sent, lost)| {
                        (s + u64::from(*sent), l + u64::from(*lost))
                    });
                if (lost as f64) < sent as f64 * Self::CLEAN && self.current < limit {
                    self.current = (self.current * 110 / 100).max(self.current + 1).min(limit);
                }
                // One step up per run of clean reports.
                self.recent.clear();
            }
        }
        if ceiling_kbps > 0 {
            let ceiling = (ceiling_kbps * 95 / 100).max(self.floor);
            self.current = self.current.min(ceiling.max(self.floor));
        }
        (self.current != before).then_some(self.current)
    }
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
            adaptive_bitrate: true,
            loss_keyframe_ms: 1000,
            keyframe_gap_secs: 0,
            send_twice: false,
            bitrate_floor_kbps: 0,
            bitrate_follows_estimate: true,
            fps_floor: 0,
            fps_target: 0,
            bitrate_top_kbps: 0,
            adapt_priority: AdaptPriority::Balanced,
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

/// The most frames `vah264enc` takes between keyframes. Asked for more, it
/// refuses the setting and falls back to a keyframe every second.
const HARDWARE_KEYFRAME_DISTANCE: u32 = 1024;

/// Slices in a hardware-encoded frame. It was 32 once, which no frame
/// survived: see [`kc_voice::dave::MAX_VIDEO_SLICES`].
const HARDWARE_SLICES: usize = 4;

/// Threads for x264, which with `tune=zerolatency` cuts every frame into
/// one slice per thread: as many as there are processors, but no more
/// than a frame may have slices.
fn x264_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |count| count.get())
        .min(kc_voice::dave::MAX_VIDEO_SLICES)
}

fn encoder_chain(settings: &VideoSettings, hardware: bool) -> String {
    if hardware {
        format!(
            "vah264enc name=enc bitrate={bitrate} key-int-max={keyint} b-frames=0 rate-control=cbr target-usage=7 num-slices={HARDWARE_SLICES}",
            bitrate = settings.bitrate_kbps,
            keyint = keyframe_distance(settings).min(HARDWARE_KEYFRAME_DISTANCE),
        )
    } else {
        format!(
            // x264enc's default 600 ms VBV lets a single IDR take 0.6 s of
            // bitrate (150 KB at 2.5 Mbit/s, ~130 RTP packets). Bound any one
            // frame to 150 ms of bitrate so keyframes stay small enough to
            // pace out within a few frame intervals.
            "{encoder} name=enc tune=zerolatency speed-preset={preset} threads={threads} bitrate={bitrate} vbv-buf-capacity=150 key-int-max={keyint} bframes=0",
            encoder = encoder_element(),
            threads = x264_threads(),
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
         ! videoconvert ! videoscale ! videorate name=pace skip-to-first=true \
         ! capsfilter name=rate caps=video/x-raw,format={format},width={width},height={height},framerate={fps}/1,colorimetry={colorimetry} \
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
         ! videorate name=pace skip-to-first=true \
         ! capsfilter name=rate caps=video/x-raw,framerate={fps}/1 \
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
        let keyframes: Arc<Mutex<KeyframeRequests>> = Arc::default();
        let keyframe_gap_ms = Arc::new(AtomicU32::new(0));
        let (thread_keyframes, thread_gap) = (keyframes.clone(), keyframe_gap_ms.clone());
        let thread_pipeline = pipeline.downgrade();
        let frame_ms = Arc::new(AtomicU32::new(frame_ms));
        let thread_frame_ms = frame_ms.clone();
        let last_frame_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let thread_last_frame = last_frame_ms.clone();
        let thread = std::thread::spawn(move || {
            let mut last_arrival: Option<Instant> = None;
            // When a keyframe last came out of the encoder, or was last
            // asked for because none had.
            let mut last_keyframe = Instant::now();
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
                    let frame_ms = thread_frame_ms.load(Ordering::Relaxed);
                    thread_last_frame.store(
                        (now.duration_since(started).as_millis() as u64).max(1),
                        Ordering::Relaxed,
                    );
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
                    if !thread_produced.swap(true, Ordering::Relaxed)
                        && let Some(pipeline) = thread_pipeline.upgrade()
                    {
                        log_formats(&pipeline);
                    }
                    let gap = u64::from(thread_gap.load(Ordering::Relaxed));
                    if is_keyframe(map.as_slice()) {
                        last_keyframe = now;
                    } else if gap > 0
                        && now.duration_since(last_keyframe) >= Duration::from_millis(gap)
                        && let Some(pipeline) = thread_pipeline.upgrade()
                    {
                        tracing::info!(
                            quiet_ms = now.duration_since(last_keyframe).as_millis(),
                            "no keyframe for a while; sending one"
                        );
                        last_keyframe = now;
                        request_keyframe(&pipeline, &thread_keyframes);
                    }
                    on_frame(EncodedFrame {
                        data: crate::h264::conform(map.as_slice()),
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
            keyframes,
            keyframe_gap_ms,
            adaptation: Mutex::new(None),
            frame_ms,
            source_held: AtomicBool::new(false),
            started,
            last_frame_ms,
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

    /// Has a keyframe sent whenever none has gone out for `gap` (zero: only
    /// when asked for). Any keyframe counts, whatever caused it.
    pub fn set_keyframe_gap(&self, gap: Duration) {
        self.keyframe_gap_ms.store(
            gap.as_millis().min(u128::from(u32::MAX)) as u32,
            Ordering::Relaxed,
        );
    }

    /// Lets the capture follow the network as `settings` ask: the bitrate
    /// (when it is automatic), the framerate (when it has a range), both
    /// or neither.
    pub fn adapt(&self, settings: &VideoSettings) {
        let top_fps = settings.fps;
        let target_fps = match settings.fps_target {
            0 => top_fps,
            target => target.min(top_fps),
        };
        let fps = (settings.fps_floor > 0 && settings.fps_floor < top_fps).then_some((
            settings.fps_floor,
            target_fps.max(settings.fps_floor),
            top_fps,
        ));
        let controller = if settings.adaptive_bitrate {
            BitrateController::with_range(
                settings.bitrate_floor_kbps,
                settings.bitrate_kbps,
                settings.bitrate_top_kbps,
            )
        } else {
            // A fixed bitrate: the controller only steers the framerate,
            // in units of its own.
            BitrateController::with_range(300, 1000, 2000)
        };
        // The capture is built for the highest framerate and starts at the
        // target.
        let mut current_fps = top_fps;
        if let Some((_, target, _)) = fps
            && target < top_fps
            && self.set_framerate(target)
        {
            current_fps = target;
        }
        let adaptation = (settings.adaptive_bitrate || fps.is_some()).then_some(Adaptation {
            controller,
            bitrate: settings.adaptive_bitrate,
            follows_estimate: settings.bitrate_follows_estimate,
            fps,
            current_fps,
            fps_changed: None,
            priority: settings.adapt_priority,
            applied_kbps: settings.bitrate_kbps,
            bitrate_changed: None,
            bitrate_spacing: if self.encodes_in_hardware() {
                HARDWARE_BITRATE_SPACING
            } else {
                Duration::ZERO
            },
        });
        if let Ok(mut slot) = self.adaptation.lock() {
            *slot = adaptation;
        }
    }

    /// Where an adapting capture stands: the bitrate it aims for (when
    /// the bitrate adapts) and its framerate (when that does).
    pub fn adapted(&self) -> (Option<u32>, Option<u32>) {
        match self.adaptation.lock().ok().as_deref() {
            Some(Some(adaptation)) => (
                adaptation.bitrate.then_some(adaptation.applied_kbps),
                adaptation.fps.map(|_| adaptation.current_fps),
            ),
            _ => (None, None),
        }
    }

    /// How the network treated the video lately (see
    /// [`BitrateController::report`]); changes the encoder's bitrate and
    /// the framerate when that calls for it. Does nothing unless the
    /// capture adapts.
    pub fn network_report(&self, sent: u32, lost: u32, estimate_kbps: u32) {
        let Ok(mut slot) = self.adaptation.lock() else {
            return;
        };
        let Some(adaptation) = slot.as_mut() else {
            return;
        };
        let ceiling = if adaptation.follows_estimate {
            estimate_kbps
        } else {
            0
        };
        adaptation.controller.report(sent, lost, ceiling);
        // A priority only means something when both adapt.
        let priority = if adaptation.bitrate && adaptation.fps.is_some() {
            adaptation.priority
        } else {
            AdaptPriority::Balanced
        };
        let (bitrate_level, fps_level) = split_level(adaptation.controller.level(), priority);
        // Above the target both climb together, toward their highest.
        let headroom = adaptation.controller.headroom();
        if adaptation.bitrate {
            let (floor, target) = adaptation.controller.range();
            let kbps = if headroom > 0.0 {
                adaptation.controller.current()
            } else {
                floor + (bitrate_level * f64::from(target - floor)).round() as u32
            };
            if kbps != adaptation.applied_kbps
                && spaced(adaptation.bitrate_changed, adaptation.bitrate_spacing)
            {
                tracing::info!(
                    from_kbps = adaptation.applied_kbps,
                    to_kbps = kbps,
                    sent,
                    lost,
                    ceiling_kbps = ceiling,
                    ?priority,
                    "video bitrate adapted"
                );
                adaptation.applied_kbps = kbps;
                adaptation.bitrate_changed = Some(Instant::now());
                self.set_bitrate(kbps);
            }
        }
        let Some((min, target, max)) = adaptation.fps else {
            return;
        };
        let wanted = if headroom > 0.0 {
            framerate_for(headroom, target, max)
        } else {
            framerate_for(fps_level, min, target)
        };
        if wanted != adaptation.current_fps
            && spaced(adaptation.fps_changed, FRAMERATE_CHANGE_SPACING)
            && self.set_framerate(wanted)
        {
            tracing::info!(
                from_fps = adaptation.current_fps,
                to_fps = wanted,
                "video framerate adapted"
            );
            adaptation.current_fps = wanted;
            adaptation.fps_changed = Some(Instant::now());
        }
    }

    /// The error the capture pipeline reported, if it has failed.
    pub fn failed(&self) -> Option<String> {
        self.failure.lock().ok().and_then(|failure| failure.clone())
    }

    /// How long since the capture last produced a frame, once it has
    /// produced one. A healthy capture is never quiet for long: a still
    /// screen is resent every second.
    pub fn quiet_for(&self) -> Option<Duration> {
        match self.last_frame_ms.load(Ordering::Relaxed) {
            0 => None,
            millis => Some(
                self.started
                    .elapsed()
                    .saturating_sub(Duration::from_millis(millis)),
            ),
        }
    }

    /// Keeps a change of framerate from reaching the capture's source. New
    /// caps make GStreamer ask everything upstream to renegotiate, and a
    /// PipeWire source answers by dropping its stream and connecting again,
    /// which ends a screencast for good (the compositor sees the capture
    /// stop). The frame-rate limiter takes any rate coming in, so nothing
    /// before it has to change: the request is dropped at its input.
    fn hold_source_format(&self) {
        if self.source_held.swap(true, Ordering::Relaxed) {
            return;
        }
        let Some(pad) = self
            .pipeline
            .by_name("pace")
            .and_then(|pace| pace.static_pad("sink"))
        else {
            return;
        };
        pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, |_, info| {
            match info.event() {
                Some(event) if event.type_() == gst::EventType::Reconfigure => {
                    gst::PadProbeReturn::Drop
                }
                _ => gst::PadProbeReturn::Ok,
            }
        });
    }

    /// Changes the framerate while the capture runs: the frame-rate limit
    /// ahead of the encoder is renegotiated, which makes the encoder start
    /// afresh with a keyframe.
    fn set_framerate(&self, fps: u32) -> bool {
        let Some(limit) = self.pipeline.by_name("rate") else {
            return false;
        };
        self.hold_source_format();
        let mut caps = limit.property::<gst::Caps>("caps");
        for structure in caps.make_mut().iter_mut() {
            structure.set("framerate", gst::Fraction::new(fps as i32, 1));
        }
        limit.set_property("caps", &caps);
        self.frame_ms.store(1000 / fps.max(1), Ordering::Relaxed);
        true
    }

    /// Changes the encoder's bitrate while it runs.
    /// True when the VA-API encoder is the one running.
    fn encodes_in_hardware(&self) -> bool {
        self.pipeline
            .by_name("enc")
            .and_then(|encoder| encoder.factory())
            .is_some_and(|factory| factory.name() == "vah264enc")
    }

    fn set_bitrate(&self, kbps: u32) {
        if let Some(encoder) = self.pipeline.by_name("enc")
            && encoder.find_property("bitrate").is_some()
        {
            encoder.set_property("bitrate", kbps);
        }
    }

    /// Asks the encoder for a keyframe: a viewer cannot decode (PLI/FIR), or
    /// packets were lost on the way. At most one goes out per
    /// `KEYFRAME_SPACING`; a request inside that time is not dropped but
    /// served when it ends, so a loss just after a keyframe is still
    /// repaired without waiting for the next scheduled one.
    pub fn force_keyframe(&self) {
        request_keyframe(&self.pipeline, &self.keyframes);
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
                "pipewiresrc plugin unavailable; start kubecord from `nix develop` or the packaged Nix build"
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
    fn bitrate_backs_off_on_heavy_loss_and_climbs_back_when_clean() {
        let mut controller = BitrateController::new(4000);
        // The odd small burst is not trouble.
        assert_eq!(controller.report(1000, 5, 0), None);
        // Two percent is: down fifteen percent each time, to a floor.
        assert_eq!(controller.report(1000, 25, 0), Some(3400));
        assert_eq!(controller.report(1000, 40, 0), Some(2890));
        for _ in 0..20 {
            controller.report(1000, 100, 0);
        }
        assert_eq!(controller.current(), 1200);
        // Clean for five reports: one step up; then back to the target.
        for _ in 0..4 {
            assert_eq!(controller.report(1000, 2, 0), None);
        }
        assert_eq!(controller.report(1000, 2, 0), Some(1320));
        for _ in 0..200 {
            controller.report(1000, 0, 0);
        }
        assert_eq!(controller.current(), 4000);
        // The receiver's estimate caps it; too little sent says nothing.
        assert_eq!(controller.report(1000, 0, 3000), Some(2850));
        assert_eq!(controller.report(10, 10, 0), None);
    }

    /// With nobody asking, a keyframe still goes out when none has for the
    /// set time; with no such time set, only the first frame is one.
    #[test]
    fn a_keyframe_is_sent_when_none_has_been_for_a_while() {
        gst::init().unwrap();
        let settings = VideoSettings {
            encoder: EncoderPreference::Cpu,
            ..VideoSettings::default()
        };
        let description = screen_description(&settings, false).replace(
            "pipewiresrc name=src do-timestamp=true keepalive-time=1000 resend-last=true",
            "videotestsrc is-live=true name=src",
        );
        let count = |gap: Duration| {
            let keyframes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = keyframes.clone();
            let source = VideoSource::start_pipeline_with(
                &description,
                33,
                move |frame: EncodedFrame| {
                    if is_keyframe(&frame.data) {
                        counted.fetch_add(1, Ordering::Relaxed);
                    }
                },
                |_| Ok(()),
            )
            .unwrap();
            source.set_keyframe_gap(gap);
            std::thread::sleep(Duration::from_millis(3600));
            drop(source);
            keyframes.load(Ordering::Relaxed)
        };
        assert_eq!(count(Duration::ZERO), 1);
        let with_gap = count(Duration::from_secs(1));
        assert!(
            (3..=5).contains(&with_gap),
            "expected about four, got {with_gap}"
        );
    }

    /// What a screen picture arrives as and what is encoded, colour range
    /// included, can be read from a running capture.
    #[test]
    fn capture_formats_are_reported() {
        gst::init().unwrap();
        let settings = VideoSettings {
            encoder: EncoderPreference::Cpu,
            ..VideoSettings::default()
        };
        let description = screen_description(&settings, false).replace(
            "pipewiresrc name=src do-timestamp=true keepalive-time=1000 resend-last=true",
            "videotestsrc is-live=true name=src ! video/x-raw,format=BGRx",
        );
        let source =
            VideoSource::start_pipeline_with(&description, 33, |_: EncodedFrame| {}, |_| Ok(()))
                .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let (captured, encoded) = capture_formats(&source.pipeline);
        drop(source);
        assert!(captured.starts_with("BGRx"), "{captured}");
        // A screen is sent in limited range, which is what decoders assume.
        assert_eq!(encoded, "I420 1280x720 colorimetry bt709");
        println!("captured: {captured}\nencoded: {encoded}");
    }

    #[test]
    fn the_framerate_follows_the_level_in_steps_within_its_range() {
        assert_eq!(framerate_for(1.0, 15, 30), 30);
        assert_eq!(framerate_for(0.0, 15, 30), 15);
        // Halfway between 15 and 30 is 22.5: the step below it.
        assert_eq!(framerate_for(0.5, 15, 30), 20);
        assert_eq!(framerate_for(0.7, 15, 30), 24);
        // The ends of the range count even when they are not steps.
        assert_eq!(framerate_for(0.0, 12, 50), 12);
        assert_eq!(framerate_for(1.0, 12, 50), 50);
        // No range, no change.
        assert_eq!(framerate_for(0.2, 30, 30), 30);
    }

    #[test]
    fn a_priority_decides_which_gives_way_first() {
        use AdaptPriority::*;
        // Clean, and at the very bottom: the same whatever the priority.
        for priority in [Balanced, Bitrate, Framerate] {
            assert_eq!(split_level(1.0, priority), (1.0, 1.0));
            assert_eq!(split_level(0.0, priority), (0.0, 0.0));
        }
        // Three quarters of the way up: Balanced lowers both a little; a
        // priority keeps its own at the top and lowers the other by half.
        assert_eq!(split_level(0.75, Balanced), (0.75, 0.75));
        assert_eq!(split_level(0.75, Bitrate), (1.0, 0.5));
        assert_eq!(split_level(0.75, Framerate), (0.5, 1.0));
        // A quarter of the way: the other is at its bottom, and the kept
        // one is on its way down.
        assert_eq!(split_level(0.25, Bitrate), (0.5, 0.0));
        assert_eq!(split_level(0.25, Framerate), (0.0, 0.5));
    }

    #[test]
    fn the_target_is_kept_and_the_top_reached_only_after_a_long_clean_spell() {
        let mut controller = BitrateController::with_range(2000, 4000, 8000);
        assert_eq!(controller.current(), 4000);
        assert_eq!((controller.level(), controller.headroom()), (1.0, 0.0));
        // Ten seconds clean is not enough to go past the target.
        for _ in 0..5 {
            controller.report(1000, 0, 0);
        }
        assert_eq!(controller.current(), 4000);
        // Thirty seconds is: one step up.
        for _ in 0..10 {
            controller.report(1000, 0, 0);
        }
        assert_eq!(controller.current(), 4400);
        assert!(controller.headroom() > 0.0);
        // A long clean run reaches the top and stops there.
        for _ in 0..600 {
            controller.report(1000, 0, 0);
        }
        assert_eq!((controller.current(), controller.headroom()), (8000, 1.0));
        // Loss brings it down past the target toward the floor; it comes
        // back to the target readily.
        for _ in 0..20 {
            controller.report(1000, 100, 0);
        }
        assert_eq!(controller.current(), 2000);
        for _ in 0..40 {
            controller.report(1000, 0, 0);
        }
        assert_eq!(controller.current(), 4000);
    }

    #[test]
    fn a_bitrate_range_set_by_hand_is_kept_to() {
        let mut controller = BitrateController::with_floor(6000, 2000);
        assert_eq!(controller.level(), 1.0);
        for _ in 0..30 {
            controller.report(1000, 100, 0);
        }
        assert_eq!(controller.current(), 2000);
        assert_eq!(controller.level(), 0.0);
        // A floor above the top is the top.
        assert_eq!(BitrateController::with_floor(1000, 5000).current(), 1000);
    }

    /// The framerate of a running capture drops and comes back as the
    /// network reports go, and frames keep coming throughout.
    #[test]
    fn the_framerate_adapts_while_encoding() {
        gst::init().unwrap();
        framerate_adapts(false);
        // The hardware encoder too, where there is one.
        if gst::ElementFactory::find("vah264enc").is_some() {
            framerate_adapts(true);
        }
    }

    fn framerate_adapts(hardware: bool) {
        let settings = VideoSettings {
            encoder: EncoderPreference::Cpu,
            fps: 30,
            fps_floor: 10,
            adaptive_bitrate: false,
            ..VideoSettings::default()
        };
        let description = screen_description(&settings, hardware).replace(
            "pipewiresrc name=src do-timestamp=true keepalive-time=1000 resend-last=true",
            "videotestsrc is-live=true name=src",
        );
        let frames = Arc::new(Mutex::new(Vec::<u32>::new()));
        let seen = frames.clone();
        let source = VideoSource::start_pipeline_with(
            &description,
            33,
            move |frame: EncodedFrame| seen.lock().unwrap().push(frame.duration_ms),
            |_| Ok(()),
        )
        .unwrap();
        source.adapt(&settings);
        let count = |seconds: u64| {
            std::thread::sleep(Duration::from_millis(600));
            frames.lock().unwrap().clear();
            std::thread::sleep(Duration::from_secs(seconds));
            let frames = frames.lock().unwrap();
            (
                frames.len() as u64 / seconds,
                frames.last().copied().unwrap_or(0),
            )
        };
        let (full, full_ms) = count(2);
        // Heavy loss: the first report takes it one step down, to 24. More
        // reports change nothing yet: changes are spaced out, since each
        // one costs a keyframe.
        for _ in 0..30 {
            source.network_report(1000, 100, 0);
        }
        let (low, low_ms) = count(2);
        drop(source);
        assert!(
            (27..=32).contains(&full),
            "expected about 30 fps, got {full} (hardware: {hardware})"
        );
        assert_eq!(full_ms, 33);
        assert!(
            (22..=26).contains(&low),
            "expected about 24 fps, got {low} (hardware: {hardware})"
        );
        assert_eq!(low_ms, 41);
    }

    /// What either encoder sends is what a receiver keeps: no SPS it would
    /// edit (which breaks end-to-end encryption), no filler, and keyframes
    /// only when asked for.
    #[test]
    fn what_is_sent_needs_no_editing_by_receivers() {
        gst::init().unwrap();
        // A small picture that hardly changes, and a large busy one.
        for (width, height, pattern) in [
            (1280, 720, "ball"),
            (1920, 1080, "zone-plate kx2=20 ky2=20 kt=3"),
        ] {
            sends_clean_frames(false, width, height, pattern);
            if gst::ElementFactory::find("vah264enc").is_some() {
                sends_clean_frames(true, width, height, pattern);
            }
        }
    }

    fn sends_clean_frames(hardware: bool, width: u32, height: u32, pattern: &str) {
        let settings = VideoSettings {
            width,
            height,
            fps: 30,
            bitrate_kbps: 8000,
            keyframe_secs: 0,
            adaptive_bitrate: false,
            ..VideoSettings::default()
        };
        let description = screen_description(&settings, hardware).replace(
            "pipewiresrc name=src do-timestamp=true keepalive-time=1000 resend-last=true",
            &format!("videotestsrc is-live=true pattern={pattern} name=src"),
        );
        let frames = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let seen = frames.clone();
        let source = VideoSource::start_pipeline_with(
            &description,
            33,
            move |frame: EncodedFrame| seen.lock().unwrap().push(frame.data),
            |_| Ok(()),
        )
        .unwrap();
        let keyframes = |frames: &[Vec<u8>]| frames.iter().filter(|f| is_keyframe(f)).count();
        std::thread::sleep(Duration::from_secs(3));
        let before = frames.lock().unwrap().clone();
        source.force_keyframe();
        std::thread::sleep(Duration::from_secs(1));
        let asked = frames.lock().unwrap().clone();
        source.set_bitrate(settings.bitrate_kbps / 2);
        std::thread::sleep(Duration::from_secs(2));
        let after = frames.lock().unwrap().clone();
        drop(source);

        assert!(
            before.len() > 60,
            "{} frames (hardware: {hardware})",
            before.len()
        );
        for frame in &after {
            // More slices than this and the frame cannot be encrypted, so
            // it is never sent.
            let slices = kc_voice::rtp::split_nalus(frame)
                .iter()
                .filter(|unit| matches!(unit[0] & 0x1F, 1 | 5))
                .count();
            assert!(
                slices <= kc_voice::dave::MAX_VIDEO_SLICES,
                "{slices} slices in a {height}p frame (hardware: {hardware})"
            );
            assert!(
                kc_voice::dave::video_frame_fits(frame),
                "a {height}p frame of {} bytes cannot be encrypted (hardware: {hardware})",
                frame.len()
            );
            for unit in kc_voice::rtp::split_nalus(frame) {
                let kind = unit[0] & 0x1F;
                assert_ne!(kind, 12, "filler was sent (hardware: {hardware})");
                assert!(
                    kind != 7 || !crate::h264::receivers_rewrite(unit),
                    "receivers would edit this SPS (hardware: {hardware})"
                );
            }
        }
        // One to start with and no more until one is asked for.
        assert_eq!(keyframes(&before), 1, "hardware: {hardware}");
        assert_eq!(keyframes(&asked), 2, "hardware: {hardware}");
        // A new bitrate costs the hardware encoder a keyframe and x264 none,
        // which is why the hardware encoder's changes are spaced out.
        assert_eq!(keyframes(&after) - 2, usize::from(hardware));
    }

    /// Loss report after loss report, x264 follows each with a lower
    /// bitrate; the hardware encoder takes the first and holds, as every
    /// change costs it a keyframe.
    #[test]
    fn a_hardware_encoder_changes_bitrate_less_often() {
        gst::init().unwrap();
        let steps = |hardware: bool| {
            let settings = VideoSettings {
                fps: 30,
                bitrate_kbps: 4000,
                adaptive_bitrate: true,
                ..VideoSettings::default()
            };
            let description = screen_description(&settings, hardware).replace(
                "pipewiresrc name=src do-timestamp=true keepalive-time=1000 resend-last=true",
                "videotestsrc is-live=true name=src",
            );
            let source = VideoSource::start_pipeline_with(
                &description,
                33,
                |_: EncodedFrame| {},
                |_| Ok(()),
            )
            .unwrap();
            source.adapt(&settings);
            let mut seen = vec![4000];
            for _ in 0..3 {
                source.network_report(1000, 100, 0);
                let kbps = source.adapted().0.unwrap();
                if seen.last() != Some(&kbps) {
                    seen.push(kbps);
                }
            }
            seen.len() - 1
        };
        assert_eq!(steps(false), 3);
        if gst::ElementFactory::find("vah264enc").is_some() {
            assert_eq!(steps(true), 1);
        }
    }

    /// A real PipeWire capture (of a throwaway test source, never a screen
    /// or camera) keeps running through a change of framerate. Needs a
    /// PipeWire session, so it only runs when asked for:
    /// `cargo test -p kc-media pipewire_capture -- --ignored`.
    #[test]
    #[ignore]
    fn pipewire_capture_survives_a_framerate_change() {
        gst::init().unwrap();
        // The test source, as a child process stopped by its own handle.
        let mut provider = std::process::Command::new("gst-launch-1.0")
            .args([
                "-q",
                "videotestsrc",
                "is-live=true",
                "!",
                "video/x-raw,format=BGRx,width=640,height=360,framerate=30/1",
                "!",
                "pipewiresink",
                "mode=provide",
                "stream-properties=props,node.name=kubecord-test-video,media.class=Video/Source",
            ])
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let dump = std::process::Command::new("pw-dump").output().unwrap();
        let graph: serde_json::Value = serde_json::from_slice(&dump.stdout).unwrap();
        let node = graph
            .as_array()
            .unwrap()
            .iter()
            .find(|object| object["info"]["props"]["node.name"] == "kubecord-test-video")
            .and_then(|object| object["id"].as_u64());
        let outcome = node.map(|node| {
            let settings = VideoSettings {
                encoder: EncoderPreference::Cpu,
                width: 640,
                height: 360,
                fps: 30,
                fps_target: 24,
                fps_floor: 10,
                adaptive_bitrate: false,
                ..VideoSettings::default()
            };
            let description = screen_description(&settings, false).replace(
                "pipewiresrc name=src",
                &format!("pipewiresrc name=src path={node}"),
            );
            let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let seen = frames.clone();
            let source = VideoSource::start_pipeline_with(
                &description,
                33,
                move |_: EncodedFrame| {
                    seen.fetch_add(1, Ordering::Relaxed);
                },
                |_| Ok(()),
            )
            .unwrap();
            source.adapt(&settings);
            std::thread::sleep(Duration::from_secs(2));
            let before = frames.swap(0, Ordering::Relaxed);
            source.network_report(1000, 100, 0);
            std::thread::sleep(Duration::from_millis(1000));
            frames.store(0, Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(2));
            let after = frames.load(Ordering::Relaxed);
            let failure = source.failure.lock().unwrap().clone();
            drop(source);
            (before, after, failure)
        });
        let _ = provider.kill();
        let _ = provider.wait();
        let (before, after, failure) = outcome.expect("the test source did not appear");
        println!("frames in 2 s before the change: {before}, after: {after}, error: {failure:?}");
        // About 48 at the target of 24, then about 40 a step down (20).
        assert!(
            (42..=52).contains(&before),
            "capture never ran at its target: {before}"
        );
        assert!(
            after > 30,
            "capture died at the change: {after} frames, {failure:?}"
        );
    }

    /// A capture that stops producing frames can be told from one that
    /// has not started and one that is running.
    #[test]
    fn a_capture_gone_quiet_is_noticed() {
        gst::init().unwrap();
        let settings = VideoSettings {
            encoder: EncoderPreference::Cpu,
            ..VideoSettings::default()
        };
        // Thirty frames, then nothing more.
        let description = screen_description(&settings, false).replace(
            "pipewiresrc name=src do-timestamp=true keepalive-time=1000 resend-last=true",
            "videotestsrc is-live=true num-buffers=30 name=src",
        );
        let source =
            VideoSource::start_pipeline_with(&description, 33, |_: EncodedFrame| {}, |_| Ok(()))
                .unwrap();
        assert_eq!(source.quiet_for(), None);
        std::thread::sleep(Duration::from_millis(600));
        let running = source.quiet_for().expect("frames by now");
        assert!(running < Duration::from_millis(300), "{running:?}");
        std::thread::sleep(Duration::from_millis(2000));
        let stopped = source.quiet_for().expect("frames by now");
        assert!(stopped > Duration::from_millis(1200), "{stopped:?}");
    }

    /// The encoder's bitrate can be changed while it runs.
    #[test]
    fn the_bitrate_changes_while_encoding() {
        gst::init().unwrap();
        let settings = VideoSettings {
            encoder: EncoderPreference::Cpu,
            bitrate_kbps: 3000,
            ..VideoSettings::default()
        };
        // Noise does not compress, so the output follows the bitrate.
        let description = screen_description(&settings, false).replace(
            "pipewiresrc name=src do-timestamp=true keepalive-time=1000 resend-last=true",
            "videotestsrc is-live=true pattern=snow name=src",
        );
        let bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = bytes.clone();
        let source = VideoSource::start_pipeline_with(
            &description,
            33,
            move |frame: EncodedFrame| {
                counted.fetch_add(frame.data.len(), Ordering::Relaxed);
            },
            |_| Ok(()),
        )
        .unwrap();
        let measure = |seconds: u64| {
            std::thread::sleep(Duration::from_millis(700));
            bytes.store(0, Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(seconds));
            bytes.load(Ordering::Relaxed) * 8 / 1000 / seconds as usize
        };
        let high = measure(2);
        source.set_bitrate(600);
        let low = measure(2);
        drop(source);
        assert!(high > 2000, "expected about 3000 kbit/s, got {high}");
        assert!(
            low < 1000,
            "expected about 600 kbit/s after the change, got {low}"
        );
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
