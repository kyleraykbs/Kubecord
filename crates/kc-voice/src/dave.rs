//! DAVE (MLS) session state for the voice gateway.
//!
//! The voice gateway speaks JSON for lifecycle opcodes (21–24) and binary
//! frames for MLS messages (25–31). This module owns the `davey` session and
//! the transition bookkeeping; the connection module owns the wire.

use std::collections::HashMap;
use std::num::NonZeroU16;

use davey::{Codec, DaveSession, MediaType, ProposalsOperationType};

use crate::error::{Error, Result};

pub struct Dave {
    session: DaveSession,
    version: u16,
    /// transition id -> protocol version, for transitions we announced ready for.
    pending: HashMap<u16, u16>,
}

impl Dave {
    pub fn new(version: u16, user_id: u64, channel_id: u64) -> Result<Self> {
        let version = NonZeroU16::new(version)
            .ok_or_else(|| Error::Message("DAVE version 0 cannot create a session".into()))?;
        let session = DaveSession::new(version, user_id, channel_id, None)
            .map_err(|error| Error::Message(format!("DAVE init: {error}")))?;
        Ok(Self {
            session,
            version: version.get(),
            pending: HashMap::new(),
        })
    }

    pub fn version(&self) -> u16 {
        self.version
    }

    pub fn is_ready(&self) -> bool {
        self.session.is_ready()
    }

    pub fn key_package(&mut self) -> Result<Vec<u8>> {
        let package = self
            .session
            .create_key_package()
            .map_err(|error| Error::Message(format!("DAVE key package: {error}")))?;
        tracing::info!(
            version = self.version,
            size = package.len(),
            "DAVE key package"
        );
        Ok(package)
    }

    pub fn set_external_sender(&mut self, data: &[u8]) -> Result<()> {
        self.session
            .set_external_sender(data)
            .map_err(|error| Error::Message(format!("DAVE external sender: {error}")))?;
        tracing::info!(size = data.len(), "DAVE external sender set");
        Ok(())
    }

    /// Processes proposals; returns the serialized commit (+ welcome when one
    /// was produced) for `MLS_COMMIT_WELCOME`.
    pub fn proposals(
        &mut self,
        op_type: u8,
        data: &[u8],
        expected_users: &[u64],
    ) -> Result<Option<Vec<u8>>> {
        let op_type = match op_type {
            0 => ProposalsOperationType::APPEND,
            1 => ProposalsOperationType::REVOKE,
            other => {
                return Err(Error::Message(format!(
                    "unknown MLS proposal operation {other}"
                )));
            }
        };

        // The voice gateway is the authenticated MLS delivery service. It can
        // send a proposal for a user before CLIENT_CONNECT reaches us, so
        // enforcing a local roster here drops valid groups (observed with a
        // second participant already in the channel).
        let _ = expected_users;
        let Some(result) = self
            .session
            .process_proposals(op_type, data, None)
            .map_err(|error| Error::Message(format!("DAVE proposals: {error}")))?
        else {
            return Ok(None);
        };

        let mut out = result.commit;
        let welcome = result.welcome.is_some();
        if let Some(welcome) = result.welcome {
            out.extend_from_slice(&welcome);
        }
        tracing::info!(?op_type, welcome, size = out.len(), "DAVE commit+welcome");
        Ok(Some(out))
    }

    pub fn commit(&mut self, transition_id: u16, commit: &[u8]) -> Result<bool> {
        self.session
            .process_commit(commit)
            .map_err(|error| Error::Message(format!("DAVE commit: {error}")))?;
        tracing::info!(transition_id, "DAVE commit processed");
        self.prepare(transition_id, self.version)
    }

    pub fn welcome(&mut self, transition_id: u16, welcome: &[u8]) -> Result<bool> {
        self.session
            .process_welcome(welcome, None)
            .map_err(|error| Error::Message(format!("DAVE welcome: {error}")))?;
        tracing::info!(transition_id, "DAVE welcome processed");
        self.prepare(transition_id, self.version)
    }

    /// Prepares a transition. Returns true when the gateway expects a
    /// `DAVE_TRANSITION_READY`; a transition id of 0 is applied immediately.
    pub fn prepare(&mut self, transition_id: u16, protocol_version: u16) -> Result<bool> {
        self.session
            .prepare_transition(transition_id, protocol_version)
            .map_err(|error| Error::Message(format!("DAVE prepare: {error}")))?;
        if transition_id == 0 {
            self.version = protocol_version;
            Ok(false)
        } else {
            self.pending.insert(transition_id, protocol_version);
            Ok(true)
        }
    }

    pub fn execute(&mut self, transition_id: u16) -> Result<bool> {
        let executed = self
            .session
            .execute_transition(transition_id)
            .map_err(|error| Error::Message(format!("DAVE execute: {error}")))?;
        if executed && let Some(version) = self.pending.remove(&transition_id) {
            self.version = version;
        }
        Ok(executed)
    }

    pub fn reinit(&mut self, protocol_version: u16, user_id: u64, channel_id: u64) -> Result<()> {
        let version = NonZeroU16::new(protocol_version)
            .ok_or_else(|| Error::Message("DAVE version 0 cannot reinitialise".into()))?;
        self.session
            .reinit(version, user_id, channel_id, None)
            .map_err(|error| Error::Message(format!("DAVE reinit: {error}")))?;
        self.version = protocol_version;
        self.pending.clear();
        Ok(())
    }

    pub fn reset(&mut self) {
        self.session.reset().ok();
        self.pending.clear();
    }

    pub fn set_passthrough(&mut self, enabled: bool, expiry_seconds: u32) {
        self.session
            .set_passthrough_mode(enabled, Some(expiry_seconds));
    }

    pub fn encrypt_audio(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if !self.session.is_ready() {
            return Ok(data.to_vec());
        }
        self.session
            .encrypt_opus(data)
            .map(|output| output.into_owned())
            .map_err(|error| Error::Message(format!("DAVE encrypt audio: {error}")))
    }

    pub fn encrypt_video(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if !self.session.is_ready() {
            return Ok(data.to_vec());
        }
        self.session
            .encrypt(MediaType::VIDEO, Codec::H264, data)
            .map(|output| output.into_owned())
            .map_err(|error| Error::Message(format!("DAVE encrypt video: {error}")))
    }

    pub fn decrypt_audio(
        &mut self,
        user_id: u64,
        data: &[u8],
    ) -> std::result::Result<Vec<u8>, FrameRejected> {
        if !self.session.is_ready() {
            // Before the group is established, peers may already encrypt.
            // An encrypted frame played as Opus is noise, so only frames
            // without the protocol's trailing marker pass through.
            return if is_dave_frame(data) {
                Err(FrameRejected::NotReady)
            } else {
                Ok(data.to_vec())
            };
        }
        match self.session.decrypt(user_id, MediaType::AUDIO, data) {
            Ok(frame) => Ok(frame),
            Err(_) if self.session.can_passthrough(user_id) && !is_dave_frame(data) => {
                Ok(data.to_vec())
            }
            Err(error) => Err(FrameRejected::from(error)),
        }
    }

    /// Decrypts one whole video frame. Like audio, a frame carrying the
    /// protocol's marker is never passed through undecrypted: a decoder fed
    /// ciphertext shows garbage until the next keyframe.
    pub fn decrypt_video(
        &mut self,
        user_id: u64,
        data: &[u8],
    ) -> std::result::Result<Vec<u8>, FrameRejected> {
        if !self.session.is_ready() {
            return if is_dave_frame(data) {
                Err(FrameRejected::NotReady)
            } else {
                Ok(data.to_vec())
            };
        }
        match self.session.decrypt(user_id, MediaType::VIDEO, data) {
            Ok(frame) => Ok(frame),
            Err(_) if self.session.can_passthrough(user_id) && !is_dave_frame(data) => {
                Ok(data.to_vec())
            }
            Err(error) => Err(FrameRejected::from(error)),
        }
    }
}

/// Why a received frame was not handed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameRejected {
    /// The frame's nonce was already used: a duplicate delivery of a frame
    /// that has been decrypted before. Nothing is lost.
    Replayed,
    /// The frame is encrypted but the session has no key for it yet (the
    /// group is still forming, or the sender is not in it).
    NotReady,
    /// The frame arrived unencrypted while encryption is required.
    Unencrypted,
    /// Authentication failed: the frame is corrupt or was cut wrongly.
    Corrupt,
}

impl From<davey::errors::DecryptError> for FrameRejected {
    fn from(error: davey::errors::DecryptError) -> Self {
        use davey::errors::{DecryptError, DecryptorDecryptError as Inner};
        match error {
            DecryptError::NoDecryptorForUser => Self::NotReady,
            DecryptError::DecryptionFailed(Inner::InvalidNonce) => Self::Replayed,
            DecryptError::DecryptionFailed(Inner::MissingKeyRatchet | Inner::MissingCryptor) => {
                Self::NotReady
            }
            DecryptError::DecryptionFailed(Inner::UnencryptedWhenPassthroughDisabled) => {
                Self::Unencrypted
            }
            DecryptError::DecryptionFailed(Inner::DecryptionFailure) => Self::Corrupt,
        }
    }
}

/// The most slices a video frame may be cut into. An encrypted frame ends
/// with a list of the stretches left in the clear, one for each slice (its
/// header), and the encryptor has 64 bytes for that list and the nonce
/// before it; a frame that needs more is not encrypted at all, and so is
/// never sent. With up to 5 bytes of nonce, 3 for the first stretch and 4
/// for each one after (in a frame under 2 MiB), 15 slices fit; 12 leaves
/// room for a stretch or two that are not slices.
pub const MAX_VIDEO_SLICES: usize = 12;

/// The room the encryptor has after a frame for its nonce and the list of
/// stretches left in the clear (davey's `TRANSFORM_PADDING_BYTES`).
const TRAILER_ROOM: usize = 64;

/// Whether an H.264 access unit can be end-to-end encrypted: whether its
/// nonce and list of clear stretches surely fit in the room there is.
pub fn video_frame_fits(frame: &[u8]) -> bool {
    video_trailer_bound(frame) <= TRAILER_ROOM
}

/// The most bytes the nonce and the list of clear stretches of `frame`
/// can take. A stretch is listed as where it starts and how long it is,
/// both in LEB128: everything up to a slice, and that slice's first bytes.
fn video_trailer_bound(frame: &[u8]) -> usize {
    /// A 32-bit nonce at its longest.
    const NONCE: usize = 5;
    /// A start code, the unit's header, and as much of a slice's header
    /// as its first three values can take.
    const SLICE_HEAD: usize = 4 + 1 + 12;
    let leb128 = |value: usize| (usize::BITS - value.max(1).leading_zeros()).div_ceil(7) as usize;
    let mut bytes = NONCE;
    // Where the clear stretch being gathered starts, and its length so far.
    let (mut start, mut clear) = (0, 0);
    let mut offset = 0;
    for unit in crate::rtp::split_nalus(frame) {
        // A receiver puts a four-byte start code before every unit.
        let length = 4 + unit.len();
        if matches!(unit[0] & 0x1F, 1 | 5) {
            bytes += leb128(start) + leb128(clear + SLICE_HEAD.min(length));
            start = offset + length;
            clear = 0;
        } else {
            clear += length;
        }
        offset += length;
    }
    if clear > 0 {
        bytes += leb128(start) + leb128(clear);
    }
    bytes
}

/// Whether a media frame carries DAVE's end-to-end encryption trailer, which
/// ends with the magic marker `0xFAFA`.
fn is_dave_frame(frame: &[u8]) -> bool {
    frame.ends_with(&[0xFA, 0xFA])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_frames_do_not_pass_through_before_the_session_is_ready() {
        let mut dave = Dave::new(1, 42, 7).expect("session");
        assert!(!dave.is_ready());
        let plain = [0x78, 1, 2, 3];
        assert_eq!(dave.decrypt_audio(9, &plain), Ok(plain.to_vec()));
        let encrypted = [0x78, 1, 2, 3, 0xFA, 0xFA];
        assert_eq!(
            dave.decrypt_audio(9, &encrypted),
            Err(FrameRejected::NotReady)
        );
    }

    /// A frame of `slices` slices of `size` bytes each, after an access
    /// unit delimiter.
    fn sliced_frame(slices: usize, size: usize) -> Vec<u8> {
        let mut frame = vec![0, 0, 0, 1, 0x09, 0x10];
        for _ in 0..slices {
            frame.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9A]);
            frame.extend(std::iter::repeat_n(0x55, size - 2));
        }
        frame
    }

    #[test]
    fn a_frame_in_too_many_slices_cannot_be_encrypted() {
        // What the hardware encoder sent: 32 slices. Small ones already
        // need more room than there is, large ones more still.
        assert!(!video_frame_fits(&sliced_frame(32, 8)));
        assert!(!video_frame_fits(&sliced_frame(32, 5_000)));
        // The most we make, in a keyframe of over a megabyte.
        assert!(video_frame_fits(&sliced_frame(MAX_VIDEO_SLICES, 100_000)));
        assert!(video_frame_fits(&sliced_frame(1, 300_000)));
    }

    #[test]
    fn the_trailer_bound_counts_each_stretch() {
        // One slice: the nonce, then a stretch at 0 of under 128 bytes.
        assert_eq!(video_trailer_bound(&sliced_frame(1, 50)), 5 + 2);
        // A second starts 60 bytes in: one byte for where, one for length.
        assert_eq!(video_trailer_bound(&sliced_frame(2, 50)), 5 + 2 + 2);
        // Past 127 and past 16,383 a position takes two and three bytes.
        assert_eq!(video_trailer_bound(&sliced_frame(2, 200)), 5 + 2 + 3);
        assert_eq!(video_trailer_bound(&sliced_frame(2, 20_000)), 5 + 2 + 4);
        // Filler after the slices is one more stretch.
        let mut padded = sliced_frame(1, 50);
        padded.extend_from_slice(&[0, 0, 0, 1, 0x0C, 0xFF, 0xFF, 0x80]);
        assert_eq!(video_trailer_bound(&padded), 5 + 2 + 2);
    }

    #[test]
    fn creates_session_and_key_package() {
        let mut dave = Dave::new(1, 42, 7).expect("session");
        let package = dave.key_package().expect("key package");
        assert!(!package.is_empty());
        assert!(!dave.is_ready());
    }
}
