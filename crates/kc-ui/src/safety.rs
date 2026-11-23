//! Safety limits: a check before the actions Discord's anti-spam watches
//! most closely (opening new DMs, sending friend requests, starting groups,
//! joining servers, and the same message sent to several channels).
//!
//! Nobody outside Discord knows its thresholds, so the limits sit where a
//! person using Discord normally rarely reaches them. Past the soft limit
//! the action asks first; past the hard limit it is refused until enough
//! time has passed. Counts are per Discord account over rolling windows and
//! are kept on disk, so restarting does not reset them. The limits can be
//! switched off in Settings → Advanced, behind a warning.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use libadwaita as adw;
use libadwaita::prelude::*;
use serde::{Deserialize, Serialize};

use kc_core::Snowflake;

use crate::shell::AppState;

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
/// Joining a server this soon after logging in asks first: a fresh login
/// followed by joins is how a taken-over account behaves.
const AFTER_LOGIN: Duration = Duration::from_secs(5 * 60);
/// The same message to this many channels within [`REPEAT_WINDOW`] asks
/// first.
const REPEAT_CHANNELS: usize = 3;
const REPEAT_WINDOW: u64 = 5 * MINUTE;
/// Messages shorter than this are not compared ("ok", "lol").
const REPEAT_MIN_CHARS: usize = 12;

/// An action the limits count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    /// Opening a DM with someone there was no DM with yet.
    NewDm,
    FriendRequest,
    GroupDm,
    JoinServer,
}

impl Action {
    /// Asks first at this many within the window (`None`: never asks).
    fn warn(self) -> Option<(usize, u64)> {
        match self {
            Action::NewDm | Action::FriendRequest => Some((3, 10 * MINUTE)),
            Action::JoinServer => Some((2, 10 * MINUTE)),
            Action::GroupDm => None,
        }
    }

    /// At most this many within each window.
    fn caps(self) -> &'static [(usize, u64)] {
        match self {
            Action::NewDm => &[(5, HOUR), (10, DAY)],
            Action::FriendRequest => &[(5, HOUR), (15, DAY)],
            Action::GroupDm => &[(2, HOUR)],
            Action::JoinServer => &[(3, HOUR), (10, DAY)],
        }
    }

    /// The action in a sentence's words.
    fn noun(self, count: usize) -> String {
        let (one, many) = match self {
            Action::NewDm => ("new DM", "new DMs"),
            Action::FriendRequest => ("friend request", "friend requests"),
            Action::GroupDm => ("new group", "new groups"),
            Action::JoinServer => ("server joined", "servers joined"),
        };
        format!("{count} {}", if count == 1 { one } else { many })
    }

    /// What doing it again means, for a dialog's heading.
    fn verb(self) -> &'static str {
        match self {
            Action::NewDm => "Open Another New DM?",
            Action::FriendRequest => "Send Another Friend Request?",
            Action::GroupDm => "Start Another Group?",
            Action::JoinServer => "Join Another Server?",
        }
    }
}

/// What the limits say about doing an action now.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Allow,
    /// Allowed after asking; the reason why it asks.
    Warn(String),
    /// Refused; the reason, with when it is allowed again.
    Block(String),
}

/// One account's history, in Unix seconds.
#[derive(Default, Serialize, Deserialize)]
struct History {
    #[serde(default)]
    actions: HashMap<Action, Vec<u64>>,
    /// Messages sent: (hash of the text, channel, when).
    #[serde(default)]
    messages: Vec<(u64, Snowflake, u64)>,
}

impl History {
    /// Forgets what no limit looks back far enough to count.
    fn prune(&mut self, now: u64) {
        for times in self.actions.values_mut() {
            times.retain(|time| now.saturating_sub(*time) < DAY);
        }
        self.actions.retain(|_, times| !times.is_empty());
        self.messages.retain(|(_, _, time)| now.saturating_sub(*time) < REPEAT_WINDOW);
    }

    fn verdict(&self, action: Action, now: u64, logged_in: Option<Duration>) -> Verdict {
        let times = self.actions.get(&action).map(Vec::as_slice).unwrap_or_default();
        let within = |window: u64| times.iter().filter(|time| now.saturating_sub(**time) < window).count();
        for &(cap, window) in action.caps() {
            let done = within(window);
            if done >= cap {
                // Allowed again when the oldest action in the window leaves it.
                let mut recent: Vec<u64> =
                    times.iter().copied().filter(|time| now.saturating_sub(*time) < window).collect();
                recent.sort_unstable();
                let free_at = recent[done - cap] + window;
                return Verdict::Block(format!(
                    "You've reached the limit of {} in {}. To keep your account clear of Discord's spam filter, \
                     Kubecord won't do more until {}.",
                    action.noun(cap),
                    span(window),
                    wait(free_at.saturating_sub(now)),
                ));
            }
        }
        if let Some((count, window)) = action.warn() {
            let done = within(window);
            if done + 1 >= count {
                return Verdict::Warn(format!(
                    "You've had {} in the last {}. Discord's spam filter bans accounts that do this quickly, \
                     especially with people you don't know.",
                    action.noun(done),
                    span(window),
                ));
            }
        }
        if action == Action::JoinServer
            && let Some(since) = logged_in
            && since < AFTER_LOGIN
        {
            return Verdict::Warn(
                "You logged in moments ago. Joining servers right after logging in is how a stolen account \
                 behaves, and Discord may lock the account to check."
                    .to_owned(),
            );
        }
        Verdict::Allow
    }

    /// Whether sending `text` to `channel` repeats it into a third channel.
    fn repeats(&self, text: &str, channel: Snowflake, now: u64) -> bool {
        if text.trim().chars().count() < REPEAT_MIN_CHARS {
            return false;
        }
        let hash = text_hash(text);
        let mut channels: Vec<Snowflake> = self
            .messages
            .iter()
            .filter(|(sent, _, time)| *sent == hash && now.saturating_sub(*time) < REPEAT_WINDOW)
            .map(|(_, channel, _)| *channel)
            .collect();
        channels.push(channel);
        channels.sort_unstable();
        channels.dedup();
        channels.len() >= REPEAT_CHANNELS
    }
}

/// "an hour", "10 minutes", "a day".
fn span(seconds: u64) -> String {
    match seconds {
        DAY => "a day".to_owned(),
        HOUR => "an hour".to_owned(),
        seconds => format!("{} minutes", seconds / MINUTE),
    }
}

/// How long until, in words: "in 12 minutes", "in about 3 hours".
fn wait(seconds: u64) -> String {
    let minutes = seconds.div_ceil(MINUTE).max(1);
    if minutes < 90 {
        format!("{minutes} minute{} from now", if minutes == 1 { "" } else { "s" })
    } else {
        format!("about {} hours from now", minutes.div_ceil(60))
    }
}

fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.trim().hash(&mut hasher);
    hasher.finish()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

/// Each account's history (by user ID), read from disk on first use.
#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    accounts: HashMap<Snowflake, History>,
    /// When each account last logged in (this run only).
    #[serde(skip)]
    logins: HashMap<Snowflake, Instant>,
}

thread_local! {
    static STORE: RefCell<Option<Store>> = const { RefCell::new(None) };
}

fn path() -> Option<std::path::PathBuf> {
    kc_core::paths::data_dir().ok().map(|dir| dir.join("safety.json"))
}

fn with_store<R>(change: impl FnOnce(&mut Store) -> R) -> R {
    STORE.with_borrow_mut(|store| {
        let store = store.get_or_insert_with(|| {
            path()
                .and_then(|path| std::fs::read(path).ok())
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default()
        });
        change(store)
    })
}

fn save() {
    let Some(path) = path() else { return };
    let bytes = with_store(|store| {
        let now = now();
        for history in store.accounts.values_mut() {
            history.prune(now);
        }
        serde_json::to_vec(store).ok()
    });
    if let Some(bytes) = bytes {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(error) = std::fs::write(&path, bytes) {
            tracing::warn!(%error, "could not save the safety limits' history");
        }
    }
}

/// Notes that `user` just logged in (the gateway's READY).
pub(crate) fn logged_in(user: Snowflake) {
    with_store(|store| store.logins.insert(user, Instant::now()));
}

fn verdict(user: Snowflake, action: Action) -> Verdict {
    with_store(|store| {
        let since = store.logins.get(&user).map(Instant::elapsed);
        store.accounts.get(&user).map_or_else(
            || History::default().verdict(action, now(), since),
            |history| history.verdict(action, now(), since),
        )
    })
}

fn record(user: Snowflake, action: Action) {
    with_store(|store| store.accounts.entry(user).or_default().actions.entry(action).or_default().push(now()));
    save();
}

/// Notes a message sent, for the repeated-message check.
pub(crate) fn record_message(user: Snowflake, channel: Snowflake, text: &str) {
    if text.trim().chars().count() < REPEAT_MIN_CHARS {
        return;
    }
    with_store(|store| {
        let history = store.accounts.entry(user).or_default();
        history.prune(now());
        history.messages.push((text_hash(text), channel, now()));
    });
    save();
}

impl AppState {
    /// The signed-in account's user ID.
    fn safety_user(&self) -> Option<Snowflake> {
        let (_, _, store) = self.active()?;
        store.read(|state| state.me.as_ref().map(|me| me.id))
    }

    /// Whether the limits are on.
    pub(crate) fn safety_on(&self) -> bool {
        self.capture.borrow().safety_limits
    }

    /// Does `proceed` if the limits allow `action`: at once, after asking,
    /// or not at all (saying when it will be allowed).
    pub(crate) fn guard(self: &Rc<Self>, action: Action, proceed: impl FnOnce(&Rc<Self>) + 'static) {
        let Some(user) = self.safety_user() else {
            proceed(self);
            return;
        };
        let verdict = if self.safety_on() { verdict(user, action) } else { Verdict::Allow };
        match verdict {
            Verdict::Allow => {
                record(user, action);
                proceed(self);
            }
            Verdict::Warn(reason) => self.ask_anyway(action.verb(), &reason, move |app| {
                record(user, action);
                proceed(app);
            }),
            Verdict::Block(reason) => {
                // Discord's lists of thresholds are not public; these are on
                // the safe side and can be switched off in Settings.
                let dialog = adw::AlertDialog::new(Some("Limit Reached"), Some(&reason));
                dialog.add_response("ok", "OK");
                dialog.set_default_response(Some("ok"));
                dialog.set_close_response("ok");
                dialog.present(Some(&self.window));
            }
        }
    }

    /// Whether sending `text` to `channel` repeats a message just sent to
    /// other channels (and the limits are on).
    pub(crate) fn would_repeat(&self, channel: Snowflake, text: &str) -> bool {
        self.safety_on()
            && self.safety_user().is_some_and(|user| {
                with_store(|store| {
                    store.accounts.get(&user).is_some_and(|history| history.repeats(text, channel, now()))
                })
            })
    }

    /// Notes a message sent to `channel`, for [`Self::would_repeat`].
    pub(crate) fn note_sent(&self, channel: Snowflake, text: &str) {
        if let Some(user) = self.safety_user() {
            record_message(user, channel, text);
        }
    }

    /// Asks before sending a message repeated across channels.
    pub(crate) fn ask_repeat(self: &Rc<Self>, send: impl FnOnce(&Rc<Self>) + 'static) {
        self.ask_anyway(
            "Send the Same Message Again?",
            "You've sent this message to other channels in the last few minutes. The same text posted to \
             several channels is what spam looks like to Discord.",
            send,
        );
    }

    /// Asks whether to go ahead despite `reason`; Cancel is the default.
    fn ask_anyway(self: &Rc<Self>, heading: &str, reason: &str, proceed: impl FnOnce(&Rc<Self>) + 'static) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(reason));
        dialog.add_responses(&[("cancel", "Cancel"), ("anyway", "Do It Anyway")]);
        dialog.set_response_appearance("anyway", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let app = self.clone();
        let proceed = RefCell::new(Some(proceed));
        dialog.connect_response(None, move |_, response| {
            if response == "anyway"
                && let Some(proceed) = proceed.take()
            {
                proceed(&app);
            }
        });
        dialog.present(Some(&self.window));
    }
}

/// The demo: notes `action` as done `age` seconds ago.
pub(crate) fn demo_record(app: &AppState, action: Action, age: u64) {
    if let Some(user) = app.safety_user() {
        with_store(|store| {
            store.accounts.entry(user).or_default().actions.entry(action).or_default().push(now() - age);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(action: Action, ages: &[u64], now: u64) -> History {
        let mut history = History::default();
        history.actions.insert(action, ages.iter().map(|age| now - age).collect());
        history
    }

    const NOW: u64 = 10 * DAY;

    #[test]
    fn new_dms_ask_at_the_third_in_ten_minutes_and_stop_after_five_an_hour() {
        let allow = history(Action::NewDm, &[60], NOW);
        assert_eq!(allow.verdict(Action::NewDm, NOW, None), Verdict::Allow);
        let warn = history(Action::NewDm, &[60, 120], NOW);
        assert!(matches!(warn.verdict(Action::NewDm, NOW, None), Verdict::Warn(_)));
        // Spread out over the hour, so no warning, but five is the cap.
        let block = history(Action::NewDm, &[11 * MINUTE, 20 * MINUTE, 30 * MINUTE, 40 * MINUTE, 50 * MINUTE], NOW);
        let Verdict::Block(reason) = block.verdict(Action::NewDm, NOW, None) else {
            panic!("expected a block");
        };
        // The oldest (50 minutes ago) leaves the hour in 10 minutes.
        assert!(reason.contains("10 minutes from now"), "{reason}");
        // An hour later they no longer count.
        let old = history(Action::NewDm, &[HOUR + 1, HOUR + 2, HOUR + 3, HOUR + 4, HOUR + 5], NOW);
        assert_eq!(old.verdict(Action::NewDm, NOW, None), Verdict::Allow);
    }

    #[test]
    fn the_daily_cap_holds_when_the_hourly_one_does_not() {
        let ages: Vec<u64> = (1..=10).map(|hour| hour * HOUR).collect();
        let day = history(Action::NewDm, &ages, NOW);
        assert!(matches!(day.verdict(Action::NewDm, NOW, None), Verdict::Block(_)));
        // Other actions keep their own counts.
        assert_eq!(day.verdict(Action::FriendRequest, NOW, None), Verdict::Allow);
    }

    #[test]
    fn joining_right_after_logging_in_asks() {
        let empty = History::default();
        assert!(matches!(
            empty.verdict(Action::JoinServer, NOW, Some(Duration::from_secs(30))),
            Verdict::Warn(_)
        ));
        assert_eq!(empty.verdict(Action::JoinServer, NOW, Some(Duration::from_secs(600))), Verdict::Allow);
    }

    #[test]
    fn the_same_message_into_a_third_channel_is_a_repeat() {
        let text = "check out my new server everyone";
        let mut history = History::default();
        let hash = text_hash(text);
        history.messages.push((hash, Snowflake::new(1), NOW - 60));
        assert!(!history.repeats(text, Snowflake::new(2), NOW));
        history.messages.push((hash, Snowflake::new(2), NOW - 30));
        assert!(history.repeats(text, Snowflake::new(3), NOW));
        // Again in a channel it already went to is not a new channel.
        assert!(!history.repeats(text, Snowflake::new(2), NOW));
        // Short messages and old ones do not count.
        assert!(!history.repeats("ok", Snowflake::new(3), NOW));
        assert!(!history.repeats(text, Snowflake::new(3), NOW + REPEAT_WINDOW));
    }

    #[test]
    fn the_history_round_trips_through_its_file() {
        let mut store = Store::default();
        store.accounts.insert(Snowflake::new(42), history(Action::NewDm, &[60], NOW));
        let bytes = serde_json::to_vec(&store).unwrap();
        let back: Store = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.accounts[&Snowflake::new(42)].actions[&Action::NewDm], vec![NOW - 60]);
    }

    #[test]
    fn old_entries_are_pruned() {
        let mut history = history(Action::JoinServer, &[60, DAY + 60], NOW);
        history.messages.push((1, Snowflake::new(1), NOW - REPEAT_WINDOW - 1));
        history.prune(NOW);
        assert_eq!(history.actions[&Action::JoinServer].len(), 1);
        assert!(history.messages.is_empty());
    }
}
