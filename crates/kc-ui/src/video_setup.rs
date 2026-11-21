//! Turning on the camera and going live: the popouts that ask first, and the
//! pieces they share with the settings panel (the preview box and the
//! quality controls).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_media::decode::VideoFeed;
use kc_media::portal::{CameraAccess, ShareSources};
use kc_media::share_audio::AudioApp;
use kc_media::video::{
    EncodedFrame, FrameTap, PreparedCapture, VideoSettings, VideoSource, is_keyframe,
};

use crate::live::LiveVideo;
use crate::session::{AccountId, CameraPurpose, Command};
use crate::settings::{
    BITRATE_FLOORS, BITRATES, CAMERA_FORMATS, CapturePreferences, DROP_AFTER_MS, FRAMERATE_FLOORS,
    FRAMERATES, KEYFRAME_GAPS, KEYFRAME_INTERVALS, LOSS_KEYFRAME_SPACINGS_MS, RESOLUTIONS,
    RETRANSMIT_WINDOWS_MS, SHARE_SOURCES, STREAM_DELAYS,
};
use crate::shell::AppState;

/// Index of "Camera" in the share popup's source list, after the screen
/// choices.
const CAMERA_SOURCE: u32 = SHARE_SOURCES.len() as u32;

const ONE_TIME_NOTE: &str = "Changes here apply this time only. To change them permanently, go to Settings → Voice &amp; Video.";

// -- preview -----------------------------------------------------------------

/// A video box with a button over it. Running, it shows the capture encoded
/// and decoded again, so it looks the way viewers will see it.
pub(crate) struct Preview {
    picture: gtk::Picture,
    pub(crate) button: gtk::Button,
    idle_label: Cell<&'static str>,
    live: RefCell<Option<LiveVideo>>,
    source: RefCell<Option<VideoSource>>,
    /// Where a screen preview's frames go, so going live can take the
    /// running capture over instead of reconnecting to the screen.
    tap: RefCell<Option<FrameTap>>,
    /// Bumped by every start and stop, so a capture that finishes starting
    /// after its preview was stopped is thrown away.
    generation: Cell<u64>,
}

impl Preview {
    pub(crate) fn new(idle_label: &'static str, height: i32) -> (Rc<Self>, gtk::Overlay) {
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_can_shrink(true);
        let button = gtk::Button::with_label(idle_label);
        button.add_css_class("pill");
        button.set_halign(gtk::Align::Center);
        button.set_valign(gtk::Align::Center);
        let frame = gtk::Overlay::new();
        frame.add_css_class("kc-preview");
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.set_size_request(-1, height);
        frame.set_child(Some(&picture));
        frame.add_overlay(&button);
        let preview = Rc::new(Self {
            picture,
            button,
            idle_label: Cell::new(idle_label),
            live: RefCell::new(None),
            source: RefCell::new(None),
            tap: RefCell::new(None),
            generation: Cell::new(0),
        });
        (preview, frame)
    }

    pub(crate) fn running(&self) -> bool {
        self.live.borrow().is_some()
    }

    pub(crate) fn set_idle_label(&self, label: &'static str) {
        self.idle_label.set(label);
        if !self.running() {
            self.button.set_label(label);
        }
    }

    /// Starts showing a new feed; the capture is attached when it is up.
    fn begin(&self) -> Option<(Arc<VideoFeed>, u64)> {
        self.stop();
        let feed = Arc::new(VideoFeed::default());
        let live = LiveVideo::start(feed.clone())?;
        self.picture.set_paintable(Some(&live.paintable));
        *self.live.borrow_mut() = Some(live);
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.button.set_label("Stop Preview");
        self.button.set_valign(gtk::Align::End);
        self.button.set_margin_bottom(10);
        Some((feed, generation))
    }

    fn finish(
        &self,
        app: &Rc<AppState>,
        generation: u64,
        result: Result<VideoSource, String>,
        tap: Option<FrameTap>,
    ) {
        match result {
            // A stale capture is dropped here, which stops it.
            Ok(source) => {
                if self.generation.get() == generation && self.running() {
                    *self.source.borrow_mut() = Some(source);
                    *self.tap.borrow_mut() = tap;
                }
            }
            Err(error) => {
                if self.generation.get() == generation {
                    self.stop();
                }
                app.set_status(&format!("Preview: {error}"));
            }
        }
    }

    /// Takes a running screen preview's capture, still running, to go live
    /// with. Its frames are dropped until the stream points the tap at
    /// itself.
    pub(crate) fn take_capture(&self) -> Option<PreparedCapture> {
        let tap = self.tap.borrow_mut().take()?;
        let source = self.source.borrow_mut().take()?;
        tap.clear();
        Some(PreparedCapture { source, tap })
    }

    pub(crate) fn stop(&self) {
        self.generation.set(self.generation.get() + 1);
        self.tap.borrow_mut().take();
        self.source.borrow_mut().take();
        self.live.borrow_mut().take();
        self.picture.set_paintable(None::<&gtk::gdk::Paintable>);
        self.button.set_label(self.idle_label.get());
        self.button.set_valign(gtk::Align::Center);
        self.button.set_margin_bottom(0);
    }
}

/// Which camera a preview opens.
pub(crate) enum CameraPick {
    Node(u32),
    /// By name, falling back to the first camera.
    Named(String),
}

pub(crate) fn start_camera_preview(
    app: &Rc<AppState>,
    preview: &Rc<Preview>,
    access: Option<CameraAccess>,
    pick: CameraPick,
    settings: VideoSettings,
) {
    let Some((feed, generation)) = preview.begin() else {
        app.set_status("Video preview is unavailable");
        return;
    };
    let preview = preview.clone();
    spawn_local(
        app,
        async move {
            let access = match access {
                Some(access) => access,
                None => kc_media::portal::request_cameras()
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "no camera available".to_owned())?,
            };
            let camera = match pick {
                CameraPick::Node(node_id) => access.select(node_id),
                CameraPick::Named(name) => access
                    .cameras
                    .iter()
                    .find(|camera| camera.name == name)
                    .and_then(|camera| access.select(camera.node_id)),
            }
            .or_else(|| access.preferred())
            .ok_or_else(|| "no camera available".to_owned())?;
            VideoSource::camera_portal(camera, settings, preview_sink(feed))
                .map_err(|error| error.to_string())
        },
        move |app, result| preview.finish(app, generation, result, None),
    );
}

/// Opens the screen picker and previews the choice. The frames go through a
/// tap, so the capture can become the stream later.
pub(crate) fn start_screen_preview(
    app: &Rc<AppState>,
    preview: &Rc<Preview>,
    sources: ShareSources,
    settings: VideoSettings,
) {
    let Some((feed, generation)) = preview.begin() else {
        app.set_status("Video preview is unavailable");
        return;
    };
    let parent = app.window_identifier.borrow().clone();
    let preview = preview.clone();
    let tap = FrameTap::default();
    tap.set(preview_sink(feed));
    let capture_tap = tap.clone();
    spawn_local(
        app,
        async move {
            let portal = tokio::time::timeout(
                Duration::from_secs(120),
                kc_media::portal::request_sources(parent.as_deref(), sources),
            )
            .await
            .map_err(|_| "the screen picker timed out".to_owned())?
            .map_err(|error| error.to_string())?;
            VideoSource::screen(portal, settings, capture_tap.sink())
                .map_err(|error| error.to_string())
        },
        move |app, result| preview.finish(app, generation, result, Some(tap)),
    );
}

/// Encodes each captured frame and decodes it into the preview.
fn preview_sink(feed: Arc<VideoFeed>) -> impl FnMut(EncodedFrame) + Send + 'static {
    move |frame| {
        let keyframe = is_keyframe(&frame.data);
        feed.push(frame.data, keyframe);
    }
}

/// Runs `future` on the Tokio runtime, then `done` with its result on the GTK
/// thread. Unlike `AppState::spawn`, `done` may hold GTK objects.
pub(crate) fn spawn_local<T: Send + 'static>(
    app: &Rc<AppState>,
    future: impl std::future::Future<Output = T> + Send + 'static,
    done: impl FnOnce(&Rc<AppState>, T) + 'static,
) {
    let handle = app.runtime.spawn(future);
    let app = app.clone();
    glib::spawn_future_local(async move {
        if let Ok(result) = handle.await {
            done(&app, result);
        }
    });
}

// -- quality controls --------------------------------------------------------

/// Where the quality controls write: the saved preferences in Settings, a
/// one-time copy in the popouts.
pub(crate) type Apply = Rc<dyn Fn(&dyn Fn(&mut CapturePreferences))>;

pub(crate) struct QualityControls {
    pub rows: Vec<gtk::Widget>,
    pub advanced: Vec<adw::ActionRow>,
    /// Shows other preferences in the controls (after a reset).
    pub show: Rc<dyn Fn(&CapturePreferences)>,
}

/// Resolution, framerate, bitrate, the encoder and, for streams, the Go Live
/// delay; plus the advanced settings as separate rows.
pub(crate) fn quality_controls(
    initial: &CapturePreferences,
    stream: bool,
    apply: Apply,
) -> QualityControls {
    let resolution_names: Vec<&str> = RESOLUTIONS.iter().map(|(_, _, name)| *name).collect();
    let resolution = gtk::DropDown::from_strings(&resolution_names);
    let fps_names: Vec<String> = FRAMERATES.iter().map(|fps| format!("{fps} fps")).collect();
    let mut framerate_names = vec!["Automatic".to_owned()];
    framerate_names.extend(fps_names.iter().cloned());
    let framerate = string_dropdown(&framerate_names);
    // The range an automatic framerate moves in.
    let fps_floor_names: Vec<String> = FRAMERATE_FLOORS
        .iter()
        .map(|fps| format!("{fps} fps"))
        .collect();
    let fps_min = string_dropdown(&fps_floor_names);
    let fps_max = string_dropdown(&fps_names);
    let mut fps_target_names = vec!["Automatic".to_owned()];
    fps_target_names.extend(fps_names.iter().cloned());
    let fps_target = string_dropdown(&fps_target_names);
    let mut bitrate_names = vec!["Automatic".to_owned()];
    bitrate_names.extend(BITRATES.iter().map(|kbps| format!("{kbps} kbps")));
    let bitrate = string_dropdown(&bitrate_names);
    // The range an automatic bitrate moves in; either end can be left to
    // kubecord.
    let mut floor_names = vec!["Automatic".to_owned()];
    floor_names.extend(BITRATE_FLOORS.iter().map(|kbps| format!("{kbps} kbps")));
    let bitrate_min = string_dropdown(&floor_names);
    let bitrate_max = string_dropdown(&bitrate_names);
    let bitrate_target = string_dropdown(&bitrate_names);
    let fps_range = range_row(
        "Framerate Range",
        &fps_target,
        &fps_min,
        &fps_max,
        "Target is used while the connection is clean. It drops toward Lowest while packets are being lost, and climbs toward Highest only after 30 seconds without loss. Automatic Target is Highest. Fewer frames leave more of the bitrate for each one.",
    );
    let bitrate_range = range_row(
        "Bitrate Range",
        &bitrate_target,
        &bitrate_min,
        &bitrate_max,
        "Target is used while the connection is clean. It drops toward Lowest while packets are being lost, and climbs toward Highest only after 30 seconds without loss. Left on Automatic, Target and Highest are the same: one set by hand stands for the other, and with neither set it is worked out from the resolution and framerate and kept under Discord's own estimate (about 4 Mbit/s for a stream). Values set here are used even above that estimate. Automatic Lowest is three tenths of Target.",
    );
    let priority = gtk::DropDown::from_strings(&ADAPT_PRIORITIES);
    let priority_row = setting_row("When the Connection Struggles", &priority);
    priority_row.set_subtitle(
        "With both on Automatic: Prefer Bitrate lowers the framerate first, keeping each frame sharp. Prefer Framerate lowers the bitrate first, keeping motion smooth. Balanced lowers both together.",
    );
    priority_row.set_subtitle_lines(0);
    let delay = gtk::DropDown::from_strings(&["None", "0.5 s", "1 s", "2 s", "3 s"]);
    let cpu_encoder = gtk::Switch::new();
    let camera_format = gtk::DropDown::from_strings(&CAMERA_FORMATS);

    let keyframe_names: Vec<String> = KEYFRAME_INTERVALS
        .iter()
        .map(|seconds| match seconds {
            0 => "Only when needed".to_owned(),
            seconds => format!("Every {seconds} s"),
        })
        .collect();
    let keyframe = string_dropdown(&keyframe_names);
    keyframe.set_tooltip_text(Some(
        "For Go Live: whether full frames are also sent on a schedule. One is always sent when a viewer asks or packets are lost, which is all the official client does; scheduled ones made viewers hitch. Your camera never schedules them.",
    ));
    let retransmit_names: Vec<String> = RETRANSMIT_WINDOWS_MS
        .iter()
        .map(|ms| {
            if *ms == 0 {
                "Off".to_owned()
            } else {
                format!("{} s", ms / 1000)
            }
        })
        .collect();
    let retransmit = string_dropdown(&retransmit_names);
    retransmit.set_tooltip_text(Some(
        "How much recently sent audio and video is kept so it can be resent after a network dropout. Longer windows repair longer dropouts; Off never resends.",
    ));
    let drop_names: Vec<String> = DROP_AFTER_MS
        .iter()
        .map(|ms| {
            if *ms == 0 {
                "Never".to_owned()
            } else {
                format!("{ms} ms")
            }
        })
        .collect();
    let drop_late = string_dropdown(&drop_names);
    let loss_names: Vec<String> = LOSS_KEYFRAME_SPACINGS_MS
        .iter()
        .map(|ms| match ms {
            500 => "Every 0.5 s".to_owned(),
            ms => format!("Every {} s", ms / 1000),
        })
        .collect();
    let loss_keyframes = string_dropdown(&loss_names);
    let gap_names: Vec<String> = KEYFRAME_GAPS
        .iter()
        .map(|seconds| match seconds {
            0 => "Off".to_owned(),
            seconds => format!("Every {seconds} s"),
        })
        .collect();
    let keyframe_gap = string_dropdown(&gap_names);
    let send_twice = gtk::Switch::new();
    let leaky = gtk::Switch::new();
    leaky.set_tooltip_text(Some(
        "Leaky queue: when the encoder is slow, discard old frames instead of letting latency build.",
    ));

    let show: Rc<dyn Fn(&CapturePreferences)> = {
        let resolution = resolution.clone();
        let framerate = framerate.clone();
        let bitrate = bitrate.clone();
        let (fps_min, fps_max) = (fps_min.clone(), fps_max.clone());
        let (bitrate_min, bitrate_max) = (bitrate_min.clone(), bitrate_max.clone());
        let (fps_target, bitrate_target) = (fps_target.clone(), bitrate_target.clone());
        let (fps_range, bitrate_range) = (fps_range.clone(), bitrate_range.clone());
        let (priority, priority_row) = (priority.clone(), priority_row.clone());
        let delay = delay.clone();
        let cpu_encoder = cpu_encoder.clone();
        let camera_format = camera_format.clone();
        let keyframe = keyframe.clone();
        let retransmit = retransmit.clone();
        let drop_late = drop_late.clone();
        let loss_keyframes = loss_keyframes.clone();
        let keyframe_gap = keyframe_gap.clone();
        let send_twice = send_twice.clone();
        let leaky = leaky.clone();
        Rc::new(move |prefs: &CapturePreferences| {
            resolution.set_selected(
                RESOLUTIONS
                    .iter()
                    .position(|(width, height, _)| *width == prefs.width && *height == prefs.height)
                    .unwrap_or(1) as u32,
            );
            framerate.set_selected(if prefs.fps_auto {
                0
            } else {
                position(&FRAMERATES, &prefs.fps, 1) + 1
            });
            fps_min.set_selected(position(&FRAMERATE_FLOORS, &prefs.fps_auto_min, 2));
            fps_max.set_selected(position(&FRAMERATES, &prefs.fps_auto_max, 1));
            // 0 is Automatic; the fixed values follow.
            let listed = |values: &[u32], value: Option<u32>| {
                value
                    .and_then(|value| values.iter().position(|candidate| *candidate == value))
                    .map(|index| index + 1)
                    .unwrap_or(0) as u32
            };
            bitrate.set_selected(listed(&BITRATES, prefs.bitrate_kbps));
            bitrate_min.set_selected(listed(&BITRATE_FLOORS, prefs.auto_bitrate_min));
            bitrate_max.set_selected(listed(&BITRATES, prefs.auto_bitrate_max));
            bitrate_target.set_selected(listed(&BITRATES, prefs.auto_bitrate_target));
            fps_target.set_selected(listed(&FRAMERATES, prefs.fps_auto_target));
            // The ranges only mean something on Automatic.
            fps_range.set_visible(prefs.fps_auto);
            bitrate_range.set_visible(prefs.bitrate_kbps.is_none());
            priority.set_selected(prefs.adapt_priority.min(2));
            priority_row.set_visible(prefs.fps_auto && prefs.bitrate_kbps.is_none());
            delay.set_selected(position(&STREAM_DELAYS, &prefs.stream_delay_ms, 0));
            cpu_encoder.set_active(prefs.cpu_encoder);
            camera_format.set_selected(prefs.camera_format.min(2));
            keyframe.set_selected(position(&KEYFRAME_INTERVALS, &prefs.keyframe_secs, 0));
            retransmit.set_selected(position(&RETRANSMIT_WINDOWS_MS, &prefs.retransmit_ms, 2));
            drop_late.set_selected(position(&DROP_AFTER_MS, &prefs.drop_frames_after_ms, 2));
            loss_keyframes.set_selected(position(
                &LOSS_KEYFRAME_SPACINGS_MS,
                &prefs.loss_keyframe_ms,
                1,
            ));
            keyframe_gap.set_selected(position(&KEYFRAME_GAPS, &prefs.keyframe_gap_secs, 0));
            send_twice.set_active(prefs.send_twice);
            leaky.set_active(prefs.leaky_queue);
        })
    };
    // Show the starting values before the handlers exist, so they do not
    // write them back.
    show(initial);

    let target = apply.clone();
    resolution.connect_selected_notify(move |dropdown| {
        if let Some((width, height, _)) = RESOLUTIONS.get(dropdown.selected() as usize) {
            let (width, height) = (*width, *height);
            target(&move |prefs| {
                prefs.width = width;
                prefs.height = height;
            });
        }
    });
    // The priority only matters with both automatic.
    let both_automatic = {
        let (framerate, bitrate, priority_row) =
            (framerate.clone(), bitrate.clone(), priority_row.clone());
        move || priority_row.set_visible(framerate.selected() == 0 && bitrate.selected() == 0)
    };
    let target = apply.clone();
    let range = fps_range.clone();
    let update = both_automatic.clone();
    framerate.connect_selected_notify(move |dropdown| {
        let fixed = match dropdown.selected() as usize {
            0 => None,
            index => FRAMERATES.get(index - 1).copied(),
        };
        range.set_visible(fixed.is_none());
        update();
        target(&move |prefs| {
            prefs.fps_auto = fixed.is_none();
            if let Some(fps) = fixed {
                prefs.fps = fps;
            }
        });
    });
    let target = apply.clone();
    fps_target.connect_selected_notify(move |dropdown| {
        let fps = match dropdown.selected() as usize {
            0 => None,
            index => FRAMERATES.get(index - 1).copied(),
        };
        target(&move |prefs| prefs.fps_auto_target = fps);
    });
    let target = apply.clone();
    bitrate_target.connect_selected_notify(move |dropdown| {
        let kbps = match dropdown.selected() as usize {
            0 => None,
            index => BITRATES.get(index - 1).copied(),
        };
        target(&move |prefs| prefs.auto_bitrate_target = kbps);
    });
    let target = apply.clone();
    fps_min.connect_selected_notify(move |dropdown| {
        if let Some(fps) = FRAMERATE_FLOORS.get(dropdown.selected() as usize).copied() {
            target(&move |prefs| prefs.fps_auto_min = fps);
        }
    });
    let target = apply.clone();
    fps_max.connect_selected_notify(move |dropdown| {
        if let Some(fps) = FRAMERATES.get(dropdown.selected() as usize).copied() {
            target(&move |prefs| prefs.fps_auto_max = fps);
        }
    });
    let target = apply.clone();
    let range = bitrate_range.clone();
    let update = both_automatic;
    bitrate.connect_selected_notify(move |dropdown| {
        let kbps = match dropdown.selected() as usize {
            0 => None,
            index => BITRATES.get(index - 1).copied(),
        };
        range.set_visible(kbps.is_none());
        update();
        target(&move |prefs| prefs.bitrate_kbps = kbps);
    });
    let target = apply.clone();
    bitrate_min.connect_selected_notify(move |dropdown| {
        let kbps = match dropdown.selected() as usize {
            0 => None,
            index => BITRATE_FLOORS.get(index - 1).copied(),
        };
        target(&move |prefs| prefs.auto_bitrate_min = kbps);
    });
    let target = apply.clone();
    priority.connect_selected_notify(move |dropdown| {
        let chosen = dropdown.selected().min(2);
        target(&move |prefs| prefs.adapt_priority = chosen);
    });
    let target = apply.clone();
    bitrate_max.connect_selected_notify(move |dropdown| {
        let kbps = match dropdown.selected() as usize {
            0 => None,
            index => BITRATES.get(index - 1).copied(),
        };
        target(&move |prefs| prefs.auto_bitrate_max = kbps);
    });
    let target = apply.clone();
    delay.connect_selected_notify(move |dropdown| {
        if let Some(delay) = STREAM_DELAYS.get(dropdown.selected() as usize).copied() {
            target(&move |prefs| prefs.stream_delay_ms = delay);
        }
    });
    let target = apply.clone();
    cpu_encoder.connect_active_notify(move |switch| {
        let active = switch.is_active();
        target(&move |prefs| prefs.cpu_encoder = active);
    });
    let target = apply.clone();
    camera_format.connect_selected_notify(move |dropdown| {
        let format = dropdown.selected();
        target(&move |prefs| prefs.camera_format = format);
    });
    let target = apply.clone();
    keyframe.connect_selected_notify(move |dropdown| {
        if let Some(seconds) = KEYFRAME_INTERVALS
            .get(dropdown.selected() as usize)
            .copied()
        {
            target(&move |prefs| prefs.keyframe_secs = seconds);
        }
    });
    let target = apply.clone();
    retransmit.connect_selected_notify(move |dropdown| {
        if let Some(ms) = RETRANSMIT_WINDOWS_MS
            .get(dropdown.selected() as usize)
            .copied()
        {
            target(&move |prefs| prefs.retransmit_ms = ms);
        }
    });
    let target = apply.clone();
    drop_late.connect_selected_notify(move |dropdown| {
        if let Some(ms) = DROP_AFTER_MS.get(dropdown.selected() as usize).copied() {
            target(&move |prefs| prefs.drop_frames_after_ms = ms);
        }
    });
    let target = apply.clone();
    loss_keyframes.connect_selected_notify(move |dropdown| {
        if let Some(ms) = LOSS_KEYFRAME_SPACINGS_MS
            .get(dropdown.selected() as usize)
            .copied()
        {
            target(&move |prefs| prefs.loss_keyframe_ms = ms);
        }
    });
    let target = apply.clone();
    keyframe_gap.connect_selected_notify(move |dropdown| {
        if let Some(seconds) = KEYFRAME_GAPS.get(dropdown.selected() as usize).copied() {
            target(&move |prefs| prefs.keyframe_gap_secs = seconds);
        }
    });
    let target = apply.clone();
    send_twice.connect_active_notify(move |switch| {
        let active = switch.is_active();
        target(&move |prefs| prefs.send_twice = active);
    });
    let target = apply;
    leaky.connect_active_notify(move |switch| {
        let active = switch.is_active();
        target(&move |prefs| prefs.leaky_queue = active);
    });

    // Each row says what the setting trades, and which way to move it.
    let guided = |title: &str, widget: &gtk::DropDown, guide: &str| {
        let row = setting_row(title, widget);
        row.set_subtitle(guide);
        row.set_subtitle_lines(0);
        row
    };
    let mut rows: Vec<gtk::Widget> = vec![
        guided(
            "Resolution",
            &resolution,
            "1280 × 720 is the dependable choice. 1920 × 1080 is sharper but needs about twice the upload, and lost packets show more.",
        )
        .upcast(),
        guided(
            "Framerate",
            &framerate,
            "30 fps suits calls and most screens. 60 fps is for games and fast motion; it doubles the upload and the encoding work. Automatic drops the framerate while packets are being lost and brings it back when the connection is clean, within the range below.",
        )
        .upcast(),
        fps_range.upcast(),
        guided(
            "Bitrate",
            &bitrate,
            "Automatic backs off while packets are being lost and climbs back when the connection is clean, within the range below. A fixed value is kept to whatever happens.",
        )
        .upcast(),
        bitrate_range.upcast(),
        priority_row.upcast(),
    ];
    if stream {
        rows.push(
            guided(
                "Go Live Delay",
                &delay,
                "Holds your stream back so frames leave at an even pace. It also holds back the full frame that repairs a freeze, so with a delay every such freeze lasts at least that long. None is best for most; add delay only if the picture stutters without it.",
            )
            .upcast(),
        );
    }
    let encoder_row = setting_row("Use Software Encoder (x264)", &cpu_encoder);
    encoder_row.set_subtitle(
        "The dependable choice. Off uses your graphics card when it can, which saves processor time, but its full frames are about three times as large and it sends one at least every 1,024 frames and at every change of bitrate, so an Automatic bitrate moves at most once in 10 seconds.",
    );
    encoder_row.set_subtitle_lines(0);
    encoder_row.set_activatable_widget(Some(&cpu_encoder));
    rows.push(encoder_row.upcast());
    let format_row = setting_row("Camera Format", &camera_format);
    format_row.set_subtitle(
        "Leave on Automatic. MJPEG keeps a webcam at full framerate at large sizes (raw 720p is often only 10 fps); Automatic tries it first, then raw.",
    );
    format_row.set_subtitle_lines(0);
    rows.push(format_row.upcast());

    let drop_row = guided(
        "Drop Late Frames After",
        &drop_late,
        "A frame that waits this long past the Go Live delay is skipped instead of sent late, so the stream stays current. 250 ms is a good default; Never sends everything, however late.",
    );
    let twice_row = setting_row("Send Packets Twice", &send_twice);
    twice_row.set_subtitle(
        "Sends every video packet again 20 ms later, so a lost one is usually replaced before viewers notice. It doubles your video upload: use it when the connection drops packets but has room to spare, and not to rescue a bitrate that is already too high.",
    );
    twice_row.set_subtitle_lines(0);
    twice_row.set_activatable_widget(Some(&send_twice));
    let leaky_row = setting_row("Drop Old Frames in the Capture Queue", &leaky);
    leaky_row.set_subtitle(
        "When encoding falls behind, skip to the newest picture instead of building up delay. Leave on.",
    );
    leaky_row.set_subtitle_lines(0);
    leaky_row.set_activatable_widget(Some(&leaky));
    let advanced = vec![
        guided(
            "Scheduled Keyframes",
            &keyframe,
            "Leave on Only when needed: a full frame is sent when a viewer asks or a lost packet could not be replaced. Scheduled ones made viewers hitch.",
        ),
        guided(
            "Dropout Recovery Window",
            &retransmit,
            "How long sent audio and video is kept so it can be sent again after a network dropout. 2 s covers most; Off never resends.",
        ),
        guided(
            "Fastest Loss Recovery",
            &loss_keyframes,
            "Lost packets are sent again, which repairs the picture by itself. When a resend is lost too, a full frame repairs it; this is the least time between those. Shorter recovers sooner, but on a connection near its limit full frames cause more loss. 1 s is a good default; go longer if freezes come in clusters.",
        ),
        guided(
            "Full Frame at Least",
            &keyframe_gap,
            "Sends a full frame whenever none has gone out for this long, as a safety net for a viewer who is stuck and whose request never reaches you. Each full frame shows as a brief pulse of compression artifacts, so a regular one makes the picture pulse at that rhythm. Leave Off unless a viewer keeps freezing until they turn your video off and on.",
        ),
        twice_row,
        drop_row,
        leaky_row,
    ];
    QualityControls {
        rows,
        advanced,
        show,
    }
}

/// Where a stream's sound can come from, in the order of
/// `CapturePreferences::share_audio`.
const AUDIO_MODES: [&str; 4] = [
    "No Sound",
    "Desktop Audio",
    "One Application",
    "Patchbay Node",
];

/// What each of [`AUDIO_MODES`] does.
const AUDIO_MODE_HELP: [&str; 4] = [
    "The stream is silent.",
    "Everything playing to your default output, except kubecord itself, so viewers don't hear the call back.",
    "Only the program chosen below, wherever it plays. Programs are listed while they have sound open; Refresh looks again.",
    "Adds an output named kubecord Go Live while you stream. Connect anything to it in your patchbay, or choose it as a program's output; viewers hear what reaches it.",
];

/// The Shared Audio Boost slider's range, as a percentage of the captured
/// loudness. 100 % sends the sound unchanged; 500 % is a five-times boost.
const STREAM_AUDIO_BOOST_MIN: u32 = 100;
const STREAM_AUDIO_BOOST_MAX: u32 = 500;

/// Keeps the program chosen for one-program sound, for this stream and
/// the next.
fn remember_program(
    app: &Rc<AppState>,
    prefs: &Rc<RefCell<CapturePreferences>>,
    chosen: &AudioApp,
) {
    // A program listed while silent carries a note in its name.
    let name = chosen
        .name
        .strip_suffix(" (no sound right now)")
        .unwrap_or(&chosen.name)
        .to_owned();
    let key = chosen.key.clone();
    {
        let mut prefs = prefs.borrow_mut();
        prefs.share_audio_app = Some(key.clone());
        prefs.share_audio_app_name = Some(name.clone());
    }
    app.update_prefs(move |prefs| {
        prefs.share_audio_app = Some(key.clone());
        prefs.share_audio_app_name = Some(name.clone());
    });
}

pub(crate) fn setting_row(title: &str, widget: &impl IsA<gtk::Widget>) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    widget.set_valign(gtk::Align::Center);
    row.add_suffix(widget);
    row
}

/// What gives way first when both bitrate and framerate are automatic, in
/// the order of `CapturePreferences::adapt_priority`.
const ADAPT_PRIORITIES: [&str; 3] = ["Balanced", "Prefer Bitrate", "Prefer Framerate"];

/// A setting with a target and a low and a high end, chosen side by side
/// under its title and explanation.
fn range_row(
    title: &str,
    aim: &gtk::DropDown,
    lowest: &gtk::DropDown,
    highest: &gtk::DropDown,
    guide: &str,
) -> adw::PreferencesRow {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
    column.set_margin_top(8);
    column.set_margin_bottom(10);
    column.set_margin_start(12);
    column.set_margin_end(12);
    let heading = gtk::Label::new(Some(title));
    heading.set_xalign(0.0);
    column.append(&heading);
    let explanation = gtk::Label::new(Some(guide));
    explanation.set_xalign(0.0);
    explanation.set_wrap(true);
    explanation.add_css_class("dim-label");
    explanation.add_css_class("caption");
    column.append(&explanation);
    let ends = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    ends.set_homogeneous(true);
    ends.set_margin_top(6);
    for (name, dropdown) in [("Target", aim), ("Lowest", lowest), ("Highest", highest)] {
        let end = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let label = gtk::Label::new(Some(name));
        label.set_xalign(0.0);
        label.add_css_class("caption");
        end.append(&label);
        dropdown.set_hexpand(true);
        end.append(dropdown);
        ends.append(&end);
    }
    column.append(&ends);
    let row = adw::PreferencesRow::new();
    row.set_title(title);
    row.set_activatable(false);
    row.set_child(Some(&column));
    row
}

pub(crate) fn string_dropdown(names: &[String]) -> gtk::DropDown {
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    gtk::DropDown::from_strings(&refs)
}

fn position<T: PartialEq>(values: &[T], value: &T, fallback: usize) -> u32 {
    values
        .iter()
        .position(|candidate| candidate == value)
        .unwrap_or(fallback) as u32
}

// -- the popouts -------------------------------------------------------------

/// The parts both popouts share.
struct Popout {
    dialog: adw::Dialog,
    page: gtk::Box,
    action: gtk::Button,
}

fn popout(title: &str, action: &str) -> Popout {
    let dialog = adw::Dialog::new();
    dialog.set_title(title);
    dialog.set_content_width(480);
    dialog.set_content_height(680);
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.set_margin_top(6);
    page.set_margin_bottom(16);
    page.set_margin_start(18);
    page.set_margin_end(18);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);
    scroll.set_child(Some(&page));
    crate::settings_panel::dev_scroll(&scroll);

    let action = gtk::Button::with_label(action);
    action.add_css_class("suggested-action");
    action.add_css_class("pill");
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    footer.set_halign(gtk::Align::End);
    footer.set_margin_top(10);
    footer.set_margin_bottom(12);
    footer.set_margin_end(18);
    footer.append(&action);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&scroll));
    view.add_bottom_bar(&footer);
    view.set_bottom_bar_style(adw::ToolbarStyle::Raised);
    dialog.set_child(Some(&view));
    Popout {
        dialog,
        page,
        action,
    }
}

/// The quality controls in two expanders, the one-time note, and a way to
/// Settings.
fn one_time_settings(
    app: &Rc<AppState>,
    dialog: &adw::Dialog,
    controls: QualityControls,
    title: &str,
    expanded: bool,
) -> gtk::Box {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let group = adw::PreferencesGroup::new();
    let quality = adw::ExpanderRow::new();
    quality.set_title(title);
    quality.set_expanded(expanded);
    for row in &controls.rows {
        quality.add_row(row);
    }
    group.add(&quality);
    let advanced = adw::ExpanderRow::new();
    advanced.set_title("Advanced");
    advanced.set_subtitle("Keyframes, dropout recovery and frame dropping");
    for row in &controls.advanced {
        advanced.add_row(row);
    }
    group.add(&advanced);
    column.append(&group);

    let note = gtk::Label::new(None);
    note.set_markup(ONE_TIME_NOTE);
    note.set_xalign(0.0);
    note.set_wrap(true);
    note.add_css_class("dim-label");
    note.add_css_class("caption");
    let open = gtk::Button::with_label("Open Settings");
    open.add_css_class("flat");
    open.set_halign(gtk::Align::Start);
    let app = app.clone();
    let parent = dialog.clone();
    open.connect_clicked(move |_| {
        parent.close();
        app.show_settings_page("voice");
    });
    column.append(&note);
    column.append(&open);
    column
}

fn caption(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("heading");
    label
}

impl AppState {
    /// "Ready to video chat?": an optional preview, the camera, and one-time
    /// quality settings. Nothing is sent until Turn On Camera.
    pub(crate) fn show_camera_popup(self: &Rc<Self>, account: AccountId, access: CameraAccess) {
        let Popout {
            dialog,
            page,
            action,
        } = popout("Ready to video chat?", "Turn On Camera");
        let prefs = Rc::new(RefCell::new(self.capture.borrow().clone()));
        let (preview, frame) = Preview::new("Preview", 230);
        page.append(&frame);

        let names: Vec<String> = access
            .cameras
            .iter()
            .map(|camera| camera.name.clone())
            .collect();
        let cameras = string_dropdown(&names);
        let saved = prefs.borrow().camera.clone();
        cameras.set_selected(names.iter().position(|name| *name == saved).unwrap_or(0) as u32);
        page.append(&caption("Camera"));
        page.append(&cameras);

        let always = gtk::CheckButton::with_label("Always preview video");
        always.set_active(prefs.borrow().always_preview);
        page.append(&always);

        // Starts (or restarts) the preview with the current choices.
        let start: Rc<dyn Fn()> = {
            let app = self.clone();
            let preview = preview.clone();
            let access = access.clone();
            let cameras = cameras.clone();
            let prefs = prefs.clone();
            Rc::new(move || {
                let Some(camera) = access.cameras.get(cameras.selected() as usize) else {
                    return;
                };
                let settings = prefs.borrow().video_settings();
                start_camera_preview(
                    &app,
                    &preview,
                    Some(access.clone()),
                    CameraPick::Node(camera.node_id),
                    settings,
                );
            })
        };
        let restart = {
            let preview = preview.clone();
            let start = start.clone();
            move || {
                if preview.running() {
                    start();
                }
            }
        };

        let apply: Apply = {
            let prefs = prefs.clone();
            let restart = restart.clone();
            Rc::new(move |change| {
                change(&mut prefs.borrow_mut());
                restart();
            })
        };
        let controls = quality_controls(&prefs.borrow().clone(), false, apply);
        page.append(&one_time_settings(
            self,
            &dialog,
            controls,
            "Video Quality",
            false,
        ));

        let target = preview.clone();
        let begin = start.clone();
        preview.button.connect_clicked(move |_| {
            if target.running() {
                target.stop();
            } else {
                begin();
            }
        });
        let again = restart.clone();
        cameras.connect_selected_notify(move |_| again());
        let app = self.clone();
        let begin = start.clone();
        always.connect_toggled(move |check| {
            let active = check.is_active();
            app.update_prefs(|prefs| prefs.always_preview = active);
            if active {
                begin();
            }
        });

        let committed = Rc::new(Cell::new(false));
        let app = self.clone();
        let parent = dialog.clone();
        let done = committed.clone();
        let target = preview.clone();
        action.connect_clicked(move |_| {
            let Some(camera) = access.cameras.get(cameras.selected() as usize) else {
                return;
            };
            done.set(true);
            // Release the camera before the call opens it.
            target.stop();
            let settings = prefs.borrow().video_settings();
            let _ = app.cmd_tx.try_send(Command::SetCameraDevice {
                account,
                access: access.clone(),
                node_id: camera.node_id,
                settings,
            });
            app.set_status("Turning on camera…");
            parent.close();
        });

        let app = self.clone();
        let target = preview.clone();
        dialog.connect_closed(move |_| {
            target.stop();
            if !committed.get() {
                app.reset_camera_ui(CameraPurpose::Camera);
            }
        });
        dialog.present(Some(&self.window));
        if self.capture.borrow().always_preview {
            start();
        }
    }

    /// The Go Live popout: what to share, an optional preview, and one-time
    /// stream settings. A previewed screen goes live without asking again.
    /// Where the stream's sound comes from: a choice of none, all desktop
    /// sound, one program, or a node for a patchbay. With one program, a
    /// list of programs that have sound and a button to look again. The
    /// choice is kept for next time.
    fn stream_audio_controls(
        self: &Rc<Self>,
        account: AccountId,
        prefs: Rc<RefCell<CapturePreferences>>,
    ) -> gtk::Box {
        let group = gtk::Box::new(gtk::Orientation::Vertical, 6);
        group.append(&caption("Stream Audio"));
        let saved_gain = prefs.borrow().stream_audio_gain;
        let modes: Vec<String> = AUDIO_MODES.iter().map(|name| (*name).to_owned()).collect();
        let mode = string_dropdown(&modes);
        mode.set_selected(prefs.borrow().share_audio.min(AUDIO_MODES.len() as u32 - 1));
        group.append(&mode);
        let help = gtk::Label::new(Some(AUDIO_MODE_HELP[mode.selected() as usize]));
        help.set_xalign(0.0);
        help.set_wrap(true);
        help.add_css_class("dim-label");
        help.add_css_class("caption");
        group.append(&help);

        // The program, for one-program sound.
        let program = gtk::Box::new(gtk::Orientation::Vertical, 6);
        program.set_margin_top(6);
        program.append(&caption("Application"));
        let picker = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let list = gtk::DropDown::from_strings(&["Looking…"]);
        list.set_hexpand(true);
        list.set_sensitive(false);
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.set_tooltip_text(Some("Look again for programs with sound"));
        picker.append(&list);
        picker.append(&refresh);
        program.append(&picker);
        program.set_visible(mode.selected() == 2);
        group.append(&program);

        // The programs in the list, in its order.
        let shown: Rc<RefCell<Vec<AudioApp>>> = Rc::default();
        // Set while the list is being refilled, which also selects.
        let filling = Rc::new(Cell::new(false));

        let fill: Rc<dyn Fn()> = {
            let app = self.clone();
            let list = list.clone();
            let shown = shown.clone();
            let filling = filling.clone();
            let prefs = prefs.clone();
            Rc::new(move || {
                list.set_sensitive(false);
                let list = list.clone();
                let shown = shown.clone();
                let filling = filling.clone();
                let prefs = prefs.clone();
                spawn_local(
                    &app,
                    async {
                        tokio::task::spawn_blocking(kc_media::share_audio::applications)
                            .await
                            .unwrap_or_else(|error| {
                                Err(kc_media::error::Error::Message(error.to_string()))
                            })
                    },
                    move |app, found| {
                        let (mut apps, problem) = match found {
                            Ok(apps) => (apps, None),
                            Err(error) => (Vec::new(), Some(error.to_string())),
                        };
                        // The program chosen before stays choosable while it
                        // has no sound open; it is connected once it does.
                        let (saved, saved_name) = {
                            let prefs = prefs.borrow();
                            (
                                prefs.share_audio_app.clone(),
                                prefs.share_audio_app_name.clone(),
                            )
                        };
                        if let Some(key) = &saved
                            && !apps.iter().any(|candidate| &candidate.key == key)
                        {
                            apps.insert(
                                0,
                                AudioApp {
                                    key: key.clone(),
                                    name: format!(
                                        "{} (no sound right now)",
                                        saved_name.unwrap_or_else(|| key.clone())
                                    ),
                                },
                            );
                        }
                        filling.set(true);
                        if apps.is_empty() {
                            let text = match &problem {
                                Some(_) => "Could not list programs",
                                None => "No programs have sound open",
                            };
                            list.set_model(Some(&gtk::StringList::new(&[text])));
                            list.set_tooltip_text(problem.as_deref());
                        } else {
                            let names: Vec<&str> = apps
                                .iter()
                                .map(|candidate| candidate.name.as_str())
                                .collect();
                            list.set_model(Some(&gtk::StringList::new(&names)));
                            list.set_tooltip_text(None);
                            let index = saved
                                .and_then(|key| {
                                    apps.iter().position(|candidate| candidate.key == key)
                                })
                                .unwrap_or(0);
                            list.set_selected(index as u32);
                            list.set_sensitive(true);
                        }
                        filling.set(false);
                        if let Some(problem) = problem {
                            tracing::warn!(%problem, "could not list programs with sound");
                        }
                        let first = apps.get(list.selected() as usize).cloned();
                        *shown.borrow_mut() = apps;
                        // With nothing chosen before, the first one is.
                        if prefs.borrow().share_audio_app.is_none()
                            && let Some(first) = first
                        {
                            remember_program(app, &prefs, &first);
                        }
                    },
                );
            })
        };

        {
            let app = self.clone();
            let shown = shown.clone();
            let prefs = prefs.clone();
            list.connect_selected_notify(move |list| {
                if filling.get() {
                    return;
                }
                if let Some(chosen) = shown.borrow().get(list.selected() as usize) {
                    remember_program(&app, &prefs, chosen);
                }
            });
        }
        {
            let fill = fill.clone();
            refresh.connect_clicked(move |_| fill());
        }
        {
            let app = self.clone();
            let program = program.clone();
            let fill = fill.clone();
            let listed = Cell::new(false);
            mode.connect_selected_notify(move |row| {
                let chosen = row.selected();
                help.set_label(AUDIO_MODE_HELP[chosen as usize]);
                prefs.borrow_mut().share_audio = chosen;
                app.update_prefs(|prefs| prefs.share_audio = chosen);
                program.set_visible(chosen == 2);
                if chosen == 2 && !listed.replace(true) {
                    fill();
                }
            });
        }
        if mode.selected() == 2 {
            fill();
        }

        // Boost on the user's own outgoing stream sound. 100 % sends it as
        // captured; higher makes only viewers hear it louder.
        let boost_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        boost_box.set_margin_top(10);
        boost_box.append(&caption("Shared Audio Boost"));
        let boost = gtk::Scale::with_range(
            gtk::Orientation::Horizontal,
            f64::from(STREAM_AUDIO_BOOST_MIN),
            f64::from(STREAM_AUDIO_BOOST_MAX),
            5.0,
        );
        let start_percent = (f64::from(saved_gain) * 100.0).clamp(
            f64::from(STREAM_AUDIO_BOOST_MIN),
            f64::from(STREAM_AUDIO_BOOST_MAX),
        );
        boost.set_value(start_percent);
        boost.add_mark(100.0, gtk::PositionType::Bottom, Some("100%"));
        boost.set_draw_value(true);
        boost.set_value_pos(gtk::PositionType::Right);
        boost.set_format_value_func(|_, value| format!("{value:.0}%"));
        boost.set_hexpand(true);
        boost_box.append(&boost);
        let boost_help = gtk::Label::new(Some(
            "Makes viewers hear the shared sound louder without changing your own volume. Too much will distort or clip the audio.",
        ));
        boost_help.set_xalign(0.0);
        boost_help.set_wrap(true);
        boost_help.add_css_class("dim-label");
        boost_help.add_css_class("caption");
        boost_box.append(&boost_help);
        group.append(&boost_box);
        {
            let app = self.clone();
            boost.connect_value_changed(move |scale| {
                let gain = (scale.value() / 100.0) as f32;
                // Persist for the next stream, and apply live to one running.
                app.update_prefs(|prefs| prefs.stream_audio_gain = gain);
                let _ = app
                    .cmd_tx
                    .try_send(Command::SetStreamAudioGain { account, gain });
            });
        }

        group
    }

    pub(crate) fn show_share_popup(self: &Rc<Self>, account: AccountId) {
        let Popout {
            dialog,
            page,
            action,
        } = popout("Share Your Screen", "Go Live");
        let prefs = Rc::new(RefCell::new(self.capture.borrow().clone()));
        let (preview, frame) = Preview::new("Choose & Preview", 230);
        page.append(&frame);

        let mut source_names: Vec<String> = SHARE_SOURCES
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        source_names.push("Camera".to_owned());
        let source = string_dropdown(&source_names);
        source.set_selected(prefs.borrow().share_source.min(CAMERA_SOURCE - 1));
        page.append(&caption("Share"));
        page.append(&source);

        // Cameras, listed when Camera is first chosen.
        let camera_list = gtk::DropDown::from_strings(&["Loading cameras…"]);
        camera_list.set_visible(false);
        page.append(&camera_list);
        let camera_access: Rc<RefCell<Option<CameraAccess>>> = Rc::new(RefCell::new(None));

        page.append(&self.stream_audio_controls(account, prefs.clone()));

        let selected_camera = {
            let access = camera_access.clone();
            let list = camera_list.clone();
            move || -> Option<(CameraAccess, u32)> {
                let access = access.borrow().clone()?;
                let node_id = access.cameras.get(list.selected() as usize)?.node_id;
                Some((access, node_id))
            }
        };

        // Starts the preview: the camera, or the screen picker.
        let start: Rc<dyn Fn()> = {
            let app = self.clone();
            let preview = preview.clone();
            let source = source.clone();
            let prefs = prefs.clone();
            let selected_camera = selected_camera.clone();
            Rc::new(move || {
                let settings = prefs.borrow().video_settings();
                if source.selected() == CAMERA_SOURCE {
                    let Some((access, node_id)) = selected_camera() else {
                        return;
                    };
                    start_camera_preview(
                        &app,
                        &preview,
                        Some(access),
                        CameraPick::Node(node_id),
                        settings,
                    );
                } else {
                    start_screen_preview(
                        &app,
                        &preview,
                        ShareSources::from_index(source.selected()),
                        settings,
                    );
                }
            })
        };
        // A quality change restarts a camera preview. A screen preview stops
        // instead: its capture cannot be reopened without the picker, and it
        // must not go live with the old settings.
        let restart = {
            let preview = preview.clone();
            let start = start.clone();
            let source = source.clone();
            move || {
                if !preview.running() {
                    return;
                }
                if source.selected() == CAMERA_SOURCE {
                    start();
                } else {
                    preview.stop();
                }
            }
        };
        let apply: Apply = {
            let prefs = prefs.clone();
            let restart = restart.clone();
            Rc::new(move |change| {
                change(&mut prefs.borrow_mut());
                restart();
            })
        };
        let controls = quality_controls(&prefs.borrow().clone(), true, apply);
        page.append(&one_time_settings(
            self,
            &dialog,
            controls,
            "Stream Quality",
            true,
        ));

        let target = preview.clone();
        let begin = start.clone();
        preview.button.connect_clicked(move |_| {
            if target.running() {
                target.stop();
            } else {
                begin();
            }
        });

        let app = self.clone();
        let target = preview.clone();
        let list = camera_list.clone();
        let access_slot = camera_access.clone();
        source.connect_selected_notify(move |source| {
            // A different kind of source means a new pick.
            target.stop();
            let camera = source.selected() == CAMERA_SOURCE;
            list.set_visible(camera);
            target.set_idle_label(if camera {
                "Preview"
            } else {
                "Choose & Preview"
            });
            if camera && access_slot.borrow().is_none() {
                let list = list.clone();
                let access_slot = access_slot.clone();
                spawn_local(
                    &app,
                    kc_media::portal::request_cameras(),
                    move |app, result| {
                        let access = match result {
                            Ok(Some(access)) if !access.cameras.is_empty() => access,
                            Ok(_) => {
                                list.set_model(Some(&gtk::StringList::new(&[
                                    "No camera available",
                                ])));
                                return;
                            }
                            Err(error) => {
                                list.set_model(Some(&gtk::StringList::new(&[
                                    "No camera available",
                                ])));
                                app.set_status(&format!("Camera: {error}"));
                                return;
                            }
                        };
                        let names: Vec<&str> = access
                            .cameras
                            .iter()
                            .map(|camera| camera.name.as_str())
                            .collect();
                        list.set_model(Some(&gtk::StringList::new(&names)));
                        let saved = app.capture.borrow().camera.clone();
                        list.set_selected(
                            access
                                .cameras
                                .iter()
                                .position(|camera| camera.name == saved)
                                .unwrap_or(0) as u32,
                        );
                        *access_slot.borrow_mut() = Some(access);
                    },
                );
            }
        });
        let target = preview.clone();
        camera_list.connect_selected_notify(move |_| target.stop());

        let committed = Rc::new(Cell::new(false));
        let app = self.clone();
        let parent = dialog.clone();
        let done = committed.clone();
        let target = preview.clone();
        action.connect_clicked(move |_| {
            let settings = prefs.borrow().video_settings();
            let delay_ms = prefs.borrow().stream_delay_ms;
            let audio = prefs.borrow().audio_share();
            let stream_audio_gain = prefs.borrow().stream_audio_gain;
            if prefs.borrow().share_audio == 2 && audio.is_none() {
                app.set_status("Choose the program to share sound from");
                return;
            }
            // Carries the saved boost to the runtime so the stream starts at
            // it, even if the slider was not touched this time. Queued before
            // the start command, so it is read when the capture begins.
            let _ = app.cmd_tx.try_send(Command::SetStreamAudioGain {
                account,
                gain: stream_audio_gain,
            });
            if source.selected() == CAMERA_SOURCE {
                let Some((access, node_id)) = selected_camera() else {
                    app.set_status("No camera available");
                    return;
                };
                target.stop();
                let _ = app.cmd_tx.try_send(Command::StartStreamCamera {
                    account,
                    access,
                    node_id,
                    settings,
                    delay_ms,
                    audio,
                });
                app.set_status("Starting camera stream…");
            } else {
                // Go live with the capture being previewed, if there is one;
                // otherwise the session opens the picker.
                let capture = target.take_capture().map(Box::new);
                target.stop();
                let parent = app.window_identifier.borrow().clone();
                let _ = app.cmd_tx.try_send(Command::StartStream {
                    account,
                    parent,
                    settings,
                    delay_ms,
                    sources: ShareSources::from_index(source.selected()),
                    capture,
                    audio,
                });
                app.set_status("Starting screen share…");
            }
            done.set(true);
            parent.close();
        });

        let app = self.clone();
        dialog.connect_closed(move |_| {
            preview.stop();
            if !committed.get() {
                app.reset_stream_ui();
            }
        });
        dialog.present(Some(&self.window));
    }
}
