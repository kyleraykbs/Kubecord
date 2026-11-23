//! Voice messages: recording one from the microphone (Ogg Opus, as Discord
//! wants, with a waveform of its loudness), and playing one back.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::error::{Error, Result};

const RATE: u64 = 48_000;
/// One waveform sample per this many audio samples (100 ms).
const WINDOW: usize = (RATE / 10) as usize;
/// Discord's waveforms have at most this many samples.
pub const WAVEFORM_SAMPLES: usize = 256;

/// A finished recording.
#[derive(Clone, Debug)]
pub struct Clip {
    /// The Ogg Opus file.
    pub ogg: Vec<u8>,
    pub duration_secs: f64,
    /// Loudness over time, 0-255, at most [`WAVEFORM_SAMPLES`].
    pub waveform: Vec<u8>,
}

/// Loudness so far, and how much audio there is.
#[derive(Default)]
struct Levels {
    /// The loudest sample of each finished window, 0.0-1.0.
    peaks: Vec<f32>,
    /// The window under way: its loudest sample and how many it has.
    current: f32,
    filled: usize,
}

/// A recording under way. Dropping it without [`Recorder::finish`] throws
/// it away.
pub struct Recorder {
    pipeline: gst::Element,
    levels: Arc<Mutex<Levels>>,
    samples: Arc<AtomicU64>,
    path: PathBuf,
}

impl Recorder {
    /// Records from the chosen microphone (or the default one).
    pub fn start() -> Result<Self> {
        // Demos record a test tone, never the real microphone.
        if let Ok(source) = std::env::var("KUBECORD_TEST_MIC") {
            return Self::start_with(&source);
        }
        let source = match crate::devices::input_device() {
            Some(device) => format!("pulsesrc device=\"{device}\" client-name=kubecord"),
            None => "autoaudiosrc".to_owned(),
        };
        Self::start_with(&source)
    }

    /// Records from `source`, a pipeline fragment (tests use a test tone).
    pub fn start_with(source: &str) -> Result<Self> {
        crate::init()?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kubecord-voice-{}-{}.ogg",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let location = path.to_string_lossy().replace('"', "");
        let description = format!(
            "{source} ! audioconvert ! audioresample \
             ! audio/x-raw,format=S16LE,rate={RATE},channels=1 ! tee name=t \
             t. ! queue ! opusenc bitrate=64000 ! oggmux ! filesink location=\"{location}\" \
             t. ! queue ! appsink name=levels sync=false"
        );
        let pipeline = gst::parse::launch(&description)
            .map_err(|error| Error::Message(format!("voice recorder: {error}")))?;
        let bin = pipeline
            .clone()
            .downcast::<gst::Bin>()
            .map_err(|_| Error::Message("voice recorder is not a bin".into()))?;
        let sink = bin
            .by_name("levels")
            .and_then(|sink| sink.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| Error::Message("voice recorder has no level sink".into()))?;

        let levels = Arc::new(Mutex::new(Levels::default()));
        let samples = Arc::new(AtomicU64::new(0));
        let (thread_levels, thread_samples) = (levels.clone(), samples.clone());
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let Some(buffer) = sample.buffer() else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    let Ok(map) = buffer.map_readable() else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    let pcm: Vec<i16> = map
                        .chunks_exact(2)
                        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                        .collect();
                    thread_samples.fetch_add(pcm.len() as u64, Ordering::Relaxed);
                    if let Ok(mut levels) = thread_levels.lock() {
                        add_levels(&mut levels, &pcm);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        if let Err(error) = pipeline.set_state(gst::State::Playing) {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(Error::Message(format!("voice recorder: {error}")));
        }
        Ok(Self {
            pipeline,
            levels,
            samples,
            path,
        })
    }

    /// Seconds recorded so far.
    pub fn elapsed(&self) -> f64 {
        self.samples.load(Ordering::Relaxed) as f64 / RATE as f64
    }

    /// The loudness of each 100 ms so far, 0.0-1.0 (for a live waveform).
    pub fn peaks(&self) -> Vec<f32> {
        self.levels.lock().map(|levels| levels.peaks.clone()).unwrap_or_default()
    }

    /// Whether the pipeline has failed (no microphone, say).
    pub fn failed(&self) -> Option<String> {
        let bus = self.pipeline.bus()?;
        while let Some(message) = bus.pop_filtered(&[gst::MessageType::Error]) {
            if let gst::MessageView::Error(error) = message.view() {
                return Some(error.error().to_string());
            }
        }
        None
    }

    /// Stops and returns the recording.
    pub fn finish(self) -> Result<Clip> {
        // End of stream, so the muxer writes the file out properly.
        self.pipeline.send_event(gst::event::Eos::new());
        if let Some(bus) = self.pipeline.bus() {
            let _ = bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(3),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            );
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        let ogg = std::fs::read(&self.path)
            .map_err(|error| Error::Message(format!("voice recording: {error}")))?;
        let duration_secs = self.elapsed();
        let mut peaks = self.peaks();
        if let Ok(levels) = self.levels.lock()
            && levels.filled > 0
        {
            peaks.push(levels.current);
        }
        Ok(Clip {
            ogg,
            duration_secs,
            waveform: waveform(&peaks),
        })
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        let _ = std::fs::remove_file(&self.path);
    }
}

fn add_levels(levels: &mut Levels, pcm: &[i16]) {
    for sample in pcm {
        let loud = f32::from(sample.unsigned_abs().min(i16::MAX as u16)) / f32::from(i16::MAX);
        levels.current = levels.current.max(loud);
        levels.filled += 1;
        if levels.filled == WINDOW {
            let peak = levels.current;
            levels.peaks.push(peak);
            levels.current = 0.0;
            levels.filled = 0;
        }
    }
}

/// Discord's waveform from loudness over time: at most
/// [`WAVEFORM_SAMPLES`] values (the loudest of each stretch), scaled so the
/// loudest is 255.
pub fn waveform(peaks: &[f32]) -> Vec<u8> {
    if peaks.is_empty() {
        return Vec::new();
    }
    let buckets = peaks.len().min(WAVEFORM_SAMPLES);
    let reduced: Vec<f32> = (0..buckets)
        .map(|bucket| {
            let start = bucket * peaks.len() / buckets;
            let end = ((bucket + 1) * peaks.len() / buckets).max(start + 1);
            peaks[start..end].iter().copied().fold(0.0, f32::max)
        })
        .collect();
    let loudest = reduced.iter().copied().fold(0.0, f32::max);
    reduced
        .into_iter()
        .map(|peak| {
            if loudest <= 0.0 {
                0
            } else {
                ((peak / loudest) * 255.0).round() as u8
            }
        })
        .collect()
}

/// A voice message playing (or paused).
pub struct Player {
    playbin: gst::Element,
    ended: bool,
}

impl Player {
    /// Opens `path`, paused at its start.
    pub fn open(path: &Path) -> Result<Self> {
        crate::init()?;
        Self::open_on(path, crate::sound::output_sink()?)
    }

    /// Opens `path` playing into `sink` (tests use a `fakesink`).
    pub fn open_on(path: &Path, sink: gst::Element) -> Result<Self> {
        crate::init()?;
        let uri = glib::filename_to_uri(path, None)
            .map_err(|error| Error::Message(format!("voice message path: {error}")))?;
        let playbin = gst::ElementFactory::make("playbin")
            .property("uri", uri.as_str())
            .property("audio-sink", &sink)
            .build()
            .map_err(|error| Error::Message(format!("playbin: {error}")))?;
        playbin.set_property_from_str("flags", "audio");
        playbin
            .set_state(gst::State::Paused)
            .map_err(|error| Error::Message(format!("voice message: {error}")))?;
        Ok(Self {
            playbin,
            ended: false,
        })
    }

    pub fn play(&mut self) {
        if self.ended {
            self.seek(0.0);
            self.ended = false;
        }
        let _ = self.playbin.set_state(gst::State::Playing);
    }

    pub fn pause(&self) {
        let _ = self.playbin.set_state(gst::State::Paused);
    }

    pub fn is_playing(&self) -> bool {
        !self.ended && self.playbin.current_state() == gst::State::Playing
    }

    /// Jumps to `fraction` (0.0-1.0) of the way through.
    pub fn seek(&mut self, fraction: f64) {
        if let Some(duration) = self.playbin.query_duration::<gst::ClockTime>() {
            let at = (duration.nseconds() as f64 * fraction.clamp(0.0, 1.0)) as u64;
            let _ = self
                .playbin
                .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, gst::ClockTime::from_nseconds(at));
            self.ended = false;
        }
    }

    /// Seconds played.
    pub fn position(&self) -> f64 {
        if self.ended {
            return self.duration().unwrap_or(0.0);
        }
        self.playbin
            .query_position::<gst::ClockTime>()
            .map_or(0.0, |at| Duration::from_nanos(at.nseconds()).as_secs_f64())
    }

    pub fn duration(&self) -> Option<f64> {
        self.playbin
            .query_duration::<gst::ClockTime>()
            .map(|at| Duration::from_nanos(at.nseconds()).as_secs_f64())
    }

    /// Reads what happened since last asked: whether it has reached its end
    /// (it then stays paused there), or an error.
    pub fn poll(&mut self) -> Result<bool> {
        let Some(bus) = self.playbin.bus() else {
            return Ok(self.ended);
        };
        while let Some(message) = bus.pop_filtered(&[gst::MessageType::Eos, gst::MessageType::Error]) {
            match message.view() {
                gst::MessageView::Error(error) => {
                    return Err(Error::Message(format!("voice message: {}", error.error())));
                }
                _ => {
                    self.ended = true;
                    let _ = self.playbin.set_state(gst::State::Paused);
                }
            }
        }
        Ok(self.ended)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.playbin.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveform_keeps_the_loudest_of_each_stretch_scaled_to_255() {
        assert!(waveform(&[]).is_empty());
        assert_eq!(waveform(&[0.25, 0.5]), vec![128, 255]);
        let long: Vec<f32> = (0..1000).map(|i| if i == 999 { 1.0 } else { 0.5 }).collect();
        let reduced = waveform(&long);
        assert_eq!(reduced.len(), WAVEFORM_SAMPLES);
        assert_eq!(*reduced.last().unwrap(), 255);
        assert_eq!(reduced[0], 128);
    }

    #[test]
    fn records_a_tone_and_plays_it_back() {
        let recorder = Recorder::start_with("audiotestsrc is-live=true wave=sine volume=0.5").unwrap();
        std::thread::sleep(Duration::from_millis(700));
        assert!(recorder.failed().is_none());
        let clip = recorder.finish().unwrap();
        assert!(clip.ogg.starts_with(b"OggS"), "an Ogg file");
        assert!((0.4..1.5).contains(&clip.duration_secs), "{}", clip.duration_secs);
        assert!(!clip.waveform.is_empty());
        assert!(clip.waveform.iter().any(|&level| level > 200));

        let path = std::env::temp_dir().join(format!("kubecord-voice-test-{}.ogg", std::process::id()));
        std::fs::write(&path, &clip.ogg).unwrap();
        let sink = gst::ElementFactory::make("fakesink").property("sync", true).build().unwrap();
        let mut player = Player::open_on(&path, sink).unwrap();
        player.play();
        let mut ended = false;
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(50));
            if player.poll().unwrap() {
                ended = true;
                break;
            }
        }
        assert!(ended, "played to its end");
        let duration = player.duration().unwrap();
        assert!((0.3..1.5).contains(&duration), "{duration}");
        let _ = std::fs::remove_file(&path);
    }
}
