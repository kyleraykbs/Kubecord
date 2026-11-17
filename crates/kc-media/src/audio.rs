//! Voice audio: mic capture, Opus encode, receive decode, mix and playback.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gstreamer as gst;
use gstreamer_app::prelude::*;
use opus2::{Application, Channels, Decoder, Encoder};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::error::{Error, Result};
use kc_voice::VoiceHandle;

const SAMPLE_RATE: u32 = 48_000;
const FRAME_SAMPLES: usize = 960; // 20 ms per channel
const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * 2;
const MAX_OPUS_FRAME: usize = 5760;

/// Plays interleaved 48 kHz stereo S16 samples.
pub struct Playback {
    pipeline: gst::Pipeline,
    appsrc: gstreamer_app::AppSrc,
}

impl Playback {
    pub fn new() -> Result<Self> {
        gst::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))?;
        let element = gst::parse::launch(
            "appsrc name=src is-live=true format=time do-timestamp=true \
             ! queue max-size-buffers=4 max-size-time=200000000 \
             ! audioconvert ! audioresample ! autoaudiosink sync=true",
        )
        .map_err(|error| Error::Message(format!("playback pipeline: {error}")))?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| Error::Message("playback pipeline is not a pipeline".into()))?;
        let appsrc = pipeline
            .by_name("src")
            .and_then(|element| element.downcast::<gstreamer_app::AppSrc>().ok())
            .ok_or_else(|| Error::Message("playback appsrc missing".into()))?;

        let caps = gst::Caps::builder("audio/x-raw")
            .field("format", "S16LE")
            .field("rate", SAMPLE_RATE as i32)
            .field("channels", 2i32)
            .field("layout", "interleaved")
            .build();
        appsrc.set_caps(Some(&caps));
        appsrc.set_format(gst::Format::Time);
        appsrc.set_max_bytes(FRAME_INTERLEAVED as u64 * 2 * 8);
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| Error::Message(format!("playback state: {error}")))?;

        Ok(Self { pipeline, appsrc })
    }

    pub fn push(&self, samples: &[i16]) -> Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let buffer = gst::Buffer::from_slice(bytes);
        self.appsrc
            .push_buffer(buffer)
            .map_err(|error| Error::Message(format!("playback push: {error}")))?;
        Ok(())
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Captures the default input device as 48 kHz stereo S16, in 20 ms frames.
pub struct Capture {
    pipeline: gst::Pipeline,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    pub fn start(on_frame: impl FnMut(Vec<i16>) + Send + 'static) -> Result<Self> {
        Self::start_with_source(
            "autoaudiosrc ! audioconvert ! audioresample \
             ! audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved \
             ! appsink name=sink sync=false max-buffers=8 drop=true",
            on_frame,
        )
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

struct UserAudio {
    mono: Decoder,
    stereo: Decoder,
    queue: VecDeque<Vec<i16>>,
    volume: f32,
    last_timestamp: Option<u32>,
}

impl UserAudio {
    fn new() -> Result<Self> {
        Ok(Self {
            mono: Decoder::new(SAMPLE_RATE, Channels::Mono)
                .map_err(|error| Error::Message(format!("opus decoder: {error}")))?,
            stereo: Decoder::new(SAMPLE_RATE, Channels::Stereo)
                .map_err(|error| Error::Message(format!("opus decoder: {error}")))?,
            queue: VecDeque::with_capacity(8),
            volume: 1.0,
            last_timestamp: None,
        })
    }

    fn push(&mut self, opus: &[u8], timestamp: u32) -> Result<()> {
        if opus.is_empty() {
            return Ok(());
        }
        let stereo = (opus[0] >> 2) & 1 == 1;
        let mut pcm = vec![0i16; MAX_OPUS_FRAME * 2];
        let decoded = if stereo {
            self.stereo
                .decode(opus, &mut pcm, false)
                .map_err(|error| Error::Message(format!("opus decode: {error}")))?
        } else {
            let decoded = self
                .mono
                .decode(opus, &mut pcm, false)
                .map_err(|error| Error::Message(format!("opus decode: {error}")))?;
            // Duplicate mono into stereo in place, back to front.
            for index in (0..decoded).rev() {
                pcm[index * 2] = pcm[index];
                pcm[index * 2 + 1] = pcm[index];
            }
            decoded
        };

        let mut frame = pcm[..decoded * 2].to_vec();
        if frame.len() < FRAME_INTERLEAVED {
            frame.resize(FRAME_INTERLEAVED, 0);
        }

        // Drop packets that are too late to be useful.
        if let Some(last) = self.last_timestamp
            && timestamp.wrapping_add(FRAME_SAMPLES as u32 * 5) < last
        {
            return Ok(());
        }
        self.last_timestamp = Some(timestamp);
        if self.queue.len() >= 10 {
            self.queue.pop_front();
        }
        self.queue.push_back(frame);
        Ok(())
    }

    fn mix_into(&mut self, output: &mut [i32]) {
        if let Some(frame) = self.queue.pop_front() {
            for (index, sample) in frame.iter().enumerate() {
                if index >= output.len() {
                    break;
                }
                output[index] += (*sample as f32 * self.volume) as i32;
            }
        }
    }
}

struct Mixer {
    users: HashMap<u64, UserAudio>,
    master: f32,
}

impl Mixer {
    fn new() -> Self {
        Self {
            users: HashMap::new(),
            master: 1.0,
        }
    }

    fn push(&mut self, user_id: u64, opus: &[u8], timestamp: u32) -> Result<()> {
        let user = match self.users.get_mut(&user_id) {
            Some(user) => user,
            None => {
                self.users.insert(user_id, UserAudio::new()?);
                self.users.get_mut(&user_id).expect("just inserted")
            }
        };
        user.push(opus, timestamp)
    }

    fn mix(&mut self) -> Vec<i16> {
        let mut accumulator = vec![0i32; FRAME_INTERLEAVED];
        for user in self.users.values_mut() {
            user.mix_into(&mut accumulator);
        }
        accumulator
            .into_iter()
            .map(|sample| {
                let scaled = (sample as f32 * self.master).clamp(i16::MIN as f32, i16::MAX as f32);
                scaled as i16
            })
            .collect()
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
    pub fn start(voice: VoiceHandle, bitrate: u32) -> Result<Self> {
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
        let capture = match Capture::start_monitor(move |frame| {
            let mut encoder = encoder.lock();
            match encoder.encode_vec(&frame, 4000) {
                Ok(packet) => send_voice.send_soundshare_audio(packet, 20),
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

/// Owns the local audio path for one voice connection.
pub struct VoiceAudio {
    mixer: Arc<Mutex<Mixer>>,
    mic_enabled: Arc<AtomicBool>,
    _playback: Playback,
    _capture: Capture,
    task: tokio::task::JoinHandle<()>,
}

impl VoiceAudio {
    pub fn start(voice: VoiceHandle) -> Result<Self> {
        let playback = Playback::new()?;
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
        let capture = Capture::start(move |frame| {
            if !capture_enabled.load(Ordering::Relaxed) {
                return;
            }
            let mut encoder = encoder.lock();
            match encoder.encode_vec(&frame, 4000) {
                Ok(packet) => voice.send_audio(packet, 20),
                Err(error) => debug!(%error, "opus encode failed"),
            }
        })?;

        let mixer = Arc::new(Mutex::new(Mixer::new()));
        let playback_for_task = playback.appsrc.clone();
        let mixer_for_task = mixer.clone();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(20));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let frame = mixer_for_task.lock().mix();
                let mut bytes = Vec::with_capacity(frame.len() * 2);
                for sample in frame {
                    bytes.extend_from_slice(&sample.to_le_bytes());
                }
                let buffer = gst::Buffer::from_slice(bytes);
                if playback_for_task.push_buffer(buffer).is_err() {
                    debug!("playback appsrc rejected a frame");
                }
            }
        });

        Ok(Self {
            mixer,
            mic_enabled,
            _playback: playback,
            _capture: capture,
            task,
        })
    }

    pub fn set_volume(&self, user_id: u64, volume: f32) {
        let mut mixer = self.mixer.lock();
        if let Some(user) = mixer.users.get_mut(&user_id) {
            user.volume = volume.clamp(0.0, 2.0);
        }
    }

    pub fn set_master_volume(&self, volume: f32) {
        self.mixer.lock().master = volume.clamp(0.0, 2.0);
    }

    pub fn set_mic_enabled(&self, enabled: bool) {
        self.mic_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Deafen means no microphone and no playback; undeafen restores both.
    pub fn set_deafened(&self, deafened: bool) {
        self.mic_enabled.store(!deafened, Ordering::Relaxed);
        self.set_master_volume(if deafened { 0.0 } else { 1.0 });
    }

    pub fn push_remote(&self, user_id: u64, opus: &[u8], timestamp: u32) {
        if let Err(error) = self.mixer.lock().push(user_id, opus, timestamp) {
            warn!(%error, user_id, "could not decode remote audio");
        }
    }
}

impl Drop for VoiceAudio {
    fn drop(&mut self) {
        self.task.abort();
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
