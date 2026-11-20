//! Short interface sounds (notifications, joins, mutes, ringing): each one
//! is its own `playbin` on its own thread, so sounds overlap freely and
//! never touch the call's pipelines. A ring loops until it is stopped.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::error::{Error, Result};

/// The longest a sound that does not loop may play.
const LONGEST: Duration = Duration::from_secs(20);

/// A playing sound. Dropping it lets the sound finish; [`Sound::stop`]
/// ends it (a loop plays until stopped).
pub struct Sound {
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    /// Reached its end at least once without an error.
    ended: Arc<AtomicBool>,
}

impl Sound {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Whether it has finished (or failed).
    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    /// Whether it played through to its end (at least once, for a loop).
    pub fn played(&self) -> bool {
        self.ended.load(Ordering::Relaxed)
    }
}

/// Where sounds play: the configured output, or the default one.
fn output_sink() -> Result<gst::Element> {
    let sink = match crate::devices::output_device() {
        Some(device) => gst::ElementFactory::make("pulsesink")
            .property("device", device)
            .build(),
        None if gst::ElementFactory::find("pulsesink").is_some() => {
            gst::ElementFactory::make("pulsesink").build()
        }
        None => gst::ElementFactory::make("autoaudiosink").build(),
    }
    .map_err(|error| Error::Message(format!("sound output: {error}")))?;
    if sink.has_property("stream-properties") {
        // Marks the stream as an event sound for the sound server, which
        // may treat it differently from a call or music.
        if let Ok(properties) =
            "props,media.role=event,application.name=kylecord".parse::<gst::Structure>()
        {
            sink.set_property("stream-properties", properties);
        }
    }
    Ok(sink)
}

/// Plays `path` at `volume` (0.0-1.0) on the sound output.
pub fn play(path: &Path, volume: f64, looping: bool) -> Result<Sound> {
    gst::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))?;
    play_on(path, volume, looping, output_sink()?)
}

/// Plays `path` into `sink` (tests use a `fakesink`).
pub fn play_on(path: &Path, volume: f64, looping: bool, sink: gst::Element) -> Result<Sound> {
    gst::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))?;
    let uri = glib::filename_to_uri(path, None)
        .map_err(|error| Error::Message(format!("sound path: {error}")))?;
    let playbin = gst::ElementFactory::make("playbin")
        .property("uri", uri.as_str())
        .property("volume", volume.clamp(0.0, 1.0))
        .property("audio-sink", &sink)
        .build()
        .map_err(|error| Error::Message(format!("playbin: {error}")))?;
    // Sound only: an MP3's cover art must not open a video window.
    playbin.set_property_from_str("flags", "audio");
    let bus = playbin
        .bus()
        .ok_or_else(|| Error::Message("playbin has no bus".into()))?;
    if let Err(error) = playbin.set_state(gst::State::Playing) {
        let _ = playbin.set_state(gst::State::Null);
        return Err(Error::Message(format!("sound: {error}")));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread_done = done.clone();
    let ended = Arc::new(AtomicBool::new(false));
    let thread_ended = ended.clone();
    let name = path.display().to_string();
    std::thread::spawn(move || {
        let started = Instant::now();
        loop {
            if thread_stop.load(Ordering::Relaxed) || (!looping && started.elapsed() > LONGEST) {
                break;
            }
            let Some(message) = bus.timed_pop_filtered(
                gst::ClockTime::from_mseconds(50),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            ) else {
                continue;
            };
            match message.view() {
                gst::MessageView::Eos(_) if looping => {
                    thread_ended.store(true, Ordering::Relaxed);
                    let _ = playbin.seek_simple(
                        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                        gst::ClockTime::ZERO,
                    );
                }
                gst::MessageView::Error(error) => {
                    tracing::warn!(sound = name, error = %error.error(), "sound failed");
                    break;
                }
                _ => {
                    thread_ended.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
        let _ = playbin.set_state(gst::State::Null);
        thread_done.store(true, Ordering::Relaxed);
    });
    Ok(Sound { stop, done, ended })
}

/// A WAV file (48 kHz mono 16-bit) of the notes given as `(Hz, ms)`;
/// a frequency of 0 is a rest. Each note fades in and out over 5 ms so it
/// does not click. The built-in stand-in for a sound that could not be
/// downloaded.
pub fn tone_wav(notes: &[(f32, u32)], volume: f32) -> Vec<u8> {
    const RATE: u32 = 48_000;
    let fade = (RATE / 200) as usize;
    let mut samples: Vec<i16> = Vec::new();
    for &(frequency, millis) in notes {
        let count = (RATE as u64 * u64::from(millis) / 1000) as usize;
        for index in 0..count {
            let envelope = (index.min(count - 1 - index).min(fade) as f32) / fade as f32;
            let value = if frequency > 0.0 {
                let phase = index as f32 * frequency / RATE as f32;
                (phase * std::f32::consts::TAU).sin()
                    + 0.3 * (phase * 2.0 * std::f32::consts::TAU).sin()
            } else {
                0.0
            };
            samples.push((value / 1.3 * envelope * volume.clamp(0.0, 1.0) * 32_767.0) as i16);
        }
    }
    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fakesink() -> gst::Element {
        gst::init().unwrap();
        gst::ElementFactory::make("fakesink")
            .property("sync", true)
            .build()
            .unwrap()
    }

    fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("kylecord-sound-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn wait(sound: &Sound, limit: Duration) -> Duration {
        let started = Instant::now();
        while !sound.finished() && started.elapsed() < limit {
            std::thread::sleep(Duration::from_millis(10));
        }
        started.elapsed()
    }

    #[test]
    fn tones_are_valid_wav() {
        let wav = tone_wav(&[(440.0, 100), (0.0, 50), (660.0, 100)], 0.5);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        let data = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
        assert_eq!(data, 250 * 48 * 2);
        assert_eq!(wav.len(), 44 + data);
        // Faded ends, a tone in the middle.
        assert_eq!(i16::from_le_bytes([wav[44], wav[45]]), 0);
        assert!(
            wav[44..]
                .chunks(2)
                .any(|pair| i16::from_le_bytes([pair[0], pair[1]]) > 8000)
        );
    }

    #[test]
    fn sounds_play_to_the_end_and_overlap() {
        let path = temp_file("beep.wav", &tone_wav(&[(880.0, 300)], 0.5));
        let first = play_on(&path, 0.5, false, fakesink()).unwrap();
        let second = play_on(&path, 0.5, false, fakesink()).unwrap();
        let took = wait(&first, Duration::from_secs(5));
        wait(&second, Duration::from_secs(5));
        assert!(first.played() && second.played());
        // Played in real time, not skipped through.
        assert!(took >= Duration::from_millis(250), "{took:?}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn loops_play_until_stopped() {
        let path = temp_file("ring.wav", &tone_wav(&[(440.0, 100)], 0.5));
        let ring = play_on(&path, 0.5, true, fakesink()).unwrap();
        std::thread::sleep(Duration::from_millis(600));
        assert!(!ring.finished(), "a loop ended by itself");
        assert!(ring.played(), "the loop did not come round");
        ring.stop();
        wait(&ring, Duration::from_secs(2));
        assert!(ring.finished());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mp3_files_play() {
        gst::init().unwrap();
        // Discord's sounds are MP3: make one and play it back.
        let path = std::env::temp_dir().join(format!("kylecord-sound-{}.mp3", std::process::id()));
        let encode = gst::parse::launch(&format!(
            "audiotestsrc num-buffers=20 ! audioconvert ! lamemp3enc ! filesink location={}",
            path.display()
        ))
        .unwrap();
        encode.set_state(gst::State::Playing).unwrap();
        let bus = encode.bus().unwrap();
        bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        encode.set_state(gst::State::Null).unwrap();
        let sound = play_on(&path, 1.0, false, fakesink()).unwrap();
        wait(&sound, Duration::from_secs(5));
        assert!(sound.played(), "the MP3 did not play");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_missing_file_ends_quietly() {
        let sound = play_on(
            Path::new("/nonexistent/kylecord.mp3"),
            1.0,
            false,
            fakesink(),
        );
        if let Ok(sound) = sound {
            wait(&sound, Duration::from_secs(5));
            assert!(sound.finished() && !sound.played());
        }
    }
}
