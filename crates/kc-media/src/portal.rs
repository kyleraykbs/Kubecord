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

/// A camera selected through the XDG camera portal.
pub struct CameraCast {
    pub node_id: u32,
    pub name: String,
    fd: OwnedFd,
}

impl CameraCast {
    pub fn fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.fd.as_raw_fd()
    }
}

/// Requests camera access. The portal may show a permission prompt; `None`
/// means no camera is present.
pub async fn request_camera() -> Result<Option<CameraCast>> {
    let Some((fd, streams)) = ashpd::desktop::camera::request()
        .await
        .map_err(|error| Error::Message(format!("camera portal: {error}")))?
    else {
        return Ok(None);
    };
    let describe = |stream: &ashpd::desktop::camera::Stream| -> String {
        let properties = stream.properties();
        properties
            .get("device.description")
            .or_else(|| properties.get("node.description"))
            .or_else(|| properties.get("node.name"))
            .cloned()
            .unwrap_or_default()
    };
    let Some(stream) = streams.iter().find(|stream| {
        let description = describe(stream).to_ascii_lowercase();
        // The UGREEN capture card on this machine is single-claimant; a
        // camera client must never take it. Prefer any other camera.
        !description.contains("ugreen") && !description.contains("macrosilicon")
    }) else {
        return Ok(None);
    };
    let name = describe(stream);
    Ok(Some(CameraCast {
        node_id: stream.node_id(),
        name,
        fd,
    }))
}
