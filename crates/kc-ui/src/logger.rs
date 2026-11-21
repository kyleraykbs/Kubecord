//! The message logger: keeps copies of messages as they arrive so that,
//! once deleted, they stay visible (marked in red) instead of vanishing.
//!
//! Only messages Discord sends to this client anyway are logged (gateway
//! events, and the history loaded to show a channel); nothing is fetched to
//! log. Each message falls in one of three kinds -- in the channel being
//! viewed, one that would notify (DND or not), or any other -- and each kind
//! has its own time to keep the text and the media (attachments, downloaded
//! when the message arrives, since Discord removes them with the message).
//!
//! The log is an SQLite database in the data folder, with the media beside
//! it; it is pruned as entries expire.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use kc_core::Snowflake;
use kc_state::Message;
use kc_state::notify::NotifyPrefs;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::AccountId;

/// How long to keep something logged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Keep {
    /// Not logged at all.
    Off,
    Day,
    Week,
    Month,
    HalfYear,
    Year,
    #[default]
    Forever,
}

impl Keep {
    pub const ALL: [Keep; 7] = [
        Keep::Off,
        Keep::Day,
        Keep::Week,
        Keep::Month,
        Keep::HalfYear,
        Keep::Year,
        Keep::Forever,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Keep::Off => "Don't Log",
            Keep::Day => "1 Day",
            Keep::Week => "1 Week",
            Keep::Month => "1 Month",
            Keep::HalfYear => "6 Months",
            Keep::Year => "1 Year",
            Keep::Forever => "Forever",
        }
    }

    /// Seconds kept, `None` for forever; only meaningful when not `Off`.
    fn seconds(self) -> Option<i64> {
        const DAY: i64 = 24 * 60 * 60;
        match self {
            Keep::Off => Some(0),
            Keep::Day => Some(DAY),
            Keep::Week => Some(7 * DAY),
            Keep::Month => Some(30 * DAY),
            Keep::HalfYear => Some(182 * DAY),
            Keep::Year => Some(365 * DAY),
            Keep::Forever => None,
        }
    }

    /// When something logged now expires: `None` for never.
    fn until(self, now: i64) -> Option<i64> {
        self.seconds().map(|seconds| now + seconds)
    }
}

/// How long to keep one kind of message's text and media.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepPair {
    pub text: Keep,
    pub media: Keep,
}

/// The logger's settings: one pair per kind of message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggerPrefs {
    /// In the channel being viewed.
    pub viewed: KeepPair,
    /// That would notify (whether or not Do Not Disturb was on).
    pub notifying: KeepPair,
    /// Every other message Discord sends.
    pub other: KeepPair,
}

impl Default for LoggerPrefs {
    fn default() -> Self {
        Self {
            viewed: KeepPair {
                text: Keep::Forever,
                media: Keep::HalfYear,
            },
            notifying: KeepPair {
                text: Keep::Year,
                media: Keep::Week,
            },
            other: KeepPair {
                text: Keep::Week,
                media: Keep::Off,
            },
        }
    }
}

/// Which kind a message is, for its times to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Viewed,
    Notifying,
    Other,
}

impl LoggerPrefs {
    fn pair(&self, kind: Kind) -> KeepPair {
        match kind {
            Kind::Viewed => self.viewed,
            Kind::Notifying => self.notifying,
            Kind::Other => self.other,
        }
    }
}

struct Logger {
    db: Mutex<Option<Connection>>,
    settings: Mutex<(LoggerPrefs, NotifyPrefs)>,
    /// The channel each account has open.
    viewed: Mutex<HashMap<AccountId, Snowflake>>,
}

static LOGGER: LazyLock<Logger> = LazyLock::new(|| Logger {
    db: Mutex::new(None),
    settings: Mutex::new((LoggerPrefs::default(), NotifyPrefs::default())),
    viewed: Mutex::new(HashMap::new()),
});

fn folder() -> Option<PathBuf> {
    kc_core::paths::data_dir().ok().map(|dir| dir.join("logger"))
}

fn media_folder() -> Option<PathBuf> {
    folder().map(|dir| dir.join("media"))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs() as i64)
}

const SCHEMA: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    CREATE TABLE IF NOT EXISTS logged (
        id INTEGER PRIMARY KEY,
        channel_id INTEGER NOT NULL,
        received_at INTEGER NOT NULL,
        json TEXT NOT NULL,
        text_until INTEGER,
        media_until INTEGER,
        deleted_at INTEGER
    );
    CREATE INDEX IF NOT EXISTS logged_deleted ON logged (channel_id, deleted_at, id);
    CREATE TABLE IF NOT EXISTS logged_media (
        message_id INTEGER NOT NULL,
        attachment_id INTEGER NOT NULL,
        path TEXT NOT NULL,
        PRIMARY KEY (message_id, attachment_id)
    );
";

/// Runs `work` on the open log (opening it the first time).
fn with_db<T>(work: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Option<T> {
    let mut db = LOGGER.db.lock().ok()?;
    if db.is_none() {
        let dir = folder()?;
        std::fs::create_dir_all(&dir).ok()?;
        let connection = Connection::open(dir.join("log.sqlite3"))
            .inspect_err(|error| tracing::warn!(%error, "could not open the message log"))
            .ok()?;
        connection
            .execute_batch(SCHEMA)
            .inspect_err(|error| tracing::warn!(%error, "could not set up the message log"))
            .ok()?;
        *db = Some(connection);
    }
    work(db.as_ref()?)
        .inspect_err(|error| tracing::warn!(%error, "message log"))
        .ok()
}

fn id_of(value: &Value) -> Option<Snowflake> {
    value.as_str()?.parse().ok()
}

/// Sets what to keep (and the notification choices that decide what would
/// notify).
pub(crate) fn set_prefs(prefs: LoggerPrefs, notify: NotifyPrefs) {
    if let Ok(mut settings) = LOGGER.settings.lock() {
        *settings = (prefs, notify);
    }
}

/// Notes the channel `account` has open (or none).
pub(crate) fn set_viewed(account: AccountId, channel: Option<Snowflake>) {
    if let Ok(mut viewed) = LOGGER.viewed.lock() {
        match channel {
            Some(channel) => viewed.insert(account, channel),
            None => viewed.remove(&account),
        };
    }
}

/// Which kind `message` is for `account`.
fn kind_of(account: AccountId, message: &Message, store: &kc_state::Store, notify: &NotifyPrefs) -> Kind {
    let viewed = LOGGER
        .viewed
        .lock()
        .ok()
        .and_then(|viewed| viewed.get(&account).copied());
    if viewed == Some(message.channel_id) {
        return Kind::Viewed;
    }
    let notifies = store.read(|state| {
        let facts = kc_state::notify::MessageFacts {
            viewing: false,
            do_not_disturb: false,
            ..state.message_facts(message, false)
        };
        kc_state::notify::decide(&facts, notify).is_some()
    });
    if notifies { Kind::Notifying } else { Kind::Other }
}

/// Logs `data` (a message as Discord sent it) for `account`, if its kind is
/// kept; a deleted one (`deleted`) is marked so at once.
fn log_message(account: AccountId, data: &Value, store: &kc_state::Store, deleted: bool) {
    let Ok(message) = serde_json::from_value::<Message>(data.clone()) else {
        return;
    };
    let (prefs, notify) = match LOGGER.settings.lock() {
        Ok(settings) => *settings,
        Err(_) => return,
    };
    let pair = prefs.pair(kind_of(account, &message, store, &notify));
    if pair.text == Keep::Off && pair.media == Keep::Off {
        return;
    }
    let now = now();
    let mut json = data.clone();
    // Media only: the text is not kept.
    if pair.text == Keep::Off {
        json["content"] = Value::String(String::new());
        json["embeds"] = Value::Array(Vec::new());
    }
    let has_media = !message.attachments.is_empty() && pair.media != Keep::Off;
    let text_until = if pair.text == Keep::Off { Some(now) } else { pair.text.until(now) };
    let media_until = if has_media { pair.media.until(now) } else { Some(0) };
    let id = message.id.get() as i64;
    with_db(|db| {
        db.execute(
            "INSERT INTO logged (id, channel_id, received_at, json, text_until, media_until, deleted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET json = excluded.json",
            params![
                id,
                message.channel_id.get() as i64,
                now,
                json.to_string(),
                text_until,
                media_until,
                deleted.then_some(now),
            ],
        )
    });
    if has_media {
        download_media(&message);
    }
}

/// Keeps copies of a message's attachments, in the background.
fn download_media(message: &Message) {
    let Some(dir) = media_folder().map(|dir| dir.join(message.id.to_string())) else {
        return;
    };
    let message_id = message.id.get() as i64;
    let files: Vec<(i64, String, String)> = message
        .attachments
        .iter()
        .map(|attachment| {
            let safe: String = attachment
                .filename
                .chars()
                .map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
                .collect();
            (attachment.id.get() as i64, attachment.url.clone(), safe)
        })
        .collect();
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    runtime.spawn(async move {
        for (attachment_id, url, name) in files {
            let Ok(bytes) = kc_http::cdn::fetch(&url).await else {
                continue;
            };
            if tokio::fs::create_dir_all(&dir).await.is_err() {
                return;
            }
            let path = dir.join(format!("{attachment_id}-{name}"));
            if tokio::fs::write(&path, bytes).await.is_ok() {
                let path = path.to_string_lossy().into_owned();
                with_db(|db| {
                    db.execute(
                        "INSERT OR REPLACE INTO logged_media (message_id, attachment_id, path) VALUES (?1, ?2, ?3)",
                        params![message_id, attachment_id, path],
                    )
                });
            }
        }
    });
}

/// Watches a gateway event for `account` before the store applies it:
/// logs new messages, follows edits, and marks deletions (logging a
/// deleted message the store still has, if it was not logged yet).
pub(crate) fn observe(account: AccountId, name: &str, data: &Value, store: &kc_state::Store) {
    match name {
        "MESSAGE_CREATE" => log_message(account, data, store, false),
        "MESSAGE_UPDATE" => {
            let Some(id) = id_of(&data["id"]) else {
                return;
            };
            let id = id.get() as i64;
            // A full message replaces the copy; a partial one (an embed
            // filling in) patches it.
            let stored: Option<String> = with_db(|db| {
                db.query_row("SELECT json FROM logged WHERE id = ?1", params![id], |row| row.get(0))
                    .optional()
            })
            .flatten();
            let Some(stored) = stored else {
                return;
            };
            let mut json: Value = serde_json::from_str(&stored).unwrap_or(Value::Null);
            if data.get("author").is_some() {
                json = data.clone();
            } else if let Some(object) = data.as_object() {
                for (key, value) in object {
                    json[key] = value.clone();
                }
            }
            with_db(|db| {
                db.execute("UPDATE logged SET json = ?2 WHERE id = ?1", params![id, json.to_string()])
            });
        }
        "MESSAGE_DELETE" | "MESSAGE_DELETE_BULK" => {
            let Some(channel) = id_of(&data["channel_id"]) else {
                return;
            };
            let ids: Vec<Snowflake> = match data["ids"].as_array() {
                Some(ids) => ids.iter().filter_map(id_of).collect(),
                None => id_of(&data["id"]).into_iter().collect(),
            };
            let now = now();
            for id in ids {
                let marked = with_db(|db| {
                    db.execute(
                        "UPDATE logged SET deleted_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
                        params![id.get() as i64, now],
                    )
                })
                .unwrap_or(0);
                let known = with_db(|db| {
                    db.query_row("SELECT 1 FROM logged WHERE id = ?1", params![id.get() as i64], |_| Ok(()))
                        .optional()
                })
                .flatten()
                .is_some();
                // Not logged when it came (it was loaded with the history):
                // log it now, from the copy the store still has.
                if marked == 0 && !known {
                    let copy = store.read(|state| {
                        state
                            .messages
                            .get(&channel)
                            .and_then(|buffer| buffer.get(id))
                            .and_then(|message| serde_json::to_value(message).ok())
                    });
                    if let Some(copy) = copy {
                        log_message(account, &copy, store, true);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Reads a logged row as a deleted message, its attachments pointing at the
/// kept copies.
fn deleted_message(json: &str, media: &HashMap<i64, String>) -> Option<Message> {
    let mut message: Message = serde_json::from_str(json).ok()?;
    message.deleted = true;
    for attachment in &mut message.attachments {
        if let Some(path) = media.get(&(attachment.id.get() as i64)) {
            let uri = format!("file://{path}");
            attachment.url = uri.clone();
            attachment.proxy_url = uri;
        }
    }
    Some(message)
}

fn media_of(db: &Connection, message_id: i64) -> rusqlite::Result<HashMap<i64, String>> {
    let mut query = db.prepare_cached("SELECT attachment_id, path FROM logged_media WHERE message_id = ?1")?;
    let rows = query.query_map(params![message_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// The logged copy of a message that was just deleted, if it is kept.
pub(crate) fn deleted(message_id: Snowflake) -> Option<Message> {
    let id = message_id.get() as i64;
    with_db(|db| {
        let json: Option<String> = db
            .query_row(
                "SELECT json FROM logged WHERE id = ?1 AND deleted_at IS NOT NULL",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(json.and_then(|json| deleted_message(&json, &media_of(db, id).unwrap_or_default())))
    })
    .flatten()
}

/// The deleted messages logged for `channel` from `low` to `high`.
pub(crate) fn deleted_between(channel: Snowflake, low: Snowflake, high: Snowflake) -> Vec<Message> {
    with_db(|db| {
        let mut query = db.prepare_cached(
            "SELECT id, json FROM logged
             WHERE channel_id = ?1 AND deleted_at IS NOT NULL AND id BETWEEN ?2 AND ?3",
        )?;
        let rows: Vec<(i64, String)> = query
            .query_map(
                params![channel.get() as i64, low.get() as i64, high.get().min(i64::MAX as u64) as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, json)| deleted_message(&json, &media_of(db, id).unwrap_or_default()))
            .collect())
    })
    .unwrap_or_default()
}

fn remove_media_files(paths: &[String]) {
    for path in paths {
        let path = Path::new(path);
        let _ = std::fs::remove_file(path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

/// Takes a message out of the log (and its media).
pub(crate) fn forget(message_id: Snowflake) {
    let id = message_id.get() as i64;
    let paths: Vec<String> = with_db(|db| {
        let paths = media_of(db, id)?.into_values().collect();
        db.execute("DELETE FROM logged_media WHERE message_id = ?1", params![id])?;
        db.execute("DELETE FROM logged WHERE id = ?1", params![id])?;
        Ok(paths)
    })
    .unwrap_or_default();
    remove_media_files(&paths);
}

/// Drops what has expired: media past its time, then text (a message whose
/// media is still kept loses only its text).
pub(crate) fn prune() {
    let now = now();
    let paths: Vec<String> = with_db(|db| {
        let mut query = db.prepare(
            "SELECT m.path FROM logged_media m JOIN logged l ON l.id = m.message_id
             WHERE l.media_until IS NOT NULL AND l.media_until < ?1",
        )?;
        let paths: Vec<String> = query
            .query_map(params![now], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        db.execute(
            "DELETE FROM logged_media WHERE message_id IN
             (SELECT id FROM logged WHERE media_until IS NOT NULL AND media_until < ?1)",
            params![now],
        )?;
        db.execute(
            "UPDATE logged SET media_until = 0 WHERE media_until IS NOT NULL AND media_until < ?1",
            params![now],
        )?;
        // Text expired with no media left: gone.
        db.execute(
            "DELETE FROM logged WHERE text_until IS NOT NULL AND text_until < ?1 AND media_until = 0",
            params![now],
        )?;
        // Text expired with media still kept: the text goes.
        let mut blank = db.prepare(
            "SELECT id, json FROM logged WHERE text_until IS NOT NULL AND text_until < ?1",
        )?;
        let rows: Vec<(i64, String)> = blank
            .query_map(params![now], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, json) in rows {
            let mut json: Value = serde_json::from_str(&json).unwrap_or(Value::Null);
            json["content"] = Value::String(String::new());
            json["embeds"] = Value::Array(Vec::new());
            db.execute(
                "UPDATE logged SET json = ?2, text_until = NULL WHERE id = ?1",
                params![id, json.to_string()],
            )?;
        }
        Ok(paths)
    })
    .unwrap_or_default();
    remove_media_files(&paths);
}

/// How much is logged: messages, deleted ones, and the media's size.
pub(crate) fn stats() -> (u64, u64, u64) {
    let (all, deleted) = with_db(|db| {
        db.query_row(
            "SELECT COUNT(*), COUNT(deleted_at) FROM logged",
            [],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
        )
    })
    .unwrap_or((0, 0));
    let bytes = media_folder().map_or(0, |dir| folder_size(&dir));
    (all, deleted, bytes)
}

fn folder_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => folder_size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Empties the log.
pub(crate) fn clear() {
    with_db(|db| db.execute_batch("DELETE FROM logged_media; DELETE FROM logged;"));
    if let Some(dir) = media_folder() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_request() {
        let prefs = LoggerPrefs::default();
        assert_eq!(prefs.viewed, KeepPair { text: Keep::Forever, media: Keep::HalfYear });
        assert_eq!(prefs.notifying, KeepPair { text: Keep::Year, media: Keep::Week });
        assert_eq!(prefs.other, KeepPair { text: Keep::Week, media: Keep::Off });
    }

    #[test]
    fn times_to_keep() {
        assert_eq!(Keep::Forever.until(100), None);
        assert_eq!(Keep::Day.until(100), Some(100 + 86_400));
        assert_eq!(Keep::ALL.len(), 7);
        assert_eq!(serde_json::to_value(Keep::HalfYear).unwrap(), "half_year");
    }

    #[test]
    fn deleted_copies_point_at_kept_media() {
        let json = serde_json::json!({
            "id": "5", "channel_id": "1", "content": "hi",
            "author": {"id": "2", "username": "a"}, "timestamp": "2026-11-21T10:00:00+00:00",
            "attachments": [{"id": "9", "filename": "cat.png", "url": "https://cdn/cat.png", "proxy_url": "https://media/cat.png"}]
        })
        .to_string();
        let media = HashMap::from([(9, "/logs/5/9-cat.png".to_owned())]);
        let message = deleted_message(&json, &media).unwrap();
        assert!(message.deleted);
        assert_eq!(message.attachments[0].url, "file:///logs/5/9-cat.png");
    }
}

impl crate::shell::AppState {
    /// Puts a message that was just deleted back in `account`'s store from
    /// the log (marked deleted), when the log kept it and it belongs in the
    /// loaded stretch of its channel. False when it goes away as usual.
    pub(crate) fn keep_deleted(&self, account: AccountId, channel: Snowflake, message_id: Snowflake) -> bool {
        let Some(message) = deleted(message_id) else {
            return false;
        };
        let store = self
            .model
            .borrow()
            .sessions
            .get(&account)
            .map(|session| session.store.clone());
        let Some(store) = store else {
            return false;
        };
        store.write(|state| {
            let Some(buffer) = state.messages.get_mut(&channel) else {
                return false;
            };
            let messages = buffer.messages();
            let (Some(first), Some(last)) = (messages.first(), messages.last()) else {
                return false;
            };
            let inside = message_id >= first.id && (message_id <= last.id || !buffer.is_detached());
            if inside {
                buffer.insert(message);
            }
            inside
        })
    }

    /// Adds the logged deleted messages of `channel` that fall in its loaded
    /// stretch (after history replaced or grew it).
    pub(crate) fn merge_logged(&self, account: AccountId, channel: Snowflake) {
        let store = self
            .model
            .borrow()
            .sessions
            .get(&account)
            .map(|session| session.store.clone());
        let Some(store) = store else {
            return;
        };
        store.write(|state| {
            let Some(buffer) = state.messages.get_mut(&channel) else {
                return;
            };
            let messages = buffer.messages();
            let (Some(first), Some(last)) = (messages.first().map(|m| m.id), messages.last().map(|m| m.id)) else {
                return;
            };
            let high = if buffer.is_detached() { last } else { Snowflake::new(i64::MAX as u64) };
            let missing: Vec<Message> = deleted_between(channel, first, high)
                .into_iter()
                .filter(|message| buffer.get(message.id).is_none())
                .collect();
            if !missing.is_empty() {
                buffer.extend(missing);
            }
        });
    }
}
