//! The libadwaita application shell: login page, guild rail, channel sidebar,
//! message list and composer.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk::gdk::prelude::*;
use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::{Account, AccountStore, Snowflake, Token};
use kc_media::portal::{CameraAccess, CameraChoice};
use kc_state::{Channel, ChannelType, Message, StoreEvent};

use crate::markdown::{format_size, format_timestamp, render_markdown};
use crate::settings::{
    BITRATES, CapturePreferences, FRAMERATES, RESOLUTIONS, STREAM_DELAYS,
};
use crate::session::{
    self, AccountId, CameraPurpose, Command, ConnState, Session, UiEvent, VoiceUiState,
};

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
            .default_height(720)
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
            let identifier =
                ashpd::WindowIdentifier::from_native(&identifier_state.window).await;
            tracing::info!(
                available = identifier.is_some(),
                "portal parent window identifier"
            );
            *identifier_state.window_identifier.borrow_mut() = identifier.map(Arc::new);
        });

        if let Ok(path) = std::env::var("KYLECORD_SCREENSHOT") {
            let window = state.window.clone();
            let app = app.clone();
            glib::timeout_add_local_once(Duration::from_secs(3), move || {
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
fn save_window_png(window: &adw::ApplicationWindow, path: &std::path::Path) -> Result<(), String> {
    let width = window.width() as f64;
    let height = window.height() as f64;
    if width <= 0.0 || height <= 0.0 {
        return Err("window has no size".into());
    }
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, width, height);
    let node = snapshot.to_node().ok_or("empty render node")?;
    let renderer = window.renderer().ok_or("no renderer")?;
    let texture = renderer.render_texture(&node, None);
    texture.save_to_png(path).map_err(|error| error.to_string())
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
    guild_name_label: gtk::Label,
    guild_subtitle_label: gtk::Label,
    channel_box: gtk::Box,

    // Channel header.
    channel_glyph_label: gtk::Label,
    channel_glyph_image: gtk::Image,
    channel_title_label: gtk::Label,
    channel_topic_label: gtk::Label,
    search_entry: gtk::SearchEntry,
    members_toggle: gtk::ToggleButton,
    members_panel: gtk::Box,
    members_box: gtk::Box,
    members_count_label: gtk::Label,

    // Message area.
    messages_scroll: gtk::ScrolledWindow,
    messages_box: gtk::Box,
    typing_label: gtk::Label,
    voice_panel: gtk::Box,
    voice_channel_label: gtk::Label,
    voice_status_label: gtk::Label,
    voice_join: gtk::Button,
    voice_leave: gtk::Button,
    voice_mute: gtk::ToggleButton,
    voice_deafen: gtk::ToggleButton,
    voice_camera: gtk::ToggleButton,
    voice_screen: gtk::ToggleButton,
    capture_button: gtk::Button,
    emoji_button: gtk::MenuButton,
    user_avatar: adw::Avatar,
    user_name_label: gtk::Label,
    user_status_label: gtk::Label,
    composer: gtk::TextView,
    send_button: gtk::Button,
    status_bar: gtk::Label,

    model: RefCell<Model>,
    cmd_tx: async_channel::Sender<Command>,
    runtime: Arc<tokio::runtime::Runtime>,
    ui_tx: async_channel::Sender<UiEvent>,
    capture: RefCell<CapturePreferences>,
    window_identifier: RefCell<Option<Arc<ashpd::WindowIdentifier>>>,
    typing_generation: Rc<Cell<u64>>,
    last_rendered_channel: Cell<Option<Snowflake>>,
    message_filter: RefCell<String>,
    search_guard: Cell<bool>,
    show_members: Cell<bool>,
    members_guard: Cell<bool>,
    speaking: RefCell<HashSet<Snowflake>>,
    speaking_generation: RefCell<HashMap<Snowflake, u64>>,
}

impl AppState {
    fn new(
        window: adw::ApplicationWindow,
        accounts: AccountStore,
        cmd_tx: async_channel::Sender<Command>,
        runtime: Arc<tokio::runtime::Runtime>,
        ui_tx: async_channel::Sender<UiEvent>,
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
        guild_box.set_margin_start(8);
        guild_box.set_margin_end(8);
        guild_box.set_valign(gtk::Align::Start);
        let guild_scroll = gtk::ScrolledWindow::new();
        guild_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        guild_scroll.set_width_request(72);
        guild_scroll.set_child(Some(&guild_box));

        // -- channel sidebar ---------------------------------------------
        let guild_name_label = gtk::Label::new(None);
        guild_name_label.set_xalign(0.0);
        guild_name_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        guild_name_label.add_css_class("title-4");

        let guild_subtitle_label = gtk::Label::new(None);
        guild_subtitle_label.set_xalign(0.0);
        guild_subtitle_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        guild_subtitle_label.add_css_class("dim-label");
        guild_subtitle_label.add_css_class("caption");

        let guild_header = gtk::Box::new(gtk::Orientation::Vertical, 2);
        guild_header.set_margin_start(10);
        guild_header.set_margin_end(10);
        guild_header.set_margin_top(10);
        guild_header.set_margin_bottom(4);
        guild_header.append(&guild_name_label);
        guild_header.append(&guild_subtitle_label);

        let channel_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        channel_box.set_margin_top(2);
        channel_box.set_margin_bottom(8);
        channel_box.set_margin_start(6);
        channel_box.set_margin_end(6);
        channel_box.set_valign(gtk::Align::Start);
        let channel_scroll = gtk::ScrolledWindow::new();
        channel_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        channel_scroll.set_vexpand(true);
        channel_scroll.set_child(Some(&channel_box));

        // Shown while a voice channel is selected and the account is not
        // connected to voice. The connected state uses the card below.
        let voice_join = gtk::Button::new();
        voice_join.add_css_class("suggested-action");
        voice_join.set_margin_start(8);
        voice_join.set_margin_end(8);
        voice_join.set_margin_top(4);
        voice_join.set_margin_bottom(4);
        voice_join.set_child(Some(&icon_label_row(
            "audio-input-microphone-symbolic",
            "Join Voice",
        )));
        voice_join.set_visible(false);

        // Voice connected card. It stays visible for as long as the account
        // has a voice session, whichever channel is being browsed.
        let voice_panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
        voice_panel.add_css_class("card");
        voice_panel.set_margin_start(8);
        voice_panel.set_margin_end(8);
        voice_panel.set_margin_top(4);
        voice_panel.set_margin_bottom(4);
        let voice_panel_content = gtk::Box::new(gtk::Orientation::Vertical, 4);
        voice_panel_content.set_margin_top(8);
        voice_panel_content.set_margin_bottom(8);
        voice_panel_content.set_margin_start(8);
        voice_panel_content.set_margin_end(8);

        let voice_connected = gtk::Label::new(Some("Voice Connected"));
        voice_connected.set_xalign(0.0);
        voice_connected.add_css_class("caption");
        voice_connected.add_css_class("success");

        let voice_channel_label = gtk::Label::new(None);
        voice_channel_label.set_xalign(0.0);
        voice_channel_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        set_bold(&voice_channel_label, true);

        // No latency sample is exposed by the voice layer yet, so the ping
        // stays a placeholder rather than an invented number.
        let voice_ping_label = gtk::Label::new(Some("-- ms"));
        voice_ping_label.add_css_class("dim-label");
        voice_ping_label.add_css_class("caption");

        let voice_channel_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        voice_channel_row.append(&voice_channel_label);
        voice_channel_row.append(&voice_ping_label);

        let voice_status_label = gtk::Label::new(None);
        voice_status_label.set_xalign(0.0);
        voice_status_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        voice_status_label.add_css_class("dim-label");
        voice_status_label.add_css_class("caption");
        voice_status_label.set_visible(false);

        let voice_camera = toggle_icon_button("camera-video-symbolic", "Camera");
        voice_camera.set_tooltip_text(Some("Turn On Camera"));
        let voice_screen = toggle_icon_button("video-display-symbolic", "Share");
        voice_screen.set_tooltip_text(Some("Share Your Screen"));
        let capture_button = icon_button("applications-multimedia-symbolic", "Quality");
        capture_button.set_tooltip_text(Some("Capture quality"));
        let voice_leave = icon_button("phone-hangup-symbolic", "Disconnect");
        voice_leave.set_tooltip_text(Some("Disconnect"));

        let voice_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        voice_buttons.append(&voice_camera);
        voice_buttons.append(&voice_screen);
        voice_buttons.append(&voice_leave);

        voice_panel_content.append(&voice_connected);
        voice_panel_content.append(&voice_channel_row);
        voice_panel_content.append(&voice_status_label);
        voice_panel_content.append(&voice_buttons);
        voice_panel.append(&voice_panel_content);
        voice_panel.set_visible(false);

        // User panel, fixed at the bottom of the sidebar.
        let user_avatar = adw::Avatar::new(32, None, true);

        let user_name_label = gtk::Label::new(None);
        user_name_label.set_xalign(0.0);
        user_name_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        set_bold(&user_name_label, true);

        let user_status_label = gtk::Label::new(Some("Offline"));
        user_status_label.set_xalign(0.0);
        user_status_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        user_status_label.add_css_class("dim-label");
        user_status_label.add_css_class("caption");

        let user_text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        user_text.set_hexpand(true);
        user_text.append(&user_name_label);
        user_text.append(&user_status_label);

        let voice_mute = toggle_icon_button("audio-input-microphone-symbolic", "Mute");
        voice_mute.set_tooltip_text(Some("Mute Microphone"));
        voice_mute.set_sensitive(false);
        let voice_deafen = toggle_icon_button("audio-volume-high-symbolic", "Deafen");
        voice_deafen.set_tooltip_text(Some("Deafen"));
        voice_deafen.set_sensitive(false);

        let account_button = gtk::MenuButton::new();
        account_button.add_css_class("flat");
        account_button.add_css_class("circular");
        account_button.set_tooltip_text(Some("Account"));
        if icon_available("emblem-system-symbolic") {
            account_button.set_icon_name("emblem-system-symbolic");
        } else {
            account_button.set_label("Account");
        }
        let account_popover = gtk::Popover::new();
        let account_popover_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        account_popover_box.set_margin_top(6);
        account_popover_box.set_margin_bottom(6);
        account_popover_box.set_margin_start(6);
        account_popover_box.set_margin_end(6);
        account_popover_box.set_width_request(220);
        account_popover.set_child(Some(&account_popover_box));
        account_button.set_popover(Some(&account_popover));

        let user_panel = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        user_panel.set_margin_start(8);
        user_panel.set_margin_end(8);
        user_panel.set_margin_top(6);
        user_panel.set_margin_bottom(8);
        user_panel.append(&user_avatar);
        user_panel.append(&user_text);
        user_panel.append(&voice_mute);
        user_panel.append(&voice_deafen);
        user_panel.append(&capture_button);
        user_panel.append(&account_button);

        let channel_sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        channel_sidebar.set_width_request(240);
        channel_sidebar.add_css_class("sidebar");
        channel_sidebar.append(&guild_header);
        channel_sidebar.append(&channel_scroll);
        channel_sidebar.append(&voice_join);
        channel_sidebar.append(&voice_panel);
        channel_sidebar.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        channel_sidebar.append(&user_panel);

        // -- channel header ----------------------------------------------
        let channel_glyph_label = gtk::Label::new(Some("#"));
        channel_glyph_label.set_xalign(0.5);
        channel_glyph_label.add_css_class("dim-label");
        channel_glyph_label.add_css_class("title-3");

        let channel_glyph_image = gtk::Image::from_icon_name("audio-volume-high-symbolic");
        channel_glyph_image.add_css_class("dim-label");
        channel_glyph_image.set_visible(false);

        let channel_glyph = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        channel_glyph.append(&channel_glyph_label);
        channel_glyph.append(&channel_glyph_image);

        let channel_title_label = gtk::Label::new(Some("kylecord"));
        channel_title_label.set_xalign(0.0);
        channel_title_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        channel_title_label.add_css_class("title-3");

        let channel_topic_label = gtk::Label::new(None);
        channel_topic_label.set_xalign(0.0);
        channel_topic_label.set_hexpand(true);
        channel_topic_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        channel_topic_label.add_css_class("dim-label");
        channel_topic_label.add_css_class("caption");

        let search_entry = gtk::SearchEntry::new();
        search_entry.set_placeholder_text(Some("Search messages"));
        search_entry.set_width_request(200);
        search_entry.set_valign(gtk::Align::Center);

        let members_toggle = toggle_icon_button("system-users-symbolic", "Members");
        members_toggle.set_tooltip_text(Some("Member List"));
        members_toggle.set_active(true);
        members_toggle.set_sensitive(false);

        let channel_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        channel_header.set_margin_start(16);
        channel_header.set_margin_end(12);
        channel_header.set_margin_top(8);
        channel_header.set_margin_bottom(6);
        channel_header.append(&channel_glyph);
        channel_header.append(&channel_title_label);
        channel_header.append(&channel_topic_label);
        channel_header.append(&search_entry);
        channel_header.append(&members_toggle);

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
        composer_scroll.set_child(Some(&composer));

        // The picker is filled in `connect_signals`, where the shared state
        // needed to insert into the buffer exists.
        let emoji_button = gtk::MenuButton::new();
        emoji_button.add_css_class("flat");
        emoji_button.add_css_class("circular");
        emoji_button.set_tooltip_text(Some("Emoji"));
        emoji_button.set_valign(gtk::Align::End);
        emoji_button.set_margin_end(4);
        emoji_button.set_margin_bottom(4);
        if icon_available("face-smile-symbolic") {
            emoji_button.set_icon_name("face-smile-symbolic");
        } else {
            emoji_button.set_label("Emoji");
        }

        let composer_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        composer_box.add_css_class("card");
        composer_box.set_hexpand(true);
        composer_box.append(&composer_scroll);
        composer_box.append(&emoji_button);

        let send_button = gtk::Button::with_label("Send");
        send_button.add_css_class("suggested-action");
        send_button.set_valign(gtk::Align::End);

        let composer_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        composer_row.set_margin_start(12);
        composer_row.set_margin_end(12);
        composer_row.set_margin_top(6);
        composer_row.set_margin_bottom(12);
        composer_row.append(&composer_box);
        composer_row.append(&send_button);

        let message_area = gtk::Box::new(gtk::Orientation::Vertical, 0);
        message_area.set_hexpand(true);
        message_area.append(&channel_header);
        message_area.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        message_area.append(&messages_scroll);
        message_area.append(&typing_label);
        message_area.append(&composer_row);

        // Optional right-hand member list, populated from cached guild members.
        let members_title = gtk::Label::new(Some("Members"));
        members_title.set_xalign(0.0);
        members_title.add_css_class("title-4");

        let members_count_label = gtk::Label::new(None);
        members_count_label.set_xalign(0.0);
        members_count_label.add_css_class("dim-label");
        members_count_label.add_css_class("caption");

        let members_header = gtk::Box::new(gtk::Orientation::Vertical, 2);
        members_header.set_margin_top(10);
        members_header.set_margin_bottom(6);
        members_header.set_margin_start(12);
        members_header.set_margin_end(12);
        members_header.append(&members_title);
        members_header.append(&members_count_label);

        let members_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        members_box.set_margin_bottom(8);
        members_box.set_margin_start(6);
        members_box.set_margin_end(6);
        members_box.set_valign(gtk::Align::Start);
        let members_scroll = gtk::ScrolledWindow::new();
        members_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        members_scroll.set_vexpand(true);
        members_scroll.set_child(Some(&members_box));

        let members_panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
        members_panel.set_width_request(220);
        members_panel.add_css_class("sidebar");
        members_panel.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        members_panel.append(&members_header);
        members_panel.append(&members_scroll);
        members_panel.set_visible(false);

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
        body.append(&channel_sidebar);
        body.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        body.append(&message_area);
        body.append(&members_panel);

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
            guild_name_label,
            guild_subtitle_label,
            channel_box,
            channel_glyph_label,
            channel_glyph_image,
            channel_title_label,
            channel_topic_label,
            search_entry,
            members_toggle,
            members_panel,
            members_box,
            members_count_label,
            messages_scroll,
            messages_box,
            typing_label,
            voice_panel,
            voice_channel_label,
            voice_status_label,
            voice_join,
            voice_leave,
            voice_mute,
            voice_deafen,
            voice_camera,
            voice_screen,
            capture_button,
            emoji_button,
            user_avatar,
            user_name_label,
            user_status_label,
            composer,
            send_button,
            status_bar,
            model: RefCell::new(Model {
                accounts,
                sessions: HashMap::new(),
                active: None,
            }),
            cmd_tx,
            runtime,
            ui_tx,
            capture: RefCell::new(CapturePreferences::load()),
            window_identifier: RefCell::new(None),
            typing_generation: Rc::new(Cell::new(0)),
            last_rendered_channel: Cell::new(None),
            message_filter: RefCell::new(String::new()),
            search_guard: Cell::new(false),
            show_members: Cell::new(true),
            members_guard: Cell::new(false),
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
        self.search_entry.connect_search_changed(move |entry| {
            if app.search_guard.get() {
                return;
            }
            *app.message_filter.borrow_mut() = entry.text().trim().to_lowercase();
            app.render_messages();
        });

        // A picker of common emoji; each button writes into the composer.
        let emoji_popover = gtk::Popover::new();
        let emoji_grid = gtk::Grid::new();
        emoji_grid.set_row_spacing(2);
        emoji_grid.set_column_spacing(2);
        emoji_grid.set_margin_top(6);
        emoji_grid.set_margin_bottom(6);
        emoji_grid.set_margin_start(6);
        emoji_grid.set_margin_end(6);
        for (index, emoji) in COMPOSER_EMOJI.iter().enumerate() {
            let button = gtk::Button::with_label(emoji);
            button.add_css_class("flat");
            let app = self.clone();
            let popover = emoji_popover.clone();
            let emoji = (*emoji).to_owned();
            button.connect_clicked(move |_| {
                app.insert_composer_text(&emoji);
                popover.popdown();
            });
            emoji_grid.attach(&button, (index % 6) as i32, (index / 6) as i32, 1, 1);
        }
        emoji_popover.set_child(Some(&emoji_grid));
        self.emoji_button.set_popover(Some(&emoji_popover));

        let app = self.clone();
        self.members_toggle.connect_toggled(move |button| {
            if app.members_guard.get() {
                return;
            }
            app.show_members.set(button.is_active());
            app.render_members();
        });

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
        let app = self.clone();
        self.capture_button
            .connect_clicked(move |_| app.show_capture_settings());
    }

    fn show_capture_settings(self: &Rc<Self>) {
        let current = self.capture.borrow().clone();
        let dialog = gtk::Dialog::builder()
            .title("Capture quality")
            .transient_for(&self.window)
            .modal(true)
            .build();
        dialog.add_button("Cancel", gtk::ResponseType::Cancel);
        dialog.add_button("Save", gtk::ResponseType::Accept);

        let grid = gtk::Grid::new();
        grid.set_row_spacing(10);
        grid.set_column_spacing(16);
        grid.set_margin_top(12);
        grid.set_margin_bottom(12);
        grid.set_margin_start(12);
        grid.set_margin_end(12);

        let resolution_names: Vec<&str> = RESOLUTIONS.iter().map(|(_, _, name)| *name).collect();
        let resolution = gtk::DropDown::from_strings(&resolution_names);
        let resolution_index = RESOLUTIONS
            .iter()
            .position(|(width, height, _)| *width == current.width && *height == current.height)
            .unwrap_or(1) as u32;
        resolution.set_selected(resolution_index);

        let framerate_names: Vec<String> = FRAMERATES.iter().map(|fps| format!("{fps} fps")).collect();
        let framerate_refs: Vec<&str> = framerate_names.iter().map(String::as_str).collect();
        let framerate = gtk::DropDown::from_strings(&framerate_refs);
        framerate.set_selected(
            FRAMERATES
                .iter()
                .position(|fps| *fps == current.fps)
                .unwrap_or(1) as u32,
        );

        let mut bitrate_names = vec!["Automatic".to_owned()];
        bitrate_names.extend(BITRATES.iter().map(|kbps| format!("{kbps} kbps")));
        let bitrate_refs: Vec<&str> = bitrate_names.iter().map(String::as_str).collect();
        let bitrate = gtk::DropDown::from_strings(&bitrate_refs);
        let bitrate_index = current
            .bitrate_kbps
            .and_then(|kbps| BITRATES.iter().position(|value| *value == kbps))
            .map(|index| index + 1)
            .unwrap_or(0) as u32;
        bitrate.set_selected(bitrate_index);

        let delay_names = ["None", "0.5 s", "1 s", "2 s", "3 s"];
        let delay = gtk::DropDown::from_strings(&delay_names);
        delay.set_selected(
            STREAM_DELAYS
                .iter()
                .position(|delay| *delay == current.stream_delay_ms)
                .unwrap_or(0) as u32,
        );

        let cpu_encoder = gtk::CheckButton::with_label("Use software CPU encoder (x264)");
        cpu_encoder.set_active(current.cpu_encoder);

        for (row, label_text, dropdown) in [
            (0, "Resolution", &resolution),
            (1, "Framerate", &framerate),
            (2, "Bitrate", &bitrate),
            (3, "Go Live delay", &delay),
        ] {
            let label = gtk::Label::new(Some(label_text));
            label.set_xalign(0.0);
            grid.attach(&label, 0, row, 1, 1);
            grid.attach(dropdown, 1, row, 1, 1);
        }

        let encoder_label = gtk::Label::new(Some("Encoder"));
        encoder_label.set_xalign(0.0);
        grid.attach(&encoder_label, 0, 4, 1, 1);
        grid.attach(&cpu_encoder, 1, 4, 1, 1);

        let note = gtk::Label::new(Some(
            "Automatic bitrate is derived from resolution and framerate. Hardware encoding is used when available and falls back to x264. Changes apply to the next capture.",
        ));
        note.set_xalign(0.0);
        note.set_wrap(true);
        note.add_css_class("dim-label");
        note.add_css_class("caption");
        grid.attach(&note, 0, 5, 2, 1);
        dialog.content_area().append(&grid);

        let app = self.clone();
        dialog.connect_response(move |dialog, response| {
            dialog.close();
            if response != gtk::ResponseType::Accept {
                return;
            }
            let (width, height, _) = RESOLUTIONS[resolution.selected() as usize];
            let fps = FRAMERATES[framerate.selected() as usize];
            let bitrate_kbps = match bitrate.selected() as usize {
                0 => None,
                index => BITRATES.get(index - 1).copied(),
            };
            let prefs = CapturePreferences {
                width,
                height,
                fps,
                bitrate_kbps,
                stream_delay_ms: STREAM_DELAYS[delay.selected() as usize],
                cpu_encoder: cpu_encoder.is_active(),
            };
            match prefs.save() {
                Ok(()) => {
                    *app.capture.borrow_mut() = prefs;
                    app.set_status("Capture quality saved for the next capture");
                }
                Err(error) => app.set_status(&format!("capture settings: {error}")),
            }
        });
        dialog.present();
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
        self.clear_search();
        self.last_rendered_channel.set(None);
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

            let cached = !session
                .store
                .read(|state| state.guild_channels(guild_id).is_empty());
            if !cached
                && !session.loaded_guild_channels.contains(&guild_id)
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

        if let Some(guild_id) = channel.guild_id {
            let _ = self.cmd_tx.try_send(Command::SubscribeChannel {
                account: active,
                guild_id,
                channel_id: channel.id,
            });
        }
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

    fn on_screen_toggled(self: &Rc<Self>) {
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
            self.show_share_source_dialog(account);
        }
    }

    /// Asks the account's Tokio side to open the camera portal and enumerate
    /// devices. The response comes back as `UiEvent::CameraChoices`.
    fn request_cameras(self: &Rc<Self>, account: AccountId, purpose: CameraPurpose) {
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

    fn show_camera_dialog(
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

        let dialog = gtk::Dialog::builder()
            .title("Choose camera")
            .transient_for(&self.window)
            .modal(true)
            .build();
        dialog.add_button("Cancel", gtk::ResponseType::Cancel);
        dialog.add_button("Use Camera", gtk::ResponseType::Accept);

        let names: Vec<String> = cameras.iter().map(|camera| camera.name.clone()).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let dropdown = gtk::DropDown::from_strings(&name_refs);
        drop(name_refs);
        dialog.content_area().append(&dropdown);
        let label = gtk::Label::new(Some(match purpose {
            CameraPurpose::Camera => "Select a camera for your camera tile.",
            CameraPurpose::Stream => "Select a camera to use as the Go Live source.",
        }));
        label.set_xalign(0.0);
        label.set_wrap(true);
        dialog.content_area().prepend(&label);

        let app = self.clone();
        dialog.connect_response(move |dialog, response| {
            dialog.close();
            if response == gtk::ResponseType::Accept {
                let Some(camera) = cameras.get(dropdown.selected() as usize) else {
                    app.reset_camera_ui(purpose);
                    return;
                };
                match purpose {
                    CameraPurpose::Camera => {
                        let settings = app.capture.borrow().video_settings();
                        let _ = app.cmd_tx.try_send(Command::SetCameraDevice {
                            account,
                            access: access.clone(),
                            node_id: camera.node_id,
                            settings,
                        });
                        app.set_status("Turning on camera…");
                    }
                    CameraPurpose::Stream => {
                        let settings = app.capture.borrow().video_settings();
                        let delay_ms = app.capture.borrow().stream_delay_ms;
                        let _ = app.cmd_tx.try_send(Command::StartStreamCamera {
                            account,
                            access: access.clone(),
                            node_id: camera.node_id,
                            settings,
                            delay_ms,
                        });
                        app.set_status("Starting camera stream…");
                    }
                }
            } else {
                app.reset_camera_ui(purpose);
            }
        });
        dialog.present();
    }

    fn show_share_source_dialog(self: &Rc<Self>, account: AccountId) {
        let dialog = gtk::Dialog::builder()
            .title("Share")
            .transient_for(&self.window)
            .modal(true)
            .build();
        dialog.add_button("Cancel", gtk::ResponseType::Cancel);
        dialog.add_button("Share Screen", gtk::ResponseType::Accept);
        dialog.add_button("Share Camera", gtk::ResponseType::Other(1));

        let label = gtk::Label::new(Some(
            "Choose a screen-cast portal source, or stream from a camera.",
        ));
        label.set_xalign(0.0);
        label.set_wrap(true);
        dialog.content_area().append(&label);

        let app = self.clone();
        dialog.connect_response(move |dialog, response| {
            dialog.close();
            match response {
                gtk::ResponseType::Accept => {
                    let parent = app.window_identifier.borrow().clone();
                    let settings = app.capture.borrow().video_settings();
                    let delay_ms = app.capture.borrow().stream_delay_ms;
                    let _ = app.cmd_tx.try_send(Command::StartStream {
                        account,
                        parent,
                        settings,
                        delay_ms,
                    });
                    app.set_status("Starting screen share…");
                }
                gtk::ResponseType::Other(1) => {
                    app.request_cameras(account, CameraPurpose::Stream);
                }
                _ => app.reset_stream_ui(),
            }
        });
        dialog.present();
    }

    fn reset_camera_ui(self: &Rc<Self>, purpose: CameraPurpose) {
        if purpose == CameraPurpose::Camera {
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
        } else {
            self.reset_stream_ui();
        }
    }

    fn reset_stream_ui(self: &Rc<Self>) {
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

    /// Renders the sidebar voice controls: the connected card whenever the
    /// account holds a voice session, and the join prompt only for a selected
    /// voice channel that has not been joined.
    fn render_voice_panel(self: &Rc<Self>) {
        let snapshot = {
            let model = self.model.borrow();
            let Some(active) = model.active else {
                self.voice_panel.set_visible(false);
                self.voice_join.set_visible(false);
                return;
            };
            let Some(session) = model.sessions.get(&active) else {
                self.voice_panel.set_visible(false);
                self.voice_join.set_visible(false);
                return;
            };
            let voice = session.voice.clone();
            let selected_voice = session.selected_channel.is_some_and(|channel_id| {
                session.store.read(|state| {
                    state
                        .channels
                        .get(&channel_id)
                        .is_some_and(|channel| channel.kind.is_voice() && !channel.kind.is_text())
                })
            });
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
            (voice, selected_voice, voice_channel)
        };

        let (voice, selected_voice, voice_channel) = snapshot;
        let joined = voice.is_some();

        self.voice_panel.set_visible(joined);
        self.voice_join.set_visible(!joined && selected_voice);
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
        self.render_channel_header();
        self.render_members();
        self.render_voice_panel();
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
        let name = current
            .map(|account| self.account_name(account))
            .unwrap_or_else(|| "Not signed in".to_owned());
        self.user_avatar.set_text(Some(&name));
        self.user_name_label.set_label(&name);

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
            let (channels, unread, voice_users, member_count) = session.store.read(|state| {
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
                let member_count = guild_id
                    .and_then(|guild_id| state.guild_members.get(&guild_id))
                    .map_or(0, HashSet::len);
                (channels, unread, voice_users, member_count)
            });
            Some((
                dm_mode,
                guild_id,
                guild_name,
                channels,
                unread,
                voice_users,
                member_count,
                session.selected_channel,
            ))
        })();
        let Some((
            dm_mode,
            guild_id,
            guild_name,
            channels,
            unread,
            voice_users,
            member_count,
            selected,
        )) = snapshot
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
        // Member counts come from the cached member sets; hide the subtitle
        // when the guild has not delivered any members yet.
        let subtitle = if dm_mode {
            String::new()
        } else {
            match member_count {
                0 => String::new(),
                1 => "1 member".to_owned(),
                count => format!("{count} members"),
            }
        };
        self.guild_name_label.set_label(&header_text);
        self.guild_subtitle_label.set_label(&subtitle);
        self.guild_subtitle_label.set_visible(!subtitle.is_empty());

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

    /// Renders the optional member list from the cached guild members.
    fn render_members(&self) {
        while let Some(child) = self.members_box.first_child() {
            self.members_box.remove(&child);
        }

        let members = {
            let model = self.model.borrow();
            let Some(active) = model.active else {
                self.members_panel.set_visible(false);
                self.members_toggle.set_sensitive(false);
                return;
            };
            let Some(session) = model.sessions.get(&active) else {
                self.members_panel.set_visible(false);
                self.members_toggle.set_sensitive(false);
                return;
            };
            if session.dm_mode {
                None
            } else {
                session
                    .selected_guild
                    .map(|guild| session.store.read(|state| state.guild_member_users(guild)))
            }
        };

        let members = members.unwrap_or_default();
        let available = !members.is_empty();
        self.members_toggle.set_sensitive(available);
        self.members_guard.set(true);
        self.members_toggle
            .set_active(available && self.show_members.get());
        self.members_guard.set(false);
        self.members_panel
            .set_visible(available && self.show_members.get());

        if !available {
            self.members_count_label.set_label("");
            return;
        }

        let count = members.len();
        self.members_count_label.set_label(&match count {
            1 => "1 member".to_owned(),
            count => format!("{count} members"),
        });

        if !self.show_members.get() {
            return;
        }

        // Long guilds would build thousands of rows; cap the list and state
        // the remainder instead.
        const MAX_ROWS: usize = 200;
        for user in members.iter().take(MAX_ROWS) {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.set_margin_start(6);
            row.set_margin_end(6);
            row.set_margin_top(1);
            row.set_margin_bottom(1);
            let name = user.display_name();
            row.append(&adw::Avatar::new(24, Some(name), true));
            let label = gtk::Label::new(Some(name));
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            row.append(&label);
            if user.bot == Some(true) {
                let bot = gtk::Label::new(Some("BOT"));
                bot.add_css_class("dim-label");
                bot.add_css_class("caption");
                row.append(&bot);
            }
            self.members_box.append(&row);
        }
        if count > MAX_ROWS {
            let more = gtk::Label::new(Some(&format!("and {} more", count - MAX_ROWS)));
            more.set_xalign(0.0);
            more.set_margin_start(6);
            more.add_css_class("dim-label");
            more.add_css_class("caption");
            self.members_box.append(&more);
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

    /// Fills the chat header from the selected channel, if any.
    fn render_channel_header(&self) {
        let channel = {
            let model = self.model.borrow();
            let Some(active) = model.active else {
                self.set_channel_header(None);
                return;
            };
            let Some(session) = model.sessions.get(&active) else {
                self.set_channel_header(None);
                return;
            };
            let Some(channel_id) = session.selected_channel else {
                self.set_channel_header(None);
                return;
            };
            session
                .store
                .read(|state| state.channels.get(&channel_id).cloned())
        };
        self.set_channel_header(channel.as_ref());
    }

    fn set_channel_header(&self, channel: Option<&Channel>) {
        let Some(channel) = channel else {
            self.channel_glyph_label.set_label("#");
            self.channel_glyph_label.set_visible(true);
            self.channel_glyph_image.set_visible(false);
            self.channel_title_label.set_label("No channel selected");
            self.channel_topic_label.set_visible(false);
            return;
        };

        let voice = channel.kind.is_voice() && !channel.kind.is_text();
        let dm = matches!(channel.kind, ChannelType::Dm | ChannelType::GroupDm);
        let glyph_icon = voice && icon_available("audio-volume-high-symbolic");
        self.channel_glyph_image.set_visible(glyph_icon);
        self.channel_glyph_label.set_visible(!glyph_icon);
        self.channel_glyph_label.set_label(if dm {
            "@"
        } else if voice {
            "Voice"
        } else {
            "#"
        });

        self.channel_title_label.set_label(&channel.display_name());
        let topic = channel.topic.clone().unwrap_or_default();
        self.channel_topic_label.set_label(&topic);
        self.channel_topic_label
            .set_visible(!topic.trim().is_empty());
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
                self.clear_search();
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

        if channel_changed {
            self.clear_search();
        }

        let Some(channel) = channel else {
            self.messages_box.append(&empty_state("Select a channel"));
            return;
        };
        if channel.kind.is_voice() && !channel.kind.is_text() {
            self.messages_box.append(&empty_state("Voice channel"));
            return;
        }

        let filter = self.message_filter.borrow().clone();
        let messages: Vec<Message> = if filter.is_empty() {
            messages
        } else {
            messages
                .into_iter()
                .filter(|message| message.content.to_lowercase().contains(&filter))
                .collect()
        };

        if messages.is_empty() {
            self.messages_box.append(&empty_state(if filter.is_empty() {
                "No messages yet"
            } else {
                "No matching messages"
            }));
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

        let status = {
            let model = self.model.borrow();
            model
                .active
                .and_then(|id| model.sessions.get(&id))
                .map(|session| match session.connection {
                    ConnState::Connecting => "Connecting…",
                    ConnState::Connected | ConnState::Ready => "Online",
                    ConnState::Disconnected => "Offline",
                })
                .unwrap_or("Offline")
        };
        self.user_status_label.set_label(status);
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

    /// Empties the message search, guarding against the entry emitting a
    /// change while the caller is already re-rendering.
    fn clear_search(&self) {
        self.message_filter.borrow_mut().clear();
        if !self.search_entry.text().is_empty() {
            self.search_guard.set(true);
            self.search_entry.set_text("");
            self.search_guard.set(false);
        }
    }

    // -- composer --------------------------------------------------------

    fn insert_composer_text(&self, text: &str) {
        self.composer.buffer().insert_at_cursor(text);
        self.composer.grab_focus();
    }

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
                    self.render_channel_header();
                    self.update_title();
                    // Discord selects a guild on launch; without this the
                    // sidebar shows "Select a server" and no channels appear
                    // until the user clicks a server.
                    let first = {
                        let model = self.model.borrow();
                        model.sessions.get(&account).and_then(|session| {
                            (session.selected_guild.is_none() && !session.dm_mode)
                                .then(|| session.guilds.first().map(|guild| guild.id))
                                .flatten()
                        })
                    };
                    if let Some(guild_id) = first {
                        self.select_guild(guild_id);
                    }
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
                    self.render_voice_panel();
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
                    self.render_voice_panel();
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
                if self.model.borrow().active == Some(account) {
                    if self.update_voice_flag(|voice| {
                        if voice.camera {
                            None
                        } else {
                            voice.camera = true;
                            Some(())
                        }
                    }).is_some()
                    {
                        self.render_voice_panel();
                    }
                }
            }
            UiEvent::CameraStopped { account } => {
                if self.model.borrow().active == Some(account) {
                    if self.update_voice_flag(|voice| {
                        if voice.camera {
                            voice.camera = false;
                            Some(())
                        } else {
                            None
                        }
                    }).is_some()
                    {
                        self.render_voice_panel();
                    }
                }
            }
            UiEvent::CameraFailed { account } => {
                if self.model.borrow().active == Some(account) {
                    if self.update_voice_flag(|voice| {
                        if voice.camera {
                            voice.camera = false;
                            Some(())
                        } else {
                            None
                        }
                    }).is_some()
                    {
                        self.render_voice_panel();
                    }
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
                    self.render_channel_header();
                    self.update_title();
                }
            }
            StoreEvent::ChannelAdded(_)
            | StoreEvent::ChannelUpdated(_)
            | StoreEvent::ChannelRemoved(_)
            | StoreEvent::RelationshipsUpdated => {
                if active == Some(account) {
                    self.render_channels();
                    self.render_channel_header();
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
            StoreEvent::GuildMembersUpdated(guild) => {
                if active == Some(account)
                    && self
                        .model
                        .borrow()
                        .sessions
                        .get(&account)
                        .is_some_and(|session| session.selected_guild == Some(guild))
                {
                    self.render_channels();
                    self.render_members();
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
                    self.render_voice_panel();
                }
            }
            StoreEvent::Ready if active == Some(account) => {
                self.render_guilds();
                self.render_channels();
                self.render_channel_header();
                self.render_members();
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

/// Emoji offered by the composer picker. These are message content rather than
/// UI labels and are inserted into the composer verbatim.
const COMPOSER_EMOJI: [&str; 12] = [
    "\u{1f44d}",
    "\u{1f44e}",
    "\u{1f604}",
    "\u{1f622}",
    "\u{1f62e}",
    "\u{2764}\u{fe0f}",
    "\u{1f525}",
    "\u{1f389}",
    "\u{2705}",
    "\u{274c}",
    "\u{1f440}",
    "\u{1f64f}",
];

/// Whether the active icon theme provides `name`, so controls can fall back to
/// a text label instead of rendering an empty button.
fn icon_available(name: &str) -> bool {
    gtk::gdk::Display::default()
        .map(|display| gtk::IconTheme::for_display(&display).has_icon(name))
        .unwrap_or(false)
}

fn icon_button(icon: &str, fallback: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("flat");
    if icon_available(icon) {
        button.add_css_class("circular");
        button.set_child(Some(&gtk::Image::from_icon_name(icon)));
    } else {
        button.set_label(fallback);
    }
    button
}

fn toggle_icon_button(icon: &str, fallback: &str) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::new();
    button.add_css_class("flat");
    if icon_available(icon) {
        button.add_css_class("circular");
        button.set_child(Some(&gtk::Image::from_icon_name(icon)));
    } else {
        button.set_label(fallback);
    }
    button
}

/// An icon followed by a text label, used for the Join Voice button.
fn icon_label_row(icon: &str, text: &str) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.set_halign(gtk::Align::Center);
    if icon_available(icon) {
        row.append(&gtk::Image::from_icon_name(icon));
    }
    row.append(&gtk::Label::new(Some(text)));
    row
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
