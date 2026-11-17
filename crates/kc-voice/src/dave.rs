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
        tracing::info!(version = self.version, size = package.len(), "DAVE key package");
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

        let Some(result) = self
            .session
            .process_proposals(op_type, data, Some(expected_users))
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

    pub fn decrypt_audio(&mut self, user_id: u64, data: &[u8]) -> Option<Vec<u8>> {
        if !self.session.is_ready() {
            return Some(data.to_vec());
        }
        match self.session.decrypt(user_id, MediaType::AUDIO, data) {
            Ok(frame) => Some(frame),
            Err(error) if self.session.can_passthrough(user_id) => {
                tracing::debug!(user_id, %error, "DAVE audio passthrough");
                Some(data.to_vec())
            }
            Err(error) => {
                tracing::debug!(user_id, %error, "DAVE audio decrypt failed");
                None
            }
        }
    }

    pub fn decrypt_video(&mut self, user_id: u64, data: &[u8]) -> Option<Vec<u8>> {
        if !self.session.is_ready() {
            return Some(data.to_vec());
        }
        match self.session.decrypt(user_id, MediaType::VIDEO, data) {
            Ok(frame) => Some(frame),
            Err(error) => {
                tracing::debug!(user_id, %error, "DAVE video decrypt failed");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_session_and_key_package() {
        let mut dave = Dave::new(1, 42, 7).expect("session");
        let package = dave.key_package().expect("key package");
        assert!(!package.is_empty());
        assert!(!dave.is_ready());
    }
}
