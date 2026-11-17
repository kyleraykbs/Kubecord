//! SRTP for Discord's `rtpsize` AEAD modes.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit};
use chacha20poly1305::XChaCha20Poly1305;

use crate::error::{Error, Result};

pub const NONCE_SUFFIX: usize = 4;
pub const AUTH_TAG_LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Aes256GcmRtpsize,
    XChaCha20Poly1305Rtpsize,
}

impl Mode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "aead_aes256_gcm_rtpsize" => Some(Self::Aes256GcmRtpsize),
            "aead_xchacha20_poly1305_rtpsize" => Some(Self::XChaCha20Poly1305Rtpsize),
            _ => None,
        }
    }
}

/// Send-side SRTP state. Receive-side needs no counter: the nonce travels in
/// the last four bytes of each packet.
#[derive(Debug)]
pub struct Srtp {
    mode: Mode,
    key: [u8; 32],
    counter: u32,
}

impl Srtp {
    pub fn new(mode: Mode, key: &[u8]) -> Result<Self> {
        let key: [u8; 32] = key
            .try_into()
            .map_err(|_| Error::Message(format!("SRTP key is {} bytes, expected 32", key.len())))?;
        Ok(Self {
            mode,
            key,
            counter: 0,
        })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Encrypts one RTP payload. Returns the complete packet:
    /// unencrypted header, ciphertext + tag, four-byte nonce suffix.
    pub fn encrypt(&mut self, header: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
        self.counter = self.counter.wrapping_add(1);
        let mut nonce = [0u8; 24];
        nonce[..4].copy_from_slice(&self.counter.to_be_bytes());

        let ciphertext = match self.mode {
            Mode::Aes256GcmRtpsize => {
                let cipher = Aes256Gcm::new_from_slice(&self.key)
                    .map_err(|_| Error::Message("invalid AES key".into()))?;
                cipher
                    .encrypt(
                        aes_gcm::Nonce::from_slice(&nonce[..12]),
                        Payload {
                            msg: payload,
                            aad: header,
                        },
                    )
                    .map_err(|_| Error::Message("AES-GCM encryption failed".into()))?
            }
            Mode::XChaCha20Poly1305Rtpsize => {
                let cipher = XChaCha20Poly1305::new_from_slice(&self.key)
                    .map_err(|_| Error::Message("invalid XChaCha20 key".into()))?;
                cipher
                    .encrypt(
                        &nonce.into(),
                        Payload {
                            msg: payload,
                            aad: header,
                        },
                    )
                    .map_err(|_| Error::Message("XChaCha20 encryption failed".into()))?
            }
        };

        let mut packet = Vec::with_capacity(header.len() + ciphertext.len() + NONCE_SUFFIX);
        packet.extend_from_slice(header);
        packet.extend_from_slice(&ciphertext);
        packet.extend_from_slice(&nonce[..4]);
        Ok(packet)
    }

    /// Decrypts a complete RTP packet, returning the plaintext payload.
    pub fn decrypt(&self, packet: &[u8]) -> Result<Vec<u8>> {
        if packet.len() < 12 + AUTH_TAG_LEN + NONCE_SUFFIX {
            return Err(Error::Message("SRTP packet too short".into()));
        }

        let (header, _) = crate::rtp::RtpHeader::parse(packet)
            .ok_or_else(|| Error::Message("invalid RTP header".into()))?;
        let nonce = &packet[packet.len() - NONCE_SUFFIX..];
        let end = packet.len() - NONCE_SUFFIX;
        let encrypted = &packet[header.header_len..end];
        let aad = &packet[..header.header_len];

        let mut nonce_buffer = [0u8; 24];
        nonce_buffer[..4].copy_from_slice(nonce);

        match self.mode {
            Mode::Aes256GcmRtpsize => {
                let cipher = Aes256Gcm::new_from_slice(&self.key)
                    .map_err(|_| Error::Message("invalid AES key".into()))?;
                cipher
                    .decrypt(
                        aes_gcm::Nonce::from_slice(&nonce_buffer[..12]),
                        Payload {
                            msg: encrypted,
                            aad,
                        },
                    )
                    .map_err(|_| Error::Message("AES-GCM decryption failed".into()))
            }
            Mode::XChaCha20Poly1305Rtpsize => {
                let cipher = XChaCha20Poly1305::new_from_slice(&self.key)
                    .map_err(|_| Error::Message("invalid XChaCha20 key".into()))?;
                cipher
                    .decrypt(
                        &nonce_buffer.into(),
                        Payload {
                            msg: encrypted,
                            aad,
                        },
                    )
                    .map_err(|_| Error::Message("XChaCha20 decryption failed".into()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(mode: Mode) {
        let mut srtp = Srtp::new(mode, &[7u8; 32]).unwrap();
        let header = crate::rtp::RtpHeader::write(false, 120, 42, 960, 1234);
        let payload = b"opus frame";
        let packet = srtp.encrypt(&header, payload).unwrap();
        assert_eq!(&packet[..12], &header);
        let decrypted = srtp.decrypt(&packet).unwrap();
        assert_eq!(decrypted, payload);
    }

    #[test]
    fn aes_gcm_roundtrip() {
        roundtrip(Mode::Aes256GcmRtpsize);
    }

    #[test]
    fn xchacha_roundtrip() {
        roundtrip(Mode::XChaCha20Poly1305Rtpsize);
    }

    #[test]
    fn rtpsize_extension_bytes_are_inside_the_encrypted_region() {
        // Header: 12 bytes + ext preamble with two 32-bit words. The ext data
        // is encrypted along with the payload; the RTP payload starts after it.
        let mut aad = crate::rtp::RtpHeader::write(false, 120, 1, 0, 99).to_vec();
        aad[0] |= 0x10;
        aad.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x02]);
        let extension = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut plaintext = extension.to_vec();
        plaintext.extend_from_slice(b"opus");

        let mut srtp = Srtp::new(Mode::XChaCha20Poly1305Rtpsize, &[3u8; 32]).unwrap();
        let packet = srtp.encrypt(&aad, &plaintext).unwrap();
        let (header, _) = crate::rtp::RtpHeader::parse(&packet).unwrap();
        assert_eq!(header.header_len, 16);
        assert_eq!(header.payload_offset, 24);

        let decrypted = srtp.decrypt(&packet).unwrap();
        assert_eq!(
            &decrypted[header.payload_offset - header.header_len..],
            b"opus"
        );
    }

    #[test]
    fn tampering_fails() {
        let mut srtp = Srtp::new(Mode::XChaCha20Poly1305Rtpsize, &[9u8; 32]).unwrap();
        let header = crate::rtp::RtpHeader::write(false, 120, 1, 0, 1);
        let mut packet = srtp.encrypt(&header, b"secret").unwrap();
        let last = packet.len() - 6;
        packet[last] ^= 1;
        assert!(srtp.decrypt(&packet).is_err());
    }
}
