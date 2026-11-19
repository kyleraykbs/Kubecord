//! Persisted capture-quality preferences.

use kc_core::paths::{config_dir, ensure_dir};
use kc_media::video::{CameraFormat, EncoderPreference, VideoSettings};
use serde::{Deserialize, Serialize};

pub const RESOLUTIONS: [(u32, u32, &str); 4] = [
    (1920, 1080, "1920 × 1080"),
    (1280, 720, "1280 × 720"),
    (960, 540, "960 × 540"),
    (640, 360, "640 × 360"),
];

pub const FRAMERATES: [u32; 4] = [60, 30, 24, 15];

/// Seconds between scheduled Go Live keyframes; zero sends them only when
/// needed.
pub const KEYFRAME_INTERVALS: [u32; 5] = [0, 2, 4, 6, 10];

pub const DROP_AFTER_MS: [u32; 6] = [0, 100, 250, 500, 1000, 2000];

pub const RETRANSMIT_WINDOWS_MS: [u32; 5] = [0, 1000, 2000, 5000, 10000];

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
    /// Seconds between scheduled Go Live keyframes; zero sends them only
    /// when a viewer asks or packets are lost.
    /// Stored under a new name so the old two-second default, which made
    /// viewers hitch, does not carry over.
    #[serde(rename = "stream_keyframe_secs")]
    pub keyframe_secs: u32,
    /// Drop a frame when it has waited this long beyond the configured delay.
    /// Zero disables dropping.
    pub drop_frames_after_ms: u32,
    /// Drop old frames in the capture queue instead of accumulating latency.
    pub leaky_queue: bool,
    /// How long sent packets are kept to repair a network dropout by
    /// retransmission. Zero disables retransmission.
    pub retransmit_ms: u32,
    /// Fixed Go Live buffering delay in milliseconds.
    pub stream_delay_ms: u64,
    /// Force the software x264 encoder instead of trying VA-API first.
    pub cpu_encoder: bool,
    /// Microphone device node name; empty for the system default.
    pub input_device: String,
    /// Output (speaker/headphone) device node name; empty for the default.
    pub output_device: String,
    /// Microphone volume, 0.0 to 2.0.
    pub input_volume: f32,
    /// Speaker volume, 0.0 to 2.0.
    pub output_volume: f32,
    /// Name of the camera the camera button uses; empty to ask each time.
    pub camera: String,
    /// What the screen-share picker offers: 0 screens and windows, 1
    /// screens, 2 windows.
    pub share_source: u32,
    /// Width of the channel sidebar in pixels.
    pub sidebar_width: i32,
    /// Start the camera preview as soon as the camera popup opens.
    pub always_preview: bool,
    /// Show Discord's captchas in kylecord instead of sending the user to
    /// the browser.
    pub captcha_in_app: bool,
    /// Tell Discord when you are typing, so others see it.
    pub send_typing: bool,
    /// Height of the call above a DM's chat, in pixels.
    pub call_height: i32,
    /// Camera capture format: 0 automatic, 1 MJPEG, 2 raw.
    pub camera_format: u32,
    /// Show your own camera and stream in the call.
    pub self_preview: bool,
}

pub const CAMERA_FORMATS: [&str; 3] = ["Automatic", "MJPEG (compressed)", "Raw (uncompressed)"];

/// Default and least height of the call area above a DM's chat.
pub const CALL_HEIGHT: i32 = 320;
pub const CALL_HEIGHT_MIN: i32 = 120;

/// Default and limits of the resizable channel sidebar.
pub const SIDEBAR_WIDTH: i32 = 340;
pub const SIDEBAR_MIN: i32 = 220;
pub const SIDEBAR_MAX: i32 = 480;

pub const SHARE_SOURCES: [&str; 3] = ["Screen or Window", "Entire Screen", "Window"];

impl Default for CapturePreferences {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: None,
            keyframe_secs: 0,
            drop_frames_after_ms: 250,
            leaky_queue: true,
            retransmit_ms: 2000,
            stream_delay_ms: 0,
            cpu_encoder: false,
            input_device: String::new(),
            output_device: String::new(),
            input_volume: 1.0,
            output_volume: 1.0,
            camera: String::new(),
            share_source: 0,
            sidebar_width: SIDEBAR_WIDTH,
            always_preview: false,
            captcha_in_app: true,
            send_typing: true,
            call_height: CALL_HEIGHT,
            camera_format: 0,
            self_preview: true,
        }
    }
}

impl CapturePreferences {
    pub fn video_settings(&self) -> VideoSettings {
        VideoSettings {
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_kbps: self.bitrate_kbps.unwrap_or_else(|| {
                VideoSettings::automatic_bitrate_kbps(self.width, self.height, self.fps)
            }),
            keyframe_secs: self.keyframe_secs,
            drop_frames_after_ms: self.drop_frames_after_ms,
            leaky_queue: self.leaky_queue,
            retransmit_ms: self.retransmit_ms,
            preset: "ultrafast".into(),
            encoder: if self.cpu_encoder {
                EncoderPreference::Cpu
            } else {
                EncoderPreference::Auto
            },
            camera_format: match self.camera_format {
                1 => CameraFormat::Mjpeg,
                2 => CameraFormat::Raw,
                _ => CameraFormat::Auto,
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

    /// Pushes the chosen devices and volumes into the media pipelines.
    pub fn apply_devices(&self) {
        kc_media::devices::set_input_device(
            (!self.input_device.is_empty()).then(|| self.input_device.clone()),
        );
        kc_media::devices::set_output_device(
            (!self.output_device.is_empty()).then(|| self.output_device.clone()),
        );
        crate::session::set_self_preview(self.self_preview);
        kc_media::devices::set_input_gain(self.input_volume);
        kc_media::devices::set_output_gain(self.output_volume);
        kc_media::portal::set_share_sources(match self.share_source {
            1 => kc_media::portal::ShareSources::Screen,
            2 => kc_media::portal::ShareSources::Window,
            _ => kc_media::portal::ShareSources::ScreenOrWindow,
        });
    }

    /// Puts the stream quality settings (and the advanced ones) back to
    /// their defaults, keeping devices, volumes and layout.
    pub fn reset_video(&mut self) {
        let defaults = Self::default();
        self.width = defaults.width;
        self.height = defaults.height;
        self.fps = defaults.fps;
        self.bitrate_kbps = defaults.bitrate_kbps;
        self.keyframe_secs = defaults.keyframe_secs;
        self.drop_frames_after_ms = defaults.drop_frames_after_ms;
        self.leaky_queue = defaults.leaky_queue;
        self.retransmit_ms = defaults.retransmit_ms;
        self.stream_delay_ms = defaults.stream_delay_ms;
        self.cpu_encoder = defaults.cpu_encoder;
        self.camera_format = defaults.camera_format;
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
            self.keyframe_secs = 0;
        }
        if !DROP_AFTER_MS.contains(&self.drop_frames_after_ms) {
            self.drop_frames_after_ms = 250;
        }
        if !RETRANSMIT_WINDOWS_MS.contains(&self.retransmit_ms) {
            self.retransmit_ms = 2000;
        }
        for volume in [&mut self.input_volume, &mut self.output_volume] {
            if !volume.is_finite() || !(0.0..=2.0).contains(volume) {
                *volume = 1.0;
            }
        }
        if self.camera_format > 2 {
            self.camera_format = 0;
        }
        if self.share_source > 2 {
            self.share_source = 0;
        }
        self.sidebar_width = self.sidebar_width.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self.call_height = self.call_height.max(CALL_HEIGHT_MIN);
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
            retransmit_ms: 5000,
            stream_delay_ms: 1000,
            cpu_encoder: true,
            ..CapturePreferences::default()
        };
        let settings = prefs.video_settings();
        assert_eq!(settings.bitrate_kbps, 6000);
        assert_eq!(settings.drop_frames_after_ms, 500);
        assert!(!settings.leaky_queue);
    }

    #[test]
    fn the_old_keyframe_schedule_does_not_carry_over() {
        // A settings file from before keyframes became on-demand.
        let old = r#"{"width":1280,"height":720,"fps":30,"keyframe_secs":2}"#;
        let mut prefs: CapturePreferences = serde_json::from_str(old).unwrap();
        prefs.normalize();
        assert_eq!(prefs.keyframe_secs, 0);
        assert_eq!(prefs.video_settings().keyframe_secs, 0);
        // A schedule chosen since then is kept.
        let chosen = r#"{"stream_keyframe_secs":4}"#;
        let prefs: CapturePreferences = serde_json::from_str(chosen).unwrap();
        assert_eq!(prefs.keyframe_secs, 4);
    }

    #[test]
    fn reset_video_keeps_devices_and_layout() {
        let mut prefs = CapturePreferences {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: Some(8000),
            cpu_encoder: true,
            leaky_queue: false,
            input_device: "mic".into(),
            output_volume: 0.5,
            camera: "Webcam".into(),
            sidebar_width: 400,
            ..CapturePreferences::default()
        };
        prefs.reset_video();
        let defaults = CapturePreferences::default();
        assert_eq!((prefs.width, prefs.height, prefs.fps), (1280, 720, 30));
        assert_eq!(prefs.bitrate_kbps, None);
        assert_eq!(prefs.cpu_encoder, defaults.cpu_encoder);
        assert_eq!(prefs.leaky_queue, defaults.leaky_queue);
        assert_eq!(prefs.input_device, "mic");
        assert_eq!(prefs.output_volume, 0.5);
        assert_eq!(prefs.camera, "Webcam");
        assert_eq!(prefs.sidebar_width, 400);
    }
}
