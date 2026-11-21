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
    /// Media kept per message, in MB (0: no limit); the largest
    /// attachments are left out first.
    pub per_message_mb: u32,
    /// The most the log may hold of what arrived in the last week, month
    /// and year, in MB (0: no limit). Named apart from the first version of
    /// these settings, whose zeros (no limit) were its defaults.
    pub week_cap_mb: u32,
    pub month_cap_mb: u32,
    pub year_cap_mb: u32,
    /// Which kinds the size limits may remove.
    pub collect: Collect,
}

/// A size limit as shown: "No limit", "500 MB", "1 GB", "1.5 GB".
pub(crate) fn size_text(mb: u32) -> String {
    match mb {
        0 => "No limit".to_owned(),
        mb if mb >= 1024 => {
            let gb = format!("{:.2}", f64::from(mb) / 1024.0);
            format!("{} GB", gb.trim_end_matches('0').trim_end_matches('.'))
        }
        mb => format!("{mb} MB"),
    }
}

/// A size limit as typed ("2 GB", "500", "500mb", "no limit"), in MB.
pub(crate) fn parse_size(text: &str) -> Option<u32> {
    let text = text.trim().to_lowercase();
    if text.is_empty() || text.starts_with("no") || text == "none" {
        return Some(0);
    }
    let (number, factor) = if let Some(number) = text.strip_suffix("gb") {
        (number, 1024.0)
    } else if let Some(number) = text.strip_suffix("mb") {
        (number, 1.0)
    } else {
        (text.as_str(), 1.0)
    };
    let value: f64 = number.trim().parse().ok()?;
    (value >= 0.0).then(|| (value * factor).round() as u32)
}

/// Which kinds of message the size limits may remove.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Collect {
    pub viewed: bool,
    pub notifying: bool,
    pub other: bool,
}

impl Default for Collect {
    fn default() -> Self {
        Self {
            viewed: false,
            notifying: false,
            other: true,
        }
    }
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
            per_message_mb: 20,
            week_cap_mb: 1024,
            month_cap_mb: 2 * 1024,
            year_cap_mb: 20 * 1024,
            collect: Collect::default(),
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

impl Kind {
    /// Stored with each message; the higher, the sooner it goes when the
    /// log is too big.
    fn rank(self) -> i64 {
        match self {
            Kind::Viewed => 0,
            Kind::Notifying => 1,
            Kind::Other => 2,
        }
    }
}

impl LoggerPrefs {
    /// The ranks the size limits may remove.
    fn collectable(&self) -> Vec<i64> {
        [
            (self.collect.viewed, Kind::Viewed),
            (self.collect.notifying, Kind::Notifying),
            (self.collect.other, Kind::Other),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .map(|(_, kind)| kind.rank())
        .collect()
    }

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
    CREATE INDEX IF NOT EXISTS logged_received ON logged (received_at);
    CREATE TABLE IF NOT EXISTS logged_edits (
        message_id INTEGER NOT NULL,
        edited_at TEXT NOT NULL,
        content TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS logged_edits_message ON logged_edits (message_id);
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
        // Columns added later: absent from a log made before them (the
        // error for one already there is expected).
        for column in [
            "ALTER TABLE logged ADD COLUMN kind INTEGER NOT NULL DEFAULT 2",
            "ALTER TABLE logged_media ADD COLUMN size INTEGER NOT NULL DEFAULT 0",
        ] {
            let _ = connection.execute(column, []);
        }
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
    let changed = match LOGGER.settings.lock() {
        Ok(mut settings) => {
            let changed = settings.0 != prefs;
            *settings = (prefs, notify);
            changed
        }
        Err(_) => false,
    };
    // Tighter limits apply at once (off the interface's thread).
    if changed && prefs.week_cap_mb + prefs.month_cap_mb + prefs.year_cap_mb > 0 {
        std::thread::spawn(move || enforce_limits(&prefs));
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
    let kind = kind_of(account, &message, store, &notify);
    let pair = prefs.pair(kind);
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
            "INSERT INTO logged (id, channel_id, received_at, json, text_until, media_until, deleted_at, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET json = excluded.json",
            params![
                id,
                message.channel_id.get() as i64,
                now,
                json.to_string(),
                text_until,
                media_until,
                deleted.then_some(now),
                kind.rank(),
            ],
        )
    });
    if has_media {
        download_media(&message, prefs);
    }
}

/// Keeps copies of a message's attachments, in the background.
fn download_media(message: &Message, prefs: LoggerPrefs) {
    let Some(dir) = media_folder().map(|dir| dir.join(message.id.to_string())) else {
        return;
    };
    let message_id = message.id.get() as i64;
    let files: Vec<(i64, String, String)> = within_message_limit(&message.attachments, prefs.per_message_mb)
        .into_iter()
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
            let size = bytes.len() as i64;
            if tokio::fs::write(&path, bytes).await.is_ok() {
                let path = path.to_string_lossy().into_owned();
                with_db(|db| {
                    db.execute(
                        "INSERT OR REPLACE INTO logged_media (message_id, attachment_id, path, size) VALUES (?1, ?2, ?3, ?4)",
                        params![message_id, attachment_id, path, size],
                    )
                });
            }
        }
        // New media may take the log past a limit.
        if prefs.week_cap_mb + prefs.month_cap_mb + prefs.year_cap_mb > 0 {
            let _ = tokio::task::spawn_blocking(move || enforce_limits(&prefs)).await;
        }
    });
}

/// The attachments kept under a per-message limit of `mb` (0: all): the
/// smallest first, so the largest are the ones left out.
fn within_message_limit(attachments: &[kc_state::Attachment], mb: u32) -> Vec<&kc_state::Attachment> {
    let mut sorted: Vec<&kc_state::Attachment> = attachments.iter().collect();
    if mb == 0 {
        return sorted;
    }
    let limit = u64::from(mb) * 1024 * 1024;
    sorted.sort_by_key(|attachment| attachment.size);
    let mut total = 0;
    sorted
        .into_iter()
        .take_while(|attachment| {
            total += attachment.size;
            total <= limit
        })
        .collect()
}

/// What the log holds of what arrived since `since` (unix seconds): the
/// text and the media, in bytes.
fn held_since(db: &Connection, since: i64) -> rusqlite::Result<i64> {
    db.query_row(
        "SELECT COALESCE((SELECT SUM(length(json)) FROM logged WHERE received_at >= ?1), 0)
              + COALESCE((SELECT SUM(m.size) FROM logged_media m JOIN logged l ON l.id = m.message_id
                          WHERE l.received_at >= ?1), 0)
              + COALESCE((SELECT SUM(length(e.content)) FROM logged_edits e JOIN logged l ON l.id = e.message_id
                          WHERE l.received_at >= ?1), 0)",
        params![since],
        |row| row.get(0),
    )
}

/// Brings the log within its week, month and year limits. Media goes
/// first, the largest first, across every limit; only once no media is left
/// to drop does text go, the least important kind and oldest first (with its
/// earlier versions). Only the kinds whose clean-up is on are touched; past
/// that, a limit is left exceeded.
pub(crate) fn enforce_limits(prefs: &LoggerPrefs) {
    let paths = with_db(|db| enforce_on(db, prefs, now())).unwrap_or_default();
    remove_media_files(&paths);
}

/// [`enforce_limits`] on `db` at time `now`; returns the media files to
/// delete.
fn enforce_on(db: &Connection, prefs: &LoggerPrefs, now: i64) -> rusqlite::Result<Vec<String>> {
    let mut removed = Vec::new();
    let ranks = prefs.collectable();
    if ranks.is_empty() {
        return Ok(removed);
    }
    let ranks_sql = ranks.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    const DAY: i64 = 24 * 60 * 60;
    let windows: Vec<(i64, i64)> = [(7, prefs.week_cap_mb), (30, prefs.month_cap_mb), (365, prefs.year_cap_mb)]
        .into_iter()
        .filter(|(_, mb)| *mb > 0)
        .map(|(days, mb)| (now - days * DAY, i64::from(mb) * 1024 * 1024))
        .collect();

    // First pass: media, every limit.
    for &(since, limit) in &windows {
        let mut over = held_since(db, since)? - limit;
        if over <= 0 {
            continue;
        }
        let media: Vec<(i64, i64, String, i64)> = db
            .prepare(&format!(
                "SELECT m.message_id, m.attachment_id, m.path, m.size FROM logged_media m
                 JOIN logged l ON l.id = m.message_id
                 WHERE l.received_at >= ?1 AND l.kind IN ({ranks_sql})
                 ORDER BY m.size DESC"
            ))?
            .query_map(params![since], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (message_id, attachment_id, path, size) in media {
            if over <= 0 {
                break;
            }
            db.execute(
                "DELETE FROM logged_media WHERE message_id = ?1 AND attachment_id = ?2",
                params![message_id, attachment_id],
            )?;
            removed.push(path);
            over -= size.max(1);
        }
    }

    // Second pass, only where dropping media was not enough: text.
    for &(since, limit) in &windows {
        let mut over = held_since(db, since)? - limit;
        if over <= 0 {
            continue;
        }
        let texts: Vec<(i64, i64)> = db
            .prepare(&format!(
                "SELECT l.id, length(l.json)
                   + COALESCE((SELECT SUM(length(e.content)) FROM logged_edits e WHERE e.message_id = l.id), 0)
                 FROM logged l
                 WHERE l.received_at >= ?1 AND l.kind IN ({ranks_sql})
                 ORDER BY l.kind DESC, l.received_at ASC"
            ))?
            .query_map(params![since], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, size) in texts {
            if over <= 0 {
                break;
            }
            let paths: Vec<String> = db
                .prepare_cached("SELECT path FROM logged_media WHERE message_id = ?1")?
                .query_map(params![id], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            db.execute("DELETE FROM logged_media WHERE message_id = ?1", params![id])?;
            db.execute("DELETE FROM logged_edits WHERE message_id = ?1", params![id])?;
            db.execute("DELETE FROM logged WHERE id = ?1", params![id])?;
            removed.extend(paths);
            over -= size.max(1);
        }
    }
    Ok(removed)
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
            let snowflake = id;
            let id = id.get() as i64;
            let read = || -> Option<String> {
                with_db(|db| {
                    db.query_row("SELECT json FROM logged WHERE id = ?1", params![id], |row| row.get(0))
                        .optional()
                })
                .flatten()
            };
            // Not logged when it came (loaded with the history): logged now,
            // from the copy the store still has, so its old text is kept.
            let stored = read().or_else(|| {
                let channel = id_of(&data["channel_id"])?;
                let copy = store.read(|state| {
                    state
                        .messages
                        .get(&channel)
                        .and_then(|buffer| buffer.get(snowflake))
                        .and_then(|message| serde_json::to_value(message).ok())
                })?;
                log_message(account, &copy, store, false);
                read()
            });
            let Some(stored) = stored else {
                return;
            };
            let mut json: Value = serde_json::from_str(&stored).unwrap_or(Value::Null);
            // The text changed: the old version goes in the history, dated
            // when it was written.
            if let Some(new) = data.get("content").and_then(Value::as_str)
                && let Some(old) = json["content"].as_str()
                && old != new
                && !old.is_empty()
            {
                let written = json["edited_timestamp"]
                    .as_str()
                    .or_else(|| json["timestamp"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                let old = old.to_owned();
                with_db(|db| {
                    db.execute(
                        "INSERT INTO logged_edits (message_id, edited_at, content) VALUES (?1, ?2, ?3)",
                        params![id, written, old],
                    )
                });
            }
            // A full message replaces the copy; a partial one (an embed
            // filling in) patches it.
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
fn deleted_message(json: &str, media: &HashMap<i64, String>, edits: Vec<kc_state::MessageEdit>) -> Option<Message> {
    let mut message: Message = serde_json::from_str(json).ok()?;
    message.deleted = true;
    message.edits = edits;
    for attachment in &mut message.attachments {
        if let Some(path) = media.get(&(attachment.id.get() as i64)) {
            let uri = format!("file://{path}");
            attachment.url = uri.clone();
            attachment.proxy_url = uri;
        }
    }
    Some(message)
}

/// The earlier versions of a message, oldest first.
fn edits_of(db: &Connection, message_id: i64) -> rusqlite::Result<Vec<kc_state::MessageEdit>> {
    let mut query = db.prepare_cached(
        "SELECT content, edited_at FROM logged_edits WHERE message_id = ?1 ORDER BY rowid",
    )?;
    let rows = query.query_map(params![message_id], |row| {
        Ok(kc_state::MessageEdit {
            content: row.get(0)?,
            at: row.get(1)?,
        })
    })?;
    rows.collect()
}

/// The earlier versions of messages in `channel` from `low` to `high`, by
/// message.
pub(crate) fn edits_between(
    channel: Snowflake,
    low: Snowflake,
    high: Snowflake,
) -> HashMap<Snowflake, Vec<kc_state::MessageEdit>> {
    with_db(|db| {
        let mut query = db.prepare_cached(
            "SELECT e.message_id, e.content, e.edited_at FROM logged_edits e
             JOIN logged l ON l.id = e.message_id
             WHERE l.channel_id = ?1 AND e.message_id BETWEEN ?2 AND ?3
             ORDER BY e.rowid",
        )?;
        let mut found: HashMap<Snowflake, Vec<kc_state::MessageEdit>> = HashMap::new();
        let rows = query.query_map(
            params![channel.get() as i64, low.get() as i64, high.get().min(i64::MAX as u64) as i64],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    kc_state::MessageEdit {
                        content: row.get(1)?,
                        at: row.get(2)?,
                    },
                ))
            },
        )?;
        for row in rows {
            let (id, edit) = row?;
            found.entry(Snowflake::new(id as u64)).or_default().push(edit);
        }
        Ok(found)
    })
    .unwrap_or_default()
}

/// The earlier versions of one message.
pub(crate) fn edits(message_id: Snowflake) -> Vec<kc_state::MessageEdit> {
    with_db(|db| edits_of(db, message_id.get() as i64)).unwrap_or_default()
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
        Ok(json.and_then(|json| {
            deleted_message(&json, &media_of(db, id).unwrap_or_default(), edits_of(db, id).unwrap_or_default())
        }))
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
            .filter_map(|(id, json)| {
                deleted_message(&json, &media_of(db, id).unwrap_or_default(), edits_of(db, id).unwrap_or_default())
            })
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

/// Drops a message's earlier versions from the log.
pub(crate) fn forget_edits(message_id: Snowflake) {
    let id = message_id.get() as i64;
    with_db(|db| db.execute("DELETE FROM logged_edits WHERE message_id = ?1", params![id]));
}

/// Takes a message out of the log (and its media).
pub(crate) fn forget(message_id: Snowflake) {
    let id = message_id.get() as i64;
    let paths: Vec<String> = with_db(|db| {
        let paths = media_of(db, id)?.into_values().collect();
        db.execute("DELETE FROM logged_media WHERE message_id = ?1", params![id])?;
        db.execute("DELETE FROM logged_edits WHERE message_id = ?1", params![id])?;
        db.execute("DELETE FROM logged WHERE id = ?1", params![id])?;
        Ok(paths)
    })
    .unwrap_or_default();
    remove_media_files(&paths);
}

/// Drops what has expired: media past its time, then text (a message whose
/// media is still kept loses only its text).
pub(crate) fn prune() {
    let prefs = LOGGER.settings.lock().map(|settings| settings.0).unwrap_or_default();
    enforce_limits(&prefs);
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
        db.execute(
            "DELETE FROM logged_edits WHERE message_id NOT IN (SELECT id FROM logged)",
            [],
        )?;
        db.execute(
            "DELETE FROM logged_edits WHERE message_id IN
             (SELECT id FROM logged WHERE text_until IS NOT NULL AND text_until < ?1)",
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
    with_db(|db| db.execute_batch("DELETE FROM logged_media; DELETE FROM logged_edits; DELETE FROM logged;"));
    if let Some(dir) = media_folder() {
        let _ = std::fs::remove_dir_all(dir);
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

    /// Gives a message in `account`'s store the earlier versions of its text
    /// the log kept (after an edit replaced it).
    pub(crate) fn attach_edits(&self, account: AccountId, channel: Snowflake, message_id: Snowflake) {
        let found = edits(message_id);
        if found.is_empty() {
            return;
        }
        let store = self
            .model
            .borrow()
            .sessions
            .get(&account)
            .map(|session| session.store.clone());
        if let Some(store) = store {
            store.write(|state| {
                if let Some(buffer) = state.messages.get_mut(&channel) {
                    buffer.update_with(message_id, |message| message.edits = found);
                }
            });
        }
    }

    /// Adds the logged deleted messages of `channel` that fall in its loaded
    /// stretch (after history replaced or grew it), and the earlier versions
    /// of the edited ones.
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
            for (id, found) in edits_between(channel, first, high) {
                buffer.update_with(id, |message| {
                    if message.edits.is_empty() {
                        message.edits = found;
                    }
                });
            }
        });
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
    fn the_per_message_limit_leaves_out_the_largest() {
        let attachment = |id: u64, mb: u64| -> kc_state::Attachment {
            serde_json::from_value(serde_json::json!({
                "id": id.to_string(), "filename": "f", "size": mb * 1024 * 1024
            }))
            .unwrap()
        };
        let files = [attachment(1, 15), attachment(2, 3), attachment(3, 4)];
        let kept: Vec<u64> = within_message_limit(&files, 20).iter().map(|a| a.id.get()).collect();
        assert_eq!(kept, vec![2, 3]);
        assert_eq!(within_message_limit(&files, 0).len(), 3);
    }

    #[test]
    fn size_limits_take_least_important_and_oldest_text_first() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db.execute("ALTER TABLE logged ADD COLUMN kind INTEGER NOT NULL DEFAULT 2", []).unwrap();
        db.execute("ALTER TABLE logged_media ADD COLUMN size INTEGER NOT NULL DEFAULT 0", []).unwrap();
        for (id, kind, at) in [(1, 0, 10), (2, 2, 20), (3, 1, 5), (4, 2, 30)] {
            db.execute(
                "INSERT INTO logged (id, channel_id, received_at, json, kind) VALUES (?1, 1, ?2, 'x', ?3)",
                params![id, at, kind],
            )
            .unwrap();
        }
        // The order the text would go in, with everything collectable.
        let order: Vec<i64> = db
            .prepare("SELECT id FROM logged WHERE kind IN (0,1,2) ORDER BY kind DESC, received_at ASC")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(order, vec![2, 4, 3, 1]);
        assert_eq!(held_since(&db, 0).unwrap(), 4);
        let defaults = LoggerPrefs::default();
        assert_eq!(defaults.collectable(), vec![2]);
        assert_eq!(defaults.per_message_mb, 20);

        // Over a 1 MB week: media first, the largest first.
        let mb = 1024 * 1024;
        db.execute(
            "INSERT INTO logged_media (message_id, attachment_id, path, size) VALUES (2, 1, 'big', ?1), (4, 1, 'small', ?2)",
            params![mb, mb / 2],
        )
        .unwrap();
        let mut prefs = LoggerPrefs { week_cap_mb: 1, month_cap_mb: 0, year_cap_mb: 0, ..LoggerPrefs::default() };
        prefs.collect = Collect { viewed: true, notifying: true, other: true };
        let removed = enforce_on(&db, &prefs, 40).unwrap();
        assert_eq!(removed, vec!["big".to_owned()]);
        let rows: i64 = db.query_row("SELECT COUNT(*) FROM logged", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 4, "no text went while media could go");

        // A tiny limit: the rest of the media, then text in order -- but
        // only kinds whose clean-up is on (here not the viewed one).
        prefs.collect.viewed = false;
        db.execute("UPDATE logged SET json = ?1", params!["x".repeat(400 * 1024)]).unwrap();
        let removed = enforce_on(&db, &prefs, 40).unwrap();
        assert_eq!(removed, vec!["small".to_owned()]);
        let left: Vec<i64> = db
            .prepare("SELECT id FROM logged ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        // 4 x 400 KB is over 1 MB: other (2, then 4) goes until under.
        assert_eq!(left, vec![1, 3]);
    }

    #[test]
    fn all_media_goes_before_any_text() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db.execute("ALTER TABLE logged ADD COLUMN kind INTEGER NOT NULL DEFAULT 2", []).unwrap();
        db.execute("ALTER TABLE logged_media ADD COLUMN size INTEGER NOT NULL DEFAULT 0", []).unwrap();
        let mb = 1024 * 1024;
        let day = 24 * 60 * 60;
        let now = 400 * day;
        // This week: 600 KB of text; three weeks ago: 2 MB of media.
        db.execute(
            "INSERT INTO logged (id, channel_id, received_at, json, kind) VALUES (1, 1, ?1, ?2, 2), (2, 1, ?3, 'x', 2)",
            params![now - day, "x".repeat(600 * 1024), now - 21 * day],
        )
        .unwrap();
        db.execute("INSERT INTO logged_media (message_id, attachment_id, path, size) VALUES (2, 1, 'old', ?1)", params![2 * mb])
            .unwrap();
        db.execute("INSERT INTO logged_edits (message_id, edited_at, content) VALUES (1, 't', 'before')", [])
            .unwrap();
        // Over the month's 1 MB (2.6 MB) and the week's 0.5 MB (0.6 MB).
        let prefs = LoggerPrefs { week_cap_mb: 0, month_cap_mb: 1, year_cap_mb: 0, ..LoggerPrefs::default() };
        let removed = enforce_on(&db, &prefs, now).unwrap();
        // Dropping the old media was enough for the month: no text went.
        assert_eq!(removed, vec!["old".to_owned()]);
        let rows: i64 = db.query_row("SELECT COUNT(*) FROM logged", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 2);
        // A week limit below the text: no media is left, so text goes,
        // with its earlier versions.
        let prefs = LoggerPrefs { week_cap_mb: 0, month_cap_mb: 0, year_cap_mb: 0, ..LoggerPrefs::default() };
        assert!(enforce_on(&db, &prefs, now).unwrap().is_empty());
        db.execute("UPDATE logged SET json = ?1 WHERE id = 1", params!["x".repeat(2 * 1024 * 1024)]).unwrap();
        let prefs = LoggerPrefs { week_cap_mb: 1, month_cap_mb: 0, year_cap_mb: 0, ..LoggerPrefs::default() };
        enforce_on(&db, &prefs, now).unwrap();
        let edits: i64 = db.query_row("SELECT COUNT(*) FROM logged_edits", [], |row| row.get(0)).unwrap();
        let left: i64 = db.query_row("SELECT COUNT(*) FROM logged", [], |row| row.get(0)).unwrap();
        assert_eq!((left, edits), (1, 0));
    }

    #[test]
    fn sizes_read_and_show_in_mb_or_gb() {
        assert_eq!(size_text(0), "No limit");
        assert_eq!(size_text(20), "20 MB");
        assert_eq!(size_text(1024), "1 GB");
        assert_eq!(size_text(1536), "1.5 GB");
        assert_eq!(size_text(20 * 1024), "20 GB");
        assert_eq!(parse_size("2 GB"), Some(2048));
        assert_eq!(parse_size("1.5gb"), Some(1536));
        assert_eq!(parse_size("500"), Some(500));
        assert_eq!(parse_size("500 MB"), Some(500));
        assert_eq!(parse_size("No limit"), Some(0));
        assert_eq!(parse_size("lots"), None);
        let defaults = LoggerPrefs::default();
        assert_eq!((defaults.week_cap_mb, defaults.month_cap_mb, defaults.year_cap_mb), (1024, 2048, 20480));
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
        let message = deleted_message(&json, &media, Vec::new()).unwrap();
        assert!(message.deleted);
        assert_eq!(message.attachments[0].url, "file:///logs/5/9-cat.png");
    }
}
