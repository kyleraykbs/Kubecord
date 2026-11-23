//! Desktop notifications for new messages and incoming calls.
//!
//! Whether a message notifies is decided by [`kc_state::notify::decide`]
//! (Discord's server and channel settings, mutes, Do Not Disturb, blocks,
//! the channel you are looking at) plus the choices on the Notifications
//! page. Notifications go through the `GtkApplication` (`gio::Notification`);
//! clicking one runs the `app.open-message` action, which brings the window
//! up and opens the channel at that message.

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use kc_core::Snowflake;
use kc_state::notify::{NotifyReason, decide};
use kc_state::{Message, State};

use crate::session::AccountId;
use crate::shell::AppState;
use crate::widgets::snippet;

/// The application action a notification runs when clicked.
pub(crate) const OPEN_ACTION: &str = "open-message";
/// Longest message text shown in a notification.
const PREVIEW_CHARS: usize = 200;

/// What is showing on the desktop.
#[derive(Default)]
pub(crate) struct Shown {
    /// Channels with a notification up, by account.
    channels: HashMap<(AccountId, Snowflake), VecDeque<String>>,
    /// Calls with a notification up.
    calls: HashSet<Snowflake>,
    /// Notifications since the window was last looked at.
    unseen: u32,
    /// Messages that recently notified, so one seen by two signed-in accounts
    /// (both in the same server or group) sounds and shows only once.
    recent: std::collections::VecDeque<Snowflake>,
    /// The window title without the count.
    title: String,
}

/// The window title with the count of unseen notifications in front.
pub(crate) fn window_title(title: &str, unseen: u32) -> String {
    if unseen > 0 {
        format!("({unseen}) {title}")
    } else {
        title.to_owned()
    }
}

/// A notification ready to send.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Prepared {
    pub reason: NotifyReason,
    pub title: String,
    pub body: String,
    pub avatar: Option<String>,
}

/// `account:channel:message`, the click action's target.
pub(crate) fn action_target(account: AccountId, channel: Snowflake, message: Snowflake) -> String {
    format!("{account}:{channel}:{message}")
}

pub(crate) fn parse_target(target: &str) -> Option<(AccountId, Snowflake, Option<Snowflake>)> {
    let mut parts = target.split(':');
    let account = parts.next()?.parse().ok()?;
    let channel = parts.next()?.parse().ok()?;
    let message = parts.next().and_then(|id| id.parse().ok());
    Some((account, channel, message))
}

/// One notification per message: reusing an id makes the daemon replace the
/// notification already up -- silently, with no new popup -- so a second
/// message in a channel would only be heard, never seen.
fn message_id_name(account: AccountId, channel: Snowflake, message: Snowflake) -> String {
    format!("message-{account}-{channel}-{message}")
}

/// Notifications kept up per channel; older ones are taken down.
const PER_CHANNEL: usize = 5;
/// How long a notification waits for the sender's avatar (it shares the media
/// download queue) before it goes without one.
const AVATAR_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

fn call_id_name(channel: Snowflake) -> String {
    format!("call-{channel}")
}

/// The title: who wrote, and where (nothing more for a one-to-one DM).
pub(crate) fn title_for(author: &str, channel: Option<&str>, place: Option<&str>) -> String {
    match (channel, place) {
        (Some(channel), Some(server)) => format!("{author} (#{channel}, {server})"),
        (None, Some(group)) => format!("{author} ({group})"),
        _ => author.to_owned(),
    }
}

/// What the notification says when the text is hidden or empty.
pub(crate) fn body_without_text(reason: NotifyReason, message: &Message, preview: bool) -> String {
    if preview {
        if !message.attachments.is_empty() {
            return "Sent an attachment".to_owned();
        }
        if !message.sticker_items.is_empty() {
            return "Sent a sticker".to_owned();
        }
        if !message.embeds.is_empty() {
            return "Sent a link".to_owned();
        }
    }
    match reason {
        NotifyReason::Mention => "Mentioned you",
        NotifyReason::RoleMention => "Mentioned a role you have",
        NotifyReason::Everyone => "Mentioned everyone",
        NotifyReason::DirectMessage | NotifyReason::Message => "New message",
    }
    .to_owned()
}

/// Builds the notification for a message, if it should notify.
pub(crate) fn prepare(
    state: &State,
    message: &Message,
    viewing: bool,
    prefs: &crate::settings::CapturePreferences,
) -> Option<Prepared> {
    let reason = decide(
        &state.message_facts(message, viewing),
        &prefs.notify_prefs(),
    )?;
    let channel = state.channels.get(&message.channel_id);
    let guild_id = message
        .guild_id
        .or_else(|| channel.and_then(|channel| channel.guild_id));
    let author = state.display_name(guild_id, &message.author);
    let title = match guild_id {
        Some(guild_id) => title_for(
            &author,
            channel.and_then(|channel| channel.name.as_deref()),
            state.guilds.get(&guild_id).map(|guild| guild.name.as_str()),
        ),
        None => {
            let group = channel
                .filter(|channel| {
                    channel
                        .recipients
                        .as_ref()
                        .is_some_and(|recipients| recipients.len() > 1)
                })
                .map(|channel| channel.display_name());
            title_for(&author, None, group.as_deref())
        }
    };
    let text = if prefs.notify_preview {
        let pieces = crate::rich::parse(&message.content);
        let resolver = crate::content::StateResolver {
            state,
            guild: guild_id,
        };
        snippet(
            &crate::rich::to_plain(&pieces, &resolver, false),
            PREVIEW_CHARS,
        )
    } else {
        String::new()
    };
    let body = if text.is_empty() {
        body_without_text(reason, message, prefs.notify_preview)
    } else {
        text
    };
    Some(Prepared {
        reason,
        title,
        body,
        avatar: Some(kc_http::cdn::user_avatar(
            message.author.id,
            message.author.avatar.as_deref(),
            128,
        )),
    })
}

/// Whether notifications are only printed (demo runs): nothing reaches the
/// desktop.
fn dry_run() -> bool {
    std::env::var_os("KUBECORD_DEMO").is_some() || std::env::var_os("KUBECORD_NOTIFY_DRY").is_some()
}

impl AppState {
    /// Registers the click action. Called once, at start.
    pub(crate) fn install_notification_actions(self: &Rc<Self>, app: &impl IsA<gio::ActionMap>) {
        let action = gio::SimpleAction::new(OPEN_ACTION, Some(glib::VariantTy::STRING));
        let state = Rc::downgrade(self);
        action.connect_activate(move |_, parameter| {
            let (Some(state), Some(target)) = (
                state.upgrade(),
                parameter.and_then(|parameter| parameter.get::<String>()),
            ) else {
                return;
            };
            state.open_from_notification(&target);
        });
        app.add_action(&action);
        // Looking at the window clears the count in its title.
        let state = Rc::downgrade(self);
        self.window.connect_is_active_notify(move |window| {
            if window.is_active()
                && let Some(state) = state.upgrade()
            {
                state.notifications.borrow_mut().unseen = 0;
                state.show_unseen();
            }
        });
    }

    /// A message arrived on `account`: notify (and play its sound) if it
    /// should.
    pub(crate) fn message_notification(
        self: &Rc<Self>,
        account: AccountId,
        channel_id: Snowflake,
        message_id: Snowflake,
    ) {
        let prefs = self.capture.borrow().clone();
        // Whether this is the channel on screen. Channel ids are global, so
        // this holds whichever signed-in account the message reached: with two
        // accounts in one server, the background account must not ping you
        // about the channel you are reading with the other.
        let open = self
            .with_session(|session| session.selected_channel == Some(channel_id))
            .unwrap_or(false);
        let viewing = open
            && match prefs.open_channel_quiet {
                crate::settings::OpenChannelQuiet::WhileFocused => self.window.is_active(),
                crate::settings::OpenChannelQuiet::Always => true,
                crate::settings::OpenChannelQuiet::Never => false,
            };
        let prepared = {
            let model = self.model.borrow();
            let Some(session) = model.sessions.get(&account) else {
                return;
            };
            session.store.read(|state| {
                // Message requests wait quietly under Message Requests.
                if state.channels.get(&channel_id).is_some_and(kc_state::Channel::is_request) {
                    return None;
                }
                let message = state.messages.get(&channel_id)?.get(message_id)?;
                prepare(state, message, viewing, &prefs)
            })
        };
        let Some(prepared) = prepared else {
            return;
        };
        {
            let mut shown = self.notifications.borrow_mut();
            if shown.recent.contains(&message_id) {
                return;
            }
            shown.recent.push_back(message_id);
            if shown.recent.len() > 64 {
                shown.recent.pop_front();
            }
        }
        self.notification_sound(prepared.reason.is_mention());
        if !prefs.notify_desktop {
            return;
        }
        let id = message_id_name(account, channel_id, message_id);
        let target = action_target(account, channel_id, message_id);
        let expired = {
            let mut shown = self.notifications.borrow_mut();
            let ids = shown.channels.entry((account, channel_id)).or_default();
            ids.push_back(id.clone());
            (ids.len() > PER_CHANNEL).then(|| ids.pop_front()).flatten()
        };
        if let Some(old) = expired {
            self.withdraw_desktop(&old);
        }
        self.count_unseen();
        let app = self.clone();
        let send = move |icon: Option<std::path::PathBuf>| {
            app.send_desktop(
                &id,
                &prepared.title,
                &prepared.body,
                &target,
                icon,
                prepared.reason.is_mention(),
            );
        };
        // Sent once: with the avatar when it arrives in time, else without.
        let pending: std::rc::Rc<std::cell::RefCell<Option<Box<dyn FnOnce(Option<std::path::PathBuf>)>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Some(Box::new(send))));
        let fire = move |icon: Option<std::path::PathBuf>| {
            let send = pending.borrow_mut().take();
            if let Some(send) = send {
                send(icon);
            }
        };
        match prepared.avatar.clone() {
            Some(url) if !dry_run() => {
                let with_icon = fire.clone();
                self.images.file(&url, "png", with_icon);
                glib::timeout_add_local_once(AVATAR_WAIT, move || fire(None));
            }
            _ => fire(None),
        }
    }

    fn withdraw_desktop(&self, id: &str) {
        if dry_run() {
            return;
        }
        if let Some(application) = self.window.application() {
            application.withdraw_notification(id);
        }
    }

    fn send_desktop(
        &self,
        id: &str,
        title: &str,
        body: &str,
        target: &str,
        icon: Option<std::path::PathBuf>,
        important: bool,
    ) {
        if dry_run() {
            eprintln!("demo: notification id={id} title={title:?} body={body:?} target={target}");
            return;
        }
        let Some(application) = self.window.application() else {
            return;
        };
        let notification = gio::Notification::new(title);
        notification.set_body(Some(body));
        if let Some(path) = icon {
            notification.set_icon(&gio::FileIcon::new(&gio::File::for_path(path)));
        }
        notification.set_priority(if important {
            gio::NotificationPriority::High
        } else {
            gio::NotificationPriority::Normal
        });
        notification.set_default_action_and_target_value(
            &format!("app.{OPEN_ACTION}"),
            Some(&target.to_variant()),
        );
        application.send_notification(Some(id), &notification);
    }

    /// Takes a channel's notifications off the desktop (it was opened).
    pub(crate) fn withdraw_channel_notification(&self, account: AccountId, channel_id: Snowflake) {
        let ids = self
            .notifications
            .borrow_mut()
            .channels
            .remove(&(account, channel_id))
            .unwrap_or_default();
        for id in ids {
            self.withdraw_desktop(&id);
        }
    }

    /// A call is ringing us: a notification, unless the window is in use
    /// or Do Not Disturb is on.
    pub(crate) fn call_notification(self: &Rc<Self>, account: AccountId, channel_id: Snowflake) {
        let prefs = self.capture.borrow().clone();
        if !prefs.notify_desktop || !prefs.notify_calls || self.window.is_active() {
            return;
        }
        let caller = {
            let model = self.model.borrow();
            let Some(session) = model.sessions.get(&account) else {
                return;
            };
            session.store.read(|state| {
                if state.status.as_deref() == Some("dnd") {
                    return None;
                }
                let channel = state.channels.get(&channel_id)?;
                let solo = channel
                    .recipients
                    .as_ref()
                    .filter(|recipients| recipients.len() == 1)
                    .and_then(|recipients| recipients.first());
                Some(match solo {
                    Some(user) => (
                        state.display_name(None, user),
                        Some(kc_http::cdn::user_avatar(
                            user.id,
                            user.avatar.as_deref(),
                            128,
                        )),
                    ),
                    None => (channel.display_name(), None),
                })
            })
        };
        let Some((name, avatar)) = caller else {
            return;
        };
        self.notifications.borrow_mut().calls.insert(channel_id);
        self.count_unseen();
        let id = call_id_name(channel_id);
        let target = format!("{account}:{channel_id}");
        let app = self.clone();
        let send = move |icon: Option<std::path::PathBuf>| {
            app.send_desktop(&id, &name, "Incoming call", &target, icon, true);
        };
        match avatar {
            Some(url) if !dry_run() => self.images.file(&url, "png", send),
            _ => send(None),
        }
    }

    pub(crate) fn withdraw_call_notification(&self, channel_id: Snowflake) {
        if !self.notifications.borrow_mut().calls.remove(&channel_id) || dry_run() {
            return;
        }
        if let Some(application) = self.window.application() {
            application.withdraw_notification(&call_id_name(channel_id));
        }
    }

    /// One more notification the window has not been looked at since.
    fn count_unseen(&self) {
        if self.window.is_active() {
            return;
        }
        self.notifications.borrow_mut().unseen += 1;
        self.show_unseen();
    }

    /// Puts the count of unseen notifications in the window title (most
    /// taskbars show it), when that setting is on.
    fn show_unseen(&self) {
        let (unseen, title) = {
            let shown = self.notifications.borrow();
            let title = if shown.title.is_empty() {
                "kubecord".to_owned()
            } else {
                shown.title.clone()
            };
            (shown.unseen, title)
        };
        let unseen = if self.capture.borrow().notify_mark_window {
            unseen
        } else {
            0
        };
        self.window.set_title(Some(&window_title(&title, unseen)));
    }

    /// Sets the window title (what is open); the count of unseen
    /// notifications stays in front of it.
    pub(crate) fn set_window_title(&self, title: &str) {
        self.notifications.borrow_mut().title = title.to_owned();
        self.show_unseen();
    }

    /// A notification was clicked: bring the window up and open the
    /// channel, at the message when there is one.
    pub(crate) fn open_from_notification(self: &Rc<Self>, target: &str) {
        self.window.present();
        let Some((account, channel_id, message_id)) = parse_target(target) else {
            return;
        };
        if self.model.borrow().active != Some(account) {
            self.switch_account(account);
        }
        let channel = self
            .with_session(|session| {
                session
                    .store
                    .read(|state| state.channels.get(&channel_id).cloned())
            })
            .flatten();
        let Some(channel) = channel else {
            return;
        };
        self.open_channel(channel);
        if let Some(message_id) = message_id {
            // Centre and flash the message itself, not just the channel. If
            // the channel's history is still loading this waits for it, so
            // the first page's render does not snap back to the bottom.
            self.jump_when_ready(channel_id, message_id);
        }
    }

    /// Changes the account's Discord notification settings for a server
    /// (`None`: DMs) and takes Discord's answer into the store at once
    /// (the gateway's update follows). The same settings show in the
    /// official apps.
    pub(crate) fn change_notification_settings(
        self: &Rc<Self>,
        guild_id: Option<Snowflake>,
        payload: serde_json::Value,
        done: &'static str,
    ) {
        let Some((_, http, store)) = self.active() else {
            return;
        };
        self.spawn(
            async move { http.patch_guild_settings(guild_id, payload).await },
            move |app, result| match result {
                Ok(settings) => {
                    if settings.is_object() {
                        store.apply("USER_GUILD_SETTINGS_UPDATE", &settings);
                    }
                    app.set_status(done);
                }
                Err(error) => app.set_status(&format!(
                    "Could not change notification settings: {}",
                    error.describe()
                )),
            },
        );
    }

    /// The test button on the Notifications page.
    pub(crate) fn test_notification(self: &Rc<Self>) {
        self.send_desktop(
            "test",
            "kubecord",
            "Notifications work. Click this to come back to kubecord.",
            "0:0",
            None,
            false,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_round_trip() {
        let target = action_target(42, Snowflake(7), Snowflake(9));
        assert_eq!(
            parse_target(&target),
            Some((42, Snowflake(7), Some(Snowflake(9))))
        );
        assert_eq!(parse_target("42:7"), Some((42, Snowflake(7), None)));
        assert_eq!(parse_target("nonsense"), None);
    }

    #[test]
    fn the_window_title_counts_unseen_notifications() {
        assert_eq!(window_title("general — Demo", 0), "general — Demo");
        assert_eq!(window_title("general — Demo", 3), "(3) general — Demo");
    }

    #[test]
    fn titles_say_where() {
        assert_eq!(title_for("Ada", None, None), "Ada");
        assert_eq!(title_for("Ada", None, Some("Friends")), "Ada (Friends)");
        assert_eq!(
            title_for("Ada", Some("general"), Some("Demo Server")),
            "Ada (#general, Demo Server)"
        );
    }

    fn message(extra: serde_json::Value) -> Message {
        let mut value = serde_json::json!({
            "id": "1", "channel_id": "2", "author": {"id": "3", "username": "ada"},
            "content": ""
        });
        if let (Some(base), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            for (key, item) in extra {
                base.insert(key.clone(), item.clone());
            }
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn bodies_without_text() {
        let file = message(serde_json::json!({"attachments": [
            {"id": "5", "filename": "a.png", "size": 1, "url": "u", "proxy_url": "p"}
        ]}));
        assert_eq!(
            body_without_text(NotifyReason::Message, &file, true),
            "Sent an attachment"
        );
        // With previews off, nothing about the content is told.
        assert_eq!(
            body_without_text(NotifyReason::Mention, &file, false),
            "Mentioned you"
        );
        assert_eq!(
            body_without_text(
                NotifyReason::DirectMessage,
                &message(serde_json::json!({})),
                true
            ),
            "New message"
        );
    }

    #[test]
    fn prepared_from_the_store() {
        let mut state = State::default();
        state.apply(
            "READY",
            &serde_json::json!({
                "user": {"id": "1", "username": "me"},
                "users": [{"id": "3", "username": "ada", "global_name": "Ada"}],
                "guilds": [{"id": "500", "name": "Demo Server",
                    "roles": [{"id": "500", "name": "@everyone", "permissions": "0", "position": 0}],
                    "channels": [{"id": "2", "type": 0, "name": "general"}]}],
                "merged_members": [[{"user_id": "1", "roles": []}]],
                "private_channels": [], "relationships": [],
                "read_state": {"entries": []}, "user_guild_settings": {"entries": []},
                "user_settings": {"status": "online"}
            }),
        );
        let mention = message(serde_json::json!({
            "content": "hey <@1> look", "author": {"id": "3", "username": "ada", "global_name": "Ada"},
            "mentions": [{"id": "1", "username": "me"}]
        }));
        let mut prefs = crate::settings::CapturePreferences::default();
        let prepared = prepare(&state, &mention, false, &prefs).unwrap();
        assert_eq!(prepared.reason, NotifyReason::Mention);
        assert_eq!(prepared.title, "Ada (#general, Demo Server)");
        assert_eq!(prepared.body, "hey @me look");
        prefs.notify_preview = false;
        assert_eq!(
            prepare(&state, &mention, false, &prefs).unwrap().body,
            "Mentioned you"
        );
        // Looking at the channel: nothing.
        assert_eq!(prepare(&state, &mention, true, &prefs), None);
    }
}
