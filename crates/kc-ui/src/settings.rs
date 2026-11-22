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

/// The lowest framerate an automatic framerate may go down to.
pub const FRAMERATE_FLOORS: [u32; 6] = [5, 10, 15, 20, 24, 30];

/// The lowest bitrate an automatic bitrate may go down to, in kbit/s.
pub const BITRATE_FLOORS: [u32; 8] = [300, 500, 800, 1000, 1500, 2000, 3000, 4000];

/// Seconds between scheduled Go Live keyframes; zero sends them only when
/// needed.
pub const KEYFRAME_INTERVALS: [u32; 5] = [0, 2, 4, 6, 10];

/// How far one person's volume can be turned up, as a multiple of normal.
pub const VOLUME_BOOST_CAPS: [u32; 5] = [1, 2, 3, 4, 5];

/// The longest a stream goes without a keyframe, in seconds (0: no limit).
pub const KEYFRAME_GAPS: [u32; 5] = [0, 3, 5, 10, 15];

pub const DROP_AFTER_MS: [u32; 6] = [0, 100, 250, 500, 1000, 2000];

pub const RETRANSMIT_WINDOWS_MS: [u32; 5] = [0, 1000, 2000, 5000, 10000];

pub const BITRATES: [u32; 10] = [
    1000, 1500, 2500, 4000, 6000, 8000, 10000, 12000, 15000, 20000,
];

pub const STREAM_DELAYS: [u64; 5] = [0, 500, 1000, 2000, 3000];

/// When messages in the open channel stay quiet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenChannelQuiet {
    /// While Kubecord is the focused window (as Discord does).
    #[default]
    WhileFocused,
    /// Whenever it is the open channel, even with Kubecord in the background.
    Always,
    /// Never: the open channel notifies like any other.
    Never,
}

/// The choices, in the order the Notifications page lists them.
pub const OPEN_CHANNEL_QUIET: [(OpenChannelQuiet, &str); 3] = [
    (OpenChannelQuiet::WhileFocused, "While Kubecord Is Focused"),
    (OpenChannelQuiet::Always, "Always"),
    (OpenChannelQuiet::Never, "Never"),
];

/// Least time between keyframes sent because packets were lost.
pub const LOSS_KEYFRAME_SPACINGS_MS: [u32; 4] = [500, 1000, 2000, 3000];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CapturePreferences {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Let the framerate follow the network instead of holding `fps`:
    /// between `fps_auto_min` and `fps_auto_max`.
    pub fps_auto: bool,
    pub fps_auto_min: u32,
    pub fps_auto_max: u32,
    /// The framerate used while the connection is clean; `None` is the
    /// highest.
    pub fps_auto_target: Option<u32>,
    /// `None` means automatic: the bitrate follows the network.
    pub bitrate_kbps: Option<u32>,
    /// The range an automatic bitrate moves in, in kbit/s. `None` leaves
    /// that end to kubecord: the top from resolution and framerate, the
    /// bottom at three tenths of the top.
    pub auto_bitrate_min: Option<u32>,
    pub auto_bitrate_max: Option<u32>,
    /// The bitrate used while the connection is clean; `None` is the
    /// highest (or, with no highest set, worked out from the picture).
    pub auto_bitrate_target: Option<u32>,
    /// With both automatic, which is kept longest when the connection
    /// struggles: 0 balanced, 1 bitrate, 2 framerate.
    pub adapt_priority: u32,
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
    /// Fixed Go Live buffering delay in milliseconds. Saved under a new
    /// name since 2026-11-19 so the old default of one second, which also
    /// delayed every recovery from loss by a second, does not carry over.
    #[serde(rename = "go_live_delay_ms")]
    pub stream_delay_ms: u64,
    /// Send every video packet a second time, 20 ms later.
    pub send_twice: bool,
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
    /// Where a stream's sound comes from: 0 none, 1 everything playing
    /// (except kubecord), 2 one program, 3 a node for a patchbay.
    pub share_audio: u32,
    /// The program shared when `share_audio` is 2, by its
    /// [`kc_media::share_audio::AudioApp::key`], and the name it showed.
    pub share_audio_app: Option<String>,
    pub share_audio_app_name: Option<String>,
    /// Boost on the user's own outgoing shared-stream sound (what viewers
    /// hear), as a linear multiplier; `1.0` leaves it unchanged. Does not
    /// touch the user's own output/monitor volume. The struct's `serde(default)`
    /// fills it with `1.0` (from [`Default`]) for settings files written
    /// before this existed.
    pub stream_audio_gain: f32,
    /// Width of the channel sidebar in pixels.
    pub sidebar_width: i32,
    /// Start the camera preview as soon as the camera popup opens.
    pub always_preview: bool,
    /// Show Discord's captchas in kubecord instead of sending the user to
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
    /// Least time between keyframes sent because packets were lost.
    pub loss_keyframe_ms: u32,
    /// Send a keyframe when none has gone out for this many seconds (0:
    /// only when asked or on loss). Saved under a new name since
    /// 2026-11-19 so the old default of 5 s, whose keyframes showed as a
    /// pulse of compression artifacts, does not carry over.
    #[serde(rename = "full_frame_gap_secs")]
    pub keyframe_gap_secs: u32,
    /// The most one person's volume can be turned up, as a multiple of
    /// normal (the top of the User Volume slider).
    pub volume_boost_cap: u32,
    /// Write the log file (see [`crate::logging`]).
    pub log_to_file: bool,
    /// Also log every media and voice event; far larger.
    pub log_detailed: bool,
    /// Log performance diagnostics (main-thread stalls and sidebar rebuild
    /// timings), to investigate a hitch. Off by default.
    #[serde(default)]
    pub log_diagnostics: bool,
    /// Show times on a 24-hour clock (`17:58`) instead of `5:58 PM`.
    #[serde(rename = "use_24_hour_time")]
    pub use_24_hour: bool,
    /// Free memory by unloading messages far from where you are reading (they
    /// reload when you scroll back). Off keeps every message you scroll into
    /// view. On by default.
    #[serde(default = "default_true")]
    pub unload_distant_messages: bool,
    /// Keep GIFs animating even when the window is not focused. Off by default
    /// (GIFs pause when unfocused, to save work).
    #[serde(default)]
    pub animate_gifs_unfocused: bool,
    /// Use the system's libadwaita colors (and its light/dark preference).
    /// On by default; off uses Discord's own palette, in `dark_scheme`.
    pub follow_system_scheme: bool,
    /// Discord's dark (true) or light theme, when not following the system.
    pub dark_scheme: bool,
    /// Use Discord's typeface (gg sans, downloaded on request). Off by default.
    pub discord_font: bool,
    /// Draw emoji with Twemoji, as Discord does. On by default.
    pub discord_emoji: bool,
    /// See through the main window's background to the desktop behind it.
    /// Off by default.
    pub transparent_background: bool,
    /// How opaque the background is while transparent, in percent (1-100).
    pub background_opacity: u8,
    /// Tell the compositor it decorates the window (KDE's server-decoration
    /// protocol), so niri draws no backdrop behind a see-through window.
    /// Off by default: KWin and sway would add a second title bar.
    pub kde_transparency_workaround: bool,
    /// What the message logger keeps, and for how long.
    pub logger: crate::logger::LoggerPrefs,
    /// Underline misspelled words in the message box. On by default.
    #[serde(default = "default_true")]
    pub spellcheck: bool,
    /// Noise suppression: 0 off, 1 standard (WebRTC), 2 strong (RNNoise).
    pub noise_suppression: u32,
    pub echo_cancellation: bool,
    pub auto_gain_control: bool,
    /// Let kubecord decide which microphone frames are speech; when off,
    /// `input_threshold_db` does.
    pub auto_input_sensitivity: bool,
    /// Microphone frames quieter than this (dBFS) are not sent.
    pub input_threshold_db: f32,
    /// Show desktop notifications.
    pub notify_desktop: bool,
    /// Put the message text in a notification.
    pub notify_preview: bool,
    /// Every DM message notifies (off: only pings in DMs).
    pub notify_all_dms: bool,
    /// Only mentions notify, whatever servers and channels say.
    pub notify_mentions_only: bool,
    /// `@everyone` and `@here` notify where the server allows them.
    pub notify_everyone: bool,
    /// Mentions of your roles notify where the server allows them.
    pub notify_roles: bool,
    /// Incoming DM and group calls notify.
    pub notify_calls: bool,
    /// When messages in the channel you have open stay quiet (no sound, no
    /// notification): one of [`OPEN_CHANNEL_QUIET`].
    pub open_channel_quiet: OpenChannelQuiet,
    /// Show the number of new notifications in the window title until the
    /// window is looked at again.
    pub notify_mark_window: bool,
    /// Play interface sounds at all.
    pub sounds_enabled: bool,
    /// Their volume, 0.0 to 1.0.
    pub sound_volume: f32,
    /// Sounds switched off, by [`crate::sounds::SoundEvent::key`].
    pub sounds_off: Vec<String>,
    /// Each sound's own volume, 0.0 to 1.0, by key; applied on top of
    /// `sound_volume`. A sound not listed plays at 1.0 (100 %).
    pub sound_volumes: std::collections::BTreeMap<String, f32>,
}

impl CapturePreferences {
    /// The volume one sound plays at: the overall sound volume times its own.
    pub fn sound_volume_of(&self, key: &str) -> f32 {
        self.sound_volume * self.sound_level(key)
    }

    /// A sound's own volume, 0.0 to 1.0 (1.0 when never set).
    pub fn sound_level(&self, key: &str) -> f32 {
        self.sound_volumes.get(key).copied().unwrap_or(1.0)
    }
}

pub const NOISE_SUPPRESSION: [&str; 3] = ["Off", "Standard", "Strong"];
/// The input sensitivity slider's range, in dBFS.
pub const THRESHOLD_MIN_DB: f32 = -100.0;
pub const THRESHOLD_MAX_DB: f32 = 0.0;

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
            fps_auto: false,
            fps_auto_min: 15,
            fps_auto_max: 30,
            fps_auto_target: None,
            bitrate_kbps: None,
            auto_bitrate_min: None,
            auto_bitrate_max: None,
            auto_bitrate_target: None,
            adapt_priority: 0,
            keyframe_secs: 0,
            drop_frames_after_ms: 250,
            leaky_queue: true,
            retransmit_ms: 2000,
            stream_delay_ms: 0,
            send_twice: false,
            cpu_encoder: true,
            input_device: String::new(),
            output_device: String::new(),
            input_volume: 1.0,
            output_volume: 1.0,
            camera: String::new(),
            share_source: 0,
            share_audio: 1,
            share_audio_app: None,
            share_audio_app_name: None,
            stream_audio_gain: 1.0,
            sidebar_width: SIDEBAR_WIDTH,
            always_preview: false,
            captcha_in_app: true,
            send_typing: true,
            call_height: CALL_HEIGHT,
            camera_format: 0,
            self_preview: true,
            loss_keyframe_ms: 1000,
            keyframe_gap_secs: 0,
            volume_boost_cap: 3,
            log_to_file: true,
            log_detailed: false,
            log_diagnostics: false,
            use_24_hour: false,
            unload_distant_messages: true,
            animate_gifs_unfocused: false,
            spellcheck: true,
            follow_system_scheme: true,
            dark_scheme: true,
            discord_font: false,
            discord_emoji: true,
            transparent_background: false,
            background_opacity: 80,
            kde_transparency_workaround: false,
            logger: crate::logger::LoggerPrefs::default(),
            noise_suppression: 0,
            echo_cancellation: false,
            auto_gain_control: false,
            auto_input_sensitivity: true,
            input_threshold_db: kc_media::processing::DEFAULT_THRESHOLD_DB,
            notify_desktop: true,
            notify_preview: true,
            notify_all_dms: true,
            notify_mentions_only: false,
            notify_everyone: true,
            notify_roles: true,
            notify_calls: true,
            open_channel_quiet: OpenChannelQuiet::WhileFocused,
            notify_mark_window: true,
            sounds_enabled: true,
            sound_volume: 0.6,
            sounds_off: Vec::new(),
            sound_volumes: std::collections::BTreeMap::new(),
        }
    }
}

impl CapturePreferences {
    pub fn video_settings(&self) -> VideoSettings {
        // An automatic framerate starts at the top of its range.
        // An automatic framerate's capture is built for its highest and
        // starts at its target.
        let fps = if self.fps_auto {
            self.fps_auto_max
        } else {
            self.fps
        };
        let fps_target = if self.fps_auto {
            self.fps_auto_target.unwrap_or(fps).min(fps)
        } else {
            fps
        };
        let automatic = self.bitrate_kbps.is_none();
        // An automatic bitrate's highest, and its target: either one set by
        // hand stands for the other when left automatic; with neither, it
        // is worked out from the picture and there is nothing above it.
        let (target, top) = match self.bitrate_kbps {
            Some(fixed) => (fixed, fixed),
            None => {
                let top = self
                    .auto_bitrate_max
                    .or(self.auto_bitrate_target)
                    .unwrap_or_else(|| {
                        VideoSettings::automatic_bitrate_kbps(self.width, self.height, fps_target)
                    });
                (self.auto_bitrate_target.unwrap_or(top).min(top), top)
            }
        };
        VideoSettings {
            width: self.width,
            height: self.height,
            fps,
            fps_target,
            bitrate_kbps: target,
            bitrate_top_kbps: top,
            bitrate_floor_kbps: match self.auto_bitrate_min {
                Some(floor) if automatic => floor.min(target),
                _ => 0,
            },
            // A bitrate set by hand is trusted over Discord's estimate.
            bitrate_follows_estimate: !(automatic
                && (self.auto_bitrate_max.is_some() || self.auto_bitrate_target.is_some())),
            adapt_priority: match self.adapt_priority {
                1 => kc_media::video::AdaptPriority::Bitrate,
                2 => kc_media::video::AdaptPriority::Framerate,
                _ => kc_media::video::AdaptPriority::Balanced,
            },
            fps_floor: if self.fps_auto {
                self.fps_auto_min.min(fps)
            } else {
                0
            },
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
            // A bitrate chosen by hand is kept to; Automatic follows the
            // network.
            adaptive_bitrate: self.bitrate_kbps.is_none(),
            loss_keyframe_ms: self.loss_keyframe_ms,
            keyframe_gap_secs: self.keyframe_gap_secs,
            send_twice: self.send_twice,
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

    /// Where a stream's sound comes from, or `None` for no sound. Choosing
    /// one program without having picked it shares nothing.
    pub fn audio_share(&self) -> Option<kc_media::share_audio::AudioShare> {
        use kc_media::share_audio::AudioShare;
        match self.share_audio {
            1 => Some(AudioShare::Desktop),
            2 => self
                .share_audio_app
                .clone()
                .map(|key| AudioShare::Application { key }),
            3 => Some(AudioShare::Node),
            _ => None,
        }
    }

    /// The notification choices for [`kc_state::notify::decide`].
    pub fn notify_prefs(&self) -> kc_state::notify::NotifyPrefs {
        kc_state::notify::NotifyPrefs {
            all_dm_messages: self.notify_all_dms,
            mentions_only: self.notify_mentions_only,
            everyone: self.notify_everyone,
            roles: self.notify_roles,
        }
    }

    /// The microphone processing these settings ask for.
    pub fn voice_processing(&self) -> kc_media::processing::VoiceProcessing {
        use kc_media::processing::{NoiseSuppression, VoiceProcessing};
        VoiceProcessing {
            noise_suppression: match self.noise_suppression {
                1 => NoiseSuppression::Standard,
                2 => NoiseSuppression::Strong,
                _ => NoiseSuppression::Off,
            },
            echo_cancellation: self.echo_cancellation,
            auto_gain: self.auto_gain_control,
            auto_sensitivity: self.auto_input_sensitivity,
            threshold_db: self.input_threshold_db,
        }
    }

    /// Pushes the chosen devices and volumes into the media pipelines.
    pub fn apply_devices(&self) {
        kc_media::processing::set(self.voice_processing());
        kc_media::devices::set_input_device(
            (!self.input_device.is_empty()).then(|| self.input_device.clone()),
        );
        kc_media::devices::set_output_device(
            (!self.output_device.is_empty()).then(|| self.output_device.clone()),
        );
        crate::session::set_self_preview(self.self_preview);
        crate::content::set_24_hour(self.use_24_hour);
        kc_state::set_unload_distant(self.unload_distant_messages);
        crate::animation::set_animate_unfocused(self.animate_gifs_unfocused);
        crate::spelling::set_enabled(self.spellcheck);
        crate::logger::set_prefs(self.logger, self.notify_prefs());
        crate::theme::apply_color_scheme(self);
        crate::logging::configure(self.log_to_file, self.log_detailed);
        crate::logging::set_diagnostics(self.log_diagnostics);
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
        self.fps_auto = defaults.fps_auto;
        self.fps_auto_min = defaults.fps_auto_min;
        self.fps_auto_max = defaults.fps_auto_max;
        self.fps_auto_target = defaults.fps_auto_target;
        self.bitrate_kbps = defaults.bitrate_kbps;
        self.auto_bitrate_min = defaults.auto_bitrate_min;
        self.auto_bitrate_max = defaults.auto_bitrate_max;
        self.auto_bitrate_target = defaults.auto_bitrate_target;
        self.adapt_priority = defaults.adapt_priority;
        self.keyframe_secs = defaults.keyframe_secs;
        self.drop_frames_after_ms = defaults.drop_frames_after_ms;
        self.leaky_queue = defaults.leaky_queue;
        self.retransmit_ms = defaults.retransmit_ms;
        self.stream_delay_ms = defaults.stream_delay_ms;
        self.send_twice = defaults.send_twice;
        self.cpu_encoder = defaults.cpu_encoder;
        self.camera_format = defaults.camera_format;
        self.loss_keyframe_ms = defaults.loss_keyframe_ms;
        self.keyframe_gap_secs = defaults.keyframe_gap_secs;
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
        if !FRAMERATES.contains(&self.fps_auto_max) {
            self.fps_auto_max = 30;
        }
        if !FRAMERATE_FLOORS.contains(&self.fps_auto_min) {
            self.fps_auto_min = 15;
        }
        if self
            .auto_bitrate_min
            .is_some_and(|floor| !BITRATE_FLOORS.contains(&floor))
        {
            self.auto_bitrate_min = None;
        }
        if self
            .auto_bitrate_max
            .is_some_and(|top| !BITRATES.contains(&top))
        {
            self.auto_bitrate_max = None;
        }
        if self
            .fps_auto_target
            .is_some_and(|fps| !FRAMERATES.contains(&fps))
        {
            self.fps_auto_target = None;
        }
        if self
            .auto_bitrate_target
            .is_some_and(|kbps| !BITRATES.contains(&kbps))
        {
            self.auto_bitrate_target = None;
        }
        if self.adapt_priority > 2 {
            self.adapt_priority = 0;
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
        if !LOSS_KEYFRAME_SPACINGS_MS.contains(&self.loss_keyframe_ms) {
            self.loss_keyframe_ms = 1000;
        }
        if !KEYFRAME_GAPS.contains(&self.keyframe_gap_secs) {
            self.keyframe_gap_secs = 0;
        }
        if !VOLUME_BOOST_CAPS.contains(&self.volume_boost_cap) {
            self.volume_boost_cap = 3;
        }
        if !self.sound_volume.is_finite() || !(0.0..=1.0).contains(&self.sound_volume) {
            self.sound_volume = 0.6;
        }
        if self.noise_suppression > 2 {
            self.noise_suppression = 0;
        }
        if !self.input_threshold_db.is_finite()
            || !(THRESHOLD_MIN_DB..=THRESHOLD_MAX_DB).contains(&self.input_threshold_db)
        {
            self.input_threshold_db = kc_media::processing::DEFAULT_THRESHOLD_DB;
        }
        if self.camera_format > 2 {
            self.camera_format = 0;
        }
        if self.share_audio > 3 {
            self.share_audio = 1;
        }
        if self.share_source > 2 {
            self.share_source = 0;
        }
        self.sidebar_width = self.sidebar_width.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self.call_height = self.call_height.max(CALL_HEIGHT_MIN);
        self.sound_volumes
            .retain(|_, level| level.is_finite() && (0.0..=1.0).contains(level));
    }
}

/// serde default for a bool field that should be `true` when a saved settings
/// file predates it.
fn default_true() -> bool {
    true
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
    fn defaults_are_the_recommended_settings() {
        let prefs = CapturePreferences::default();
        assert_eq!((prefs.width, prefs.height, prefs.fps), (1280, 720, 30));
        assert_eq!(prefs.bitrate_kbps, None);
        assert_eq!(prefs.stream_delay_ms, 0);
        assert!(!prefs.send_twice);
        assert_eq!(prefs.keyframe_secs, 0);
        // The processor encodes: every viewer can play what x264 makes.
        assert!(prefs.cpu_encoder);
        assert!(prefs.log_to_file && !prefs.log_detailed);
        assert!(!prefs.use_24_hour);
        // Automatic bitrate stays within what Discord carries for a stream.
        assert_eq!(VideoSettings::automatic_bitrate_kbps(1280, 720, 30), 1935);
        assert_eq!(VideoSettings::automatic_bitrate_kbps(1920, 1080, 30), 4354);
        assert_eq!(VideoSettings::automatic_bitrate_kbps(1920, 1080, 60), 6000);
    }

    #[test]
    fn saved_settings_without_the_clock_get_twelve_hours() {
        let old: CapturePreferences = serde_json::from_str(r#"{"fps": 24}"#).unwrap();
        assert!(!old.use_24_hour);
        let set: CapturePreferences =
            serde_json::from_str(r#"{"use_24_hour_time": true}"#).unwrap();
        assert!(set.use_24_hour);
    }

    #[test]
    fn old_settings_get_the_voice_defaults() {
        let mut old: CapturePreferences = serde_json::from_str(r#"{"fps": 30}"#).unwrap();
        old.normalize();
        assert!(old.notify_desktop && old.notify_preview && old.notify_calls);
        assert_eq!(old.notify_prefs(), kc_state::notify::NotifyPrefs::default());
        let processing = old.voice_processing();
        assert_eq!(processing, kc_media::processing::VoiceProcessing::default());
        assert!(processing.auto_sensitivity);

        let mut odd: CapturePreferences =
            serde_json::from_str(r#"{"noise_suppression": 7, "input_threshold_db": 12.0}"#)
                .unwrap();
        odd.normalize();
        assert_eq!(odd.noise_suppression, 0);
        assert_eq!(
            odd.input_threshold_db,
            kc_media::processing::DEFAULT_THRESHOLD_DB
        );

        let chosen: CapturePreferences = serde_json::from_str(
            r#"{"noise_suppression": 2, "echo_cancellation": true, "auto_input_sensitivity": false, "input_threshold_db": -42.0}"#,
        )
        .unwrap();
        let processing = chosen.voice_processing();
        assert_eq!(
            processing.noise_suppression,
            kc_media::processing::NoiseSuppression::Strong
        );
        assert!(processing.echo_cancellation && !processing.auto_gain);
        assert_eq!(
            processing.sensitivity(),
            kc_media::processing::Sensitivity::Manual(-42.0)
        );
    }

    #[test]
    fn automatic_ranges_reach_the_capture() {
        // Left alone: the top from the picture, Discord's estimate heeded.
        let mut prefs = CapturePreferences::default();
        let settings = prefs.video_settings();
        assert_eq!((settings.bitrate_floor_kbps, settings.fps_floor), (0, 0));
        assert!(settings.bitrate_follows_estimate);

        // A range set by hand, and a framerate that adapts.
        prefs.auto_bitrate_min = Some(2000);
        prefs.auto_bitrate_max = Some(6000);
        prefs.fps_auto = true;
        prefs.fps_auto_min = 15;
        prefs.fps_auto_max = 24;
        let settings = prefs.video_settings();
        assert!(settings.adaptive_bitrate && !settings.bitrate_follows_estimate);
        assert_eq!(
            (settings.bitrate_floor_kbps, settings.bitrate_kbps),
            (2000, 6000)
        );
        assert_eq!((settings.fps_floor, settings.fps), (15, 24));

        // A target inside the range: started at, above it only later.
        prefs.auto_bitrate_target = Some(4000);
        prefs.fps_auto_target = Some(20);
        let settings = prefs.video_settings();
        assert_eq!(
            (settings.bitrate_kbps, settings.bitrate_top_kbps),
            (4000, 6000)
        );
        assert_eq!((settings.fps_target, settings.fps), (20, 24));
        prefs.auto_bitrate_target = None;
        prefs.fps_auto_target = None;

        // A fixed bitrate has no range; a floor above the top is the top.
        prefs.bitrate_kbps = Some(2500);
        assert_eq!(prefs.video_settings().bitrate_floor_kbps, 0);
        prefs.bitrate_kbps = None;
        prefs.auto_bitrate_min = Some(4000);
        prefs.auto_bitrate_max = Some(2500);
        assert_eq!(prefs.video_settings().bitrate_floor_kbps, 2500);
        prefs.fps_auto_min = 30;
        assert_eq!(prefs.video_settings().fps_floor, 24);
    }

    #[test]
    fn only_an_automatic_bitrate_adapts() {
        let mut prefs = CapturePreferences::default();
        assert!(prefs.video_settings().adaptive_bitrate);
        assert_eq!(prefs.video_settings().loss_keyframe_ms, 1000);
        assert_eq!(prefs.video_settings().keyframe_gap_secs, 0);
        prefs.bitrate_kbps = Some(2500);
        let settings = prefs.video_settings();
        assert!(!settings.adaptive_bitrate);
        assert_eq!(settings.bitrate_kbps, 2500);
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
            cpu_encoder: false,
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
