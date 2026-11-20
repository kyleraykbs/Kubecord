//! The settings panel: categories on the left, the chosen page on the right.
//! Changes apply as they are made, as in Discord; there is nothing to save.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_http::cdn;
use kc_media::audio::MicTest;
use kc_media::portal::{CameraAccess, ShareSources};

use crate::settings::{CapturePreferences, SHARE_SOURCES};
use crate::shell::AppState;
use crate::video_setup::{
    Apply, CameraPick, Preview, quality_controls, spawn_local, start_camera_preview,
    start_screen_preview, string_dropdown,
};

/// Longest device name shown in a dropdown before it is shortened.
const DEVICE_LABEL_CHARS: usize = 40;
/// Segments in the mic test's level meter.
const METER_BLOCKS: f64 = 40.0;

/// The pages, in order: name, title, icon. Each one is a tab in the list
/// on the left.
const PAGES: [(&str, &str, &str); 5] = [
    ("account", "My Account", "avatar-default-symbolic"),
    ("voice", "Voice", "audio-input-microphone-symbolic"),
    ("video", "Video", "camera-web-symbolic"),
    ("chat", "Chat", "chat-message-new-symbolic"),
    ("advanced", "Advanced", "utilities-terminal-symbolic"),
];

/// The heading above a page's row, where a group of pages starts.
fn page_heading(name: &str) -> Option<&'static str> {
    match name {
        "account" => Some("User Settings"),
        "voice" => Some("App Settings"),
        "advanced" => Some("Other"),
        _ => None,
    }
}

/// Page names from before the pages were split up, so dev aids and old
/// callers still land on the right page.
fn page_alias(name: &str) -> &str {
    match name {
        "features" | "appearance" | "text" => "chat",
        "compat" | "compatibility" | "debug" => "advanced",
        other => other,
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

    /// Opens the panel on `page`, one of the names in [`PAGES`] (older
    /// names such as "features" or "debug" still work).
    pub(crate) fn show_settings_page(self: &Rc<Self>, page: &str) {
        let dialog = adw::Dialog::new();
        dialog.set_title("Settings");
        dialog.set_content_width(960);
        dialog.set_content_height(
            std::env::var("KYLECORD_WINDOW_HEIGHT")
                .ok()
                .and_then(|height| height.parse::<i32>().ok())
                .map_or(700, |height| height - 20),
        );
        // Required with a breakpoint: the size it may shrink to.
        dialog.set_width_request(360);
        dialog.set_height_request(400);

        let (camera, camera_frame) = Preview::new("Test Camera", 180);
        let (screen, screen_frame) = Preview::new("Test Screenshare", 180);
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
        let camera_dropdown = gtk::DropDown::from_strings(&["Ask Every Time"]);
        for (name, _, _) in PAGES {
            let content = match name {
                "account" => self.account_page(&dialog),
                "voice" => self.voice_page(&panel),
                "video" => self.video_page(&panel, &camera_frame, &screen_frame, &camera_dropdown),
                "chat" => self.chat_page(),
                _ => self.advanced_page(),
            };
            pages.add_named(&page_scroll(&content), Some(name));
        }

        let content_view = adw::ToolbarView::new();
        content_view.add_top_bar(&adw::HeaderBar::new());
        content_view.set_content(Some(&pages));
        let content_page = adw::NavigationPage::new(&content_view, "My Account");

        // Categories, under Discord's two headings.
        let categories = gtk::ListBox::new();
        categories.add_css_class("navigation-sidebar");
        for (_, title, icon) in PAGES {
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
            let Some(heading) = PAGES
                .get(row.index() as usize)
                .and_then(|(name, _, _)| page_heading(name))
            else {
                row.set_header(None::<&gtk::Widget>);
                return;
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
            let Some((name, title, _)) = PAGES.get(row.index() as usize) else {
                return;
            };
            pages.set_visible_child_name(name);
            content_page.set_title(title);
            target_split.set_show_content(true);
            if *name == "video" && !opened.cameras_requested.replace(true) {
                app.load_cameras(&opened, &dropdown);
            }
        });
        let page = page_alias(page);
        let initial = PAGES
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

    // -- chat ----------------------------------------------------------------

    /// How messages look and what others see of your typing.
    fn chat_page(self: &Rc<Self>) -> gtk::Box {
        let page = page_box();
        page.append(&section_title("Chat"));

        let typing = adw::PreferencesGroup::new();
        typing.set_title("Typing");
        let show = adw::SwitchRow::new();
        show.set_title("Show When You're Typing");
        show.set_subtitle(
            "Let others see that you're typing. Turn it off to type unseen. The eye button in the message box switches this too.",
        );
        show.set_subtitle_lines(0);
        show.set_active(self.capture.borrow().send_typing);
        let app = self.clone();
        show.connect_active_notify(move |row| app.set_send_typing(row.is_active()));
        typing.add(&show);
        page.append(&typing);

        let time = adw::PreferencesGroup::new();
        time.set_title("Time");
        let clock = adw::SwitchRow::new();
        clock.set_title("Use 24-Hour Time");
        clock.set_subtitle("Show times as 17:58 instead of 5:58 PM.");
        clock.set_active(self.capture.borrow().use_24_hour);
        let app = self.clone();
        clock.connect_active_notify(move |row| {
            app.update_prefs(|prefs| prefs.use_24_hour = row.is_active());
            app.redraw_times();
        });
        time.add(&clock);
        page.append(&time);
        page
    }

    /// How far the User Volume slider in a person's menu goes.
    fn volume_boost_row(self: &Rc<Self>) -> adw::ComboRow {
        let names: Vec<String> = crate::settings::VOLUME_BOOST_CAPS
            .iter()
            .map(|times| format!("{times}×"))
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let boost = adw::ComboRow::new();
        boost.set_title("Maximum User Volume");
        boost.set_subtitle(
            "How far the User Volume slider in a person's right-click menu goes: 3× is up to 300%. Above 1× makes quiet people louder; very high settings can distort people who are already loud.",
        );
        boost.set_subtitle_lines(0);
        boost.set_model(Some(&gtk::StringList::new(&names)));
        let current = self.capture.borrow().volume_boost_cap;
        boost.set_selected(
            crate::settings::VOLUME_BOOST_CAPS
                .iter()
                .position(|times| *times == current)
                .unwrap_or(2) as u32,
        );
        let app = self.clone();
        boost.connect_selected_notify(move |row| {
            if let Some(times) = crate::settings::VOLUME_BOOST_CAPS
                .get(row.selected() as usize)
                .copied()
            {
                app.update_prefs(|prefs| prefs.volume_boost_cap = times);
            }
        });
        boost
    }

    // -- advanced ------------------------------------------------------------

    /// Discord's checks for the official app, and what kylecord logs.
    fn advanced_page(self: &Rc<Self>) -> gtk::Box {
        let page = page_box();
        page.append(&section_title("Advanced"));

        // Compatibility: how kylecord handles the checks Discord makes for
        // the official app.
        let group = adw::PreferencesGroup::new();
        group.set_title("Captchas");
        group.set_description(Some(
            "Discord sometimes asks for a captcha, for example before it sends a friend request.",
        ));
        let in_app = adw::SwitchRow::new();
        in_app.set_title("Solve Captchas in kylecord");
        in_app.set_subtitle(if crate::captcha::AVAILABLE {
            "Show the captcha here and send the request again once it is solved. When this is off, or the captcha fails, you can finish in your browser instead."
        } else {
            "This build of kylecord was made without the captcha view, so captchas are finished in your browser."
        });
        in_app.set_active(crate::captcha::AVAILABLE && self.capture.borrow().captcha_in_app);
        in_app.set_sensitive(crate::captcha::AVAILABLE);
        let app = self.clone();
        in_app.connect_active_notify(move |row| {
            let active = row.is_active();
            app.update_prefs(|prefs| prefs.captcha_in_app = active);
        });
        group.add(&in_app);

        let test_row = adw::ActionRow::builder()
            .title("Test the Captcha View")
            .subtitle("Opens a test captcha that always passes. Nothing is sent to Discord.")
            .build();
        let test = gtk::Button::with_label("Test");
        test.set_valign(gtk::Align::Center);
        test.set_sensitive(crate::captcha::AVAILABLE);
        let app = self.clone();
        test.connect_clicked(move |_| app.test_captcha());
        test_row.add_suffix(&test);
        group.add(&test_row);
        page.append(&group);

        // Debug: the log helps work out what went wrong in a call. It stays
        // on this computer.
        let group = adw::PreferencesGroup::new();
        group.set_title("Log File");
        let path = crate::logging::path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "unavailable".to_owned());
        group.set_description(Some(&format!(
            "{path}\nEach launch starts a new log and keeps the one before. It never holds tokens or message text.",
        )));

        let to_file = adw::SwitchRow::new();
        to_file.set_title("Write a Log File");
        to_file.set_subtitle(
            "Small: about 200 KB for an hour of calls with video. Off writes nothing.",
        );
        to_file.set_subtitle_lines(0);
        to_file.set_active(self.capture.borrow().log_to_file);
        let app = self.clone();
        to_file.connect_active_notify(move |row| {
            let active = row.is_active();
            app.update_prefs(|prefs| prefs.log_to_file = active);
        });
        group.add(&to_file);

        let detailed = adw::SwitchRow::new();
        detailed.set_title("Detailed Media Logging");
        detailed.set_subtitle(&format!(
            "Also records every frame and packet event in calls. Large: tens of megabytes an hour. Only while chasing a problem. The file stops growing at {} MB either way.",
            crate::logging::MAX_BYTES / (1024 * 1024)
        ));
        detailed.set_subtitle_lines(0);
        detailed.set_active(self.capture.borrow().log_detailed);
        let app = self.clone();
        detailed.connect_active_notify(move |row| {
            let active = row.is_active();
            app.update_prefs(|prefs| prefs.log_detailed = active);
        });
        group.add(&detailed);

        let size = adw::ActionRow::builder().title("This Run's Log").build();
        let show_size = {
            let size = size.clone();
            move || {
                let bytes = crate::logging::written();
                size.set_subtitle(&if bytes < 1024 * 1024 {
                    format!("{} KB so far", bytes.div_ceil(1024))
                } else {
                    format!("{:.1} MB so far", bytes as f64 / (1024.0 * 1024.0))
                });
            }
        };
        show_size();
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.add_css_class("flat");
        refresh.set_valign(gtk::Align::Center);
        refresh.set_tooltip_text(Some("Refresh"));
        refresh.connect_clicked(move |_| show_size());
        size.add_suffix(&refresh);
        let open = gtk::Button::with_label("Open Folder");
        open.set_valign(gtk::Align::Center);
        let window = self.window.clone();
        open.connect_clicked(move |_| {
            if let Some(path) = crate::logging::path() {
                gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(path)))
                    .open_containing_folder(Some(&window), None::<&gtk::gio::Cancellable>, |_| {});
            }
        });
        size.add_suffix(&open);
        group.add(&size);
        page.append(&group);
        page
    }

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

    fn voice_page(self: &Rc<Self>, panel: &Rc<Panel>) -> gtk::Box {
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

        let people = adw::PreferencesGroup::new();
        people.set_title("Other People");
        people.add(&self.volume_boost_row());
        page.append(&people);
        page
    }

    fn video_page(
        self: &Rc<Self>,
        panel: &Rc<Panel>,
        camera_frame: &gtk::Overlay,
        screen_frame: &gtk::Overlay,
        camera_dropdown: &gtk::DropDown,
    ) -> gtk::Box {
        let page = page_box();
        let current = self.capture.borrow().clone();

        // The two test boxes, a source choice under each, then the stream
        // quality settings.
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

        let preview_group = adw::PreferencesGroup::new();
        let self_preview = adw::SwitchRow::new();
        self_preview.set_title("Preview My Camera and Stream");
        self_preview.set_subtitle(
            "Show your own video in calls. Turning it off saves work on a slow machine. You can also right-click your own tile in a call.",
        );
        self_preview.set_active(current.self_preview);
        let app = self.clone();
        self_preview.connect_active_notify(move |row| app.set_self_preview(row.is_active()));
        preview_group.add(&self_preview);
        page.append(&preview_group);

        page.append(&self.stream_quality_section());
        page
    }

    /// Resolution, framerate and the rest of the capture settings, the
    /// Advanced expander, and the button that resets them.
    fn stream_quality_section(self: &Rc<Self>) -> gtk::Box {
        let section = gtk::Box::new(gtk::Orientation::Vertical, 12);
        section.set_margin_top(8);
        let current = self.capture.borrow().clone();
        let app = self.clone();
        let apply: Apply = Rc::new(move |change| app.update_prefs(|prefs| change(prefs)));
        let controls = quality_controls(&current, true, apply);

        let quality = adw::PreferencesGroup::new();
        quality.set_title("Stream Quality");
        quality.set_description(Some(
            "Used for your camera and Go Live, from the next time you start one. The defaults are a good place to stay: change one thing at a time, and Reset Video Settings below puts them back.",
        ));
        for row in &controls.rows {
            quality.add(row);
        }
        section.append(&quality);

        let advanced = adw::PreferencesGroup::new();
        let expander = adw::ExpanderRow::new();
        expander.set_title("Advanced");
        expander.set_subtitle("Keyframes, dropout recovery and frame dropping");
        for row in &controls.advanced {
            expander.add_row(row);
        }
        advanced.add(&expander);
        section.append(&advanced);

        let reset = gtk::Button::with_label("Reset Video Settings");
        reset.add_css_class("destructive-action");
        reset.set_halign(gtk::Align::Start);
        reset.set_tooltip_text(Some(
            "Puts resolution, framerate, bitrate, delay, encoder and the advanced settings back to their defaults.",
        ));
        let app = self.clone();
        let show = controls.show;
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

fn start_camera_test(app: &Rc<AppState>, panel: &Rc<Panel>) {
    let access = panel.cameras.borrow().clone();
    let wanted = app.capture.borrow().camera.clone();
    let settings = app.capture.borrow().video_settings();
    start_camera_preview(
        app,
        &panel.camera,
        access,
        CameraPick::Named(wanted),
        settings,
    );
}

fn start_screen_test(app: &Rc<AppState>, panel: &Rc<Panel>) {
    let settings = app.capture.borrow().video_settings();
    start_screen_preview(app, &panel.screen, ShareSources::current(), settings);
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
    dev_scroll(&scroll);
    scroll
}

/// Development aid: `KYLECORD_SETTINGS_SCROLL=<pixels>` scrolls settings
/// pages and popups down once they are laid out, for screenshots of long
/// ones.
pub(crate) fn dev_scroll(scroll: &gtk::ScrolledWindow) {
    if let Some(offset) = std::env::var("KYLECORD_SETTINGS_SCROLL")
        .ok()
        .and_then(|offset| offset.parse::<f64>().ok())
    {
        let target = scroll.clone();
        gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
            target.vadjustment().set_value(offset);
        });
    }
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
    fn old_page_names_still_open_a_page() {
        for name in ["features", "compat", "debug", "voice", "account", "video"] {
            let page = page_alias(name);
            assert!(PAGES.iter().any(|(known, _, _)| *known == page), "{name}");
        }
        // Every page group starts with a heading.
        assert_eq!(page_heading(PAGES[0].0), Some("User Settings"));
    }

    #[test]
    fn long_device_names_are_shortened() {
        assert_eq!(shorten("short", 10), "short");
        assert_eq!(shorten("abcdefghijkl", 6), "abcde…");
    }
}
