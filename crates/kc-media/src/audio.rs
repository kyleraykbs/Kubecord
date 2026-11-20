//! Voice audio: mic capture and Opus encode. Receiving is in [`crate::receive`].

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use gstreamer as gst;
use gstreamer_app::prelude::*;
use opus2::{Application, Channels, Decoder, Encoder};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::debug;

use crate::delay::DelayedSender;
use crate::error::{Error, Result};
use crate::processing::{CAPTURE_CAPS, GateAction, VoiceGate};
use crate::receive::{ReceiveStats, RemoteAudio};
use crate::share_audio::{AudioShare, Linker, VirtualOutput};

use kc_voice::VoiceHandle;

const SAMPLE_RATE: u32 = 48_000;
const FRAME_SAMPLES: usize = 960; // 20 ms per channel
const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * 2;
const MAX_OPUS_FRAME: usize = 5760;

/// Builds a capture pipeline's launch line; `true` asks for the
/// microphone processing, `false` for the plain microphone.
type LineBuilder = Box<dyn Fn(bool) -> String + Send>;

/// Captures the default input device as 48 kHz stereo S16, in 20 ms frames.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Whether the microphone processing is running (false while it is
    /// off, failed, or this is not a microphone).
    processed: Arc<AtomicBool>,
}

/// The microphone's launch line: the chosen device, then the processing
/// (see [`crate::processing`]) unless `processed` is false.
fn microphone_line(processed: bool) -> String {
    // The chosen microphone, or the system default.
    let source = match crate::devices::input_device() {
        Some(device) => format!("pulsesrc device=\"{device}\""),
        None => "autoaudiosrc".to_owned(),
    };
    let chain = if processed {
        crate::processing::chain(
            &crate::processing::current(),
            crate::processing::probe_running(),
            &crate::processing::element_available,
        )
    } else {
        String::new()
    };
    format!(
        "{source} ! audioconvert ! audioresample ! {CAPTURE_CAPS}{chain} \
         ! appsink name=sink sync=false max-buffers=8 drop=true"
    )
}

impl Capture {
    /// Starts the microphone. It follows the processing settings and the
    /// chosen device while it runs, rebuilding its pipeline when they
    /// change.
    pub fn start(on_frame: impl FnMut(Vec<i16>) + Send + 'static) -> Result<Self> {
        Self::start_rebuilding(Box::new(microphone_line), on_frame)
    }

    /// Starts the pipeline `line` makes, and makes it again whenever
    /// [`crate::processing::generation`] changes. If the processed
    /// pipeline cannot start or fails, it falls back to the plain one until
    /// the settings change again.
    fn start_rebuilding(
        line: LineBuilder,
        on_frame: impl FnMut(Vec<i16>) + Send + 'static,
    ) -> Result<Self> {
        let generation = crate::processing::generation();
        let (pipeline, processed) = match launch(&line(true), |_| {}) {
            Ok(pipeline) => (pipeline, true),
            Err(error) => {
                tracing::warn!(%error, "microphone processing could not start; capturing without it");
                (launch(&line(false), |_| {})?, false)
            }
        };
        let shown = Arc::new(AtomicBool::new(processed));
        Ok(Self::run(
            pipeline,
            Some(Rebuild {
                line,
                generation,
                processed,
                shown: shown.clone(),
            }),
            shown,
            on_frame,
        ))
    }

    /// Whether the processed pipeline is the one running.
    pub fn is_processed(&self) -> bool {
        self.processed.load(Ordering::Relaxed)
    }

    /// Captures the default output's monitor, for Go Live system audio. The
    /// ScreenCast portal does not carry audio; this is the Linux equivalent of
    /// sharing desktop audio. It intentionally uses the PulseAudio/PipeWire
    /// monitor device rather than the default input.
    pub fn start_monitor(on_frame: impl FnMut(Vec<i16>) + Send + 'static) -> Result<Self> {
        let device =
            std::env::var("KYLECORD_MONITOR_DEVICE").unwrap_or_else(|_| "@DEFAULT_MONITOR@".into());
        let pipeline = format!(
            "pulsesrc device=\"{device}\" ! audioconvert ! audioresample \
             ! audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved \
             ! appsink name=sink sync=false max-buffers=8 drop=true"
        );
        Self::start_with_source(&pipeline, on_frame)
    }

    /// Captures what is played into the output named `node_name` (its
    /// monitor), such as kylecord's own [`VirtualOutput`].
    pub fn start_monitor_of(
        node_name: &str,
        on_frame: impl FnMut(Vec<i16>) + Send + 'static,
    ) -> Result<Self> {
        let pipeline = format!(
            "pulsesrc device=\"{node_name}.monitor\" client-name=kylecord \
             ! audioconvert ! audioresample \
             ! audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved \
             ! appsink name=sink sync=false max-buffers=8 drop=true"
        );
        Self::start_with_source(&pipeline, on_frame)
    }

    fn start_with_source(
        source: &str,
        on_frame: impl FnMut(Vec<i16>) + Send + 'static,
    ) -> Result<Self> {
        Self::start_configured(source, on_frame, |_| {})
    }

    /// Starts `source`, letting `configure` set what a launch line cannot
    /// before it plays.
    fn start_configured(
        source: &str,
        on_frame: impl FnMut(Vec<i16>) + Send + 'static,
        configure: impl FnOnce(&gst::Pipeline),
    ) -> Result<Self> {
        let pipeline = launch(source, configure)?;
        Ok(Self::run(
            pipeline,
            None,
            Arc::new(AtomicBool::new(false)),
            on_frame,
        ))
    }

    fn run(
        running: Running,
        mut rebuild: Option<Rebuild>,
        processed: Arc<AtomicBool>,
        mut on_frame: impl FnMut(Vec<i16>) + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut running = running;
            let mut pending: Vec<i16> = Vec::with_capacity(FRAME_INTERLEAVED * 2);
            while !thread_stop.load(Ordering::Relaxed) {
                if let Some(rebuild) = &mut rebuild
                    && let Some(replacement) = rebuild.check(&running)
                {
                    running = replacement;
                    pending.clear();
                }
                let Some(sample) = running
                    .appsink
                    .try_pull_sample(gst::ClockTime::from_mseconds(20))
                else {
                    continue;
                };
                let Some(buffer) = sample.buffer() else {
                    continue;
                };
                let Ok(map) = buffer.map_readable() else {
                    continue;
                };
                let bytes = map.as_slice();
                for chunk in bytes.as_chunks::<2>().0 {
                    pending.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                }
                while pending.len() >= FRAME_INTERLEAVED {
                    let frame: Vec<i16> = pending.drain(..FRAME_INTERLEAVED).collect();
                    on_frame(frame);
                }
            }
            let _ = running.pipeline.set_state(gst::State::Null);
        });
        Self {
            stop,
            thread: Some(thread),
            processed,
        }
    }
}

/// A playing capture pipeline and its sink.
struct Running {
    pipeline: gst::Pipeline,
    appsink: gstreamer_app::AppSink,
}

/// Parses `line`, lets `configure` adjust it, and starts it.
fn launch(line: &str, configure: impl FnOnce(&gst::Pipeline)) -> Result<Running> {
    gst::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))?;
    let element = gst::parse::launch(line)
        .map_err(|error| Error::Message(format!("capture pipeline: {error}")))?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| Error::Message("capture pipeline is not a pipeline".into()))?;
    configure(&pipeline);
    let appsink = pipeline
        .by_name("sink")
        .and_then(|element| element.downcast::<gstreamer_app::AppSink>().ok())
        .ok_or_else(|| Error::Message("capture appsink missing".into()))?;
    appsink.set_sync(false);
    appsink.set_drop(true);
    if let Err(error) = pipeline.set_state(gst::State::Playing) {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(Error::Message(format!("capture state: {error}")));
    }
    Ok(Running { pipeline, appsink })
}

/// How a microphone capture remakes itself when its settings change.
struct Rebuild {
    line: LineBuilder,
    /// The settings generation the running pipeline was made for.
    generation: u64,
    /// Whether it runs with the processing (false after a failure).
    processed: bool,
    /// The same, for [`Capture::is_processed`].
    shown: Arc<AtomicBool>,
}

impl Rebuild {
    /// A new pipeline if the settings changed or the processed one
    /// failed; the old one is stopped first.
    fn check(&mut self, running: &Running) -> Option<Running> {
        let generation = crate::processing::generation();
        let failed = self.processed
            && running.pipeline.bus().is_some_and(|bus| {
                bus.pop_filtered(&[gst::MessageType::Error])
                    .is_some_and(|message| {
                        if let gst::MessageView::Error(error) = message.view() {
                            tracing::warn!(error = %error.error(), debug = ?error.debug(), "microphone processing failed; capturing without it");
                        }
                        true
                    })
            });
        if generation == self.generation && !failed {
            return None;
        }
        self.generation = generation;
        let _ = running.pipeline.set_state(gst::State::Null);
        let mut processed = !failed;
        let replacement = launch(&(self.line)(processed), |_| {}).or_else(|error| {
            tracing::warn!(%error, "microphone processing could not start; capturing without it");
            processed = false;
            launch(&(self.line)(false), |_| {})
        });
        match replacement {
            Ok(replacement) => {
                tracing::info!(processed, "microphone pipeline rebuilt");
                self.processed = processed;
                self.shown.store(processed, Ordering::Relaxed);
                Some(replacement)
            }
            Err(error) => {
                // Keep what ran before rather than going silent.
                tracing::warn!(%error, "microphone pipeline could not be rebuilt");
                let _ = running.pipeline.set_state(gst::State::Playing);
                None
            }
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // The capture thread stops its pipeline on the way out.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Sound captured for a stream. Everything goes through a virtual output
/// of kylecord's own (`kylecord Go Live`), recorded from its monitor:
/// kylecord connects other programs into it, or a patchbay does.
pub struct SharedSound {
    // Dropped in this order: stop connecting, stop recording, then remove
    // the output.
    _linker: Option<Linker>,
    _capture: Capture,
    _output: Option<VirtualOutput>,
}

impl SharedSound {
    /// Starts capturing `share` in 20 ms stereo frames.
    pub fn start(
        share: &AudioShare,
        on_frame: impl FnMut(Vec<i16>) + Send + 'static,
    ) -> Result<Self> {
        if !crate::share_audio::tools_available() {
            return match share {
                // Desktop sound can still come from the default output's
                // monitor, kylecord's own sound included.
                AudioShare::Desktop => {
                    tracing::warn!(
                        "pw-cli, pw-dump or pw-link missing; sharing everything the default output plays"
                    );
                    Ok(Self {
                        _linker: None,
                        _capture: Capture::start_monitor(on_frame)?,
                        _output: None,
                    })
                }
                _ => Err(Error::Message(
                    "this needs PipeWire's pw-cli, pw-dump and pw-link".into(),
                )),
            };
        }
        let name = match share {
            // The same name every time, so a patchbay remembers it.
            AudioShare::Node => crate::share_audio::NODE_NAME.to_owned(),
            // A name of its own, so the linker finds this output and not
            // another kylecord's.
            _ => format!(
                "{}-{}-{}",
                crate::share_audio::NODE_NAME,
                std::process::id(),
                CAPTURE_COUNT.fetch_add(1, Ordering::Relaxed)
            ),
        };
        let output = VirtualOutput::create(&name)?;
        let capture = Capture::start_monitor_of(&name, on_frame)?;
        let linker = match share {
            AudioShare::Node => None,
            share => Some(Linker::start(name, share.clone())),
        };
        tracing::info!(?share, "sharing sound with the stream");
        Ok(Self {
            _linker: linker,
            _capture: capture,
            _output: Some(output),
        })
    }
}

/// Numbers each stream sound capture, for unique node names.
static CAPTURE_COUNT: AtomicU32 = AtomicU32::new(0);

/// System-audio capture for a Go Live stream connection. The stream's audio
/// SSRC is separate from the parent voice connection; Discord expects this on
/// the stream connection with the SOUNDSHARE speaking flag.
pub struct ScreenAudio {
    _sound: SharedSound,
    voice: VoiceHandle,
    /// Linear multiplier applied to each outgoing frame (`f32` bits), so
    /// viewers can be made to hear the shared sound louder without touching
    /// the user's own output. Read in the capture callback, so a change
    /// takes effect on the next frame. `1.0` leaves the sound unchanged.
    gain: Arc<AtomicU32>,
}

impl ScreenAudio {
    pub fn start(
        voice: VoiceHandle,
        bitrate: u32,
        delay_ms: u64,
        share: AudioShare,
        gain: f32,
    ) -> Result<Self> {
        let encoder = Arc::new(Mutex::new(
            Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Audio)
                .map_err(|error| Error::Message(format!("opus encoder: {error}")))?,
        ));
        if let Some(mut encoder) = encoder.try_lock() {
            let _ = encoder.set_bitrate(opus2::Bitrate::Bits(bitrate as i32));
            let _ = encoder.set_inband_fec(false);
        }

        voice.set_speaking(2); // SOUNDSHARE
        let send_voice = voice.clone();
        let delayed = DelayedSender::start(
            std::time::Duration::from_millis(delay_ms),
            move |packet, frame_ms| send_voice.send_soundshare_audio(packet, frame_ms),
        );
        let gain = Arc::new(AtomicU32::new(gain.max(0.0).to_bits()));
        let frame_gain = gain.clone();
        let on_frame = move |mut frame: Vec<i16>| {
            // Scale the viewers' copy only; this capture is a separate
            // monitor tap, so the user's own playback is untouched.
            crate::devices::apply_gain(
                &mut frame,
                f32::from_bits(frame_gain.load(Ordering::Relaxed)),
            );
            let mut encoder = encoder.lock();
            match encoder.encode_vec(&frame, 4000) {
                Ok(packet) => delayed.send(packet, 20),
                Err(error) => debug!(%error, "screen audio encode failed"),
            }
        };
        let sound = match SharedSound::start(&share, on_frame) {
            Ok(sound) => sound,
            Err(error) => {
                voice.set_speaking(0);
                return Err(error);
            }
        };
        Ok(Self {
            _sound: sound,
            voice,
            gain,
        })
    }

    /// Sets the outgoing gain (a linear multiplier, `1.0` = unchanged) while
    /// streaming. It applies to the next frame, without restarting.
    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.max(0.0).to_bits(), Ordering::Relaxed);
    }
}

impl Drop for ScreenAudio {
    fn drop(&mut self) {
        self.voice.set_speaking(0);
    }
}

/// Owns the local audio path for one voice connection: the microphone on its
/// way out, and everyone else on their way in.
pub struct VoiceAudio {
    remote: RemoteAudio,
    mic_enabled: Arc<AtomicBool>,
    _capture: Capture,
}

impl VoiceAudio {
    /// `on_speaking` is called, from the capture thread, when the local user
    /// starts or stops speaking.
    pub fn start(voice: VoiceHandle, on_speaking: impl Fn(bool) + Send + 'static) -> Result<Self> {
        let remote = RemoteAudio::start()?;
        let encoder = Arc::new(Mutex::new(
            Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Voip)
                .map_err(|error| Error::Message(format!("opus encoder: {error}")))?,
        ));
        if let Some(mut encoder) = encoder.try_lock() {
            let _ = encoder.set_bitrate(opus2::Bitrate::Bits(64_000));
            let _ = encoder.set_inband_fec(true);
            let _ = encoder.set_packet_loss_perc(10);
        }

        let mic_enabled = Arc::new(AtomicBool::new(true));
        let capture_enabled = mic_enabled.clone();
        let first_packet = Arc::new(AtomicBool::new(true));
        let mut gate = VoiceGate::default();
        // Whether the local ring shows speech.
        let mut speaking = false;
        let capture = Capture::start(move |mut frame| {
            crate::devices::apply_gain(&mut frame, crate::devices::input_gain());
            // The input sensitivity decides which frames go out. A pause
            // ends with silence frames and the speaking flag off, as the
            // official clients do; the next loud frame turns it on again.
            let level = crate::processing::level_db(&frame);
            let sensitivity = crate::processing::current().sensitivity();
            let enabled = capture_enabled.load(Ordering::Relaxed);
            let action = gate.feed(level, sensitivity, enabled);
            crate::processing::report_level(
                level,
                action == GateAction::Send,
                crate::processing::shown_threshold(&gate, sensitivity),
            );
            match action {
                GateAction::Send => {}
                GateAction::Skip => {
                    voice.skip_audio(20);
                    return;
                }
                GateAction::EndSpeech => {
                    voice.end_talk_burst(20);
                    if std::mem::take(&mut speaking) {
                        on_speaking(false);
                    }
                    return;
                }
            }
            if !std::mem::replace(&mut speaking, true) {
                on_speaking(true);
            }
            let mut encoder = encoder.lock();
            match encoder.encode_vec(&frame, 4000) {
                Ok(packet) => {
                    if first_packet.swap(false, Ordering::Relaxed) {
                        tracing::info!(bytes = packet.len(), "first microphone opus frame");
                    }
                    voice.send_audio(packet, 20)
                }
                Err(error) => debug!(%error, "opus encode failed"),
            }
        })?;

        Ok(Self {
            remote,
            mic_enabled,
            _capture: capture,
        })
    }

    pub fn set_volume(&self, user_id: u64, volume: f32) {
        self.remote.set_volume(user_id, volume);
    }

    pub fn set_master_volume(&self, volume: f32) {
        self.remote.set_master_volume(volume);
    }

    pub fn set_mic_enabled(&self, enabled: bool) {
        self.mic_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Deafen means no microphone and no playback; undeafen restores both.
    pub fn set_deafened(&self, deafened: bool) {
        self.mic_enabled.store(!deafened, Ordering::Relaxed);
        self.set_master_volume(if deafened { 0.0 } else { 1.0 });
    }

    /// Queues one Opus packet received from a participant.
    pub fn push_remote(&self, user_id: u64, sequence: u16, opus: &[u8]) {
        self.remote.push(user_id, sequence, opus);
    }

    /// Forgets a participant who left the call.
    pub fn remove_remote(&self, user_id: u64) {
        self.remote.remove(user_id);
    }

    /// What has happened to received packets so far.
    pub fn receive_stats(&self) -> ReceiveStats {
        self.remote.stats()
    }
}

/// Frames of microphone audio queued for the mic test's playback; more and
/// the loopback lags, so the oldest are dropped.
const MIC_TEST_QUEUE: usize = 4;

/// The settings' mic test: plays the microphone back on the chosen output,
/// with the configured volumes, and reports how loud it is.
pub struct MicTest {
    _capture: Capture,
    _output: crate::receive::Output,
    peak: Arc<AtomicU32>,
}

impl MicTest {
    pub fn start() -> Result<Self> {
        let queue = Arc::new(Mutex::new(VecDeque::<Vec<i16>>::new()));
        let peak = Arc::new(AtomicU32::new(0));
        let captured = queue.clone();
        let level = peak.clone();
        let mut gate = VoiceGate::default();
        let capture = Capture::start(move |mut frame| {
            crate::devices::apply_gain(&mut frame, crate::devices::input_gain());
            let loudest = frame.iter().map(|sample| sample.unsigned_abs()).max();
            level.fetch_max(u32::from(loudest.unwrap_or(0)), Ordering::Relaxed);
            // Play back what a call would send: below the input
            // sensitivity, silence.
            let db = crate::processing::level_db(&frame);
            let sensitivity = crate::processing::current().sensitivity();
            let action = gate.feed(db, sensitivity, true);
            crate::processing::report_level(
                db,
                action == GateAction::Send,
                crate::processing::shown_threshold(&gate, sensitivity),
            );
            if action != GateAction::Send {
                frame.fill(0);
            }
            let mut queue = captured.lock();
            if queue.len() >= MIC_TEST_QUEUE {
                queue.pop_front();
            }
            queue.push_back(frame);
        })?;
        let output = crate::receive::Output::start(move || {
            let mut frame = queue
                .lock()
                .pop_front()
                .unwrap_or_else(|| vec![0; FRAME_INTERLEAVED]);
            crate::devices::apply_gain(&mut frame, crate::devices::output_gain());
            frame
        })?;
        Ok(Self {
            _capture: capture,
            _output: output,
            peak,
        })
    }

    /// The loudest sample since the last call, from 0.0 to 1.0.
    pub fn take_peak(&self) -> f32 {
        self.peak.swap(0, Ordering::Relaxed) as f32 / 32_768.0
    }
}

/// Starts the microphone path on its own, for codec smoke tests.
pub fn capture_frames(on_frame: impl FnMut(Vec<i16>) + Send + 'static) -> Result<Capture> {
    Capture::start(on_frame)
}

/// Sends 20 ms Opus frames to a fresh encoder; useful for tests and tools.
pub struct OpusEncoder {
    inner: Encoder,
}

impl OpusEncoder {
    pub fn new() -> Result<Self> {
        let mut inner = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Voip)
            .map_err(|error| Error::Message(format!("opus encoder: {error}")))?;
        let _ = inner.set_bitrate(opus2::Bitrate::Bits(64_000));
        Ok(Self { inner })
    }

    pub fn encode(&mut self, samples: &[i16]) -> Result<Vec<u8>> {
        self.inner
            .encode_vec(samples, 4000)
            .map_err(|error| Error::Message(format!("opus encode: {error}")))
    }
}

/// Decodes one Opus packet to interleaved stereo S16.
pub fn decode_opus(packet: &[u8]) -> Result<Vec<i16>> {
    let stereo = !packet.is_empty() && (packet[0] >> 2) & 1 == 1;
    let mut decoder = Decoder::new(
        SAMPLE_RATE,
        if stereo {
            Channels::Stereo
        } else {
            Channels::Mono
        },
    )
    .map_err(|error| Error::Message(format!("opus decoder: {error}")))?;
    let mut pcm = vec![0i16; MAX_OPUS_FRAME * 2];
    let samples = decoder
        .decode(packet, &mut pcm, false)
        .map_err(|error| Error::Message(format!("opus decode: {error}")))?;
    if stereo {
        Ok(pcm[..samples * 2].to_vec())
    } else {
        let mut stereo_pcm = Vec::with_capacity(samples * 2);
        for sample in &pcm[..samples] {
            stereo_pcm.push(*sample);
            stereo_pcm.push(*sample);
        }
        Ok(stereo_pcm)
    }
}

/// Creates a channel of decoded frames; used by the UI for level meters.
pub fn frame_channel() -> (
    mpsc::UnboundedSender<Vec<i16>>,
    mpsc::UnboundedReceiver<Vec<i16>>,
) {
    mpsc::unbounded_channel()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_pipeline_delivers_20ms_frames() {
        let (tx, rx) = std::sync::mpsc::channel();
        let _capture = Capture::start_with_source(
            "audiotestsrc num-buffers=200 ! audioconvert ! audioresample \
             ! audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved \
             ! appsink name=sink sync=false",
            move |frame| {
                let _ = tx.send(frame.len());
            },
        )
        .unwrap();
        let first = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("capture produced no frames");
        assert_eq!(first, FRAME_INTERLEAVED);
    }

    /// A microphone made of a test tone, with `processing` applied the
    /// way the real microphone applies it.
    fn tone_line(
        processing: std::sync::Arc<Mutex<crate::processing::VoiceProcessing>>,
    ) -> LineBuilder {
        Box::new(move |processed| {
            let chain = if processed {
                crate::processing::chain(
                    &processing.lock(),
                    crate::processing::probe_running(),
                    &crate::processing::element_available,
                )
            } else {
                String::new()
            };
            format!(
                "audiotestsrc is-live=true wave=sine freq=440 volume=0.3 \
                 ! audioconvert ! audioresample ! {CAPTURE_CAPS}{chain} \
                 ! appsink name=sink sync=false"
            )
        })
    }

    /// Frames and the loudest frame level seen in `window`.
    fn collect(
        rx: &std::sync::mpsc::Receiver<Vec<i16>>,
        window: std::time::Duration,
    ) -> (usize, f32) {
        let deadline = std::time::Instant::now() + window;
        let mut frames = 0;
        let mut loudest = crate::processing::FLOOR_DB;
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            let Ok(frame) = rx.recv_timeout(left) else {
                break;
            };
            assert_eq!(frame.len(), FRAME_INTERLEAVED);
            frames += 1;
            loudest = loudest.max(crate::processing::level_db(&frame));
        }
        (frames, loudest)
    }

    #[test]
    fn microphone_processing_passes_audio_and_follows_changes() {
        use crate::processing::{NoiseSuppression, VoiceProcessing};
        gst::init().unwrap();
        for element in ["webrtcdsp", "webrtcechoprobe", "audiornnoise"] {
            assert!(
                crate::processing::element_available(element),
                "{element} is missing from the GStreamer plugin path"
            );
        }
        // A call's playback, with the probe echo cancellation listens to.
        let playback = gst::parse::launch(&format!(
            "audiotestsrc is-live=true wave=ticks volume=0.2 ! audioconvert \
             ! webrtcechoprobe name={} ! fakesink sync=true",
            crate::processing::ECHO_PROBE
        ))
        .unwrap();
        playback.set_state(gst::State::Playing).unwrap();
        let _probe = crate::processing::EchoProbeGuard::new();

        let settings = std::sync::Arc::new(Mutex::new(VoiceProcessing::default()));
        let (tx, rx) = std::sync::mpsc::channel();
        let capture = Capture::start_rebuilding(tone_line(settings.clone()), move |frame| {
            let _ = tx.send(frame);
        })
        .unwrap();
        let (frames, loudest) = collect(&rx, std::time::Duration::from_millis(1500));
        assert!(frames >= 40, "plain: {frames} frames");
        assert!(loudest > -20.0, "plain: {loudest} dB");

        let variants = [
            VoiceProcessing {
                noise_suppression: NoiseSuppression::Standard,
                ..VoiceProcessing::default()
            },
            VoiceProcessing {
                noise_suppression: NoiseSuppression::Strong,
                ..VoiceProcessing::default()
            },
            VoiceProcessing {
                auto_gain: true,
                ..VoiceProcessing::default()
            },
            VoiceProcessing {
                echo_cancellation: true,
                noise_suppression: NoiseSuppression::Standard,
                auto_gain: true,
                ..VoiceProcessing::default()
            },
            VoiceProcessing::default(),
        ];
        for variant in variants {
            *settings.lock() = variant;
            crate::processing::rebuild();
            // The rebuild happens within a frame; skip what came before.
            std::thread::sleep(std::time::Duration::from_millis(200));
            while rx.try_recv().is_ok() {}
            let (frames, _) = collect(&rx, std::time::Duration::from_millis(1500));
            assert!(frames >= 40, "{variant:?}: {frames} frames");
            // Not a silent fallback to the plain microphone.
            assert!(capture.is_processed(), "{variant:?} fell back");
        }
        // Back to plain: the tone is as loud as at the start.
        let (_, loudest) = collect(&rx, std::time::Duration::from_millis(500));
        assert!(loudest > -20.0, "plain again: {loudest} dB");
        drop(capture);
        playback.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn opus_roundtrip_keeps_frame_size() {
        let mut encoder = OpusEncoder::new().unwrap();
        let mut pcm = vec![0i16; FRAME_INTERLEAVED];
        for (index, sample) in pcm.iter_mut().enumerate() {
            let phase = index as f32 / SAMPLE_RATE as f32;
            *sample = ((phase * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        }
        let packet = encoder.encode(&pcm).unwrap();
        assert!(!packet.is_empty());
        let decoded = decode_opus(&packet).unwrap();
        assert!(decoded.len() >= FRAME_INTERLEAVED / 2);
        assert!(decoded.iter().any(|sample| *sample != 0));
    }
}
