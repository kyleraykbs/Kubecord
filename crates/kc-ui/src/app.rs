//! The libadwaita application shell: login page, guild rail, channel sidebar,
//! message list and composer.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::{Account, AccountStore, Snowflake, Token};
use kc_media::portal::{CameraAccess, CameraChoice};
use kc_state::{Channel, StoreEvent};

use crate::session::{
    self, AccountId, CameraPurpose, Command, ConnState, Session, UiEvent, VoiceUiState,
};
use crate::settings::{SIDEBAR_MAX, SIDEBAR_MIN};
use crate::shell::AppState;
use crate::widgets::{clear_box, label, status_dot};

pub(crate) const TYPING_TIMEOUT_SECS: u32 = 10;
pub(crate) const TYPING_THROTTLE: Duration = Duration::from_secs(8);

/// Builds the libadwaita application. Call [`run`] from the process entry point.
pub fn build_app() -> adw::Application {
    // A screenshot run must not hand over to (and quit in favour of) an
    // instance that is already open.
    let flags = if std::env::var_os("KYLECORD_SCREENSHOT").is_some() {
        gtk::gio::ApplicationFlags::NON_UNIQUE
    } else {
        gtk::gio::ApplicationFlags::empty()
    };
    adw::Application::builder()
        .application_id("dev.kyle.kylecord")
        .flags(flags)
        .build()
}

/// Runs the GTK main loop with a background Tokio runtime.
pub fn run() -> glib::ExitCode {
    match kc_media::video::preload_pipewire() {
        Ok(()) => tracing::info!("gstreamer pipewire plugin preloaded"),
        Err(error) => tracing::warn!(%error, "could not preload pipewire plugin"),
    }
    let app = build_app();

    let runtime = std::sync::Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime"),
    );
    let (ui_tx, ui_rx) = async_channel::unbounded::<UiEvent>();
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<Command>();
    session::spawn_dispatcher(&runtime, cmd_rx, ui_tx.clone());
    let runtime_for_ui = runtime.clone();
    let ui_tx_for_ui = ui_tx.clone();

    let state_slot: Rc<RefCell<Option<Rc<AppState>>>> = Rc::new(RefCell::new(None));
    let ui_rx_slot = Rc::new(RefCell::new(Some(ui_rx)));
    let cmd_tx_for_activate = cmd_tx;

    app.connect_activate(move |app| {
        if let Some(state) = state_slot.borrow().as_ref() {
            state.window.present();
            return;
        }

        let accounts = match AccountStore::load() {
            Ok(accounts) => accounts,
            Err(error) => {
                tracing::error!(%error, "could not load accounts");
                AccountStore::default()
            }
        };
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("kylecord")
            .default_width(1100)
            // Development aid: `KYLECORD_WINDOW_HEIGHT` makes the window
            // taller, so a screenshot shows more of a long page.
            .default_height(
                std::env::var("KYLECORD_WINDOW_HEIGHT")
                    .ok()
                    .and_then(|height| height.parse().ok())
                    .unwrap_or(720),
            )
            .build();
        let state = AppState::new(
            window,
            accounts,
            cmd_tx_for_activate.clone(),
            runtime_for_ui.clone(),
            ui_tx_for_ui.clone(),
        );
        state.connect_signals();

        let receiver = ui_rx_slot
            .borrow_mut()
            .take()
            .expect("activation happens once");
        let ui_state = state.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = receiver.recv().await {
                ui_state.handle_event(event);
            }
        });

        state.start_existing();
        state.window.present();

        let identifier_state = state.clone();
        glib::MainContext::default().spawn_local(async move {
            let identifier = ashpd::WindowIdentifier::from_native(&identifier_state.window).await;
            tracing::info!(
                available = identifier.is_some(),
                "portal parent window identifier"
            );
            *identifier_state.window_identifier.borrow_mut() = identifier.map(Arc::new);
        });

        // Development aids: `KYLECORD_OPEN=<channel id>` opens a channel
        // once the account is ready, and `KYLECORD_SCREENSHOT=<path>` saves
        // the window as a PNG a few seconds later and quits.
        if let Some(channel_id) = std::env::var("KYLECORD_OPEN")
            .ok()
            .and_then(|id| id.parse::<Snowflake>().ok())
        {
            let app_state = state.clone();
            glib::timeout_add_local_once(Duration::from_millis(3500), move || {
                let channel = app_state
                    .with_session(|session| {
                        session
                            .store
                            .read(|state| state.channels.get(&channel_id).cloned())
                    })
                    .flatten();
                if let Some(channel) = channel {
                    app_state.open_channel(channel);
                }
            });
        }
        // Keeps the information panels on videos current.
        let info_state = state.clone();
        glib::timeout_add_seconds_local(1, move || {
            info_state.refresh_video_info();
            glib::ControlFlow::Continue
        });
        if let Ok(mode) = std::env::var("KYLECORD_VIDEO_DEMO") {
            let app_state = state.clone();
            glib::timeout_add_local_once(Duration::from_millis(4000), move || {
                app_state.show_video_demo(&mode);
            });
        }
        if let Ok(mode) = std::env::var("KYLECORD_DEMO") {
            let app_state = state.clone();
            glib::timeout_add_local_once(Duration::from_millis(1000), move || {
                app_state.show_demo(&mode);
            });
        }
        if let Ok(popup) = std::env::var("KYLECORD_POPUP") {
            let app_state = state.clone();
            glib::timeout_add_local_once(Duration::from_millis(4000), move || {
                // The share popup needs no account until Go Live is pressed,
                // so it can be looked at without signing in.
                if popup == "share" && app_state.active().is_none() {
                    app_state.show_share_popup(0);
                    return;
                }
                let Some((account, _, store)) = app_state.active() else {
                    return;
                };
                match popup.as_str() {
                    "camera" => app_state.request_cameras(account, CameraPurpose::Camera),
                    "share" => app_state.show_share_popup(account),
                    "captcha" => app_state.test_captcha(),
                    // A DM call shown from a local CALL_CREATE (nothing is
                    // sent); `call-dm-end` then ends it with CALL_DELETE.
                    "call-dm" | "call-dm-end" => {
                        let found = store.read(|state| {
                            state
                                .relationships
                                .values()
                                .find(|relationship| {
                                    relationship.kind == kc_state::Relationship::FRIEND
                                })
                                .and_then(|relationship| {
                                    Some((relationship.id, state.dm_with(relationship.id)?.clone()))
                                })
                        });
                        if let Some((friend, channel)) = found {
                            let channel_id = channel.id;
                            app_state.open_channel(channel);
                            store.apply(
                                "CALL_CREATE",
                                &serde_json::json!({
                                    "channel_id": channel_id.to_string(),
                                    "voice_states": [{
                                        "channel_id": channel_id.to_string(),
                                        "user_id": friend.to_string(),
                                    }],
                                }),
                            );
                            app_state.refresh_voice_view();
                            if popup == "call-dm-end" {
                                let ended = app_state.clone();
                                glib::timeout_add_local_once(
                                    Duration::from_millis(800),
                                    move || {
                                        store.apply(
                                        "CALL_DELETE",
                                        &serde_json::json!({"channel_id": channel_id.to_string()}),
                                    );
                                        ended.refresh_voice_view();
                                    },
                                );
                            }
                        }
                    }
                    // The incoming call card for the first friend's DM;
                    // local only.
                    "ringing" => {
                        let channel = store.read(|state| {
                            state
                                .relationships
                                .values()
                                .find(|relationship| {
                                    relationship.kind == kc_state::Relationship::FRIEND
                                })
                                .and_then(|relationship| state.dm_with(relationship.id))
                                .map(|channel| channel.id)
                        });
                        if let Some(channel) = channel {
                            app_state.show_incoming_call(channel);
                        }
                    }
                    // The signal bars and ping graph with made-up samples.
                    "ping" => {
                        let now = glib::DateTime::now_local()
                            .map(|now| now.to_unix())
                            .unwrap_or(0);
                        {
                            let mut history = app_state.ping_history.borrow_mut();
                            for index in 0..48i64 {
                                let millis = match index {
                                    20 => Some(96),
                                    33 => None,
                                    _ => Some(22 + (index * 7 % 9) as u32),
                                };
                                history.push_back((now - (47 - index) * 5, millis));
                            }
                        }
                        app_state.note_ping(Some(24));
                        app_state.voice_panel.set_visible(true);
                        let panel = app_state.clone();
                        glib::timeout_add_local_once(Duration::from_millis(600), move || {
                            if let Some(popover) = panel.ping.button.popover() {
                                popover.set_autohide(false);
                                panel.ping.button.popup();
                                if let Ok(path) = std::env::var("KYLECORD_SCREENSHOT") {
                                    glib::timeout_add_local_once(
                                        Duration::from_millis(1200),
                                        move || {
                                            let _ = save_window_png(
                                                &popover,
                                                std::path::Path::new(&format!(
                                                    "{path}.popover.png"
                                                )),
                                            );
                                        },
                                    );
                                }
                            }
                        });
                    }
                    "notice" => {
                        app_state.notify("You're typing unseen", None, 30);
                        app_state.notify(
                            "Incoming call from Latte",
                            Some(("Join", Box::new(|| {}))),
                            30,
                        );
                    }
                    // The emoji picker in the first server, or in a DM; the
                    // popover is its own surface, saved beside the window
                    // screenshot as `<path>.popover.png`.
                    "emoji" | "emoji-dm" => {
                        if popup == "emoji" {
                            if let Some(guild) =
                                store.read(|state| state.guilds.keys().next().copied())
                            {
                                app_state.select_guild(guild);
                            }
                        } else if let Some(channel) = store.read(|state| {
                            state
                                .relationships
                                .values()
                                .find(|relationship| {
                                    relationship.kind == kc_state::Relationship::FRIEND
                                })
                                .and_then(|relationship| state.dm_with(relationship.id).cloned())
                        }) {
                            app_state.open_channel(channel);
                        }
                        let picker = app_state.clone();
                        glib::timeout_add_local_once(Duration::from_millis(600), move || {
                            // Without a real click there is no input grab to
                            // take, so the screenshot's popover must not hide.
                            if let Some(popover) = picker.emoji_button.popover() {
                                popover.set_autohide(false);
                            }
                            picker.emoji_button.popup();
                            if let Ok(path) = std::env::var("KYLECORD_SCREENSHOT") {
                                let popover = picker.emoji_button.popover();
                                glib::timeout_add_local_once(
                                    Duration::from_millis(1500),
                                    move || {
                                        if let Some(popover) = popover {
                                            let _ = save_window_png(
                                                &popover,
                                                std::path::Path::new(&format!(
                                                    "{path}.popover.png"
                                                )),
                                            )
                                            .map_err(|error| {
                                                eprintln!("popover screenshot: {error}")
                                            });
                                        }
                                    },
                                );
                            }
                        });
                    }
                    // Opens the first server and types into the composer.
                    "compose" | "compose-empty" => {
                        let guild = store.read(|state| state.guilds.keys().next().copied());
                        if let Some(guild) = guild {
                            app_state.select_guild(guild);
                        }
                        // Text for the layout check only: no typing notice
                        // goes to Discord.
                        app_state.with_session_mut(|session| {
                            session.last_typing = Some(std::time::Instant::now());
                        });
                        app_state.composer.buffer().set_text(if popup == "compose" {
                            "dfsdfsdfsdf"
                        } else {
                            ""
                        });
                    }
                    "profile-guild" => {
                        let guild = store.read(|state| state.guilds.keys().next().copied());
                        if let Some(guild) = guild {
                            app_state.select_guild(guild);
                        }
                        if let Some(me) = store.read(|state| state.my_id()) {
                            let anchor = app_state.window.clone().upcast::<gtk::Widget>();
                            app_state.show_profile(me, &anchor);
                        }
                    }
                    "profile-friend" => {
                        let (guild, friend) = store.read(|state| {
                            (
                                state.guilds.keys().next().copied(),
                                state
                                    .relationships
                                    .values()
                                    .find(|relationship| {
                                        relationship.kind == kc_state::Relationship::FRIEND
                                    })
                                    .map(|relationship| relationship.id),
                            )
                        });
                        if let Some(guild) = guild {
                            app_state.select_guild(guild);
                        }
                        if let Some(friend) = friend {
                            let anchor = app_state.window.clone().upcast::<gtk::Widget>();
                            app_state.show_profile(friend, &anchor);
                        }
                    }
                    _ => {
                        if let Some(me) = store.read(|state| state.my_id()) {
                            let anchor = app_state.window.clone().upcast::<gtk::Widget>();
                            app_state.show_profile(me, &anchor);
                        }
                    }
                }
            });
        }
        if let Ok(page) = std::env::var("KYLECORD_SETTINGS") {
            let app_state = state.clone();
            glib::timeout_add_local_once(Duration::from_millis(4000), move || {
                app_state.show_settings_page(&page);
            });
        }
        if let Ok(path) = std::env::var("KYLECORD_SCREENSHOT") {
            let window = state.window.clone();
            let app = app.clone();
            // `KYLECORD_SCREENSHOT_DELAY` (seconds) waits longer, for pages
            // that load from the network.
            let delay = std::env::var("KYLECORD_SCREENSHOT_DELAY")
                .ok()
                .and_then(|secs| secs.parse::<u64>().ok())
                .unwrap_or(7);
            glib::timeout_add_local_once(Duration::from_secs(delay), move || {
                if let Err(error) = save_window_png(&window, std::path::Path::new(&path)) {
                    eprintln!("could not save screenshot: {error}");
                }
                app.quit();
            });
        }

        *state_slot.borrow_mut() = Some(state);
    });

    let code = app.run();
    if let Ok(runtime) = std::sync::Arc::try_unwrap(runtime) {
        runtime.shutdown_timeout(Duration::from_millis(250));
    }
    code
}

/// Renders a widget to a PNG without relying on a compositor. Used by
/// `KYLECORD_SCREENSHOT` for visual checks of the GTK layout.
pub(crate) fn save_window_png(
    window: &(impl IsA<gtk::Native> + IsA<gtk::Widget>),
    path: &std::path::Path,
) -> Result<(), String> {
    let width = window.upcast_ref::<gtk::Widget>().width() as f64;
    let height = window.upcast_ref::<gtk::Widget>().height() as f64;
    if width <= 0.0 || height <= 0.0 {
        return Err("window has no size".into());
    }
    let paintable = gtk::WidgetPaintable::new(Some(window.upcast_ref::<gtk::Widget>()));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, width, height);
    let node = snapshot.to_node().ok_or("empty render node")?;
    let renderer = window.renderer().ok_or("no renderer")?;
    let texture = renderer.render_texture(&node, None);
    texture.save_to_png(path).map_err(|error| error.to_string())
}

impl AppState {
    pub(crate) fn connect_signals(self: &Rc<Self>) {
        let app = self.clone();
        self.login_button
            .connect_clicked(move |_| app.submit_login());
        let app = self.clone();
        self.token_entry
            .connect_activate(move |_| app.submit_login());
        let app = self.clone();
        self.label_entry
            .connect_activate(move |_| app.submit_login());
        let app = self.clone();
        self.login_cancel.connect_clicked(move |_| {
            app.stack.set_visible_child_name("chat");
        });
        // The banner's button is only offered for a signed-out account.
        let app = self.clone();
        self.banner.connect_button_clicked(move |_| {
            let active = app.model.borrow().active;
            if let Some(account) = active {
                app.show_signed_out(account);
            }
        });

        self.connect_chat();
        self.connect_composer();

        // Search runs on the server when the entry is submitted.
        let app = self.clone();
        self.search_entry.connect_activate(move |entry| {
            let query = entry.text().trim().to_owned();
            if !query.is_empty() {
                app.run_search(query);
            }
        });
        let app = self.clone();
        self.pins_button.connect_clicked(move |_| app.show_pins());

        let app = self.clone();
        self.dm_call_button
            .connect_clicked(move |_| app.toggle_dm_call());

        let app = self.clone();
        self.members_toggle.connect_toggled(move |button| {
            if app.members_guard.get() {
                return;
            }
            app.show_members.set(button.is_active());
            app.render_members();
        });

        // Reading resumes when the window regains focus at the bottom of a
        // channel.
        let app = self.clone();
        self.window.connect_is_active_notify(move |window| {
            // GIFs play only while the window is focused.
            app.queue_players_update();
            if window.is_active() && app.chat.borrow().at_bottom {
                let channel = app.chat.borrow().channel;
                if let Some(channel) = channel {
                    app.ack_channel(channel);
                }
            }
        });

        // Ctrl+K focuses search; Alt+Up/Down walk the channel list.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let app = self.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            use gtk::gdk::{Key, ModifierType};
            if modifiers.contains(ModifierType::CONTROL_MASK) && key == Key::k {
                app.search_entry.grab_focus();
                return glib::Propagation::Stop;
            }
            if modifiers.contains(ModifierType::ALT_MASK) && matches!(key, Key::Up | Key::Down) {
                app.step_channel(if key == Key::Up { -1 } else { 1 });
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(keys);

        let app = self.clone();
        self.voice_leave.connect_clicked(move |_| app.leave_voice());
        let app = self.clone();
        self.voice_mute
            .connect_toggled(move |_| app.on_mute_toggled());
        let app = self.clone();
        self.voice_deafen
            .connect_toggled(move |_| app.on_deafen_toggled());
        let app = self.clone();
        self.voice_camera
            .connect_toggled(move |_| app.on_camera_toggled());
        let app = self.clone();
        self.voice_screen
            .connect_toggled(move |_| app.on_screen_toggled());
        let app = self.clone();
        self.capture_button
            .connect_clicked(move |_| app.show_settings());

        self.connect_ping();
        self.render_typing_toggle();
        let app = self.clone();
        self.typing_toggle.connect_clicked(move |_| {
            let send = !app.capture.borrow().send_typing;
            app.set_send_typing(send);
            app.set_status(if send {
                "Others can see when you're typing"
            } else {
                "You're typing unseen"
            });
        });

        // Closing the window leaves any call first: hidden at once, it
        // waits for the calls to end (at most two seconds), then gives the
        // gateway a moment to send the leave before the app exits.
        // Otherwise Discord keeps showing you in the channel until the
        // dropped connection times out.
        let app = self.clone();
        let leaving = Rc::new(std::cell::Cell::new(false));
        self.window.connect_close_request(move |window| {
            app.close_popouts();
            app.close_aux_windows();
            if leaving.get() {
                return glib::Propagation::Proceed;
            }
            let in_call: Vec<AccountId> = app
                .model
                .borrow()
                .sessions
                .iter()
                .filter(|(_, session)| session.voice.is_some())
                .map(|(account, _)| *account)
                .collect();
            if in_call.is_empty() {
                return glib::Propagation::Proceed;
            }
            for account in in_call {
                let _ = app.cmd_tx.try_send(Command::LeaveVoice { account });
            }
            leaving.set(true);
            window.set_visible(false);
            let started = std::time::Instant::now();
            let state = app.clone();
            let window = window.clone();
            glib::timeout_add_local(Duration::from_millis(50), move || {
                let still = state
                    .model
                    .borrow()
                    .sessions
                    .values()
                    .any(|session| session.voice.is_some());
                if still && started.elapsed() < Duration::from_secs(2) {
                    return glib::ControlFlow::Continue;
                }
                let window = window.clone();
                glib::timeout_add_local_once(Duration::from_millis(300), move || window.close());
                glib::ControlFlow::Break
            });
            glib::Propagation::Stop
        });

        // Clicking outside a dialog closes it, as in Discord. The click is
        // only watched, never claimed: a dialog on screen already keeps
        // clicks from what is behind it, and claiming here once left every
        // button in the window dead when a dialog was wrongly thought open.
        let outside = gtk::GestureClick::new();
        outside.set_button(gtk::gdk::BUTTON_PRIMARY);
        outside.set_propagation_phase(gtk::PropagationPhase::Capture);
        let window = self.window.clone();
        outside.connect_pressed(move |gesture, _, x, y| {
            let Some(dialog) = window.visible_dialog() else {
                return;
            };
            // A click in a popover (a menu opened from the dialog) reaches
            // the window too, on the popover's own surface.
            let on_window = gesture
                .current_event()
                .and_then(|event| event.surface())
                .is_none_or(|surface| Some(surface) == window.surface());
            if !on_window {
                return;
            }
            let Some(card) = dialog.child() else {
                return;
            };
            if !dialog.is_mapped() || !card.is_mapped() {
                return;
            }
            let Some(bounds) = card.compute_bounds(&window) else {
                return;
            };
            if bounds.width() < 1.0 || bounds.height() < 1.0 {
                return;
            }
            let point = gtk::graphene::Point::new(x as f32, y as f32);
            if !bounds.contains_point(&point) {
                tracing::info!(title = %dialog.title(), "closing a dialog on a click outside it");
                dialog.close();
            }
        });
        self.window.add_controller(outside);

        // The call's height above a DM's chat is kept too, while both show.
        let app = self.clone();
        self.call_split.connect_position_notify(move |split| {
            if !(app.call_area.is_visible() && app.chat_box.is_visible()) {
                return;
            }
            let height = split.position();
            if height >= crate::settings::CALL_HEIGHT_MIN
                && height != app.capture.borrow().call_height
            {
                app.update_prefs(|prefs| prefs.call_height = height);
            }
        });

        // The sidebar width is kept between launches.
        let app = self.clone();
        self.sidebar_paned.connect_position_notify(move |paned| {
            let width = paned.position();
            if width > SIDEBAR_MAX {
                paned.set_position(SIDEBAR_MAX);
                return;
            }
            if width >= SIDEBAR_MIN && width != app.capture.borrow().sidebar_width {
                app.update_prefs(|prefs| prefs.sidebar_width = width);
            }
        });
    }

    /// Opens the previous or next channel in the sidebar.
    pub(crate) fn step_channel(self: &Rc<Self>, step: i32) {
        let ids = self.visible_channel_ids();
        if ids.is_empty() {
            return;
        }
        let current = self
            .with_session(|session| session.selected_channel)
            .flatten()
            .and_then(|selected| ids.iter().position(|id| *id == selected));
        let next = match current {
            Some(index) => (index as i32 + step).rem_euclid(ids.len() as i32) as usize,
            None => 0,
        };
        let channel = self
            .with_session(|session| {
                session
                    .store
                    .read(|state| state.channels.get(&ids[next]).cloned())
            })
            .flatten();
        if let Some(channel) = channel {
            self.open_channel(channel);
        }
    }

    // -- account lifecycle -----------------------------------------------

    pub(crate) fn start_existing(self: &Rc<Self>) {
        let accounts = self.model.borrow().accounts.accounts().to_vec();
        if accounts.is_empty() {
            self.stack.set_visible_child_name("login");
            self.account_button.set_visible(false);
            self.token_entry.grab_focus();
            return;
        }

        for account in &accounts {
            self.start_account(account.clone());
        }
        let first = session::account_id(&accounts[0].token);
        self.model.borrow_mut().active = Some(first);
        self.stack.set_visible_child_name("chat");
        self.render_all();
    }

    /// Creates the per-account session and asks Tokio to start its gateway.
    pub(crate) fn start_account(&self, account: Account) -> AccountId {
        let id = session::account_id(&account.token);
        let mut start = None;
        {
            let mut model = self.model.borrow_mut();
            if let Some(existing) = model.sessions.get_mut(&id) {
                existing.account = account;
            } else {
                let session = Session::new(account.clone());
                let store = session.store.clone();
                model.sessions.insert(id, session);
                start = Some((account.token.clone(), store));
            }
        }
        if let Some((token, store)) = start {
            let _ = self.cmd_tx.try_send(Command::StartAccount {
                account: id,
                token,
                store,
            });
        }
        id
    }

    pub(crate) fn switch_account(self: &Rc<Self>, id: AccountId) {
        if !self.model.borrow().sessions.contains_key(&id) {
            return;
        }
        self.model.borrow_mut().active = Some(id);
        self.chat.borrow_mut().channel = None;
        self.render_all();
    }

    pub(crate) fn account_name(&self, account: &Account) -> String {
        let id = session::account_id(&account.token);
        let from_session = {
            let model = self.model.borrow();
            model
                .sessions
                .get(&id)
                .and_then(|session| session.user.as_ref())
                .map(|user| user.display_name().to_owned())
        };
        from_session
            .or_else(|| {
                account
                    .label
                    .clone()
                    .filter(|label| !label.trim().is_empty())
            })
            .unwrap_or_else(|| account.token.redacted())
    }

    pub(crate) fn persist_account(&self, account: Account) -> Option<String> {
        let mut model = self.model.borrow_mut();
        model.accounts.upsert(account);
        model
            .accounts
            .save()
            .err()
            .map(|error| format!("could not save accounts: {error}"))
    }

    pub(crate) fn submit_login(self: &Rc<Self>) {
        let token_text = self.token_entry.text();
        let token_text = token_text.trim();
        if token_text.is_empty() {
            self.login_error.set_label("Enter a token");
            self.login_error.set_visible(true);
            return;
        }
        let label = {
            let text = self.label_entry.text();
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_owned())
        };
        self.login_error.set_visible(false);
        self.login_button.set_sensitive(false);
        let _ = self.cmd_tx.try_send(Command::Login {
            token: Token::new(token_text),
            label,
        });
    }

    pub(crate) fn show_login(self: &Rc<Self>) {
        self.login_error.set_visible(false);
        self.login_button.set_sensitive(true);
        self.stack.set_visible_child_name("login");
        self.token_entry.grab_focus();
        // Going back is only possible when an account is already signed in.
        let has_accounts = !self.model.borrow().accounts.is_empty();
        self.login_cancel.set_visible(has_accounts);
    }

    // -- navigation ------------------------------------------------------

    // -- voice -----------------------------------------------------------

    pub(crate) fn on_mute_toggled(self: &Rc<Self>) {
        let desired = self.voice_mute.is_active();
        let account = self.update_voice_flag(|voice| {
            if voice.muted == desired {
                None
            } else {
                voice.muted = desired;
                Some(desired)
            }
        });
        if let Some(account) = account {
            let _ = self.cmd_tx.try_send(Command::SetMute {
                account,
                muted: desired,
            });
            // Your own entry shows the change at once.
            self.queue_sidebar();
            self.refresh_voice_view();
        }
    }

    pub(crate) fn on_deafen_toggled(self: &Rc<Self>) {
        let desired = self.voice_deafen.is_active();
        let account = self.update_voice_flag(|voice| {
            if voice.deafened == desired {
                None
            } else {
                voice.deafened = desired;
                Some(desired)
            }
        });
        if let Some(account) = account {
            let _ = self.cmd_tx.try_send(Command::SetDeafen {
                account,
                deafened: desired,
            });
            // Your own entry shows the change at once.
            self.queue_sidebar();
            self.refresh_voice_view();
        }
    }

    pub(crate) fn on_camera_toggled(self: &Rc<Self>) {
        let desired = self.voice_camera.is_active();
        let camera_on = {
            let model = self.model.borrow();
            model
                .active
                .and_then(|account| model.sessions.get(&account))
                .and_then(|session| session.voice.as_ref())
                .is_some_and(|voice| voice.camera)
        };
        // A programmatic set_active follows this same signal. Only act when
        // the widget and the model disagree; in particular, a click while the
        // camera is running must stop it instead of reopening the picker.
        if camera_on == desired {
            return;
        }
        if desired {
            let account = self.update_voice_flag(|voice| {
                if voice.camera {
                    None
                } else {
                    voice.camera = true;
                    Some(())
                }
            });
            if let Some(account) = account {
                self.request_cameras(account, CameraPurpose::Camera);
            }
        } else {
            let account = self.update_voice_flag(|voice| {
                if voice.camera {
                    voice.camera = false;
                    Some(())
                } else {
                    None
                }
            });
            if let Some(account) = account {
                let _ = self.cmd_tx.try_send(Command::SetCamera {
                    account,
                    camera: false,
                });
            }
        }
    }

    pub(crate) fn on_screen_toggled(self: &Rc<Self>) {
        let desired = self.voice_screen.is_active();
        if !desired {
            let account = self.update_voice_flag(|voice| {
                if voice.streaming {
                    voice.streaming = false;
                    Some(())
                } else {
                    None
                }
            });
            if let Some(account) = account {
                let _ = self.cmd_tx.try_send(Command::StopStream { account });
            }
            return;
        }

        let account = self.update_voice_flag(|voice| {
            if voice.streaming {
                None
            } else {
                voice.streaming = true;
                Some(())
            }
        });
        if let Some(account) = account {
            self.show_share_popup(account);
        }
    }

    /// Asks the account's Tokio side to open the camera portal and enumerate
    /// devices. The response comes back as `UiEvent::CameraChoices`.
    pub(crate) fn request_cameras(self: &Rc<Self>, account: AccountId, purpose: CameraPurpose) {
        self.set_status("Requesting camera access…");
        let runtime = self.runtime.clone();
        let ui = self.ui_tx.clone();
        runtime.spawn(async move {
            match kc_media::portal::request_cameras().await {
                Ok(Some(access)) => {
                    let cameras = access.cameras.clone();
                    let _ = ui
                        .send(UiEvent::CameraChoices {
                            account,
                            purpose,
                            access,
                            cameras,
                        })
                        .await;
                }
                Ok(None) => {
                    let _ = ui
                        .send(UiEvent::CameraUnavailable {
                            account,
                            purpose,
                            message: "no camera available".to_owned(),
                        })
                        .await;
                }
                Err(error) => {
                    let _ = ui
                        .send(UiEvent::CameraUnavailable {
                            account,
                            purpose,
                            message: format!("camera portal: {error}"),
                        })
                        .await;
                }
            }
        });
    }

    /// Camera access arrived: ask in the camera popout before anything is
    /// sent.
    pub(crate) fn show_camera_dialog(
        self: &Rc<Self>,
        account: AccountId,
        purpose: CameraPurpose,
        access: CameraAccess,
        cameras: Vec<CameraChoice>,
    ) {
        if cameras.is_empty() {
            self.reset_camera_ui(purpose);
            self.set_status("No camera available");
            return;
        }
        self.show_camera_popup(account, access);
    }

    pub(crate) fn reset_camera_ui(self: &Rc<Self>, _purpose: CameraPurpose) {
        let changed = self.update_voice_flag(|voice| {
            if voice.camera {
                voice.camera = false;
                Some(())
            } else {
                None
            }
        });
        if changed.is_some() {
            self.render_voice_panel();
        }
    }

    pub(crate) fn reset_stream_ui(self: &Rc<Self>) {
        let changed = self.update_voice_flag(|voice| {
            if voice.streaming {
                voice.streaming = false;
                Some(())
            } else {
                None
            }
        });
        if changed.is_some() {
            self.render_voice_panel();
        }
    }

    /// Applies a toggle change to the active session's voice state. Returns the
    /// account id only when the state actually changed, so programmatic
    /// `set_active` calls during rendering never echo commands back to Tokio.
    pub(crate) fn update_voice_flag<T: Copy>(
        &self,
        change: impl FnOnce(&mut VoiceUiState) -> Option<T>,
    ) -> Option<AccountId> {
        let mut model = self.model.borrow_mut();
        let active = model.active?;
        let session = model.sessions.get_mut(&active)?;
        let voice = session.voice.as_mut()?;
        change(voice)?;
        Some(active)
    }

    /// Renders the sidebar voice card, shown whenever the account holds a
    /// voice session, whichever channel is being browsed.
    pub(crate) fn render_voice_panel(self: &Rc<Self>) {
        let snapshot = {
            let model = self.model.borrow();
            let Some(active) = model.active else {
                self.voice_panel.set_visible(false);
                return;
            };
            let Some(session) = model.sessions.get(&active) else {
                self.voice_panel.set_visible(false);
                return;
            };
            let voice = session.voice.clone();
            let voice_channel = voice.as_ref().map(|voice| {
                let name = session.store.read(|state| {
                    state
                        .channels
                        .get(&voice.channel_id)
                        .map(Channel::display_name)
                });
                let guild_name = voice.guild_id.and_then(|guild_id| {
                    session
                        .guilds
                        .iter()
                        .find(|guild| guild.id == guild_id)
                        .map(|guild| guild.name.clone())
                });
                (name, guild_name)
            });
            (voice, voice_channel)
        };

        let (voice, voice_channel) = snapshot;
        let joined = voice.is_some();

        self.voice_panel.set_visible(joined);
        self.voice_mute.set_sensitive(joined);
        self.voice_deafen.set_sensitive(joined);

        let (name, guild_name) = voice_channel.unwrap_or((None, None));
        self.voice_channel_label
            .set_label(&name.unwrap_or_else(|| "Voice channel".to_owned()));
        match guild_name.filter(|name| !name.is_empty()) {
            Some(guild) => self
                .voice_channel_label
                .set_tooltip_text(Some(&format!("Voice channel in {guild}"))),
            None => self.voice_channel_label.set_tooltip_text(None),
        }

        let (muted, deafened, camera, streaming) = voice
            .as_ref()
            .map(|voice| (voice.muted, voice.deafened, voice.camera, voice.streaming))
            .unwrap_or((false, false, false, false));

        self.voice_mute.set_active(muted);
        self.voice_deafen.set_active(deafened);
        self.voice_camera.set_active(camera);
        self.voice_screen.set_active(streaming);

        self.voice_camera.set_tooltip_text(Some(if camera {
            "Turn Off Camera"
        } else {
            "Turn On Camera"
        }));
        self.voice_screen.set_tooltip_text(Some(if streaming {
            "Stop Sharing"
        } else {
            "Share Your Screen"
        }));
        if streaming {
            self.voice_screen.add_css_class("suggested-action");
        } else {
            self.voice_screen.remove_css_class("suggested-action");
        }

        let mut state = Vec::new();
        if camera {
            state.push("Camera on");
        }
        if streaming {
            state.push("Sharing screen with desktop audio");
        }
        if state.is_empty() {
            self.voice_status_label.set_visible(false);
        } else {
            self.voice_status_label.set_label(&state.join(" · "));
            self.voice_status_label.set_visible(true);
        }
    }

    /// A participant (or the local user) started or stopped speaking. The
    /// voice layer reports both edges, so the ring follows them exactly.
    pub(crate) fn record_speaking(
        self: &Rc<Self>,
        account: AccountId,
        user_id: Snowflake,
        speaking: bool,
    ) {
        if self.model.borrow().active != Some(account) {
            return;
        }
        let changed = if speaking {
            self.speaking.borrow_mut().insert(user_id)
        } else {
            self.speaking.borrow_mut().remove(&user_id)
        };
        if changed {
            self.show_speaking(user_id, speaking);
        }
    }

    /// Nobody is speaking any more (the call ended).
    pub(crate) fn clear_speaking(&self) {
        let speakers: Vec<Snowflake> = self.speaking.borrow_mut().drain().collect();
        for user_id in speakers {
            self.show_speaking(user_id, false);
        }
    }

    // -- rendering -------------------------------------------------------

    pub(crate) fn render_all(self: &Rc<Self>) {
        self.render_account_controls();
        self.render_user_bar();
        self.render_guilds();
        self.render_channels();
        self.render_channel_header();
        self.render_messages();
        self.render_composer();
        self.render_members();
        self.render_voice_panel();
        self.render_typing();
        self.update_connection();
    }

    /// Shows a banner while the gateway is not connected.
    pub(crate) fn update_connection(&self) {
        let state = self.with_session(|session| session.connection.clone());
        let (text, button) = match state {
            Some(ConnState::Connecting) => ("Connecting to Discord…", None),
            Some(ConnState::Disconnected) => ("Disconnected. Reconnecting…", None),
            Some(ConnState::SignedOut) => ("Discord signed this account out.", Some("What Now?")),
            Some(ConnState::Stopped) => (
                "Discord closed the connection. Restart kylecord to reconnect.",
                None,
            ),
            _ => ("", None),
        };
        self.banner.set_button_label(button);
        self.banner.set_title(text);
        self.banner.set_revealed(!text.is_empty());
    }

    /// Fills the status and account menu behind the user bar.
    pub(crate) fn render_account_controls(self: &Rc<Self>) {
        let (active, accounts) = {
            let model = self.model.borrow();
            (model.active, model.accounts.accounts().to_vec())
        };
        clear_box(&self.account_popover_box);
        let menu_item = |text: &str| {
            let button = gtk::Button::new();
            button.add_css_class("flat");
            button.set_child(Some(&label(text, &[])));
            button
        };

        let heading = label("STATUS", &["kc-category"]);
        heading.set_margin_start(6);
        self.account_popover_box.append(&heading);
        for (status, text) in [
            ("online", "Online"),
            ("idle", "Idle"),
            ("dnd", "Do Not Disturb"),
            ("invisible", "Invisible"),
        ] {
            let button = gtk::Button::new();
            button.add_css_class("flat");
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let dot = status_dot(if status == "invisible" {
                "offline"
            } else {
                status
            });
            dot.set_valign(gtk::Align::Center);
            row.append(&dot);
            row.append(&label(text, &[]));
            button.set_child(Some(&row));
            let app = self.clone();
            let popover = self.account_popover.clone();
            button.connect_clicked(move |_| {
                popover.popdown();
                app.set_own_status(status);
            });
            self.account_popover_box.append(&button);
        }

        let custom = gtk::Entry::new();
        custom.set_placeholder_text(Some("Set a custom status"));
        custom.set_margin_top(4);
        custom.set_margin_bottom(4);
        if let Some(text) = self
            .with_session(|session| {
                session.store.read(|state| {
                    state
                        .custom_status
                        .as_ref()
                        .and_then(|custom| custom.text.clone())
                })
            })
            .flatten()
        {
            custom.set_text(&text);
        }
        let app = self.clone();
        let popover = self.account_popover.clone();
        custom.connect_activate(move |entry| {
            popover.popdown();
            app.set_custom_status(entry.text().to_string());
        });
        self.account_popover_box.append(&custom);

        self.account_popover_box
            .append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let heading = label("ACCOUNTS", &["kc-category"]);
        heading.set_margin_start(6);
        heading.set_margin_top(4);
        self.account_popover_box.append(&heading);

        for account in &accounts {
            let id = session::account_id(&account.token);
            let button = gtk::Button::new();
            button.add_css_class("flat");
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let name = label(&self.account_name(account), &[]);
            name.set_hexpand(true);
            row.append(&name);
            if active == Some(id) {
                row.append(&gtk::Image::from_icon_name("object-select-symbolic"));
            }
            button.set_child(Some(&row));
            let app = self.clone();
            let popover = self.account_popover.clone();
            button.connect_clicked(move |_| {
                popover.popdown();
                app.switch_account(id);
            });
            self.account_popover_box.append(&button);
        }

        let add = menu_item("Add Account…");
        let app = self.clone();
        let popover = self.account_popover.clone();
        add.connect_clicked(move |_| {
            popover.popdown();
            app.show_login();
        });
        self.account_popover_box.append(&add);
    }

    // -- composer --------------------------------------------------------

    // -- events ----------------------------------------------------------

    pub(crate) fn handle_event(self: &Rc<Self>, event: UiEvent) {
        match event {
            UiEvent::Run(work) => work(self),
            UiEvent::LoginSuccess { user, token, label } => {
                self.login_button.set_sensitive(true);
                self.login_error.set_visible(false);
                let account = Account {
                    user_id: Some(user.id),
                    token: token.clone(),
                    label,
                };
                // Signing in again after Discord ended a session: the old
                // session, under its dead token, goes.
                {
                    let fresh = session::account_id(&token);
                    self.model.borrow_mut().sessions.retain(|id, session| {
                        *id == fresh || session.account.user_id != Some(user.id)
                    });
                }
                let save_error = self.persist_account(account.clone());
                let id = self.start_account(account);
                self.token_entry.set_text("");
                self.label_entry.set_text("");
                self.stack.set_visible_child_name("chat");
                self.switch_account(id);
                if let Some(error) = save_error {
                    self.set_status(&error);
                }
            }
            UiEvent::LoginFailed { message } => {
                self.login_button.set_sensitive(true);
                self.login_error.set_label(&message);
                self.login_error.set_visible(true);
            }
            UiEvent::Connected { account } | UiEvent::Resumed { account } => {
                let changed = {
                    let mut model = self.model.borrow_mut();
                    if let Some(session) = model.sessions.get_mut(&account) {
                        session.connection = ConnState::Connected;
                    }
                    model.active == Some(account)
                };
                if changed {
                    self.update_connection();
                }
            }
            UiEvent::Ready { account, user } => {
                let mut save_error = None;
                {
                    let mut model = self.model.borrow_mut();
                    if let Some(session) = model.sessions.get_mut(&account) {
                        session.connection = ConnState::Ready;
                        if let Some(user) = &user {
                            session.user = Some(user.clone());
                            session.account.user_id = Some(user.id);
                            let account = session.account.clone();
                            model.accounts.upsert(account);
                            if let Err(error) = model.accounts.save() {
                                save_error = Some(format!("could not save accounts: {error}"));
                            }
                        }
                    }
                }
                if self.model.borrow().active == Some(account) {
                    self.render_all();
                }
                if let Some(error) = save_error {
                    self.set_status(&error);
                }
            }
            UiEvent::StoreEvent { account, event } => {
                self.handle_store_event(account, event);
            }
            UiEvent::GatewayClosed {
                account,
                code,
                reason,
                fatal,
            } => {
                // 4004: Discord no longer takes the token.
                let signed_out = fatal && code == Some(4004);
                let active = {
                    let mut model = self.model.borrow_mut();
                    if let Some(session) = model.sessions.get_mut(&account) {
                        session.connection = if signed_out {
                            ConnState::SignedOut
                        } else if fatal {
                            ConnState::Stopped
                        } else {
                            ConnState::Disconnected
                        };
                    }
                    model.active == Some(account)
                };
                if active {
                    self.update_connection();
                    self.render_user_bar();
                    if signed_out {
                        self.set_status("Discord signed this account out");
                        self.show_signed_out(account);
                    } else if fatal {
                        self.set_status(&format!("Discord closed the connection: {reason}"));
                    }
                }
            }
            UiEvent::VideoUpload {
                account,
                stream,
                upload,
            } => {
                self.video_info
                    .borrow_mut()
                    .uploads
                    .insert((account, stream), upload);
            }
            UiEvent::RequiredAction { account, action } => {
                if self.model.borrow().active == Some(account) {
                    match action {
                        Some(action) => {
                            tracing::warn!(action, "discord requires an action on the account");
                            self.show_required_action(&action);
                        }
                        None => self.set_status("Discord has verified this account"),
                    }
                }
            }
            UiEvent::VoiceJoined {
                account,
                channel_id,
            } => {
                {
                    let mut model = self.model.borrow_mut();
                    if let Some(session) = model.sessions.get_mut(&account) {
                        let guild_id = session.store.read(|state| {
                            state
                                .channels
                                .get(&channel_id)
                                .and_then(|channel| channel.guild_id)
                        });
                        let (muted, deafened, camera, streaming) = session
                            .voice
                            .as_ref()
                            .map(|voice| {
                                (voice.muted, voice.deafened, voice.camera, voice.streaming)
                            })
                            .unwrap_or((false, false, false, false));
                        let watching = session
                            .voice
                            .as_ref()
                            .filter(|voice| voice.channel_id == channel_id)
                            .and_then(|voice| voice.watching);
                        session.voice = Some(VoiceUiState {
                            guild_id,
                            channel_id,
                            muted,
                            deafened,
                            camera,
                            streaming,
                            watching,
                        });
                    }
                }
                if self.model.borrow().active == Some(account) {
                    self.render_voice_panel();
                    self.queue_sidebar();
                    self.refresh_voice_view();
                }
            }
            UiEvent::VoiceLeft { account } => {
                {
                    let mut model = self.model.borrow_mut();
                    if let Some(session) = model.sessions.get_mut(&account) {
                        session.voice = None;
                    }
                }
                if self.model.borrow().active == Some(account) {
                    self.clear_ping();
                    self.clear_speaking();
                    self.focused_video.set(None);
                    self.render_voice_panel();
                    self.queue_sidebar();
                    // Leaving a DM call returns to its chat; a guild voice
                    // channel redraws its (now emptier) tiles.
                    self.render_messages();
                    self.refresh_voice_view();
                }
            }
            UiEvent::VoiceSpeaking {
                account,
                user_id,
                speaking,
            } => {
                self.record_speaking(account, user_id, speaking);
            }
            UiEvent::VoiceError { account, message } => {
                if self.model.borrow().active == Some(account) {
                    self.set_status(&message);
                }
            }
            UiEvent::VoicePing { account, millis } => {
                if self.model.borrow().active == Some(account) {
                    self.note_ping(millis);
                }
            }
            UiEvent::CallRinging {
                account,
                channel_id,
                ringing,
            } => {
                if ringing {
                    if self.model.borrow().active == Some(account) {
                        self.show_incoming_call(channel_id);
                    } else {
                        self.set_status("Incoming call on another account");
                    }
                } else {
                    self.dismiss_incoming_call(channel_id);
                }
            }
            UiEvent::VideoStarted {
                account,
                user_id,
                stream,
                feed,
            } => {
                if let Some(video) = crate::live::LiveVideo::start(feed) {
                    self.videos
                        .borrow_mut()
                        .insert((account, user_id, stream), video);
                }
                self.update_popout((account, user_id, stream));
                if self.model.borrow().active == Some(account) {
                    self.refresh_voice_view();
                }
            }
            UiEvent::VideoStopped {
                account,
                user_id,
                stream,
                feed,
            } => {
                // A newer video for the same person may already have
                // replaced this one; only remove the one that ended.
                let key = (account, user_id, stream);
                let ended = self
                    .videos
                    .borrow()
                    .get(&key)
                    .is_some_and(|video| std::sync::Arc::ptr_eq(&video.feed, &feed));
                if ended {
                    let video = self.videos.borrow_mut().remove(&key);
                    if let Some(video) = &video {
                        let (received, decoded) = video.progress();
                        tracing::info!(%user_id, stream, received, decoded, "video stopped");
                    }
                    if self.focused_video.get() == Some((user_id, stream)) {
                        self.focused_video.set(None);
                    }
                    // Its own window, if it had one, goes with it.
                    let popout = self.popouts.borrow_mut().remove(&key);
                    if let Some(popout) = popout {
                        popout.close();
                    }
                    if self.model.borrow().active == Some(account) {
                        self.refresh_voice_view();
                    }
                }
            }
            UiEvent::StreamWatch { account, user_id } => {
                {
                    let mut model = self.model.borrow_mut();
                    if let Some(session) = model.sessions.get_mut(&account)
                        && let Some(voice) = &mut session.voice
                    {
                        voice.watching = user_id;
                    }
                }
                // Watching opens the stream enlarged; stopping clears it.
                match user_id {
                    Some(owner) => self.focused_video.set(Some((owner, true))),
                    None => {
                        if matches!(self.focused_video.get(), Some((_, true))) {
                            self.focused_video.set(None);
                        }
                    }
                }
                if self.model.borrow().active == Some(account) {
                    self.refresh_voice_view();
                }
            }
            UiEvent::StreamStarted { account } => {
                {
                    let mut model = self.model.borrow_mut();
                    if let Some(session) = model.sessions.get_mut(&account)
                        && let Some(voice) = &mut session.voice
                    {
                        voice.streaming = true;
                    }
                }
                if self.model.borrow().active == Some(account) {
                    self.set_status("Sharing screen");
                    self.render_voice_panel();
                }
            }
            UiEvent::StreamStopped { account } => {
                {
                    let mut model = self.model.borrow_mut();
                    if let Some(session) = model.sessions.get_mut(&account)
                        && let Some(voice) = &mut session.voice
                    {
                        voice.streaming = false;
                    }
                }
                if self.model.borrow().active == Some(account) {
                    self.set_status("");
                    self.render_voice_panel();
                }
            }
            UiEvent::StreamError { account, message } => {
                {
                    let mut model = self.model.borrow_mut();
                    if let Some(session) = model.sessions.get_mut(&account)
                        && let Some(voice) = &mut session.voice
                    {
                        voice.streaming = false;
                    }
                }
                if self.model.borrow().active == Some(account) {
                    self.set_status(&message);
                    self.render_voice_panel();
                }
            }
            UiEvent::CameraChoices {
                account,
                purpose,
                access,
                cameras,
            } => {
                self.show_camera_dialog(account, purpose, access, cameras);
            }
            UiEvent::CameraUnavailable {
                account,
                purpose,
                message,
            } => {
                self.reset_camera_ui(purpose);
                if self.model.borrow().active == Some(account) {
                    self.set_status(&message);
                }
            }
            UiEvent::CameraStarted { account } => {
                let changed = self.model.borrow().active == Some(account)
                    && self
                        .update_voice_flag(|voice| (!voice.camera).then(|| voice.camera = true))
                        .is_some();
                if changed {
                    self.render_voice_panel();
                }
            }
            UiEvent::CameraStopped { account } | UiEvent::CameraFailed { account } => {
                let changed = self.model.borrow().active == Some(account)
                    && self
                        .update_voice_flag(|voice| voice.camera.then(|| voice.camera = false))
                        .is_some();
                if changed {
                    self.render_voice_panel();
                }
            }
            UiEvent::Error { account, message } => {
                let active = self.model.borrow().active;
                if account.is_none() || account == active {
                    self.set_status(&message);
                }
            }
        }
    }

    pub(crate) fn handle_store_event(self: &Rc<Self>, account: AccountId, event: StoreEvent) {
        if self.model.borrow().active != Some(account) {
            return;
        }
        let selected_channel = self
            .with_session(|session| session.selected_channel)
            .flatten();
        let selected_guild = self
            .with_session(|session| session.selected_guild)
            .flatten();
        match event {
            StoreEvent::Ready => {
                self.sync_session_guilds();
                // Land on the home view until the user picks a server.
                let first_run = self
                    .with_session_mut(|session| {
                        let fresh = session.selected_guild.is_none() && !session.dm_mode;
                        if fresh {
                            session.dm_mode = true;
                        }
                        fresh
                    })
                    .unwrap_or(false);
                let _ = first_run;
                self.render_all();
            }
            StoreEvent::GuildAdded(_)
            | StoreEvent::GuildUpdated(_)
            | StoreEvent::GuildRemoved(_) => {
                self.sync_session_guilds();
                self.queue_sidebar();
                self.render_channel_header();
            }
            StoreEvent::ChannelAdded(_) | StoreEvent::ChannelUpdated(_) => {
                self.queue_sidebar();
                self.render_channel_header();
            }
            StoreEvent::ChannelRemoved(channel_id) => {
                if selected_channel == Some(channel_id) {
                    self.with_session_mut(|session| session.selected_channel = None);
                    self.render_all();
                } else {
                    self.queue_sidebar();
                }
            }
            StoreEvent::RelationshipsUpdated => {
                self.queue_sidebar();
                if selected_channel.is_none() {
                    self.render_friends();
                }
            }
            StoreEvent::MessageAdded {
                channel_id,
                message_id,
            } => {
                let author = self
                    .with_session(|session| {
                        session.store.read(|state| {
                            Some(state.messages.get(&channel_id)?.get(message_id)?.author.id)
                        })
                    })
                    .flatten();
                if let Some(author) = author {
                    self.clear_typing_for(channel_id, author);
                }
                self.message_added(channel_id, message_id);
            }
            StoreEvent::MessageUpdated {
                channel_id,
                message_id,
            } => self.message_updated(channel_id, message_id),
            StoreEvent::MessageDeleted {
                channel_id,
                message_id,
            } => self.message_deleted(channel_id, message_id),
            StoreEvent::HistoryLoaded(channel_id) => {
                self.history_loaded(channel_id);
                if selected_channel == Some(channel_id) {
                    self.ack_channel(channel_id);
                }
            }
            StoreEvent::GuildChannelsLoaded(guild) => {
                if let Some(session) = self.model.borrow_mut().sessions.get_mut(&account) {
                    session.loading_guild_channels.remove(&guild);
                    session.loaded_guild_channels.insert(guild);
                }
                if selected_guild == Some(guild) {
                    self.queue_sidebar();
                }
            }
            StoreEvent::GuildMembersUpdated(_) => {}
            StoreEvent::MemberListUpdated(guild) => {
                if selected_guild == Some(guild) {
                    self.render_members();
                }
            }
            StoreEvent::Typing {
                channel_id,
                user_id,
            } => self.note_typing(channel_id, user_id),
            StoreEvent::PresenceUpdated { .. } => {
                if selected_guild.is_none() {
                    self.queue_sidebar();
                    if selected_channel.is_none() {
                        self.render_friends();
                    }
                }
            }
            StoreEvent::VoiceStateUpdated { .. } => {
                self.queue_sidebar();
                self.render_voice_panel();
                self.refresh_voice_view();
            }
            StoreEvent::ReadStateUpdated(_) => self.queue_sidebar(),
            StoreEvent::SettingsUpdated => {
                self.queue_sidebar();
                self.render_user_bar();
            }
            StoreEvent::VoiceServerUpdated { .. }
            | StoreEvent::StreamUpdated(_)
            | StoreEvent::BansUpdated(_) => {}
        }
    }

    /// Keeps the session's guild snapshot (used by the voice panel) current.
    pub(crate) fn sync_session_guilds(&self) {
        let mut model = self.model.borrow_mut();
        let Some(active) = model.active else { return };
        if let Some(session) = model.sessions.get_mut(&active) {
            session.guilds = session
                .store
                .read(|state| state.ordered_guilds().into_iter().cloned().collect());
        }
    }
}
