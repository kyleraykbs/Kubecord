//! Development aid (`KYLECORD_DEMO=<mode>`): a made-up account with one
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
//! - `mod-menu`, `mod-menu-higher`, `mod-move`: the user menu (saved to
//!   `<screenshot>.menu.png`, as popovers are not part of the window
//!   picture) for a member below you, a member above you, and the Move To
//!   list.
//! - `mod-kick`, `mod-ban`, `mod-timeout`, `mod-roles`: the dialogs.
//! - `mod-error`: a kick that fails, to see the failure shown.
//! - `settings-<page>` (`overview`, `roles`, `members`, `invites`, `bans`,
//!   `audit`), `settings-role` (the role editor), `settings-error` (bans
//!   page without made-up data: the load fails) and `invite`.
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
        json!({"id": GENERAL.to_string(), "type": 0, "name": "general", "position": 0, "parent_id": "600"}),
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
    channels.push(json!({"id": LOUNGE.to_string(), "type": 2, "name": "Lounge", "position": 10, "parent_id": "650"}));
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
    json!({
        "user": {"id": ME.to_string(), "username": "demo", "global_name": "Demo User"},
        "users": [
            {"id": "2", "username": "ada", "global_name": "Ada"},
            {"id": "3", "username": "grace", "global_name": "Grace"},
            {"id": "4", "username": "linus", "global_name": "Linus"},
            {"id": OWNER.to_string(), "username": "owner", "global_name": "Server Owner"}
        ],
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
        "private_channels": [],
        "relationships": [],
        "read_state": {"entries": []},
        "user_guild_settings": {"entries": []},
        "user_settings": {"status": "online", "guild_folders": []}
    })
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

impl AppState {
    pub(crate) fn show_demo(self: &Rc<Self>, mode: &str) {
        let token = Token::new("kylecord-demo-account");
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
            Token::new("kylecord-demo-account"),
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
        for channel in [GENERAL, LOUNGE, EMPTY_VOICE] {
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
            _ => {
                self.open_channel(general);
                self.demo_chat(&store);
            }
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
        if let (Some(popover), Ok(path)) = (menu, std::env::var("KYLECORD_SCREENSHOT")) {
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
