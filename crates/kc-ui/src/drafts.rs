//! Drafts: what was typed in each channel's message box, kept per account
//! and channel, so switching away and back finds it again, and on disk
//! (`$XDG_DATA_HOME/kubecord/drafts.json`, readable only by you), so a
//! restart or a crash loses nothing.
//!
//! The message box shows one channel's draft at a time: [`AppState::sync_draft`]
//! notices when the account or channel changed (it runs whenever the box is
//! redrawn), files the text under the old one and brings up the new one's.
//! Editing a message borrows the box: the draft is set aside and comes back
//! when the edit is sent or cancelled.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use kc_core::Snowflake;
use serde::{Deserialize, Serialize};

use crate::shell::AppState;

/// One channel's draft: the text, and the mentions and emoji in it with what
/// they stand for (`@Name` → `<@123>`), so they still send as mentions.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    pub text: String,
    #[serde(default)]
    pub tokens: Vec<(String, String)>,
}

/// Drafts by account (its user ID) and channel.
type Drafts = BTreeMap<String, BTreeMap<String, Draft>>;

/// Whose draft the message box shows: account and channel.
type Owner = (String, Snowflake);

thread_local! {
    static DRAFTS: RefCell<Option<Drafts>> = const { RefCell::new(None) };
    static OWNER: RefCell<Option<Owner>> = const { RefCell::new(None) };
    /// The box holds a message being edited; its draft is stored aside.
    static EDITING: Cell<bool> = const { Cell::new(false) };
    /// A draft is being put into the box: not typing.
    static LOADING: Cell<bool> = const { Cell::new(false) };
    static SAVE_PENDING: Cell<bool> = const { Cell::new(false) };
}

/// How long after the last keystroke drafts are written to disk.
const SAVE_DELAY: Duration = Duration::from_millis(800);

fn path() -> Option<PathBuf> {
    kc_core::paths::data_dir().ok().map(|dir| dir.join("drafts.json"))
}

fn with_drafts<R>(work: impl FnOnce(&mut Drafts) -> R) -> R {
    DRAFTS.with_borrow_mut(|slot| {
        let drafts = slot.get_or_insert_with(|| {
            path()
                .and_then(|path| std::fs::read(path).ok())
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default()
        });
        work(drafts)
    })
}

/// Files `draft` under `owner` (an empty one removes it).
fn store(owner: &Owner, draft: Draft) -> bool {
    with_drafts(|drafts| {
        let channel = owner.1.to_string();
        let account = drafts.entry(owner.0.clone()).or_default();
        let changed = if draft.text.trim().is_empty() {
            account.remove(&channel).is_some()
        } else {
            account.insert(channel, draft.clone()).as_ref() != Some(&draft)
        };
        if account.is_empty() {
            drafts.remove(&owner.0);
        }
        changed
    })
}

fn load(owner: &Owner) -> Draft {
    with_drafts(|drafts| {
        drafts
            .get(&owner.0)
            .and_then(|account| account.get(&owner.1.to_string()))
            .cloned()
            .unwrap_or_default()
    })
}

/// Writes the drafts to disk now (atomically, readable only by you).
pub(crate) fn save_now() {
    SAVE_PENDING.set(false);
    let Some(path) = path() else { return };
    let Some(bytes) = DRAFTS.with_borrow(|drafts| drafts.as_ref().and_then(|drafts| serde_json::to_vec_pretty(drafts).ok()))
    else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = kc_core::paths::ensure_dir(dir);
    }
    let temporary = path.with_extension("json.tmp");
    if std::fs::write(&temporary, bytes).is_ok() {
        let _ = kc_core::paths::restrict(&temporary);
        if let Err(error) = std::fs::rename(&temporary, &path) {
            tracing::warn!(%error, "could not save drafts");
        }
    }
}

fn save_soon() {
    if SAVE_PENDING.replace(true) {
        return;
    }
    glib::timeout_add_local_once(SAVE_DELAY, save_now);
}

/// Whether a draft is being put into the message box right now (so the
/// change is not typing).
pub(crate) fn loading() -> bool {
    LOADING.get()
}

impl AppState {
    /// The account (its user ID, else its session) and channel whose draft
    /// the message box should show.
    fn draft_owner(&self) -> Option<Owner> {
        let (account, ..) = self.active()?;
        self.with_session(|session| {
            let channel = session.selected_channel?;
            let key = session
                .account
                .user_id
                .map_or_else(|| format!("session-{account}"), |id| id.to_string());
            Some((key, channel))
        })
        .flatten()
    }

    fn current_draft(&self) -> Draft {
        Draft {
            text: self.composer_text(),
            tokens: crate::composer::composer_tokens(),
        }
    }

    fn show_draft(&self, draft: Draft) {
        LOADING.set(true);
        self.set_composer_text(&draft.text);
        crate::composer::set_composer_tokens(draft.tokens);
        LOADING.set(false);
    }

    /// Brings up the right draft when the account or channel changed: the
    /// box's text is filed under the one it belonged to, and the new one's
    /// draft takes its place. Cheap when nothing changed.
    pub(crate) fn sync_draft(&self) {
        let now = self.draft_owner();
        if OWNER.with_borrow(|owner| *owner == now) {
            return;
        }
        if let Some(previous) = OWNER.with_borrow(Clone::clone)
            && !EDITING.replace(false)
            && store(&previous, self.current_draft())
        {
            save_soon();
        }
        EDITING.set(false);
        OWNER.with_borrow_mut(|owner| owner.clone_from(&now));
        match &now {
            Some(owner) => self.show_draft(load(owner)),
            None => self.show_draft(Draft::default()),
        }
    }

    /// After typing: keeps the draft current and saves it shortly.
    pub(crate) fn note_draft_changed(&self) {
        if LOADING.get() || EDITING.get() {
            return;
        }
        let Some(owner) = OWNER.with_borrow(Clone::clone) else {
            return;
        };
        if store(&owner, self.current_draft()) {
            save_soon();
        }
    }

    /// Editing a message borrows the box: the draft is set aside first.
    pub(crate) fn set_draft_aside(&self) {
        if EDITING.get() {
            return;
        }
        if let Some(owner) = OWNER.with_borrow(Clone::clone)
            && store(&owner, self.current_draft())
        {
            save_soon();
        }
        EDITING.set(true);
    }

    /// The edit is sent or cancelled: the draft comes back.
    pub(crate) fn restore_draft_after_edit(&self) {
        if !EDITING.replace(false) {
            return;
        }
        let draft = OWNER.with_borrow(|owner| owner.as_ref().map(load)).unwrap_or_default();
        self.show_draft(draft);
    }

    /// A sent message: its draft is gone, on disk too.
    pub(crate) fn clear_draft(self: &Rc<Self>) {
        if let Some(owner) = OWNER.with_borrow(Clone::clone)
            && store(&owner, Draft::default())
        {
            save_soon();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drafts_are_kept_per_account_and_channel_and_empty_ones_dropped() {
        DRAFTS.with_borrow_mut(|slot| *slot = Some(Drafts::new()));
        let a = ("1".to_owned(), Snowflake::new(10));
        let b = ("1".to_owned(), Snowflake::new(11));
        let other = ("2".to_owned(), Snowflake::new(10));
        assert!(store(&a, Draft { text: "hello".into(), tokens: vec![] }));
        assert!(store(&other, Draft { text: "other account".into(), tokens: vec![] }));
        assert_eq!(load(&a).text, "hello");
        assert_eq!(load(&b), Draft::default());
        assert_eq!(load(&other).text, "other account");
        // Storing the same again is no change; clearing removes it.
        assert!(!store(&a, Draft { text: "hello".into(), tokens: vec![] }));
        assert!(store(&a, Draft { text: "   ".into(), tokens: vec![] }));
        assert_eq!(load(&a), Draft::default());
        assert!(DRAFTS.with_borrow(|d| !d.as_ref().unwrap().contains_key("1")));
    }

    #[test]
    fn drafts_round_trip_through_json() {
        let mut drafts = Drafts::new();
        drafts.entry("1".into()).or_default().insert(
            "10".into(),
            Draft { text: "hi @Ada".into(), tokens: vec![("@Ada".into(), "<@2>".into())] },
        );
        let json = serde_json::to_vec(&drafts).unwrap();
        let back: Drafts = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, drafts);
    }
}
