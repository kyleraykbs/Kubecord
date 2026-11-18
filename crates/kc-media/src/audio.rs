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
use crate::receive::{ReceiveStats, RemoteAudio};
use kc_voice::VoiceHandle;

const SAMPLE_RATE: u32 = 48_000;
const FRAME_SAMPLES: usize = 960; // 20 ms per channel
const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * 2;
const MAX_OPUS_FRAME: usize = 5760;

/// Captures the default input device as 48 kHz stereo S16, in 20 ms frames.
pub struct Capture {
    pipeline: gst::Pipeline,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    pub fn start(on_frame: impl FnMut(Vec<i16>) + Send + 'static) -> Result<Self> {
        // The chosen microphone, or the system default.
        let source = match crate::devices::input_device() {
            Some(device) => format!("pulsesrc device=\"{device}\""),
            None => "autoaudiosrc".to_owned(),
        };
        let pipeline = format!(
            "{source} ! audioconvert ! audioresample \
             ! audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved \
             ! appsink name=sink sync=false max-buffers=8 drop=true"
        );
        Self::start_with_source(&pipeline, on_frame)
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

    fn start_with_source(
        source: &str,
        mut on_frame: impl FnMut(Vec<i16>) + Send + 'static,
    ) -> Result<Self> {
        gst::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))?;
        let element = gst::parse::launch(source)
            .map_err(|error| Error::Message(format!("capture pipeline: {error}")))?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| Error::Message("capture pipeline is not a pipeline".into()))?;
        let appsink = pipeline
            .by_name("sink")
            .and_then(|element| element.downcast::<gstreamer_app::AppSink>().ok())
            .ok_or_else(|| Error::Message("capture appsink missing".into()))?;
        appsink.set_sync(false);
        appsink.set_drop(true);
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| Error::Message(format!("capture state: {error}")))?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut pending: Vec<i16> = Vec::with_capacity(FRAME_INTERLEAVED * 2);
            while !thread_stop.load(Ordering::Relaxed) {
                let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(20))
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
        });

        Ok(Self {
            pipeline,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// System-audio capture for a Go Live stream connection. The stream's audio
/// SSRC is separate from the parent voice connection; Discord expects this on
/// the stream connection with the SOUNDSHARE speaking flag.
pub struct ScreenAudio {
    _capture: Capture,
    voice: VoiceHandle,
}

impl ScreenAudio {
    pub fn start(voice: VoiceHandle, bitrate: u32, delay_ms: u64) -> Result<Self> {
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
        let capture = match Capture::start_monitor(move |frame| {
            let mut encoder = encoder.lock();
            match encoder.encode_vec(&frame, 4000) {
                Ok(packet) => delayed.send(packet, 20),
                Err(error) => debug!(%error, "screen audio encode failed"),
            }
        }) {
            Ok(capture) => capture,
            Err(error) => {
                voice.set_speaking(0);
                return Err(error);
            }
        };

        Ok(Self {
            _capture: capture,
            voice,
        })
    }
}

impl Drop for ScreenAudio {
    fn drop(&mut self) {
        self.voice.set_speaking(0);
    }
}

/// Root-mean-square level above which the microphone counts as speech,
/// about -38 dBFS: well above a quiet room, well below normal speech.
const SPEECH_RMS: f64 = 400.0;
/// Frames the speaking state outlives the last loud frame (300 ms), so it
/// does not flicker between words.
const SPEECH_HANGOVER_FRAMES: u32 = 15;

/// Decides from microphone frames whether the user is speaking.
#[derive(Default)]
struct SpeechDetector {
    /// Frames left before a pause counts as silence.
    hangover: u32,
}

impl SpeechDetector {
    /// Feeds one frame; returns `Some(state)` when speaking starts or stops.
    fn feed(&mut self, frame: &[i16]) -> Option<bool> {
        let was = self.hangover > 0;
        let energy: f64 = frame
            .iter()
            .map(|sample| f64::from(*sample) * f64::from(*sample))
            .sum();
        let rms = (energy / frame.len().max(1) as f64).sqrt();
        if rms >= SPEECH_RMS {
            self.hangover = SPEECH_HANGOVER_FRAMES;
        } else {
            self.hangover = self.hangover.saturating_sub(1);
        }
        let now = self.hangover > 0;
        (was != now).then_some(now)
    }

    /// Ends speech at once (the microphone was switched off).
    fn silence(&mut self) -> Option<bool> {
        (std::mem::take(&mut self.hangover) > 0).then_some(false)
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
        let mut detector = SpeechDetector::default();
        let capture = Capture::start(move |mut frame| {
            crate::devices::apply_gain(&mut frame, crate::devices::input_gain());
            if !capture_enabled.load(Ordering::Relaxed) {
                if let Some(speaking) = detector.silence() {
                    on_speaking(speaking);
                }
                return;
            }
            if let Some(speaking) = detector.feed(&frame) {
                on_speaking(speaking);
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
        let capture = Capture::start(move |mut frame| {
            crate::devices::apply_gain(&mut frame, crate::devices::input_gain());
            let loudest = frame.iter().map(|sample| sample.unsigned_abs()).max();
            level.fetch_max(u32::from(loudest.unwrap_or(0)), Ordering::Relaxed);
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

    #[test]
    fn speech_detector_reports_edges_and_holds_through_pauses() {
        let quiet = vec![20i16; FRAME_INTERLEAVED];
        let loud = vec![3000i16; FRAME_INTERLEAVED];
        let mut detector = SpeechDetector::default();
        assert_eq!(detector.feed(&quiet), None);
        assert_eq!(detector.feed(&loud), Some(true));
        assert_eq!(detector.feed(&loud), None);
        // A pause shorter than the hangover does not end speech.
        for _ in 0..SPEECH_HANGOVER_FRAMES - 1 {
            assert_eq!(detector.feed(&quiet), None);
        }
        assert_eq!(detector.feed(&quiet), Some(false));
        assert_eq!(detector.feed(&quiet), None);
        // Muting ends speech immediately.
        assert_eq!(detector.feed(&loud), Some(true));
        assert_eq!(detector.silence(), Some(false));
        assert_eq!(detector.silence(), None);
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
