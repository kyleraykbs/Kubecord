//! XDG desktop portal screen capture.

use std::os::fd::OwnedFd;

use ashpd::desktop::Session;
use ashpd::desktop::screencast::{Screencast, SelectSourcesOptions};

use crate::error::{Error, Result};

/// A selected screen-cast stream. The session and its PipeWire fd must stay
/// alive for as long as the source is used.
pub struct ScreenCast {
    pub node_id: u32,
    fd: OwnedFd,
    _session: Session<Screencast>,
}

impl ScreenCast {
    pub fn fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.fd.as_raw_fd()
    }
}

/// Shows the portal picker and returns a stream for the selected monitor or
/// window. This must be called from a Tokio runtime with a running portal.
pub async fn request() -> Result<ScreenCast> {
    let proxy = Screencast::new()
        .await
        .map_err(|error| Error::Message(format!("screencast portal: {error}")))?;
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(|error| Error::Message(format!("screencast session: {error}")))?;
    proxy
        .select_sources(&session, SelectSourcesOptions::default())
        .await
        .map_err(|error| Error::Message(format!("screencast sources: {error}")))?;
    let response = proxy
        .start(&session, None, Default::default())
        .await
        .map_err(|error| Error::Message(format!("screencast start: {error}")))?
        .response()
        .map_err(|error| Error::Message(format!("screencast response: {error}")))?;

    let stream = response
        .streams()
        .first()
        .ok_or_else(|| Error::Message("portal returned no streams".into()))?;
    let node_id = stream.pipe_wire_node_id();
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(|error| Error::Message(format!("screencast pipewire fd: {error}")))?;

    Ok(ScreenCast {
        node_id,
        fd,
        _session: session,
    })
}
