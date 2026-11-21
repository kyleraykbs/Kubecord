//! Development aid (`KUBECORD_DEMO=<mode>`): a made-up account with one
//! server, filled through the real store reducers and shown without
//! signing in or touching the network (apart from default avatars). It is
//! for checking the chat list, the channel sidebar and the voice channel
//! pages on screen. Modes:
//!
//! - `chat`: a long channel; you "send" two messages, and one of them
//!   grows a moment later, as an embed loading would make it.
//! - `sidebar`: the channel list scrolled to the bottom, then a click on a
//!   voice channel there and a voice state update.
//! - `voice-empty`: the page of a voice channel nobody is in.
//! - `voice-people`: the page of a voice channel others are in.
//! - `voice-joined`: in a voice channel, muted and deafened.
//! - `stream-menu`: watching Ada's screen share; her user menu (saved like
//!   the other menus) offers Stop Watching and a Stream Volume slider.
//! - `mod-menu`, `mod-menu-higher`, `mod-move`: the user menu (saved to
//!   `<screenshot>.menu.png`, as popovers are not part of the window
//!   picture) for a member below you, a member above you, and the Move To
//!   list.
//! - `mod-kick`, `mod-ban`, `mod-timeout`, `mod-roles`: the dialogs.
//! - `mod-error`: a kick that fails, to see the failure shown.
//! - `settings-<page>` (`overview`, `roles`, `members`, `invites`, `bans`,
//!   `audit`), `settings-role` (the role editor), `settings-error` (bans
//!   page without made-up data: the load fails) and `invite`.
//! - `viewer`: the image viewer (a default avatar, the only download),
//!   zoomed in; saved to `<screenshot>.viewer.png`.
//! - `gif`: a message with a real animated-GIF attachment (the GIF tag and
//!   layout render offline; the motion needs the network). `gif-viewer`
//!   also opens it in the image viewer, saved to `<screenshot>.viewer.png`.
//!   `gif-picker` fills the GIF picker with made-up animated results (the
//!   demo REST cannot reach Tenor), saved to `<screenshot>.menu.png`.
//! - `file`: a message with a non-image file attachment, so the file card
//!   (left-aligned name and size, Download button on the right) can be seen.
//! - `notify`: messages arrive in other channels, a DM, a muted channel
//!   and the open one; the notifications they would raise are printed
//!   (`demo: notification …`, nothing reaches the desktop), then one is
//!   "clicked" through the real `app.open-message` action.
//! - `sounds`: you join the Lounge, Linus joins and leaves, you mute,
//!   someone starts and stops watching your stream, the call is lost; the
//!   sound each would play is printed (`demo: sound …`), then the Sounds
//!   page opens. Nothing is downloaded or played.
//! - `channel-menu` (saved like the user menu), `channel-create`,
//!   `channel-edit` (a voice channel), `channel-text` (a text channel),
//!   `channel-perms`.
//! - `login-<state>`: the login page offline (see `show_login_demo`).
//! - `group-create`, `group-picked` (three friends ticked), `group-search`,
//!   `group-full` (nine ticked), `group-add` (adding to the group DM),
//!   `group-menu` (its right-click menu, saved like the user menu),
//!   `group-members` (the open group with its member list), `group-rename`,
//!   `group-leave`.
//! - `dm-hover` (both DM rows drawn as hovered, showing their ×),
//!   `dm-leave` (the group's × clicked: the confirmation), `dm-deleted`
//!   (the open DM is closed elsewhere: CHANNEL_DELETE arrives).
//! - `accounts` (the account menu with two accounts, each with a Log Out
//!   button, saved to `<screenshot>.popover.png`), `accounts-logout`
//!   (the Log Out confirmation dialog) and `accounts-done` (the active
//!   account logged out; prints the resulting session/account/page state).
//!
//! You are a Moderator (not the owner): Ada is a Member, Grace an Admin
//! above you, Linus an Artist and Member. The REST client points at an
//! address nothing listens on, so nothing is ever sent to Discord.
//!
//! Scroll positions are printed to stderr (`demo:` lines) so a run can be
//! checked without looking at the screenshot.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use libadwaita as adw;
use serde_json::{Value, json};

use kc_core::{Account, Snowflake, Token};
use kc_state::permissions::*;
use kc_state::time::{millis_to_iso, unix_millis_now};

use crate::session::{self, ConnState, Session, VoiceUiState};
use crate::shell::AppState;

const ME: u64 = 1;
const GUILD: u64 = 500;
const GENERAL: u64 = 601;
const LOUNGE: u64 = 690;
const EMPTY_VOICE: u64 = 691;
const OWNER: u64 = 9;
const ADA: u64 = 2;
const GRACE: u64 = 3;
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
/// A DM with Ada.
const DM: u64 = 950;
/// A group DM you own with Ada and Grace.
const GROUP: u64 = 951;
/// Friends besides Ada, for the group DM picker.
const FRIEND_NAMES: [&str; 10] = [
    "Alan", "Barbara", "Claude", "Dennis", "Edsger", "Frances", "Guido", "Hedy", "Ivan", "Jean",
];

/// A snowflake minted `ago` milliseconds before now, so the times read as
/// today's.
fn id_at(ago: u64, sequence: u64) -> u64 {
    (unix_millis_now() - ago - DISCORD_EPOCH_MS) << 22 | sequence
}

fn iso_of(id: u64) -> String {
    format!("{}+00:00", millis_to_iso((id >> 22) + DISCORD_EPOCH_MS))
}

fn ready() -> Value {
    let mut channels = vec![
        json!({"id": "600", "type": 4, "name": "Text Channels", "position": 0}),
        json!({"id": GENERAL.to_string(), "type": 0, "name": "general", "position": 0, "parent_id": "600",
               "topic": "Anything goes", "rate_limit_per_user": 10,
               "permission_overwrites": [{"id": "104", "type": 0, "allow": EMBED_LINKS.to_string(), "deny": ATTACH_FILES.to_string()}]}),
    ];
    for index in 0..18 {
        channels.push(json!({
            "id": (602 + index).to_string(), "type": 0, "name": format!("topic-{index:02}"),
            "position": index + 1, "parent_id": "600"
        }));
    }
    channels.push(json!({"id": "650", "type": 4, "name": "Voice Channels", "position": 1}));
    for index in 0..6 {
        channels.push(json!({
            "id": (680 + index).to_string(), "type": 2, "name": format!("Room {index}"),
            "position": index, "parent_id": "650"
        }));
    }
    channels.push(json!({"id": LOUNGE.to_string(), "type": 2, "name": "Lounge", "position": 10, "parent_id": "650",
                         "bitrate": 64000, "user_limit": 8}));
    channels.push(json!({"id": EMPTY_VOICE.to_string(), "type": 2, "name": "Quiet Room", "position": 11, "parent_id": "650"}));
    let everyone = VIEW_CHANNEL
        | SEND_MESSAGES
        | SEND_MESSAGES_IN_THREADS
        | READ_MESSAGE_HISTORY
        | ADD_REACTIONS
        | EMBED_LINKS
        | ATTACH_FILES
        | CREATE_INSTANT_INVITE
        | CHANGE_NICKNAME
        | CONNECT
        | SPEAK
        | STREAM
        | USE_VAD;
    let moderator = KICK_MEMBERS
        | BAN_MEMBERS
        | MODERATE_MEMBERS
        | MANAGE_NICKNAMES
        | MANAGE_ROLES
        | MANAGE_CHANNELS
        | MANAGE_GUILD
        | MANAGE_MESSAGES
        | VIEW_AUDIT_LOG
        | MOVE_MEMBERS
        | MUTE_MEMBERS
        | DEAFEN_MEMBERS;
    let mut users = vec![
        json!({"id": "2", "username": "ada", "global_name": "Ada"}),
        json!({"id": "3", "username": "grace", "global_name": "Grace"}),
        json!({"id": "4", "username": "linus", "global_name": "Linus"}),
        json!({"id": OWNER.to_string(), "username": "owner", "global_name": "Server Owner"}),
    ];
    let mut relationships = vec![json!({"id": "2", "type": 1, "user_id": "2"})];
    for (index, name) in FRIEND_NAMES.iter().enumerate() {
        let id = (20 + index).to_string();
        users.push(json!({"id": id, "username": name.to_lowercase(), "global_name": name}));
        relationships.push(json!({"id": id, "type": 1, "user_id": id}));
    }
    let mut ready = json!({
        "user": {"id": ME.to_string(), "username": "demo", "global_name": "Demo User"},
        "users": users,
        "guilds": [{
            "id": GUILD.to_string(), "name": "Demo Server", "owner_id": OWNER.to_string(),
            "joined_at": "2024-01-01T00:00:00+00:00",
            "afk_timeout": 300,
            "roles": [
                {"id": GUILD.to_string(), "name": "@everyone", "permissions": everyone.to_string(), "position": 0},
                {"id": "100", "name": "Admin", "permissions": ADMINISTRATOR.to_string(), "position": 5, "color": 0xe74c3c, "hoist": true},
                {"id": "101", "name": "Moderator", "permissions": moderator.to_string(), "position": 4, "color": 0x3498db, "hoist": true},
                {"id": "102", "name": "Music Bot", "permissions": "0", "position": 3, "color": 0x2ecc71, "managed": true},
                {"id": "103", "name": "Artist", "permissions": "0", "position": 2, "color": 0xf1c40f, "mentionable": true},
                {"id": "104", "name": "Member", "permissions": "0", "position": 1}
            ],
            "channels": channels
        }],
        "merged_members": [[
            {"user_id": ME.to_string(), "roles": ["101"]},
            {"user_id": ADA.to_string(), "roles": ["104"]},
            {"user_id": GRACE.to_string(), "roles": ["100"]},
            {"user_id": "4", "roles": ["103", "104"]},
            {"user_id": OWNER.to_string(), "roles": []}
        ]],
        "private_channels": [
            {"id": DM.to_string(), "type": 1, "recipient_ids": ["2"]},
            {"id": GROUP.to_string(), "type": 3, "recipient_ids": ["2", "3"], "owner_id": ME.to_string()}
        ],
        "relationships": relationships,
        "read_state": {"entries": []},
        "user_guild_settings": {"entries": []},
        "user_settings": {"status": "online", "guild_folders": []}
    });
    // Dev aid: `KUBECORD_DEMO_GUILDS=N` adds N extra servers (each with an
    // icon, so the rail loads an avatar per server) to measure the rail
    // rebuild at a busy account's scale.
    if let Some(extra) = std::env::var("KUBECORD_DEMO_GUILDS")
        .ok()
        .and_then(|n| n.parse::<u64>().ok())
    {
        let guilds = ready["guilds"].as_array_mut().unwrap();
        for index in 0..extra {
            let id = (700_000 + index).to_string();
            guilds.push(json!({
                "id": id, "name": format!("Server {index}"), "owner_id": OWNER.to_string(),
                "icon": "0000000000000000000000000000000a",
                "joined_at": "2024-01-01T00:00:00+00:00",
                "roles": [{"id": id, "name": "@everyone", "permissions": "0", "position": 0}],
                "channels": [{"id": id, "type": 0, "name": "general", "position": 0}]
            }));
        }
    }
    ready
}

thread_local! {
    /// Made-up REST answers (bans, invites, audit log) for the demo.
    static FAKE_REST: Cell<bool> = const { Cell::new(false) };
}

fn fake<T>(make: impl FnOnce() -> Value) -> Option<T>
where
    T: serde::de::DeserializeOwned,
{
    FAKE_REST
        .get()
        .then(make)
        .and_then(|value| serde_json::from_value(value).ok())
}

pub(crate) fn fake_bans() -> Option<Vec<kc_http::Ban>> {
    fake(|| {
        json!([
            {"user": {"id": "70", "username": "spammer", "global_name": "Free Nitro"}, "reason": "Phishing links"},
            {"user": {"id": "71", "username": "troll"}, "reason": null},
            {"user": {"id": "72", "username": "raider42", "global_name": "Raider"}, "reason": "Raid on 2026-11-01"}
        ])
    })
}

pub(crate) fn fake_invites() -> Option<Vec<kc_http::Invite>> {
    let soon = format!("{}+00:00", millis_to_iso(unix_millis_now() + 5 * 3_600_000));
    fake(move || {
        json!([
            {"code": "demoAbc", "uses": 3, "max_uses": 0, "max_age": 0, "expires_at": null,
             "inviter": {"id": "1", "username": "demo", "global_name": "Demo User"},
             "channel": {"id": "601", "name": "general"}},
            {"code": "x7Yz9Q", "uses": 1, "max_uses": 10, "max_age": 86400, "expires_at": soon,
             "inviter": {"id": "3", "username": "grace", "global_name": "Grace"},
             "channel": {"id": "601", "name": "general"}}
        ])
    })
}

pub(crate) fn fake_audit_log() -> Option<Value> {
    fake(|| {
        let at =
            |ago_minutes: u64, sequence: u64| id_at(ago_minutes * 60_000, sequence).to_string();
        json!({
            "users": [
                {"id": "1", "username": "demo", "global_name": "Demo User"},
                {"id": "2", "username": "ada", "global_name": "Ada"},
                {"id": "3", "username": "grace", "global_name": "Grace"},
                {"id": "70", "username": "spammer", "global_name": "Free Nitro"}
            ],
            "audit_log_entries": [
                {"id": at(3, 1), "user_id": "1", "action_type": 24, "target_id": "2",
                 "changes": [{"key": "communication_disabled_until", "new_value": "2030-01-01T00:00:00+00:00"}],
                 "reason": "Calm down for a bit"},
                {"id": at(20, 2), "user_id": "3", "action_type": 22, "target_id": "70", "reason": "Phishing links"},
                {"id": at(45, 3), "user_id": "3", "action_type": 25, "target_id": "2",
                 "changes": [{"key": "$add", "new_value": [{"id": "104", "name": "Member"}]}]},
                {"id": at(90, 4), "user_id": "1", "action_type": 10, "target_id": "601",
                 "changes": [{"key": "name", "new_value": "general"}]},
                {"id": at(200, 5), "user_id": "3", "action_type": 72, "target_id": "70",
                 "options": {"channel_id": "601", "count": "1"}}
            ]
        })
    })
}

fn message(id: u64, author: u64, content: &str) -> Value {
    let names = ["", "demo", "ada", "grace", "linus"];
    json!({
        "id": id.to_string(), "channel_id": GENERAL.to_string(), "guild_id": GUILD.to_string(),
        "author": {"id": author.to_string(), "username": names[author as usize % names.len()]},
        "content": content, "timestamp": iso_of(id), "type": 0,
        "attachments": [], "embeds": [], "mentions": []
    })
}

/// A message carrying one `image/gif` attachment, for the GIF demo. The URL
/// is a real public GIF so the layout (and the animation, when online) can be
/// seen; offline the still placeholder and GIF tag still render.
fn gif_message(id: u64, author: u64) -> Value {
    let url = "https://upload.wikimedia.org/wikipedia/commons/2/2c/Rotating_earth_%28large%29.gif";
    let names = ["", "demo", "ada", "grace", "linus"];
    json!({
        "id": id.to_string(), "channel_id": GENERAL.to_string(), "guild_id": GUILD.to_string(),
        "author": {"id": author.to_string(), "username": names[author as usize % names.len()]},
        "content": "look at this gif", "timestamp": iso_of(id), "type": 0,
        "attachments": [{
            "id": "1", "filename": "creaking.gif", "size": 1_000_000,
            "url": url, "proxy_url": url,
            "content_type": "image/gif", "width": 400, "height": 400,
        }],
        "embeds": [], "mentions": []
    })
}

/// A message carrying one non-image file attachment, for the file-card demo
/// (the Download button, left-aligned name/size). Nothing is downloaded until
/// the button is clicked.
fn file_message(id: u64, author: u64) -> Value {
    let names = ["", "demo", "ada", "grace", "linus"];
    json!({
        "id": id.to_string(), "channel_id": GENERAL.to_string(), "guild_id": GUILD.to_string(),
        "author": {"id": author.to_string(), "username": names[author as usize % names.len()]},
        "content": "here is the model", "timestamp": iso_of(id), "type": 0,
        "attachments": [{
            "id": "1", "filename": "wumpus-figurine.stl", "size": 4_200_000,
            "url": "https://example.invalid/wumpus-figurine.stl",
            "content_type": "model/stl",
        }],
        "embeds": [], "mentions": []
    })
}

impl AppState {
    pub(crate) fn show_demo(self: &Rc<Self>, mode: &str) {
        if let Some(what) = mode.strip_prefix("login") {
            self.show_login_demo(what.trim_start_matches('-'));
            return;
        }
        let token = Token::new("kubecord-demo-account");
        let account = session::account_id(&token);
        let mut session = Session::new(Account {
            user_id: Some(Snowflake::new(ME)),
            token,
            label: Some("Demo".to_owned()),
        });
        session.connection = ConnState::Ready;
        // Nothing in the demo may reach Discord: requests go nowhere and
        // fail, which also shows how failures look.
        session.http = kc_http::HttpClient::with_base(
            Token::new("kubecord-demo-account"),
            "https://127.0.0.1:9/api/v9",
        );
        let store = session.store.clone();
        store.apply("READY", &ready());
        // Grace is server muted (red), Ada muted herself.
        for (user, channel, mute, server_mute) in [
            (ADA, LOUNGE, true, false),
            (GRACE, LOUNGE, false, true),
            (4, 680, false, false),
        ] {
            store.apply(
                "VOICE_STATE_UPDATE",
                &json!({"user_id": user.to_string(), "channel_id": channel.to_string(),
                        "guild_id": GUILD.to_string(), "session_id": "x", "self_mute": mute,
                        "mute": server_mute}),
            );
        }
        session.user = store.read(|state| state.me.clone());
        // Nothing to fetch or subscribe to: there is no gateway.
        for channel in [GENERAL, LOUNGE, EMPTY_VOICE, GROUP] {
            session.fetched.insert(Snowflake::new(channel));
            session.subscribed.insert(Snowflake::new(channel));
        }
        for index in 0..40u64 {
            let id = id_at(3_600_000 - index * 60_000, index);
            let text = if index % 7 == 3 {
                "A longer message that wraps over more than one line in a normal window, \
                 to give the list rows of different heights like a real channel has."
                    .to_owned()
            } else {
                format!("Message number {index}")
            };
            store.apply("MESSAGE_CREATE", &message(id, 2 + index % 3, &text));
        }

        // Store changes reach the interface as they do from the gateway.
        let mut events = store.subscribe();
        let app = self.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = events.recv().await {
                app.handle_event(session::UiEvent::StoreEvent { account, event });
            }
        });

        {
            let mut model = self.model.borrow_mut();
            // List the demo account (and, for the account-menu demos, a
            // second one) so the account menu and its Log Out buttons show.
            model.accounts.upsert(session.account.clone());
            if mode.starts_with("accounts") {
                model.accounts.upsert(Account {
                    user_id: Some(Snowflake::new(ADA)),
                    token: Token::new("kubecord-demo-second"),
                    label: Some("Ada".to_owned()),
                });
            }
            model.sessions.insert(account, session);
            model.active = Some(account);
        }
        self.stack.set_visible_child_name("chat");
        let channel =
            |id: u64| store.read(|state| state.channels.get(&Snowflake::new(id)).cloned());
        let Some(general) = channel(GENERAL) else {
            return;
        };
        match mode {
            "sidebar" => self.demo_sidebar(&store),
            "voice-empty" => {
                if let Some(room) = channel(EMPTY_VOICE) {
                    self.open_channel(room);
                }
            }
            "voice-people" => {
                if let Some(lounge) = channel(LOUNGE) {
                    self.open_channel(lounge);
                }
            }
            "voice-joined" => {
                store.apply(
                    "VOICE_STATE_UPDATE",
                    &json!({"user_id": ME.to_string(), "channel_id": LOUNGE.to_string(),
                            "guild_id": GUILD.to_string(), "session_id": "x"}),
                );
                self.with_session_mut(|session| {
                    session.voice = Some(VoiceUiState {
                        guild_id: Some(Snowflake::new(GUILD)),
                        channel_id: Snowflake::new(LOUNGE),
                        muted: true,
                        deafened: true,
                        camera: false,
                        streaming: false,
                        watching: None,
                    });
                });
                if let Some(lounge) = channel(LOUNGE) {
                    self.open_channel(lounge);
                }
                self.render_voice_panel();
            }
            "stream-menu" => {
                // Ada is screen-sharing and you are watching her: her user
                // menu then offers Stop Watching and a Stream Volume slider
                // beside the voice one. Saved to `<screenshot>.menu.png`.
                store.apply(
                    "VOICE_STATE_UPDATE",
                    &json!({"user_id": ADA.to_string(), "channel_id": LOUNGE.to_string(),
                            "guild_id": GUILD.to_string(), "session_id": "x", "self_stream": true}),
                );
                store.apply(
                    "VOICE_STATE_UPDATE",
                    &json!({"user_id": ME.to_string(), "channel_id": LOUNGE.to_string(),
                            "guild_id": GUILD.to_string(), "session_id": "x"}),
                );
                self.with_session_mut(|session| {
                    session.voice = Some(VoiceUiState {
                        guild_id: Some(Snowflake::new(GUILD)),
                        channel_id: Snowflake::new(LOUNGE),
                        muted: false,
                        deafened: false,
                        camera: false,
                        streaming: false,
                        watching: Some(Snowflake::new(ADA)),
                    });
                });
                if let Some(lounge) = channel(LOUNGE) {
                    self.open_channel(lounge);
                }
                self.render_voice_panel();
                // Open the menu once the window has been sized, so its
                // popover lays out (and can be captured) correctly.
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                    let anchor = app.window.clone().upcast::<gtk::Widget>();
                    let menu = app.show_user_menu(Snowflake::new(ADA), &anchor, 420.0, 120.0);
                    if let (Some(popover), Ok(path)) = (menu, std::env::var("KUBECORD_SCREENSHOT"))
                    {
                        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                            let path = format!("{path}.menu.png");
                            if let Err(error) =
                                crate::app::save_window_png(&popover, std::path::Path::new(&path))
                            {
                                eprintln!("demo: could not save the menu: {error}");
                            }
                        });
                    }
                });
            }
            mode if mode.starts_with("settings-") || mode == "invite" => {
                FAKE_REST.set(mode != "settings-error");
                self.open_channel(general);
                let app = self.clone();
                let mode = mode.to_owned();
                glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                    let guild = Snowflake::new(GUILD);
                    match mode.as_str() {
                        "invite" => app.invite_dialog(guild),
                        "settings-role" => {
                            if let Some(panel) = app.show_server_settings(guild, Some("roles")) {
                                app.role_editor(&panel, Snowflake::new(103));
                            }
                        }
                        "settings-error" => {
                            app.show_server_settings(guild, Some("bans"));
                        }
                        other => {
                            app.show_server_settings(guild, other.strip_prefix("settings-"));
                        }
                    }
                });
            }
            "file" => {
                // A message with a non-image file attachment, so the file
                // card (left-aligned name/size, Download button on the right)
                // can be seen.
                let id = id_at(0, 42);
                store.apply("MESSAGE_CREATE", &file_message(id, ADA));
                self.open_channel(general);
            }
            "gif" | "gif-viewer" => {
                // A message with a real animated-GIF attachment. The still
                // placeholder, GIF tag and layout render offline; the motion
                // itself needs the network (the CDN), so a screenshot shows a
                // frame, not the animation.
                let id = id_at(0, 41);
                store.apply("MESSAGE_CREATE", &gif_message(id, ADA));
                // Seed the favourites cache so the star on the GIF shows its
                // favourited (filled, visible) state in the demo; without this
                // it is only revealed on hover, which a screenshot cannot show.
                let url = "https://upload.wikimedia.org/wikipedia/commons/2/2c/Rotating_earth_%28large%29.gif";
                crate::gif_favorites::set_all(vec![kc_http::Gif {
                    id: String::new(),
                    url: url.to_owned(),
                    src: url.to_owned(),
                    gif_src: url.to_owned(),
                    preview: url.to_owned(),
                    width: 400,
                    height: 400,
                    video: false,
                }]);
                self.open_channel(general);
                if mode == "gif-viewer" {
                    // Open the GIF in the viewer (it animates there when
                    // online); the still/first frame is what a screenshot
                    // catches.
                    let url = "https://upload.wikimedia.org/wikipedia/commons/2/2c/Rotating_earth_%28large%29.gif";
                    let app = self.clone();
                    let url = url.to_owned();
                    glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                        app.show_image(&url, &url);
                        if let Ok(path) = std::env::var("KUBECORD_SCREENSHOT") {
                            let app = app.clone();
                            let url = url.clone();
                            glib::timeout_add_local_once(Duration::from_millis(2500), move || {
                                if let Some(window) = app.window_of(&format!("image:{url}")) {
                                    let path = format!("{path}.viewer.png");
                                    let _ = crate::app::save_window_png(
                                        &window,
                                        std::path::Path::new(&path),
                                    );
                                }
                            });
                        }
                    });
                }
            }
            "gif-picker" => {
                // The GIF picker filled with made-up animated results (the
                // demo REST points nowhere, so real trending/search cannot
                // load). The popover is a separate surface, so it is saved to
                // `<screenshot>.menu.png`.
                self.open_channel(general);
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                    let popover = app.demo_gif_picker();
                    if let (Ok(path), Some(popover)) =
                        (std::env::var("KUBECORD_SCREENSHOT"), popover)
                    {
                        glib::timeout_add_local_once(Duration::from_millis(2500), move || {
                            let path = format!("{path}.menu.png");
                            let _ =
                                crate::app::save_window_png(&popover, std::path::Path::new(&path));
                        });
                    }
                });
            }
            "viewer" => {
                self.open_channel(general);
                let url = "https://cdn.discordapp.com/embed/avatars/2.png?size=1024";
                self.show_image(url, url);
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(2500), move || {
                    app.demo_zoom_viewer(url, 4.0);
                    let Ok(path) = std::env::var("KUBECORD_SCREENSHOT") else {
                        return;
                    };
                    let app = app.clone();
                    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                        if let Some(window) = app.window_of(&format!("image:{url}")) {
                            eprintln!("demo: viewer modal={}", window.is_modal());
                            let path = format!("{path}.viewer.png");
                            if let Err(error) =
                                crate::app::save_window_png(&window, std::path::Path::new(&path))
                            {
                                eprintln!("demo: could not save the viewer: {error}");
                            }
                        }
                    });
                });
            }
            "notify" => {
                self.open_channel(general);
                self.demo_notify(&store, account);
            }
            "sounds" => {
                if let Some(lounge) = channel(LOUNGE) {
                    self.open_channel(lounge);
                }
                self.demo_sounds(&store, account);
            }
            mode if mode.starts_with("channel-") => {
                self.open_channel(general);
                let app = self.clone();
                let mode = mode.to_owned();
                glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                    app.demo_channels(&mode);
                });
            }
            mode if mode.starts_with("dm-") => {
                if let Some(dm) = channel(DM) {
                    self.open_channel(dm);
                }
                let app = self.clone();
                let mode = mode.to_owned();
                glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                    let closes: Vec<gtk::Button> =
                        descendants::<gtk::Button>(&app.channel_box.clone().upcast())
                            .into_iter()
                            .filter(|button| button.has_css_class("kc-dm-close"))
                            .collect();
                    eprintln!("demo: DM close buttons={}", closes.len());
                    match mode.as_str() {
                        "dm-hover" => {
                            for close in &closes {
                                if let Some(row) =
                                    close.parent().and_then(|content| content.parent())
                                {
                                    row.set_state_flags(gtk::StateFlags::PRELIGHT, false);
                                }
                            }
                        }
                        // The group is listed first (it is newer).
                        "dm-leave" => {
                            if let Some(close) = closes.first() {
                                close.emit_clicked();
                            }
                        }
                        _ => {
                            app.with_session(|session| {
                                session.store.apply(
                                    "CHANNEL_DELETE",
                                    &json!({"id": DM.to_string(), "type": 1}),
                                )
                            });
                            let selected = app
                                .with_session(|session| session.selected_channel)
                                .flatten();
                            eprintln!("demo: after delete selected={selected:?}");
                        }
                    }
                });
            }
            mode if mode.starts_with("group-") => {
                match mode {
                    "group-create" | "group-picked" | "group-search" | "group-full" => {
                        self.open_home()
                    }
                    _ => {
                        if let Some(group) = channel(GROUP) {
                            self.open_channel(group);
                        }
                    }
                }
                let app = self.clone();
                let mode = mode.to_owned();
                glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                    app.demo_group(&mode);
                });
            }
            mode if mode.starts_with("mod-") => {
                if let Some(lounge) = channel(LOUNGE) {
                    self.open_channel(lounge);
                }
                let app = self.clone();
                let mode = mode.to_owned();
                glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                    app.demo_moderation(&mode);
                });
            }
            // The account menu with its per-account Log Out buttons
            // (saved to `<screenshot>.popover.png`), or the Log Out
            // confirmation dialog (an in-window dialog, in the main shot).
            "accounts" | "accounts-logout" | "accounts-done" => {
                self.open_channel(general);
                let app = self.clone();
                let mode = mode.to_owned();
                glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                    app.render_account_controls();
                    if mode == "accounts-done" {
                        // Actually log the active account out and report the
                        // resulting state, to check the teardown path.
                        app.log_out_account(account);
                        let (sessions, accounts, page) = {
                            let model = app.model.borrow();
                            (
                                model.sessions.len(),
                                model.accounts.accounts().len(),
                                app.stack.visible_child_name(),
                            )
                        };
                        eprintln!(
                            "demo: after logout sessions={sessions} accounts={accounts} page={page:?}"
                        );
                        return;
                    }
                    if mode == "accounts-logout" {
                        app.confirm_log_out(account, "Demo User");
                        return;
                    }
                    if let Some(popover) = app.account_button.popover() {
                        popover.set_autohide(false);
                        popover.popup();
                        if let Ok(path) = std::env::var("KUBECORD_SCREENSHOT") {
                            glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                                let _ = crate::app::save_window_png(
                                    &popover,
                                    std::path::Path::new(&format!("{path}.popover.png")),
                                )
                                .map_err(|error| eprintln!("demo: account popover: {error}"));
                            });
                        }
                    }
                });
            }
            "chat-sending" => {
                self.open_channel(general);
                self.demo_chat(&store);
                // One message still sending, one that failed, as a user would
                // see just after pressing Enter and after a send failed.
                if let Some(me) = store.read(|state| state.me.clone()) {
                    let general_id = Snowflake::new(GENERAL);
                    let mut sending = kc_state::Message::pending(
                        Snowflake::from_unix_millis(unix_millis_now(), 1),
                        general_id,
                        me.clone(),
                        "sending this one…".to_owned(),
                        "demo-sending".to_owned(),
                    );
                    sending.guild_id = Some(Snowflake::new(GUILD));
                    store.add_pending(general_id, sending);
                    let mut failed = kc_state::Message::pending(
                        Snowflake::from_unix_millis(unix_millis_now(), 2),
                        general_id,
                        me,
                        "this one did not go through".to_owned(),
                        "demo-failed".to_owned(),
                    );
                    failed.guild_id = Some(Snowflake::new(GUILD));
                    store.add_pending(general_id, failed);
                    store.set_send_state(general_id, "demo-failed", kc_state::SendState::Failed);
                }
            }
            _ => {
                self.open_channel(general);
                self.demo_chat(&store);
            }
        }
    }

    fn demo_notify(self: &Rc<Self>, store: &kc_state::Store, account: session::AccountId) {
        // topic-05 is muted; topic-06 is set to Nothing.
        store.apply(
            "USER_GUILD_SETTINGS_UPDATE",
            &json!({"guild_id": GUILD.to_string(), "message_notifications": 0,
            "channel_overrides": [
                {"channel_id": "607", "muted": true, "message_notifications": 3},
                {"channel_id": "608", "muted": false, "message_notifications": 2}
            ]}),
        );
        let me = json!([{"id": ME.to_string(), "username": "demo"}]);
        let arrivals = [
            (
                "plain in topic-01",
                603,
                2,
                "Anyone up for a game tonight?",
                json!({}),
            ),
            (
                "in the open channel",
                GENERAL,
                3,
                "Seen here already",
                json!({}),
            ),
            ("muted channel", 607, 4, "Muted chatter", json!({})),
            (
                "muted channel, mentioned",
                607,
                4,
                "<@1> this one is for you",
                json!({"mentions": me}),
            ),
            (
                "channel set to Nothing",
                608,
                2,
                "<@1> even this",
                json!({"mentions": me}),
            ),
            (
                "role mention",
                609,
                3,
                "<@&101> please look",
                json!({"mention_roles": ["101"]}),
            ),
            ("silent", 603, 2, "@silent hello", json!({"flags": 4096})),
            ("own message", 603, ME, "my own words", json!({})),
            ("DM", DM, 2, "Hi! Are you around?", json!({})),
        ];
        let ids: Vec<u64> = (0..arrivals.len() as u64)
            .map(|index| id_at(0, 700 + index))
            .collect();
        let dm_message = ids[arrivals.len() - 1];
        for (index, (what, channel, author, content, extra)) in arrivals.into_iter().enumerate() {
            let id = ids[index];
            let app = self.clone();
            let store = store.clone();
            glib::timeout_add_local_once(
                Duration::from_millis(1500 + index as u64 * 150),
                move || {
                    eprintln!("demo: arriving: {what}");
                    let mut value = message(id, author, content);
                    value["channel_id"] = json!(channel.to_string());
                    if channel == DM {
                        value["guild_id"] = serde_json::Value::Null;
                    }
                    if let (Some(base), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
                        for (key, item) in extra {
                            base.insert(key.clone(), item.clone());
                        }
                    }
                    store.apply("MESSAGE_CREATE", &value);
                    let _ = &app;
                },
            );
        }
        // Click the DM's notification through the real action.
        let app = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(3500), move || {
            let target = crate::notifications::action_target(
                account,
                Snowflake::new(DM),
                Snowflake::new(dm_message),
            );
            if let Some(application) = app.window.application() {
                application.activate_action(
                    crate::notifications::OPEN_ACTION,
                    Some(&target.to_variant()),
                );
            }
            let app = app.clone();
            glib::timeout_add_local_once(Duration::from_millis(500), move || {
                let open = app
                    .with_session(|session| session.selected_channel)
                    .flatten();
                eprintln!(
                    "demo: after the click the open channel is {open:?}; window title {:?}",
                    app.window.title()
                );
            });
        });
    }

    fn demo_sounds(self: &Rc<Self>, store: &kc_state::Store, account: session::AccountId) {
        type Step = Box<dyn Fn(&Rc<AppState>, &kc_state::Store)>;
        let steps: Vec<(&str, Step)> = vec![
            (
                "you join the Lounge",
                Box::new(move |app, store| {
                    store.apply(
                        "VOICE_STATE_UPDATE",
                        &json!({"user_id": ME.to_string(), "channel_id": LOUNGE.to_string(),
                                "guild_id": GUILD.to_string(), "session_id": "x"}),
                    );
                    app.handle_event(session::UiEvent::VoiceJoined {
                        account,
                        channel_id: Snowflake::new(LOUNGE),
                    });
                    // Reported again once the connection is ready: no
                    // second sound.
                    app.handle_event(session::UiEvent::VoiceJoined {
                        account,
                        channel_id: Snowflake::new(LOUNGE),
                    });
                }),
            ),
            (
                "Linus joins",
                Box::new(|_, store| {
                    store.apply(
                        "VOICE_STATE_UPDATE",
                        &json!({"user_id": "4", "channel_id": LOUNGE.to_string(),
                                "guild_id": GUILD.to_string(), "session_id": "y"}),
                    );
                }),
            ),
            (
                "Linus leaves",
                Box::new(|_, store| {
                    store.apply(
                        "VOICE_STATE_UPDATE",
                        &json!({"user_id": "4", "channel_id": null,
                                "guild_id": GUILD.to_string(), "session_id": "y"}),
                    );
                }),
            ),
            (
                "you mute",
                Box::new(|app, _| app.voice_mute.set_active(true)),
            ),
            (
                "you unmute",
                Box::new(|app, _| app.voice_mute.set_active(false)),
            ),
            (
                "your stream: nobody, then Ada watching, then not",
                Box::new(|_, store| {
                    let key = format!("guild:{GUILD}:{LOUNGE}:{ME}");
                    for (index, viewers) in
                        [json!([]), json!(["2"]), json!([])].into_iter().enumerate()
                    {
                        let store = store.clone();
                        let key = key.clone();
                        glib::timeout_add_local_once(
                            Duration::from_millis(index as u64 * 80),
                            move || {
                                store.apply(
                                    "STREAM_UPDATE",
                                    &json!({"stream_key": key, "channel_id": LOUNGE.to_string(),
                                            "viewer_ids": viewers, "paused": false}),
                                );
                            },
                        );
                    }
                }),
            ),
            (
                "the call is lost",
                Box::new(move |app, _| {
                    app.handle_event(session::UiEvent::VoiceLeft {
                        account,
                        lost: true,
                    });
                }),
            ),
        ];
        for (index, (what, step)) in steps.into_iter().enumerate() {
            let app = self.clone();
            let store = store.clone();
            glib::timeout_add_local_once(
                Duration::from_millis(1200 + index as u64 * 300),
                move || {
                    eprintln!("demo: step: {what}");
                    step(&app, &store);
                },
            );
        }
        let app = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(3800), move || {
            app.show_settings_page("sounds");
        });
    }

    fn demo_channels(self: &Rc<Self>, mode: &str) {
        let guild = Snowflake::new(GUILD);
        match mode {
            "channel-create" => self.create_channel_dialog(guild, Some(Snowflake::new(600)), false),
            "channel-edit" => self.edit_channel_dialog(Snowflake::new(LOUNGE)),
            "channel-text" => self.edit_channel_dialog(Snowflake::new(GENERAL)),
            "channel-perms" => self.channel_permissions_dialog(Snowflake::new(GENERAL)),
            _ => {
                let Some(channel) = self
                    .with_session(|session| {
                        session
                            .store
                            .read(|state| state.channels.get(&Snowflake::new(GENERAL)).cloned())
                    })
                    .flatten()
                else {
                    return;
                };
                let popover = gtk::Popover::new();
                self.anchor_popover(&popover, &self.window, 120.0, 100.0);
                let items = gtk::Box::new(gtk::Orientation::Vertical, 2);
                items.add_css_class("kc-menu");
                items.set_width_request(200);
                items.append(&crate::menu::menu_item("Mark As Read", false));
                self.add_channel_items(&items, &popover, &channel);
                popover.set_child(Some(&items));
                popover.popup();
                if let Ok(path) = std::env::var("KUBECORD_SCREENSHOT") {
                    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                        let path = format!("{path}.menu.png");
                        if let Err(error) =
                            crate::app::save_window_png(&popover, std::path::Path::new(&path))
                        {
                            eprintln!("demo: could not save the menu: {error}");
                        }
                    });
                }
            }
        }
    }

    fn demo_group(self: &Rc<Self>, mode: &str) {
        let group = Snowflake::new(GROUP);
        match mode {
            "group-create" | "group-picked" | "group-search" | "group-full" => {
                self.group_dm_dialog(None);
                let ticks = match mode {
                    "group-picked" => 3,
                    "group-full" => 9,
                    _ => 0,
                };
                let Some(dialog) = descendants::<adw::Dialog>(&self.window.clone().upcast()).pop()
                else {
                    return;
                };
                let dialog = dialog.upcast::<gtk::Widget>();
                let checks = descendants::<gtk::CheckButton>(&dialog);
                eprintln!("demo: group picker rows={}", checks.len());
                for check in checks.iter().take(ticks) {
                    check.set_active(true);
                }
                if mode == "group-search" {
                    for entry in descendants::<gtk::SearchEntry>(&dialog) {
                        entry.set_text("ed");
                    }
                }
                if mode == "group-full" {
                    // A tenth tick is refused.
                    if let Some(check) = checks.get(9) {
                        check.set_active(true);
                        eprintln!("demo: tenth tick kept={}", check.is_active());
                    }
                }
            }
            "group-add" => self.group_dm_dialog(Some(group)),
            "group-rename" => self.rename_group_dialog(group),
            "group-leave" => self.confirm_leave_group(group),
            "group-members" => {
                self.show_members.set(true);
                self.render_members();
            }
            "group-menu" => {
                let popover = self.show_group_menu(group, &self.window, 120.0, 200.0);
                if let Ok(path) = std::env::var("KUBECORD_SCREENSHOT") {
                    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                        let path = format!("{path}.menu.png");
                        if let Err(error) =
                            crate::app::save_window_png(&popover, std::path::Path::new(&path))
                        {
                            eprintln!("demo: could not save the menu: {error}");
                        }
                    });
                }
            }
            _ => {}
        }
    }

    fn demo_moderation(self: &Rc<Self>, mode: &str) {
        let guild = Snowflake::new(GUILD);
        let ada = Snowflake::new(ADA);
        let anchor = self.window.clone().upcast::<gtk::Widget>();
        let menu = match mode {
            "mod-menu" | "mod-move" => self.show_user_menu(ada, &anchor, 420.0, 120.0),
            "mod-menu-higher" => self.show_user_menu(Snowflake::new(GRACE), &anchor, 420.0, 120.0),
            "mod-kick" => {
                self.kick_dialog(guild, ada, "Ada");
                None
            }
            "mod-ban" => {
                self.ban_dialog(guild, ada, "Ada");
                None
            }
            "mod-timeout" => {
                self.timeout_dialog(guild, ada, "Ada");
                None
            }
            "mod-roles" => {
                self.roles_dialog(guild, Snowflake::new(4));
                None
            }
            "mod-error" => {
                if let Some((_, http, _)) = self.active() {
                    self.moderate("Could not kick", "kicked".into(), async move {
                        http.kick_member(guild, ada, Some("demo")).await
                    });
                }
                None
            }
            _ => None,
        };
        if mode == "mod-move"
            && let Some(popover) = &menu
        {
            self.show_move_targets(popover, guild, ada);
        }
        if let (Some(popover), Ok(path)) = (menu, std::env::var("KUBECORD_SCREENSHOT")) {
            glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                let path = format!("{path}.menu.png");
                if let Err(error) =
                    crate::app::save_window_png(&popover, std::path::Path::new(&path))
                {
                    eprintln!("demo: could not save the menu: {error}");
                }
            });
        }
    }

    /// Prints where a scrolled window stands.
    fn demo_report(what: &str, scroll: &gtk::ScrolledWindow) {
        let adjustment = scroll.vadjustment();
        eprintln!(
            "demo: {what}: value={:.0} upper={:.0} page={:.0} bottom_gap={:.0}",
            adjustment.value(),
            adjustment.upper(),
            adjustment.page_size(),
            adjustment.upper() - adjustment.page_size() - adjustment.value()
        );
    }

    /// Where the last message row ends, relative to the list's window.
    fn demo_last_row(&self) {
        let Some(row) = self.messages_list.last_child() else {
            return;
        };
        if let Some(bounds) = row.compute_bounds(&self.messages_scroll) {
            eprintln!(
                "demo: last row top={:.0} bottom={:.0} window_height={}",
                bounds.y(),
                bounds.y() + bounds.height(),
                self.messages_scroll.height()
            );
        }
    }

    fn demo_chat(self: &Rc<Self>, store: &kc_state::Store) {
        let send = |delay_ms: u64, content: &'static str, sequence: u64| {
            let app = self.clone();
            let store = store.clone();
            glib::timeout_add_local_once(Duration::from_millis(delay_ms), move || {
                Self::demo_report("before sending", &app.messages_scroll);
                let id = id_at(0, 900 + sequence);
                store.apply("MESSAGE_CREATE", &message(id, ME, content));
                let app2 = app.clone();
                glib::timeout_add_local_once(Duration::from_millis(500), move || {
                    Self::demo_report("after sending", &app2.messages_scroll);
                    app2.demo_last_row();
                });
                if sequence == 1 {
                    // The message grows, as when an embed or image arrives.
                    let app = app.clone();
                    glib::timeout_add_local_once(Duration::from_millis(1000), move || {
                        let mut grown = message(
                            id,
                            ME,
                            "This message grew after it was shown:\n\nline two\n\nline three\n\nline four\n\nline five",
                        );
                        grown["edited_timestamp"] = json!(iso_of(id));
                        store.apply("MESSAGE_UPDATE", &grown);
                        let app = app.clone();
                        glib::timeout_add_local_once(Duration::from_millis(500), move || {
                            Self::demo_report("after it grew", &app.messages_scroll);
                            app.demo_last_row();
                        });
                    });
                }
            });
        };
        send(1500, "Hello! This is the message I just sent.", 0);
        send(3000, "And a second one.", 1);
    }

    fn demo_sidebar(self: &Rc<Self>, store: &kc_state::Store) {
        let Some(general) =
            store.read(|state| state.channels.get(&Snowflake::new(GENERAL)).cloned())
        else {
            return;
        };
        self.open_channel(general);
        let Some(scroll) = self
            .channel_box
            .ancestor(gtk::ScrolledWindow::static_type())
            .and_downcast::<gtk::ScrolledWindow>()
        else {
            return;
        };
        let app = self.clone();
        let store = store.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            let adjustment = scroll.vadjustment();
            adjustment.set_value(adjustment.upper() - adjustment.page_size());
            Self::demo_report("sidebar scrolled down", &scroll);
            let app = app.clone();
            let scroll = scroll.clone();
            glib::timeout_add_local_once(Duration::from_millis(500), move || {
                // Click a voice channel near the bottom, as a pointer would:
                // the button takes focus, then the click handler runs.
                let name = format!("channel-{EMPTY_VOICE}");
                let mut child = app.channel_box.first_child();
                while let Some(widget) = child {
                    if widget.widget_name() == name.as_str() {
                        if let Some(button) = widget.downcast_ref::<gtk::Button>() {
                            if button.gets_focus_on_click() {
                                button.grab_focus();
                            }
                            button.emit_clicked();
                        }
                        break;
                    }
                    child = widget.next_sibling();
                }
                let report = scroll.clone();
                glib::timeout_add_local_once(Duration::from_millis(500), move || {
                    Self::demo_report("after the click", &report);
                });
                // Someone joins a channel: the list is redrawn again.
                let report = scroll.clone();
                glib::timeout_add_local_once(Duration::from_millis(1000), move || {
                    store.apply(
                        "VOICE_STATE_UPDATE",
                        &json!({"user_id": "4", "channel_id": EMPTY_VOICE.to_string(),
                                "guild_id": GUILD.to_string(), "session_id": "y"}),
                    );
                    glib::timeout_add_local_once(Duration::from_millis(500), move || {
                        Self::demo_report("after a voice state update", &report);
                    });
                });
            });
        });
    }
}

/// Every widget of type `T` under `root`, depth first.
fn descendants<T: IsA<gtk::Widget>>(root: &gtk::Widget) -> Vec<T> {
    let mut found = Vec::new();
    let mut child = root.first_child();
    while let Some(widget) = child {
        if let Some(hit) = widget.downcast_ref::<T>() {
            found.push(hit.clone());
        }
        found.extend(descendants::<T>(&widget));
        child = widget.next_sibling();
    }
    found
}
