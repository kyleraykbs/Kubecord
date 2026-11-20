//! Choosing audio input/output devices for capture and playback.
//!
//! The selection is process-global: the UI sets it, and the capture and
//! playback pipelines read it when they (re)start. An empty selection means
//! the system default device.

use std::sync::RwLock;
use std::sync::atomic::{AtomicU32, Ordering};

use gstreamer as gst;
use gstreamer::prelude::*;

static INPUT_DEVICE: RwLock<Option<String>> = RwLock::new(None);
static OUTPUT_DEVICE: RwLock<Option<String>> = RwLock::new(None);
static INPUT_GAIN: AtomicU32 = AtomicU32::new(1.0f32.to_bits());
static OUTPUT_GAIN: AtomicU32 = AtomicU32::new(1.0f32.to_bits());

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
        let name = name.filter(|name| !name.is_empty());
        if *guard != name {
            *guard = name;
            // A running microphone moves to the new device.
            crate::processing::rebuild();
        }
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

/// Sets the microphone volume, from 0.0 to 2.0. It applies at once, also to
/// a call in progress.
pub fn set_input_gain(gain: f32) {
    INPUT_GAIN.store(gain.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
}

/// Sets the speaker volume, from 0.0 to 2.0. It applies at once.
pub fn set_output_gain(gain: f32) {
    OUTPUT_GAIN.store(gain.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
}

pub fn input_gain() -> f32 {
    f32::from_bits(INPUT_GAIN.load(Ordering::Relaxed))
}

pub fn output_gain() -> f32 {
    f32::from_bits(OUTPUT_GAIN.load(Ordering::Relaxed))
}

/// Scales samples in place, saturating instead of wrapping.
pub fn apply_gain(samples: &mut [i16], gain: f32) {
    if (gain - 1.0).abs() < f32::EPSILON {
        return;
    }
    for sample in samples {
        *sample =
            (f32::from(*sample) * gain).clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
    }
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
    // The PulseAudio provider lists exactly what pulsesrc and pulsesink can
    // open. A general device monitor also probes cameras, and some camera
    // drivers report caps GStreamer warns about.
    let devices: Vec<gst::Device> = match gst::DeviceProviderFactory::by_name("pulsedeviceprovider")
    {
        Some(provider) => provider
            .devices()
            .into_iter()
            .filter(|device| device.has_classes(classes))
            .collect(),
        None => {
            let monitor = gst::DeviceMonitor::new();
            let _ = monitor.add_filter(Some(classes), None);
            if monitor.start().is_err() {
                return Vec::new();
            }
            let devices = monitor.devices().into_iter().collect();
            monitor.stop();
            devices
        }
    };
    let mut out: Vec<AudioDevice> = Vec::new();
    for device in devices {
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
        if let Some(name) = device_name(&device) {
            let device = AudioDevice {
                name,
                label: device.display_name().to_string(),
            };
            if !out.contains(&device) {
                out.push(device);
            }
        }
    }
    out
}

/// The name pulsesrc/pulsesink take as `device`.
fn device_name(device: &gst::Device) -> Option<String> {
    if device.find_property("internal-name").is_some()
        && let Ok(name) = device.property_value("internal-name").get::<String>()
        && !name.is_empty()
    {
        return Some(name);
    }
    let props = device.properties()?;
    props
        .get::<String>("device.name")
        .ok()
        .or_else(|| props.get::<String>("node.name").ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_scales_and_saturates() {
        let mut samples = [1000i16, -1000, 30_000, -30_000];
        apply_gain(&mut samples, 2.0);
        assert_eq!(samples, [2000, -2000, i16::MAX, i16::MIN]);
        apply_gain(&mut samples, 0.0);
        assert_eq!(samples, [0; 4]);
    }
}
