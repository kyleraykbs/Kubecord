//! XDG desktop portal screen capture.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use ashpd::desktop::Session;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};

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

/// What the screen-share picker offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareSources {
    ScreenOrWindow = 0,
    Screen = 1,
    Window = 2,
}

static SHARE_SOURCES: AtomicU8 = AtomicU8::new(0);

/// Sets what the screen-share picker offers from now on.
pub fn set_share_sources(sources: ShareSources) {
    SHARE_SOURCES.store(sources as u8, Ordering::Relaxed);
}

fn source_types() -> SourceType {
    match SHARE_SOURCES.load(Ordering::Relaxed) {
        1 => SourceType::MONITOR,
        2 => SourceType::WINDOW,
        _ => SourceType::MONITOR | SourceType::WINDOW,
    }
}

/// Shows the portal picker and returns a stream for the selected monitor or
/// window. This must be called from a Tokio runtime with a running portal.
pub async fn request(parent: Option<&ashpd::WindowIdentifier>) -> Result<ScreenCast> {
    let proxy = Screencast::new()
        .await
        .map_err(|error| Error::Message(format!("screencast portal: {error}")))?;
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(|error| Error::Message(format!("screencast session: {error}")))?;
    let options = SelectSourcesOptions::default()
        .set_sources(source_types())
        .set_multiple(false)
        .set_cursor_mode(CursorMode::EMBEDDED);
    proxy
        .select_sources(&session, options)
        .await
        .map_err(|error| Error::Message(format!("screencast sources: {error}")))?;
    let response = proxy
        .start(&session, parent, Default::default())
        .await
        .map_err(|error| Error::Message(format!("screencast start: {error}")))?
        .response()
        .map_err(|error| Error::Message(format!("screencast response: {error}")))?;

    let stream = response
        .streams()
        .first()
        .ok_or_else(|| Error::Message("portal returned no streams".into()))?;
    let node_id = stream.pipe_wire_node_id();
    tracing::info!(
        streams = response.streams().len(),
        node_id,
        source_type = ?stream.source_type(),
        size = ?stream.size(),
        "screencast stream selected"
    );
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

/// A camera exposed by the XDG camera portal.
#[derive(Clone, Debug)]
pub struct CameraChoice {
    pub node_id: u32,
    pub name: String,
    /// PipeWire `node.name`, used as a target when the numeric id is
    /// rejected by the portal remote.
    pub node_name: String,
    /// PipeWire `object.serial`, another valid target object.
    pub serial: String,
    /// Direct V4L2 device for virtual cameras that the portal does not expose.
    pub device_path: Option<String>,
}

/// One portal session and fd shared by every camera in the list.
#[derive(Clone)]
pub struct CameraAccess {
    fd: Arc<OwnedFd>,
    pub cameras: Vec<CameraChoice>,
}

impl CameraAccess {
    pub fn select(&self, node_id: u32) -> Option<CameraCast> {
        let camera = self
            .cameras
            .iter()
            .find(|camera| camera.node_id == node_id)?;
        Some(CameraCast {
            node_id: camera.node_id,
            name: camera.name.clone(),
            node_name: camera.node_name.clone(),
            serial: camera.serial.clone(),
            device_path: camera.device_path.clone(),
            fd: self.fd.clone(),
        })
    }

    /// The first camera in the portal's order.
    pub fn preferred(&self) -> Option<CameraCast> {
        self.cameras
            .first()
            .and_then(|camera| self.select(camera.node_id))
    }
}

/// A camera selected through the XDG camera portal.
#[derive(Clone, Debug)]
pub struct CameraCast {
    pub node_id: u32,
    pub name: String,
    pub node_name: String,
    pub serial: String,
    pub device_path: Option<String>,
    fd: Arc<OwnedFd>,
}

impl CameraCast {
    pub fn fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.fd.as_raw_fd()
    }
}

/// Requests camera access and returns the usable camera entries. The portal may
/// show a permission prompt.
///
/// The portal remote also enumerates the raw `v4l2_input.*` nodes behind each
/// camera, but refuses to capture from those (`-EINVAL` on the output format).
/// A V4L2 entry whose name also appears in a libcamera entry is omitted.
/// Virtual cameras are opened directly below: the portal omits the loopback
/// OBS writes to and exposes producerless loops that never deliver a frame.
pub async fn request_cameras() -> Result<Option<CameraAccess>> {
    let Some((fd, streams)) = ashpd::desktop::camera::request()
        .await
        .map_err(|error| Error::Message(format!("camera portal: {error}")))?
    else {
        return Ok(None);
    };
    let clean_name = |raw: &str| -> String {
        let words: Vec<&str> = raw.split_whitespace().collect();
        let half = words.len() / 2;
        if words.len() > 2 && words.len().is_multiple_of(2) && words[..half] == words[half..] {
            words[..half].join(" ")
        } else {
            words.join(" ")
        }
    };
    let describe = |stream: &ashpd::desktop::camera::Stream| -> String {
        let properties = stream.properties();
        let name = properties
            .get("node.nick")
            .filter(|nick| !nick.is_empty())
            .or_else(|| properties.get("device.description"))
            .or_else(|| properties.get("node.description"))
            .or_else(|| properties.get("node.name"))
            .cloned()
            .unwrap_or_default();
        clean_name(&name)
    };
    let normalize = |name: &str| {
        name.to_ascii_lowercase()
            .replace("(v4l2)", " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    struct RawCamera {
        choice: CameraChoice,
        match_key: String,
        v4l2: bool,
    }
    let raws: Vec<RawCamera> = streams
        .iter()
        .map(|stream| {
            let properties = stream.properties();
            let node_name = properties.get("node.name").cloned().unwrap_or_default();
            let name = describe(stream);
            let match_name = properties
                .get("node.nick")
                .filter(|nick| !nick.is_empty())
                .cloned()
                .unwrap_or_else(|| name.clone());
            RawCamera {
                v4l2: node_name.starts_with("v4l2_input."),
                match_key: normalize(&match_name),
                choice: CameraChoice {
                    node_id: stream.node_id(),
                    name: if name.is_empty() {
                        format!("Camera {}", stream.node_id())
                    } else {
                        name
                    },
                    node_name,
                    serial: properties.get("object.serial").cloned().unwrap_or_default(),
                    device_path: None,
                },
            }
        })
        .collect();
    let backed: Vec<String> = raws
        .iter()
        .filter(|camera| !camera.v4l2)
        .map(|camera| normalize(&camera.choice.name))
        .collect();
    let mut cameras: Vec<CameraChoice> = raws
        .into_iter()
        .filter(|camera| {
            if camera
                .choice
                .node_name
                .starts_with("v4l2_input._sys_devices_virtual_video4linux")
            {
                // Direct V4L2 below handles virtual cameras; the portal's
                // loopback nodes can block forever without a producer.
                return false;
            }
            !camera.v4l2
                || camera.match_key.len() < 4
                || !backed.iter().any(|name| name.contains(&camera.match_key))
        })
        .map(|camera| camera.choice)
        .collect();
    cameras.extend(direct_virtual_cameras());
    if cameras.is_empty() {
        return Ok(None);
    }
    Ok(Some(CameraAccess {
        fd: Arc::new(fd),
        cameras,
    }))
}

/// Virtual cameras are v4l2loopback devices. The camera portal omits the
/// device OBS writes to, and a loopback device with no producer accepts the
/// pipeline but never delivers a frame. Only devices already opened by a
/// producer are offered; they are opened directly.
fn direct_virtual_cameras() -> Vec<CameraChoice> {
    let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") else {
        return Vec::new();
    };
    let mut cameras: Vec<(u32, CameraChoice)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let device_name = entry.file_name().into_string().ok()?;
            let number = device_name.strip_prefix("video")?.parse::<u32>().ok()?;
            let name = std::fs::read_to_string(entry.path().join("name"))
                .ok()?
                .trim()
                .to_owned();
            if !name.starts_with("VirtualCam") {
                return None;
            }
            let device_path = format!("/dev/{device_name}");
            if !std::path::Path::new(&device_path).exists() || !device_has_producer(&device_path) {
                return None;
            }
            Some((
                number,
                CameraChoice {
                    // Synthetic id: these cameras do not have a portal node.
                    node_id: u32::MAX - number,
                    name: format!("{name} ({device_path})"),
                    node_name: String::new(),
                    serial: String::new(),
                    device_path: Some(device_path),
                },
            ))
        })
        .collect();
    cameras.sort_by_key(|(number, _)| *number);
    cameras.into_iter().map(|(_, camera)| camera).collect()
}

fn device_has_producer(device: &str) -> bool {
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return false;
    };
    for process in processes.flatten() {
        let Ok(fds) = std::fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if std::fs::read_link(fd.path())
                .is_ok_and(|target| target == std::path::Path::new(device))
            {
                return true;
            }
        }
    }
    false
}

/// Requests camera access and returns the preferred camera.
pub async fn request_camera() -> Result<Option<CameraCast>> {
    Ok(request_cameras()
        .await?
        .and_then(|access| access.preferred()))
}
