//! Persisted capture-quality preferences.

use kc_core::paths::{config_dir, ensure_dir};
use kc_media::video::{EncoderPreference, VideoSettings};
use serde::{Deserialize, Serialize};

pub const RESOLUTIONS: [(u32, u32, &str); 4] = [
    (1920, 1080, "1920 × 1080"),
    (1280, 720, "1280 × 720"),
    (960, 540, "960 × 540"),
    (640, 360, "640 × 360"),
];

pub const FRAMERATES: [u32; 4] = [60, 30, 24, 15];

pub const KEYFRAME_INTERVALS: [u32; 4] = [2, 4, 6, 10];

pub const DROP_AFTER_MS: [u32; 6] = [0, 100, 250, 500, 1000, 2000];

pub const BITRATES: [u32; 7] = [1000, 1500, 2500, 4000, 6000, 8000, 10000];

pub const STREAM_DELAYS: [u64; 5] = [0, 500, 1000, 2000, 3000];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CapturePreferences {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `None` means derive the bitrate from resolution and framerate.
    pub bitrate_kbps: Option<u32>,
    /// Seconds between IDR keyframes.
    pub keyframe_secs: u32,
    /// Drop a frame when it has waited this long beyond the configured delay.
    /// Zero disables dropping.
    pub drop_frames_after_ms: u32,
    /// Drop old frames in the capture queue instead of accumulating latency.
    pub leaky_queue: bool,
    /// Fixed Go Live buffering delay in milliseconds.
    pub stream_delay_ms: u64,
    /// Force the software x264 encoder instead of trying VA-API first.
    pub cpu_encoder: bool,
}

impl Default for CapturePreferences {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: None,
            keyframe_secs: 2,
            drop_frames_after_ms: 250,
            leaky_queue: true,
            stream_delay_ms: 0,
            cpu_encoder: false,
        }
    }
}

impl CapturePreferences {
    pub fn video_settings(&self) -> VideoSettings {
        VideoSettings {
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_kbps: self
                .bitrate_kbps
                .unwrap_or_else(|| VideoSettings::automatic_bitrate_kbps(self.width, self.height, self.fps)),
            keyframe_secs: self.keyframe_secs,
            drop_frames_after_ms: self.drop_frames_after_ms,
            leaky_queue: self.leaky_queue,
            preset: "ultrafast".into(),
            encoder: if self.cpu_encoder {
                EncoderPreference::Cpu
            } else {
                EncoderPreference::Auto
            },
        }
    }

    pub fn load() -> Self {
        let Ok(dir) = config_dir() else {
            return Self::default();
        };
        let Ok(bytes) = std::fs::read(dir.join("settings.json")) else {
            return Self::default();
        };
        let mut prefs: Self = serde_json::from_slice(&bytes).unwrap_or_default();
        prefs.normalize();
        prefs
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = config_dir().map_err(|error| error.to_string())?;
        ensure_dir(&dir).map_err(|error| error.to_string())?;
        let path = dir.join("settings.json");
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
        let _ = kc_core::paths::restrict(&path);
        Ok(())
    }

    fn normalize(&mut self) {
        if !RESOLUTIONS
            .iter()
            .any(|(width, height, _)| *width == self.width && *height == self.height)
        {
            self.width = 1280;
            self.height = 720;
        }
        if !FRAMERATES.contains(&self.fps) {
            self.fps = 30;
        }
        if let Some(bitrate) = self.bitrate_kbps
            && !(500..=50_000).contains(&bitrate)
        {
            self.bitrate_kbps = None;
        }
        if !STREAM_DELAYS.contains(&self.stream_delay_ms) {
            self.stream_delay_ms = 0;
        }
        if !KEYFRAME_INTERVALS.contains(&self.keyframe_secs) {
            self.keyframe_secs = 2;
        }
        if !DROP_AFTER_MS.contains(&self.drop_frames_after_ms) {
            self.drop_frames_after_ms = 250;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_bitrate_is_derived_from_resolution_and_fps() {
        let prefs = CapturePreferences::default();
        let settings = prefs.video_settings();
        assert_eq!(settings.width, 1280);
        assert_eq!(settings.height, 720);
        assert_eq!(settings.fps, 30);
        assert_eq!(
            settings.bitrate_kbps,
            VideoSettings::automatic_bitrate_kbps(1280, 720, 30)
        );
        assert_eq!(settings.drop_frames_after_ms, 250);
        assert!(settings.leaky_queue);
    }

    #[test]
    fn explicit_bitrate_is_preserved() {
        let prefs = CapturePreferences {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: Some(6000),
            keyframe_secs: 4,
            drop_frames_after_ms: 500,
            leaky_queue: false,
            stream_delay_ms: 1000,
            cpu_encoder: true,
        };
        let settings = prefs.video_settings();
        assert_eq!(settings.bitrate_kbps, 6000);
        assert_eq!(settings.drop_frames_after_ms, 500);
        assert!(!settings.leaky_queue);
    }
}
