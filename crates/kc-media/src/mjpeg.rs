//! Checking a camera's MJPEG frames before they are decoded.
//!
//! A webcam sends each frame as a JPEG over USB. When part of one goes
//! missing on the way, the frame still arrives, shorter, and a JPEG decoder
//! does what it can: garbage from the hole onwards, then flat gray where the
//! data ran out. Decoders treat this as a warning, not an error, so the
//! damaged picture goes on to the encoder and everyone sees it.
//!
//! [`check`] walks a frame's entropy-coded data the way a decoder would,
//! without computing any pixels, and reports whether it holds together: every
//! code valid, every block within its 64 coefficients, the data ending
//! exactly where the last block does. A frame that fails is dropped and the
//! previous picture stays up for one frame instead.

/// What [`check`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every block decodes and the data ends where it should.
    Intact,
    /// Damaged in transit; do not decode it.
    Damaged(Damage),
    /// Not a kind of JPEG this check understands (progressive, no Huffman
    /// tables of its own, and so on). Only its framing was checked.
    Unchecked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Damage {
    /// Does not start with a JPEG header.
    NoStart,
    /// Stops before the end-of-image marker.
    Truncated,
    /// A header segment runs past the end or contradicts itself.
    BadHeader,
    /// A bit pattern that is no Huffman code.
    BadCode,
    /// A block with more than 64 coefficients.
    BlockOverrun,
    /// The data ran out, or a marker came, before the last block.
    EndedEarly,
    /// Data left over after the last block.
    TrailingData,
    /// A restart marker missing or out of order.
    BadRestart,
}

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DHT: u8 = 0xC4;
const DRI: u8 = 0xDD;

/// A Huffman table in the form the JPEG specification decodes with.
#[derive(Clone)]
struct Huffman {
    /// For each code length, one past the largest code of that length,
    /// left-aligned to 16 bits. Zero for lengths with no codes.
    limit: [u32; 17],
    /// For each code length, the first code of that length.
    first: [u32; 17],
    /// For each code length, where its symbols start in `symbols`.
    offset: [usize; 17],
    symbols: Vec<u8>,
    /// Codes of up to `FAST_BITS` bits: `length << 8 | symbol`, zero if
    /// longer.
    fast: Vec<u16>,
}

const FAST_BITS: u32 = 9;

impl Huffman {
    fn new(counts: &[u8], symbols: &[u8]) -> Option<Self> {
        let mut table = Self {
            limit: [0; 17],
            first: [0; 17],
            offset: [0; 17],
            symbols: symbols.to_vec(),
            fast: vec![0; 1 << FAST_BITS],
        };
        let mut code = 0u32;
        let mut index = 0usize;
        for length in 1..=16u32 {
            let count = u32::from(counts[length as usize - 1]);
            table.first[length as usize] = code;
            table.offset[length as usize] = index;
            if code + count > 1 << length {
                return None;
            }
            for _ in 0..count {
                if length <= FAST_BITS {
                    let symbol = *table.symbols.get(index)?;
                    let start = (code << (FAST_BITS - length)) as usize;
                    let span = 1usize << (FAST_BITS - length);
                    for entry in &mut table.fast[start..start + span] {
                        *entry = (length as u16) << 8 | u16::from(symbol);
                    }
                }
                code += 1;
                index += 1;
            }
            table.limit[length as usize] = code << (16 - length);
            code <<= 1;
        }
        (index == table.symbols.len()).then_some(table)
    }
}

/// Reads bits from entropy-coded data, undoing byte stuffing and stopping
/// at the first marker.
struct Bits<'a> {
    data: &'a [u8],
    position: usize,
    buffer: u64,
    /// Bits held in `buffer`, counted from its top.
    held: u32,
    /// A marker (or the end of the data) was reached; no more bits.
    ended: bool,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], position: usize) -> Self {
        Self {
            data,
            position,
            buffer: 0,
            held: 0,
            ended: false,
        }
    }

    fn fill(&mut self) {
        while self.held <= 56 && !self.ended {
            let Some(&byte) = self.data.get(self.position) else {
                self.ended = true;
                break;
            };
            if byte == 0xFF {
                match self.data.get(self.position + 1) {
                    Some(0) => self.position += 2,
                    _ => {
                        self.ended = true;
                        break;
                    }
                }
            } else {
                self.position += 1;
            }
            self.buffer |= u64::from(byte) << (56 - self.held);
            self.held += 8;
        }
    }

    /// Skips `count` bits; false if the data does not have them.
    fn skip(&mut self, count: u32) -> bool {
        if self.held < count {
            self.fill();
            if self.held < count {
                return false;
            }
        }
        self.buffer <<= count;
        self.held -= count;
        true
    }

    /// Decodes one symbol.
    fn symbol(&mut self, table: &Huffman) -> Result<u8, Damage> {
        if self.held < 16 {
            self.fill();
        }
        let window = (self.buffer >> 48) as u32;
        let entry = table.fast[(window >> (16 - FAST_BITS)) as usize];
        let (length, symbol) = if entry != 0 {
            (u32::from(entry >> 8), entry as u8)
        } else {
            let mut found = None;
            for length in FAST_BITS + 1..=16 {
                if window < table.limit[length as usize] {
                    let code = window >> (16 - length);
                    let index = table.offset[length as usize]
                        + (code - table.first[length as usize]) as usize;
                    found = table.symbols.get(index).map(|symbol| (length, *symbol));
                    break;
                }
            }
            found.ok_or(Damage::BadCode)?
        };
        if length > self.held {
            // The code was read out of padding past the end of the data.
            return Err(Damage::EndedEarly);
        }
        self.buffer <<= length;
        self.held -= length;
        Ok(symbol)
    }

    /// Drops the rest of the current byte and returns the position of the
    /// next unread byte in the data.
    fn align(&mut self) -> usize {
        let whole = self.held / 8;
        self.buffer = 0;
        self.held = 0;
        // Bytes still in the buffer were read ahead; none of them was a
        // stuffed 0xFF pair only when counting back is exact, so walk back.
        let mut position = self.position;
        for _ in 0..whole {
            position -= 1;
            if position >= 1 && self.data[position] == 0 && self.data[position - 1] == 0xFF {
                position -= 1;
            }
        }
        self.position = position;
        self.ended = false;
        position
    }
}

struct Component {
    id: u8,
    blocks: u32,
    dc: usize,
    ac: usize,
}

/// Checks one MJPEG frame.
pub fn check(data: &[u8]) -> Verdict {
    if data.len() < 4 || data[0] != 0xFF || data[1] != SOI {
        return Verdict::Damaged(Damage::NoStart);
    }
    // The end marker is looked for where the picture data ends, not at the
    // end of the buffer: cameras hand frames over in fixed-size buffers with
    // old data after the picture.

    let mut dc: [Option<Huffman>; 4] = [None, None, None, None];
    let mut ac: [Option<Huffman>; 4] = [None, None, None, None];
    let mut components: Vec<Component> = Vec::new();
    let (mut width, mut height) = (0u32, 0u32);
    let (mut h_max, mut v_max) = (1u32, 1u32);
    let mut restart_interval = 0u32;
    let mut baseline = false;
    let mut position = 2usize;

    loop {
        // A marker: 0xFF, possibly repeated as fill, then its code.
        if data.get(position) != Some(&0xFF) {
            return Verdict::Damaged(Damage::BadHeader);
        }
        while data.get(position + 1) == Some(&0xFF) {
            position += 1;
        }
        let Some(&marker) = data.get(position + 1) else {
            return Verdict::Damaged(Damage::Truncated);
        };
        position += 2;
        if marker == EOI {
            // No picture data at all.
            return Verdict::Damaged(Damage::Truncated);
        }
        let Some(length) = data
            .get(position..position + 2)
            .map(|bytes| usize::from(u16::from_be_bytes([bytes[0], bytes[1]])))
        else {
            return Verdict::Damaged(Damage::Truncated);
        };
        let Some(segment) = data.get(position + 2..position + length) else {
            return Verdict::Damaged(Damage::BadHeader);
        };
        position += length;
        match marker {
            // Baseline and extended sequential frames.
            0xC0 | 0xC1 => {
                if segment.len() < 6 {
                    return Verdict::Damaged(Damage::BadHeader);
                }
                height = u32::from(u16::from_be_bytes([segment[1], segment[2]]));
                width = u32::from(u16::from_be_bytes([segment[3], segment[4]]));
                let count = usize::from(segment[5]);
                if segment.len() < 6 + count * 3 {
                    return Verdict::Damaged(Damage::BadHeader);
                }
                components.clear();
                for entry in segment[6..6 + count * 3].as_chunks::<3>().0 {
                    let (h, v) = (u32::from(entry[1] >> 4), u32::from(entry[1] & 15));
                    if h == 0 || v == 0 {
                        return Verdict::Damaged(Damage::BadHeader);
                    }
                    h_max = h_max.max(h);
                    v_max = v_max.max(v);
                    components.push(Component {
                        id: entry[0],
                        blocks: h * v,
                        dc: 0,
                        ac: 0,
                    });
                }
                baseline = true;
            }
            // Other frame types (progressive, lossless, arithmetic).
            0xC2 | 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return Verdict::Unchecked,
            DHT => {
                let mut rest = segment;
                while !rest.is_empty() {
                    if rest.len() < 17 {
                        return Verdict::Damaged(Damage::BadHeader);
                    }
                    let (class, slot) = (rest[0] >> 4, usize::from(rest[0] & 15));
                    let counts = &rest[1..17];
                    let total: usize = counts.iter().map(|count| usize::from(*count)).sum();
                    let Some(symbols) = rest.get(17..17 + total) else {
                        return Verdict::Damaged(Damage::BadHeader);
                    };
                    if slot > 3 || class > 1 {
                        return Verdict::Damaged(Damage::BadHeader);
                    }
                    let Some(table) = Huffman::new(counts, symbols) else {
                        return Verdict::Damaged(Damage::BadHeader);
                    };
                    if class == 0 {
                        dc[slot] = Some(table);
                    } else {
                        ac[slot] = Some(table);
                    }
                    rest = &rest[17 + total..];
                }
            }
            DRI => {
                if segment.len() < 2 {
                    return Verdict::Damaged(Damage::BadHeader);
                }
                restart_interval = u32::from(u16::from_be_bytes([segment[0], segment[1]]));
            }
            SOS => {
                if !baseline || width == 0 || height == 0 {
                    return Verdict::Unchecked;
                }
                let count = usize::from(*segment.first().unwrap_or(&0));
                if segment.len() < 1 + count * 2 + 3 {
                    return Verdict::Damaged(Damage::BadHeader);
                }
                // Only a single scan carrying every component is walked.
                if count != components.len() {
                    return Verdict::Unchecked;
                }
                for (component, entry) in components
                    .iter_mut()
                    .zip(segment[1..1 + count * 2].as_chunks::<2>().0)
                {
                    if component.id != entry[0] {
                        return Verdict::Unchecked;
                    }
                    component.dc = usize::from(entry[1] >> 4);
                    component.ac = usize::from(entry[1] & 15);
                    if component.dc > 3 || component.ac > 3 {
                        return Verdict::Damaged(Damage::BadHeader);
                    }
                    // A camera relying on the decoder's default tables.
                    if dc[component.dc].is_none() || ac[component.ac].is_none() {
                        return Verdict::Unchecked;
                    }
                }
                break;
            }
            _ => {}
        }
    }

    let mcus = width.div_ceil(8 * h_max) * height.div_ceil(8 * v_max);
    let mut bits = Bits::new(data, position);
    for mcu in 0..mcus {
        if restart_interval != 0 && mcu != 0 && mcu % restart_interval == 0 {
            let at = bits.align();
            let expected = 0xD0 + ((mcu / restart_interval - 1) % 8) as u8;
            if data.get(at..at + 2) != Some(&[0xFF, expected]) {
                return Verdict::Damaged(Damage::BadRestart);
            }
            bits = Bits::new(data, at + 2);
        }
        for component in &components {
            let (Some(dc), Some(ac)) = (&dc[component.dc], &ac[component.ac]) else {
                return Verdict::Unchecked;
            };
            for _ in 0..component.blocks {
                let size = match bits.symbol(dc) {
                    Ok(size) => u32::from(size),
                    Err(damage) => return Verdict::Damaged(damage),
                };
                if size > 16 {
                    return Verdict::Damaged(Damage::BadCode);
                }
                if !bits.skip(size) {
                    return Verdict::Damaged(Damage::EndedEarly);
                }
                let mut coefficient = 1u32;
                while coefficient < 64 {
                    let symbol = match bits.symbol(ac) {
                        Ok(symbol) => symbol,
                        Err(damage) => return Verdict::Damaged(damage),
                    };
                    let (run, size) = (u32::from(symbol >> 4), u32::from(symbol & 15));
                    if size == 0 {
                        if run != 15 {
                            break;
                        }
                        coefficient += 16;
                    } else {
                        coefficient += run;
                        if coefficient > 63 {
                            return Verdict::Damaged(Damage::BlockOverrun);
                        }
                        if !bits.skip(size) {
                            return Verdict::Damaged(Damage::EndedEarly);
                        }
                        coefficient += 1;
                    }
                }
                if coefficient > 64 {
                    return Verdict::Damaged(Damage::BlockOverrun);
                }
            }
        }
    }

    // The last block ends in the byte before the end marker (fill bytes of
    // 0xFF may come between).
    let mut at = bits.align();
    while data.get(at) == Some(&0xFF) && data.get(at + 1) == Some(&0xFF) {
        at += 1;
    }
    if data.get(at..at + 2) == Some(&[0xFF, EOI]) {
        Verdict::Intact
    } else {
        Verdict::Damaged(Damage::TrailingData)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;

    /// JPEG frames of a moving test picture, made by GStreamer's encoder.
    fn frames(count: u32, width: u32, height: u32) -> Vec<Vec<u8>> {
        gst::init().unwrap();
        let pipeline = gst::parse::launch(&format!(
            "videotestsrc num-buffers={count} pattern=ball ! video/x-raw,width={width},height={height} \
             ! jpegenc ! appsink name=sink sync=false"
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let sink = pipeline
            .by_name("sink")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let mut frames = Vec::new();
        while let Ok(sample) = sink.pull_sample() {
            let buffer = sample.buffer().unwrap();
            frames.push(buffer.map_readable().unwrap().as_slice().to_vec());
        }
        pipeline.set_state(gst::State::Null).unwrap();
        frames
    }

    #[test]
    fn whole_frames_are_intact() {
        for (width, height) in [(320, 240), (322, 242), (64, 48)] {
            for frame in frames(3, width, height) {
                assert_eq!(check(&frame), Verdict::Intact, "{width}x{height}");
            }
        }
    }

    #[test]
    fn a_cut_off_frame_is_caught() {
        let frame = frames(1, 320, 240).remove(0);
        assert!(matches!(
            check(&frame[..frame.len() / 2]),
            Verdict::Damaged(_)
        ));
        assert_eq!(check(&frame[2..]), Verdict::Damaged(Damage::NoStart));
    }

    #[test]
    fn data_after_the_picture_is_ignored() {
        // A camera's buffer: the frame, then whatever was there before.
        let mut frames = frames(2, 320, 240);
        let mut buffer = frames.remove(0);
        buffer.extend_from_slice(&frames[0]);
        buffer.extend_from_slice(&[0xAB; 4096]);
        assert_eq!(check(&buffer), Verdict::Intact);
        // Cut short, the old data behind it must not make it pass.
        let whole = frames.remove(0);
        let mut short = whole[..whole.len() * 2 / 3].to_vec();
        short.extend_from_slice(&whole[whole.len() / 3..]);
        assert!(matches!(check(&short), Verdict::Damaged(_)));
    }

    #[test]
    fn a_frame_with_a_hole_is_caught() {
        // What a lost USB packet does: bytes missing from the middle, the
        // end marker still in place.
        let frame = frames(1, 640, 480).remove(0);
        let mut caught = 0;
        let mut tried = 0;
        for start in (frame.len() / 4..frame.len() * 3 / 4).step_by(97) {
            for hole in [64usize, 512, 1024] {
                if start + hole + 16 >= frame.len() {
                    continue;
                }
                let mut damaged = frame[..start].to_vec();
                damaged.extend_from_slice(&frame[start + hole..]);
                tried += 1;
                if matches!(check(&damaged), Verdict::Damaged(_)) {
                    caught += 1;
                }
            }
        }
        assert!(tried > 20);
        assert_eq!(caught, tried, "every hole should be noticed");
    }

    #[test]
    fn other_data_is_not_a_frame() {
        assert_eq!(check(&[]), Verdict::Damaged(Damage::NoStart));
        assert_eq!(check(&[0; 64]), Verdict::Damaged(Damage::NoStart));
    }
}
