//! The libadwaita application shell: login page, guild rail, channel sidebar,
//! message list and composer.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::{Account, AccountStore, Snowflake, Token};
use kc_state::{Channel, ChannelType, Message, StoreEvent};

use crate::markdown::{format_size, format_timestamp, render_markdown};
use crate::session::{self, AccountId, Command, ConnState, Session, UiEvent, VoiceUiState};

const TYPING_TIMEOUT_SECS: u32 = 8;
const TYPING_THROTTLE: Duration = Duration::from_secs(8);

/// Builds the libadwaita application. Call [`run`] from the process entry point.
pub fn build_app() -> adw::Application {
    adw::Application::builder()
        .application_id("dev.kyle.kylecord")
        .build()
}

/// Runs the GTK main loop with a background Tokio runtime.
pub fn run() -> glib::ExitCode {
    let app = build_app();

    let runtime = std::sync::Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime"),
    );
    let (ui_tx, ui_rx) = async_channel::unbounded::<UiEvent>();
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<Command>();
    session::spawn_dispatcher(&runtime, cmd_rx, ui_tx);

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
            .default_height(720)
            .build();
        let state = AppState::new(window, accounts, cmd_tx_for_activate.clone());
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
        *state_slot.borrow_mut() = Some(state);
    });

    let code = app.run();
    if let Ok(runtime) = std::sync::Arc::try_unwrap(runtime) {
        runtime.shutdown_timeout(Duration::from_millis(250));
    }
    code
}

struct Model {
    accounts: AccountStore,
    sessions: HashMap<AccountId, Session>,
    active: Option<AccountId>,
}

struct AppState {
    window: adw::ApplicationWindow,
    stack: gtk::Stack,

    // Login.
    token_entry: gtk::PasswordEntry,
    label_entry: gtk::Entry,
    login_button: gtk::Button,
    login_error: gtk::Label,

    // Header.
    account_button: gtk::MenuButton,
    account_popover: gtk::Popover,
    account_popover_box: gtk::Box,
    window_title: adw::WindowTitle,
    connection_label: gtk::Label,

    // Guild rail and channel sidebar.
    guild_box: gtk::Box,
    channel_box: gtk::Box,

    // Message area.
    messages_scroll: gtk::ScrolledWindow,
    messages_box: gtk::Box,
    typing_label: gtk::Label,
    voice_bar: gtk::Box,
    voice_label: gtk::Label,
    voice_join: gtk::Button,
    voice_leave: gtk::Button,
    voice_mute: gtk::ToggleButton,
    voice_deafen: gtk::ToggleButton,
    voice_camera: gtk::ToggleButton,
    voice_screen: gtk::ToggleButton,
    composer: gtk::TextView,
    send_button: gtk::Button,
    status_bar: gtk::Label,

    model: RefCell<Model>,
    cmd_tx: async_channel::Sender<Command>,
    typing_generation: Rc<Cell<u64>>,
    last_rendered_channel: Cell<Option<Snowflake>>,
    speaking: RefCell<HashSet<Snowflake>>,
    speaking_generation: RefCell<HashMap<Snowflake, u64>>,
}

impl AppState {
    fn new(
        window: adw::ApplicationWindow,
        accounts: AccountStore,
        cmd_tx: async_channel::Sender<Command>,
    ) -> Rc<Self> {
        let stack = gtk::Stack::new();
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);

        // -- login -------------------------------------------------------
        let login_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let login_form = gtk::Box::new(gtk::Orientation::Vertical, 10);
        login_form.set_halign(gtk::Align::Center);
        login_form.set_valign(gtk::Align::Center);
        login_form.set_width_request(360);
        login_form.set_margin_start(24);
        login_form.set_margin_end(24);

        let login_title = gtk::Label::new(Some("kylecord"));
        login_title.add_css_class("title-1");
        let login_subtitle = gtk::Label::new(Some("Sign in with a Discord user token"));
        login_subtitle.add_css_class("dim-label");
        login_subtitle.set_wrap(true);

        let token_entry = gtk::PasswordEntry::new();
        token_entry.set_show_peek_icon(true);
        token_entry.set_placeholder_text(Some("User token"));

        let label_entry = gtk::Entry::new();
        label_entry.set_placeholder_text(Some("Label (optional)"));

        let login_button = gtk::Button::with_label("Add account");
        login_button.add_css_class("suggested-action");

        let login_error = gtk::Label::new(None);
        login_error.add_css_class("error");
        login_error.set_wrap(true);
        login_error.set_visible(false);

        login_form.append(&login_title);
        login_form.append(&login_subtitle);
        login_form.append(&token_entry);
        login_form.append(&label_entry);
        login_form.append(&login_button);
        login_form.append(&login_error);
        login_page.append(&login_form);
        stack.add_named(&login_page, Some("login"));

        // -- header ------------------------------------------------------
        let header = adw::HeaderBar::new();

        let account_button = gtk::MenuButton::new();
        account_button.set_label("Account");
        let account_popover = gtk::Popover::new();
        let account_popover_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        account_popover_box.set_margin_top(6);
        account_popover_box.set_margin_bottom(6);
        account_popover_box.set_margin_start(6);
        account_popover_box.set_margin_end(6);
        account_popover_box.set_width_request(220);
        account_popover.set_child(Some(&account_popover_box));
        account_button.set_popover(Some(&account_popover));
        header.pack_start(&account_button);

        let window_title = adw::WindowTitle::new("kylecord", "");
        header.set_title_widget(Some(&window_title));

        let connection_label = gtk::Label::new(Some("Offline"));
        connection_label.add_css_class("dim-label");
        header.pack_end(&connection_label);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);

        // -- guild rail --------------------------------------------------
        let guild_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        guild_box.set_margin_top(8);
        guild_box.set_margin_bottom(8);
        guild_box.set_valign(gtk::Align::Start);
        let guild_scroll = gtk::ScrolledWindow::new();
        guild_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        guild_scroll.set_width_request(72);
        guild_scroll.set_child(Some(&guild_box));

        // -- channel sidebar ---------------------------------------------
        let channel_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        channel_box.set_margin_top(8);
        channel_box.set_margin_bottom(8);
        channel_box.set_margin_start(6);
        channel_box.set_margin_end(6);
        channel_box.set_valign(gtk::Align::Start);
        let channel_scroll = gtk::ScrolledWindow::new();
        channel_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        channel_scroll.set_width_request(220);
        channel_scroll.set_child(Some(&channel_box));

        // -- messages and composer ----------------------------------------
        let messages_scroll = gtk::ScrolledWindow::new();
        messages_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        messages_scroll.set_vexpand(true);
        let messages_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        messages_box.set_margin_top(12);
        messages_box.set_margin_bottom(12);
        messages_box.set_margin_start(16);
        messages_box.set_margin_end(16);
        messages_box.set_valign(gtk::Align::Start);
        messages_scroll.set_child(Some(&messages_box));

        let typing_label = gtk::Label::new(None);
        typing_label.set_xalign(0.0);
        typing_label.set_margin_start(16);
        typing_label.set_margin_bottom(2);
        typing_label.add_css_class("dim-label");
        typing_label.set_visible(false);

        // Fixed sibling of the message scroller and composer; shown only for
        // voice channels so it survives message re-renders.
        let voice_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        voice_bar.set_margin_start(12);
        voice_bar.set_margin_end(12);
        voice_bar.set_margin_top(4);
        let voice_label = gtk::Label::new(None);
        voice_label.set_xalign(0.0);
        voice_label.set_hexpand(true);
        voice_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        voice_label.add_css_class("title-4");
        let voice_join = gtk::Button::with_label("Join");
        voice_join.add_css_class("suggested-action");
        let voice_leave = gtk::Button::with_label("Leave");
        let voice_mute = gtk::ToggleButton::with_label("Mute");
        let voice_deafen = gtk::ToggleButton::with_label("Deafen");
        let voice_camera = gtk::ToggleButton::with_label("Camera");
        let voice_screen = gtk::ToggleButton::with_label("Share screen");
        voice_bar.append(&voice_label);
        voice_bar.append(&voice_join);
        voice_bar.append(&voice_leave);
        voice_bar.append(&voice_mute);
        voice_bar.append(&voice_deafen);
        voice_bar.append(&voice_camera);
        voice_bar.append(&voice_screen);
        voice_bar.set_visible(false);

        let composer = gtk::TextView::new();
        composer.set_wrap_mode(gtk::WrapMode::WordChar);
        composer.set_accepts_tab(false);
        composer.set_top_margin(6);
        composer.set_bottom_margin(6);
        composer.set_left_margin(8);
        composer.set_right_margin(8);

        let composer_scroll = gtk::ScrolledWindow::new();
        // Natural height propagation only works with the vertical policy off;
        // together with the maximum height this grows the composer 1-5 lines.
        composer_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
        composer_scroll.set_min_content_height(42);
        composer_scroll.set_max_content_height(112);
        composer_scroll.set_propagate_natural_height(true);
        composer_scroll.set_hexpand(true);
        composer_scroll.add_css_class("card");
        composer_scroll.set_child(Some(&composer));

        let send_button = gtk::Button::with_label("Send");
        send_button.add_css_class("suggested-action");
        send_button.set_valign(gtk::Align::End);

        let composer_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        composer_row.set_margin_start(12);
        composer_row.set_margin_end(12);
        composer_row.set_margin_top(6);
        composer_row.set_margin_bottom(12);
        composer_row.append(&composer_scroll);
        composer_row.append(&send_button);

        let message_area = gtk::Box::new(gtk::Orientation::Vertical, 0);
        message_area.set_hexpand(true);
        message_area.append(&messages_scroll);
        message_area.append(&typing_label);
        message_area.append(&voice_bar);
        message_area.append(&composer_row);

        let status_bar = gtk::Label::new(None);
        status_bar.set_xalign(0.0);
        status_bar.set_margin_start(12);
        status_bar.set_margin_end(12);
        status_bar.set_margin_top(2);
        status_bar.set_margin_bottom(6);
        status_bar.add_css_class("dim-label");
        status_bar.set_visible(false);

        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.append(&guild_scroll);
        body.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        body.append(&channel_scroll);
        body.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        body.append(&message_area);

        let chat_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        chat_page.append(&body);
        chat_page.append(&status_bar);
        stack.add_named(&chat_page, Some("chat"));

        toolbar.set_content(Some(&stack));
        window.set_content(Some(&toolbar));

        Rc::new(Self {
            window,
            stack,
            token_entry,
            label_entry,
            login_button,
            login_error,
            account_button,
            account_popover,
            account_popover_box,
            window_title,
            connection_label,
            guild_box,
            channel_box,
            messages_scroll,
            messages_box,
            typing_label,
            voice_bar,
            voice_label,
            voice_join,
            voice_leave,
            voice_mute,
            voice_deafen,
            voice_camera,
            voice_screen,
            composer,
            send_button,
            status_bar,
            model: RefCell::new(Model {
                accounts,
                sessions: HashMap::new(),
                active: None,
            }),
            cmd_tx,
            typing_generation: Rc::new(Cell::new(0)),
            last_rendered_channel: Cell::new(None),
            speaking: RefCell::new(HashSet::new()),
            speaking_generation: RefCell::new(HashMap::new()),
        })
    }

    fn connect_signals(self: &Rc<Self>) {
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
        self.send_button
            .connect_clicked(move |_| app.send_current());

        let app = self.clone();
        self.composer.buffer().connect_changed(move |_| {
            app.on_composer_changed();
        });

        let keys = gtk::EventControllerKey::new();
        let app = self.clone();
        keys.connect_key_pressed(move |_, key, _, state| {
            if key == gtk::gdk::Key::Return || key == gtk::gdk::Key::KP_Enter {
                if state.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
                    return glib::Propagation::Proceed;
                }
                app.send_current();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.composer.add_controller(keys);

        let app = self.clone();
        self.voice_join.connect_clicked(move |_| app.join_voice());
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
    }

    // -- account lifecycle -----------------------------------------------

    fn start_existing(self: &Rc<Self>) {
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
    fn start_account(&self, account: Account) -> AccountId {
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

    fn switch_account(self: &Rc<Self>, id: AccountId) {
        if !self.model.borrow().sessions.contains_key(&id) {
            return;
        }
        self.model.borrow_mut().active = Some(id);
        self.clear_typing();
        self.render_all();
    }

    fn account_name(&self, account: &Account) -> String {
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

    fn persist_account(&self, account: Account) -> Option<String> {
        let mut model = self.model.borrow_mut();
        model.accounts.upsert(account);
        model
            .accounts
            .save()
            .err()
            .map(|error| format!("could not save accounts: {error}"))
    }

    fn submit_login(self: &Rc<Self>) {
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

    fn show_login(self: &Rc<Self>) {
        self.login_error.set_visible(false);
        self.login_button.set_sensitive(true);
        self.stack.set_visible_child_name("login");
        self.token_entry.grab_focus();
        let has_accounts = !self.model.borrow().accounts.is_empty();
        self.account_button.set_visible(has_accounts);
    }

    // -- navigation ------------------------------------------------------

    fn open_dms(self: &Rc<Self>) {
        {
            let mut model = self.model.borrow_mut();
            let Some(active) = model.active else { return };
            let Some(session) = model.sessions.get_mut(&active) else {
                return;
            };
            session.dm_mode = true;
            session.selected_guild = None;
            session.selected_channel = None;
        }
        self.clear_typing();
        self.set_status("");
        self.render_all();
    }

    fn select_guild(self: &Rc<Self>, guild_id: Snowflake) {
        let fetch = {
            let mut model = self.model.borrow_mut();
            let Some(active) = model.active else { return };
            let Some(session) = model.sessions.get_mut(&active) else {
                return;
            };
            session.dm_mode = false;
            session.selected_guild = Some(guild_id);
            session.selected_channel = None;

            if !session.loaded_guild_channels.contains(&guild_id)
                && session.loading_guild_channels.insert(guild_id)
            {
                Some((active, session.http.clone(), session.store.clone()))
            } else {
                None
            }
        };
        self.clear_typing();
        self.set_status("");
        self.render_all();
        if let Some((account, http, store)) = fetch {
            let _ = self.cmd_tx.try_send(Command::FetchGuildChannels {
                account,
                http,
                store,
                guild: guild_id,
            });
            self.set_status("Loading channels…");
        }
    }

    fn open_channel(self: &Rc<Self>, channel: Channel) {
        let Some(active) = self.model.borrow().active else {
            return;
        };
        let is_voice = channel.kind.is_voice() && !channel.kind.is_text();
        let (fetch, last_message, http, store) = {
            let mut model = self.model.borrow_mut();
            let Some(session) = model.sessions.get_mut(&active) else {
                return;
            };
            let already_selected = session.selected_channel == Some(channel.id);
            session.selected_channel = Some(channel.id);
            if !already_selected {
                session.last_typing = None;
            }
            match channel.kind {
                ChannelType::Dm | ChannelType::GroupDm => {
                    session.dm_mode = true;
                    session.selected_guild = None;
                }
                _ => {
                    session.dm_mode = false;
                    if let Some(guild_id) = channel.guild_id {
                        session.selected_guild = Some(guild_id);
                    }
                }
            }
            let empty = session
                .store
                .read(|state| state.messages(channel.id).is_empty());
            let should_fetch =
                !is_voice && !already_selected && empty && !session.fetched.contains(&channel.id);
            let fetch = if should_fetch {
                session.fetched.insert(channel.id);
                Some((session.http.clone(), session.store.clone()))
            } else {
                None
            };
            let last_message = session
                .store
                .read(|state| state.last_message_id(channel.id));
            let http = session.http.clone();
            let store = session.store.clone();
            (fetch, last_message, http, store)
        };

        self.clear_typing();
        if is_voice {
            self.set_status("");
        } else {
            self.set_status("");
            if let Some((http, store)) = fetch {
                let _ = self.cmd_tx.try_send(Command::FetchHistory {
                    account: active,
                    http,
                    store,
                    channel: channel.id,
                });
            }
            if let Some(message_id) = last_message {
                store.mark_read(channel.id, message_id);
                let _ = self.cmd_tx.try_send(Command::Ack {
                    http,
                    channel: channel.id,
                    message_id,
                });
            }
        }
        self.render_all();
    }

    // -- voice -----------------------------------------------------------

    fn join_voice(self: &Rc<Self>) {
        let target = {
            let model = self.model.borrow();
            let Some(active) = model.active else { return };
            let Some(session) = model.sessions.get(&active) else {
                return;
            };
            let Some(channel_id) = session.selected_channel else {
                return;
            };
            let channel = session
                .store
                .read(|state| state.channels.get(&channel_id).cloned());
            let Some(channel) = channel else { return };
            if !(channel.kind.is_voice() && !channel.kind.is_text()) {
                return;
            }
            (active, channel.guild_id, channel_id)
        };
        let _ = self.cmd_tx.try_send(Command::JoinVoice {
            account: target.0,
            guild_id: target.1,
            channel_id: target.2,
        });
        self.set_status("Joining voice…");
    }

    fn leave_voice(&self) {
        let Some(account) = self.model.borrow().active else {
            return;
        };
        let _ = self.cmd_tx.try_send(Command::LeaveVoice { account });
    }

    fn on_mute_toggled(self: &Rc<Self>) {
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
        }
    }

    fn on_deafen_toggled(self: &Rc<Self>) {
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
        }
    }

    fn on_camera_toggled(self: &Rc<Self>) {
        let desired = self.voice_camera.is_active();
        let account = self.update_voice_flag(|voice| {
            if voice.camera == desired {
                None
            } else {
                voice.camera = desired;
                Some(desired)
            }
        });
        if let Some(account) = account {
            let _ = self.cmd_tx.try_send(Command::SetCamera {
                account,
                camera: desired,
            });
        }
    }

    fn on_screen_toggled(self: &Rc<Self>) {
        let desired = self.voice_screen.is_active();
        let account = self.update_voice_flag(|voice| {
            if voice.streaming == desired {
                None
            } else {
                voice.streaming = desired;
                Some(desired)
            }
        });
        if let Some(account) = account {
            let command = if desired {
                Command::StartStream { account }
            } else {
                Command::StopStream { account }
            };
            let _ = self.cmd_tx.try_send(command);
            self.set_status(if desired {
                "Starting screen share…"
            } else {
                ""
            });
        }
    }

    /// Applies a toggle change to the active session's voice state. Returns the
    /// account id only when the state actually changed, so programmatic
    /// `set_active` calls during rendering never echo commands back to Tokio.
    fn update_voice_flag<T: Copy>(
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

    fn render_voice_bar(self: &Rc<Self>) {
        let snapshot = {
            let model = self.model.borrow();
            let Some(active) = model.active else {
                self.voice_bar.set_visible(false);
                return;
            };
            let Some(session) = model.sessions.get(&active) else {
                self.voice_bar.set_visible(false);
                return;
            };
            let Some(channel_id) = session.selected_channel else {
                self.voice_bar.set_visible(false);
                return;
            };
            let channel = session
                .store
                .read(|state| state.channels.get(&channel_id).cloned());
            let Some(channel) = channel else {
                self.voice_bar.set_visible(false);
                return;
            };
            if !(channel.kind.is_voice() && !channel.kind.is_text()) {
                self.voice_bar.set_visible(false);
                return;
            }
            (
                channel.display_name(),
                channel_id,
                channel.guild_id,
                session.voice.clone(),
            )
        };

        let (name, channel_id, guild_id, voice) = snapshot;
        self.voice_bar.set_visible(true);
        self.voice_label.set_label(&name);

        let joined = voice
            .as_ref()
            .is_some_and(|voice| voice.channel_id == channel_id && voice.guild_id == guild_id);
        self.voice_join.set_visible(!joined);
        self.voice_leave.set_visible(joined);
        self.voice_mute.set_visible(joined);
        self.voice_deafen.set_visible(joined);
        self.voice_camera.set_visible(joined);
        self.voice_screen.set_visible(joined);
        self.voice_screen.set_sensitive(joined);

        let (muted, deafened, camera, streaming) = match (&voice, joined) {
            (Some(voice), true) => (voice.muted, voice.deafened, voice.camera, voice.streaming),
            _ => (false, false, false, false),
        };
        self.voice_mute.set_active(muted);
        self.voice_deafen.set_active(deafened);
        self.voice_camera.set_active(camera);
        self.voice_screen.set_active(streaming);
    }

    fn record_speaking(self: &Rc<Self>, account: AccountId, user_id: Snowflake, speaking: bool) {
        if self.model.borrow().active != Some(account) {
            return;
        }
        let generation = {
            let mut generations = self.speaking_generation.borrow_mut();
            let generation = generations
                .get(&user_id)
                .copied()
                .unwrap_or(0)
                .wrapping_add(1);
            generations.insert(user_id, generation);
            generation
        };
        if speaking {
            self.speaking.borrow_mut().insert(user_id);
        } else {
            self.speaking.borrow_mut().remove(&user_id);
        }
        self.render_channels();

        if speaking {
            let app = self.clone();
            glib::timeout_add_local_once(Duration::from_millis(800), move || {
                let current = app.speaking_generation.borrow().get(&user_id).copied();
                if current == Some(generation) {
                    app.speaking.borrow_mut().remove(&user_id);
                    if app.model.borrow().active == Some(account) {
                        app.render_channels();
                    }
                }
            });
        }
    }

    // -- rendering -------------------------------------------------------

    fn render_all(self: &Rc<Self>) {
        self.render_account_controls();
        self.render_guilds();
        self.render_channels();
        self.render_voice_bar();
        self.render_messages();
        self.update_title();
        self.update_connection();
    }

    fn render_account_controls(self: &Rc<Self>) {
        let (active, accounts) = {
            let model = self.model.borrow();
            (model.active, model.accounts.accounts().to_vec())
        };
        self.account_button.set_visible(!accounts.is_empty());

        let current = active.and_then(|id| {
            accounts
                .iter()
                .find(|account| session::account_id(&account.token) == id)
        });
        let label = current
            .map(|account| self.account_name(account))
            .unwrap_or_else(|| "Account".to_owned());
        self.account_button.set_label(&label);

        while let Some(child) = self.account_popover_box.first_child() {
            self.account_popover_box.remove(&child);
        }

        let heading = gtk::Label::new(Some("Accounts"));
        heading.set_xalign(0.0);
        heading.set_margin_start(6);
        heading.set_margin_end(6);
        heading.set_margin_top(4);
        heading.add_css_class("dim-label");
        self.account_popover_box.append(&heading);

        for account in &accounts {
            let id = session::account_id(&account.token);
            let mut name = self.account_name(account);
            if active == Some(id) {
                name.push_str(" (active)");
            }
            let button = gtk::Button::new();
            button.add_css_class("flat");
            let text = gtk::Label::new(Some(&name));
            text.set_xalign(0.0);
            text.set_hexpand(true);
            button.set_child(Some(&text));

            let app = self.clone();
            let popover = self.account_popover.clone();
            button.connect_clicked(move |_| {
                app.switch_account(id);
                popover.popdown();
            });
            self.account_popover_box.append(&button);
        }

        self.account_popover_box
            .append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let add = gtk::Button::new();
        add.add_css_class("flat");
        let add_label = gtk::Label::new(Some("Add account"));
        add_label.set_xalign(0.0);
        add_label.set_hexpand(true);
        add.set_child(Some(&add_label));
        let app = self.clone();
        let popover = self.account_popover.clone();
        add.connect_clicked(move |_| {
            popover.popdown();
            app.show_login();
        });
        self.account_popover_box.append(&add);
    }

    fn render_guilds(self: &Rc<Self>) {
        while let Some(child) = self.guild_box.first_child() {
            self.guild_box.remove(&child);
        }

        let (guilds, selected_guild, dm_mode) = {
            let model = self.model.borrow();
            let Some(active) = model.active else { return };
            let Some(session) = model.sessions.get(&active) else {
                return;
            };
            (
                session.guilds.clone(),
                session.selected_guild,
                session.dm_mode,
            )
        };

        let dm_button = gtk::Button::with_label("DM");
        dm_button.set_size_request(44, 44);
        dm_button.set_tooltip_text(Some("Direct messages"));
        dm_button.add_css_class("flat");
        dm_button.add_css_class("circular");
        if dm_mode {
            dm_button.add_css_class("suggested-action");
        }
        let app = self.clone();
        dm_button.connect_clicked(move |_| app.open_dms());
        self.guild_box.append(&dm_button);

        for guild in &guilds {
            let button = gtk::Button::with_label(&guild.initials());
            button.set_size_request(44, 44);
            button.add_css_class("flat");
            button.add_css_class("circular");
            if !dm_mode && selected_guild == Some(guild.id) {
                button.add_css_class("suggested-action");
            }
            let tooltip = if guild.name.is_empty() {
                guild.id.to_string()
            } else {
                guild.name.clone()
            };
            button.set_tooltip_text(Some(&tooltip));
            let app = self.clone();
            let id = guild.id;
            button.connect_clicked(move |_| app.select_guild(id));
            self.guild_box.append(&button);
        }
    }

    fn render_channels(self: &Rc<Self>) {
        while let Some(child) = self.channel_box.first_child() {
            self.channel_box.remove(&child);
        }

        let snapshot = (|| {
            let model = self.model.borrow();
            let active = model.active?;
            let session = model.sessions.get(&active)?;
            let dm_mode = session.dm_mode;
            let guild_id = session.selected_guild;
            let guild_name = guild_id.and_then(|id| {
                session
                    .guilds
                    .iter()
                    .find(|guild| guild.id == id)
                    .map(|guild| guild.name.clone())
            });
            let (channels, unread, voice_users) = session.store.read(|state| {
                let channels: Vec<Channel> = if dm_mode {
                    state.dm_channels().into_iter().cloned().collect()
                } else if let Some(guild_id) = guild_id {
                    state
                        .guild_channels(guild_id)
                        .into_iter()
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                };
                let unread: HashSet<Snowflake> = channels
                    .iter()
                    .filter(|channel| state.unread(channel.id))
                    .map(|channel| channel.id)
                    .collect();
                let voice_users: HashMap<Snowflake, Vec<(Snowflake, String)>> = guild_id
                    .map(|guild_id| {
                        channels
                            .iter()
                            .filter(|channel| channel.kind.is_voice() && !channel.kind.is_text())
                            .map(|channel| {
                                let users = state
                                    .users_in_channel_voice(guild_id, channel.id)
                                    .into_iter()
                                    .map(|user| (user.id, user.display_name().to_owned()))
                                    .collect();
                                (channel.id, users)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (channels, unread, voice_users)
            });
            Some((
                dm_mode,
                guild_id,
                guild_name,
                channels,
                unread,
                voice_users,
                session.selected_channel,
            ))
        })();
        let Some((dm_mode, guild_id, guild_name, channels, unread, voice_users, selected)) =
            snapshot
        else {
            return;
        };

        let header_text = if dm_mode {
            "Direct messages".to_owned()
        } else if let Some(name) = &guild_name {
            if name.is_empty() {
                "Server".to_owned()
            } else {
                name.clone()
            }
        } else {
            "Servers".to_owned()
        };
        let header = gtk::Label::new(Some(&header_text));
        header.set_xalign(0.0);
        header.set_margin_start(8);
        header.set_margin_top(6);
        header.set_margin_bottom(6);
        header.add_css_class("title-4");
        self.channel_box.append(&header);

        if dm_mode {
            for channel in &channels {
                let row = self.channel_row(
                    channel,
                    unread.contains(&channel.id),
                    selected == Some(channel.id),
                );
                self.channel_box.append(&row);
                self.append_voice_users(&voice_users, channel.id);
            }
            if channels.is_empty() {
                self.channel_box.append(&empty_state("No direct messages"));
            }
            return;
        }

        if guild_id.is_none() {
            self.channel_box.append(&empty_state("Select a server"));
            return;
        }

        for group in grouped_channels(&channels) {
            match group {
                ChannelGroup::Header(name) => {
                    let label = gtk::Label::new(Some(&name));
                    label.set_xalign(0.0);
                    label.set_margin_start(8);
                    label.set_margin_top(10);
                    label.set_margin_bottom(2);
                    label.add_css_class("dim-label");
                    self.channel_box.append(&label);
                }
                ChannelGroup::Row(channel) => {
                    let row = self.channel_row(
                        &channel,
                        unread.contains(&channel.id),
                        selected == Some(channel.id),
                    );
                    self.channel_box.append(&row);
                    self.append_voice_users(&voice_users, channel.id);
                }
            }
        }
        if channels.is_empty() {
            self.channel_box.append(&empty_state("No channels"));
        }
    }

    fn channel_row(
        self: &Rc<Self>,
        channel: &Channel,
        unread: bool,
        selected: bool,
    ) -> gtk::Button {
        let button = gtk::Button::new();
        button.add_css_class("flat");
        button.set_halign(gtk::Align::Fill);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.set_margin_start(4);

        let name = match channel.kind {
            ChannelType::GuildText | ChannelType::GuildAnnouncement => {
                format!("# {}", channel.display_name())
            }
            _ => channel.display_name(),
        };
        let label = gtk::Label::new(Some(&name));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        if selected {
            label.add_css_class("accent");
            set_bold(&label, true);
        }
        row.append(&label);

        if unread {
            let bullet = gtk::Label::new(Some("•"));
            bullet.add_css_class("accent");
            row.append(&bullet);
        }

        button.set_child(Some(&row));
        let tooltip = match channel.kind {
            ChannelType::GuildVoice | ChannelType::GuildStageVoice => "Voice channel",
            ChannelType::Dm | ChannelType::GroupDm => "Direct message",
            _ => "Text channel",
        };
        button.set_tooltip_text(Some(tooltip));

        let app = self.clone();
        let channel = channel.clone();
        button.connect_clicked(move |_| app.open_channel(channel.clone()));
        button
    }

    /// A dim caption listing the users connected to a voice channel.
    fn append_voice_users(
        &self,
        voice_users: &HashMap<Snowflake, Vec<(Snowflake, String)>>,
        channel_id: Snowflake,
    ) {
        let Some(users) = voice_users.get(&channel_id) else {
            return;
        };
        if users.is_empty() {
            return;
        }
        let speaking = self.speaking.borrow();
        let names: Vec<String> = users
            .iter()
            .map(|(user_id, name)| {
                if speaking.contains(user_id) {
                    format!("* {name}")
                } else {
                    name.clone()
                }
            })
            .collect();
        let label = gtk::Label::new(Some(&names.join(", ")));
        label.set_xalign(0.0);
        label.set_margin_start(28);
        label.set_margin_bottom(2);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.add_css_class("dim-label");
        label.add_css_class("caption");
        self.channel_box.append(&label);
    }

    fn render_messages(self: &Rc<Self>) {
        let adjustment = self.messages_scroll.vadjustment();
        let at_bottom = adjustment.upper() - adjustment.page_size() - adjustment.value() < 48.0;

        while let Some(child) = self.messages_box.first_child() {
            self.messages_box.remove(&child);
        }

        let (channel, messages, channel_changed) = {
            let model = self.model.borrow();
            let Some(active) = model.active else {
                return;
            };
            let Some(session) = model.sessions.get(&active) else {
                return;
            };
            let Some(channel_id) = session.selected_channel else {
                self.last_rendered_channel.set(None);
                self.messages_box.append(&empty_state("Select a channel"));
                return;
            };
            let channel_changed =
                self.last_rendered_channel.replace(Some(channel_id)) != Some(channel_id);
            let channel = session
                .store
                .read(|state| state.channels.get(&channel_id).cloned());
            let messages = session.store.read(|state| state.messages(channel_id));
            (channel, messages, channel_changed)
        };

        let Some(channel) = channel else {
            self.messages_box.append(&empty_state("Select a channel"));
            return;
        };
        if channel.kind.is_voice() && !channel.kind.is_text() {
            self.messages_box.append(&empty_state("Voice channel"));
            return;
        }
        if messages.is_empty() {
            self.messages_box.append(&empty_state("No messages yet"));
            return;
        }

        for message in &messages {
            self.messages_box.append(&self.message_row(message));
        }

        if at_bottom || channel_changed {
            let adjustment = self.messages_scroll.vadjustment();
            glib::idle_add_local_once(move || {
                let target = (adjustment.upper() - adjustment.page_size()).max(0.0);
                adjustment.set_value(target);
            });
        }
    }

    fn message_row(&self, message: &Message) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
        row.set_margin_top(4);
        row.set_margin_bottom(4);

        if let Some(referenced) = &message.referenced_message {
            let reply = gtk::Label::new(Some(&format!(
                "replying to {}: {}",
                referenced.author.display_name(),
                snippet(&referenced.content, 60)
            )));
            reply.set_xalign(0.0);
            reply.set_ellipsize(gtk::pango::EllipsizeMode::End);
            reply.add_css_class("dim-label");
            row.append(&reply);
        }

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let author = gtk::Label::new(Some(message.author.display_name()));
        author.set_xalign(0.0);
        set_bold(&author, true);
        header.append(&author);

        let time = format_timestamp(&message.timestamp);
        if !time.is_empty() {
            let stamp = gtk::Label::new(Some(&time));
            stamp.add_css_class("dim-label");
            header.append(&stamp);
        }
        if message.edited_timestamp.is_some() {
            let edited = gtk::Label::new(Some("(edited)"));
            edited.add_css_class("dim-label");
            header.append(&edited);
        }
        row.append(&header);

        if !message.content.is_empty() {
            let content = gtk::Label::new(None);
            content.set_xalign(0.0);
            content.set_wrap(true);
            content.set_selectable(true);
            content.set_markup(&render_markdown(&message.content));
            content.connect_activate_link(|_, uri| {
                gtk::show_uri(None::<&gtk::Window>, uri, 0);
                glib::Propagation::Stop
            });
            row.append(&content);
        }

        for attachment in &message.attachments {
            let label = gtk::Label::new(Some(&format!(
                "{} ({})",
                attachment.filename,
                format_size(attachment.size)
            )));
            label.set_xalign(0.0);
            label.set_wrap(true);
            label.set_selectable(true);
            label.add_css_class("dim-label");
            row.append(&label);
        }

        if !message.reactions.is_empty() {
            let reactions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            reactions.set_margin_top(2);
            for reaction in &message.reactions {
                let emoji = reaction
                    .emoji
                    .name
                    .clone()
                    .unwrap_or_else(|| "?".to_owned());
                let label = gtk::Label::new(Some(&format!("{emoji} {}", reaction.count)));
                label.add_css_class("dim-label");
                reactions.append(&label);
            }
            row.append(&reactions);
        }

        row
    }

    fn update_title(&self) {
        let model = self.model.borrow();
        let Some(active) = model.active else { return };
        let Some(session) = model.sessions.get(&active) else {
            return;
        };

        if session.dm_mode {
            let title = session
                .selected_channel
                .and_then(|id| session.store.read(|state| state.channels.get(&id).cloned()))
                .map(|channel| format!("@{}", channel.display_name()));
            self.window_title
                .set_title(&title.unwrap_or_else(|| "Direct messages".to_owned()));
            self.window_title.set_subtitle("");
            return;
        }

        if let Some(guild_id) = session.selected_guild {
            let guild_name = session
                .guilds
                .iter()
                .find(|guild| guild.id == guild_id)
                .map(|guild| guild.name.clone())
                .unwrap_or_default();
            if let Some(channel) = session
                .selected_channel
                .and_then(|id| session.store.read(|state| state.channels.get(&id).cloned()))
            {
                let title = if channel.kind.is_text() && !channel.kind.is_voice() {
                    format!("# {}", channel.display_name())
                } else {
                    channel.display_name()
                };
                self.window_title.set_title(&title);
                self.window_title.set_subtitle(&guild_name);
            } else {
                self.window_title.set_title(if guild_name.is_empty() {
                    "kylecord"
                } else {
                    &guild_name
                });
                self.window_title.set_subtitle("");
            }
            return;
        }

        self.window_title.set_title("kylecord");
        self.window_title.set_subtitle("");
    }

    fn update_connection(&self) {
        let label = {
            let model = self.model.borrow();
            model
                .active
                .and_then(|id| model.sessions.get(&id))
                .map(|session| session.connection.label())
                .unwrap_or_else(|| "Offline".to_owned())
        };
        self.connection_label.set_label(&label);
    }

    fn set_status(&self, text: &str) {
        self.status_bar.set_label(text);
        self.status_bar.set_visible(!text.is_empty());
    }

    fn clear_typing(&self) {
        self.typing_label.set_visible(false);
        self.typing_generation
            .set(self.typing_generation.get().wrapping_add(1));
    }

    // -- composer --------------------------------------------------------

    fn send_current(self: &Rc<Self>) {
        let buffer = self.composer.buffer();
        let content = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .trim()
            .to_owned();
        if content.is_empty() {
            return;
        }
        let Some((active, http, channel)) = self.composer_target() else {
            return;
        };
        let _ = self.cmd_tx.try_send(Command::SendMessage {
            account: active,
            http,
            channel,
            content,
            reply_to: None,
        });
        buffer.set_text("");
    }

    fn on_composer_changed(self: &Rc<Self>) {
        let buffer = self.composer.buffer();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        if text.trim().is_empty() {
            return;
        }

        let mut model = self.model.borrow_mut();
        let Some(active) = model.active else { return };
        let Some(session) = model.sessions.get_mut(&active) else {
            return;
        };
        let Some(channel) = session.selected_channel else {
            return;
        };
        let is_text = session.store.read(|state| {
            state
                .channels
                .get(&channel)
                .is_some_and(|channel| channel.kind.is_text())
        });
        if !is_text {
            return;
        }
        let now = Instant::now();
        let due = session
            .last_typing
            .is_none_or(|last| now.duration_since(last) >= TYPING_THROTTLE);
        if !due {
            return;
        }
        session.last_typing = Some(now);
        let http = session.http.clone();
        drop(model);
        let _ = self.cmd_tx.try_send(Command::SendTyping {
            account: active,
            http,
            channel,
        });
    }

    fn composer_target(&self) -> Option<(AccountId, kc_http::HttpClient, Snowflake)> {
        let model = self.model.borrow();
        let active = model.active?;
        let session = model.sessions.get(&active)?;
        let channel = session.selected_channel?;
        let is_text = session.store.read(|state| {
            state
                .channels
                .get(&channel)
                .is_some_and(|channel| channel.kind.is_text())
        });
        if !is_text {
            return None;
        }
        Some((active, session.http.clone(), channel))
    }

    // -- events ----------------------------------------------------------

    fn handle_event(self: &Rc<Self>, event: UiEvent) {
        match event {
            UiEvent::LoginSuccess { user, token, label } => {
                self.login_button.set_sensitive(true);
                self.login_error.set_visible(false);
                let account = Account {
                    user_id: Some(user.id),
                    token: token.clone(),
                    label,
                };
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
                let http = {
                    let model = self.model.borrow();
                    model
                        .sessions
                        .get(&account)
                        .map(|session| session.http.clone())
                };
                if let Some(http) = http {
                    let _ = self
                        .cmd_tx
                        .try_send(Command::RefreshGuilds { account, http });
                }
                let active = self.model.borrow().active == Some(account);
                if active {
                    self.update_connection();
                    self.render_account_controls();
                    self.render_guilds();
                }
                if let Some(error) = save_error {
                    self.set_status(&error);
                }
            }
            UiEvent::Guilds { account, guilds } => {
                {
                    let mut model = self.model.borrow_mut();
                    if let Some(session) = model.sessions.get_mut(&account) {
                        session.guilds = guilds;
                    }
                }
                if self.model.borrow().active == Some(account) {
                    self.render_guilds();
                    self.render_channels();
                    self.update_title();
                }
            }
            UiEvent::GuildChannelsFailed {
                account,
                guild,
                message,
            } => {
                if let Some(session) = self.model.borrow_mut().sessions.get_mut(&account) {
                    session.loading_guild_channels.remove(&guild);
                }
                if self.model.borrow().active == Some(account) {
                    self.set_status(&message);
                }
            }
            UiEvent::StoreEvent { account, event } => {
                self.handle_store_event(account, event);
            }
            UiEvent::GatewayClosed {
                account,
                code,
                reason,
            } => {
                let active = {
                    let mut model = self.model.borrow_mut();
                    if let Some(session) = model.sessions.get_mut(&account) {
                        session.connection = ConnState::Disconnected;
                    }
                    model.active == Some(account)
                };
                if active {
                    self.update_connection();
                    let message = if code == Some(4004) {
                        "Gateway rejected the token".to_owned()
                    } else {
                        format!("Gateway disconnected: {reason}")
                    };
                    self.set_status(&message);
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
                        session.voice = Some(VoiceUiState {
                            guild_id,
                            channel_id,
                            muted,
                            deafened,
                            camera,
                            streaming,
                        });
                    }
                }
                if self.model.borrow().active == Some(account) {
                    self.set_status("");
                    self.render_voice_bar();
                    self.render_channels();
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
                    self.set_status("");
                    self.render_voice_bar();
                    self.render_channels();
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
                    self.render_voice_bar();
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
                    self.render_voice_bar();
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
                    self.render_voice_bar();
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

    fn handle_store_event(self: &Rc<Self>, account: AccountId, event: StoreEvent) {
        let active = self.model.borrow().active;
        match event {
            StoreEvent::GuildAdded(_)
            | StoreEvent::GuildUpdated(_)
            | StoreEvent::GuildRemoved(_) => {
                self.sync_guilds(account);
                if active == Some(account) {
                    self.render_guilds();
                    self.render_channels();
                    self.update_title();
                }
            }
            StoreEvent::ChannelAdded(_)
            | StoreEvent::ChannelUpdated(_)
            | StoreEvent::ChannelRemoved(_)
            | StoreEvent::RelationshipsUpdated => {
                if active == Some(account) {
                    self.render_channels();
                    self.update_title();
                }
            }
            StoreEvent::MessageAdded { channel_id, .. }
            | StoreEvent::MessageUpdated { channel_id, .. }
            | StoreEvent::MessageDeleted { channel_id, .. } => {
                if active == Some(account)
                    && self.session_selected_channel(account) == Some(channel_id)
                {
                    self.render_messages();
                }
            }
            StoreEvent::HistoryLoaded(channel_id) => {
                if active == Some(account)
                    && self.session_selected_channel(account) == Some(channel_id)
                {
                    self.render_messages();
                    self.ack_newest(account, channel_id);
                }
            }
            StoreEvent::GuildChannelsLoaded(guild) => {
                if let Some(session) = self.model.borrow_mut().sessions.get_mut(&account) {
                    session.loading_guild_channels.remove(&guild);
                    session.loaded_guild_channels.insert(guild);
                }
                if active == Some(account)
                    && self
                        .model
                        .borrow()
                        .sessions
                        .get(&account)
                        .is_some_and(|session| session.selected_guild == Some(guild))
                {
                    self.set_status("");
                    self.render_channels();
                }
            }
            StoreEvent::Typing {
                channel_id,
                user_id,
            } => {
                if active == Some(account) {
                    self.handle_typing(account, channel_id, user_id);
                }
            }
            StoreEvent::VoiceStateUpdated { .. } => {
                if active == Some(account) {
                    self.render_channels();
                    self.render_voice_bar();
                }
            }
            StoreEvent::Ready if active == Some(account) => {
                self.render_guilds();
                self.render_channels();
                self.update_title();
            }
            _ => {}
        }
    }

    fn sync_guilds(&self, account: AccountId) {
        let mut model = self.model.borrow_mut();
        if let Some(session) = model.sessions.get_mut(&account) {
            session.guilds = session
                .store
                .read(|state| state.ordered_guilds().into_iter().cloned().collect());
        }
    }

    fn session_selected_channel(&self, account: AccountId) -> Option<Snowflake> {
        self.model
            .borrow()
            .sessions
            .get(&account)
            .and_then(|session| session.selected_channel)
    }

    fn ack_newest(self: &Rc<Self>, account: AccountId, channel: Snowflake) {
        let (http, store, message_id) = {
            let model = self.model.borrow();
            let Some(session) = model.sessions.get(&account) else {
                return;
            };
            (
                session.http.clone(),
                session.store.clone(),
                session.store.read(|state| state.last_message_id(channel)),
            )
        };
        let Some(message_id) = message_id else {
            return;
        };
        store.mark_read(channel, message_id);
        let _ = self.cmd_tx.try_send(Command::Ack {
            http,
            channel,
            message_id,
        });
        if self.model.borrow().active == Some(account) {
            self.render_channels();
        }
    }

    fn handle_typing(self: &Rc<Self>, account: AccountId, channel: Snowflake, user_id: Snowflake) {
        if self.session_selected_channel(account) != Some(channel) {
            return;
        }
        let (me, name) = {
            let model = self.model.borrow();
            let Some(session) = model.sessions.get(&account) else {
                return;
            };
            let me = session.user.as_ref().map(|user| user.id);
            let name = session.store.read(|state| {
                state
                    .users
                    .get(&user_id)
                    .map(|user| user.display_name().to_owned())
            });
            (me, name)
        };
        if me == Some(user_id) {
            return;
        }
        let name = name.unwrap_or_else(|| "Someone".to_owned());
        self.typing_label.set_label(&format!("{name} is typing…"));
        self.typing_label.set_visible(true);

        let generation = self.typing_generation.get().wrapping_add(1);
        self.typing_generation.set(generation);
        let cell = self.typing_generation.clone();
        let label = self.typing_label.clone();
        glib::timeout_add_seconds_local_once(TYPING_TIMEOUT_SECS, move || {
            if cell.get() == generation {
                label.set_visible(false);
            }
        });
    }
}

enum ChannelGroup {
    Header(String),
    Row(Channel),
}

/// Splits a sorted guild channel list into uncategorised rows followed by each
/// category header and its children.
fn grouped_channels(channels: &[Channel]) -> Vec<ChannelGroup> {
    let category_ids: HashSet<Snowflake> = channels
        .iter()
        .filter(|channel| channel.kind.is_category())
        .map(|channel| channel.id)
        .collect();

    let mut groups = Vec::new();
    for channel in channels.iter().filter(|channel| {
        !channel.kind.is_category()
            && channel
                .parent_id
                .is_none_or(|parent| !category_ids.contains(&parent))
    }) {
        groups.push(ChannelGroup::Row(channel.clone()));
    }
    for category in channels.iter().filter(|channel| channel.kind.is_category()) {
        groups.push(ChannelGroup::Header(
            category.name.clone().unwrap_or_default(),
        ));
        for channel in channels
            .iter()
            .filter(|channel| !channel.kind.is_category() && channel.parent_id == Some(category.id))
        {
            groups.push(ChannelGroup::Row(channel.clone()));
        }
    }
    groups
}

fn empty_state(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_halign(gtk::Align::Center);
    label.set_margin_top(48);
    label.add_css_class("dim-label");
    label
}

fn set_bold(label: &gtk::Label, bold: bool) {
    if bold {
        let attributes = gtk::pango::AttrList::new();
        attributes.insert(gtk::pango::AttrInt::new_weight(gtk::pango::Weight::Bold));
        label.set_attributes(Some(&attributes));
    } else {
        label.set_attributes(None);
    }
}

fn snippet(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max_chars {
        let mut shortened: String = collapsed.chars().take(max_chars).collect();
        shortened.push('…');
        shortened
    } else if collapsed.is_empty() {
        "[attachment]".to_owned()
    } else {
        collapsed
    }
}
