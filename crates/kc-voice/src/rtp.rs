//! RTP header parsing and H.264 packetisation.

/// RTP header length without CSRCs or extensions.
pub const HEADER_LEN: usize = 12;
/// Maximum RTP payload before fragmentation.
pub const MAX_PAYLOAD: usize = 1200;

pub const PT_OPUS: u8 = 120;
pub const PT_H264: u8 = 101;
pub const PT_H264_RTX: u8 = 102;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtpHeader {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    /// Bytes of the packet that stay unencrypted: the fixed header, CSRCs and
    /// the extension preamble when the X bit is set. Matches Discord's
    /// `rtpsize` modes and Abaddon's AAD length.
    pub header_len: usize,
    /// Start of the codec payload after SRTP decryption: the full header,
    /// including the whole extension block. Abaddon's GetPayloadOffset does
    /// the same; using `header_len` here feeds extension bytes to Opus/H.264.
    pub payload_offset: usize,
}

impl RtpHeader {
    pub fn parse(packet: &[u8]) -> Option<(Self, &[u8])> {
        if packet.len() < HEADER_LEN {
            return None;
        }
        let first = packet[0];
        if first >> 6 != 2 {
            return None;
        }
        let has_extension = first & 0x10 != 0;
        let csrc_count = (first & 0x0F) as usize;
        let base_len = HEADER_LEN + csrc_count * 4;
        let mut header_len = base_len;
        let mut payload_offset = base_len;
        if has_extension {
            if packet.len() < base_len + 4 {
                return None;
            }
            // Extension header: 2-byte profile, 2-byte length in 32-bit words.
            let words = u16::from_be_bytes([packet[base_len + 2], packet[base_len + 3]]) as usize;
            payload_offset = base_len + 4 + words * 4;
            header_len = base_len + 4;
            if packet.len() < payload_offset {
                return None;
            }
        }

        Some((
            Self {
                marker: packet[1] & 0x80 != 0,
                payload_type: packet[1] & 0x7F,
                sequence: u16::from_be_bytes([packet[2], packet[3]]),
                timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
                ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
                header_len,
                payload_offset,
            },
            &packet[payload_offset..],
        ))
    }

    pub fn write(
        marker: bool,
        payload_type: u8,
        sequence: u16,
        timestamp: u32,
        ssrc: u32,
    ) -> [u8; HEADER_LEN] {
        let mut header = [0u8; HEADER_LEN];
        header[0] = 0x80;
        header[1] = (payload_type & 0x7F) | if marker { 0x80 } else { 0 };
        header[2..4].copy_from_slice(&sequence.to_be_bytes());
        header[4..8].copy_from_slice(&timestamp.to_be_bytes());
        header[8..12].copy_from_slice(&ssrc.to_be_bytes());
        header
    }
}

/// Splits an Annex-B stream into NAL units without their start codes.
pub fn split_nalus(annexb: &[u8]) -> Vec<&[u8]> {
    let mut units: Vec<(usize, usize)> = Vec::new();
    let mut index = 0;
    while index + 3 <= annexb.len() {
        if annexb[index] == 0 && annexb[index + 1] == 0 && annexb[index + 2] == 1 {
            units.push((index, index + 3));
            index += 3;
        } else if index + 4 <= annexb.len()
            && annexb[index] == 0
            && annexb[index + 1] == 0
            && annexb[index + 2] == 0
            && annexb[index + 3] == 1
        {
            units.push((index, index + 4));
            index += 4;
        } else {
            index += 1;
        }
    }

    units
        .iter()
        .enumerate()
        .filter_map(|(position, (_code_start, nal_start))| {
            let end = units
                .get(position + 1)
                .map(|(next_code_start, _)| *next_code_start)
                .unwrap_or(annexb.len());
            (end > *nal_start).then_some(&annexb[*nal_start..end])
        })
        .collect()
}

pub fn is_keyframe(annexb: &[u8]) -> bool {
    split_nalus(annexb).iter().any(|nalu| {
        let kind = nalu[0] & 0x1F;
        kind == 5 || kind == 7
    })
}

/// Packetises one Annex-B access unit into RTP payloads.
///
/// The last payload of the frame is returned with `marker = true`.
pub fn packetize(annexb: &[u8], max_payload: usize) -> Vec<(Vec<u8>, bool)> {
    let nalus = split_nalus(annexb);
    let mut payloads = Vec::new();
    for nalu in &nalus {
        if nalu.is_empty() {
            continue;
        }
        if nalu.len() <= max_payload {
            payloads.push((nalu.to_vec(), false));
            continue;
        }

        let indicator = (nalu[0] & 0xE0) | 28;
        let kind = nalu[0] & 0x1F;
        let body = &nalu[1..];
        let capacity = max_payload - 2;
        let mut offset = 0;
        while offset < body.len() {
            let end = (offset + capacity).min(body.len());
            let start = offset == 0;
            let finish = end == body.len();
            let mut payload = Vec::with_capacity(2 + end - offset);
            payload.push(indicator);
            payload.push((if start { 0x80 } else { 0 }) | (if finish { 0x40 } else { 0 }) | kind);
            payload.extend_from_slice(&body[offset..end]);
            payloads.push((payload, false));
            offset = end;
        }
    }

    if let Some(last) = payloads.last_mut() {
        last.1 = true;
    }
    payloads
}

/// Reassembles H.264 RTP payloads into Annex-B access units.
#[derive(Default)]
pub struct H264Depacketizer {
    buffer: Vec<u8>,
    timestamp: Option<u32>,
}

impl H264Depacketizer {
    /// Returns a completed frame once the marker bit arrives or the timestamp
    /// changes before a frame was flushed.
    pub fn push(&mut self, payload: &[u8], timestamp: u32, marker: bool) -> Option<Vec<u8>> {
        let mut completed = None;
        if let Some(current) = self.timestamp
            && current != timestamp
            && !self.buffer.is_empty()
        {
            completed = Some(std::mem::take(&mut self.buffer));
        }
        self.timestamp = Some(timestamp);

        if payload.is_empty() {
            return completed;
        }

        let kind = payload[0] & 0x1F;
        match kind {
            1..=23 => {
                self.buffer.extend_from_slice(&[0, 0, 0, 1]);
                self.buffer.extend_from_slice(payload);
            }
            24 => {
                // STAP-A: [header][u16 len][NAL]...
                let mut offset = 1;
                while offset + 2 <= payload.len() {
                    let length =
                        u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
                    offset += 2;
                    if offset + length > payload.len() {
                        break;
                    }
                    self.buffer.extend_from_slice(&[0, 0, 0, 1]);
                    self.buffer
                        .extend_from_slice(&payload[offset..offset + length]);
                    offset += length;
                }
            }
            28 => {
                // FU-A: [indicator][fu header][data]
                if payload.len() < 3 {
                    return completed;
                }
                let start = payload[1] & 0x80 != 0;
                let end = payload[1] & 0x40 != 0;
                let reconstructed = (payload[0] & 0xE0) | (payload[1] & 0x1F);
                if start {
                    // Not a frame boundary: a frame is often parameter sets
                    // followed by a fragmented picture, or several slices.
                    self.buffer.extend_from_slice(&[0, 0, 0, 1, reconstructed]);
                } else if self.buffer.is_empty() {
                    // Lost the first fragment; wait for the next keyframe.
                    return completed;
                }
                self.buffer.extend_from_slice(&payload[2..]);
                if end && marker {
                    completed = Some(std::mem::take(&mut self.buffer));
                }
            }
            _ => {}
        }

        if marker && !self.buffer.is_empty() {
            completed = Some(std::mem::take(&mut self.buffer));
        }
        completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic random source for the test below.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn byte(&mut self) -> u8 {
            // Zeros and ones far more often than chance, to reach the
            // start-code-like cases quickly.
            match self.next() % 8 {
                0 | 1 => 0,
                2 => 1,
                _ => (self.next() & 0xFF) as u8,
            }
        }
    }

    /// True if `00 00 01` occurs in `data`.
    fn has_start_code(data: &[u8]) -> bool {
        data.windows(3).any(|window| window == [0, 0, 1])
    }

    /// An end-to-end encrypted frame is slices whose bodies are ciphertext:
    /// any bytes at all, except that the encryptor re-rolls until no start
    /// code appears in or around a body. Packetised and put back together
    /// the way a WebRTC receiver does (a long start code before each NAL),
    /// the frame must come out byte for byte, or it will not decrypt.
    #[test]
    fn encrypted_frames_survive_packetisation_exactly() {
        let mut random = Lcg(7);
        let mut failures = Vec::new();
        for trial in 0..200_000u32 {
            let mut frame = Vec::new();
            let nals = 1 + random.next() % 4;
            for nal in 0..nals {
                frame.extend_from_slice(&[0, 0, 0, 1]);
                // Header and the slice-header bytes left in the clear; real
                // ones never end in a zero byte pair.
                frame.push(0x41);
                frame.push(0x9A);
                loop {
                    let start = frame.len();
                    let length = 1 + (random.next() % 12) as usize;
                    for _ in 0..length {
                        frame.push(random.byte());
                    }
                    if nal + 1 == nals {
                        // The last body is followed by the trailer.
                        frame.extend_from_slice(&[0x0B, 0xFA, 0xFA]);
                    }
                    // What the encryptor checks: the body with two bytes of
                    // what surrounds it.
                    let mut window = frame[start - 2..].to_vec();
                    if nal + 1 != nals {
                        window.extend_from_slice(&[0, 0]);
                    }
                    if !has_start_code(&window) {
                        break;
                    }
                    frame.truncate(start);
                }
            }
            let mut rebuilt = Vec::new();
            for (payload, _) in packetize(&frame, 1200) {
                rebuilt.extend_from_slice(&[0, 0, 0, 1]);
                rebuilt.extend_from_slice(&payload);
            }
            if rebuilt != frame && failures.len() < 5 {
                failures.push((trial, frame, rebuilt));
            }
        }
        assert!(failures.is_empty(), "{failures:02x?}");
    }

    #[test]
    fn parses_header() {
        let header = RtpHeader::write(true, PT_OPUS, 7, 96_000, 1234);
        let (parsed, payload) = RtpHeader::parse(&header).unwrap();
        assert!(parsed.marker);
        assert_eq!(parsed.payload_type, PT_OPUS);
        assert_eq!(parsed.sequence, 7);
        assert_eq!(parsed.timestamp, 96_000);
        assert_eq!(parsed.ssrc, 1234);
        assert_eq!(parsed.header_len, 12);
        assert!(payload.is_empty());
    }

    #[test]
    fn parses_extension_length_and_payload_offset() {
        let mut packet = RtpHeader::write(false, PT_OPUS, 1, 960, 42).to_vec();
        packet[0] |= 0x10; // X bit
        packet.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x02]); // two 32-bit words
        packet.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        packet.extend_from_slice(b"opus");

        let (header, payload) = RtpHeader::parse(&packet).unwrap();
        assert_eq!(header.header_len, 16);
        assert_eq!(header.payload_offset, 24);
        assert_eq!(payload, b"opus");
    }

    #[test]
    fn splits_annex_b() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3];
        let nalus = split_nalus(&stream);
        assert_eq!(nalus, vec![&[0x67, 1, 2][..], &[0x68, 3][..]]);
    }

    #[test]
    fn packetizes_and_depacketizes_fu_a() {
        let frame = {
            let mut frame = vec![0, 0, 0, 1, 0x65];
            frame.extend(std::iter::repeat_n(0xAB, 5000));
            frame
        };
        let payloads = packetize(&frame, 1200);
        assert!(payloads.len() >= 4);

        let mut depacketizer = H264Depacketizer::default();
        let mut decoded = None;
        for (payload, marker) in payloads {
            if let Some(frame) = depacketizer.push(&payload, 90_000, marker) {
                decoded = Some(frame);
            }
        }
        assert_eq!(decoded.unwrap(), frame);
    }

    #[test]
    fn a_frame_of_several_nal_units_stays_one_frame() {
        // Parameter sets followed by a fragmented picture, as encoders send
        // keyframes. Splitting it would hand the decoder (and the end-to-end
        // decryptor) half a frame.
        let mut frame = vec![0, 0, 0, 1, 0x67, 1, 2, 3, 0, 0, 0, 1, 0x68, 4, 5];
        frame.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        frame.extend(std::iter::repeat_n(0xCD, 4000));
        frame.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        frame.extend(std::iter::repeat_n(0xEF, 2500));

        let mut depacketizer = H264Depacketizer::default();
        let mut frames = Vec::new();
        for (payload, marker) in packetize(&frame, 1200) {
            frames.extend(depacketizer.push(&payload, 90_000, marker));
        }
        assert_eq!(frames, vec![frame]);
    }

    #[test]
    fn detects_keyframes() {
        assert!(is_keyframe(&[0, 0, 0, 1, 0x65, 1]));
        assert!(!is_keyframe(&[0, 0, 0, 1, 0x41, 1]));
    }
}
