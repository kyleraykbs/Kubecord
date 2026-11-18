//! RTCP feedback a video receiver sends: keyframe requests and requests
//! for packets that went missing.

/// An empty receiver report. Feedback travels in a compound packet, and a
/// compound packet starts with a report.
fn receiver_report(sender_ssrc: u32) -> [u8; 8] {
    let mut packet = [0x80, 201, 0, 1, 0, 0, 0, 0];
    packet[4..].copy_from_slice(&sender_ssrc.to_be_bytes());
    packet
}

fn feedback_header(format: u8, packet_type: u8, words: u16, sender: u32, media: u32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(4 * (words as usize + 1));
    packet.push(0x80 | format);
    packet.push(packet_type);
    packet.extend_from_slice(&words.to_be_bytes());
    packet.extend_from_slice(&sender.to_be_bytes());
    packet.extend_from_slice(&media.to_be_bytes());
    packet
}

/// Picture Loss Indication: asks the sender of `media_ssrc` for a keyframe.
pub fn picture_loss(sender_ssrc: u32, media_ssrc: u32) -> Vec<u8> {
    let mut packet = receiver_report(sender_ssrc).to_vec();
    packet.extend(feedback_header(1, 206, 2, sender_ssrc, media_ssrc));
    packet
}

/// Generic NACK: asks for the listed packets of `media_ssrc` again.
/// `missing` must be in ascending order (wrap-around aware).
pub fn nack(sender_ssrc: u32, media_ssrc: u32, missing: &[u16]) -> Vec<u8> {
    // Each entry names one packet and, as a bitmask, any of the 16 after it.
    let mut entries: Vec<(u16, u16)> = Vec::new();
    for &sequence in missing {
        match entries.last_mut() {
            Some((first, mask)) if (1..=16).contains(&sequence.wrapping_sub(*first)) => {
                *mask |= 1 << (sequence.wrapping_sub(*first) - 1);
            }
            Some((first, _)) if sequence == *first => {}
            _ => entries.push((sequence, 0)),
        }
    }
    let mut packet = receiver_report(sender_ssrc).to_vec();
    packet.extend(feedback_header(
        1,
        205,
        2 + entries.len() as u16,
        sender_ssrc,
        media_ssrc,
    ));
    for (first, mask) in entries {
        packet.extend_from_slice(&first.to_be_bytes());
        packet.extend_from_slice(&mask.to_be_bytes());
    }
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picture_loss_is_a_report_followed_by_the_request() {
        let packet = picture_loss(0x0102_0304, 0x0A0B_0C0D);
        assert_eq!(packet.len(), 20);
        assert_eq!(&packet[..8], &[0x80, 201, 0, 1, 1, 2, 3, 4]);
        assert_eq!(&packet[8..12], &[0x81, 206, 0, 2]);
        assert_eq!(&packet[12..16], &[1, 2, 3, 4]);
        assert_eq!(&packet[16..], &[0x0A, 0x0B, 0x0C, 0x0D]);
    }

    #[test]
    fn nack_packs_neighbours_into_one_entry() {
        // 100, 101 and 116 fit one entry; 117 starts the next.
        let packet = nack(1, 2, &[100, 101, 116, 117]);
        let feedback = &packet[8..];
        assert_eq!(&feedback[..4], &[0x81, 205, 0, 4]);
        assert_eq!(&feedback[12..14], &100u16.to_be_bytes());
        assert_eq!(&feedback[14..16], &0b1000_0000_0000_0001u16.to_be_bytes());
        assert_eq!(&feedback[16..18], &117u16.to_be_bytes());
        assert_eq!(&feedback[18..20], &0u16.to_be_bytes());
        // The length field counts 32-bit words after the first.
        assert_eq!(feedback.len(), 4 * (4 + 1));
    }

    #[test]
    fn nack_handles_sequence_wrap() {
        let packet = nack(1, 2, &[65_535, 0, 1]);
        let feedback = &packet[8..];
        assert_eq!(&feedback[12..14], &65_535u16.to_be_bytes());
        assert_eq!(&feedback[14..16], &0b11u16.to_be_bytes());
        assert_eq!(feedback.len(), 16);
    }
}
