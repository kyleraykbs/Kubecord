//! What happens to the microphone before it is sent: noise suppression,
//! echo cancellation and automatic gain (GStreamer's `webrtcdsp` and
//! `audiornnoise`), and the input sensitivity gate that decides which
//! frames are sent at all.
//!
//! Like the devices, the settings are process-global: the UI sets them and
//! the microphone capture follows them while it runs. A change to the
//! processing chain rebuilds the capture pipeline within a frame or two; a
//! change to the sensitivity applies to the next frame.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use gstreamer as gst;
use parking_lot::RwLock;

/// How strongly background noise is removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NoiseSuppression {
    #[default]
    Off,
    /// WebRTC's suppressor (`webrtcdsp`): light, keeps the voice natural.
    Standard,
    /// RNNoise (`audiornnoise`): a small neural network; removes more,
    /// keyboard and fans included.
    Strong,
}

/// The microphone settings that are not the device or the volume.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoiceProcessing {
    pub noise_suppression: NoiseSuppression,
    pub echo_cancellation: bool,
    pub auto_gain: bool,
    /// Let kubecord find the level of the room and send only what rises
    /// above it; when off, `threshold_db` decides.
    pub auto_sensitivity: bool,
    /// Frames quieter than this (dBFS, RMS) are not sent.
    pub threshold_db: f32,
}

impl Default for VoiceProcessing {
    fn default() -> Self {
        Self {
            noise_suppression: NoiseSuppression::Off,
            echo_cancellation: false,
            auto_gain: false,
            auto_sensitivity: true,
            threshold_db: DEFAULT_THRESHOLD_DB,
        }
    }
}

/// The manual sensitivity's starting point: under normal speech, over a
/// quiet room.
pub const DEFAULT_THRESHOLD_DB: f32 = -50.0;
/// The quietest level reported; digital silence reads as this.
pub const FLOOR_DB: f32 = -100.0;

impl VoiceProcessing {
    /// Whether two settings need different capture pipelines.
    fn same_chain(&self, other: &Self) -> bool {
        self.noise_suppression == other.noise_suppression
            && self.echo_cancellation == other.echo_cancellation
            && self.auto_gain == other.auto_gain
    }

    pub fn sensitivity(&self) -> Sensitivity {
        if self.auto_sensitivity {
            Sensitivity::Automatic
        } else {
            Sensitivity::Manual(self.threshold_db)
        }
    }
}

static SETTINGS: RwLock<VoiceProcessing> = RwLock::new(VoiceProcessing {
    noise_suppression: NoiseSuppression::Off,
    echo_cancellation: false,
    auto_gain: false,
    auto_sensitivity: true,
    threshold_db: DEFAULT_THRESHOLD_DB,
});
/// Bumped whenever the microphone pipeline must be rebuilt.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Sets the processing; a running microphone follows.
pub fn set(processing: VoiceProcessing) {
    let mut current = SETTINGS.write();
    if !current.same_chain(&processing) {
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
    *current = processing;
}

pub fn current() -> VoiceProcessing {
    *SETTINGS.read()
}

/// Changes whenever a running microphone capture should rebuild itself:
/// the processing chain or the input device changed, or echo cancellation
/// gained or lost the playback it listens to.
pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

pub(crate) fn rebuild() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

// -- echo probe -----------------------------------------------------------

/// The name of the `webrtcechoprobe` in call playback. Echo cancellation
/// needs to hear what the speakers play; `webrtcdsp` finds the probe by
/// name, in any pipeline of the process.
pub(crate) const ECHO_PROBE: &str = "kcechoprobe";
/// How many playback pipelines carry the probe right now.
static ECHO_PROBES: AtomicUsize = AtomicUsize::new(0);

/// Marks a playback pipeline with the echo probe for as long as it lives.
pub(crate) struct EchoProbeGuard;

impl EchoProbeGuard {
    pub(crate) fn new() -> Self {
        if ECHO_PROBES.fetch_add(1, Ordering::Relaxed) == 0 {
            // A microphone running without the probe can now cancel echo.
            rebuild();
        }
        Self
    }
}

impl Drop for EchoProbeGuard {
    fn drop(&mut self) {
        if ECHO_PROBES.fetch_sub(1, Ordering::Relaxed) == 1 {
            rebuild();
        }
    }
}

/// Whether the echo probe element can be made (gst-plugins-bad's webrtc
/// DSP plugin is installed).
pub(crate) fn echo_probe_available() -> bool {
    gst::init().is_ok() && gst::ElementFactory::find("webrtcechoprobe").is_some()
}

// -- pipeline -------------------------------------------------------------

/// The capture format every microphone frame has.
pub(crate) const CAPTURE_CAPS: &str =
    "audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved";

/// The launch line between the capture caps and the appsink: the
/// processing elements, or nothing. `probe` says whether a playback echo
/// probe exists; without one, echo cancellation is left out (webrtcdsp
/// refuses to start without its probe). Elements that are not installed
/// are left out too, with a warning.
pub(crate) fn chain(
    processing: &VoiceProcessing,
    probe: bool,
    available: &dyn Fn(&str) -> bool,
) -> String {
    let mut chain = String::new();
    let echo = processing.echo_cancellation && probe;
    let webrtc_noise = processing.noise_suppression == NoiseSuppression::Standard;
    if (echo || webrtc_noise || processing.auto_gain) && available("webrtcdsp") {
        chain.push_str(&format!(
            " ! webrtcdsp name=dsp probe={ECHO_PROBE} echo-cancel={echo} \
             noise-suppression={webrtc_noise} gain-control={} high-pass-filter=true \
             ! audioconvert ! {CAPTURE_CAPS}",
            processing.auto_gain
        ));
    } else if echo || webrtc_noise || processing.auto_gain {
        tracing::warn!("webrtcdsp is not installed; microphone processing is off");
    }
    if processing.noise_suppression == NoiseSuppression::Strong {
        if available("audiornnoise") {
            chain.push_str(&format!(
                " ! audioconvert ! audiornnoise ! audioconvert ! {CAPTURE_CAPS}"
            ));
        } else {
            tracing::warn!("audiornnoise is not installed; strong noise suppression is off");
        }
    }
    chain
}

pub(crate) fn element_available(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

pub(crate) fn probe_running() -> bool {
    ECHO_PROBES.load(Ordering::Relaxed) > 0
}

// -- level and gate -------------------------------------------------------

/// The RMS level of interleaved samples in dBFS, at least [`FLOOR_DB`].
pub fn level_db(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return FLOOR_DB;
    }
    let energy: f64 = samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum();
    let rms = (energy / samples.len() as f64).sqrt() / 32_768.0;
    if rms <= 0.0 {
        return FLOOR_DB;
    }
    ((20.0 * rms.log10()) as f32).max(FLOOR_DB)
}

/// What decides whether the microphone is sent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sensitivity {
    /// Above the room's own noise, as kubecord measures it.
    Automatic,
    /// At or above this level (dBFS).
    Manual(f32),
}

/// How long the gate stays open after the last loud frame, in 20 ms
/// frames: 400 ms, so the ends of words and short pauses are not cut.
pub const HOLD_FRAMES: u32 = 20;
/// In automatic mode, how far above the room's noise speech must be.
const AUTO_MARGIN_DB: f32 = 12.0;
/// In automatic mode, nothing quieter than this counts as speech.
const AUTO_MIN_DB: f32 = -60.0;
/// The measured noise level rises this much per quiet frame (1.5 dB a
/// second), so a louder room is learned slowly and speech is not mistaken
/// for it.
const NOISE_RISE_DB: f32 = 0.03;

/// What to do with one microphone frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateAction {
    /// Send it.
    Send,
    /// Hold it back; nothing was being sent.
    Skip,
    /// Hold it back, and end the speech that was being sent.
    EndSpeech,
}

/// The input sensitivity gate. It opens on the first frame loud enough
/// and closes [`HOLD_FRAMES`] after the last one.
#[derive(Clone, Debug)]
pub struct VoiceGate {
    open: bool,
    hold: u32,
    /// The room's noise level (automatic mode).
    noise_db: f32,
}

impl Default for VoiceGate {
    /// Starts open, as a call starts with speaking announced: the first
    /// quiet stretch then ends it properly.
    fn default() -> Self {
        Self {
            open: true,
            hold: HOLD_FRAMES,
            noise_db: AUTO_MIN_DB - AUTO_MARGIN_DB,
        }
    }
}

impl VoiceGate {
    /// Whether frames are being sent.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The level automatic mode currently needs, in dBFS.
    pub fn automatic_threshold(&self) -> f32 {
        (self.noise_db + AUTO_MARGIN_DB).max(AUTO_MIN_DB)
    }

    /// Whether a frame at `level` counts as speech.
    fn loud(&mut self, level: f32, sensitivity: Sensitivity) -> bool {
        match sensitivity {
            Sensitivity::Manual(threshold) => level >= threshold,
            Sensitivity::Automatic => {
                let loud = level >= self.automatic_threshold();
                // The room is the quietest it has been lately: drop to a
                // quieter level at once, rise slowly. Speech has pauses at
                // the room's level, so it cannot pull this up for long.
                if level < self.noise_db {
                    self.noise_db = level.max(FLOOR_DB);
                } else {
                    self.noise_db = (self.noise_db + NOISE_RISE_DB).min(level);
                }
                loud
            }
        }
    }

    /// Feeds one frame's level; `enabled` is false while muted.
    pub fn feed(&mut self, level: f32, sensitivity: Sensitivity, enabled: bool) -> GateAction {
        let loud = self.loud(level, sensitivity);
        if !enabled {
            self.hold = 0;
            return self.close();
        }
        if loud {
            self.open = true;
            self.hold = HOLD_FRAMES;
            return GateAction::Send;
        }
        if self.open && self.hold > 0 {
            self.hold -= 1;
            return GateAction::Send;
        }
        self.close()
    }

    fn close(&mut self) -> GateAction {
        if std::mem::replace(&mut self.open, false) {
            GateAction::EndSpeech
        } else {
            GateAction::Skip
        }
    }
}

// -- meter ----------------------------------------------------------------

static METER_LEVEL: AtomicU32 = AtomicU32::new(0);
static METER_OPEN: AtomicBool = AtomicBool::new(false);
static METER_THRESHOLD: AtomicU32 = AtomicU32::new(0);
/// Milliseconds since [`METER_EPOCH`] of the last report, plus one (0:
/// never).
static METER_AT: AtomicU64 = AtomicU64::new(0);
static METER_EPOCH: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

/// The microphone's latest level, for the settings meter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputLevel {
    pub level_db: f32,
    /// Whether that frame was sent (or, in the mic test, played).
    pub open: bool,
    /// The level the gate needs right now.
    pub threshold_db: f32,
}

pub(crate) fn report_level(level_db: f32, open: bool, threshold_db: f32) {
    METER_LEVEL.store(level_db.to_bits(), Ordering::Relaxed);
    METER_OPEN.store(open, Ordering::Relaxed);
    METER_THRESHOLD.store(threshold_db.to_bits(), Ordering::Relaxed);
    METER_AT.store(
        METER_EPOCH.elapsed().as_millis() as u64 + 1,
        Ordering::Relaxed,
    );
}

/// The microphone's level if a call or the mic test reported one in the
/// last half second.
pub fn input_level() -> Option<InputLevel> {
    let at = METER_AT.load(Ordering::Relaxed);
    if at == 0 || (METER_EPOCH.elapsed().as_millis() as u64 + 1).saturating_sub(at) > 500 {
        return None;
    }
    Some(InputLevel {
        level_db: f32::from_bits(METER_LEVEL.load(Ordering::Relaxed)),
        open: METER_OPEN.load(Ordering::Relaxed),
        threshold_db: f32::from_bits(METER_THRESHOLD.load(Ordering::Relaxed)),
    })
}

/// The gate's threshold to show for `sensitivity`.
pub(crate) fn shown_threshold(gate: &VoiceGate, sensitivity: Sensitivity) -> f32 {
    match sensitivity {
        Sensitivity::Manual(threshold) => threshold,
        Sensitivity::Automatic => gate.automatic_threshold(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(amplitude: i16) -> Vec<i16> {
        (0..1920)
            .map(|index| {
                if index % 2 == 0 {
                    amplitude
                } else {
                    -amplitude
                }
            })
            .collect()
    }

    #[test]
    fn levels_are_rms_in_dbfs() {
        assert_eq!(level_db(&[0; 1920]), FLOOR_DB);
        assert!((level_db(&tone(32_767)) - 0.0).abs() < 0.01);
        assert!((level_db(&tone(3277)) + 20.0).abs() < 0.05);
        assert!((level_db(&tone(328)) + 40.0).abs() < 0.1);
    }

    #[test]
    fn manual_gate_opens_at_once_and_holds_through_word_endings() {
        let mut gate = VoiceGate::default();
        let manual = Sensitivity::Manual(-40.0);
        // It starts open and closes after the hold.
        for _ in 0..HOLD_FRAMES {
            assert_eq!(gate.feed(-70.0, manual, true), GateAction::Send);
        }
        assert_eq!(gate.feed(-70.0, manual, true), GateAction::EndSpeech);
        assert_eq!(gate.feed(-70.0, manual, true), GateAction::Skip);
        // Speech opens it on its first frame.
        assert_eq!(gate.feed(-30.0, manual, true), GateAction::Send);
        assert!(gate.is_open());
        // A pause shorter than the hold keeps it open.
        for _ in 0..HOLD_FRAMES - 1 {
            assert_eq!(gate.feed(-55.0, manual, true), GateAction::Send);
        }
        assert_eq!(gate.feed(-35.0, manual, true), GateAction::Send);
        for _ in 0..HOLD_FRAMES {
            assert_eq!(gate.feed(-55.0, manual, true), GateAction::Send);
        }
        assert_eq!(gate.feed(-55.0, manual, true), GateAction::EndSpeech);
        // Exactly at the threshold counts as speech.
        assert_eq!(gate.feed(-40.0, manual, true), GateAction::Send);
    }

    #[test]
    fn muting_ends_speech_at_once() {
        let mut gate = VoiceGate::default();
        let manual = Sensitivity::Manual(-40.0);
        assert_eq!(gate.feed(-20.0, manual, true), GateAction::Send);
        assert_eq!(gate.feed(-20.0, manual, false), GateAction::EndSpeech);
        assert_eq!(gate.feed(-20.0, manual, false), GateAction::Skip);
        // Unmuted while talking: sent again straight away.
        assert_eq!(gate.feed(-20.0, manual, true), GateAction::Send);
    }

    #[test]
    fn automatic_gate_learns_the_room() {
        let mut gate = VoiceGate::default();
        let auto = Sensitivity::Automatic;
        // A noisy room at -45 dB: after a while the noise alone is not
        // sent.
        let mut last = GateAction::Send;
        for _ in 0..2000 {
            last = gate.feed(-45.0, auto, true);
        }
        assert_eq!(last, GateAction::Skip);
        assert!((gate.automatic_threshold() - (-33.0)).abs() < 0.5);
        // Speech well above it is sent.
        assert_eq!(gate.feed(-20.0, auto, true), GateAction::Send);
        // A quieter room is learned at once.
        for _ in 0..HOLD_FRAMES + 1 {
            gate.feed(-80.0, auto, true);
        }
        assert_eq!(gate.automatic_threshold(), AUTO_MIN_DB);
        // Low-level hiss under the minimum never opens it.
        assert_eq!(gate.feed(-65.0, auto, true), GateAction::Skip);
        // Ten seconds of unbroken speech raise the estimate a little, and
        // the next pause puts it back.
        for _ in 0..500 {
            assert_eq!(gate.feed(-20.0, auto, true), GateAction::Send);
        }
        assert!(gate.automatic_threshold() < -50.0);
        gate.feed(-80.0, auto, true);
        assert_eq!(gate.automatic_threshold(), AUTO_MIN_DB);
    }

    #[test]
    fn chain_holds_only_what_is_asked_for_and_installed() {
        let everything = |_: &str| true;
        let off = VoiceProcessing::default();
        assert_eq!(chain(&off, true, &everything), "");

        let standard = VoiceProcessing {
            noise_suppression: NoiseSuppression::Standard,
            ..off
        };
        let line = chain(&standard, true, &everything);
        assert!(line.contains("webrtcdsp"));
        assert!(line.contains("noise-suppression=true"));
        assert!(line.contains("echo-cancel=false"));
        assert!(!line.contains("audiornnoise"));

        // Echo cancellation needs a playback probe to listen to.
        let echo = VoiceProcessing {
            echo_cancellation: true,
            ..off
        };
        assert!(chain(&echo, true, &everything).contains("echo-cancel=true"));
        assert_eq!(chain(&echo, false, &everything), "");

        let strong = VoiceProcessing {
            noise_suppression: NoiseSuppression::Strong,
            auto_gain: true,
            ..off
        };
        let line = chain(&strong, false, &everything);
        assert!(line.contains("audiornnoise"));
        assert!(line.contains("gain-control=true"));
        assert!(line.contains("noise-suppression=false"));

        // Missing plugins are left out rather than failing the capture.
        assert_eq!(chain(&strong, false, &|_: &str| false), "");
    }

    #[test]
    fn only_chain_changes_rebuild_the_capture() {
        let saved = current();
        let before = generation();
        set(VoiceProcessing {
            threshold_db: -30.0,
            auto_sensitivity: false,
            ..current()
        });
        assert_eq!(generation(), before);
        set(VoiceProcessing {
            auto_gain: !current().auto_gain,
            ..current()
        });
        assert!(generation() > before);
        set(saved);
    }
}
