//! The settings panel: categories on the left, the chosen page on the right.
//! Changes apply as they are made, as in Discord; there is nothing to save.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_http::cdn;
use kc_media::audio::MicTest;
use kc_media::decode::VideoFeed;
use kc_media::portal::CameraAccess;
use kc_media::video::{VideoSettings, VideoSource, is_keyframe};

use crate::live::LiveVideo;
use crate::settings::{
    BITRATES, CapturePreferences, DROP_AFTER_MS, FRAMERATES, KEYFRAME_INTERVALS, RESOLUTIONS,
    RETRANSMIT_WINDOWS_MS, SHARE_SOURCES, STREAM_DELAYS,
};
use crate::shell::AppState;

/// Longest device name shown in a dropdown before it is shortened.
const DEVICE_LABEL_CHARS: usize = 40;
/// Segments in the mic test's level meter.
const METER_BLOCKS: f64 = 40.0;

/// One of the two video test boxes: a picture with a button over it.
struct Preview {
    picture: gtk::Picture,
    button: gtk::Button,
    idle_label: &'static str,
    live: RefCell<Option<LiveVideo>>,
    source: RefCell<Option<VideoSource>>,
    /// Bumped by every start and stop, so a capture that finishes starting
    /// after its test was stopped is thrown away.
    generation: Cell<u64>,
}

impl Preview {
    fn new(idle_label: &'static str) -> (Rc<Self>, gtk::Overlay) {
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
        frame.set_size_request(-1, 180);
        frame.set_child(Some(&picture));
        frame.add_overlay(&button);
        let preview = Rc::new(Self {
            picture,
            button,
            idle_label,
            live: RefCell::new(None),
            source: RefCell::new(None),
            generation: Cell::new(0),
        });
        (preview, frame)
    }

    fn running(&self) -> bool {
        self.live.borrow().is_some()
    }

    /// Starts showing a new feed; the capture is attached when it is up.
    fn begin(&self) -> Option<(Arc<VideoFeed>, u64)> {
        let feed = Arc::new(VideoFeed::default());
        let live = LiveVideo::start(feed.clone())?;
        self.picture.set_paintable(Some(&live.paintable));
        *self.live.borrow_mut() = Some(live);
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.button.set_label("Stop Test");
        self.button.set_valign(gtk::Align::End);
        self.button.set_margin_bottom(10);
        Some((feed, generation))
    }

    fn attach(&self, generation: u64, source: VideoSource) {
        // A stale capture is dropped here, which stops it.
        if self.generation.get() == generation && self.running() {
            *self.source.borrow_mut() = Some(source);
        }
    }

    fn stop(&self) {
        self.generation.set(self.generation.get() + 1);
        self.source.borrow_mut().take();
        self.live.borrow_mut().take();
        self.picture.set_paintable(None::<&gtk::gdk::Paintable>);
        self.button.set_label(self.idle_label);
        self.button.set_valign(gtk::Align::Center);
        self.button.set_margin_bottom(0);
    }
}

/// What the panel keeps running while it is open.
struct Panel {
    mic_test: RefCell<Option<MicTest>>,
    camera: Rc<Preview>,
    screen: Rc<Preview>,
    /// Camera access from the portal, once asked for.
    cameras: RefCell<Option<CameraAccess>>,
    cameras_requested: Cell<bool>,
    /// Set while a dropdown is refilled, so its handler stays quiet.
    refilling: Cell<bool>,
}

impl Panel {
    fn stop_all(&self) {
        self.mic_test.borrow_mut().take();
        self.camera.stop();
        self.screen.stop();
    }
}

impl AppState {
    /// Changes the preferences and applies them at once; the file is written
    /// shortly after, so dragging a slider does not write on every step.
    pub(crate) fn update_prefs(self: &Rc<Self>, change: impl FnOnce(&mut CapturePreferences)) {
        change(&mut self.capture.borrow_mut());
        self.capture.borrow().apply_devices();
        thread_local! {
            static SAVE_PENDING: Cell<bool> = const { Cell::new(false) };
        }
        if SAVE_PENDING.replace(true) {
            return;
        }
        let app = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(400), move || {
            SAVE_PENDING.set(false);
            if let Err(error) = app.capture.borrow().save() {
                app.set_status(&format!("Could not save settings: {error}"));
            }
        });
    }

    pub(crate) fn show_settings(self: &Rc<Self>) {
        self.show_settings_page("account");
    }

    /// Opens the panel on `page`: "account" or "voice".
    pub(crate) fn show_settings_page(self: &Rc<Self>, page: &str) {
        let dialog = adw::Dialog::new();
        dialog.set_title("Settings");
        dialog.set_content_width(960);
        dialog.set_content_height(700);

        let (camera, camera_frame) = Preview::new("Test Camera");
        let (screen, screen_frame) = Preview::new("Test Screenshare");
        let panel = Rc::new(Panel {
            mic_test: RefCell::new(None),
            camera,
            screen,
            cameras: RefCell::new(None),
            cameras_requested: Cell::new(false),
            refilling: Cell::new(false),
        });

        let pages = gtk::Stack::new();
        pages.set_transition_type(gtk::StackTransitionType::Crossfade);
        pages.add_named(&page_scroll(&self.account_page(&dialog)), Some("account"));
        let camera_dropdown = gtk::DropDown::from_strings(&["Ask Every Time"]);
        pages.add_named(
            &page_scroll(&self.voice_video_page(
                &panel,
                &camera_frame,
                &screen_frame,
                &camera_dropdown,
            )),
            Some("voice"),
        );

        let content_view = adw::ToolbarView::new();
        content_view.add_top_bar(&adw::HeaderBar::new());
        content_view.set_content(Some(&pages));
        let content_page = adw::NavigationPage::new(&content_view, "My Account");

        // Categories, under Discord's two headings.
        let categories = gtk::ListBox::new();
        categories.add_css_class("navigation-sidebar");
        let entries: [(&str, &str, &str); 2] = [
            ("account", "My Account", "avatar-default-symbolic"),
            ("voice", "Voice & Video", "audio-input-microphone-symbolic"),
        ];
        for (_, title, icon) in entries {
            let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            row_box.set_margin_top(6);
            row_box.set_margin_bottom(6);
            row_box.set_margin_start(4);
            row_box.append(&gtk::Image::from_icon_name(icon));
            let text = gtk::Label::new(Some(title));
            text.set_xalign(0.0);
            row_box.append(&text);
            categories.append(&row_box);
        }
        categories.set_header_func(|row, _| {
            let heading = match row.index() {
                0 => "User Settings",
                1 => "App Settings",
                _ => {
                    row.set_header(None::<&gtk::Widget>);
                    return;
                }
            };
            let header = gtk::Label::new(Some(heading));
            header.set_xalign(0.0);
            header.add_css_class("caption-heading");
            header.add_css_class("dim-label");
            header.set_margin_start(12);
            header.set_margin_top(if row.index() == 0 { 6 } else { 14 });
            header.set_margin_bottom(4);
            row.set_header(Some(&header));
        });

        let split = adw::NavigationSplitView::new();
        let sidebar_view = adw::ToolbarView::new();
        sidebar_view.add_top_bar(&adw::HeaderBar::new());
        sidebar_view.set_content(Some(&categories));
        let sidebar_page = adw::NavigationPage::new(&sidebar_view, "Settings");
        split.set_sidebar(Some(&sidebar_page));
        split.set_content(Some(&content_page));
        split.set_min_sidebar_width(200.0);
        split.set_max_sidebar_width(240.0);
        split.set_show_content(true);

        let app = self.clone();
        let opened = panel.clone();
        let target_split = split.clone();
        let dropdown = camera_dropdown.clone();
        categories.connect_row_selected(move |_, row| {
            let Some(row) = row else {
                return;
            };
            let Some((name, title, _)) = entries.get(row.index() as usize) else {
                return;
            };
            pages.set_visible_child_name(name);
            content_page.set_title(title);
            target_split.set_show_content(true);
            if *name == "voice" && !opened.cameras_requested.replace(true) {
                app.load_cameras(&opened, &dropdown);
            }
        });
        let initial = entries
            .iter()
            .position(|(name, _, _)| *name == page)
            .unwrap_or(0);
        if let Some(row) = categories.row_at_index(initial as i32) {
            categories.select_row(Some(&row));
            // Start with focus on the categories: focus in the page would
            // scroll it away from the top.
            dialog.set_focus(Some(&row));
        }

        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            600.0,
            adw::LengthUnit::Sp,
        ));
        breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
        dialog.add_breakpoint(breakpoint);
        dialog.set_child(Some(&split));

        let running = panel.clone();
        dialog.connect_closed(move |_| running.stop_all());
        dialog.present(Some(&self.window));
    }

    // -- my account ----------------------------------------------------------

    fn account_page(self: &Rc<Self>, dialog: &adw::Dialog) -> gtk::Box {
        let page = page_box();
        let user = self
            .with_session(|session| {
                session
                    .store
                    .read(|state| state.me.clone())
                    .or_else(|| session.user.clone())
            })
            .flatten();
        let Some(user) = user else {
            page.append(&gtk::Label::new(Some("Not signed in")));
            return page;
        };

        let name = user.display_name().to_owned();
        let avatar = adw::Avatar::new(80, Some(&name), true);
        self.images.set_avatar(
            &avatar,
            &cdn::user_avatar(user.id, user.avatar.as_deref(), 128),
        );
        let display = gtk::Label::new(Some(&name));
        display.set_xalign(0.0);
        display.add_css_class("title-2");
        let handle = gtk::Label::new(Some(&format!("@{}", user.username)));
        handle.set_xalign(0.0);
        handle.add_css_class("dim-label");
        let names = gtk::Box::new(gtk::Orientation::Vertical, 2);
        names.set_valign(gtk::Align::Center);
        names.set_hexpand(true);
        names.append(&display);
        names.append(&handle);
        let edit = gtk::Button::with_label("Edit User Profile");
        edit.add_css_class("suggested-action");
        edit.set_valign(gtk::Align::Center);
        let app = self.clone();
        let parent = dialog.clone();
        edit.connect_clicked(move |_| {
            parent.close();
            app.edit_profile_dialog();
        });
        let banner = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        banner.add_css_class("card");
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        inner.set_margin_top(16);
        inner.set_margin_bottom(16);
        inner.set_margin_start(16);
        inner.set_margin_end(16);
        inner.set_hexpand(true);
        inner.append(&avatar);
        inner.append(&names);
        inner.append(&edit);
        banner.append(&inner);
        page.append(&section_title("Account Info"));
        page.append(&banner);

        let info = adw::PreferencesGroup::new();
        let row = adw::ActionRow::builder()
            .title("Display Name")
            .subtitle(name.as_str())
            .build();
        info.add(&row);
        let row = adw::ActionRow::builder()
            .title("Username")
            .subtitle(user.username.as_str())
            .build();
        info.add(&row);
        info.add(&revealable_row(
            "Email",
            user.email.as_deref(),
            mask_email,
            "You haven't added an email address.",
        ));
        info.add(&revealable_row(
            "Phone Number",
            user.phone.as_deref(),
            mask_phone,
            "You haven't added a phone number.",
        ));
        page.append(&info);

        let security = adw::PreferencesGroup::new();
        security.set_title("Password &amp; Security");
        let mfa = adw::ActionRow::builder()
            .title("Multi-Factor Authentication")
            .subtitle(match user.mfa_enabled {
                Some(true) => "Enabled",
                Some(false) => "Not enabled",
                None => "Unknown",
            })
            .build();
        security.add(&mfa);
        page.append(&security);

        let note = gtk::Label::new(Some(
            "Email, phone and password are changed in the official Discord app or website.",
        ));
        note.set_xalign(0.0);
        note.set_wrap(true);
        note.add_css_class("dim-label");
        note.add_css_class("caption");
        page.append(&note);
        page
    }

    // -- voice & video -------------------------------------------------------

    fn voice_video_page(
        self: &Rc<Self>,
        panel: &Rc<Panel>,
        camera_frame: &gtk::Overlay,
        screen_frame: &gtk::Overlay,
        camera_dropdown: &gtk::DropDown,
    ) -> gtk::Box {
        let page = page_box();
        let current = self.capture.borrow().clone();

        // Voice: devices side by side, volumes under them, then the test.
        page.append(&section_title("Voice Settings"));
        let inputs = kc_media::devices::inputs();
        let outputs = kc_media::devices::outputs();
        let (input_names, input_dd) = device_dropdown(
            inputs
                .iter()
                .map(|device| (device.name.clone(), device.label.clone())),
            &current.input_device,
        );
        let (output_names, output_dd) = device_dropdown(
            outputs
                .iter()
                .map(|device| (device.name.clone(), device.label.clone())),
            &current.output_device,
        );
        let input_volume = volume_scale(current.input_volume);
        let output_volume = volume_scale(current.output_volume);

        let grid = gtk::Grid::new();
        grid.set_column_homogeneous(true);
        grid.set_column_spacing(20);
        grid.set_row_spacing(6);
        grid.attach(&field_label("Microphone"), 0, 0, 1, 1);
        grid.attach(&field_label("Speaker"), 1, 0, 1, 1);
        grid.attach(&input_dd, 0, 1, 1, 1);
        grid.attach(&output_dd, 1, 1, 1, 1);
        let volume_heading = field_label("Microphone Volume");
        volume_heading.set_margin_top(12);
        grid.attach(&volume_heading, 0, 2, 1, 1);
        let volume_heading = field_label("Speaker Volume");
        volume_heading.set_margin_top(12);
        grid.attach(&volume_heading, 1, 2, 1, 1);
        grid.attach(&input_volume, 0, 3, 1, 1);
        grid.attach(&output_volume, 1, 3, 1, 1);
        page.append(&grid);

        let mic_test = gtk::Button::with_label("Mic Test");
        mic_test.add_css_class("suggested-action");
        mic_test.set_size_request(140, -1);
        let meter = gtk::LevelBar::new();
        meter.set_mode(gtk::LevelBarMode::Discrete);
        meter.set_min_value(0.0);
        meter.set_max_value(METER_BLOCKS);
        meter.set_hexpand(true);
        meter.set_valign(gtk::Align::Center);
        meter.add_css_class("kc-meter");
        // One colour for every block, like Discord's meter.
        for offset in [
            gtk::LEVEL_BAR_OFFSET_LOW,
            gtk::LEVEL_BAR_OFFSET_HIGH,
            gtk::LEVEL_BAR_OFFSET_FULL,
        ] {
            meter.remove_offset_value(Some(offset));
        }
        let test_row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        test_row.set_margin_top(14);
        test_row.append(&mic_test);
        test_row.append(&meter);
        page.append(&test_row);
        let voice_note = gtk::Label::new(Some(
            "Mic Test plays your microphone back to you. Device changes take effect the next time you join a call; volume changes apply right away.",
        ));
        voice_note.set_xalign(0.0);
        voice_note.set_wrap(true);
        voice_note.add_css_class("dim-label");
        voice_note.add_css_class("caption");
        page.append(&voice_note);

        let app = self.clone();
        let state = panel.clone();
        let names = input_names.clone();
        let button = mic_test.clone();
        input_dd.connect_selected_notify(move |dropdown| {
            let name = names
                .get(dropdown.selected() as usize)
                .cloned()
                .unwrap_or_default();
            app.update_prefs(|prefs| prefs.input_device = name);
            restart_mic_test(&app, &state, &button);
        });
        let app = self.clone();
        let state = panel.clone();
        let names = output_names.clone();
        let button = mic_test.clone();
        output_dd.connect_selected_notify(move |dropdown| {
            let name = names
                .get(dropdown.selected() as usize)
                .cloned()
                .unwrap_or_default();
            app.update_prefs(|prefs| prefs.output_device = name);
            restart_mic_test(&app, &state, &button);
        });
        let app = self.clone();
        input_volume.connect_value_changed(move |scale| {
            let volume = (scale.value() / 100.0) as f32;
            app.update_prefs(|prefs| prefs.input_volume = volume);
        });
        let app = self.clone();
        output_volume.connect_value_changed(move |scale| {
            let volume = (scale.value() / 100.0) as f32;
            app.update_prefs(|prefs| prefs.output_volume = volume);
        });

        let app = self.clone();
        let state = panel.clone();
        mic_test.connect_clicked(move |button| {
            if state.mic_test.borrow_mut().take().is_some() {
                button.set_label("Mic Test");
                return;
            }
            start_mic_test(&app, &state, button, &meter);
        });

        page.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        // Video: the two test boxes, a source choice under each, then the
        // stream quality settings.
        page.append(&section_title("Video Settings"));
        let share_dd = gtk::DropDown::from_strings(&SHARE_SOURCES);
        share_dd.set_selected(current.share_source.min(2));
        let camera_column = gtk::Box::new(gtk::Orientation::Vertical, 8);
        camera_column.append(camera_frame);
        camera_column.append(&field_label("Camera"));
        camera_column.append(camera_dropdown);
        let screen_column = gtk::Box::new(gtk::Orientation::Vertical, 8);
        screen_column.append(screen_frame);
        screen_column.append(&field_label("Screen Share"));
        screen_column.append(&share_dd);
        let previews = gtk::Box::new(gtk::Orientation::Horizontal, 20);
        previews.set_homogeneous(true);
        previews.append(&camera_column);
        previews.append(&screen_column);
        page.append(&previews);

        let app = self.clone();
        let state = panel.clone();
        panel.camera.button.connect_clicked(move |_| {
            if state.camera.running() {
                state.camera.stop();
            } else {
                start_camera_test(&app, &state);
            }
        });
        let app = self.clone();
        let state = panel.clone();
        panel.screen.button.connect_clicked(move |_| {
            if state.screen.running() {
                state.screen.stop();
            } else {
                start_screen_test(&app, &state);
            }
        });
        let app = self.clone();
        let state = panel.clone();
        camera_dropdown.connect_selected_notify(move |dropdown| {
            if state.refilling.get() {
                return;
            }
            let name = match dropdown.selected() {
                0 => String::new(),
                _ => dropdown_text(dropdown),
            };
            app.update_prefs(|prefs| prefs.camera = name);
            if state.camera.running() {
                state.camera.stop();
                start_camera_test(&app, &state);
            }
        });
        let app = self.clone();
        share_dd.connect_selected_notify(move |dropdown| {
            let source = dropdown.selected();
            app.update_prefs(|prefs| prefs.share_source = source);
        });
        // Until the portal lists the cameras, show the saved choice.
        if !current.camera.is_empty() {
            panel.refilling.set(true);
            if let Some(model) = camera_dropdown.model().and_downcast::<gtk::StringList>() {
                model.append(&current.camera);
                camera_dropdown.set_selected(1);
            }
            panel.refilling.set(false);
        }

        page.append(&self.stream_quality_section());
        page
    }

    /// Resolution, framerate and the rest of the capture settings, the
    /// Advanced expander, and the button that resets them.
    fn stream_quality_section(self: &Rc<Self>) -> gtk::Box {
        let section = gtk::Box::new(gtk::Orientation::Vertical, 12);
        section.set_margin_top(8);
        let current = self.capture.borrow().clone();

        let resolution_names: Vec<&str> = RESOLUTIONS.iter().map(|(_, _, name)| *name).collect();
        let resolution = gtk::DropDown::from_strings(&resolution_names);
        let framerate_names: Vec<String> =
            FRAMERATES.iter().map(|fps| format!("{fps} fps")).collect();
        let framerate = string_dropdown(&framerate_names);
        let mut bitrate_names = vec!["Automatic".to_owned()];
        bitrate_names.extend(BITRATES.iter().map(|kbps| format!("{kbps} kbps")));
        let bitrate = string_dropdown(&bitrate_names);
        let delay = gtk::DropDown::from_strings(&["None", "0.5 s", "1 s", "2 s", "3 s"]);
        let cpu_encoder = gtk::Switch::new();
        cpu_encoder.set_valign(gtk::Align::Center);

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
        leaky.set_valign(gtk::Align::Center);
        leaky.set_tooltip_text(Some(
            "Leaky queue: when the encoder is slow, discard old frames instead of letting latency build.",
        ));

        // Shows `prefs` in every widget; the handlers below then agree.
        let show = {
            let resolution = resolution.clone();
            let framerate = framerate.clone();
            let bitrate = bitrate.clone();
            let delay = delay.clone();
            let cpu_encoder = cpu_encoder.clone();
            let keyframe = keyframe.clone();
            let retransmit = retransmit.clone();
            let drop_late = drop_late.clone();
            let leaky = leaky.clone();
            move |prefs: &CapturePreferences| {
                resolution.set_selected(
                    RESOLUTIONS
                        .iter()
                        .position(|(width, height, _)| {
                            *width == prefs.width && *height == prefs.height
                        })
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
            }
        };
        show(&current);

        let app = self.clone();
        resolution.connect_selected_notify(move |dropdown| {
            if let Some((width, height, _)) = RESOLUTIONS.get(dropdown.selected() as usize) {
                app.update_prefs(|prefs| {
                    prefs.width = *width;
                    prefs.height = *height;
                });
            }
        });
        let app = self.clone();
        framerate.connect_selected_notify(move |dropdown| {
            if let Some(fps) = FRAMERATES.get(dropdown.selected() as usize) {
                app.update_prefs(|prefs| prefs.fps = *fps);
            }
        });
        let app = self.clone();
        bitrate.connect_selected_notify(move |dropdown| {
            let kbps = match dropdown.selected() as usize {
                0 => None,
                index => BITRATES.get(index - 1).copied(),
            };
            app.update_prefs(|prefs| prefs.bitrate_kbps = kbps);
        });
        let app = self.clone();
        delay.connect_selected_notify(move |dropdown| {
            if let Some(delay) = STREAM_DELAYS.get(dropdown.selected() as usize) {
                app.update_prefs(|prefs| prefs.stream_delay_ms = *delay);
            }
        });
        let app = self.clone();
        cpu_encoder.connect_active_notify(move |switch| {
            let active = switch.is_active();
            app.update_prefs(|prefs| prefs.cpu_encoder = active);
        });
        let app = self.clone();
        keyframe.connect_selected_notify(move |dropdown| {
            if let Some(seconds) = KEYFRAME_INTERVALS.get(dropdown.selected() as usize) {
                app.update_prefs(|prefs| prefs.keyframe_secs = *seconds);
            }
        });
        let app = self.clone();
        retransmit.connect_selected_notify(move |dropdown| {
            if let Some(ms) = RETRANSMIT_WINDOWS_MS.get(dropdown.selected() as usize) {
                app.update_prefs(|prefs| prefs.retransmit_ms = *ms);
            }
        });
        let app = self.clone();
        drop_late.connect_selected_notify(move |dropdown| {
            if let Some(ms) = DROP_AFTER_MS.get(dropdown.selected() as usize) {
                app.update_prefs(|prefs| prefs.drop_frames_after_ms = *ms);
            }
        });
        let app = self.clone();
        leaky.connect_active_notify(move |switch| {
            let active = switch.is_active();
            app.update_prefs(|prefs| prefs.leaky_queue = active);
        });

        let quality = adw::PreferencesGroup::new();
        quality.set_title("Stream Quality");
        quality.set_description(Some(
            "Used for your camera and Go Live. Automatic bitrate follows resolution and framerate. Changes apply the next time you start a camera or stream.",
        ));
        quality.add(&setting_row("Resolution", &resolution));
        quality.add(&setting_row("Framerate", &framerate));
        quality.add(&setting_row("Bitrate", &bitrate));
        quality.add(&setting_row("Go Live Delay", &delay));
        let encoder_row = setting_row("Use Software Encoder (x264)", &cpu_encoder);
        encoder_row.set_subtitle("Hardware encoding is used when available otherwise");
        encoder_row.set_activatable_widget(Some(&cpu_encoder));
        quality.add(&encoder_row);
        section.append(&quality);

        let advanced = adw::PreferencesGroup::new();
        let expander = adw::ExpanderRow::new();
        expander.set_title("Advanced");
        expander.set_subtitle("Keyframes, dropout recovery and frame dropping");
        expander.add_row(&setting_row("Keyframe Interval", &keyframe));
        expander.add_row(&setting_row("Dropout Recovery Window", &retransmit));
        let drop_row = setting_row("Drop Late Frames After", &drop_late);
        drop_row.set_subtitle("Applies beyond the Go Live delay");
        expander.add_row(&drop_row);
        let leaky_row = setting_row("Drop Old Frames in the Capture Queue", &leaky);
        leaky_row.set_activatable_widget(Some(&leaky));
        expander.add_row(&leaky_row);
        advanced.add(&expander);
        section.append(&advanced);

        let reset = gtk::Button::with_label("Reset Video Settings");
        reset.add_css_class("destructive-action");
        reset.set_halign(gtk::Align::Start);
        reset.set_tooltip_text(Some(
            "Puts resolution, framerate, bitrate, delay, encoder and the advanced settings back to their defaults.",
        ));
        let app = self.clone();
        reset.connect_clicked(move |_| {
            app.update_prefs(CapturePreferences::reset_video);
            // Clone first: the widgets' handlers borrow the preferences.
            let prefs = app.capture.borrow().clone();
            show(&prefs);
            app.set_status("Video settings reset to defaults");
        });
        section.append(&reset);
        section
    }

    /// Asks the camera portal for the cameras and fills the dropdown.
    fn load_cameras(self: &Rc<Self>, panel: &Rc<Panel>, dropdown: &gtk::DropDown) {
        let panel = panel.clone();
        let dropdown = dropdown.clone();
        spawn_local(
            self,
            kc_media::portal::request_cameras(),
            move |app, result| {
                let access = match result {
                    Ok(Some(access)) => access,
                    Ok(None) => return,
                    Err(error) => {
                        tracing::debug!(%error, "camera list for settings");
                        return;
                    }
                };
                let saved = app.capture.borrow().camera.clone();
                let mut names = vec!["Ask Every Time".to_owned()];
                names.extend(access.cameras.iter().map(|camera| camera.name.clone()));
                if !saved.is_empty() && !names.contains(&saved) {
                    names.push(saved.clone());
                }
                let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                panel.refilling.set(true);
                dropdown.set_model(Some(&gtk::StringList::new(&refs)));
                let index = names
                    .iter()
                    .position(|name| !saved.is_empty() && *name == saved)
                    .unwrap_or(0);
                dropdown.set_selected(index as u32);
                panel.refilling.set(false);
                *panel.cameras.borrow_mut() = Some(access);
            },
        );
    }
}

/// Runs `future` on the Tokio runtime, then `done` with its result on the GTK
/// thread. Unlike `AppState::spawn`, `done` may hold GTK objects.
fn spawn_local<T: Send + 'static>(
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

fn start_mic_test(
    app: &Rc<AppState>,
    panel: &Rc<Panel>,
    button: &gtk::Button,
    meter: &gtk::LevelBar,
) {
    match MicTest::start() {
        Ok(test) => {
            *panel.mic_test.borrow_mut() = Some(test);
            button.set_label("Stop Testing");
        }
        Err(error) => {
            app.set_status(&format!("Mic test: {error}"));
            return;
        }
    }
    // Follow the level until the test stops or the panel goes away.
    let state = Rc::downgrade(panel);
    let meter = meter.clone();
    let shown = Cell::new(0.0f64);
    glib::timeout_add_local(Duration::from_millis(40), move || {
        let Some(state) = state.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let peak = match state.mic_test.borrow().as_ref() {
            Some(test) => test.take_peak(),
            None => {
                meter.set_value(0.0);
                return glib::ControlFlow::Break;
            }
        };
        // -60 dBFS to 0 dBFS across the meter, falling back gently.
        let level = if peak > 0.0 {
            ((20.0 * f64::from(peak).log10() + 60.0) / 60.0).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let level = level.max(shown.get() * 0.85);
        shown.set(level);
        meter.set_value(level * METER_BLOCKS);
        glib::ControlFlow::Continue
    });
}

/// A device change applies to a running mic test straight away.
fn restart_mic_test(app: &Rc<AppState>, panel: &Rc<Panel>, button: &gtk::Button) {
    if panel.mic_test.borrow_mut().take().is_none() {
        return;
    }
    match MicTest::start() {
        Ok(test) => *panel.mic_test.borrow_mut() = Some(test),
        Err(error) => {
            button.set_label("Mic Test");
            app.set_status(&format!("Mic test: {error}"));
        }
    }
}

/// Encodes each captured frame and decodes it into the preview, so the test
/// shows exactly what others would see with the current settings.
fn preview_sink(
    feed: Arc<VideoFeed>,
) -> impl FnMut(kc_media::video::EncodedFrame) + Send + 'static {
    move |frame| {
        let keyframe = is_keyframe(&frame.data);
        feed.push(frame.data, keyframe);
    }
}

fn start_camera_test(app: &Rc<AppState>, panel: &Rc<Panel>) {
    let preview = panel.camera.clone();
    let Some((feed, generation)) = preview.begin() else {
        app.set_status("Video preview is unavailable");
        return;
    };
    let access = panel.cameras.borrow().clone();
    let wanted = app.capture.borrow().camera.clone();
    let settings = app.capture.borrow().video_settings();
    spawn_local(
        app,
        camera_test_source(access, wanted, settings, feed),
        move |app, result| match result {
            Ok(source) => preview.attach(generation, source),
            Err(error) => {
                if preview.generation.get() == generation {
                    preview.stop();
                }
                app.set_status(&format!("Camera test: {error}"));
            }
        },
    );
}

async fn camera_test_source(
    access: Option<CameraAccess>,
    wanted: String,
    settings: VideoSettings,
    feed: Arc<VideoFeed>,
) -> Result<VideoSource, String> {
    let access = match access {
        Some(access) => access,
        None => kc_media::portal::request_cameras()
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "no camera available".to_owned())?,
    };
    let camera = access
        .cameras
        .iter()
        .find(|camera| camera.name == wanted)
        .and_then(|camera| access.select(camera.node_id))
        .or_else(|| access.preferred())
        .ok_or_else(|| "no camera available".to_owned())?;
    VideoSource::camera_portal(camera, settings, preview_sink(feed))
        .map_err(|error| error.to_string())
}

fn start_screen_test(app: &Rc<AppState>, panel: &Rc<Panel>) {
    let preview = panel.screen.clone();
    let Some((feed, generation)) = preview.begin() else {
        app.set_status("Video preview is unavailable");
        return;
    };
    let parent = app.window_identifier.borrow().clone();
    let settings = app.capture.borrow().video_settings();
    spawn_local(
        app,
        async move {
            let portal = tokio::time::timeout(
                Duration::from_secs(120),
                kc_media::portal::request(parent.as_deref()),
            )
            .await
            .map_err(|_| "the screen picker timed out".to_owned())?
            .map_err(|error| error.to_string())?;
            VideoSource::screen(portal, settings, preview_sink(feed))
                .map_err(|error| error.to_string())
        },
        move |app, result| match result {
            Ok(source) => preview.attach(generation, source),
            Err(error) => {
                if preview.generation.get() == generation {
                    preview.stop();
                }
                app.set_status(&format!("Screenshare test: {error}"));
            }
        },
    );
}

// -- widgets -----------------------------------------------------------------

fn page_box() -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 14);
    page.set_margin_top(24);
    page.set_margin_bottom(32);
    page.set_margin_start(24);
    page.set_margin_end(24);
    page
}

fn page_scroll(page: &gtk::Box) -> gtk::ScrolledWindow {
    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(760);
    clamp.set_child(Some(page));
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_child(Some(&clamp));
    scroll
}

fn section_title(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("title-3");
    label
}

fn field_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("heading");
    label
}

fn setting_row(title: &str, widget: &impl IsA<gtk::Widget>) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    widget.set_valign(gtk::Align::Center);
    row.add_suffix(widget);
    row
}

fn string_dropdown(names: &[String]) -> gtk::DropDown {
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    gtk::DropDown::from_strings(&refs)
}

fn position<T: PartialEq>(values: &[T], value: &T, fallback: usize) -> u32 {
    values
        .iter()
        .position(|candidate| candidate == value)
        .unwrap_or(fallback) as u32
}

/// "Default" and the devices, with the node names in the same order.
fn device_dropdown(
    devices: impl Iterator<Item = (String, String)>,
    selected: &str,
) -> (Rc<Vec<String>>, gtk::DropDown) {
    let mut names = vec![String::new()];
    let mut labels = vec!["Default".to_owned()];
    for (name, label) in devices {
        names.push(name);
        labels.push(shorten(&label, DEVICE_LABEL_CHARS));
    }
    let dropdown = string_dropdown(&labels);
    dropdown.set_hexpand(true);
    dropdown.set_selected(names.iter().position(|name| name == selected).unwrap_or(0) as u32);
    (Rc::new(names), dropdown)
}

fn volume_scale(volume: f32) -> gtk::Scale {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 200.0, 1.0);
    scale.set_value(f64::from(volume) * 100.0);
    scale.add_mark(100.0, gtk::PositionType::Bottom, None);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_format_value_func(|_, value| format!("{value:.0}%"));
    scale.set_hexpand(true);
    scale
}

fn dropdown_text(dropdown: &gtk::DropDown) -> String {
    dropdown
        .selected_item()
        .and_downcast::<gtk::StringObject>()
        .map(|item| item.string().to_string())
        .unwrap_or_default()
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(max - 1).collect();
    short.push('…');
    short
}

/// A row whose value is masked until Reveal is pressed, as Discord shows
/// email addresses and phone numbers.
fn revealable_row(
    title: &str,
    value: Option<&str>,
    mask: fn(&str) -> String,
    missing: &str,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        row.set_subtitle(missing);
        return row;
    };
    let masked = mask(value);
    row.set_subtitle(&masked);
    let reveal = gtk::Button::with_label("Reveal");
    reveal.add_css_class("flat");
    reveal.set_valign(gtk::Align::Center);
    let shown = Cell::new(false);
    let value = value.to_owned();
    let target = row.clone();
    reveal.connect_clicked(move |button| {
        let reveal = !shown.get();
        shown.set(reveal);
        target.set_subtitle(if reveal {
            value.as_str()
        } else {
            masked.as_str()
        });
        button.set_label(if reveal { "Hide" } else { "Reveal" });
    });
    row.add_suffix(&reveal);
    row
}

fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((local, domain)) => format!("{}@{domain}", "*".repeat(local.chars().count())),
        None => "*".repeat(email.chars().count()),
    }
}

fn mask_phone(phone: &str) -> String {
    let count = phone.chars().count();
    let keep = 4.min(count);
    let tail: String = phone.chars().skip(count - keep).collect();
    format!("{}{tail}", "*".repeat(count - keep))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_hide_all_but_the_domain_and_last_digits() {
        assert_eq!(mask_email("kyle@example.com"), "****@example.com");
        assert_eq!(mask_phone("+15551239091"), "********9091");
        assert_eq!(mask_phone("12"), "12");
    }

    #[test]
    fn long_device_names_are_shortened() {
        assert_eq!(shorten("short", 10), "short");
        assert_eq!(shorten("abcdefghijkl", 6), "abcde…");
    }
}
