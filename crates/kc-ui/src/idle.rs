//! Going idle when away (the `auto_idle` setting, off by default).
//!
//! Discord's own clients show the account as Idle after ten minutes away,
//! and come back to Online when the user does. From a capture of the web
//! client: it sends its presence as idle twice (`afk` false, then true, with
//! `since` the time it went idle), and stops its server subscriptions
//! (typing, activities, member lists) until they are needed again. Only an
//! Online account goes idle; Do Not Disturb and Invisible stay as they are,
//! and the saved status is never changed.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use kc_core::Snowflake;

use crate::session::AccountId;
use crate::shell::AppState;

/// How long without a focused window counts as away.
const AWAY_AFTER: Duration = Duration::from_secs(10 * 60);

thread_local! {
    /// The countdown to going idle, while away.
    static AWAY_TIMER: RefCell<Option<glib::SourceId>> = const { RefCell::new(None) };
    /// The accounts this made idle, to bring back.
    static IDLED: RefCell<HashSet<AccountId>> = RefCell::new(HashSet::new());
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn custom(status: &Option<kc_state::CustomStatus>) -> Option<(&str, Option<&str>, Option<Snowflake>)> {
    status.as_ref().map(|custom| {
        (
            custom.text.as_deref().unwrap_or(""),
            custom.emoji_name.as_deref(),
            custom.emoji_id,
        )
    })
}

impl AppState {
    /// Called when a window gains or loses the focus, or the setting
    /// changes: starts the countdown to idle, or comes back from it.
    pub(crate) fn away_changed(self: &Rc<Self>) {
        let away = !crate::animation::window_focused() && self.capture.borrow().auto_idle;
        if !away {
            if let Some(timer) = AWAY_TIMER.with_borrow_mut(Option::take) {
                timer.remove();
            }
            self.come_back();
            return;
        }
        let counting = AWAY_TIMER.with_borrow(Option::is_some);
        let idle = IDLED.with_borrow(|idled| !idled.is_empty());
        if counting || idle {
            return;
        }
        let app = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(AWAY_AFTER, move || {
            AWAY_TIMER.with_borrow_mut(Option::take);
            if let Some(app) = app.upgrade() {
                app.go_idle();
            }
        });
        AWAY_TIMER.with_borrow_mut(|slot| *slot = Some(timer));
    }

    fn go_idle(&self) {
        let since = now_ms();
        let accounts: Vec<AccountId> = self.model.borrow().sessions.keys().copied().collect();
        for account in accounts {
            let Some(gateway) = crate::session::gateway(account) else {
                continue;
            };
            let went = self.with_account_session_mut(account, |session| {
                let (status, custom_status) =
                    session.store.read(|state| (state.status.clone(), state.custom_status.clone()));
                if status.as_deref() != Some("online") {
                    return false;
                }
                let custom = custom(&custom_status);
                gateway.update_presence_since("idle", custom, since, false);
                gateway.update_presence_since("idle", custom, since, true);
                let guilds: HashSet<Snowflake> = session.store.read(|state| {
                    session
                        .subscribed
                        .iter()
                        .filter_map(|channel| state.channels.get(channel)?.guild_id)
                        .collect()
                });
                gateway.unsubscribe_guilds(&guilds.into_iter().collect::<Vec<_>>());
                // Opening a channel subscribes it again.
                session.subscribed.clear();
                true
            });
            if went == Some(true) {
                IDLED.with_borrow_mut(|idled| idled.insert(account));
            }
        }
    }

    fn come_back(&self) {
        let idled: Vec<AccountId> = IDLED.with_borrow_mut(|idled| idled.drain().collect());
        for account in idled {
            let Some(gateway) = crate::session::gateway(account) else {
                continue;
            };
            self.with_account_session_mut(account, |session| {
                let (status, custom_status) =
                    session.store.read(|state| (state.status.clone(), state.custom_status.clone()));
                let status = status.unwrap_or_else(|| "online".to_owned());
                gateway.update_presence(&status, custom(&custom_status), false);
                // The open channel's subscription comes back now; others
                // when they are opened.
                if let Some(channel) = session.selected_channel
                    && let Some(guild) = session.store.read(|state| state.channels.get(&channel)?.guild_id)
                {
                    gateway.subscribe_channel(guild, channel);
                    session.subscribed.insert(channel);
                }
            });
        }
    }
}
