//! Persisted capture-quality preferences.

use kc_core::paths::{config_dir, ensure_dir};
use kc_media::video::VideoSettings;
use serde::{Deserialize, Serialize};

pub const RESOLUTIONS: [(u32, u32, &str); 4] = [
    (1920, 1080, "1920 × 1080"),
    (1280, 720, "1280 × 720"),
    (960, 540, "960 × 540"),
    (640, 360, "640 × 360"),
];

pub const FRAMERATES: [u32; 4] = [60, 30, 24, 15];

pub const BITRATES: [u32; 7] = [1000, 1500, 2500, 4000, 6000, 8000, 10000];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CapturePreferences {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `None` means derive the bitrate from resolution and framerate.
    pub bitrate_kbps: Option<u32>,
}

impl Default for CapturePreferences {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: None,
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
            preset: "ultrafast".into(),
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
    }

    #[test]
    fn explicit_bitrate_is_preserved() {
        let prefs = CapturePreferences {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: Some(6000),
        };
        let settings = prefs.video_settings();
        assert_eq!(settings.bitrate_kbps, 6000);
    }
}
