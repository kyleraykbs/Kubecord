//! Receiving H.264 video: putting packets back in order, asking for the
//! ones that went missing, and reassembling frames a decoder can take.
//!
//! A frame is spread over many RTP packets. A decoder shown a frame with a
//! piece missing, or a frame that builds on one it never saw, draws garbage
//! until the next keyframe. So only frames whose packets all arrived are
//! passed on, and after an unrecoverable loss everything is held back until
//! a keyframe, which the caller requests from the sender.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::rtp::{self, H264Depacketizer};

/// How long to wait for a missing packet to be retransmitted before giving
/// the frame up. The picture is frozen while waiting.
const GAP_WAIT: Duration = Duration::from_millis(250);
/// How often a still-missing packet is asked for again.
const NACK_INTERVAL: Duration = Duration::from_millis(80);
/// Packets held behind a gap before giving up regardless of time.
const MAX_HELD: usize = 2048;
/// Missing packets requested at once; a bigger hole is not worth repairing.
const MAX_NACKED: usize = 256;
/// This many packets in a row from "the past" mean the sender restarted
/// its numbering, not that they are late.
const LATE_RUN_RESET: u32 = 64;

/// One received video RTP packet, after transport decryption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub sequence: u16,
    pub timestamp: u32,
    pub marker: bool,
    pub payload: Vec<u8>,
}

/// Puts packets back in sequence order and tracks which are missing.
#[derive(Default)]
struct Reorder {
    /// Extended sequence number of the next packet to release.
    next: Option<u64>,
    held: BTreeMap<u64, Packet>,
    /// When the packet at `next` was first found missing.
    gap_since: Option<Instant>,
    /// When each missing packet was last asked for.
    requested: HashMap<u64, Instant>,
    late_run: u32,
}

impl Reorder {
    fn push(&mut self, packet: Packet) {
        let Some(next) = self.next else {
            // Start well above zero so earlier packets do not underflow.
            let start = 1 << 32 | packet.sequence as u64;
            self.next = Some(start);
            self.held.insert(start, packet);
            return;
        };
        let offset = packet.sequence.wrapping_sub(next as u16) as i16;
        if offset < 0 {
            // Already released or given up on (a duplicate, or too late).
            self.late_run += 1;
            if self.late_run >= LATE_RUN_RESET {
                *self = Self::default();
                self.push(packet);
            }
            return;
        }
        self.late_run = 0;
        if offset as usize > MAX_HELD * 2 {
            // Too far ahead to be loss: the sender jumped. Start over there.
            *self = Self::default();
            self.push(packet);
            return;
        }
        let extended = next + offset as u64;
        self.requested.remove(&extended);
        self.held.entry(extended).or_insert(packet);
    }

    /// The next packet in order, if it has arrived or its wait is over.
    fn pop(&mut self, now: Instant) -> Option<Packet> {
        let next = self.next?;
        let (&first, _) = self.held.first_key_value()?;
        if first != next {
            let since = *self.gap_since.get_or_insert(now);
            if now.duration_since(since) < GAP_WAIT && self.held.len() < MAX_HELD {
                return None;
            }
            // Give the missing packets up; the caller sees the jump.
            self.requested.retain(|sequence, _| *sequence > first);
        }
        self.gap_since = None;
        self.next = Some(first + 1);
        self.held.remove(&first)
    }

    /// Sequence numbers to ask the sender for again, oldest first.
    fn missing(&mut self, now: Instant) -> Vec<u16> {
        let (Some(next), Some((&last, _))) = (self.next, self.held.last_key_value()) else {
            return Vec::new();
        };
        if last - next > MAX_NACKED as u64 * 4 {
            return Vec::new();
        }
        let mut missing = Vec::new();
        for sequence in next..last {
            if self.held.contains_key(&sequence) {
                continue;
            }
            let due = self
                .requested
                .get(&sequence)
                .is_none_or(|at| now.duration_since(*at) >= NACK_INTERVAL);
            if due {
                self.requested.insert(sequence, now);
                missing.push(sequence as u16);
                if missing.len() >= MAX_NACKED {
                    break;
                }
            }
        }
        missing
    }
}

/// What a received packet led to.
#[derive(Debug, PartialEq, Eq)]
pub enum Assembled {
    /// A complete frame in Annex-B form, with its RTP timestamp (90 kHz).
    /// With DAVE it is still encrypted.
    Frame(Vec<u8>, u32),
    /// Packets were lost for good: request a keyframe.
    Lost,
}

/// Turns in-order packets into whole frames, dropping damaged ones.
struct FrameAssembler {
    depacketizer: H264Depacketizer,
    /// The sequence number the next packet should carry.
    expected: Option<u16>,
    /// False while the frame being received is known to be incomplete.
    intact: bool,
    /// Timestamp of the frame being received.
    timestamp: u32,
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self {
            depacketizer: H264Depacketizer::default(),
            expected: None,
            intact: true,
            timestamp: 0,
        }
    }
}

impl FrameAssembler {
    fn push(&mut self, packet: &Packet) -> Option<Assembled> {
        let lost = self
            .expected
            .is_some_and(|expected| expected != packet.sequence);
        self.expected = Some(packet.sequence.wrapping_add(1));
        if lost {
            self.depacketizer = H264Depacketizer::default();
            // The hole may be the tail of the previous frame or the head of
            // this one; this packet's frame is only safe if it starts here,
            // which a packet alone cannot prove. Drop through to its marker.
            self.intact = false;
        }
        if !self.intact {
            if packet.marker {
                self.intact = true;
            }
            return lost.then_some(Assembled::Lost);
        }
        // A frame normally ends on its own marker packet; one that ends
        // because the next frame began carries the earlier timestamp.
        let timestamp = if packet.marker {
            packet.timestamp
        } else {
            self.timestamp
        };
        self.timestamp = packet.timestamp;
        self.depacketizer
            .push(&packet.payload, packet.timestamp, packet.marker)
            .map(|frame| Assembled::Frame(frame, timestamp))
    }
}

/// Receive state for one sender's video.
pub struct VideoStream {
    /// The primary SSRC this state belongs to. A sender that switches
    /// quality layers changes SSRC and starts over.
    pub ssrc: u32,
    reorder: Reorder,
    assembler: FrameAssembler,
    /// True until a keyframe gets through; later frames depend on it.
    needs_keyframe: bool,
    keyframe_requested: Option<Instant>,
    pub last_packet: Instant,
}

impl VideoStream {
    pub fn new(ssrc: u32, now: Instant) -> Self {
        Self {
            ssrc,
            reorder: Reorder::default(),
            assembler: FrameAssembler::default(),
            needs_keyframe: true,
            keyframe_requested: None,
            last_packet: now,
        }
    }

    /// Takes one packet (a retransmission carries its original sequence
    /// number) and appends whatever became ready to `out`.
    pub fn push(&mut self, packet: Packet, now: Instant, out: &mut Vec<Assembled>) {
        self.last_packet = now;
        self.reorder.push(packet);
        self.poll(now, out);
    }

    /// Releases packets whose wait for a retransmission has run out.
    pub fn poll(&mut self, now: Instant, out: &mut Vec<Assembled>) {
        while let Some(packet) = self.reorder.pop(now) {
            if let Some(result) = self.assembler.push(&packet) {
                if result == Assembled::Lost {
                    self.needs_keyframe = true;
                }
                out.push(result);
            }
        }
    }

    /// Sequence numbers to request again from the sender.
    pub fn missing(&mut self, now: Instant) -> Vec<u16> {
        self.reorder.missing(now)
    }

    /// Whether a decrypted frame may go to the decoder. Until a keyframe
    /// arrives, frames that depend on earlier ones are held back.
    pub fn admit(&mut self, frame: &[u8]) -> bool {
        if self.needs_keyframe && !rtp::is_keyframe(frame) {
            return false;
        }
        self.needs_keyframe = false;
        self.keyframe_requested = None;
        true
    }

    /// The stream is undecodable from here (a frame could not be
    /// decrypted): wait for a keyframe.
    pub fn require_keyframe(&mut self) {
        self.needs_keyframe = true;
    }

    /// Whether to ask the sender for a keyframe now. Asks at most once per
    /// `interval`, and only while one is needed and video is arriving.
    pub fn should_request_keyframe(&mut self, now: Instant, interval: Duration) -> bool {
        if !self.needs_keyframe || now.duration_since(self.last_packet) > Duration::from_secs(2) {
            return false;
        }
        if self
            .keyframe_requested
            .is_some_and(|at| now.duration_since(at) < interval)
        {
            return false;
        }
        self.keyframe_requested = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDR: u8 = 0x65;
    const P: u8 = 0x41;

    /// An access unit with one NAL of `kind` and `size` payload bytes.
    fn frame(kind: u8, size: usize) -> Vec<u8> {
        let mut frame = vec![0, 0, 0, 1, kind];
        frame.extend(std::iter::repeat_n(0xAB, size));
        frame
    }

    /// Packetises frames with sequence numbers that cross the wrap.
    fn packets(frames: &[Vec<u8>]) -> Vec<Packet> {
        let mut sequence = 65_530u16;
        let mut out = Vec::new();
        for (index, frame) in frames.iter().enumerate() {
            for (payload, marker) in rtp::packetize(frame, 1200) {
                out.push(Packet {
                    sequence,
                    timestamp: index as u32 * 3000,
                    marker,
                    payload,
                });
                sequence = sequence.wrapping_add(1);
            }
        }
        out
    }

    fn frames_of(out: &[Assembled]) -> Vec<Vec<u8>> {
        out.iter()
            .filter_map(|item| match item {
                Assembled::Frame(frame, _) => Some(frame.clone()),
                Assembled::Lost => None,
            })
            .collect()
    }

    #[test]
    fn complete_frames_come_out_whole() {
        let frames = vec![frame(IDR, 5000), frame(P, 800), frame(P, 3000)];
        let now = Instant::now();
        let mut stream = VideoStream::new(1, now);
        let mut out = Vec::new();
        for packet in packets(&frames) {
            stream.push(packet, now, &mut out);
        }
        assert_eq!(frames_of(&out), frames);
        assert!(stream.missing(now).is_empty());
    }

    #[test]
    fn reordered_packets_are_put_back_in_order() {
        let frames = vec![frame(IDR, 5000), frame(P, 3000)];
        let mut all = packets(&frames);
        all.swap(1, 3);
        all.swap(5, 6);
        let now = Instant::now();
        let mut stream = VideoStream::new(1, now);
        let mut out = Vec::new();
        for packet in all {
            stream.push(packet, now, &mut out);
        }
        assert_eq!(frames_of(&out), frames);
    }

    #[test]
    fn a_missing_packet_is_requested_and_repaired_by_retransmission() {
        let frames = vec![frame(IDR, 5000), frame(P, 800)];
        let all = packets(&frames);
        let start = Instant::now();
        let mut stream = VideoStream::new(1, start);
        let mut out = Vec::new();
        for (index, packet) in all.iter().enumerate() {
            if index != 2 {
                stream.push(packet.clone(), start, &mut out);
            }
        }
        // Nothing can be released past the hole yet.
        assert!(out.is_empty());
        assert_eq!(stream.missing(start), vec![all[2].sequence]);
        // Not asked again until the interval has passed.
        assert!(stream.missing(start).is_empty());
        assert_eq!(stream.missing(start + NACK_INTERVAL), vec![all[2].sequence]);

        stream.push(all[2].clone(), start + Duration::from_millis(90), &mut out);
        assert_eq!(frames_of(&out), frames);
        assert!(!out.contains(&Assembled::Lost));
    }

    #[test]
    fn an_unrepaired_loss_drops_the_frame_and_waits_for_a_keyframe() {
        let frames = vec![
            frame(IDR, 100),
            frame(P, 3000),
            frame(P, 100),
            frame(IDR, 100),
        ];
        let all = packets(&frames);
        let start = Instant::now();
        let mut stream = VideoStream::new(1, start);
        let mut out = Vec::new();
        for (index, packet) in all.iter().enumerate() {
            // Lose a middle packet of the big P frame (packets 1..=3).
            if index != 2 {
                stream.push(packet.clone(), start, &mut out);
            }
        }
        assert_eq!(frames_of(&out), vec![frames[0].clone()]);
        assert!(stream.admit(&frames[0]));

        // The wait runs out: the damaged frame is dropped and reported.
        out.clear();
        stream.poll(start + GAP_WAIT, &mut out);
        assert_eq!(out[0], Assembled::Lost);
        assert_eq!(frames_of(&out), vec![frames[2].clone(), frames[3].clone()]);
        // The decoder must not see a frame that builds on the lost one.
        assert!(!stream.admit(&frames[2]));
        assert!(stream.should_request_keyframe(start + GAP_WAIT, Duration::from_millis(500)));
        assert!(!stream.should_request_keyframe(start + GAP_WAIT, Duration::from_millis(500)));
        assert!(stream.admit(&frames[3]));
        assert!(stream.admit(&frames[2]), "after a keyframe, P frames flow");
    }

    #[test]
    fn a_new_stream_waits_for_its_first_keyframe() {
        let now = Instant::now();
        let mut stream = VideoStream::new(1, now);
        assert!(!stream.admit(&frame(P, 10)));
        assert!(stream.should_request_keyframe(now, Duration::from_millis(500)));
        assert!(stream.admit(&frame(IDR, 10)));
        assert!(!stream.should_request_keyframe(now, Duration::from_millis(500)));
        stream.require_keyframe();
        assert!(!stream.admit(&frame(P, 10)));
    }

    #[test]
    fn duplicates_are_ignored() {
        let frames = vec![frame(IDR, 100), frame(P, 100)];
        let all = packets(&frames);
        let now = Instant::now();
        let mut stream = VideoStream::new(1, now);
        let mut out = Vec::new();
        stream.push(all[0].clone(), now, &mut out);
        stream.push(all[0].clone(), now, &mut out);
        stream.push(all[1].clone(), now, &mut out);
        stream.push(all[1].clone(), now, &mut out);
        assert_eq!(frames_of(&out), frames);
    }

    #[test]
    fn a_sender_that_restarts_its_numbering_is_followed() {
        let now = Instant::now();
        let mut stream = VideoStream::new(1, now);
        let mut out = Vec::new();
        let whole = frame(IDR, 10);
        let (payload, _) = rtp::packetize(&whole, 1200).remove(0);
        let packet = |sequence: u16| Packet {
            sequence,
            timestamp: sequence as u32 * 3000,
            marker: true,
            payload: payload.clone(),
        };
        stream.push(packet(40_000), now, &mut out);
        assert_eq!(frames_of(&out).len(), 1);
        // Far "behind": ignored at first, then accepted as the new numbering.
        for sequence in 30_000..30_000 + LATE_RUN_RESET as u16 + 4 {
            stream.push(packet(sequence), now, &mut out);
        }
        // The packet that proves the restart is dropped with the hole
        // before it; the four after it come through.
        assert_eq!(frames_of(&out).len(), 5);
        // Far ahead: followed at once rather than waited for.
        stream.push(packet(50_000), now, &mut out);
        stream.push(packet(50_001), now, &mut out);
        assert_eq!(frames_of(&out).len(), 6);
        assert!(stream.missing(now).is_empty());
    }
}
