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

thread_local! {
    /// Every open per-sound volume slider, by sound key, so the copies on the
    /// Sounds and Notifications pages stay in step.
    static SOUND_SCALES: RefCell<Vec<(&'static str, glib::WeakRef<gtk::Scale>)>> =
        const { RefCell::new(Vec::new()) };
}
/// Segments in the mic test's level meter.
const METER_BLOCKS: f64 = 40.0;

/// The pages, in order: name, title, icon. Each one is a tab in the list
/// on the left.
const PAGES: [(&str, &str, &str); 8] = [
    ("account", "My Account", "avatar-default-symbolic"),
    ("voice", "Voice", "audio-input-microphone-symbolic"),
    ("video", "Video", "camera-web-symbolic"),
    (
        "notifications",
        "Notifications",
        "preferences-system-notifications-symbolic",
    ),
    ("sounds", "Sounds", "audio-volume-high-symbolic"),
    ("chat", "Chat", "chat-message-new-symbolic"),
    ("theming", "Theming", "applications-graphics-symbolic"),
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
        "features" | "text" => "chat",
        "appearance" | "theme" | "themes" => "theming",
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
            std::env::var("KUBECORD_WINDOW_HEIGHT")
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
                "notifications" => self.notifications_page(),
                "sounds" => self.sounds_page(),
                "theming" => self.theming_page(),
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

    // -- notifications -------------------------------------------------------

    /// A switch row bound to one boolean preference.
    fn pref_switch(
        self: &Rc<Self>,
        title: &str,
        subtitle: &str,
        get: fn(&CapturePreferences) -> bool,
        set: fn(&mut CapturePreferences, bool),
    ) -> adw::SwitchRow {
        let row = adw::SwitchRow::new();
        row.set_title(title);
        row.set_subtitle(subtitle);
        row.set_subtitle_lines(0);
        row.set_active(get(&self.capture.borrow()));
        let app = self.clone();
        row.connect_active_notify(move |row| {
            let active = row.is_active();
            app.update_prefs(|prefs| set(prefs, active));
        });
        row
    }

    /// When the channel you have open stays quiet (no sound or notification).
    fn open_channel_quiet_row(self: &Rc<Self>) -> adw::ComboRow {
        use crate::settings::OPEN_CHANNEL_QUIET;
        let row = adw::ComboRow::new();
        row.set_title("Quiet the Open Channel");
        row.set_subtitle(
            "No sound or notification for messages in the channel you have open. \"While Kubecord Is Focused\" is Discord's behaviour; \"Always\" also keeps it quiet with the window in the background.",
        );
        row.set_subtitle_lines(0);
        let names: Vec<&str> = OPEN_CHANNEL_QUIET.iter().map(|(_, name)| *name).collect();
        row.set_model(Some(&gtk::StringList::new(&names)));
        let current = self.capture.borrow().open_channel_quiet;
        row.set_selected(
            OPEN_CHANNEL_QUIET
                .iter()
                .position(|(choice, _)| *choice == current)
                .unwrap_or(0) as u32,
        );
        let app = self.clone();
        row.connect_selected_notify(move |row| {
            if let Some((choice, _)) = OPEN_CHANNEL_QUIET.get(row.selected() as usize) {
                let choice = *choice;
                app.update_prefs(|prefs| prefs.open_channel_quiet = choice);
            }
        });
        row
    }

    /// Desktop notifications: whether, what, and how they look.
    fn notifications_page(self: &Rc<Self>) -> gtk::Box {
        let page = page_box();
        page.append(&section_title("Notifications"));
        let intro = gtk::Label::new(Some(
            "Each server's and channel's own notification settings apply too (right-click a channel, or the server menu). Nothing notifies while your status is Do Not Disturb, for your own messages, for people you blocked, or for the channel you are looking at.",
        ));
        intro.set_xalign(0.0);
        intro.set_wrap(true);
        intro.add_css_class("dim-label");
        page.append(&intro);

        let desktop = adw::PreferencesGroup::new();
        desktop.set_title("Desktop Notifications");
        desktop.add(&self.pref_switch(
            "Enable Desktop Notifications",
            "Show a notification for new messages that concern you. Clicking it opens the message.",
            |prefs| prefs.notify_desktop,
            |prefs, on| prefs.notify_desktop = on,
        ));
        desktop.add(&self.pref_switch(
            "Show Message Content",
            "Put the message text in the notification. Off, it only says who wrote and where.",
            |prefs| prefs.notify_preview,
            |prefs, on| prefs.notify_preview = on,
        ));
        let test_row = adw::ActionRow::builder()
            .title("Test Notifications")
            .subtitle("Sends a notification to your desktop now.")
            .build();
        let test = gtk::Button::with_label("Test");
        test.set_valign(gtk::Align::Center);
        let app = self.clone();
        test.connect_clicked(move |_| app.test_notification());
        test_row.add_suffix(&test);
        desktop.add(&test_row);
        page.append(&desktop);

        // The message sounds' own volumes, mirrored from the Sounds page (the
        // same setting: moving either slider moves both).
        let sounds = adw::PreferencesGroup::new();
        sounds.set_title("Notification Sounds");
        sounds.set_description(Some(
            "The same sliders as on the Sounds page, which also has the overall volume and every other sound.",
        ));
        for event in [
            crate::sounds::SoundEvent::Message,
            crate::sounds::SoundEvent::Mention,
        ] {
            let row = adw::ActionRow::builder().title(event.title()).build();
            row.add_suffix(&self.sound_level_scale(event));
            row.add_suffix(&self.sound_preview_button(event));
            sounds.add(&row);
        }
        page.append(&sounds);

        let what = adw::PreferencesGroup::new();
        what.set_title("What Notifies");
        what.add(&self.pref_switch(
            "All Messages in Direct Messages",
            "Every message in a DM or group DM. Off, only messages that mention you there.",
            |prefs| prefs.notify_all_dms,
            |prefs, on| prefs.notify_all_dms = on,
        ));
        what.add(&self.pref_switch(
            "Only Mentions in Servers",
            "Ignore servers and channels set to All Messages: only mentions and replies that ping you.",
            |prefs| prefs.notify_mentions_only,
            |prefs, on| prefs.notify_mentions_only = on,
        ));
        what.add(&self.pref_switch(
            "@everyone and @here",
            "Unless the server's own settings suppress them.",
            |prefs| prefs.notify_everyone,
            |prefs, on| prefs.notify_everyone = on,
        ));
        what.add(&self.pref_switch(
            "Role Mentions",
            "Mentions of a role you have, unless the server's own settings suppress them.",
            |prefs| prefs.notify_roles,
            |prefs, on| prefs.notify_roles = on,
        ));
        what.add(&self.pref_switch(
            "Incoming Calls",
            "When someone calls you in a DM or group, while kubecord's window is not in front.",
            |prefs| prefs.notify_calls,
            |prefs, on| prefs.notify_calls = on,
        ));
        what.add(&self.open_channel_quiet_row());
        page.append(&what);

        let window = adw::PreferencesGroup::new();
        window.set_title("Window");
        window.add(&self.pref_switch(
            "Count New Notifications in the Window Title",
            "Shows \"(2) kubecord\" until you come back to the window; most taskbars show the title. GTK 4 cannot flash a window or set an urgency hint, so this stands in for it.",
            |prefs| prefs.notify_mark_window,
            |prefs, on| prefs.notify_mark_window = on,
        ));
        page.append(&window);
        page
    }

    // -- sounds --------------------------------------------------------------

    /// A 0-100 % slider for one sound's own volume. Every slider for the same
    /// sound (the Sounds page row and its copy on the Notifications page)
    /// follows the others, so they never disagree.
    fn sound_level_scale(self: &Rc<Self>, event: crate::sounds::SoundEvent) -> gtk::Scale {
        let key = event.key();
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        scale.set_value(f64::from(self.capture.borrow().sound_level(key)) * 100.0);
        scale.set_draw_value(true);
        scale.set_value_pos(gtk::PositionType::Right);
        scale.set_format_value_func(|_, value| format!("{value:.0}%"));
        scale.set_width_request(160);
        scale.set_valign(gtk::Align::Center);
        scale.set_tooltip_text(Some(&format!("{} volume", event.title())));
        SOUND_SCALES.with_borrow_mut(|scales| {
            scales.retain(|(_, weak)| weak.upgrade().is_some());
            scales.push((key, scale.downgrade()));
        });
        let app = self.clone();
        scale.connect_value_changed(move |scale| {
            let value = scale.value();
            let level = (value / 100.0) as f32;
            app.update_prefs(|prefs| {
                if level >= 1.0 {
                    prefs.sound_volumes.remove(key);
                } else {
                    prefs.sound_volumes.insert(key.to_owned(), level);
                }
            });
            // Move the other sliders for this sound (collected first: setting
            // one runs its own handler, which reads the list again).
            let others: Vec<gtk::Scale> = SOUND_SCALES.with_borrow(|scales| {
                scales
                    .iter()
                    .filter(|(other_key, _)| *other_key == key)
                    .filter_map(|(_, weak)| weak.upgrade())
                    .filter(|other| other != scale)
                    .collect()
            });
            for other in others {
                if (other.value() - value).abs() > f64::EPSILON {
                    other.set_value(value);
                }
            }
        });
        scale
    }

    /// A button that previews one sound (a looping ring plays for 4 s).
    fn sound_preview_button(self: &Rc<Self>, event: crate::sounds::SoundEvent) -> gtk::Button {
        let preview = gtk::Button::from_icon_name("media-playback-start-symbolic");
        preview.add_css_class("flat");
        preview.set_valign(gtk::Align::Center);
        preview.set_tooltip_text(Some("Play"));
        let app = self.clone();
        preview.connect_clicked(move |_| {
            if let Some(sound) = app.play_sound_anyway(event) {
                glib::timeout_add_seconds_local_once(4, move || sound.stop());
            }
        });
        preview
    }

    /// Interface sounds: on or off, how loud, where they come from, and
    /// each one with a preview.
    fn sounds_page(self: &Rc<Self>) -> gtk::Box {
        use crate::sounds::SoundEvent;

        let page = page_box();
        page.append(&section_title("Sounds"));

        let general = adw::PreferencesGroup::new();
        general.add(&self.pref_switch(
            "Play Sounds",
            "Notification, call and voice sounds. They play on the speaker chosen on the Voice page.",
            |prefs| prefs.sounds_enabled,
            |prefs, on| prefs.sounds_enabled = on,
        ));
        let volume_row = adw::ActionRow::builder().title("Volume").build();
        let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        volume.set_value(f64::from(self.capture.borrow().sound_volume) * 100.0);
        volume.set_draw_value(true);
        volume.set_value_pos(gtk::PositionType::Right);
        volume.set_format_value_func(|_, value| format!("{value:.0}%"));
        volume.set_hexpand(true);
        volume.set_width_request(220);
        volume.set_valign(gtk::Align::Center);
        let app = self.clone();
        volume.connect_value_changed(move |scale| {
            let volume = (scale.value() / 100.0) as f32;
            app.update_prefs(|prefs| prefs.sound_volume = volume);
        });
        volume_row.add_suffix(&volume);
        general.add(&volume_row);
        page.append(&general);

        let source = adw::PreferencesGroup::new();
        source.set_title("Discord's Sounds");
        source.set_description(Some(
            "kubecord does not ship Discord's sounds: it downloads them from discord.com's public web app into its cache (no account details are sent). Until then, or if they cannot be found, short built-in tones play.",
        ));
        let status = adw::ActionRow::builder()
            .title("Downloaded Sounds")
            .subtitle(self.sounds.download.borrow().describe())
            .subtitle_lines(0)
            .build();
        let refresh = gtk::Button::with_label("Refresh Sounds");
        refresh.set_valign(gtk::Align::Center);
        refresh.set_tooltip_text(Some(
            "Looks up Discord's sounds again and downloads any that changed",
        ));
        let app = self.clone();
        refresh.connect_clicked(move |_| app.fetch_sounds(true));
        status.add_suffix(&refresh);
        source.add(&status);
        let folder = crate::sounds::override_dir();
        let own = adw::ActionRow::builder()
            .title("Your Own Sounds")
            .subtitle(format!(
                "A file named after a sound replaces it: {}/mention.ogg, for example (mp3, ogg, wav, flac or opus).",
                folder
                    .as_ref()
                    .map_or_else(|| "?".to_owned(), |dir| dir.display().to_string())
            ))
            .subtitle_lines(0)
            .build();
        let open = gtk::Button::with_label("Open Folder");
        open.set_valign(gtk::Align::Center);
        let window = self.window.clone();
        open.connect_clicked(move |_| {
            if let Some(dir) = &folder {
                let _ = std::fs::create_dir_all(dir);
                gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(dir))).launch(
                    Some(&window),
                    None::<&gtk::gio::Cancellable>,
                    |_| {},
                );
            }
        });
        own.add_suffix(&open);
        source.add(&own);
        page.append(&source);

        let events = adw::PreferencesGroup::new();
        events.set_title("Each Sound");
        let mut rows = Vec::new();
        for event in SoundEvent::ALL {
            let row = adw::ActionRow::builder().title(event.title()).build();
            row.add_suffix(&self.sound_level_scale(event));
            row.add_suffix(&self.sound_preview_button(event));
            let switch = gtk::Switch::new();
            switch.set_valign(gtk::Align::Center);
            switch.set_active(
                !self
                    .capture
                    .borrow()
                    .sounds_off
                    .iter()
                    .any(|key| key == event.key()),
            );
            let app = self.clone();
            switch.connect_active_notify(move |switch| {
                let on = switch.is_active();
                app.update_prefs(|prefs| {
                    prefs.sounds_off.retain(|key| key != event.key());
                    if !on {
                        prefs.sounds_off.push(event.key().to_owned());
                    }
                });
            });
            row.add_suffix(&switch);
            row.set_activatable_widget(Some(&switch));
            events.add(&row);
            rows.push((event, row.downgrade()));
        }
        page.append(&events);

        // Each row says where its sound comes from, and follows downloads.
        let show_sources = {
            let app = Rc::downgrade(self);
            move || {
                let Some(app) = app.upgrade() else {
                    return false;
                };
                let mut alive = false;
                for (event, row) in &rows {
                    if let (Some(row), Some(source)) = (row.upgrade(), app.sound_source(*event)) {
                        alive = true;
                        row.set_subtitle(&format!("{} · {}", source.describe(), event.key()));
                    }
                }
                alive
            }
        };
        show_sources();
        let status = status.downgrade();
        self.sounds
            .on_change
            .borrow_mut()
            .push(Box::new(move |download| {
                let Some(status) = status.upgrade() else {
                    return false;
                };
                status.set_subtitle(&download.describe());
                show_sources()
            }));
        page
    }

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

        let history = adw::PreferencesGroup::new();
        history.set_title("History");
        let unload = adw::SwitchRow::new();
        unload.set_title("Unload Distant Messages");
        unload.set_subtitle(
            "Free memory by dropping messages far from where you are reading; they reload when you scroll back. Turn off to keep every message you scroll into view.",
        );
        unload.set_subtitle_lines(0);
        unload.set_active(self.capture.borrow().unload_distant_messages);
        let app = self.clone();
        unload.connect_active_notify(move |row| {
            app.update_prefs(|prefs| prefs.unload_distant_messages = row.is_active());
        });
        history.add(&unload);
        page.append(&history);

        let gifs = adw::PreferencesGroup::new();
        gifs.set_title("GIFs");
        let unfocused = adw::SwitchRow::new();
        unfocused.set_title("Animate GIFs While Unfocused");
        unfocused.set_subtitle(
            "Keep GIFs moving when kubecord is not the focused window. Off by default; GIFs pause when unfocused to save work.",
        );
        unfocused.set_subtitle_lines(0);
        unfocused.set_active(self.capture.borrow().animate_gifs_unfocused);
        let app = self.clone();
        unfocused.connect_active_notify(move |row| {
            let on = row.is_active();
            // Apply live (update_prefs only re-applies device settings).
            crate::animation::set_animate_unfocused(on);
            app.update_prefs(|prefs| prefs.animate_gifs_unfocused = on);
        });
        gifs.add(&unfocused);
        page.append(&gifs);

        page.append(&self.spelling_group());
        page
    }

    /// The spell-check toggle and the manager for words added to the dictionary.
    fn spelling_group(self: &Rc<Self>) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::new();
        group.set_title("Spelling");

        let check = adw::SwitchRow::new();
        check.set_title("Check Spelling");
        check.set_subtitle("Underline misspelled words in the message box as you type.");
        check.set_subtitle_lines(0);
        check.set_active(self.capture.borrow().spellcheck);
        let app = self.clone();
        check.connect_active_notify(move |row| {
            let on = row.is_active();
            crate::spelling::set_enabled(on);
            app.update_prefs(|prefs| prefs.spellcheck = on);
            app.recheck_spelling();
        });
        group.add(&check);

        // The words the user has taught the checker, each removable.
        let added = adw::ExpanderRow::new();
        added.set_title("Added Words");
        let tracked: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

        // `repopulate` rebuilds the list; it is shared so the per-row remove
        // buttons (built inside it) can trigger a rebuild after they delete.
        let repopulate: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let builder: Rc<dyn Fn()> = {
            let added = added.clone();
            let tracked = tracked.clone();
            let app = self.clone();
            let repopulate = repopulate.clone();
            Rc::new(move || {
                for row in tracked.borrow_mut().drain(..) {
                    added.remove(&row);
                }
                let words = crate::spelling::custom_words();
                added.set_subtitle(&match words.len() {
                    0 => "None yet".to_owned(),
                    1 => "1 word".to_owned(),
                    n => format!("{n} words"),
                });
                for word in words {
                    let row = adw::ActionRow::new();
                    row.set_title(&glib::markup_escape_text(&word));
                    let remove = gtk::Button::from_icon_name("user-trash-symbolic");
                    remove.add_css_class("flat");
                    remove.set_valign(gtk::Align::Center);
                    remove.set_tooltip_text(Some("Remove"));
                    let app = app.clone();
                    let repopulate = repopulate.clone();
                    let word = word.clone();
                    remove.connect_clicked(move |_| {
                        crate::spelling::remove_word(&word);
                        app.recheck_spelling();
                        if let Some(run) = repopulate.borrow().as_ref() {
                            run();
                        }
                    });
                    row.add_suffix(&remove);
                    added.add_row(&row);
                    tracked.borrow_mut().push(row);
                }
            })
        };
        *repopulate.borrow_mut() = Some(builder.clone());
        builder();

        // An entry to add a new word.
        let add = adw::EntryRow::new();
        add.set_title("Add a Word");
        add.set_show_apply_button(true);
        let app = self.clone();
        let builder_apply = builder.clone();
        add.connect_apply(move |entry| {
            let text = entry.text();
            let word = text.trim();
            if !word.is_empty() {
                crate::spelling::add_word(word);
                entry.set_text("");
                app.recheck_spelling();
                builder_apply();
            }
        });
        group.add(&add);
        group.add(&added);
        group
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

    // -- theming -------------------------------------------------------------

    /// Light/dark and the typeface.
    fn theming_page(self: &Rc<Self>) -> gtk::Box {
        let page = page_box();
        page.append(&section_title("Theming"));

        let colors = adw::PreferencesGroup::new();
        colors.set_title("Colors");
        let follow = adw::SwitchRow::new();
        follow.set_title("Follow System Color Scheme");
        follow.set_subtitle(
            "Use your system's libadwaita colors and its light or dark preference. Off, kubecord uses Discord's own colors.",
        );
        follow.set_subtitle_lines(0);
        follow.set_active(self.capture.borrow().follow_system_scheme);
        let scheme = adw::ComboRow::new();
        scheme.set_title("Discord Theme");
        scheme.set_subtitle("Used while not following the system's colors.");
        scheme.set_model(Some(&gtk::StringList::new(&["Dark", "Light"])));
        scheme.set_selected(if self.capture.borrow().dark_scheme { 0 } else { 1 });
        scheme.set_sensitive(!self.capture.borrow().follow_system_scheme);
        let app = self.clone();
        let picker = scheme.clone();
        follow.connect_active_notify(move |row| {
            let on = row.is_active();
            picker.set_sensitive(!on);
            app.update_prefs(|prefs| prefs.follow_system_scheme = on);
        });
        let app = self.clone();
        scheme.connect_selected_notify(move |row| {
            let dark = row.selected() == 0;
            app.update_prefs(|prefs| prefs.dark_scheme = dark);
        });
        colors.add(&follow);
        colors.add(&scheme);
        page.append(&colors);

        let text = adw::PreferencesGroup::new();
        text.set_title("Text");
        let font = adw::SwitchRow::new();
        font.set_title("Use Discord's Font");
        font.set_subtitle(
            "gg sans, as in the official app. kubecord does not ship it: turning this on downloads it once from discord.com's public web app into the cache (no account details are sent).",
        );
        font.set_subtitle_lines(0);
        font.set_active(self.capture.borrow().discord_font);
        let app = self.clone();
        font.connect_active_notify(move |row| {
            let on = row.is_active();
            app.update_prefs(|prefs| prefs.discord_font = on);
            app.apply_discord_font(on);
        });
        text.add(&font);
        page.append(&text);
        page
    }

    // -- advanced ------------------------------------------------------------

    /// Discord's checks for the official app, and what kubecord logs.
    fn advanced_page(self: &Rc<Self>) -> gtk::Box {
        let page = page_box();
        page.append(&section_title("Advanced"));

        // Compatibility: how kubecord handles the checks Discord makes for
        // the official app.
        let group = adw::PreferencesGroup::new();
        group.set_title("Captchas");
        group.set_description(Some(
            "Discord sometimes asks for a captcha, for example before it sends a friend request.",
        ));
        let in_app = adw::SwitchRow::new();
        in_app.set_title("Solve Captchas in kubecord");
        in_app.set_subtitle(if crate::captcha::AVAILABLE {
            "Show the captcha here and send the request again once it is solved. When this is off, or the captcha fails, you can finish in your browser instead."
        } else {
            "This build of kubecord was made without the captcha view, so captchas are finished in your browser."
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

        let diagnostics = adw::SwitchRow::new();
        diagnostics.set_title("Performance Diagnostics");
        diagnostics.set_subtitle(
            "Logs when the interface stalls and how long the sidebar takes to redraw, to track down a hitch. Harmless and off by default.",
        );
        diagnostics.set_subtitle_lines(0);
        diagnostics.set_active(self.capture.borrow().log_diagnostics);
        let app = self.clone();
        diagnostics.connect_active_notify(move |row| {
            let active = row.is_active();
            app.update_prefs(|prefs| prefs.log_diagnostics = active);
        });
        group.add(&diagnostics);

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
            "Mic Test plays your microphone back to you as a call would send it. Microphone and volume changes apply right away, also in a call; a new speaker is used from the next call.",
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

        page.append(&self.sensitivity_group(panel));
        page.append(&self.processing_group());

        let people = adw::PreferencesGroup::new();
        people.set_title("Other People");
        people.add(&self.volume_boost_row());
        page.append(&people);
        page
    }

    /// Input sensitivity: automatic, or a threshold slider drawn over a
    /// live level meter, so you can see where your voice sits.
    fn sensitivity_group(self: &Rc<Self>, panel: &Rc<Panel>) -> adw::PreferencesGroup {
        let current = self.capture.borrow().clone();
        let group = adw::PreferencesGroup::new();
        group.set_title("Input Sensitivity");
        group.set_description(Some(
            "Only sound above the line is sent; your speaking ring lights with it. The bar shows your microphone while you talk in a call or during Mic Test: green is sent, amber is not.",
        ));

        let auto = adw::SwitchRow::new();
        auto.set_title("Automatically Determine Input Sensitivity");
        auto.set_subtitle("kubecord learns how loud your room is and sends what rises above it.");
        auto.set_subtitle_lines(0);
        auto.set_active(current.auto_input_sensitivity);
        group.add(&auto);

        let meter = gtk::DrawingArea::new();
        meter.set_content_height(SENSITIVITY_HEIGHT);
        meter.set_hexpand(true);
        meter.set_valign(gtk::Align::Center);
        let scale = gtk::Scale::with_range(
            gtk::Orientation::Horizontal,
            f64::from(crate::settings::THRESHOLD_MIN_DB),
            f64::from(crate::settings::THRESHOLD_MAX_DB),
            1.0,
        );
        scale.add_css_class("kc-sensitivity");
        scale.set_value(f64::from(current.input_threshold_db));
        scale.set_draw_value(true);
        scale.set_value_pos(gtk::PositionType::Right);
        scale.set_format_value_func(|_, value| format!("{value:.0} dB"));
        scale.set_hexpand(true);
        scale.set_tooltip_text(Some("Sound quieter than this is not sent"));
        scale.set_sensitive(!current.auto_input_sensitivity);
        // The meter is drawn under the slider, lined up with its track.
        let stack = gtk::Overlay::new();
        stack.set_child(Some(&meter));
        stack.add_overlay(&scale);
        stack.set_measure_overlay(&scale, true);
        let hint = gtk::Label::new(Some(
            "Start Mic Test above, or join a call, to see your level.",
        ));
        hint.set_xalign(0.0);
        hint.set_wrap(true);
        hint.add_css_class("dim-label");
        hint.add_css_class("caption");
        let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
        column.set_margin_top(10);
        column.set_margin_bottom(10);
        column.set_margin_start(12);
        column.set_margin_end(12);
        column.append(&stack);
        column.append(&hint);
        let row = adw::PreferencesRow::new();
        row.set_activatable(false);
        row.set_child(Some(&column));
        group.add(&row);

        let shown = Rc::new(Cell::new(MeterShown::default()));
        {
            let shown = shown.clone();
            let scale = scale.clone();
            let auto = auto.clone();
            meter.set_draw_func(move |area, cr, width, height| {
                draw_sensitivity(
                    area,
                    cr,
                    width,
                    height,
                    shown.get(),
                    &scale,
                    auto.is_active(),
                );
            });
        }
        let app = self.clone();
        let redraw = meter.clone();
        scale.connect_value_changed(move |scale| {
            let threshold = scale.value() as f32;
            app.update_prefs(|prefs| prefs.input_threshold_db = threshold);
            redraw.queue_draw();
        });
        let app = self.clone();
        let target = scale.clone();
        let redraw = meter.clone();
        auto.connect_active_notify(move |row| {
            let active = row.is_active();
            target.set_sensitive(!active);
            app.update_prefs(|prefs| prefs.auto_input_sensitivity = active);
            redraw.queue_draw();
        });

        // Follow the microphone while the panel is open.
        let alive = Rc::downgrade(panel);
        let meter = meter.downgrade();
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let (Some(_panel), Some(meter)) = (alive.upgrade(), meter.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            let level = demo_level().or_else(kc_media::processing::input_level);
            hint.set_visible(level.is_none());
            let mut now = shown.get();
            now.follow(level);
            shown.set(now);
            meter.queue_draw();
            glib::ControlFlow::Continue
        });
        group
    }

    /// Noise suppression, echo cancellation and automatic gain.
    fn processing_group(self: &Rc<Self>) -> adw::PreferencesGroup {
        let current = self.capture.borrow().clone();
        let group = adw::PreferencesGroup::new();
        group.set_title("Voice Processing");
        group.set_description(Some(
            "Applied to your microphone before it is sent, and in Mic Test. Changes apply at once, also in a call.",
        ));

        let noise = adw::ComboRow::new();
        noise.set_title("Noise Suppression");
        noise.set_subtitle(
            "Removes steady background noise. Standard keeps your voice natural; Strong (RNNoise) also takes out keyboards and fans and costs a little more processor time.",
        );
        noise.set_subtitle_lines(0);
        noise.set_model(Some(&gtk::StringList::new(
            &crate::settings::NOISE_SUPPRESSION,
        )));
        noise.set_selected(current.noise_suppression.min(2));
        let app = self.clone();
        noise.connect_selected_notify(move |row| {
            let level = row.selected();
            app.update_prefs(|prefs| prefs.noise_suppression = level);
        });
        group.add(&noise);

        let echo = adw::SwitchRow::new();
        echo.set_title("Echo Cancellation");
        echo.set_subtitle(
            "Keeps others from hearing themselves when you use speakers. Not needed with headphones. Works in calls (it listens to the call's sound).",
        );
        echo.set_subtitle_lines(0);
        echo.set_active(current.echo_cancellation);
        let app = self.clone();
        echo.connect_active_notify(move |row| {
            let active = row.is_active();
            app.update_prefs(|prefs| prefs.echo_cancellation = active);
        });
        group.add(&echo);

        let gain = adw::SwitchRow::new();
        gain.set_title("Automatic Gain Control");
        gain.set_subtitle(
            "Evens out your volume when you move closer to or away from the microphone.",
        );
        gain.set_subtitle_lines(0);
        gain.set_active(current.auto_gain_control);
        let app = self.clone();
        gain.connect_active_notify(move |row| {
            let active = row.is_active();
            app.update_prefs(|prefs| prefs.auto_gain_control = active);
        });
        group.add(&gain);
        group
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

// -- input sensitivity -------------------------------------------------------

/// Height of the input sensitivity meter.
const SENSITIVITY_HEIGHT: i32 = 14;

/// What the sensitivity meter shows, smoothed for the eye.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MeterShown {
    /// The level drawn (dBFS), or `None` with nothing to show.
    level: Option<f32>,
    open: bool,
    /// Where automatic sensitivity currently draws its line.
    auto_threshold: Option<f32>,
}

impl MeterShown {
    /// Rises at once, falls back gently, so speech is easy to follow.
    fn follow(&mut self, sample: Option<kc_media::processing::InputLevel>) {
        let Some(sample) = sample else {
            *self = Self::default();
            return;
        };
        let fallen = self.level.map_or(sample.level_db, |level| level - 1.5);
        self.level = Some(sample.level_db.max(fallen));
        self.open = sample.open;
        self.auto_threshold = Some(sample.threshold_db);
    }
}

/// Where `db` falls across the meter, from 0 to 1.
fn meter_fraction(db: f32) -> f64 {
    let min = crate::settings::THRESHOLD_MIN_DB;
    let max = crate::settings::THRESHOLD_MAX_DB;
    f64::from(((db - min) / (max - min)).clamp(0.0, 1.0))
}

/// Draws the level under the slider's track: amber up to the threshold,
/// green past it, with a line at the threshold in use.
fn draw_sensitivity(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    shown: MeterShown,
    scale: &gtk::Scale,
    automatic: bool,
) {
    // Line the meter up with the slider's trough.
    let mut trough = scale.first_child();
    while let Some(child) = &trough {
        if child.css_name() == "trough" {
            break;
        }
        trough = child.next_sibling();
    }
    let (left, track) = trough
        .and_then(|trough| {
            let bounds = trough.compute_bounds(area)?;
            Some((f64::from(bounds.x()), f64::from(bounds.width())))
        })
        .unwrap_or((0.0, f64::from(width)));
    let top = (f64::from(height) - f64::from(SENSITIVITY_HEIGHT)) / 2.0;
    let bar = f64::from(SENSITIVITY_HEIGHT);
    let foreground = area.color();
    rounded(cr, left, top, track, bar);
    cr.set_source_rgba(
        f64::from(foreground.red()),
        f64::from(foreground.green()),
        f64::from(foreground.blue()),
        0.12,
    );
    let _ = cr.fill();

    let threshold = if automatic {
        shown.auto_threshold
    } else {
        Some(scale.value() as f32)
    };
    if let Some(level) = shown.level {
        let reach = meter_fraction(level) * track;
        let cut = threshold.map_or(0.0, |threshold| meter_fraction(threshold) * track);
        let _ = cr.save();
        rounded(cr, left, top, track, bar);
        cr.clip();
        // Below the threshold: amber; above it: green (sent).
        cr.set_source_rgb(0.98, 0.65, 0.10);
        cr.rectangle(left, top, reach.min(cut), bar);
        let _ = cr.fill();
        if reach > cut {
            if shown.open {
                cr.set_source_rgb(0.23, 0.65, 0.36);
            } else {
                cr.set_source_rgb(0.55, 0.55, 0.55);
            }
            cr.rectangle(left + cut, top, reach - cut, bar);
            let _ = cr.fill();
        }
        let _ = cr.restore();
    }
    if automatic && let Some(threshold) = threshold {
        let x = left + meter_fraction(threshold) * track;
        cr.set_source_rgba(
            f64::from(foreground.red()),
            f64::from(foreground.green()),
            f64::from(foreground.blue()),
            0.8,
        );
        cr.rectangle(x - 1.0, top - 2.0, 2.0, bar + 4.0);
        let _ = cr.fill();
    }
}

fn rounded(cr: &gtk::cairo::Context, x: f64, y: f64, width: f64, height: f64) {
    let radius = height / 2.0;
    cr.new_sub_path();
    cr.arc(
        x + width - radius,
        y + radius,
        radius,
        -std::f64::consts::FRAC_PI_2,
        std::f64::consts::FRAC_PI_2,
    );
    cr.arc(
        x + radius,
        y + radius,
        radius,
        std::f64::consts::FRAC_PI_2,
        3.0 * std::f64::consts::FRAC_PI_2,
    );
    cr.close_path();
}

/// Development aid: `KUBECORD_DEMO_LEVEL=<dB>` shows a made-up microphone
/// level on the sensitivity meter, for screenshots without a microphone.
fn demo_level() -> Option<kc_media::processing::InputLevel> {
    let level: f32 = std::env::var("KUBECORD_DEMO_LEVEL").ok()?.parse().ok()?;
    Some(kc_media::processing::InputLevel {
        level_db: level,
        open: level >= -50.0,
        threshold_db: -50.0,
    })
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

/// Development aid: `KUBECORD_SETTINGS_SCROLL=<pixels>` scrolls settings
/// pages and popups down once they are laid out, for screenshots of long
/// ones.
pub(crate) fn dev_scroll(scroll: &gtk::ScrolledWindow) {
    if let Some(offset) = std::env::var("KUBECORD_SETTINGS_SCROLL")
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
    fn the_meter_rises_at_once_and_falls_gently() {
        let sample = |level_db| kc_media::processing::InputLevel {
            level_db,
            open: true,
            threshold_db: -50.0,
        };
        let mut shown = MeterShown::default();
        shown.follow(Some(sample(-20.0)));
        assert_eq!(shown.level, Some(-20.0));
        shown.follow(Some(sample(-80.0)));
        assert_eq!(shown.level, Some(-21.5));
        shown.follow(Some(sample(-10.0)));
        assert_eq!(shown.level, Some(-10.0));
        shown.follow(None);
        assert_eq!(shown, MeterShown::default());
        assert_eq!(meter_fraction(-100.0), 0.0);
        assert_eq!(meter_fraction(-50.0), 0.5);
        assert_eq!(meter_fraction(10.0), 1.0);
    }

    #[test]
    fn long_device_names_are_shortened() {
        assert_eq!(shorten("short", 10), "short");
        assert_eq!(shorten("abcdefghijkl", 6), "abcde…");
    }
}
