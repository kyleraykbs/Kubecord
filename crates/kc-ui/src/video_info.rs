//! The information panel on a video: what size and rate it has, how much
//! is arriving, and for your own video how it fares on the way up. It is
//! opened from the (i) button on a tile or a pop-out window and measured
//! once a second.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use gtk::glib;
use gtk::prelude::*;

use kc_media::decode::FeedStats;

use crate::session::{AccountId, VideoUpload};
use crate::shell::AppState;
use crate::voice::VideoKey;

/// One measurement of a video, kept to work out rates from the next.
struct Sample {
    at: Instant,
    stats: FeedStats,
    shown: u64,
    /// When the keyframe count last went up, for a steady "N s ago" that
    /// counts real time rather than dividing frames by a fluctuating rate.
    keyframe_at: Instant,
    /// What the panel says until the next measurement.
    text: String,
}

/// Which videos show their information, and what that takes.
#[derive(Default)]
pub(crate) struct VideoInfo {
    shown: HashSet<VideoKey>,
    /// The panels on screen. Tiles are redrawn often, so these are weak.
    labels: Vec<(VideoKey, glib::WeakRef<gtk::Label>)>,
    samples: HashMap<VideoKey, Sample>,
    /// The latest report on our own camera (`false`) and stream (`true`).
    pub uploads: HashMap<(AccountId, bool), VideoUpload>,
}

impl VideoInfo {
    pub(crate) fn shown(&self, key: VideoKey) -> bool {
        self.shown.contains(&key)
    }

    /// Shows or hides a video's information; returns whether it now shows.
    pub(crate) fn toggle(&mut self, key: VideoKey) -> bool {
        if self.shown.remove(&key) {
            self.samples.remove(&key);
            false
        } else {
            self.shown.insert(key);
            true
        }
    }
}

const MEASURING: &str = "Measuring…";

/// A number with thousands separated, as `12,345`.
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// The panel's text, from two measurements `seconds` apart.
fn describe(
    size: (i32, i32),
    before: (FeedStats, u64),
    now: (FeedStats, u64),
    seconds: f64,
    // Real seconds since the last full frame, or None when none has arrived.
    since_keyframe: Option<f64>,
    upload: Option<VideoUpload>,
) -> String {
    let (earlier, shown_before) = before;
    let (stats, shown) = now;
    let seconds = seconds.max(0.001);
    let fps = (shown.saturating_sub(shown_before)) as f64 / seconds;
    let kbps = (stats.bytes.saturating_sub(earlier.bytes)) as f64 * 8.0 / 1000.0 / seconds;
    let mut lines = vec![
        format!("{} × {}  ·  {:.0} fps", size.0, size.1, fps),
        format!("{} kbit/s  ·  H.264", grouped(kbps.round() as u64)),
        format!(
            "Frames: {} in, {} shown",
            grouped(stats.frames),
            grouped(shown)
        ),
    ];
    // A full frame is one that needs none before it; a viewer who lost
    // the picture gets it back at the next one.
    lines.push(match since_keyframe {
        None => "Full frames: none yet".to_owned(),
        Some(ago) => format!(
            "Full frames: {} · {:.0} s ago",
            grouped(stats.keyframes),
            ago
        ),
    });
    if let Some(upload) = upload {
        if upload.sent > 0 {
            lines.push(format!(
                "Upload loss: {:.1}%",
                f64::from(upload.lost) * 100.0 / f64::from(upload.sent)
            ));
        }
        match (upload.target_kbps, upload.fps) {
            (Some(kbps), Some(fps)) => lines.push(format!(
                "Adapting: {} kbit/s, {fps} fps",
                grouped(u64::from(kbps))
            )),
            (Some(kbps), None) => {
                lines.push(format!("Adapting: {} kbit/s", grouped(u64::from(kbps))))
            }
            (None, Some(fps)) => lines.push(format!("Adapting: {fps} fps")),
            (None, None) => {}
        }
    }
    lines.join("\n")
}

impl AppState {
    /// A panel for `key`'s information, kept current while it is on screen.
    pub(crate) fn video_info_label(self: &Rc<Self>, key: VideoKey) -> gtk::Label {
        let mut info = self.video_info.borrow_mut();
        let text = info
            .samples
            .get(&key)
            .map(|sample| sample.text.as_str())
            .unwrap_or(MEASURING);
        let label = gtk::Label::new(Some(text));
        label.add_css_class("kc-video-info");
        label.set_xalign(0.0);
        label.set_can_target(false);
        info.labels.push((key, label.downgrade()));
        label
    }

    /// Measures the videos whose information is showing and updates their
    /// panels. Runs once a second.
    pub(crate) fn refresh_video_info(self: &Rc<Self>) {
        let mut info = self.video_info.borrow_mut();
        info.labels.retain(|(_, label)| label.upgrade().is_some());
        if info.shown.is_empty() {
            return;
        }
        let me = self
            .with_session(|session| session.user.as_ref().map(|user| user.id))
            .flatten();
        let now = Instant::now();
        let keys: Vec<VideoKey> = info.shown.iter().copied().collect();
        for key in keys {
            let measured = self.videos.borrow().get(&key).map(|video| {
                let (stats, shown) = video.stats();
                let size = (
                    video.paintable.intrinsic_width(),
                    video.paintable.intrinsic_height(),
                );
                (stats, shown, size)
            });
            let Some((stats, shown, size)) = measured else {
                // The video ended; its panel goes with it.
                info.samples.remove(&key);
                continue;
            };
            let upload = (Some(key.1) == me)
                .then(|| info.uploads.get(&(key.0, key.2)).copied())
                .flatten();
            // The last full frame's time carries forward until a new one
            // arrives (the keyframe count goes up), so "N s ago" only ever
            // climbs and then resets, never jitters.
            let keyframe_at = match info.samples.get(&key) {
                Some(before) if stats.keyframes > before.stats.keyframes => now,
                Some(before) => before.keyframe_at,
                None => now,
            };
            let since_keyframe =
                (stats.keyframes > 0).then(|| now.duration_since(keyframe_at).as_secs_f64());
            let text = match info.samples.get(&key) {
                Some(before) => describe(
                    size,
                    (before.stats, before.shown),
                    (stats, shown),
                    now.duration_since(before.at).as_secs_f64(),
                    since_keyframe,
                    upload,
                ),
                None => MEASURING.to_owned(),
            };
            for (_, label) in info.labels.iter().filter(|(owner, _)| *owner == key) {
                if let Some(label) = label.upgrade() {
                    label.set_label(&text);
                }
            }
            info.samples.insert(
                key,
                Sample {
                    at: now,
                    stats,
                    shown,
                    keyframe_at,
                    text,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(frames: u64, bytes: u64, keyframes: u64, since_keyframe: u64) -> FeedStats {
        FeedStats {
            frames,
            bytes,
            keyframes,
            since_keyframe,
        }
    }

    #[test]
    fn the_panel_says_size_rate_and_bitrate() {
        // One second: 30 frames in and shown, 250,000 bytes.
        let text = describe(
            (1280, 720),
            (stats(1000, 5_000_000, 3, 10), 990),
            (stats(1030, 5_250_000, 3, 40), 1020),
            1.0,
            Some(1.0),
            None,
        );
        assert_eq!(
            text,
            "1280 × 720  ·  30 fps\n2,000 kbit/s  ·  H.264\nFrames: 1,030 in, 1,020 shown\nFull frames: 3 · 1 s ago"
        );
    }

    #[test]
    fn your_own_video_also_says_how_the_upload_fares() {
        let upload = VideoUpload {
            sent: 1000,
            lost: 33,
            target_kbps: Some(3400),
            fps: Some(24),
        };
        let text = describe(
            (1920, 1080),
            (stats(0, 0, 0, 0), 0),
            (stats(48, 850_000, 0, 48), 48),
            2.0,
            None,
            Some(upload),
        );
        assert!(text.starts_with("1920 × 1080  ·  24 fps\n3,400 kbit/s"));
        assert!(text.contains("Full frames: none yet"));
        assert!(text.ends_with("Upload loss: 3.3%\nAdapting: 3,400 kbit/s, 24 fps"));
    }

    #[test]
    fn numbers_are_grouped_in_thousands() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }

    #[test]
    fn toggling_shows_and_hides() {
        let mut info = VideoInfo::default();
        let key = (1, kc_core::Snowflake::new(2), true);
        assert!(!info.shown(key));
        assert!(info.toggle(key));
        assert!(info.shown(key));
        assert!(!info.toggle(key));
        assert!(!info.shown(key));
    }
}
