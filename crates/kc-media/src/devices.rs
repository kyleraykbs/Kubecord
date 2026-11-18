//! Choosing audio input/output devices for capture and playback.
//!
//! The selection is process-global: the UI sets it, and the capture and
//! playback pipelines read it when they (re)start. An empty selection means
//! the system default device.

use std::sync::RwLock;

use gstreamer as gst;
use gstreamer::prelude::*;

static INPUT_DEVICE: RwLock<Option<String>> = RwLock::new(None);
static OUTPUT_DEVICE: RwLock<Option<String>> = RwLock::new(None);

/// A selectable audio device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    /// The PulseAudio/PipeWire node name, passed to `pulsesrc`/`pulsesink`.
    pub name: String,
    /// A human-readable label.
    pub label: String,
}

/// Sets the microphone to capture from (`None` for the default).
pub fn set_input_device(name: Option<String>) {
    if let Ok(mut guard) = INPUT_DEVICE.write() {
        *guard = name.filter(|name| !name.is_empty());
    }
}

/// Sets the speaker/headphone to play to (`None` for the default).
pub fn set_output_device(name: Option<String>) {
    if let Ok(mut guard) = OUTPUT_DEVICE.write() {
        *guard = name.filter(|name| !name.is_empty());
    }
}

/// The chosen microphone node name, if any.
pub fn input_device() -> Option<String> {
    INPUT_DEVICE.read().ok().and_then(|guard| guard.clone())
}

/// The chosen output node name, if any.
pub fn output_device() -> Option<String> {
    OUTPUT_DEVICE.read().ok().and_then(|guard| guard.clone())
}

/// Lists microphones (and other input sources).
pub fn inputs() -> Vec<AudioDevice> {
    enumerate("Audio/Source")
}

/// Lists speakers and headphones.
pub fn outputs() -> Vec<AudioDevice> {
    enumerate("Audio/Sink")
}

fn enumerate(classes: &str) -> Vec<AudioDevice> {
    if gst::init().is_err() {
        return Vec::new();
    }
    let monitor = gst::DeviceMonitor::new();
    let _ = monitor.add_filter(Some(classes), None);
    if monitor.start().is_err() {
        return Vec::new();
    }
    let mut out: Vec<AudioDevice> = Vec::new();
    for device in monitor.devices() {
        let props = device.properties();
        // Skip output-monitor sources; they are not real microphones.
        let is_monitor = props
            .as_ref()
            .and_then(|p| p.get::<String>("device.class").ok())
            .map(|class| class == "monitor")
            .unwrap_or(false);
        if is_monitor {
            continue;
        }
        let name = props.as_ref().and_then(|p| {
            p.get::<String>("device.name")
                .ok()
                .or_else(|| p.get::<String>("node.name").ok())
        });
        if let Some(name) = name {
            let device = AudioDevice {
                name,
                label: device.display_name().to_string(),
            };
            if !out.contains(&device) {
                out.push(device);
            }
        }
    }
    monitor.stop();
    out
}
