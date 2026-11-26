//! Receiving voice: a jitter buffer per speaker, Opus decoding with loss
//! concealment, mixing, and playback paced by the sound device.
//!
//! Packets arrive from the network at an uneven pace, sometimes out of order
//! and sometimes not at all. Each speaker's packets wait in a small buffer,
//! ordered by RTP sequence number, and are decoded only when the sound device
//! asks for the next 20 ms. A missing packet is rebuilt from the next
//! packet's forward error correction data when it carries any, otherwise the
//! decoder conceals the gap.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSrcCallbacks;
use opus2::{Channels, Decoder};
use parking_lot::{Mutex, RwLock};

use crate::error::{Error, Result};

/// The most one participant's volume can be turned up.
pub const MAX_USER_VOLUME: f32 = 5.0;

const SAMPLE_RATE: u32 = 48_000;
/// Samples per channel in a 20 ms frame.
const FRAME_SAMPLES: usize = 960;
/// Interleaved stereo samples in a 20 ms frame.
const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * 2;
/// The longest Opus frame, 120 ms, in samples per channel.
const MAX_OPUS_SAMPLES: usize = 5760;

/// Milliseconds of sound in one packet.
const PACKET_MS: u32 = 20;
/// Gaps longer than this are skipped rather than concealed.
const MAX_CONCEALED_GAP: u32 = 5;
/// Without waiting for late packets: consecutive empty pulls that are
/// concealed before the burst is considered over and the buffer starts
/// collecting again.
const CONCEALED_DRY_PULLS: u32 = 2;
/// A sequence jump this large means the stream restarted.
const RESTART_JUMP: i32 = 2000;

/// How each speaker's sound is buffered before it plays: the trade between
/// delay and sound that holds up when the network or the machine is uneven.
///
/// Like the devices, this is process-global: the UI sets it and calls follow
/// it from their next frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buffering {
    /// Sound collected before someone's speech starts playing.
    pub target_ms: u32,
    /// Beyond this much waiting sound, the speaker has fallen behind (a
    /// stall, or clock drift) and playback skips ahead to `target_ms`.
    pub max_ms: u32,
    /// How long an empty buffer waits for a late packet, smoothing over the
    /// wait, before it treats the speech as over. Zero waits for nothing: a
    /// late packet's slot is concealed and the packet thrown away.
    pub late_wait_ms: u32,
    /// How long the buffer must hold more than `target_ms` before one
    /// packet of the extra delay is shed. Zero never sheds it (until the
    /// speaker pauses).
    pub settle_ms: u32,
}

impl Buffering {
    pub const DEFAULT: Self = Self {
        target_ms: 60,
        max_ms: 300,
        late_wait_ms: 40,
        settle_ms: 2000,
    };
}

impl Default for Buffering {
    fn default() -> Self {
        Self::DEFAULT
    }
}

static BUFFERING: RwLock<Buffering> = RwLock::new(Buffering::DEFAULT);

/// Sets the buffering; running calls follow from their next frame.
pub fn set_buffering(buffering: Buffering) {
    *BUFFERING.write() = buffering;
}

pub fn buffering() -> Buffering {
    *BUFFERING.read()
}

/// [`Buffering`] counted in packets, made consistent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Depths {
    target: usize,
    max: usize,
    /// Whether an empty buffer keeps the late packet's slot.
    wait: bool,
    /// Consecutive empty pulls concealed before the burst counts as over.
    dry_pulls: u32,
    /// Pulls a drain window lasts; zero never drains.
    drain_window: u32,
}

impl From<Buffering> for Depths {
    fn from(buffering: Buffering) -> Self {
        let target = (buffering.target_ms / PACKET_MS).clamp(1, 50) as usize;
        let wait = buffering.late_wait_ms > 0;
        Self {
            target,
            // Room to absorb at least a little jitter before skipping.
            max: ((buffering.max_ms / PACKET_MS) as usize).max(target + 2),
            wait,
            dry_pulls: if wait {
                (buffering.late_wait_ms / PACKET_MS).max(1)
            } else {
                CONCEALED_DRY_PULLS
            },
            drain_window: buffering.settle_ms / PACKET_MS,
        }
    }
}

/// What happened to the packets of the speakers, for diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiveStats {
    /// Packets accepted into a jitter buffer.
    pub packets: u64,
    /// Packets that arrived after their slot had already been played.
    pub late: u64,
    /// Frames rebuilt from the following packet's error correction data.
    pub recovered: u64,
    /// Frames the decoder had to invent.
    pub concealed: u64,
    /// Packets dropped to catch up after falling behind.
    pub skipped: u64,
    /// Packets the decoder rejected.
    pub undecodable: u64,
    /// 20 ms frames handed to the sound device with at least one speaker.
    pub frames_with_voice: u64,
}

/// One remote participant's audio.
struct Speaker {
    decoder: Decoder,
    /// Waiting packets by extended sequence number.
    packets: BTreeMap<u32, Vec<u8>>,
    /// The newest RTP sequence seen and its extended value.
    newest: Option<(u16, u32)>,
    /// The next sequence to play. `None` while collecting the start of a
    /// talk burst.
    next: Option<u32>,
    /// While collecting, the first sequence not yet played: anything older
    /// arriving now is late. (While playing, `next` is that boundary.)
    floor: Option<u32>,
    /// Decoded samples not yet played, interleaved stereo.
    pcm: VecDeque<i16>,
    /// Consecutive pulls that found the buffer empty.
    dry: u32,
    /// The fewest packets waiting at a pull in this drain window.
    shallowest: usize,
    /// Pulls so far in this drain window.
    window_pulls: u32,
    volume: f32,
}

impl Speaker {
    fn new() -> Result<Self> {
        Ok(Self {
            // A stereo decoder plays mono packets on both channels.
            decoder: Decoder::new(SAMPLE_RATE, Channels::Stereo)
                .map_err(|error| Error::Message(format!("opus decoder: {error}")))?,
            packets: BTreeMap::new(),
            newest: None,
            next: None,
            floor: None,
            pcm: VecDeque::with_capacity(FRAME_INTERLEAVED * 2),
            dry: 0,
            shallowest: usize::MAX,
            window_pulls: 0,
            volume: 1.0,
        })
    }

    /// Maps a 16-bit RTP sequence number onto a counter that does not wrap.
    fn extend(&mut self, sequence: u16) -> u32 {
        let Some((newest, extended)) = self.newest else {
            // Start high enough that an older packet arriving late still
            // maps to a positive number.
            let extended = (1 << 16) + u32::from(sequence);
            self.newest = Some((sequence, extended));
            return extended;
        };
        let delta = i32::from(sequence.wrapping_sub(newest) as i16);
        let value = extended.wrapping_add_signed(delta);
        if delta > 0 {
            self.newest = Some((sequence, value));
        }
        value
    }

    fn push(&mut self, sequence: u16, opus: &[u8], depths: Depths, stats: &mut ReceiveStats) {
        if opus.is_empty() {
            return;
        }
        if let Some((newest, _)) = self.newest
            && i32::from(sequence.wrapping_sub(newest) as i16).abs() > RESTART_JUMP
        {
            // The sender restarted its stream; nothing buffered relates to
            // what follows.
            self.packets.clear();
            self.pcm.clear();
            self.newest = None;
            self.next = None;
            self.floor = None;
        }
        let extended = self.extend(sequence);
        if self.next.or(self.floor).is_some_and(|next| extended < next) {
            stats.late += 1;
            return;
        }
        if self.packets.insert(extended, opus.to_vec()).is_none() {
            stats.packets += 1;
        }

        if self.packets.len() > depths.max {
            // Fallen behind: drop the oldest and continue from the target
            // depth, trading a small skip for staying current.
            let excess = self.packets.len() - depths.target;
            for _ in 0..excess {
                self.packets.pop_first();
            }
            stats.skipped += excess as u64;
            let first = self.packets.keys().next().copied();
            if self.next.is_some() {
                self.next = first;
            } else {
                self.floor = first;
            }
        }
    }

    /// Decodes one packet's worth of audio into `pcm`. Returns false when
    /// there is nothing to play right now.
    fn decode_next(&mut self, depths: Depths, stats: &mut ReceiveStats) -> bool {
        let next = match self.next {
            Some(next) => next,
            None => {
                if self.packets.len() < depths.target {
                    return false;
                }
                let Some(first) = self.packets.keys().next().copied() else {
                    return false;
                };
                self.next = Some(first);
                first
            }
        };

        if let Some(packet) = self.packets.remove(&next) {
            self.dry = 0;
            self.next = Some(next + 1);
            let mut buffer = vec![0i16; MAX_OPUS_SAMPLES * 2];
            return match self.decoder.decode(&packet, &mut buffer, false) {
                Ok(samples) => {
                    self.pcm.extend(&buffer[..samples * 2]);
                    samples > 0
                }
                Err(_) => {
                    stats.undecodable += 1;
                    self.conceal(None, stats)
                }
            };
        }

        let Some(first) = self.packets.keys().next().copied() else {
            // Nothing queued: the packet is late, the network stalled, or
            // the burst ended. Smooth over it, but keep waiting for `next`
            // rather than giving its slot away: a packet that is only late
            // still plays, a frame later, and the buffer comes out a frame
            // deeper, so the next delay this size is absorbed. (Giving the
            // slot away left the buffer empty for as long as the speaker
            // kept talking, and every late packet thereafter was concealed
            // and thrown away: robotic sound after a CPU stall.) Without
            // waiting, the slot is given away as before.
            self.dry += 1;
            if self.dry > depths.dry_pulls {
                self.next = None;
                // `next` itself has not played, either way.
                self.floor = Some(next);
                self.dry = 0;
                return false;
            }
            if !depths.wait {
                self.next = Some(next + 1);
            }
            return self.conceal(None, stats);
        };

        self.dry = 0;
        if first - next > MAX_CONCEALED_GAP {
            // Too much is missing to invent; resume at what did arrive.
            self.next = Some(first);
            return self.decode_next(depths, stats);
        }
        // `next` was lost. The packet after it may carry a low-bitrate copy.
        self.next = Some(next + 1);
        let following = self.packets.get(&(next + 1)).cloned();
        self.conceal(following.as_deref(), stats)
    }

    /// Fills one 20 ms frame for a missing packet: from `following`'s error
    /// correction data when given, otherwise by concealment.
    fn conceal(&mut self, following: Option<&[u8]>, stats: &mut ReceiveStats) -> bool {
        // The output length tells the decoder how much audio is missing.
        let mut buffer = vec![0i16; FRAME_INTERLEAVED];
        let decoded = match following {
            Some(packet) => self.decoder.decode(packet, &mut buffer, true),
            None => self.decoder.decode(&[], &mut buffer, false),
        };
        match decoded {
            Ok(samples) if samples > 0 => {
                if following.is_some() {
                    stats.recovered += 1;
                } else {
                    stats.concealed += 1;
                }
                self.pcm.extend(&buffer[..samples * 2]);
                true
            }
            _ => false,
        }
    }

    /// Adds this speaker's next 20 ms to `output`. Returns whether it had
    /// audio to contribute.
    fn mix_into(&mut self, output: &mut [i32], depths: Depths, stats: &mut ReceiveStats) -> bool {
        self.drain_excess(depths, stats);
        while self.pcm.len() < FRAME_INTERLEAVED {
            if !self.decode_next(depths, stats) {
                break;
            }
        }
        if self.pcm.len() < FRAME_INTERLEAVED {
            return false;
        }
        for (slot, sample) in output.iter_mut().zip(self.pcm.drain(..FRAME_INTERLEAVED)) {
            *slot += (f32::from(sample) * self.volume) as i32;
        }
        true
    }

    /// Sheds delay the buffer no longer needs. Waiting out late packets
    /// deepens the buffer; once the network and the machine have been
    /// steady for a whole window (never fewer than the target waiting), one
    /// frame is dropped, until the depth is back at the target.
    fn drain_excess(&mut self, depths: Depths, stats: &mut ReceiveStats) {
        let Some(next) = self.next.filter(|_| depths.drain_window > 0) else {
            self.shallowest = usize::MAX;
            self.window_pulls = 0;
            return;
        };
        self.shallowest = self.shallowest.min(self.packets.len());
        self.window_pulls += 1;
        if self.window_pulls < depths.drain_window {
            return;
        }
        let excess = self.shallowest > depths.target;
        self.shallowest = usize::MAX;
        self.window_pulls = 0;
        if excess && let Some(packet) = self.packets.remove(&next) {
            // Decoded and discarded rather than skipped, so the decoder's
            // state runs on without a gap.
            let mut buffer = vec![0i16; MAX_OPUS_SAMPLES * 2];
            let _ = self.decoder.decode(&packet, &mut buffer, false);
            self.next = Some(next + 1);
            stats.skipped += 1;
        }
    }
}

/// Every speaker in a call, mixed into one stream.
struct Mixer {
    speakers: HashMap<u64, Speaker>,
    master: f32,
    depths: Depths,
    stats: ReceiveStats,
}

impl Mixer {
    fn new() -> Self {
        Self {
            speakers: HashMap::new(),
            master: 1.0,
            depths: Buffering::DEFAULT.into(),
            stats: ReceiveStats::default(),
        }
    }

    fn push(&mut self, user_id: u64, sequence: u16, opus: &[u8]) -> Result<()> {
        let speaker = match self.speakers.entry(user_id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Speaker::new()?),
        };
        speaker.push(sequence, opus, self.depths, &mut self.stats);
        Ok(())
    }

    /// The next 20 ms of everyone, as interleaved stereo.
    fn mix(&mut self) -> Vec<i16> {
        let mut sum = vec![0i32; FRAME_INTERLEAVED];
        let mut voiced = false;
        for speaker in self.speakers.values_mut() {
            voiced |= speaker.mix_into(&mut sum, self.depths, &mut self.stats);
        }
        if voiced {
            self.stats.frames_with_voice += 1;
        }
        let master = self.master * crate::devices::output_gain();
        sum.into_iter()
            .map(|sample| {
                (sample as f32 * master).clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
            })
            .collect()
    }
}

/// Plays a stream that is generated on demand. The sound device's own clock
/// decides when the next frame is needed, so playback cannot drift against it
/// the way a timer would.
pub(crate) struct Output {
    pipeline: gst::Pipeline,
    /// Present while the playback carries the echo probe.
    _probe: Option<crate::processing::EchoProbeGuard>,
}

impl Output {
    pub(crate) fn start(next_frame: impl FnMut() -> Vec<i16> + Send + 'static) -> Result<Self> {
        Self::start_with(next_frame, false)
    }

    /// Plays a call. With `echo_probe`, the playback passes through the
    /// probe echo cancellation listens to (see [`crate::processing`]).
    pub(crate) fn start_with(
        mut next_frame: impl FnMut() -> Vec<i16> + Send + 'static,
        echo_probe: bool,
    ) -> Result<Self> {
        gst::init().map_err(|error| Error::Message(format!("gstreamer init: {error}")))?;

        // A short device buffer keeps the delay low; the jitter buffer, not
        // the sink, is what absorbs network timing.
        // The chosen output: an explicit env override, then the configured
        // device, then the default sink.
        let configured = std::env::var("KUBECORD_PLAYBACK_DEVICE")
            .ok()
            .filter(|device| !device.is_empty())
            .or_else(crate::devices::output_device);
        let sink = match configured {
            Some(device) => {
                format!("pulsesink device=\"{device}\" buffer-time=60000 latency-time=10000")
            }
            _ if gst::ElementFactory::find("pulsesink").is_some() => {
                "pulsesink buffer-time=60000 latency-time=10000".to_owned()
            }
            _ => "autoaudiosink".to_owned(),
        };
        let probe = echo_probe && crate::processing::echo_probe_available();
        let probe_element = if probe {
            format!(
                "webrtcechoprobe name={} ! audioconvert ! ",
                crate::processing::ECHO_PROBE
            )
        } else {
            String::new()
        };
        let element = gst::parse::launch(&format!(
            "appsrc name=src format=time ! audioconvert ! audioresample ! {probe_element}{sink}"
        ))
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
        // Hold at most one frame: the source asks again as soon as the sink
        // has taken it.
        appsrc.set_max_bytes(FRAME_INTERLEAVED as u64 * 2);

        let frame_duration = gst::ClockTime::from_mseconds(20);
        let mut frames: u64 = 0;
        appsrc.set_callbacks(
            AppSrcCallbacks::builder()
                .need_data(move |appsrc, _| {
                    let samples = next_frame();
                    let mut bytes = Vec::with_capacity(samples.len() * 2);
                    for sample in samples {
                        bytes.extend_from_slice(&sample.to_le_bytes());
                    }
                    let mut buffer = gst::Buffer::from_slice(bytes);
                    if let Some(buffer) = buffer.get_mut() {
                        buffer.set_pts(frame_duration * frames);
                        buffer.set_duration(frame_duration);
                    }
                    frames += 1;
                    let _ = appsrc.push_buffer(buffer);
                })
                .build(),
        );

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| Error::Message(format!("playback state: {error}")))?;
        Ok(Self {
            pipeline,
            _probe: probe.then(crate::processing::EchoProbeGuard::new),
        })
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// The receiving half of a call: push packets in as they arrive, and the
/// mixed result plays on the default output.
pub struct RemoteAudio {
    mixer: Arc<Mutex<Mixer>>,
    _output: Output,
}

impl RemoteAudio {
    pub fn start() -> Result<Self> {
        let mixer = Arc::new(Mutex::new(Mixer::new()));
        let source = mixer.clone();
        let output = Output::start_with(
            move || {
                let mut mixer = source.lock();
                // A settings change applies from the next frame.
                mixer.depths = buffering().into();
                mixer.mix()
            },
            true,
        )?;
        Ok(Self {
            mixer,
            _output: output,
        })
    }

    /// Queues one Opus packet from a participant.
    pub fn push(&self, user_id: u64, sequence: u16, opus: &[u8]) {
        if let Err(error) = self.mixer.lock().push(user_id, sequence, opus) {
            tracing::warn!(%error, user_id, "could not set up a decoder for remote audio");
        }
    }

    /// Forgets a participant who left.
    pub fn remove(&self, user_id: u64) {
        self.mixer.lock().speakers.remove(&user_id);
    }

    /// Volume of one participant, from silent to `MAX_USER_VOLUME` times
    /// normal. The mix is clipped, not wrapped, when a boost overdrives it.
    pub fn set_volume(&self, user_id: u64, volume: f32) {
        if let Some(speaker) = self.mixer.lock().speakers.get_mut(&user_id) {
            speaker.volume = volume.clamp(0.0, MAX_USER_VOLUME);
        }
    }

    /// Volume of everything, from 0.0 to 2.0. Zero is how deafen works.
    pub fn set_master_volume(&self, volume: f32) {
        self.mixer.lock().master = volume.clamp(0.0, 2.0);
    }

    pub fn stats(&self) -> ReceiveStats {
        self.mixer.lock().stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus2::{Application, Encoder};

    /// Consecutive 20 ms Opus packets of a 440 Hz tone.
    fn tone_packets(count: usize, fec: bool) -> Vec<Vec<u8>> {
        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Voip).unwrap();
        encoder.set_bitrate(opus2::Bitrate::Bits(64_000)).unwrap();
        if fec {
            encoder.set_inband_fec(true).unwrap();
            encoder.set_packet_loss_perc(30).unwrap();
        }
        (0..count)
            .map(|frame| {
                let pcm: Vec<i16> = (0..FRAME_SAMPLES)
                    .flat_map(|index| {
                        let time = (frame * FRAME_SAMPLES + index) as f32 / SAMPLE_RATE as f32;
                        let sample = ((time * 440.0 * std::f32::consts::TAU).sin() * 9000.0) as i16;
                        [sample, sample]
                    })
                    .collect();
                encoder.encode_vec(&pcm, 4000).unwrap()
            })
            .collect()
    }

    fn loud(frame: &[i16]) -> bool {
        frame.iter().any(|sample| sample.unsigned_abs() > 500)
    }

    #[test]
    fn playback_waits_for_the_target_depth_then_flows() {
        let packets = tone_packets(20, false);
        let mut mixer = Mixer::new();
        mixer.push(1, 100, &packets[0]).unwrap();
        mixer.push(1, 101, &packets[1]).unwrap();
        // Two packets are not enough to start.
        assert!(!loud(&mixer.mix()));
        // From here a packet arrives for every frame played, as on a call.
        let mut voiced = 0;
        for (offset, packet) in packets.iter().enumerate().skip(2) {
            mixer.push(1, 100 + offset as u16, packet).unwrap();
            voiced += usize::from(loud(&mixer.mix()));
        }
        // The decoder needs a frame or two to ramp up from silence.
        assert!(voiced >= 16, "only {voiced} of 18 frames had audio");
        assert_eq!(mixer.stats.concealed, 0);
        assert_eq!(mixer.stats.late, 0);
        assert_eq!(mixer.stats.skipped, 0);
    }

    #[test]
    fn reordered_packets_play_in_order() {
        let packets = tone_packets(10, false);
        let mut ordered = Mixer::new();
        let mut shuffled = Mixer::new();
        for (index, packet) in packets.iter().enumerate() {
            ordered.push(1, index as u16, packet).unwrap();
        }
        for index in [1usize, 0, 3, 2, 5, 4, 7, 6, 9, 8] {
            shuffled.push(1, index as u16, &packets[index]).unwrap();
        }
        for _ in 0..10 {
            assert_eq!(ordered.mix(), shuffled.mix());
        }
        assert_eq!(shuffled.stats.concealed + shuffled.stats.recovered, 0);
    }

    #[test]
    fn a_lost_packet_is_recovered_or_concealed() {
        for fec in [true, false] {
            let packets = tone_packets(12, fec);
            let mut mixer = Mixer::new();
            for (index, packet) in packets.iter().enumerate() {
                if index != 5 {
                    mixer.push(1, index as u16, packet).unwrap();
                }
            }
            let voiced = (0..12).filter(|_| loud(&mixer.mix())).count();
            assert!(voiced >= 10, "fec={fec}: only {voiced} frames had audio");
            let stats = mixer.stats;
            assert_eq!(stats.recovered + stats.concealed, 1, "fec={fec}: {stats:?}");
            if fec {
                assert_eq!(stats.recovered, 1, "the next packet's FEC should be used");
            }
        }
    }

    #[test]
    fn a_packet_that_arrives_after_its_slot_is_dropped() {
        let packets = tone_packets(8, false);
        let mut mixer = Mixer::new();
        for index in [0usize, 1, 2, 4, 5, 6, 7] {
            mixer.push(1, index as u16, &packets[index]).unwrap();
        }
        for _ in 0..5 {
            mixer.mix();
        }
        // Slot 3 has been concealed and played; the real packet is too late.
        mixer.push(1, 3, &packets[3]).unwrap();
        assert_eq!(mixer.stats.late, 1);
    }

    #[test]
    fn the_buffer_refills_between_talk_bursts() {
        let packets = tone_packets(12, false);
        let mut mixer = Mixer::new();
        for (index, packet) in packets.iter().take(6).enumerate() {
            mixer.push(1, index as u16, packet).unwrap();
        }
        // Play the burst out, then a long silence.
        for _ in 0..20 {
            mixer.mix();
        }
        assert!(!loud(&mixer.mix()));
        // The next burst continues the sequence; one packet must not play
        // alone, three must.
        mixer.push(1, 6, &packets[6]).unwrap();
        assert!(!loud(&mixer.mix()));
        mixer.push(1, 7, &packets[7]).unwrap();
        mixer.push(1, 8, &packets[8]).unwrap();
        let voiced = (0..3).filter(|_| loud(&mixer.mix())).count();
        assert!(voiced >= 2);
    }

    #[test]
    fn sequence_numbers_wrap_around() {
        let packets = tone_packets(10, false);
        let mut mixer = Mixer::new();
        for (index, packet) in packets.iter().enumerate() {
            mixer
                .push(1, 65_530u16.wrapping_add(index as u16), packet)
                .unwrap();
        }
        let voiced = (0..10).filter(|_| loud(&mixer.mix())).count();
        assert!(voiced >= 8);
        assert_eq!(mixer.stats.late + mixer.stats.concealed, 0);
    }

    #[test]
    fn a_backlog_is_trimmed_to_stay_current() {
        let packets = tone_packets(40, false);
        let mut mixer = Mixer::new();
        for (index, packet) in packets.iter().enumerate() {
            mixer.push(1, index as u16, packet).unwrap();
        }
        let speaker = &mixer.speakers[&1];
        assert!(speaker.packets.len() <= mixer.depths.max);
        assert!(mixer.stats.skipped > 0);
        // What is left is the newest audio.
        assert_eq!(*speaker.packets.keys().next_back().unwrap(), (1 << 16) + 39);
    }

    /// A call under CPU load: playback catches up after a stall and empties
    /// the buffer, then about one packet in four arrives just after the
    /// frame it belongs to was due. Returns the stats and how many of the 57
    /// frames after the stall had sound.
    fn stalled_call(buffering: Buffering) -> (ReceiveStats, usize) {
        let packets = tone_packets(60, false);
        let mut mixer = Mixer::new();
        mixer.depths = buffering.into();
        for (index, packet) in packets.iter().enumerate().take(3) {
            mixer.push(1, index as u16, packet).unwrap();
        }
        // The catch-up burst: three pulls with nothing new arriving.
        for _ in 0..3 {
            mixer.mix();
        }
        assert!(mixer.speakers[&1].packets.is_empty());
        let mut voiced = 0;
        for (index, packet) in packets.iter().enumerate().skip(3) {
            if index % 4 == 0 {
                voiced += usize::from(loud(&mixer.mix()));
                mixer.push(1, index as u16, packet).unwrap();
            } else {
                mixer.push(1, index as u16, packet).unwrap();
                voiced += usize::from(loud(&mixer.mix()));
            }
        }
        (mixer.stats, voiced)
    }

    #[test]
    fn after_a_stall_late_packets_still_play_and_the_buffer_recovers() {
        let (stats, voiced) = stalled_call(Buffering::DEFAULT);
        // The first late packet is waited for once; that frame of depth
        // absorbs every later one. None is thrown away.
        assert_eq!(stats.late, 0, "{stats:?}");
        assert_eq!(stats.concealed, 1, "{stats:?}");
        assert!(voiced >= 54, "only {voiced} of 57 frames had audio");
    }

    #[test]
    fn without_waiting_late_packets_are_concealed_and_dropped() {
        // Waiting switched off: what happened before it existed. Every late
        // packet costs a concealed frame and is thrown away.
        let (stats, _) = stalled_call(Buffering {
            late_wait_ms: 0,
            ..Buffering::DEFAULT
        });
        assert_eq!(stats.late, 14, "{stats:?}");
        assert_eq!(stats.concealed, 14, "{stats:?}");
    }

    #[test]
    fn a_deeper_starting_buffer_rides_out_the_stall() {
        // 100 ms collected before playing: the catch-up burst no longer
        // empties it, so nothing needs concealing.
        let packets = tone_packets(60, false);
        let mut mixer = Mixer::new();
        mixer.depths = Buffering {
            target_ms: 100,
            ..Buffering::DEFAULT
        }
        .into();
        for (index, packet) in packets.iter().enumerate().take(5) {
            mixer.push(1, index as u16, packet).unwrap();
        }
        for _ in 0..3 {
            mixer.mix();
        }
        for (index, packet) in packets.iter().enumerate().skip(5) {
            if index % 4 == 0 {
                mixer.mix();
                mixer.push(1, index as u16, packet).unwrap();
            } else {
                mixer.push(1, index as u16, packet).unwrap();
                mixer.mix();
            }
        }
        assert_eq!(
            mixer.stats.concealed + mixer.stats.late,
            0,
            "{:?}",
            mixer.stats
        );
    }

    #[test]
    fn settings_become_consistent_depths() {
        assert_eq!(
            Depths::from(Buffering::DEFAULT),
            Depths {
                target: 3,
                max: 15,
                wait: true,
                dry_pulls: 2,
                drain_window: 100,
            }
        );
        // A maximum under the target still leaves room to absorb jitter,
        // and a wait shorter than a packet still waits one.
        let odd = Depths::from(Buffering {
            target_ms: 200,
            max_ms: 100,
            late_wait_ms: 10,
            settle_ms: 0,
        });
        assert_eq!((odd.target, odd.max), (10, 12));
        assert!(odd.wait && odd.dry_pulls == 1);
        assert_eq!(odd.drain_window, 0);
        // Nothing below one packet.
        assert_eq!(
            Depths::from(Buffering {
                target_ms: 0,
                ..Buffering::DEFAULT
            })
            .target,
            1
        );
    }

    #[test]
    fn shedding_switched_off_keeps_the_extra_delay() {
        let packets = tone_packets(400, false);
        let mut mixer = Mixer::new();
        mixer.depths = Buffering {
            settle_ms: 0,
            ..Buffering::DEFAULT
        }
        .into();
        for (index, packet) in packets.iter().enumerate().take(10) {
            mixer.push(1, index as u16, packet).unwrap();
        }
        for (index, packet) in packets.iter().enumerate().skip(10) {
            mixer.push(1, index as u16, packet).unwrap();
            mixer.mix();
        }
        assert_eq!(mixer.speakers[&1].packets.len(), 10);
        assert_eq!(mixer.stats.skipped, 0);
    }

    #[test]
    fn a_packet_older_than_a_finished_burst_is_late() {
        let packets = tone_packets(6, false);
        let mut mixer = Mixer::new();
        for (index, packet) in packets.iter().enumerate() {
            mixer.push(1, index as u16, packet).unwrap();
        }
        // Play the burst out until the buffer starts collecting again.
        for _ in 0..12 {
            mixer.mix();
        }
        assert!(mixer.speakers[&1].next.is_none());
        mixer.push(1, 2, &packets[2]).unwrap();
        assert_eq!(mixer.stats.late, 1);
        assert!(mixer.speakers[&1].packets.is_empty());
    }

    #[test]
    fn extra_delay_drains_back_to_the_target_once_steady() {
        // A stall left eleven packets waiting at each pull; from then on one
        // arrives per frame played, so the extra 160 ms is no longer needed.
        let packets = tone_packets(1200, false);
        let mut mixer = Mixer::new();
        for (index, packet) in packets.iter().enumerate().take(10) {
            mixer.push(1, index as u16, packet).unwrap();
        }
        for (index, packet) in packets.iter().enumerate().skip(10) {
            mixer.push(1, index as u16, packet).unwrap();
            mixer.mix();
        }
        // The next arrival brings it to the target depth at the next pull.
        let depth = mixer.speakers[&1].packets.len() + 1;
        assert_eq!(depth, mixer.depths.target, "{:?}", mixer.stats);
        assert_eq!(mixer.stats.skipped, 8);
        assert_eq!(mixer.stats.concealed + mixer.stats.late, 0);
    }

    #[test]
    fn speakers_mix_and_volume_applies() {
        let packets = tone_packets(6, false);
        let mut one = Mixer::new();
        let mut two = Mixer::new();
        for (index, packet) in packets.iter().enumerate() {
            one.push(1, index as u16, packet).unwrap();
            two.push(1, index as u16, packet).unwrap();
            two.push(2, index as u16, packet).unwrap();
        }
        let peak = |frame: Vec<i16>| frame.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        let mut single = 0;
        let mut double = 0;
        for _ in 0..6 {
            single = single.max(peak(one.mix()));
            double = double.max(peak(two.mix()));
        }
        assert!(double > single + single / 2, "{double} vs {single}");

        let mut muted = Mixer::new();
        muted.master = 0.0;
        for (index, packet) in packets.iter().enumerate() {
            muted.push(1, index as u16, packet).unwrap();
        }
        assert!((0..6).all(|_| !loud(&muted.mix())));
    }

    #[test]
    fn voice_and_stream_keys_hold_independent_volumes() {
        // A person's voice and their shared stream sound ride separate mixer
        // keys (the UI derives one from the other), so each keeps its own
        // volume: silencing one leaves the other untouched.
        const VOICE: u64 = 1;
        const STREAM: u64 = 1 | 1 << 63;
        let packets = tone_packets(6, false);
        let mut mixer = Mixer::new();
        for (index, packet) in packets.iter().enumerate() {
            mixer.push(VOICE, index as u16, packet).unwrap();
            mixer.push(STREAM, index as u16, packet).unwrap();
        }
        // Setting one key's volume must not change the other's.
        mixer.speakers.get_mut(&VOICE).unwrap().volume = 0.25;
        mixer.speakers.get_mut(&STREAM).unwrap().volume = 2.0;
        assert_eq!(mixer.speakers[&VOICE].volume, 0.25);
        assert_eq!(mixer.speakers[&STREAM].volume, 2.0);

        // Silence the stream: the voice is still heard.
        mixer.speakers.get_mut(&STREAM).unwrap().volume = 0.0;
        mixer.speakers.get_mut(&VOICE).unwrap().volume = 1.0;
        assert!((0..6).any(|_| loud(&mixer.mix())));
    }

    #[test]
    fn garbage_does_not_stop_playback() {
        let packets = tone_packets(8, false);
        let mut mixer = Mixer::new();
        for (index, packet) in packets.iter().enumerate() {
            if index == 4 {
                // A stream of nonsense where a packet should be.
                mixer.push(1, index as u16, &[0xff; 40]).unwrap();
            } else {
                mixer.push(1, index as u16, packet).unwrap();
            }
        }
        let voiced = (0..8).filter(|_| loud(&mixer.mix())).count();
        assert!(voiced >= 6);
    }
}
