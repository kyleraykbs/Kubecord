//! The application window: widget construction and the shared state every
//! interface module hangs its behaviour on.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::{AccountStore, Snowflake};

use crate::chat::ChatView;
use crate::images::ImageCache;
use crate::player::LoopingVideo;
use crate::session::{AccountId, Command, Session, UiEvent};
use crate::settings::{CapturePreferences, SIDEBAR_MAX, SIDEBAR_MIN};
use crate::widgets::{first_icon, icon_available, icon_button, toggle_icon_button};

pub(crate) struct Model {
    pub accounts: AccountStore,
    pub sessions: HashMap<AccountId, Session>,
    pub active: Option<AccountId>,
}

pub(crate) struct AppState {
    pub window: adw::ApplicationWindow,
    pub stack: gtk::Stack,
    /// Notices at the top of the window (see [`AppState::notify`]).
    pub notices: gtk::Box,

    /// The login page (also for adding an account).
    pub login: crate::login::LoginPage,

    // Server rail and channel sidebar.
    pub guild_box: gtk::Box,
    pub guild_name_label: gtk::Label,
    /// The server-name dropdown in the sidebar header.
    pub guild_menu: gtk::MenuButton,
    pub guild_menu_arrow: gtk::Image,
    pub channel_box: gtk::Box,

    // User bar.
    pub account_button: gtk::MenuButton,
    pub account_popover: gtk::Popover,
    pub account_popover_box: gtk::Box,
    pub user_avatar: adw::Avatar,
    pub user_avatar_slot: gtk::Overlay,
    pub user_name_label: gtk::Label,
    pub user_status_label: gtk::Label,
    /// Holds the custom status's emoji, ahead of the status text.
    pub user_status_emoji: gtk::Box,

    // Voice controls.
    pub voice_panel: gtk::Box,
    pub voice_channel_label: gtk::Label,
    pub voice_status_label: gtk::Label,
    /// The call's signal bars and ping graph, and what they show.
    pub ping: crate::ping::PingWidgets,
    pub ping_history: RefCell<std::collections::VecDeque<crate::ping::Sample>>,
    pub voice_leave: gtk::Button,
    pub voice_mute: gtk::ToggleButton,
    pub voice_deafen: gtk::ToggleButton,
    pub voice_camera: gtk::ToggleButton,
    pub voice_screen: gtk::ToggleButton,
    /// The settings gear in the user bar, always visible.
    pub capture_button: gtk::Button,
    /// Splits the channel sidebar from the content; dragging resizes it.
    pub sidebar_paned: gtk::Paned,

    // Content header.
    pub banner: adw::Banner,
    pub header_icon: gtk::Image,
    pub channel_title_label: gtk::Label,
    pub channel_topic_label: gtk::Label,
    pub search_entry: gtk::SearchEntry,
    pub pins_button: gtk::Button,
    pub members_toggle: gtk::ToggleButton,
    pub dm_call_button: gtk::Button,

    // Content.
    pub content_stack: gtk::Stack,
    pub split: adw::OverlaySplitView,
    pub messages_scroll: gtk::ScrolledWindow,
    pub message_stack: gtk::Stack,
    /// The call (tiles) above the chat; the divider resizes it.
    pub call_split: gtk::Paned,
    pub call_area: gtk::ScrolledWindow,
    /// Messages and the composer, under the call.
    pub chat_box: gtk::Box,
    pub message_overlay: gtk::Overlay,
    pub toolbar: gtk::Box,
    pub hover: RefCell<crate::chat::Hover>,
    pub messages_list: gtk::ListBox,
    pub history_spinner: gtk::Spinner,
    pub chat_placeholder: gtk::Box,
    pub voice_view: gtk::Box,
    /// Participant tiles of the open voice view, for speaking updates.
    pub voice_tiles: RefCell<HashMap<Snowflake, gtk::Widget>>,
    /// Cameras and streams being received, by account, person and whether
    /// it is their stream (`true`) or their camera.
    pub videos: RefCell<HashMap<(AccountId, Snowflake, bool), crate::live::LiveVideo>>,
    /// The enlarged video in the voice view: `(user, is_stream)`. Clicking a
    /// tile focuses it; clicking the big one again clears this.
    pub focused_video: Cell<Option<(Snowflake, bool)>>,
    /// Cameras and streams moved into windows of their own.
    pub popouts: RefCell<HashMap<crate::voice::VideoKey, crate::voice::Popout>>,
    /// Separate non-modal windows (image viewers) by key.
    pub aux_windows: RefCell<HashMap<String, crate::windows::AuxWindow>>,
    /// With a video enlarged: the row of other tiles under it is hidden.
    pub members_hidden: Cell<bool>,
    /// A made-up call shown by `KUBECORD_VIDEO_DEMO`.
    pub voice_demo: RefCell<Option<crate::voice::DemoCall>>,
    /// The videos whose information panel is showing, and what it needs.
    pub video_info: RefCell<crate::video_info::VideoInfo>,
    /// The ringing card for an incoming call, and its channel.
    pub incoming_call: RefCell<Option<(Snowflake, adw::Dialog)>>,
    /// Desktop notifications shown, by account and channel, so opening the
    /// channel takes them away; and how many arrived while the window was
    /// not looked at (for the window title).
    pub notifications: RefCell<crate::notifications::Shown>,
    /// Interface sounds: Discord's, downloaded, and what is ringing.
    pub sounds: crate::sounds::Sounds,
    /// Remembered per-user voice playback volumes (1.0 = unchanged), for the
    /// menu slider to reopen at the chosen level.
    pub user_volumes: RefCell<HashMap<Snowflake, f32>>,
    /// Remembered per-user stream (screen-share sound) playback volumes
    /// (1.0 = unchanged), kept apart from their voice.
    pub stream_volumes: RefCell<HashMap<Snowflake, f32>>,
    /// The last voice channel clicked and when, to recognise double-clicks.
    pub last_channel_click: Cell<Option<(Snowflake, std::time::Instant)>>,
    pub typing_label: gtk::Label,

    // Composer.
    pub composer_area: gtk::Box,
    pub composer_box: gtk::Box,
    pub composer: gtk::TextView,
    pub composer_hint: gtk::Label,
    pub context_bar: gtk::Box,
    pub context_label: gtk::Label,
    pub context_ping: gtk::ToggleButton,
    pub context_cancel: gtk::Button,
    pub upload_tray: gtk::Box,
    pub attach_button: gtk::Button,
    pub gif_button: gtk::MenuButton,
    /// Whether others see you typing; mirrors the setting in Features.
    pub typing_toggle: gtk::Button,
    pub emoji_button: gtk::MenuButton,
    /// Sends what is in the composer (as Enter does).
    pub send_button: gtk::Button,
    pub blocked_label: gtk::Label,
    pub completion: gtk::Popover,
    pub completion_list: gtk::ListBox,

    // Friends page and member list.
    pub friends_box: gtk::Box,
    pub members_box: gtk::Box,
    pub members_count_label: gtk::Label,

    pub model: RefCell<Model>,
    pub cmd_tx: async_channel::Sender<Command>,
    pub runtime: Arc<tokio::runtime::Runtime>,
    pub ui_tx: async_channel::Sender<UiEvent>,
    pub images: ImageCache,
    pub capture: RefCell<CapturePreferences>,
    pub window_identifier: RefCell<Option<Arc<ashpd::WindowIdentifier>>>,
    pub chat: RefCell<ChatView>,
    pub show_members: Cell<bool>,
    pub members_guard: Cell<bool>,
    pub speaking: RefCell<HashSet<Snowflake>>,
    /// Sidebar rows of people in voice channels, for speaking updates.
    pub voice_rows: RefCell<HashMap<Snowflake, gtk::Widget>>,
    /// Coalesces bursts of store events into one sidebar redraw.
    pub sidebar_redraw_queued: Cell<bool>,
    /// Which list the channel sidebar last showed: `(dm_mode, guild)`.
    /// Redrawing the same list keeps its scroll position.
    pub sidebar_shown: Cell<Option<(bool, Option<Snowflake>)>>,
    /// Hash of what the rail last drew, to skip a rebuild that would redraw
    /// the same thing. Reset to `None` to force the next rebuild.
    pub rail_fingerprint: Cell<Option<u64>>,
    /// Hash of what the channel list last drew, for the same reason.
    pub channels_fingerprint: Cell<Option<u64>>,
    /// Cached guild tiles by id, with a hash of their drawn state, so a rail
    /// rebuild reuses the tiles (and their loaded avatars) that did not
    /// change instead of recreating one per server.
    pub guild_slots: RefCell<HashMap<Snowflake, (u64, gtk::Overlay)>>,
    /// Inline GIF players and the widgets showing them.
    pub players: RefCell<Vec<(glib::WeakRef<gtk::Widget>, std::rc::Weak<LoopingVideo>)>>,
    pub players_update_queued: Cell<bool>,
}

impl AppState {
    pub(crate) fn new(
        window: adw::ApplicationWindow,
        accounts: AccountStore,
        cmd_tx: async_channel::Sender<Command>,
        runtime: Arc<tokio::runtime::Runtime>,
        ui_tx: async_channel::Sender<UiEvent>,
    ) -> Rc<Self> {
        crate::style::install();

        let stack = gtk::Stack::new();
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);

        // -- login -------------------------------------------------------
        let login = crate::login::LoginPage::new();
        stack.add_named(&login.page, Some("login"));

        // -- server rail -------------------------------------------------
        let guild_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        guild_box.set_margin_top(10);
        guild_box.set_margin_bottom(10);
        guild_box.set_valign(gtk::Align::Start);
        let guild_scroll = gtk::ScrolledWindow::new();
        guild_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::External);
        guild_scroll.set_width_request(72);
        guild_scroll.set_child(Some(&guild_box));
        guild_scroll.add_css_class("kc-rail");

        // -- channel sidebar ---------------------------------------------
        let guild_name_label = gtk::Label::new(None);
        guild_name_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        guild_name_label.add_css_class("heading");

        // The server name is a dropdown: in a guild it opens the server
        // menu, in DMs it is just a title.
        let guild_menu_arrow = gtk::Image::from_icon_name("pan-down-symbolic");
        let guild_menu_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        guild_menu_box.append(&guild_name_label);
        guild_menu_box.append(&guild_menu_arrow);
        let guild_menu = gtk::MenuButton::new();
        guild_menu.add_css_class("flat");
        guild_menu.add_css_class("kc-guild-menu");
        guild_menu.set_child(Some(&guild_menu_box));

        let sidebar_header = adw::HeaderBar::new();
        sidebar_header.set_show_end_title_buttons(false);
        sidebar_header.set_show_start_title_buttons(false);
        sidebar_header.set_title_widget(Some(&guild_menu));

        let channel_box = gtk::Box::new(gtk::Orientation::Vertical, 1);
        channel_box.set_margin_top(6);
        channel_box.set_margin_bottom(8);
        channel_box.set_margin_start(8);
        channel_box.set_margin_end(8);
        channel_box.set_valign(gtk::Align::Start);
        let channel_scroll = gtk::ScrolledWindow::new();
        channel_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        channel_scroll.set_vexpand(true);
        channel_scroll.set_child(Some(&channel_box));

        // Voice connected card. It stays visible for as long as the account
        // has a voice session, whichever channel is being browsed.
        let voice_panel = gtk::Box::new(gtk::Orientation::Vertical, 4);
        voice_panel.add_css_class("card");
        voice_panel.set_margin_start(8);
        voice_panel.set_margin_end(8);
        voice_panel.set_margin_top(4);
        voice_panel.set_margin_bottom(6);

        let voice_connected = gtk::Label::new(Some("Voice Connected"));
        voice_connected.set_xalign(0.0);
        voice_connected.add_css_class("caption-heading");
        voice_connected.add_css_class("kc-voice-connected");

        let voice_channel_label = gtk::Label::new(None);
        voice_channel_label.set_xalign(0.0);
        voice_channel_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        voice_channel_label.add_css_class("caption");
        voice_channel_label.add_css_class("dim-label");

        let voice_status_label = gtk::Label::new(None);
        voice_status_label.set_xalign(0.0);
        voice_status_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        voice_status_label.add_css_class("dim-label");
        voice_status_label.add_css_class("caption");
        voice_status_label.set_visible(false);

        let voice_camera = toggle_icon_button("camera-video-symbolic", "Turn On Camera");
        let voice_screen = toggle_icon_button("video-display-symbolic", "Share Your Screen");
        let voice_leave = icon_button("call-stop-symbolic", "Disconnect");
        voice_leave.add_css_class("error");

        let voice_text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        voice_text.set_hexpand(true);
        voice_text.append(&voice_connected);
        voice_text.append(&voice_channel_label);
        let ping = crate::ping::PingWidgets::new();
        let voice_top = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        voice_top.append(&ping.button);
        voice_top.append(&voice_text);
        voice_top.append(&voice_leave);

        let voice_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        voice_buttons.set_homogeneous(true);
        voice_buttons.append(&voice_camera);
        voice_buttons.append(&voice_screen);

        let voice_content = gtk::Box::new(gtk::Orientation::Vertical, 4);
        voice_content.set_margin_top(8);
        voice_content.set_margin_bottom(8);
        voice_content.set_margin_start(10);
        voice_content.set_margin_end(8);
        voice_content.append(&voice_top);
        voice_content.append(&voice_status_label);
        voice_content.append(&voice_buttons);
        voice_panel.append(&voice_content);
        voice_panel.set_visible(false);

        // User bar, fixed at the bottom of the sidebar. The avatar and name
        // open the status and account menu.
        let user_avatar = adw::Avatar::new(40, None, true);
        let user_avatar_slot = gtk::Overlay::new();
        user_avatar_slot.set_child(Some(&user_avatar));
        // Hug the avatar: stretched to the bar's height, the slot put the
        // status dot below the avatar instead of on its corner.
        user_avatar_slot.set_valign(gtk::Align::Center);

        // Names wrap at spaces onto a second line; a single word too long
        // for the bar is cut with an ellipsis instead of broken.
        let user_name_label = gtk::Label::new(None);
        user_name_label.set_xalign(0.0);
        user_name_label.set_wrap(true);
        user_name_label.set_wrap_mode(gtk::pango::WrapMode::Word);
        user_name_label.set_lines(2);
        user_name_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        // A small natural width lets the box hand the name the leftover
        // space; measured at its full width, the wrapped height came out
        // short of what the narrower allocation needed.
        user_name_label.set_max_width_chars(1);
        user_name_label.set_hexpand(true);
        user_name_label.add_css_class("heading");

        let user_status_label = gtk::Label::new(Some("Offline"));
        user_status_label.set_xalign(0.0);
        user_status_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        user_status_label.add_css_class("dim-label");
        user_status_label.add_css_class("caption");

        let user_status_emoji = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        user_status_emoji.set_visible(false);
        let user_status_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        user_status_row.append(&user_status_emoji);
        user_status_row.append(&user_status_label);

        let user_text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        user_text.set_valign(gtk::Align::Center);
        user_text.append(&user_name_label);
        user_text.append(&user_status_row);

        let user_identity = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        user_identity.append(&user_avatar_slot);
        user_identity.append(&user_text);

        let account_button = gtk::MenuButton::new();
        account_button.add_css_class("flat");
        account_button.set_hexpand(true);
        account_button.set_child(Some(&user_identity));
        account_button.set_tooltip_text(Some("Status and accounts"));
        account_button.set_direction(gtk::ArrowType::Up);
        let account_popover = gtk::Popover::new();
        let account_popover_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        account_popover_box.set_margin_top(6);
        account_popover_box.set_margin_bottom(6);
        account_popover_box.set_margin_start(6);
        account_popover_box.set_margin_end(6);
        account_popover_box.set_width_request(260);
        account_popover.set_child(Some(&account_popover_box));
        account_button.set_popover(Some(&account_popover));

        let voice_mute = toggle_icon_button("microphone-sensitivity-muted-symbolic", "Mute");
        voice_mute.set_sensitive(false);
        let voice_deafen = toggle_icon_button("audio-volume-muted-symbolic", "Deafen");
        voice_deafen.set_sensitive(false);
        let capture_button = icon_button("emblem-system-symbolic", "User Settings");

        let user_bar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        user_bar.add_css_class("kc-userbar");
        let user_bar_inner = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        user_bar_inner.set_margin_start(4);
        user_bar_inner.set_margin_end(6);
        user_bar_inner.set_margin_top(4);
        user_bar_inner.set_margin_bottom(4);
        user_bar_inner.set_hexpand(true);
        user_bar_inner.append(&account_button);
        user_bar_inner.append(&voice_mute);
        user_bar_inner.append(&voice_deafen);
        user_bar_inner.append(&capture_button);
        user_bar.append(&user_bar_inner);

        let sidebar_body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar_body.append(&channel_scroll);
        sidebar_body.append(&voice_panel);
        sidebar_body.append(&user_bar);

        let channel_sidebar = adw::ToolbarView::new();
        channel_sidebar.add_top_bar(&sidebar_header);
        channel_sidebar.set_content(Some(&sidebar_body));
        // The paned below sets the width; the minimum keeps the user bar's
        // buttons on screen.
        channel_sidebar.set_width_request(SIDEBAR_MIN);
        channel_sidebar.add_css_class("sidebar-pane");

        // -- content header ----------------------------------------------
        let header_icon = gtk::Image::new();
        header_icon.add_css_class("dim-label");

        let channel_title_label = gtk::Label::new(None);
        channel_title_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        channel_title_label.add_css_class("heading");

        let channel_topic_label = gtk::Label::new(None);
        channel_topic_label.set_xalign(0.0);
        channel_topic_label.set_hexpand(true);
        channel_topic_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        channel_topic_label.add_css_class("dim-label");
        channel_topic_label.add_css_class("caption");
        channel_topic_label.set_margin_start(8);

        let title_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        title_box.set_margin_start(6);
        title_box.append(&header_icon);
        title_box.append(&channel_title_label);
        title_box.append(&channel_topic_label);

        let search_entry = gtk::SearchEntry::new();
        search_entry.set_placeholder_text(Some("Search"));
        search_entry.set_width_request(180);
        search_entry.set_valign(gtk::Align::Center);

        let pins_button = icon_button("view-pin-symbolic", "Pinned Messages");
        let members_toggle = toggle_icon_button("system-users-symbolic", "Member List");
        members_toggle.set_active(true);

        // Start/join a call in a DM, shown only for DM and group-DM channels.
        let dm_call_button = icon_button("call-start-symbolic", "Start Voice Call");
        dm_call_button.set_visible(false);

        let content_header = adw::HeaderBar::new();
        content_header.set_show_title(false);
        content_header.pack_start(&title_box);
        content_header.pack_end(&search_entry);
        content_header.pack_end(&members_toggle);
        content_header.pack_end(&pins_button);
        content_header.pack_end(&dm_call_button);

        let banner = adw::Banner::new("");
        banner.set_revealed(false);

        // -- messages ----------------------------------------------------
        let messages_list = gtk::ListBox::new();
        messages_list.set_selection_mode(gtk::SelectionMode::None);
        messages_list.add_css_class("kc-messages");
        messages_list.set_valign(gtk::Align::End);

        let history_spinner = gtk::Spinner::new();
        history_spinner.set_margin_top(12);
        history_spinner.set_margin_bottom(12);
        history_spinner.set_visible(false);

        // The spacer absorbs spare height so a short history sits at the
        // bottom of the pane, next to the composer.
        let messages_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        messages_column.append(&spacer);
        messages_column.append(&history_spinner);
        messages_column.append(&messages_list);
        // The typing line above the composer supplies the rest of the gap.
        messages_column.set_margin_bottom(4);

        let messages_scroll = gtk::ScrolledWindow::new();
        messages_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        messages_scroll.set_vexpand(true);
        messages_scroll.set_child(Some(&messages_column));

        let chat_placeholder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        chat_placeholder.set_vexpand(true);
        let placeholder_scroll = gtk::ScrolledWindow::new();
        placeholder_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        placeholder_scroll.set_child(Some(&chat_placeholder));

        // Voice channels show who is connected instead of a message list.
        let voice_view = gtk::Box::new(gtk::Orientation::Vertical, 8);
        voice_view.set_valign(gtk::Align::Center);
        voice_view.set_margin_top(12);
        voice_view.set_margin_bottom(12);
        voice_view.set_margin_start(12);
        voice_view.set_margin_end(12);
        let voice_scroll = gtk::ScrolledWindow::new();
        voice_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        voice_scroll.set_child(Some(&voice_view));

        // One hover toolbar for the whole list, placed over the hovered row
        // by `connect_chat`.
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        toolbar.add_css_class("kc-toolbar");
        toolbar.set_visible(false);
        let message_overlay = gtk::Overlay::new();
        message_overlay.set_child(Some(&messages_scroll));
        message_overlay.add_overlay(&toolbar);

        let message_stack = gtk::Stack::new();
        message_stack.add_named(&message_overlay, Some("messages"));
        message_stack.add_named(&placeholder_scroll, Some("placeholder"));
        message_stack.set_vexpand(true);
        // The placeholder page is taller than a short window allows; it must
        // not set the minimum height of the message list.
        message_stack.set_vhomogeneous(false);

        // -- composer ----------------------------------------------------
        let context_label = gtk::Label::new(None);
        context_label.set_xalign(0.0);
        context_label.set_hexpand(true);
        context_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let context_ping = gtk::ToggleButton::with_label("@ ON");
        context_ping.add_css_class("flat");
        context_ping.set_active(true);
        context_ping.set_tooltip_text(Some("Notify the author of the message you reply to"));
        let context_cancel = icon_button("window-close-symbolic", "Cancel");
        let context_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        context_bar.add_css_class("kc-composer-bar");
        context_bar.append(&context_label);
        context_bar.append(&context_ping);
        context_bar.append(&context_cancel);
        context_bar.set_visible(false);

        let upload_tray = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        upload_tray.add_css_class("kc-composer-bar");
        upload_tray.set_visible(false);

        let composer = gtk::TextView::new();
        composer.set_wrap_mode(gtk::WrapMode::WordChar);
        composer.set_accepts_tab(false);
        composer.set_top_margin(16);
        composer.set_bottom_margin(16);
        composer.set_left_margin(4);
        composer.set_right_margin(4);

        let composer_hint = gtk::Label::new(None);
        composer_hint.set_xalign(0.0);
        composer_hint.set_margin_start(4);
        composer_hint.add_css_class("dim-label");
        composer_hint.set_can_target(false);
        composer_hint.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let composer_scroll = gtk::ScrolledWindow::new();
        composer_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        composer_scroll.set_max_content_height(220);
        composer_scroll.set_propagate_natural_height(true);
        composer_scroll.set_child(Some(&composer));

        let composer_overlay = gtk::Overlay::new();
        composer_overlay.set_hexpand(true);
        // Centre the text vertically: the round buttons beside it are taller
        // than a single line, so a top-aligned entry looked lifted.
        composer_overlay.set_valign(gtk::Align::Center);
        composer_overlay.set_child(Some(&composer_scroll));
        composer_overlay.add_overlay(&composer_hint);
        composer_hint.set_valign(gtk::Align::Center);

        let attach_button = icon_button("list-add-symbolic", "Attach Files");
        attach_button.set_valign(gtk::Align::Center);
        attach_button.add_css_class("circular");

        // Shows and switches whether others see you typing.
        let typing_toggle = gtk::Button::new();
        typing_toggle.add_css_class("flat");
        typing_toggle.add_css_class("circular");
        typing_toggle.set_valign(gtk::Align::Center);

        let gif_button = gtk::MenuButton::new();
        gif_button.add_css_class("flat");
        // A child label instead of `set_label`, which adds a menu arrow.
        gif_button.set_child(Some(&gtk::Label::new(Some("GIF"))));
        gif_button.set_tooltip_text(Some("Send a GIF"));
        gif_button.set_valign(gtk::Align::Center);
        gif_button.set_direction(gtk::ArrowType::Up);

        let emoji_button = gtk::MenuButton::new();
        emoji_button.add_css_class("flat");
        emoji_button.add_css_class("circular");
        emoji_button.set_tooltip_text(Some("Emoji"));
        emoji_button.set_valign(gtk::Align::Center);
        emoji_button.set_direction(gtk::ArrowType::Up);
        if icon_available("face-smile-symbolic") {
            emoji_button.set_icon_name("face-smile-symbolic");
        } else {
            emoji_button.set_label("Emoji");
        }

        let send_button = icon_button(
            first_icon(&["paper-plane-symbolic", "document-send-symbolic", "mail-send-symbolic"]),
            "Send Message",
        );
        send_button.add_css_class("circular");
        send_button.set_valign(gtk::Align::Center);

        let composer_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        composer_box.add_css_class("kc-composer");
        composer_box.append(&attach_button);
        composer_box.append(&composer_overlay);
        composer_box.append(&typing_toggle);
        composer_box.append(&emoji_button);
        composer_box.append(&gif_button);
        composer_box.append(&send_button);
        // Padding lives on the children so the rounded background reaches
        // the edges.
        attach_button.set_margin_start(6);
        send_button.set_margin_end(6);

        let completion_list = gtk::ListBox::new();
        completion_list.set_selection_mode(gtk::SelectionMode::Browse);
        completion_list.add_css_class("navigation-sidebar");
        let completion = gtk::Popover::new();
        completion.set_child(Some(&completion_list));
        completion.set_autohide(false);
        completion.set_can_focus(false);
        completion.set_has_arrow(false);
        completion.set_position(gtk::PositionType::Top);
        completion.set_halign(gtk::Align::Start);
        completion.set_parent(&composer_box);

        let blocked_label = gtk::Label::new(Some(
            "You do not have permission to send messages in this channel.",
        ));
        blocked_label.add_css_class("dim-label");
        blocked_label.set_margin_top(12);
        blocked_label.set_margin_bottom(12);
        blocked_label.set_visible(false);

        // "Someone is typing", just above the bar. Its line is reserved so
        // the bar does not move when someone starts typing.
        let typing_label = gtk::Label::new(None);
        typing_label.set_xalign(0.0);
        typing_label.set_margin_start(8);
        typing_label.set_margin_bottom(2);
        typing_label.add_css_class("caption");
        typing_label.add_css_class("dim-label");
        typing_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        typing_label.set_height_request(18);
        typing_label.set_valign(gtk::Align::End);

        let composer_area = gtk::Box::new(gtk::Orientation::Vertical, 0);
        composer_area.set_margin_start(16);
        composer_area.set_margin_end(16);
        // The same gap below the bar as beside it.
        composer_area.set_margin_bottom(16);
        composer_area.append(&typing_label);
        composer_area.append(&context_bar);
        composer_area.append(&upload_tray);
        composer_area.append(&composer_box);
        composer_area.append(&blocked_label);

        let chat_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        chat_box.append(&message_stack);
        chat_box.append(&composer_area);
        chat_box.set_size_request(-1, 160);

        // A call sits above the chat, as in Discord, and the divider between
        // them drags. Voice channels show the call alone, and without a
        // call the chat has the whole column.
        voice_scroll.set_size_request(-1, crate::settings::CALL_HEIGHT_MIN);
        voice_scroll.set_visible(false);
        let call_split = gtk::Paned::new(gtk::Orientation::Vertical);
        call_split.set_start_child(Some(&voice_scroll));
        call_split.set_end_child(Some(&chat_box));
        call_split.set_resize_start_child(false);
        call_split.set_shrink_start_child(false);
        call_split.set_shrink_end_child(false);
        call_split.set_vexpand(true);

        let chat_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        chat_column.append(&call_split);

        // -- member list -------------------------------------------------
        let members_count_label = gtk::Label::new(None);
        members_count_label.set_xalign(0.0);
        members_count_label.add_css_class("caption");
        members_count_label.add_css_class("dim-label");
        members_count_label.set_margin_top(10);
        members_count_label.set_margin_start(14);
        members_count_label.set_margin_bottom(2);

        let members_box = gtk::Box::new(gtk::Orientation::Vertical, 1);
        members_box.set_margin_bottom(8);
        members_box.set_margin_start(8);
        members_box.set_margin_end(8);
        members_box.set_valign(gtk::Align::Start);
        let members_scroll = gtk::ScrolledWindow::new();
        members_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        members_scroll.set_vexpand(true);
        members_scroll.set_child(Some(&members_box));

        let members_panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
        members_panel.append(&members_count_label);
        members_panel.append(&members_scroll);

        let split = adw::OverlaySplitView::new();
        split.set_sidebar_position(gtk::PackType::End);
        split.set_content(Some(&chat_column));
        split.set_sidebar(Some(&members_panel));
        split.set_min_sidebar_width(220.0);
        split.set_max_sidebar_width(260.0);
        split.set_show_sidebar(false);

        // -- friends page ------------------------------------------------
        let friends_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let friends_clamp = adw::Clamp::new();
        friends_clamp.set_maximum_size(760);
        friends_clamp.set_child(Some(&friends_box));
        let friends_scroll = gtk::ScrolledWindow::new();
        friends_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        friends_scroll.set_child(Some(&friends_clamp));

        let content_stack = gtk::Stack::new();
        content_stack.add_named(&split, Some("chat"));
        content_stack.add_named(&friends_scroll, Some("friends"));

        let content = adw::ToolbarView::new();
        content.add_top_bar(&content_header);
        content.add_top_bar(&banner);
        content.set_content(Some(&content_stack));
        content.set_hexpand(true);

        // The sidebar keeps its width when the window resizes; dragging the
        // handle changes it.
        let capture = CapturePreferences::load();
        capture.apply_devices();
        call_split.set_position(capture.call_height);
        let sidebar_paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        sidebar_paned.set_start_child(Some(&channel_sidebar));
        sidebar_paned.set_end_child(Some(&content));
        sidebar_paned.set_resize_start_child(false);
        sidebar_paned.set_shrink_start_child(false);
        sidebar_paned.set_resize_end_child(true);
        sidebar_paned.set_shrink_end_child(false);
        sidebar_paned.set_position(capture.sidebar_width.clamp(SIDEBAR_MIN, SIDEBAR_MAX));
        sidebar_paned.set_hexpand(true);

        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.append(&guild_scroll);
        body.append(&sidebar_paned);
        stack.add_named(&body, Some("chat"));

        // Notices drop in at the top, under the header bars, where they do
        // not cover the message box.
        let notices = gtk::Box::new(gtk::Orientation::Vertical, 6);
        notices.set_halign(gtk::Align::Center);
        notices.set_valign(gtk::Align::Start);
        notices.set_margin_top(54);
        let page = gtk::Overlay::new();
        page.set_child(Some(&stack));
        page.add_overlay(&notices);
        window.set_content(Some(&page));
        window.set_size_request(720, 420);

        let images = ImageCache::new(runtime.clone());

        Rc::new(Self {
            window,
            stack,
            notices,
            login,
            guild_box,
            guild_name_label,
            guild_menu,
            guild_menu_arrow,
            channel_box,
            account_button,
            account_popover,
            account_popover_box,
            user_avatar,
            user_avatar_slot,
            user_name_label,
            user_status_label,
            user_status_emoji,
            voice_panel,
            voice_channel_label,
            voice_status_label,
            ping,
            ping_history: RefCell::new(std::collections::VecDeque::new()),
            voice_leave,
            voice_mute,
            voice_deafen,
            voice_camera,
            voice_screen,
            capture_button,
            sidebar_paned,
            banner,
            header_icon,
            channel_title_label,
            channel_topic_label,
            search_entry,
            pins_button,
            members_toggle,
            dm_call_button,
            content_stack,
            split,
            messages_scroll,
            message_stack: message_stack.clone(),
            call_split,
            call_area: voice_scroll,
            chat_box,
            message_overlay,
            toolbar,
            hover: RefCell::new(crate::chat::Hover::default()),
            messages_list,
            history_spinner,
            chat_placeholder,
            voice_view,
            voice_tiles: RefCell::new(HashMap::new()),
            videos: RefCell::new(HashMap::new()),
            focused_video: Cell::new(None),
            popouts: RefCell::new(HashMap::new()),
            aux_windows: RefCell::new(HashMap::new()),
            members_hidden: Cell::new(false),
            voice_demo: RefCell::new(None),
            video_info: RefCell::default(),
            incoming_call: RefCell::new(None),
            notifications: RefCell::new(Default::default()),
            sounds: Default::default(),
            user_volumes: RefCell::new(HashMap::new()),
            stream_volumes: RefCell::new(HashMap::new()),
            last_channel_click: Cell::new(None),
            typing_label,
            composer_area,
            composer_box,
            composer,
            composer_hint,
            context_bar,
            context_label,
            context_ping,
            context_cancel,
            upload_tray,
            attach_button,
            gif_button,
            typing_toggle,
            emoji_button,
            send_button,
            blocked_label,
            completion,
            completion_list,
            friends_box,
            members_box,
            members_count_label,
            model: RefCell::new(Model {
                accounts,
                sessions: HashMap::new(),
                active: None,
            }),
            cmd_tx,
            runtime,
            ui_tx,
            images,
            capture: RefCell::new(capture),
            window_identifier: RefCell::new(None),
            chat: RefCell::new(ChatView::default()),
            show_members: Cell::new(true),
            members_guard: Cell::new(false),
            speaking: RefCell::new(HashSet::new()),
            voice_rows: RefCell::new(HashMap::new()),
            sidebar_redraw_queued: Cell::new(false),
            sidebar_shown: Cell::new(None),
            rail_fingerprint: Cell::new(None),
            channels_fingerprint: Cell::new(None),
            guild_slots: RefCell::new(HashMap::new()),
            players: RefCell::new(Vec::new()),
            players_update_queued: Cell::new(false),
        })
    }

    /// Shows a transient message at the top of the window. Empty text is
    /// ignored, so callers can pass a cleared status through.
    pub(crate) fn set_status(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.notify(text, None, 4);
    }

    /// A notice at the top of the window, with an optional action button.
    /// It goes after `seconds`, or when closed or acted on; at most three
    /// show at once.
    pub(crate) fn notify(&self, text: &str, action: Option<(&str, NoticeAction)>, seconds: u32) {
        let label = gtk::Label::new(Some(text));
        label.set_wrap(true);
        label.set_max_width_chars(60);
        label.set_xalign(0.0);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("kc-notice");
        row.append(&label);
        let revealer = gtk::Revealer::new();
        revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        revealer.set_child(Some(&row));
        if let Some((title, callback)) = action {
            let button = gtk::Button::with_label(title);
            button.add_css_class("flat");
            button.set_valign(gtk::Align::Center);
            let notice = revealer.downgrade();
            button.connect_clicked(move |_| {
                callback();
                if let Some(notice) = notice.upgrade() {
                    dismiss_notice(&notice);
                }
            });
            row.append(&button);
        }
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("circular");
        close.set_valign(gtk::Align::Center);
        close.set_tooltip_text(Some("Dismiss"));
        let notice = revealer.downgrade();
        close.connect_clicked(move |_| {
            if let Some(notice) = notice.upgrade() {
                dismiss_notice(&notice);
            }
        });
        row.append(&close);

        self.notices.append(&revealer);
        let shown = revealer.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(notice) = shown.upgrade() {
                notice.set_reveal_child(true);
            }
        });
        // The oldest makes way.
        let mut showing = Vec::new();
        let mut child = self.notices.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            if let Ok(notice) = widget.downcast::<gtk::Revealer>()
                && !notice.has_css_class("kc-leaving")
            {
                showing.push(notice);
            }
        }
        for notice in showing.iter().take(showing.len().saturating_sub(3)) {
            dismiss_notice(notice);
        }
        let notice = revealer.downgrade();
        glib::timeout_add_seconds_local_once(seconds, move || {
            if let Some(notice) = notice.upgrade() {
                dismiss_notice(&notice);
            }
        });
    }

    /// Runs `future` on the Tokio runtime, then `done` with its result on the
    /// GTK thread.
    pub(crate) fn spawn<T: Send + 'static>(
        self: &Rc<Self>,
        future: impl std::future::Future<Output = T> + Send + 'static,
        done: impl FnOnce(&Rc<AppState>, T) + Send + 'static,
    ) {
        let ui = self.ui_tx.clone();
        self.runtime.spawn(async move {
            let result = future.await;
            let _ = ui
                .send(UiEvent::Run(Box::new(move |app| done(app, result))))
                .await;
        });
    }

    /// The active account with its REST client and store.
    pub(crate) fn active(&self) -> Option<(AccountId, kc_http::HttpClient, kc_state::Store)> {
        let model = self.model.borrow();
        let active = model.active?;
        let session = model.sessions.get(&active)?;
        Some((active, session.http.clone(), session.store.clone()))
    }

    /// Reads from the active session.
    pub(crate) fn with_session<R>(&self, read: impl FnOnce(&Session) -> R) -> Option<R> {
        let model = self.model.borrow();
        let session = model.sessions.get(&model.active?)?;
        Some(read(session))
    }

    /// Mutates the active session.
    pub(crate) fn with_session_mut<R>(&self, write: impl FnOnce(&mut Session) -> R) -> Option<R> {
        let mut model = self.model.borrow_mut();
        let active = model.active?;
        let session = model.sessions.get_mut(&active)?;
        Some(write(session))
    }
}

/// What a notice's action button does.
pub(crate) type NoticeAction = Box<dyn Fn()>;

/// Slides a notice away, then removes it.
fn dismiss_notice(notice: &gtk::Revealer) {
    if notice.has_css_class("kc-leaving") {
        return;
    }
    notice.add_css_class("kc-leaving");
    notice.set_reveal_child(false);
    let gone = notice.downgrade();
    let wait = u64::from(notice.transition_duration()) + 50;
    glib::timeout_add_local_once(std::time::Duration::from_millis(wait), move || {
        if let Some(notice) = gone.upgrade()
            && let Some(parent) = notice.parent().and_downcast::<gtk::Box>()
        {
            parent.remove(&notice);
        }
    });
}
