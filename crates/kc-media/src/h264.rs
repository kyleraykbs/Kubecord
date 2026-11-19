//! Making an encoder's H.264 into what Discord's receivers take as it is.
//!
//! Discord's clients receive video through WebRTC, which edits one thing in
//! a stream before decoding it. The sequence parameter set (SPS) has to
//! promise that frames come out in the order they went in and that the
//! decoder holds no more of them than it uses for reference; an SPS that
//! does not say so is rewritten on arrival to say it. In an end-to-end
//! encrypted call that edit is fatal: the SPS travels in the clear but is
//! covered by the frame's authentication, so the edited frame fails its
//! check and the viewer waits for a picture that never decrypts. x264
//! already writes the promise. VA-API encoders declare one frame more than
//! they reference, which is why their streams never loaded. Official
//! clients make the same edit before they encrypt, and so does
//! [`conform`], by the same rules (WebRTC's `SpsVuiRewriter`).
//!
//! Filler goes as well. Hardware encoders at a constant bitrate pad every
//! frame up to its share of the bitrate with filler units, bytes of `0xFF`
//! that carry nothing: a still screen would upload its whole bitrate in
//! padding.

use kc_voice::rtp::split_nalus;

const NAL_TYPE_MASK: u8 = 0x1F;
const SPS: u8 = 7;
const FILLER: u8 = 12;
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Profiles whose SPS carries chroma, bit depth and scaling fields.
const HIGH_PROFILES: [u32; 12] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134];

/// The access unit as it should be sent: without filler, and with an SPS
/// no receiver needs to edit. Anything else comes back byte for byte.
pub fn conform(frame: &[u8]) -> Vec<u8> {
    let units = split_nalus(frame);
    let mut changed = false;
    let kept: Vec<(&[u8], Option<Vec<u8>>)> = units
        .iter()
        .filter_map(|unit| match unit[0] & NAL_TYPE_MASK {
            FILLER => {
                changed = true;
                None
            }
            SPS => {
                let rewritten = rewrite_sps(unit);
                changed |= rewritten.is_some();
                Some((*unit, rewritten))
            }
            _ => Some((*unit, None)),
        })
        .collect();
    if !changed {
        return frame.to_vec();
    }
    let mut out = Vec::with_capacity(frame.len() + 16);
    for (unit, rewritten) in kept {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(rewritten.as_deref().unwrap_or(unit));
    }
    out
}

/// True when a receiver would rewrite this SPS unit.
pub fn receivers_rewrite(sps: &[u8]) -> bool {
    rewrite_sps(sps).is_some()
}

/// The SPS as a receiver would rewrite it, or `None` when it would leave
/// it alone (or it cannot be read, in which case it is best left alone).
fn rewrite_sps(unit: &[u8]) -> Option<Vec<u8>> {
    let (header, payload) = unit.split_first()?;
    let rbsp = without_emulation_prevention(payload);
    let mut source = Reader::new(&rbsp);
    let max_num_ref_frames = read_sps_up_to_vui(&mut source)?;
    // Everything before the flag that says whether a VUI follows is kept.
    let mut out = Writer::default();
    let mut head = Reader::new(&rbsp);
    for _ in 0..source.at {
        out.put(head.bit()?, 1);
    }
    let vui_present = source.bit()? == 1;
    out.put(1, 1);
    let rewritten;
    if !vui_present {
        // aspect_ratio_info, overscan_info, video_signal_type, chroma_loc,
        // timing_info, nal_hrd, vcl_hrd, pic_struct: all absent.
        out.put(0, 8);
        // bitstream_restriction_flag
        out.put(1, 1);
        put_bitstream_restriction(&mut out, max_num_ref_frames);
        rewritten = true;
    } else {
        // aspect_ratio_info_present_flag
        if copy(&mut source, &mut out, 1)? == 1 {
            // aspect_ratio_idc; 255 is an explicit width and height.
            if copy(&mut source, &mut out, 8)? == 255 {
                copy(&mut source, &mut out, 32)?;
            }
        }
        // overscan_info_present_flag, overscan_appropriate_flag
        if copy(&mut source, &mut out, 1)? == 1 {
            copy(&mut source, &mut out, 1)?;
        }
        // video_signal_type_present_flag
        if copy(&mut source, &mut out, 1)? == 1 {
            // video_format (3), video_full_range_flag (1)
            copy(&mut source, &mut out, 4)?;
            // colour_description_present_flag, then three bytes of it
            if copy(&mut source, &mut out, 1)? == 1 {
                copy(&mut source, &mut out, 24)?;
            }
        }
        // chroma_loc_info_present_flag
        if copy(&mut source, &mut out, 1)? == 1 {
            copy_ue(&mut source, &mut out)?;
            copy_ue(&mut source, &mut out)?;
        }
        // timing_info_present_flag
        if copy(&mut source, &mut out, 1)? == 1 {
            // num_units_in_tick, time_scale, fixed_frame_rate_flag
            copy(&mut source, &mut out, 32)?;
            copy(&mut source, &mut out, 32)?;
            copy(&mut source, &mut out, 1)?;
        }
        let nal_hrd = copy(&mut source, &mut out, 1)? == 1;
        if nal_hrd {
            copy_hrd(&mut source, &mut out)?;
        }
        let vcl_hrd = copy(&mut source, &mut out, 1)? == 1;
        if vcl_hrd {
            copy_hrd(&mut source, &mut out)?;
        }
        if nal_hrd || vcl_hrd {
            // low_delay_hrd_flag
            copy(&mut source, &mut out, 1)?;
        }
        // pic_struct_present_flag
        copy(&mut source, &mut out, 1)?;
        // bitstream_restriction_flag
        let restricted = source.bit()? == 1;
        out.put(1, 1);
        if !restricted {
            put_bitstream_restriction(&mut out, max_num_ref_frames);
            rewritten = true;
        } else {
            // motion_vectors_over_pic_boundaries_flag
            copy(&mut source, &mut out, 1)?;
            // max_bytes_per_pic_denom, max_bits_per_mb_denom,
            // log2_max_mv_length_horizontal, log2_max_mv_length_vertical
            for _ in 0..4 {
                copy_ue(&mut source, &mut out)?;
            }
            let max_num_reorder_frames = source.ue()?;
            let max_dec_frame_buffering = source.ue()?;
            out.put_ue(0);
            out.put_ue(max_num_ref_frames);
            rewritten = max_num_reorder_frames != 0 || max_dec_frame_buffering > max_num_ref_frames;
        }
    }
    if !rewritten {
        return None;
    }
    // What is left is the stop bit that ends the SPS; then up to a byte.
    let end = last_set_bit(&rbsp)? + 1;
    while source.at < end {
        out.put(source.bit()?, 1);
    }
    let mut unit = vec![*header];
    with_emulation_prevention(&out.finish(), &mut unit);
    Some(unit)
}

/// Reads an SPS up to the flag that says whether a VUI follows, and
/// returns its `max_num_ref_frames`.
fn read_sps_up_to_vui(source: &mut Reader) -> Option<u32> {
    let profile = source.bits(8)?;
    // constraint flags and reserved bits, level_idc
    source.bits(16)?;
    // seq_parameter_set_id
    source.ue()?;
    if HIGH_PROFILES.contains(&profile) {
        let chroma_format = source.ue()?;
        if chroma_format == 3 {
            // separate_colour_plane_flag
            source.bit()?;
        }
        // bit_depth_luma_minus8, bit_depth_chroma_minus8
        source.ue()?;
        source.ue()?;
        // qpprime_y_zero_transform_bypass_flag
        source.bit()?;
        // seq_scaling_matrix_present_flag
        if source.bit()? == 1 {
            let lists = if chroma_format == 3 { 12 } else { 8 };
            for list in 0..lists {
                // seq_scaling_list_present_flag
                if source.bit()? == 0 {
                    continue;
                }
                let (mut last, mut next) = (8i64, 8i64);
                for _ in 0..if list < 6 { 16 } else { 64 } {
                    if next != 0 {
                        next = (last + i64::from(source.se()?) + 256) % 256;
                    }
                    if next != 0 {
                        last = next;
                    }
                }
            }
        }
    }
    // log2_max_frame_num_minus4
    source.ue()?;
    match source.ue()? {
        // log2_max_pic_order_cnt_lsb_minus4
        0 => {
            source.ue()?;
        }
        1 => {
            // delta_pic_order_always_zero_flag
            source.bit()?;
            // offset_for_non_ref_pic, offset_for_top_to_bottom_field
            source.se()?;
            source.se()?;
            for _ in 0..source.ue()? {
                source.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = source.ue()?;
    // gaps_in_frame_num_value_allowed_flag
    source.bit()?;
    // pic_width_in_mbs_minus1, pic_height_in_map_units_minus1
    source.ue()?;
    source.ue()?;
    // frame_mbs_only_flag, and mb_adaptive_frame_field_flag without it
    if source.bit()? == 0 {
        source.bit()?;
    }
    // direct_8x8_inference_flag
    source.bit()?;
    // frame_cropping_flag and its four offsets
    if source.bit()? == 1 {
        for _ in 0..4 {
            source.ue()?;
        }
    }
    Some(max_num_ref_frames)
}

/// The restriction a receiver writes where there is none: the defaults,
/// no reordering, and no more frames held than are referenced.
fn put_bitstream_restriction(out: &mut Writer, max_num_ref_frames: u32) {
    // motion_vectors_over_pic_boundaries_flag
    out.put(1, 1);
    // max_bytes_per_pic_denom, max_bits_per_mb_denom
    out.put_ue(2);
    out.put_ue(1);
    // log2_max_mv_length_horizontal, log2_max_mv_length_vertical
    out.put_ue(16);
    out.put_ue(16);
    // max_num_reorder_frames, max_dec_frame_buffering
    out.put_ue(0);
    out.put_ue(max_num_ref_frames);
}

fn copy(source: &mut Reader, out: &mut Writer, count: u32) -> Option<u32> {
    let value = source.bits(count)?;
    out.put(value, count);
    Some(value)
}

fn copy_ue(source: &mut Reader, out: &mut Writer) -> Option<u32> {
    let value = source.ue()?;
    out.put_ue(value);
    Some(value)
}

fn copy_hrd(source: &mut Reader, out: &mut Writer) -> Option<()> {
    let cpb_cnt_minus1 = copy_ue(source, out)?;
    // bit_rate_scale, cpb_size_scale
    copy(source, out, 8)?;
    for _ in 0..=cpb_cnt_minus1 {
        // bit_rate_value_minus1, cpb_size_value_minus1, cbr_flag
        copy_ue(source, out)?;
        copy_ue(source, out)?;
        copy(source, out, 1)?;
    }
    // Four lengths of five bits each.
    copy(source, out, 20)?;
    Some(())
}

/// The position of the last bit set in `data`.
fn last_set_bit(data: &[u8]) -> Option<usize> {
    let byte = data.iter().rposition(|byte| *byte != 0)?;
    Some(byte * 8 + 7 - data[byte].trailing_zeros() as usize)
}

/// A unit's payload without the `03` bytes that keep it from looking like
/// a start code.
fn without_emulation_prevention(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    for &byte in payload {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

fn with_emulation_prevention(rbsp: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
}

/// Reads bits, most significant first.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = self.data.get(self.at / 8)?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(u32::from(bit))
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }
        Some(value)
    }

    /// An unsigned Exp-Golomb value.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + u64::from(self.bits(zeros)?)) as u32)
    }

    /// A signed Exp-Golomb value.
    fn se(&mut self) -> Option<i32> {
        let code = self.ue()?;
        let magnitude = code.div_ceil(2) as i32;
        Some(if code % 2 == 1 { magnitude } else { -magnitude })
    }
}

/// Writes bits, most significant first.
#[derive(Default)]
struct Writer {
    data: Vec<u8>,
    bits: usize,
}

impl Writer {
    fn put(&mut self, value: u32, count: u32) {
        for shift in (0..count).rev() {
            if self.bits.is_multiple_of(8) {
                self.data.push(0);
            }
            if (value >> shift) & 1 == 1 {
                *self.data.last_mut().expect("a byte was just pushed") |= 1 << (7 - self.bits % 8);
            }
            self.bits += 1;
        }
    }

    /// An unsigned Exp-Golomb value.
    fn put_ue(&mut self, value: u32) {
        let code = u64::from(value) + 1;
        let length = 64 - code.leading_zeros();
        self.put(0, length - 1);
        for shift in (0..length).rev() {
            self.put(((code >> shift) & 1) as u32, 1);
        }
    }

    /// The bytes written, the last padded with zero bits.
    fn finish(self) -> Vec<u8> {
        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SPS of a 1280×720 stream from `vah264enc` on a Radeon: one
    /// reference frame, but two frames of decoder buffering declared.
    const VAAPI_SPS: [u8; 29] = [
        0x27, 0x42, 0xc0, 0x1f, 0xab, 0x40, 0x28, 0x02, 0xdd, 0xff, 0x80, 0x00, 0x80, 0x00, 0x88,
        0x00, 0x00, 0x03, 0x00, 0x08, 0x00, 0x00, 0x03, 0x01, 0xe4, 0xf8, 0x44, 0x23, 0x70,
    ];

    /// The same stream's SPS from `x264enc`.
    const X264_SPS: [u8; 28] = [
        0x67, 0x42, 0xc0, 0x1f, 0xda, 0x01, 0x40, 0x16, 0xec, 0x05, 0xa8, 0x08, 0x08, 0x0a, 0x00,
        0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0x79, 0x1e, 0x30, 0x65, 0x40,
    ];

    /// `max_num_ref_frames`, and from the VUI's bitstream restriction
    /// `max_num_reorder_frames` and `max_dec_frame_buffering`.
    fn buffering(sps: &[u8]) -> (u32, Option<(u32, u32)>) {
        let rbsp = without_emulation_prevention(&sps[1..]);
        let mut source = Reader::new(&rbsp);
        let refs = read_sps_up_to_vui(&mut source).unwrap();
        if source.bit().unwrap() == 0 {
            return (refs, None);
        }
        let mut sink = Writer::default();
        let (source, out) = (&mut source, &mut sink);
        if copy(source, out, 1).unwrap() == 1 && copy(source, out, 8).unwrap() == 255 {
            copy(source, out, 32).unwrap();
        }
        if copy(source, out, 1).unwrap() == 1 {
            copy(source, out, 1).unwrap();
        }
        if copy(source, out, 1).unwrap() == 1 {
            copy(source, out, 4).unwrap();
            if copy(source, out, 1).unwrap() == 1 {
                copy(source, out, 24).unwrap();
            }
        }
        if copy(source, out, 1).unwrap() == 1 {
            copy_ue(source, out).unwrap();
            copy_ue(source, out).unwrap();
        }
        if copy(source, out, 1).unwrap() == 1 {
            copy(source, out, 32).unwrap();
            copy(source, out, 32).unwrap();
            copy(source, out, 1).unwrap();
        }
        let nal_hrd = copy(source, out, 1).unwrap() == 1;
        if nal_hrd {
            copy_hrd(source, out).unwrap();
        }
        let vcl_hrd = copy(source, out, 1).unwrap() == 1;
        if vcl_hrd {
            copy_hrd(source, out).unwrap();
        }
        if nal_hrd || vcl_hrd {
            copy(source, out, 1).unwrap();
        }
        copy(source, out, 1).unwrap();
        if source.bit().unwrap() == 0 {
            return (refs, None);
        }
        copy(source, out, 1).unwrap();
        for _ in 0..4 {
            copy_ue(source, out).unwrap();
        }
        let reorder = source.ue().unwrap();
        let held = source.ue().unwrap();
        (refs, Some((reorder, held)))
    }

    fn frame(units: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in units {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(unit);
        }
        out
    }

    #[test]
    fn a_hardware_sps_is_made_one_no_receiver_edits() {
        assert_eq!(buffering(&VAAPI_SPS), (1, Some((0, 2))));
        assert!(receivers_rewrite(&VAAPI_SPS));
        let fixed = rewrite_sps(&VAAPI_SPS).unwrap();
        assert_eq!(buffering(&fixed), (1, Some((0, 1))));
        assert!(!receivers_rewrite(&fixed));
        // The two values take the same bits, so nothing else moves: only
        // the last byte differs, `0 011 1` having become `0 010 1`.
        let mut expected = VAAPI_SPS;
        expected[28] = 0x50;
        assert_eq!(fixed, expected);
    }

    #[test]
    fn an_x264_sps_is_left_alone() {
        assert_eq!(buffering(&X264_SPS), (1, Some((0, 1))));
        assert!(!receivers_rewrite(&X264_SPS));
        let slice = [0x65, 0x88, 0x84, 0x00, 0x33];
        let keyframe = frame(&[&[0x09, 0x10], &X264_SPS, &[0x68, 0xce, 0x3c, 0x80], &slice]);
        assert_eq!(conform(&keyframe), keyframe);
    }

    #[test]
    fn an_sps_without_a_vui_gets_the_restriction() {
        // Baseline, level 3.1, 1280×720, one reference frame, no VUI.
        let mut out = Writer::default();
        out.put(66, 8);
        out.put(0xc0, 8);
        out.put(31, 8);
        out.put_ue(0); // seq_parameter_set_id
        out.put_ue(0); // log2_max_frame_num_minus4
        out.put_ue(2); // pic_order_cnt_type
        out.put_ue(1); // max_num_ref_frames
        out.put(0, 1); // gaps_in_frame_num_value_allowed_flag
        out.put_ue(79); // pic_width_in_mbs_minus1
        out.put_ue(44); // pic_height_in_map_units_minus1
        out.put(1, 1); // frame_mbs_only_flag
        out.put(1, 1); // direct_8x8_inference_flag
        out.put(0, 1); // frame_cropping_flag
        out.put(0, 1); // vui_parameters_present_flag
        out.put(1, 1); // stop bit
        let mut sps = vec![0x67];
        with_emulation_prevention(&out.finish(), &mut sps);
        assert_eq!(buffering(&sps), (1, None));

        let fixed = rewrite_sps(&sps).unwrap();
        assert_eq!(buffering(&fixed), (1, Some((0, 1))));
        assert!(!receivers_rewrite(&fixed));
        assert_ne!(*fixed.last().unwrap(), 0, "a unit must not end in zero");
    }

    #[test]
    fn reordering_is_written_out() {
        // The hardware SPS, but claiming two frames may come out of order.
        let rbsp = without_emulation_prevention(&VAAPI_SPS[1..]);
        let mut source = Reader::new(&rbsp);
        let mut out = Writer::default();
        // Up to max_num_reorder_frames: the last two values before the stop.
        let end = last_set_bit(&rbsp).unwrap();
        // ue(0) is one bit and ue(2) three, so they start four bits back.
        for _ in 0..end - 4 {
            out.put(source.bit().unwrap(), 1);
        }
        out.put_ue(2);
        out.put_ue(4);
        out.put(1, 1);
        let mut sps = vec![VAAPI_SPS[0]];
        with_emulation_prevention(&out.finish(), &mut sps);
        assert_eq!(buffering(&sps), (1, Some((2, 4))));

        let fixed = rewrite_sps(&sps).unwrap();
        assert_eq!(buffering(&fixed), (1, Some((0, 1))));
        assert!(!receivers_rewrite(&fixed));
    }

    #[test]
    fn filler_is_dropped_and_the_rest_kept_in_order() {
        let aud = [0x09, 0x10];
        let slice = [0x21, 0xe0, 0x12, 0x34, 0x00];
        let second = [0x21, 0x04, 0x56];
        let mut filler = vec![0x0c];
        filler.extend_from_slice(&[0xff; 600]);
        filler.push(0x80);
        let padded = frame(&[&aud, &slice, &second, &filler]);
        assert_eq!(conform(&padded), frame(&[&aud, &slice, &second]));
        // Conforming again changes nothing.
        assert_eq!(conform(&conform(&padded)), conform(&padded));
    }

    #[test]
    fn a_hardware_keyframe_keeps_everything_but_the_sps_fields() {
        let aud = [0x09, 0x10];
        let pps = [0x28, 0xce, 0x3c, 0x80];
        let slice = [0x25, 0x88, 0x84, 0x00, 0x33];
        let keyframe = frame(&[&aud, &VAAPI_SPS, &pps, &slice]);
        let sent = conform(&keyframe);
        let units = split_nalus(&sent);
        assert_eq!(units.len(), 4);
        assert_eq!(units[0], aud);
        assert!(!receivers_rewrite(units[1]));
        assert_eq!(units[2], pps);
        assert_eq!(units[3], slice);
    }

    #[test]
    fn something_that_is_not_an_sps_is_left_alone() {
        assert_eq!(rewrite_sps(&[0x67]), None);
        assert_eq!(rewrite_sps(&[0x67, 0x42, 0xc0]), None);
        let broken = frame(&[&[0x67, 0x42, 0xc0], &[0x65, 0x88]]);
        assert_eq!(conform(&broken), broken);
    }

    #[test]
    fn exp_golomb_values_survive_writing_and_reading() {
        let values = [0, 1, 2, 3, 7, 16, 255, 65_535, u32::MAX - 1];
        let mut out = Writer::default();
        for value in values {
            out.put_ue(value);
        }
        let data = out.finish();
        let mut source = Reader::new(&data);
        for value in values {
            assert_eq!(source.ue(), Some(value));
        }
    }

    #[test]
    fn emulation_prevention_survives_writing_and_reading() {
        let rbsp = [
            0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x02, 0x80,
        ];
        let mut escaped = Vec::new();
        with_emulation_prevention(&rbsp, &mut escaped);
        assert!(
            !escaped
                .windows(3)
                .any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2)
        );
        assert_eq!(without_emulation_prevention(&escaped), rbsp);
    }
}
