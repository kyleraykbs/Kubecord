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
use kc_media::video::{
    EncodedFrame, FrameTap, PreparedCapture, VideoSettings, VideoSource, is_keyframe,
};

use crate::live::LiveVideo;
use crate::session::{AccountId, CameraPurpose, Command};
use crate::settings::{
    BITRATES, CapturePreferences, DROP_AFTER_MS, FRAMERATES, KEYFRAME_INTERVALS, RESOLUTIONS,
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
    pub rows: Vec<adw::ActionRow>,
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
    let framerate_names: Vec<String> = FRAMERATES.iter().map(|fps| format!("{fps} fps")).collect();
    let framerate = string_dropdown(&framerate_names);
    let mut bitrate_names = vec!["Automatic".to_owned()];
    bitrate_names.extend(BITRATES.iter().map(|kbps| format!("{kbps} kbps")));
    let bitrate = string_dropdown(&bitrate_names);
    let delay = gtk::DropDown::from_strings(&["None", "0.5 s", "1 s", "2 s", "3 s"]);
    let cpu_encoder = gtk::Switch::new();

    let keyframe_names: Vec<String> = KEYFRAME_INTERVALS
        .iter()
        .map(|seconds| format!("{seconds} s"))
        .collect();
    let keyframe = string_dropdown(&keyframe_names);
    keyframe.set_tooltip_text(Some(
        "How often a full frame is sent unprompted. Viewers that lose the picture request one themselves, so longer intervals give a steadier stream; shorter ones recover sooner if a request is missed.",
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
    let leaky = gtk::Switch::new();
    leaky.set_tooltip_text(Some(
        "Leaky queue: when the encoder is slow, discard old frames instead of letting latency build.",
    ));

    let show: Rc<dyn Fn(&CapturePreferences)> = {
        let resolution = resolution.clone();
        let framerate = framerate.clone();
        let bitrate = bitrate.clone();
        let delay = delay.clone();
        let cpu_encoder = cpu_encoder.clone();
        let keyframe = keyframe.clone();
        let retransmit = retransmit.clone();
        let drop_late = drop_late.clone();
        let leaky = leaky.clone();
        Rc::new(move |prefs: &CapturePreferences| {
            resolution.set_selected(
                RESOLUTIONS
                    .iter()
                    .position(|(width, height, _)| *width == prefs.width && *height == prefs.height)
                    .unwrap_or(1) as u32,
            );
            framerate.set_selected(position(&FRAMERATES, &prefs.fps, 1));
            bitrate.set_selected(
                prefs
                    .bitrate_kbps
                    .and_then(|kbps| BITRATES.iter().position(|value| *value == kbps))
                    .map(|index| index + 1)
                    .unwrap_or(0) as u32,
            );
            delay.set_selected(position(&STREAM_DELAYS, &prefs.stream_delay_ms, 0));
            cpu_encoder.set_active(prefs.cpu_encoder);
            keyframe.set_selected(position(&KEYFRAME_INTERVALS, &prefs.keyframe_secs, 0));
            retransmit.set_selected(position(&RETRANSMIT_WINDOWS_MS, &prefs.retransmit_ms, 2));
            drop_late.set_selected(position(&DROP_AFTER_MS, &prefs.drop_frames_after_ms, 2));
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
    let target = apply.clone();
    framerate.connect_selected_notify(move |dropdown| {
        if let Some(fps) = FRAMERATES.get(dropdown.selected() as usize).copied() {
            target(&move |prefs| prefs.fps = fps);
        }
    });
    let target = apply.clone();
    bitrate.connect_selected_notify(move |dropdown| {
        let kbps = match dropdown.selected() as usize {
            0 => None,
            index => BITRATES.get(index - 1).copied(),
        };
        target(&move |prefs| prefs.bitrate_kbps = kbps);
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
    let target = apply;
    leaky.connect_active_notify(move |switch| {
        let active = switch.is_active();
        target(&move |prefs| prefs.leaky_queue = active);
    });

    let mut rows = vec![
        setting_row("Resolution", &resolution),
        setting_row("Framerate", &framerate),
        setting_row("Bitrate", &bitrate),
    ];
    if stream {
        rows.push(setting_row("Go Live Delay", &delay));
    }
    let encoder_row = setting_row("Use Software Encoder (x264)", &cpu_encoder);
    encoder_row.set_subtitle("Hardware encoding is used when available otherwise");
    encoder_row.set_activatable_widget(Some(&cpu_encoder));
    rows.push(encoder_row);

    let drop_row = setting_row("Drop Late Frames After", &drop_late);
    drop_row.set_subtitle("Applies beyond the Go Live delay");
    let leaky_row = setting_row("Drop Old Frames in the Capture Queue", &leaky);
    leaky_row.set_activatable_widget(Some(&leaky));
    let advanced = vec![
        setting_row("Keyframe Interval", &keyframe),
        setting_row("Dropout Recovery Window", &retransmit),
        drop_row,
        leaky_row,
    ];
    QualityControls {
        rows,
        advanced,
        show,
    }
}

pub(crate) fn setting_row(title: &str, widget: &impl IsA<gtk::Widget>) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    widget.set_valign(gtk::Align::Center);
    row.add_suffix(widget);
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

        let audio = gtk::Switch::new();
        audio.set_active(true);
        let audio_row = setting_row("Share System Audio", &audio);
        audio_row.set_activatable_widget(Some(&audio));
        let audio_group = adw::PreferencesGroup::new();
        audio_group.add(&audio_row);
        page.append(&audio_group);

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
            let audio = audio.is_active();
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
                let capture = target.take_capture();
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
