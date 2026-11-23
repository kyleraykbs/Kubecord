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
//!   `gif-link` is a bare link to a GIF, which Discord turns into an
//!   `image` embed (how a favourite sent from the picker arrives);
//!   `gif-cdn-link` a signed `cdn.discordapp.com` attachment link Discord
//!   sent no embed for, which the client shows itself; `gif-unfurl` that
//!   link followed, a moment later, by Discord's update adding its preview;
//!   `gif-video` a bare Klipy MP4 link (a `video` embed with only the video).
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
//! - `emoji`: Unicode emoji in Discord's art (inline, jumbo, reaction).
//! - `emoji-picker`: the emoji picker (opening on Standard) and emoji typed
//!   in the composer, in Discord's art.
//! - `bot`: an app's messages (buttons, menus, a private reply, thinking,
//!   a components layout) and the composer's command suggestions.
//! - `bot-form`: a form an app opened.
//! - `logger`: the message logger keeping two deleted messages, also after
//!   the channel is shown again. `KUBECORD_DELETED_MENU` right-clicks one,
//!   and drops an edited message's history through its menu.
//! - `share`: share mode with a range, a redaction and an added message,
//!   exported to `KUBECORD_SHARE_EXPORT` (a folder) when set.
//!   `KUBECORD_SHARE_GIF` adds a GIF to it (`video` for a GIF host's MP4).
//! - `spelling`: the right-click corrections for a misspelled word.
//! - `shortcode`: `:smi` lists standard emoji; a closing `:` makes the emoji.
//! - `emoji-undo`: undo/redo in the composer over an emoji drawn as an image.
//! - `status`: custom statuses -- yours with a Unicode emoji, Ada's with a
//!   server emoji (drawn as an image) -- then Edit Profile, where it is set.
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
        | kc_state::permissions::USE_SOUNDBOARD
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
        | kc_state::permissions::MANAGE_GUILD_EXPRESSIONS
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

/// Whether the demo answers REST calls itself.
pub(crate) fn fake_rest() -> bool {
    FAKE_REST.get()
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

/// Mentions of the demo account, newest first.
pub(crate) fn fake_mentions() -> Option<Vec<kc_state::Message>> {
    fake(|| {
        let mention = |minutes: u64, author: (&str, &str), content: &str, everyone: bool| {
            json!({"id": id_at(minutes * 60_000, 7).to_string(), "channel_id": GENERAL.to_string(), "guild_id": GUILD.to_string(),
                "author": {"id": author.0, "username": author.1, "global_name": author.1},
                "content": content, "timestamp": millis_to_iso(unix_millis_now() - minutes * 60_000),
                "mentions": [{"id": ME.to_string(), "username": "demo"}], "mention_everyone": everyone})
        };
        json!([
            mention(5, ("2", "ada"), "<@1> could you look at the build when you get a sec?", false),
            mention(42, ("3", "grace"), "@everyone the server's game night starts at 8!", true),
            mention(180, ("2", "ada"), "Thanks <@1>, that fixed it 🎉", false)
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

/// A signed Discord CDN attachment link to a GIF, for `gif-cdn-link`.
const DEMO_CDN_GIF: &str =
    "https://cdn.discordapp.com/attachments/1/2/creakingwumpus.gif?ex=6b021b5e&is=6b00c9de&hm=00";

/// A message that is only a link to a GIF, as the GIF picker's favourites
/// send: Discord turns it into an `image` embed whose proxy thumbnail is a
/// still, with the original GIF as the thumbnail's `url`.
fn gif_link_message(id: u64, author: u64) -> Value {
    let url = "https://upload.wikimedia.org/wikipedia/commons/2/2c/Rotating_earth_%28large%29.gif";
    let names = ["", "demo", "ada", "grace", "linus"];
    json!({
        "id": id.to_string(), "channel_id": GENERAL.to_string(), "guild_id": GUILD.to_string(),
        "author": {"id": author.to_string(), "username": names[author as usize % names.len()]},
        "content": url, "timestamp": iso_of(id), "type": 0,
        "attachments": [],
        "embeds": [{
            "type": "image", "url": url,
            "thumbnail": {
                "url": url,
                "proxy_url": "https://media.discordapp.net/external/demo/still.png",
                "width": 400, "height": 400,
                "content_type": "image/gif", "flags": 32,
            },
        }],
        "mentions": []
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
    /// Where the composer's text sits in its box: the gap from the box's
    /// top to the first line's top, the gap from the last line's bottom to
    /// the box's bottom, and the sizes behind them.
    fn composer_gaps(&self) -> (f64, f64, String) {
        let view = &self.composer;
        let buffer = view.buffer();
        let first = view.iter_location(&buffer.start_iter());
        let last = view.iter_location(&buffer.end_iter());
        let (_, first_y) = view.buffer_to_window_coords(gtk::TextWindowType::Widget, 0, first.y());
        let (_, last_y) =
            view.buffer_to_window_coords(gtk::TextWindowType::Widget, 0, last.y() + last.height());
        let mut frame: gtk::Widget = view.clone().upcast();
        while let Some(parent) = frame.parent() {
            frame = parent;
            if frame.has_css_class("kc-composer") {
                break;
            }
        }
        let origin = view
            .compute_point(&frame, &gtk::graphene::Point::new(0.0, 0.0))
            .map_or(0.0, |point| f64::from(point.y()));
        let top = origin + f64::from(first_y);
        let bottom = f64::from(frame.height()) - (origin + f64::from(last_y));
        let scroll = view.parent().and_downcast::<gtk::ScrolledWindow>();
        let adjustment = scroll.as_ref().map(|scroll| scroll.vadjustment());
        let (_, natural, ..) = view.measure(gtk::Orientation::Vertical, view.width());
        let detail = format!(
            "box {} view {}x{} scroll {} (min {} max {}) adj value {:.1} upper {:.1} page {:.1}; text {} lines, ends y {} h {}, view measures {natural}",
            frame.height(),
            view.width(),
            view.height(),
            scroll.as_ref().map_or(0, |scroll| scroll.height()),
            scroll.as_ref().map_or(0, |scroll| scroll.min_content_height()),
            scroll.as_ref().map_or(0, |scroll| scroll.max_content_height()),
            adjustment.as_ref().map_or(0.0, |adjustment| adjustment.value()),
            adjustment.as_ref().map_or(0.0, |adjustment| adjustment.upper()),
            adjustment.as_ref().map_or(0.0, |adjustment| adjustment.page_size()),
            buffer.line_count(),
            last.y(),
            last.height(),
        );
        (top, bottom, detail)
    }

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
            "forward" => {
                // The Forward dialog for a message in #general.
                self.open_channel(general.clone());
                let app = self.clone();
                let channel = general.id;
                glib::timeout_add_local_once(Duration::from_millis(600), move || {
                    let last = app
                        .with_session(|session| session.store.read(|state| state.last_message_id(channel)))
                        .flatten();
                    if let Some(message) = last {
                        app.show_forward(channel, message);
                    }
                });
            }
            "voice-messages" => {
                // A voice message, and recording another (from a test tone).
                use base64::Engine as _;
                let waveform: Vec<u8> = (0..120).map(|i: u32| ((i * 37 % 200) + 40 + (i % 7) * 2) as u8).collect();
                let mut posted = message(id_at(60_000, 30), 2, "");
                posted["flags"] = json!(8192);
                posted["attachments"] = json!([{
                    "id": "900", "filename": "voice-message.ogg", "size": 30000,
                    "url": "https://cdn.discordapp.com/attachments/1/900/voice-message.ogg",
                    "proxy_url": "", "content_type": "audio/ogg", "duration_secs": 14.6,
                    "waveform": base64::engine::general_purpose::STANDARD.encode(&waveform)
                }]);
                store.apply("MESSAGE_CREATE", &posted);
                self.open_channel(general.clone());
                // SAFETY: set before any thread reads the environment for it.
                unsafe { std::env::set_var("KUBECORD_TEST_MIC", "audiotestsrc is-live=true wave=pink-noise volume=0.4") };
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(600), move || app.record_voice_message());
            }
            "stickers" | "sticker-picker" => {
                // A Lottie sticker, a join to wave at; with `sticker-picker`,
                // the picker (packs from KUBECORD_DEMO_PACKS).
                let mut posted = message(id_at(120_000, 30), 3, "");
                posted["sticker_items"] = json!([{"id": "749054660769218631", "name": "Wave", "format_type": 3}]);
                store.apply("MESSAGE_CREATE", &posted);
                let mut joined = message(id_at(60_000, 30), 4, "");
                joined["type"] = json!(7);
                store.apply("MESSAGE_CREATE", &joined);
                self.open_channel(general.clone());
                if mode == "sticker-picker" {
                    if let Some(json) = std::env::var("KUBECORD_DEMO_PACKS").ok().and_then(|path| std::fs::read_to_string(path).ok()) {
                        crate::stickers::demo_packs(&json);
                    }
                    let app = self.clone();
                    glib::timeout_add_local_once(Duration::from_millis(2000), move || {
                        let anchor = app.attach_button.clone().upcast::<gtk::Widget>();
                        let Some(popover) = app.show_sticker_picker(&anchor) else {
                            eprintln!("demo: no sticker picker");
                            return;
                        };
                        popover.set_autohide(false);
                        // A popover is its own surface: saved beside the window.
                        if let Ok(path) = std::env::var("KUBECORD_SCREENSHOT") {
                            glib::timeout_add_local_once(Duration::from_millis(2000), move || {
                                if let Err(error) = crate::app::save_window_png(&popover, std::path::Path::new(&format!("{path}.popover.png"))) {
                                    eprintln!("demo: could not save the sticker picker: {error}");
                                }
                            });
                        }
                    });
                }
            }
            "inbox" => {
                FAKE_REST.set(true);
                self.open_channel(general.clone());
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(2000), move || {
                    app.show_inbox();
                    // A popover is its own surface: saved beside the window.
                    if let (Ok(path), Some(popover)) = (
                        std::env::var("KUBECORD_SCREENSHOT"),
                        app.inbox_button.last_child().and_downcast::<gtk::Popover>(),
                    ) {
                        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                            if let Err(error) = crate::app::save_window_png(&popover, std::path::Path::new(&format!("{path}.popover.png"))) {
                                eprintln!("demo: could not save the inbox: {error}");
                            }
                        });
                    }
                });
            }
            "requests" | "request-dm" => {
                // Two message requests (one Discord took for spam); with
                // `request-dm`, the first one open.
                FAKE_REST.set(true);
                for (channel, user, name, spam) in [("880", "88", "newperson", false), ("881", "89", "freenitro", true)] {
                    store.apply("CHANNEL_CREATE", &json!({"id": channel, "type": 1, "is_message_request": true, "is_spam": spam,
                        "recipients": [{"id": user, "username": name, "global_name": name}]}));
                    let mut first = message(id_at(30_000, 9), user.parse().unwrap_or(88), if spam {
                        "free nitro!! click here discord-gift.example"
                    } else {
                        "Hey! We met at the meetup last week, wanted to share those slides."
                    });
                    first["channel_id"] = json!(channel);
                    first["guild_id"] = json!(null);
                    store.apply("MESSAGE_CREATE", &first);
                }
                self.open_home();
                let app = self.clone();
                let open_dm = mode == "request-dm";
                glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                    if open_dm {
                        app.open_request_demo(Snowflake::new(880));
                    } else {
                        app.show_message_requests();
                    }
                });
            }
            "history-top" => {
                // Scroll to the top once: one page of older messages should
                // load and the view stay where it was, not keep loading.
                let older: Vec<kc_state::Message> = (0..200u64)
                    .filter_map(|n| {
                        let value = message(id_at((300 + n) * 3_600_000, n), 2 + n % 3, &format!("Older message {n}"));
                        serde_json::from_value(value).ok()
                    })
                    .collect();
                store.load_history(general.id, older);
                self.open_channel(general.clone());
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                    let rows = app.chat.borrow().rows.len();
                    eprintln!("history-top: rows before={rows}");
                    app.messages_scroll.vadjustment().set_value(0.0);
                    for ms in [400u64, 1200, 2500] {
                        let app = app.clone();
                        glib::timeout_add_local_once(Duration::from_millis(ms), move || {
                            let rows = app.chat.borrow().rows.len();
                            let value = app.messages_scroll.vadjustment().value();
                            eprintln!("history-top: +{ms}ms rows={rows} value={value:.0}");
                        });
                    }
                });
            }
            "embed" => {
                // Your own message with a link preview (hover it for the ×).
                let mut posted = message(id_at(30_000, 31), ME, "https://example.com/kubecord");
                posted["embeds"] = json!([{"type": "rich", "url": "https://example.com/kubecord", "title": "Kubecord",
                    "description": "A native Discord client in GTK4 and libadwaita.", "color": 0x5865f2,
                    "provider": {"name": "example.com"}}]);
                store.apply("MESSAGE_CREATE", &posted);
                self.open_channel(general.clone());
            }
            "soundboard" => {
                // In the lounge, with the soundboard open: Discord's sounds
                // and the server's own.
                store.apply("VOICE_STATE_UPDATE", &json!({"user_id": ME.to_string(), "channel_id": LOUNGE.to_string(),
                    "guild_id": GUILD.to_string(), "session_id": "x"}));
                store.apply("SOUNDBOARD_SOUNDS", &json!({"guild_id": GUILD.to_string(), "soundboard_sounds": [
                    {"sound_id": "950", "name": "airhorn", "volume": 1.0, "emoji_name": "📯", "guild_id": GUILD.to_string(), "available": true},
                    {"sound_id": "951", "name": "sad trombone", "volume": 0.8, "emoji_name": "🎺", "guild_id": GUILD.to_string(), "available": true},
                    {"sound_id": "952", "name": "bonk", "volume": 1.0, "emoji_name": "🔨", "guild_id": GUILD.to_string(), "available": true}
                ]}));
                let defaults = ["quack 🦆", "airhorn 🔊", "cricket 🦗", "golf clap 👏", "sad horn 🎺", "ba dum tss 🥁"]
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let (name, emoji) = entry.rsplit_once(' ').unwrap_or((entry, ""));
                        kc_state::SoundboardSound {
                            sound_id: Snowflake::new(index as u64 + 1),
                            name: name.to_owned(),
                            volume: 1.0,
                            emoji_name: Some(emoji.to_owned()),
                            available: true,
                            ..kc_state::SoundboardSound::default()
                        }
                    })
                    .collect();
                crate::soundboard::demo_defaults(defaults);
                self.with_session_mut(|session| {
                    session.voice = Some(VoiceUiState {
                        guild_id: Some(Snowflake::new(GUILD)),
                        channel_id: Snowflake::new(LOUNGE),
                        muted: false,
                        deafened: false,
                        camera: false,
                        streaming: false,
                        watching: None,
                    });
                });
                if let Some(lounge) = channel(LOUNGE) {
                    self.open_channel(lounge);
                }
                self.render_voice_panel();
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(2000), move || {
                    let anchor = app.voice_soundboard.clone().upcast::<gtk::Widget>();
                    if let (Some(popover), Ok(path)) = (app.show_soundboard(&anchor), std::env::var("KUBECORD_SCREENSHOT")) {
                        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                            if let Err(error) = crate::app::save_window_png(&popover, std::path::Path::new(&format!("{path}.popover.png"))) {
                                eprintln!("demo: could not save the soundboard: {error}");
                            }
                        });
                    }
                });
            }
            "stage" => {
                // A live stage: two speakers, a raised hand, an audience.
                store.apply("CHANNEL_CREATE", &json!({"id": "790", "type": 13, "guild_id": GUILD.to_string(),
                    "name": "Town Hall", "position": 40}));
                store.apply("STAGE_INSTANCE_CREATE", &json!({"id": "791", "guild_id": GUILD.to_string(),
                    "channel_id": "790", "topic": "Kubecord 1.0 planning", "privacy_level": 2}));
                for (user, suppress, hand) in [(ADA, false, false), (GRACE, false, false), (4u64, true, true), (ME, true, false)] {
                    store.apply("VOICE_STATE_UPDATE", &json!({"user_id": user.to_string(), "channel_id": "790",
                        "guild_id": GUILD.to_string(), "session_id": "x", "suppress": suppress,
                        "request_to_speak_timestamp": if hand { json!("2026-11-23T04:00:00Z") } else { json!(null) }}));
                }
                let stage = store.read(|state| state.channels.get(&Snowflake::new(790)).cloned());
                if let Some(stage) = stage {
                    self.voice_channel_clicked(stage);
                }
            }
            "polls" | "poll-create" | "poll-click" => {
                // A poll waiting for a vote, one voted in, one over; with
                // `poll-create`, the Create Poll dialog.
                let answers = json!([
                    {"answer_id": 1, "poll_media": {"text": "Pizza", "emoji": {"name": "🍕"}}},
                    {"answer_id": 2, "poll_media": {"text": "Sushi", "emoji": {"name": "🍣"}}},
                    {"answer_id": 3, "poll_media": {"text": "Tacos"}}
                ]);
                let later = kc_state::time::millis_to_iso(unix_millis_now() + 5 * 3_600_000);
                let polls = [
                    ("Lunch today?", json!(null), later.clone(), false),
                    ("Which games tonight?", json!({"is_finalized": false, "answer_counts": [
                        {"id": 1, "count": 4, "me_voted": true}, {"id": 2, "count": 2, "me_voted": false}, {"id": 3, "count": 1, "me_voted": false}]}), later, true),
                    ("Best pizza topping?", json!({"is_finalized": true, "answer_counts": [
                        {"id": 1, "count": 2, "me_voted": false}, {"id": 2, "count": 7, "me_voted": false}, {"id": 3, "count": 3, "me_voted": false}]}),
                        kc_state::time::millis_to_iso(unix_millis_now() - 3_600_000), false),
                ];
                for (index, (question, results, expiry, multi)) in polls.into_iter().enumerate() {
                    let mut posted = message(id_at((3 - index as u64) * 60_000, 30), 2 + index as u64, "");
                    posted["poll"] = json!({"question": {"text": question}, "answers": answers, "expiry": expiry,
                        "allow_multiselect": multi, "layout_type": 1, "results": results});
                    store.apply("MESSAGE_CREATE", &posted);
                }
                self.open_channel(general.clone());
                if mode == "poll-click" {
                    // Scrolled up a little, click an answer of the first
                    // poll; the view must not move.
                    let app = self.clone();
                    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                        let adjustment = app.messages_scroll.vadjustment();
                        adjustment.set_value(adjustment.upper() - adjustment.page_size() - 150.0);
                        let app = app.clone();
                        glib::timeout_add_local_once(Duration::from_millis(500), move || {
                            fn find(widget: &gtk::Widget) -> Option<gtk::ToggleButton> {
                                if widget.has_css_class("kc-poll-answer") && widget.is_sensitive() {
                                    return widget.clone().downcast().ok();
                                }
                                let mut child = widget.first_child();
                                while let Some(current) = child {
                                    if let Some(found) = find(&current) {
                                        return Some(found);
                                    }
                                    child = current.next_sibling();
                                }
                                None
                            }
                            let before = app.messages_scroll.vadjustment().value();
                            if let Some(answer) = find(app.messages_list.upcast_ref()) {
                                answer.grab_focus();
                                answer.set_active(true);
                            } else {
                                eprintln!("poll-click: no answer found");
                            }
                            let app = app.clone();
                            glib::timeout_add_local_once(Duration::from_millis(800), move || {
                                let after = app.messages_scroll.vadjustment().value();
                                eprintln!("poll-click: before={before:.0} after={after:.0}");
                            });
                        });
                    });
                }
                if mode == "poll-create" {
                    let app = self.clone();
                    glib::timeout_add_local_once(Duration::from_millis(600), move || app.show_create_poll());
                }
            }
            "search-complete" => {
                // Typing KUBECORD_SEARCH_TYPE into the search box: its
                // autocomplete, saved as <screenshot>.menu.png.
                self.open_channel(general.clone());
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(500), move || {
                    app.search_entry.grab_focus();
                    let text = std::env::var("KUBECORD_SEARCH_TYPE").unwrap_or_else(|_| "fr".to_owned());
                    app.search_entry.set_text(&text);
                    app.search_entry.set_position(-1);
                    glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                        let Ok(path) = std::env::var("KUBECORD_SCREENSHOT") else { return };
                        let popover = app.search_entry.last_child().and_downcast::<gtk::Popover>();
                        match popover {
                            Some(popover) if popover.is_visible() => {
                                let path = format!("{path}.menu.png");
                                if let Err(error) = crate::app::save_window_png(&popover, std::path::Path::new(&path)) {
                                    eprintln!("demo: could not save the dropdown: {error}");
                                }
                            }
                            _ => eprintln!("demo: no dropdown showing"),
                        }
                    });
                });
            }
            "search-hold" => {
                // Scrolled up a little, the side panel opens and closes: the
                // message at the top of the view must stay put.
                self.open_channel(general.clone());
                let app = self.clone();
                let top_row = move |app: &Rc<AppState>| -> String {
                    let value = app.messages_scroll.vadjustment().value();
                    app.chat
                        .borrow()
                        .rows
                        .iter()
                        .find_map(|entry| {
                            let bounds = entry.row.compute_bounds(&app.messages_list)?;
                            (f64::from(bounds.y() + bounds.height()) > value)
                                .then(|| format!("{} at {:.0}", entry.id, f64::from(bounds.y()) - value))
                        })
                        .unwrap_or_default()
                };
                glib::timeout_add_local_once(Duration::from_millis(800), move || {
                    let adjustment = app.messages_scroll.vadjustment();
                    adjustment.set_value((adjustment.upper() - adjustment.page_size() - 700.0).max(0.0));
                    let app2 = app.clone();
                    glib::timeout_add_local_once(Duration::from_millis(500), move || {
                        eprintln!("demo: hold before: {}", top_row(&app2));
                        app2.demo_search("cats", Vec::new());
                        let app3 = app2.clone();
                        glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                            eprintln!("demo: hold with panel: {}", top_row(&app3));
                            app3.close_search();
                            let app4 = app3.clone();
                            glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                                eprintln!("demo: hold after close: {}", top_row(&app4));
                            });
                        });
                    });
                });
            }
            "search-focus" => {
                // Clicking into the search box: the panel opens on its hints.
                self.open_channel(general.clone());
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(500), move || {
                    app.search_entry.grab_focus();
                });
            }
            "search" | "search-filters" => {
                // Search results for typed filters, answered locally; with
                // `search-filters`, the Filters dialog over them.
                self.open_channel(general.clone());
                let results: Vec<kc_state::Message> = ["the cat photo from yesterday", "cats are great", "look at this cat"]
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, text)| {
                        serde_json::from_value(message(id_at((index as u64 + 1) * 3_600_000, 20), 2 + index as u64 % 2, text)).ok()
                    })
                    .collect();
                let app = self.clone();
                let filters = mode == "search-filters";
                glib::timeout_add_local_once(Duration::from_millis(500), move || {
                    app.demo_search("from:ada has:image in:general before:2026-12-01 cats", results);
                    if filters {
                        app.show_search_filters();
                    }
                });
            }
            "sidebar-chat" => {
                // The chat button on a voice channel's sidebar row, clicked.
                self.open_channel(general.clone());
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(600), move || {
                    let mut child = app.channel_box.first_child();
                    while let Some(widget) = child {
                        if widget.has_css_class("kc-channel-row")
                            && widget.first_child().is_some_and(|row| row.widget_name() == format!("channel-{LOUNGE}"))
                            && let Some(chat) = widget.last_child().and_downcast::<gtk::Button>()
                        {
                            eprintln!("demo: clicking the Lounge chat button");
                            chat.emit_clicked();
                            return;
                        }
                        child = widget.next_sibling();
                    }
                    eprintln!("demo: no chat button on the Lounge row");
                });
            }
            "voice-side-chat" => {
                // A voice channel with its text chat beside the call.
                for (index, text) in ["anyone joining?", "in a sec", "my mic is muted, typing here instead", "👍"]
                    .into_iter()
                    .enumerate()
                {
                    let mut posted = message(id_at((4 - index as u64) * 60_000, 10), 2 + index as u64 % 3, text);
                    posted["channel_id"] = json!(LOUNGE.to_string());
                    store.apply("MESSAGE_CREATE", &posted);
                }
                if let Some(lounge) = channel(LOUNGE) {
                    self.open_channel(lounge);
                }
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(400), move || {
                    app.side_chat_toggle.set_active(true);
                });
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
                // Some of the server's own emoji and stickers.
                store.apply("GUILD_EMOJIS_UPDATE", &json!({"guild_id": GUILD.to_string(), "emojis": [
                    {"id": "900001", "name": "kubecord", "animated": false},
                    {"id": "900002", "name": "party_parrot", "animated": true},
                    {"id": "900003", "name": "thinking_hard", "animated": false}
                ]}));
                store.apply("SOUNDBOARD_SOUNDS", &json!({"guild_id": GUILD.to_string(), "soundboard_sounds": [
                    {"sound_id": "950", "name": "airhorn", "volume": 1.0, "emoji_name": "📯", "guild_id": GUILD.to_string()},
                    {"sound_id": "951", "name": "sad trombone", "volume": 0.6, "emoji_name": "🎺", "guild_id": GUILD.to_string()}
                ]}));
                store.apply("GUILD_STICKERS_UPDATE", &json!({"guild_id": GUILD.to_string(), "stickers": [
                    {"id": "749054660769218631", "name": "Hello", "tags": "👋", "description": "Says hi", "format_type": 3, "guild_id": GUILD.to_string()}
                ]}));
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
            "emoji" => {
                // Unicode emoji in Discord's art: inline with a link and a
                // hidden spoiler, a jumbo emoji-only message, code left as
                // text, and a reaction.
                store.apply(
                    "MESSAGE_CREATE",
                    &message(
                        id_at(0, 41),
                        ADA,
                        "Shipping it today 🚀 notes at https://example.com ||surprise 🤫|| and `🚀` in code",
                    ),
                );
                store.apply("MESSAGE_CREATE", &message(id_at(0, 42), GRACE, "👍🏽❤️🏳️‍🌈"));
                let mut reacted = message(id_at(0, 43), ADA, "Plain text with © and #1 stays text");
                reacted["reactions"] =
                    json!([{"emoji": {"id": null, "name": "🎉"}, "count": 3, "me": true}]);
                store.apply("MESSAGE_CREATE", &reacted);
                self.open_channel(general);
            }
            "emoji-picker" => {
                // The picker (opening on Standard) and emoji typed in the
                // composer, drawn in Discord's art but sent as text.
                self.open_channel(general);
                self.set_composer_text("Typing 🚀 with emoji 👍🏽 in the box");
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_secs(3), move || {
                    eprintln!("demo: composer text = {:?}", app.composer_text());
                });
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                    let Some(popover) = app.emoji_button.popover() else {
                        return;
                    };
                    // Without a real click the compositor may refuse the popover's
                    // input grab, so show it without auto-hide.
                    popover.set_autohide(false);
                    app.emoji_button.popup();
                    // Popovers are their own surfaces: save this one apart.
                    if let Ok(path) = std::env::var("KUBECORD_POPOVER_SHOT") {
                        glib::timeout_add_local_once(Duration::from_secs(6), move || {
                            if let Err(error) =
                                crate::app::save_window_png(&popover, std::path::Path::new(&path))
                            {
                                eprintln!("demo: could not save the picker: {error}");
                            }
                        });
                    }
                });
            }
            "bot" | "bot-form" => {
                // An app's messages: a command's answer with buttons and a
                // menu, one only you see, one still thinking, and one laid
                // out with components; then the composer's command list.
                let bot = json!({"id": "900", "username": "Groovy", "bot": true});
                let me = json!({"id": "1", "username": "demo"});
                let mut played = message(id_at(0, 41), 2, "Now playing **Never Gonna Give You Up** 🎶");
                played["author"] = bot.clone();
                played["type"] = json!(20);
                played["application_id"] = json!("900");
                played["interaction_metadata"] = json!({"id": "5", "type": 2, "name": "music play", "user": me});
                played["interaction"] = json!({"id": "5", "type": 2, "name": "music play", "user": me});
                played["components"] = json!([
                    {"type": 1, "components": [
                        {"type": 2, "style": 1, "label": "Pause", "custom_id": "pause", "emoji": {"name": "⏸️"}},
                        {"type": 2, "style": 2, "label": "Skip", "custom_id": "skip"},
                        {"type": 2, "style": 3, "label": "Like", "custom_id": "like", "emoji": {"name": "👍"}},
                        {"type": 2, "style": 4, "label": "Stop", "custom_id": "stop"},
                        {"type": 2, "style": 5, "label": "Lyrics", "url": "https://example.com"},
                        {"type": 2, "style": 2, "label": "Queue", "custom_id": "queue", "disabled": true}
                    ]},
                    {"type": 1, "components": [
                        {"type": 3, "custom_id": "volume", "placeholder": "Volume", "options": [
                            {"label": "Quiet", "value": "25", "description": "25%"},
                            {"label": "Normal", "value": "50", "default": true},
                            {"label": "Loud", "value": "100", "emoji": {"name": "🔊"}}
                        ]}
                    ]}
                ]);
                store.apply("MESSAGE_CREATE", &played);
                let mut private = message(id_at(0, 42), 2, "Your playlist was saved.");
                private["author"] = bot.clone();
                private["type"] = json!(20);
                private["flags"] = json!(64);
                private["application_id"] = json!("900");
                private["interaction_metadata"] = json!({"id": "6", "type": 2, "user": me});
                private["interaction"] = json!({"id": "6", "type": 2, "name": "playlist save", "user": me});
                store.apply("MESSAGE_CREATE", &private);
                let mut thinking = message(id_at(0, 43), 2, "");
                thinking["author"] = bot.clone();
                thinking["type"] = json!(20);
                thinking["flags"] = json!(128);
                thinking["application_id"] = json!("900");
                thinking["interaction"] = json!({"id": "7", "type": 2, "name": "lyrics", "user": me});
                store.apply("MESSAGE_CREATE", &thinking);
                let mut laid_out = message(id_at(0, 44), 2, "");
                laid_out["author"] = bot;
                laid_out["flags"] = json!(1 << 15);
                laid_out["application_id"] = json!("900");
                laid_out["components"] = json!([
                    {"type": 17, "accent_color": 5793266, "components": [
                        {"type": 10, "content": "## Server Stats\nMembers online: **42**"},
                        {"type": 14, "divider": true, "spacing": 1},
                        {"type": 9, "components": [
                            {"type": 10, "content": "Top track this week\n*Never Gonna Give You Up*"}
                        ], "accessory": {"type": 2, "style": 1, "label": "Play", "custom_id": "play-top"}},
                        {"type": 1, "components": [
                            {"type": 5, "custom_id": "who", "placeholder": "Pick a member"}
                        ]}
                    ]}
                ]);
                store.apply("MESSAGE_CREATE", &laid_out);
                self.open_channel(general);

                self.seed_commands(kc_http::CommandIndex::from_value(&json!({
                    "applications": [{"id": "900", "name": "Groovy"}],
                    "application_commands": [
                        {"id": "50", "application_id": "900", "version": "1", "type": 1, "name": "music",
                         "description": "Play music", "options": [
                            {"type": 1, "name": "play", "description": "Play a song", "options": [
                                {"type": 3, "name": "song", "description": "What to play", "required": true},
                                {"type": 5, "name": "loop", "description": "Repeat it"}
                            ]},
                            {"type": 1, "name": "stop", "description": "Stop playing"}
                         ]},
                        {"id": "52", "application_id": "900", "version": "1", "type": 3, "name": "Add to Queue",
                         "description": ""},
                        {"id": "53", "application_id": "900", "version": "1", "type": 2, "name": "Show Top Tracks",
                         "description": ""},
                        {"id": "54", "application_id": "900", "version": "1", "type": 1, "name": "cover",
                         "description": "Set the cover art", "options": [
                            {"type": 11, "name": "image", "description": "The picture", "required": true}
                         ]},
                        {"id": "51", "application_id": "900", "version": "1", "type": 1, "name": "mood",
                         "description": "Set the mood", "options": [
                            {"type": 3, "name": "kind", "description": "Which", "required": true,
                             "choices": [{"name": "Chill", "value": "chill"}, {"name": "Party", "value": "party"}]}
                         ]}
                    ]
                })));
                let app = self.clone();
                let form = mode == "bot-form";
                glib::timeout_add_local_once(Duration::from_millis(800), move || {
                    if form {
                        app.show_app_form(&json!({
                            "id": "70", "nonce": "1", "channel_id": GENERAL.to_string(),
                            "title": "Song Request", "custom_id": "request",
                            "application": {"id": "900", "name": "Groovy"},
                            "components": [
                                {"type": 1, "components": [{"type": 4, "custom_id": "song", "style": 1,
                                    "label": "Song name", "placeholder": "Never Gonna Give You Up",
                                    "required": true, "min_length": 2, "max_length": 100}]},
                                {"type": 10, "content": "Tell us why you like it."},
                                {"type": 18, "label": "Why?", "description": "A few words",
                                 "component": {"type": 4, "custom_id": "why", "style": 2, "required": false}},
                                {"type": 18, "label": "Mood",
                                 "component": {"type": 3, "custom_id": "mood", "options": [
                                    {"label": "Chill", "value": "chill"}, {"label": "Party", "value": "party"}]}},
                                {"type": 18, "label": "Dedicate it to",
                                 "component": {"type": 5, "custom_id": "for", "required": false}},
                                {"type": 18, "label": "Cover art", "description": "An image, if you have one",
                                 "component": {"type": 19, "custom_id": "art", "required": false}}
                            ]
                        }));
                        return;
                    }
                    let buffer = app.composer.buffer();
                    for (typed, label) in [
                        ("/mu", "/mu"),
                        ("sic ", "/music "),
                        ("play ", "/music play "),
                        ("never gonna lo", "/music play never gonna lo"),
                    ] {
                        buffer.insert_at_cursor(typed);
                        app.refresh_completion();
                        eprintln!("demo: {label:?} -> {:?}", crate::composer::completion_titles());
                    }
                    app.set_composer_text("/mood kind:Pa");
                    app.refresh_completion();
                    eprintln!("demo: \"/mood kind:Pa\" -> {:?}", crate::composer::completion_titles());
                    app.set_composer_text("/cover image:");
                    app.refresh_completion();
                    eprintln!("demo: \"/cover image:\" -> {:?}", crate::composer::completion_titles());
                    let names = |kind| {
                        app.context_commands(kind)
                            .into_iter()
                            .map(|(command, app)| format!("{} ({app})", command.name))
                            .collect::<Vec<_>>()
                    };
                    eprintln!(
                        "demo: message apps {:?}, user apps {:?}",
                        names(kc_http::Command::MESSAGE),
                        names(kc_http::Command::USER)
                    );
                    app.set_composer_text("");
                });
            }
            "logger" => {
                // The message logger: a new message deleted, and an older
                // one (loaded with the history, so logged only as it goes)
                // deleted too; both stay, in red. Then the channel's
                // messages are dropped and the channel shown again: the
                // deleted ones come back from the log.
                self.open_channel(general);
                let app = self.clone();
                let store = store.clone();
                glib::timeout_add_local_once(Duration::from_millis(1000), move || {
                    let send = {
                        let store = store.clone();
                        move |name: &str, data: &Value| {
                            crate::logger::observe(account, name, data, &store);
                            store.apply(name, data);
                        }
                    };
                    let gone = id_at(0, 41);
                    send(
                        "MESSAGE_CREATE",
                        &message(gone, ADA, "This message gets deleted, but the logger keeps it."),
                    );
                    send("MESSAGE_CREATE", &message(id_at(0, 42), GRACE, "This one stays."));
                    // Edited twice: both earlier versions are kept.
                    let edited = id_at(0, 43);
                    send("MESSAGE_CREATE", &message(edited, 4, "Meet at 5."));
                    for (step, text) in ["Meet at 6.", "Meet at 6, at the cafe."].iter().enumerate() {
                        let mut changed = message(edited, 4, text);
                        changed["edited_timestamp"] = json!(iso_of(id_at(0, 50 + step as u64)));
                        send("MESSAGE_UPDATE", &changed);
                    }
                    // A message loaded with the history, edited: its first
                    // version comes from the store.
                    let loaded = app.chat.borrow().rows.iter().rev().nth(6).map(|entry| entry.id);
                    if let Some(loaded) = loaded {
                        let mut changed = message(loaded.get(), ADA, "Message number 34, edited.");
                        changed["edited_timestamp"] = json!(iso_of(id_at(0, 60)));
                        send("MESSAGE_UPDATE", &changed);
                    }
                    let older = app.chat.borrow().rows.iter().rev().nth(4).map(|entry| entry.id);
                    let app = app.clone();
                    let store = store.clone();
                    glib::timeout_add_local_once(Duration::from_millis(800), move || {
                        send("MESSAGE_DELETE", &json!({"id": gone.to_string(), "channel_id": GENERAL.to_string()}));
                        if let Some(older) = older {
                            send("MESSAGE_DELETE", &json!({"id": older.to_string(), "channel_id": GENERAL.to_string()}));
                        }
                        let (all, deleted, _) = crate::logger::stats();
                        eprintln!("demo: logged {all}, deleted {deleted}");
                        let kept = |label: &str| {
                            let shown: Vec<String> = store.read(|state| {
                                state
                                    .messages
                                    .get(&Snowflake::new(GENERAL))
                                    .map(|buffer| {
                                        buffer
                                            .messages()
                                            .iter()
                                            .filter(|message| message.deleted)
                                            .map(|message| message.content.clone())
                                            .collect()
                                    })
                                    .unwrap_or_default()
                            });
                            eprintln!("demo: {label}: deleted shown {shown:?}");
                        };
                        kept("after deleting");
                        // Dropped from the store, then shown again.
                        store.write(|state| {
                            if let Some(buffer) = state.messages.get_mut(&Snowflake::new(GENERAL)) {
                                buffer.remove(Snowflake::new(gone));
                                if let Some(older) = older {
                                    buffer.remove(older);
                                }
                            }
                        });
                        app.redraw_times();
                        kept("after reloading");
                        let edits: Vec<(String, Vec<String>)> = store.read(|state| {
                            state
                                .messages
                                .get(&Snowflake::new(GENERAL))
                                .map(|buffer| {
                                    buffer
                                        .messages()
                                        .iter()
                                        .filter(|message| !message.edits.is_empty())
                                        .map(|message| {
                                            (
                                                message.content.clone(),
                                                message.edits.iter().map(|edit| edit.content.clone()).collect(),
                                            )
                                        })
                                        .collect()
                                })
                                .unwrap_or_default()
                        });
                        eprintln!("demo: edit history {edits:?}");
                        // KUBECORD_DELETED_MENU: a deleted message
                        // right-clicked on its text.
                        if std::env::var_os("KUBECORD_DELETED_MENU").is_some() {
                            let app = app.clone();
                            glib::timeout_add_local_once(Duration::from_millis(600), move || {
                                app.demo_deleted_menu();
                            });
                        }
                    });
                });
            }
            "share" => {
                // Share mode: a range picked by two clicks, one message
                // redacted in it and one added outside it; both exports.
                // Two calls close the channel: one ended, one going on.
                let mut ended = message(id_at(0, 41), ADA, "");
                ended["type"] = json!(3);
                ended["call"] = json!({"participants": [ADA.to_string()],
                    "ended_timestamp": iso_of(id_at(0, 41) + ((12 * 60 * 1000) << 22))});
                store.apply("MESSAGE_CREATE", &ended);
                let mut ongoing = message(id_at(0, 55), GRACE, "");
                ongoing["type"] = json!(3);
                ongoing["call"] = json!({"participants": [GRACE.to_string()]});
                store.apply("MESSAGE_CREATE", &ongoing);
                // KUBECORD_SHARE_GIF: an animated GIF last, so pictures are
                // drawn while it plays.
                // `video`: a GIF host's MP4 (played as a looping video).
                match std::env::var("KUBECORD_SHARE_GIF").as_deref() {
                    Ok("video") => {
                        let url = "https://static.klipy.com/ii/a15b48460c436e1e92c85ffc680932cc/d1/60/IgO8Znh3bKlSCc.mp4";
                        let mut gif = gif_link_message(id_at(0, 57), ADA);
                        gif["content"] = json!(url);
                        gif["embeds"] = json!([{
                            "type": "video", "url": url,
                            "video": {
                                "url": url, "width": 402, "height": 360,
                                "content_type": "video/mp4", "flags": 0,
                            },
                        }]);
                        store.apply("MESSAGE_CREATE", &gif);
                    }
                    Ok(_) => store.apply("MESSAGE_CREATE", &gif_message(id_at(0, 57), ADA)),
                    Err(_) => {}
                }
                self.open_channel(general);
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                    let folder = std::env::var_os("KUBECORD_SHARE_EXPORT").map(std::path::PathBuf::from);
                    app.demo_share(folder);
                });
            }
            // The home page: the friends list.
            "friends" => self.open_home(),
            "font-toggle" => {
                // Theming's "Use Discord's Font" switched on and off by its
                // own switch, as a click does.
                self.open_channel(general);
                self.show_settings_page("theming");
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                    let rows = descendants::<libadwaita::SwitchRow>(&app.window.clone().upcast());
                    let font = rows.into_iter().find(|row| libadwaita::prelude::PreferencesRowExt::title(row) == "Use Discord's Font");
                    let Some(font) = font else {
                        eprintln!("demo: no font switch");
                        return;
                    };
                    let on = std::env::var("KUBECORD_FONT_TO").as_deref() != Ok("off");
                    eprintln!("demo: font switch was {}, setting {on}", font.is_active());
                    font.set_active(on);
                    // Its Nix badge, when the setting is declared.
                    let badges = descendants::<gtk::Label>(&font.clone().upcast());
                    if let Some(badge) = badges.iter().find(|label| label.has_css_class("kc-nix-badge")) {
                        eprintln!("demo: font badge reads {:?}", badge.label());
                    }
                });
            }
            "uploads" => {
                // Files attached to a message not yet sent: a picture, an
                // animated GIF (KUBECORD_UPLOAD_GIF, a path) and a text file.
                self.open_channel(general);
                let mut files: Vec<std::path::PathBuf> = vec![
                    "data/icons/kubecord.png".into(),
                    "Cargo.toml".into(),
                    "README.md".into(),
                ];
                if let Some(gif) = std::env::var_os("KUBECORD_UPLOAD_GIF") {
                    files.insert(1, gif.into());
                }
                // KUBECORD_UPLOAD_FILES: these instead (paths, `:`-separated).
                if let Some(list) = std::env::var_os("KUBECORD_UPLOAD_FILES") {
                    files = std::env::split_paths(&list).collect();
                }
                for path in files {
                    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                    match std::fs::read(&path) {
                        Ok(bytes) => self.add_upload(name, bytes),
                        Err(error) => eprintln!("demo: could not read {}: {error}", path.display()),
                    }
                }

                // KUBECORD_UPLOAD_SEND: send them with no text. Offline the
                // upload fails, which shows that a send was attempted.
                if std::env::var_os("KUBECORD_UPLOAD_SEND").is_some() {
                    let app = self.clone();
                    glib::timeout_add_local_once(Duration::from_millis(800), move || {
                        app.send_current();
                        let left = app.with_session(|session| session.uploads.len()).unwrap_or(0);
                        eprintln!("demo: sent with no text; {left} attachments left in the box");
                        let app = app.clone();
                        glib::timeout_add_local_once(Duration::from_millis(2500), move || {
                            let notices = descendants::<gtk::Label>(&app.notices.clone().upcast());
                            let texts: Vec<String> = notices.iter().map(|label| label.text().to_string()).collect();
                            eprintln!("demo: notices: {texts:?}");
                        });
                    });
                }
            }
            "spelling" => {
                // Right-click a misspelled word: its corrections, then Add to
                // Dictionary.
                self.open_channel(general);
                self.set_composer_text("I recieve teh mesage");
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                    let started = std::time::Instant::now();
                    let found = crate::spelling::suggestions("mesage", 5);
                    eprintln!("demo: mesage -> {found:?} in {:?}", started.elapsed());
                    let buffer = app.composer.buffer();
                    let at = app.composer.iter_location(&buffer.iter_at_offset(11));
                    let (x, y) = app.composer.buffer_to_window_coords(
                        gtk::TextWindowType::Widget,
                        at.x() + 2,
                        at.y() + at.height() / 2,
                    );
                    let Some(popover) = app.spelling_menu(x as f64, y as f64) else {
                        eprintln!("demo: no menu at teh");
                        return;
                    };
                    // Without a real click the compositor may refuse the
                    // popover's input grab, so show it without auto-hide.
                    popover.set_autohide(false);
                    popover.popup();
                    if let Ok(path) = std::env::var("KUBECORD_POPOVER_SHOT") {
                        glib::timeout_add_local_once(Duration::from_secs(1), move || {
                            if let Err(error) =
                                crate::app::save_window_png(&popover, std::path::Path::new(&path))
                            {
                                eprintln!("demo: could not save the menu: {error}");
                            }
                            // Pick the first correction, as a click would.
                            let _ = popover.activate_action("spell.replace", Some(&"the".to_variant()));
                            let app = app.clone();
                            glib::timeout_add_local_once(Duration::from_millis(300), move || {
                                eprintln!("demo: after choosing = {:?}", app.composer_text());
                            });
                        });
                    }
                });
            }
            "shortcode" => {
                // `:smi` lists standard emoji; closing `:smile:` makes it one.
                self.open_channel(general);
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(500), move || {
                    app.composer.grab_focus();
                    app.composer.buffer().insert_at_cursor("hi :smi");
                    let app = app.clone();
                    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                        let shown = crate::composer::completion_titles();
                        eprintln!("demo: list for :smi = {shown:?}");
                        let buffer = app.composer.buffer();
                        buffer.insert_at_cursor("le");
                        buffer.insert_at_cursor(":");
                        let app = app.clone();
                        glib::timeout_add_local_once(Duration::from_millis(500), move || {
                            eprintln!("demo: after :smile: = {:?}", app.composer_text());
                        });
                    });
                });
            }
            "emoji-undo" => {
                // Undo and redo over an emoji drawn as an image: each step
                // brings back exactly the text before it, no duplicates.
                self.open_channel(general);
                let app = self.clone();
                let report = |app: &Rc<Self>, label: &str| {
                    let buffer = app.composer.buffer();
                    let images = buffer
                        .slice(&buffer.start_iter(), &buffer.end_iter(), true)
                        .matches('\u{fffc}')
                        .count();
                    eprintln!("demo: {label}: {:?} ({images} images)", app.composer_text());
                };
                let steps: Vec<(u64, &'static str)> = vec![
                    (500, "type"),
                    (1500, "emoji"),
                    (2500, "undo"),
                    (3000, "undo"),
                    (3500, "undo"),
                    (4000, "redo"),
                    (4500, "redo"),
                    (5000, "redo"),
                ];
                for (delay, step) in steps {
                    let app = app.clone();
                    glib::timeout_add_local_once(Duration::from_millis(delay), move || {
                        let buffer = app.composer.buffer();
                        match step {
                            "type" => buffer.insert_at_cursor("go "),
                            "emoji" => buffer.insert_at_cursor("🚀"),
                            "undo" => app.undo_composer(false),
                            _ => app.undo_composer(true),
                        }
                        let app = app.clone();
                        glib::timeout_add_local_once(Duration::from_millis(200), move || {
                            report(&app, step);
                        });
                    });
                }
            }
            "gif-link" => {
                // A bare link to a GIF (an `image` embed), as favourites send.
                store.apply("MESSAGE_CREATE", &gif_link_message(id_at(0, 41), ADA));
                self.open_channel(general);
            }
            "gif-cdn-link" => {
                // A signed cdn.discordapp.com attachment link that Discord sent
                // no embed for: the client shows the GIF itself.
                let mut message = gif_link_message(id_at(0, 41), ADA);
                message["content"] = json!(DEMO_CDN_GIF);
                message["embeds"] = json!([]);
                store.apply("MESSAGE_CREATE", &message);
                self.open_channel(general);
            }
            "gif-video" => {
                // A bare link to a Klipy MP4, as Discord embeds it: a `video`
                // embed carrying only the video.
                let url = "https://static.klipy.com/ii/a15b48460c436e1e92c85ffc680932cc/d1/60/IgO8Znh3bKlSCc.mp4";
                let mut message = gif_link_message(id_at(0, 41), ADA);
                message["content"] = json!(url);
                message["embeds"] = json!([{
                    "type": "video", "url": url,
                    "video": {
                        "url": url, "width": 402, "height": 360,
                        "content_type": "video/mp4", "flags": 0,
                    },
                }]);
                // Arrives after the channel is open, at the bottom, as a GIF
                // just sent does: it must start playing without a scroll.
                self.open_channel(general);
                let store = store.clone();
                glib::timeout_add_local_once(Duration::from_millis(800), move || {
                    store.apply("MESSAGE_CREATE", &message);
                    eprintln!("demo: gif sent");
                });
            }
            "gif-unfurl" => {
                // The real sequence for a posted GIF link: the bare link first
                // (shown by the client itself), then Discord's update adding
                // its preview -- shaped as Discord sends it: the embed's `url`
                // without the signature, the thumbnail's with it.
                let id = id_at(0, 41);
                let mut message = gif_link_message(id, ADA);
                message["content"] = json!(DEMO_CDN_GIF);
                message["embeds"] = json!([]);
                store.apply("MESSAGE_CREATE", &message);
                self.open_channel(general);
                let store = store.clone();
                glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                    let bare = DEMO_CDN_GIF.split('?').next().unwrap_or(DEMO_CDN_GIF);
                    let proxy = DEMO_CDN_GIF.replace("cdn.discordapp.com", "media.discordapp.net");
                    message["embeds"] = json!([{
                        "type": "image", "url": bare,
                        "thumbnail": {
                            "url": DEMO_CDN_GIF, "proxy_url": proxy,
                            "width": 1280, "height": 966,
                            "content_type": "image/gif", "flags": 0,
                        },
                        "content_scan_version": 4,
                    }]);
                    store.apply("MESSAGE_UPDATE", &message);
                    eprintln!("demo: preview attached");
                });
            }
            "code" => {
                let blocks = [
                    "Here's the fix:\n```rust\nfn main() {\n    // Greets the world.\n    let name = \"world\";\n    println!(\"Hello, {name}! {}\", 1 + 2);\n}\n```\nworks now",
                    "```py\nimport os\n\ndef walk(path: str) -> list[str]:\n    return [f for f in os.listdir(path) if f.endswith('.py')]\n```",
                    "config 👍\n```nix\n{ pkgs, ... }: {\n  programs.kubecord.enable = true;\n  environment.systemPackages = [ pkgs.git ];\n}\n```",
                    "```ts\nconst add = (a: number, b: number): number => a + b; // <b>not bold</b>\n```\n```\nno language here, just text\n```\n```madeuplang\nplain too\n```",
                ];
                for (index, content) in blocks.iter().enumerate() {
                    store.apply("MESSAGE_CREATE", &message(id_at(0, 50 + index as u64), GRACE, content));
                }
                self.open_channel(general);
                // KUBECORD_CODE_SELECT: a selection across the code blocks.
                if std::env::var_os("KUBECORD_CODE_SELECT").is_some() {
                    let app = self.clone();
                    glib::timeout_add_local_once(Duration::from_millis(1500), move || app.demo_select_across());
                }
            }
            "select-across" => {
                // A selection dragged across several messages, one with
                // emoji in Discord's art (it copies as the emoji).
                store.apply("MESSAGE_CREATE", &message(id_at(0, 50), GRACE, "Nice work on that 👍🏽 really"));
                store.apply("MESSAGE_CREATE", &message(id_at(0, 51), GRACE, "and one more thing"));
                self.open_channel(general);
                let app = self.clone();
                if std::env::var_os("KUBECORD_NO_SELECT").is_none() {
                    glib::timeout_add_local_once(Duration::from_millis(1500), move || app.demo_select_across());
                }
            }
            "jump" => {
                // A jump to an older message with GIFs around it still loading
                // (their rows grow after the jump): it must stay in the middle,
                // and its highlight fade.
                let target = id_at(0, 30);
                for index in 0..4 {
                    store.apply("MESSAGE_CREATE", &gif_message(id_at(0, 20 + index), ADA));
                }
                store.apply("MESSAGE_CREATE", &message(target, GRACE, "This is the message the jump goes to."));
                for index in 0..4 {
                    store.apply("MESSAGE_CREATE", &gif_message(id_at(0, 40 + index), ADA));
                }
                self.open_channel(general);
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(300), move || {
                    // Rebuilt first, as after fetching the page around it.
                    app.chat.borrow_mut().channel = None;
                    app.render_messages();
                    eprintln!("demo: jumped: {}", app.jump_to_message(Snowflake::new(target)));
                    // KUBECORD_JUMP_REBUILD: its row is rebuilt mid-highlight,
                    // as older messages going in above it do.
                    if std::env::var_os("KUBECORD_JUMP_REBUILD").is_some() {
                        let app = app.clone();
                        glib::timeout_add_local_once(Duration::from_millis(500), move || {
                            app.message_updated(Snowflake::new(GENERAL), Snowflake::new(target));
                            eprintln!("demo: target row rebuilt");
                        });
                    }
                    for ms in [300u64, 1500, 3000, 5000] {
                        let app = app.clone();
                        glib::timeout_add_local_once(Duration::from_millis(ms), move || {
                            let chat = app.chat.borrow();
                            let Some(entry) = chat.rows.iter().find(|entry| entry.id == Snowflake::new(target)) else { return };
                            let Some(bounds) = entry.row.compute_bounds(&app.messages_scroll) else { return };
                            let centre = f64::from(bounds.y() + bounds.height() / 2.0);
                            let view = f64::from(app.messages_scroll.height()) / 2.0;
                            let adjustment = app.messages_scroll.vadjustment();
                            eprintln!(
                                "demo: after {ms} ms the target is {:+.0} px from the middle (scroll {:.0} of {:.0}, flash={} fading={})",
                                centre - view,
                                adjustment.value(),
                                adjustment.upper() - adjustment.page_size(),
                                entry.body.has_css_class("kc-flash"),
                                entry.body.has_css_class("kc-flash-out")
                            );
                        });
                    }
                });
            }
            "safety" => {
                // The safety limits' dialogs: new DMs until it asks
                // (KUBECORD_SAFETY_COUNT sets how many go first; 5 reaches
                // the hourly cap).
                let count: usize = std::env::var("KUBECORD_SAFETY_COUNT")
                    .ok()
                    .and_then(|count| count.parse().ok())
                    .unwrap_or(2);
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(1000), move || {
                    // Spread out, so only the caps (not the warning) apply
                    // past the first few.
                    let age = std::env::var("KUBECORD_SAFETY_AGE")
                        .ok()
                        .and_then(|age| age.parse().ok())
                        .unwrap_or(11 * 60);
                    for _ in 0..count {
                        crate::safety::demo_record(&app, crate::safety::Action::NewDm, age);
                    }
                    app.guard(crate::safety::Action::NewDm, |_| eprintln!("demo: the new DM went ahead"));
                });
            }
            "composer-lines" => {
                // KUBECORD_NO_OVERLAY_SCROLL: scrollbars that take room, as
                // some desktops set.
                if std::env::var_os("KUBECORD_NO_OVERLAY_SCROLL").is_some()
                    && let Some(settings) = gtk::Settings::default()
                {
                    settings.set_gtk_overlay_scrolling(false);
                }
                // Typed a character a frame until the text wraps onto a
                // second line, as typing does; KUBECORD_COMPOSER_CHARS sets
                // how many.
                self.open_channel(general);
                let count: usize = std::env::var("KUBECORD_COMPOSER_CHARS")
                    .ok()
                    .and_then(|count| count.parse().ok())
                    .unwrap_or(70);
                let text: Vec<char> = std::env::var("KUBECORD_COMPOSER_TEXT")
                    .unwrap_or_else(|_| "sdfsdfklsjdfklsdfkljsdklfjsjdklfjkls sdlkfjskldfjklsdlkfjklsd sldkfjsdlkjfklsjdfjkls".to_owned())
                    .chars()
                    .cycle()
                    .take(count)
                    .collect();
                let app = self.clone();
                let typed = std::cell::Cell::new(0);
                glib::timeout_add_local(Duration::from_millis(30), move || {
                    if typed.get() < text.len() {
                        let view = app.composer.upcast_ref::<gtk::Widget>();
                        let (needed, ..) = view.measure(gtk::Orientation::Vertical, view.width());
                        let buffer = app.composer.buffer();
                        let end = app.composer.iter_location(&buffer.end_iter());
                        eprintln!(
                            "demo: after {} chars the composer is {} tall, measures {needed}, last char at y {} h {}",
                            typed.get(),
                            view.height(),
                            end.y(),
                            end.height()
                        );
                        app.composer.emit_insert_at_cursor(&text[typed.get()].to_string());
                        typed.set(typed.get() + 1);
                        return glib::ControlFlow::Continue;
                    }
                    let view = app.composer.upcast_ref::<gtk::Widget>();
                    let (natural, ..) = view.measure(gtk::Orientation::Vertical, view.width());
                    let scroll = view.parent().and_downcast::<gtk::ScrolledWindow>().map(|scroll| {
                        let adjustment = scroll.vadjustment();
                        (scroll.height(), adjustment.value(), adjustment.upper(), adjustment.page_size())
                    });
                    eprintln!(
                        "demo: composer is {} tall, its text needs {natural}; scroll (height, value, upper, page) {scroll:?}",
                        view.height()
                    );
                    let mut widget = view.clone();
                    while let Some(parent) = widget.parent() {
                        let bounds = widget.compute_bounds(&parent);
                        eprintln!(
                            "demo:   {} in {} at y {:?} height {:?} (parent {} tall)",
                            widget.type_(),
                            parent.type_(),
                            bounds.map(|b| b.y()),
                            bounds.map(|b| b.height()),
                            parent.height()
                        );
                        if parent.has_css_class("kc-composer") {
                            break;
                        }
                        widget = parent;
                    }
                    glib::ControlFlow::Break
                });
            }
            "composer-type" => {
                // Types KUBECORD_COMPOSER_SCRIPT a key at a time, the way the
                // text view's own key handling does (insert at the cursor,
                // then scroll the cursor on screen; "\n" is Shift+Enter,
                // "\b" Backspace, "\p" a pasted paragraph of several lines
                // and "\c" clears the box, as sending does), every
                // KUBECORD_TYPE_MS milliseconds.
                // Before each key and for a while after the last, it reports
                // where the text sits in the box: the gap above its first
                // line and the gap below its last. They must be equal.
                if std::env::var_os("KUBECORD_NO_OVERLAY_SCROLL").is_some()
                    && let Some(settings) = gtk::Settings::default()
                {
                    settings.set_gtk_overlay_scrolling(false);
                }
                self.open_channel(general);
                let script: Vec<char> = std::env::var("KUBECORD_COMPOSER_SCRIPT")
                    .unwrap_or_else(|_| "hello there\nsecond line".to_owned())
                    .replace("\\n", "\n")
                    .replace("\\b", "\u{8}")
                    .replace("\\p", "\u{1}")
                    .replace("\\c", "\u{2}")
                    .chars()
                    .collect();
                let pause: u64 = std::env::var("KUBECORD_TYPE_MS")
                    .ok()
                    .and_then(|ms| ms.parse().ok())
                    .unwrap_or(60);
                let app = self.clone();
                let typed = std::cell::Cell::new(0usize);
                let after = std::cell::Cell::new(0u32);
                let worst = std::cell::Cell::new(0.0f64);
                glib::timeout_add_local(Duration::from_millis(pause), move || {
                    let (top, bottom, detail) = app.composer_gaps();
                    let index = typed.get();
                    // Text taller than the box scrolls: then the cursor must
                    // be in sight and the scroll range current instead.
                    let view = &app.composer;
                    let buffer = view.buffer();
                    let end = view.iter_location(&buffer.end_iter());
                    let scrolls = end.y() + end.height() + view.top_margin() + view.bottom_margin() > 220;
                    let (off, verdict) = if scrolls {
                        let cursor = view.iter_location(&buffer.iter_at_mark(&buffer.get_insert()));
                        let visible = view.visible_rect();
                        let seen = cursor.y() >= visible.y()
                            && cursor.y() + cursor.height() <= visible.y() + visible.height();
                        let current = view
                            .vadjustment()
                            .is_some_and(|adjustment| adjustment.page_size() == f64::from(view.height()));
                        let at_end = cursor.y() == end.y();
                        match (seen, current) {
                            (true, true) if at_end && (bottom - 16.0).abs() > 1.0 => (97.0, "END-NOT-AT-BOTTOM"),
                            (true, true) => (0.0, "scrolling"),
                            (false, _) => (99.0, "CURSOR-HIDDEN"),
                            (true, false) => (98.0, "STALE-RANGE"),
                        }
                    } else {
                        // This ruler rounds each line's height on its own,
                        // so it can be a pixel out for wrapped text.
                        let off = (top - bottom).abs();
                        (off, if off > 1.0 { "OFF-CENTRE" } else { "centred" })
                    };
                    if off > 1.0 || std::env::var_os("KUBECORD_TYPE_VERBOSE").is_some() {
                        eprintln!("demo: {verdict} after {index} keys: gap above {top:.1}, below {bottom:.1} | {detail}");
                    }
                    worst.set(worst.get().max(off));
                    if index < script.len() {
                        let view = &app.composer;
                        let buffer = view.buffer();
                        match script[index] {
                            '\u{8}' => view.emit_backspace(),
                            '\u{1}' => {
                                buffer.begin_user_action();
                                buffer.insert_interactive_at_cursor(
                                    "a pasted paragraph that is long enough to wrap around once or twice in the box\nits second line\n\nand a fourth",
                                    true,
                                );
                                buffer.end_user_action();
                                view.scroll_mark_onscreen(&buffer.get_insert());
                            }
                            '\u{2}' => buffer.set_text(""),
                            key => {
                                buffer.begin_user_action();
                                buffer.insert_interactive_at_cursor(&key.to_string(), true);
                                buffer.end_user_action();
                                view.scroll_mark_onscreen(&buffer.get_insert());
                            }
                        }
                        typed.set(index + 1);
                        return glib::ControlFlow::Continue;
                    }
                    after.set(after.get() + 1);
                    if after.get() < 40 {
                        return glib::ControlFlow::Continue;
                    }
                    eprintln!("demo: typing done; worst gap difference {:.1}; finally above {top:.1}, below {bottom:.1} | {detail}", worst.get());
                    if let Some(application) = app.window.application() {
                        application.quit();
                    }
                    glib::ControlFlow::Break
                });
            }
            "drafts" => {
                // A draft per channel: typed in #general, then #topic-00,
                // then back. KUBECORD_DRAFTS_CHECK only opens #general (a
                // restart: its draft should be there).
                self.open_channel(general);
                let topic = channel(602);
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(800), move || {
                    if std::env::var_os("KUBECORD_DRAFTS_CHECK").is_some() {
                        eprintln!("demo: after a restart, #general's box: {:?}", app.composer_text());
                        return;
                    }
                    app.set_composer_text("half a thought for general");
                    let Some(topic) = topic else { return };
                    let general = app.with_session(|session| session.store.read(|state| state.channels.get(&Snowflake::new(GENERAL)).cloned())).flatten();
                    app.open_channel(topic);
                    eprintln!("demo: in #topic-00 the box: {:?}", app.composer_text());
                    app.set_composer_text("something else for topic");
                    if let Some(general) = general {
                        app.open_channel(general);
                    }
                    eprintln!("demo: back in #general the box: {:?}", app.composer_text());
                });
            }
            "long-message" => {
                // A draft near the limit, then one far over it (the counter
                // shows the parts), then sent: offline, the first part fails
                // and every part's text goes back into the message box.
                self.open_channel(general);
                let near = "word ".repeat(380);
                self.set_composer_text(near.trim());
                eprintln!("demo: near the limit: {:?} visible={}", self.length_label.text(), self.length_label.is_visible());
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(800), move || {
                    let long = format!("{}\n\n{}", "first paragraph ".repeat(150), "second paragraph ".repeat(150));
                    app.set_composer_text(&long);
                    eprintln!("demo: over the limit: {:?} ({} characters)", app.length_label.text(), long.chars().count());
                    eprintln!("demo: parts: {:?}", crate::composer::split_message(long.trim(), 2000).iter().map(|part| part.chars().count()).collect::<Vec<_>>());
                    if std::env::var_os("KUBECORD_LONG_SEND").is_some() {
                        app.send_current();
                        let app = app.clone();
                        glib::timeout_add_local_once(Duration::from_millis(2500), move || {
                            eprintln!("demo: after sending, the box holds {} characters", app.composer_text().chars().count());
                        });
                    }
                });
            }
            "gif-sent" => {
                // A GIF link sent from the picker, as it arrives: the bare
                // link first, then Discord's update adding the GIF's embed. It
                // must start playing at the bottom without a channel switch.
                self.open_channel(general);
                let id = id_at(0, 58);
                let full = gif_link_message(id, ME);
                let mut bare = full.clone();
                bare["embeds"] = json!([]);
                let store = store.clone();
                glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                    store.apply("MESSAGE_CREATE", &bare);
                    eprintln!("demo: gif sent");
                    glib::timeout_add_local_once(Duration::from_millis(400), move || {
                        store.apply("MESSAGE_UPDATE", &full);
                        eprintln!("demo: embed arrived");
                    });
                });
            }
            "gif-many" => {
                // Six GIF messages, more than fit: only those in view play,
                // and scrolling up starts the ones above.
                for index in 0..6 {
                    store.apply("MESSAGE_CREATE", &gif_message(id_at(0, 41 + index), ADA));
                }
                self.open_channel(general);
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(4000), move || {
                    let adjustment = app.messages_scroll.vadjustment();
                    eprintln!("demo: scrolling up from {}", adjustment.value());
                    adjustment.set_value(adjustment.value() - 1200.0);
                });
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
                // KUBECORD_VIEWER_URL / _ZOOM pick another picture and zoom
                // (a tall one shows whether zooming keeps its shape).
                let url: &'static str = std::env::var("KUBECORD_VIEWER_URL")
                    .ok()
                    .map(|url| &*Box::leak(url.into_boxed_str()))
                    .unwrap_or("https://cdn.discordapp.com/embed/avatars/2.png?size=1024");
                let zoom = std::env::var("KUBECORD_VIEWER_ZOOM")
                    .ok()
                    .and_then(|zoom| zoom.parse::<f64>().ok())
                    .unwrap_or(4.0);
                self.show_image(url, url);
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(2500), move || {
                    app.demo_zoom_viewer(url, zoom);
                    let Ok(path) = std::env::var("KUBECORD_SCREENSHOT") else {
                        return;
                    };
                    let app = app.clone();
                    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                        if let Some(window) = app.window_of(&format!("image:{url}")) {
                            eprintln!("demo: viewer modal={}", window.is_modal());
                            // KUBECORD_MEDIA_MENU: the right-click menu over
                            // the picture (saved as `<screenshot>.menu.png`),
                            // and its Save (with KUBECORD_SAVE_TO).
                            if std::env::var_os("KUBECORD_MEDIA_MENU").is_some() {
                                app.demo_media_menu(window.upcast_ref(), url);
                            }
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
                                // The overlay holding the row and its ×.
                                if let Some(row) = close.parent() {
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
            "status" => {
                // Custom statuses: yours with a Unicode emoji, Ada's with a
                // server emoji (an image, which must show), then Edit Profile.
                store.write(|state| {
                    state.custom_status = Some(kc_state::CustomStatus {
                        text: Some("Shipping kubecord".to_owned()),
                        emoji_name: Some("🚀".to_owned()),
                        emoji_id: None,
                    });
                });
                store.apply(
                    "PRESENCE_UPDATE",
                    &json!({
                        "user": {"id": ADA.to_string()}, "guild_id": GUILD.to_string(),
                        "status": "online",
                        "activities": [{"type": 4, "name": "Custom Status", "state": "in the lab",
                            "emoji": {"id": "236622542490566658", "name": "tanooki", "animated": false}}],
                    }),
                );
                self.open_channel(general);
                self.render_user_bar();
                self.render_members();
                let app = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                    app.edit_profile_dialog();
                });
            }
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
