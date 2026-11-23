use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use kc_core::super_properties::SuperProperties;
use kc_core::{Snowflake, Token};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use wreq::ws::message::{CloseFrame, Message};
use tracing::{debug, error, info, warn};

use crate::compress::ZlibStream;
use crate::{DEFAULT_GATEWAY, GATEWAY_VERSION};

/// Guild subscriptions (the web client's op 37; op 14 is deprecated).
const GUILD_SUBSCRIPTIONS_BULK: i64 = 37;
/// The heartbeat the web client sends: op 1 plus whether the user is there.
const QOS_HEARTBEAT: i64 = 40;
/// The version of the heartbeat's QoS payload the web client sends.
const QOS_VERSION: u64 = 31;

/// Discord closes a connection that sends more than this many messages in
/// [`SEND_WINDOW`] (close code 4008).
const SEND_LIMIT: usize = 120;
const SEND_WINDOW: Duration = Duration::from_secs(60);
/// Kept free of commands, so heartbeats always fit.
const HEARTBEAT_RESERVE: usize = 10;

/// What one connection has sent in the last minute.
#[derive(Default)]
struct SendBudget {
    sent: VecDeque<Instant>,
}

impl SendBudget {
    fn record(&mut self, now: Instant) {
        self.sent.push_back(now);
    }

    /// How long until another command may go, or `None` if it may now. Only
    /// `SEND_LIMIT - HEARTBEAT_RESERVE` of the window go to commands.
    fn command_wait(&mut self, now: Instant) -> Option<Duration> {
        while self
            .sent
            .front()
            .is_some_and(|sent| *sent + SEND_WINDOW <= now)
        {
            self.sent.pop_front();
        }
        let allowed = SEND_LIMIT - HEARTBEAT_RESERVE;
        if self.sent.len() < allowed {
            return None;
        }
        // The oldest sends must age out until one slot is free.
        let freeing = self.sent[self.sent.len() - allowed];
        Some((freeing + SEND_WINDOW).saturating_duration_since(now))
    }
}

/// Queues a command, folding it into one already waiting where only the
/// newest matters: one presence update, one voice state per server (or for
/// DMs), and one member-list subscription per server with the channels merged.
fn enqueue(pending: &mut VecDeque<Value>, payload: Value) {
    let op = payload["op"].as_i64();
    let guild = payload["d"]["guild_id"].clone();
    match op {
        Some(3) => {
            pending.retain(|queued| queued["op"].as_i64() != Some(3));
        }
        Some(4) => {
            pending.retain(|queued| {
                !(queued["op"].as_i64() == Some(4) && queued["d"]["guild_id"] == guild)
            });
        }
        Some(GUILD_SUBSCRIPTIONS_BULK) => {
            if let Some(queued) = pending
                .iter_mut()
                .find(|queued| queued["op"].as_i64() == Some(GUILD_SUBSCRIPTIONS_BULK))
            {
                merge_subscriptions(&mut queued["d"]["subscriptions"], &payload["d"]["subscriptions"]);
                return;
            }
        }
        _ => {}
    }
    pending.push_back(payload);
}

/// Folds guild subscriptions into ones already waiting: per guild, the
/// channel ranges merge and the newest feature flags win.
fn merge_subscriptions(into: &mut Value, from: &Value) {
    let (Some(into), Some(from)) = (into.as_object_mut(), from.as_object()) else {
        return;
    };
    for (guild, update) in from {
        let entry = into.entry(guild.clone()).or_insert_with(|| json!({}));
        let Some(update) = update.as_object() else {
            continue;
        };
        for (key, value) in update {
            match (entry.get_mut(key), key.as_str()) {
                (Some(Value::Object(channels)), "channels") => {
                    if let Some(new) = value.as_object() {
                        for (channel, ranges) in new {
                            channels.insert(channel.clone(), ranges.clone());
                        }
                    }
                }
                _ => {
                    entry[key] = value.clone();
                }
            }
        }
    }
}

/// Whether the user is at the app, for the heartbeat's QoS payload and
/// whether to send one at once when the user comes back.
#[derive(Default)]
struct Activity {
    focused: AtomicBool,
    rtc_connected: AtomicBool,
}

impl Activity {
    fn qos(&self) -> Value {
        let mut reasons = Vec::new();
        if self.focused.load(Ordering::Relaxed) {
            reasons.push("foregrounded");
        }
        if self.rtc_connected.load(Ordering::Relaxed) {
            reasons.push("rtc_connected");
        }
        json!({"active": !reasons.is_empty(), "ver": QOS_VERSION, "reasons": reasons})
    }

    fn active(&self) -> bool {
        self.focused.load(Ordering::Relaxed) || self.rtc_connected.load(Ordering::Relaxed)
    }
}

/// Events a gateway connection publishes to its consumer.
#[derive(Clone, Debug)]
pub enum GatewayEvent {
    /// The socket is up and HELLO has been processed.
    Hello { heartbeat_interval: u64 },
    /// A dispatch with `op: 0`.
    Dispatch { name: String, data: Value },
    /// Session resumed after a reconnect.
    Resumed,
    /// Discord asked us to reconnect (op 7).
    Reconnect,
    /// Session is not resumable (op 9).
    InvalidSession { resumable: bool },
    /// Discord closed the socket. When `fatal` (bad token, invalid
    /// intents, ...) the loop stops; otherwise it reconnects by itself.
    Closed {
        code: Option<u16>,
        reason: String,
        fatal: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Reconnect { resumable: bool },
    Fatal,
}

/// A single account's gateway connection.
#[derive(Clone)]
pub struct Gateway {
    token: Token,
    activity: Arc<Activity>,
}

struct Session {
    id: Option<String>,
    resume_url: Option<String>,
    seq: Option<u64>,
    /// READY's `qos_token`, handed back in the next identify.
    qos_token: Option<String>,
    /// Identifies sent so far; the first is a cold start.
    identifies: u32,
    /// The presence last sent (op 3), which a new identify repeats.
    presence: Option<Value>,
}

/// A handle for sending payloads (voice state, streams, presence) over the
/// gateway connection owned by the run loop.
#[derive(Clone)]
pub struct GatewayHandle {
    tx: mpsc::UnboundedSender<Value>,
    activity: Arc<Activity>,
}

impl GatewayHandle {
    /// A handle attached to no connection: what it sends arrives on the
    /// returned receiver. For tests of what would be sent.
    pub fn detached() -> (Self, mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                activity: Arc::default(),
            },
            rx,
        )
    }

    /// Whether a window of the app has focus. Becoming active sends a
    /// heartbeat at once, as the web client does.
    pub fn set_focused(&self, focused: bool) {
        self.set_activity(&self.activity.focused, focused);
    }

    /// Whether the account is in a call or voice channel.
    pub fn set_rtc_connected(&self, connected: bool) {
        self.set_activity(&self.activity.rtc_connected, connected);
    }

    fn set_activity(&self, flag: &AtomicBool, value: bool) {
        let was_active = self.activity.active();
        if flag.swap(value, Ordering::Relaxed) != value && !was_active && self.activity.active() {
            self.send(json!({"op": QOS_HEARTBEAT}));
        }
    }

    pub fn send(&self, payload: Value) {
        let _ = self.tx.send(payload);
    }

    pub fn update_voice_state(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Option<Snowflake>,
        self_mute: bool,
        self_deaf: bool,
        self_video: bool,
    ) {
        self.set_rtc_connected(channel_id.is_some());
        self.send(json!({
            "op": 4,
            "d": {
                "guild_id": guild_id,
                "channel_id": channel_id,
                "self_mute": self_mute,
                "self_deaf": self_deaf,
                "self_video": self_video,
                "flags": 2,
            }
        }));
    }

    /// Starts a Go Live stream. In a guild voice channel pass the guild id;
    /// in a DM or group-DM call pass `None`, which starts a `call` stream.
    pub fn stream_create(&self, guild_id: Option<Snowflake>, channel_id: Snowflake) {
        let data = match guild_id {
            Some(guild_id) => json!({
                "type": "guild",
                "guild_id": guild_id,
                "channel_id": channel_id,
                "preferred_region": null,
            }),
            None => json!({
                "type": "call",
                "channel_id": channel_id,
                "preferred_region": null,
            }),
        };
        self.send(json!({ "op": 18, "d": data }));
    }

    pub fn stream_delete(&self, stream_key: &str) {
        self.send(json!({"op": 19, "d": {"stream_key": stream_key}}));
    }

    pub fn stream_watch(&self, stream_key: &str) {
        self.send(json!({"op": 20, "d": {"stream_key": stream_key}}));
    }

    pub fn stream_pause(&self, stream_key: &str, paused: bool) {
        self.send(json!({"op": 22, "d": {"stream_key": stream_key, "paused": paused}}));
    }

    /// Announces the account's status for this session. `status` is
    /// `online`, `idle`, `dnd` or `invisible`; `custom` is the custom status
    /// text and optional unicode emoji. Limited to 5 updates per 20 seconds.
    /// Sends the account's presence: its status and, if set, its custom
    /// status -- text plus an emoji, given by name (Unicode, or a server
    /// emoji's name) and, for a server emoji, its id.
    pub fn update_presence(
        &self,
        status: &str,
        custom: Option<(&str, Option<&str>, Option<Snowflake>)>,
        afk: bool,
    ) {
        let activities = match custom {
            Some((text, emoji_name, emoji_id)) => {
                let mut activity = json!({
                    "type": 4,
                    "name": "Custom Status",
                    "state": text,
                });
                if emoji_name.is_some() || emoji_id.is_some() {
                    activity["emoji"] = json!({
                        "name": emoji_name.unwrap_or(""),
                        "id": emoji_id.map(|id| id.to_string()),
                    });
                }
                vec![activity]
            }
            None => Vec::new(),
        };
        self.send(json!({
            "op": 3,
            "d": { "since": 0, "activities": activities, "status": status, "afk": afk }
        }));
    }

    /// Asks for specific guild members (answered by GUILD_MEMBERS_CHUNK).
    /// The REST member endpoints are not safe for user accounts.
    pub fn request_members(&self, guild_id: Snowflake, user_ids: &[Snowflake]) {
        if user_ids.is_empty() {
            return;
        }
        let ids: Vec<String> = user_ids
            .iter()
            .take(100)
            .map(Snowflake::to_string)
            .collect();
        self.send(json!({
            "op": 8,
            "d": { "guild_id": [guild_id.to_string()], "user_ids": ids, "presences": true }
        }));
    }

    /// Searches guild members by name prefix, for @mention autocomplete.
    pub fn search_members(&self, guild_id: Snowflake, query: &str, limit: u8) {
        self.send(json!({
            "op": 8,
            "d": {
                "guild_id": [guild_id.to_string()],
                "query": query,
                "limit": limit.clamp(1, 100),
                "presences": true,
            }
        }));
    }

    /// Subscribes to a guild channel's member list. Discord user clients use
    /// lazy guild subscriptions; without this, existing members (and their
    /// voice states) in a channel are not delivered.
    pub fn subscribe_channel(&self, guild_id: Snowflake, channel_id: Snowflake) {
        let subscription = json!({
            "typing": true,
            "threads": true,
            "activities": true,
            "channels": { channel_id.to_string(): [[0, 99], [100, 199]] },
        });
        self.send(json!({
            "op": GUILD_SUBSCRIPTIONS_BULK,
            "d": { "subscriptions": { guild_id.to_string(): subscription } }
        }));
    }
}

impl Gateway {
    pub fn new(token: Token) -> Self {
        Self {
            token,
            activity: Arc::default(),
        }
    }

    /// Starts the connection on a Tokio task and returns the command handle
    /// plus the event stream.
    pub fn start(
        self,
    ) -> (
        GatewayHandle,
        mpsc::UnboundedReceiver<GatewayEvent>,
        tokio::task::JoinHandle<()>,
    ) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let activity = self.activity.clone();
        let task = tokio::spawn(self.run_with(event_tx, command_rx));
        (
            GatewayHandle {
                tx: command_tx,
                activity,
            },
            event_rx,
            task,
        )
    }

    /// Connects and publishes events until the connection ends. Reconnects with
    /// backoff internally; returns only on a fatal close.
    pub async fn run(self, events: mpsc::UnboundedSender<GatewayEvent>) {
        let (_command_tx, command_rx) = mpsc::unbounded_channel();
        self.run_with(events, command_rx).await;
    }

    async fn run_with(
        self,
        events: mpsc::UnboundedSender<GatewayEvent>,
        mut commands: mpsc::UnboundedReceiver<Value>,
    ) {
        let mut session = Session {
            id: None,
            resume_url: None,
            seq: None,
            qos_token: None,
            identifies: 0,
            presence: None,
        };
        let mut backoff_ms = 1_000u64;

        loop {
            let mut established = false;
            let outcome = self
                .connect_once(&mut session, &events, &mut commands, &mut established)
                .await;
            // A connection that got as far as READY or RESUMED was healthy:
            // the next drop starts the backoff over.
            if established {
                backoff_ms = 1_000;
            }
            match outcome {
                Outcome::Fatal => {
                    info!("gateway stopped after fatal close");
                    return;
                }
                Outcome::Reconnect { resumable } => {
                    if !resumable {
                        session.id = None;
                        session.resume_url = None;
                        session.seq = None;
                    }
                    warn!(backoff_ms, resumable, "gateway reconnecting");
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(30_000);
                }
            }
        }
    }

    async fn connect_once(
        &self,
        session: &mut Session,
        events: &mpsc::UnboundedSender<GatewayEvent>,
        commands: &mut mpsc::UnboundedReceiver<Value>,
        established: &mut bool,
    ) -> Outcome {
        let url = match (&session.id, &session.resume_url) {
            (Some(_), Some(resume)) => gateway_url(resume),
            _ => gateway_url(DEFAULT_GATEWAY),
        };
        debug!(url, "gateway connecting");

        let stream = match kc_http::browser::websocket(&url).await {
            Ok(connection) => connection,
            Err(error) => {
                warn!(%error, "gateway connect failed");
                return Outcome::Reconnect { resumable: true };
            }
        };
        let (mut write, mut read) = stream.split();
        let mut inflate = ZlibStream::new();

        // HELLO must arrive first; wait for it before starting the heartbeat.
        let hello = loop {
            match read.next().await {
                Some(Ok(Message::Binary(bytes))) => {
                    if let Ok(Some(text)) = inflate.feed(&bytes)
                        && let Ok(value) = serde_json::from_str::<Value>(&text)
                        && value["op"].as_i64() == Some(10)
                    {
                        break value["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250);
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&text)
                        && value["op"].as_i64() == Some(10)
                    {
                        break value["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250);
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    let (code, reason) = close_parts(frame);
                    return fatal_or_reconnect(code, reason);
                }
                Some(Ok(_)) => continue,
                Some(Err(error)) => {
                    warn!(%error, "gateway read failed before HELLO");
                    return Outcome::Reconnect { resumable: true };
                }
                None => return Outcome::Reconnect { resumable: true },
            }
        };

        let _ = events.send(GatewayEvent::Hello {
            heartbeat_interval: hello,
        });

        if let Some(_session_id) = &session.id {
            info!(seq = ?session.seq, "gateway resuming the session");
            let payload = json!({
                "op": 6,
                "d": {
                    "token": self.token.expose(),
                    "session_id": session.id,
                    "seq": session.seq,
                }
            });
            if let Err(error) = write.send(Message::Text(payload.to_string().into())).await {
                warn!(%error, "gateway resume send failed");
                return Outcome::Reconnect { resumable: true };
            }
        } else {
            info!("gateway identifying (new session)");
            // Built now, so it carries the current build number and focus.
            let payload = identify_payload(&self.token, &SuperProperties::new(), &self.activity, session);
            session.identifies += 1;
            if let Err(error) = write.send(Message::Text(payload.into())).await {
                warn!(%error, "gateway identify send failed");
                return Outcome::Reconnect { resumable: true };
            }
        }

        let mut heartbeat = tokio::time::interval(Duration::from_millis(hello));
        // Discord wants a jittered first heartbeat; skipping the immediate tick
        // is the simplest way to stay inside the allowed window.
        heartbeat.tick().await;
        let mut awaiting_ack = false;
        // Discord's send limit, per connection: the identify/resume counts.
        let mut budget = SendBudget::default();
        budget.record(Instant::now());
        // Commands over the budget wait here instead of risking a 4008 close.
        let mut pending: VecDeque<Value> = VecDeque::new();

        loop {
            // Send what the budget allows; a timer wakes for the rest.
            while !pending.is_empty() && budget.command_wait(Instant::now()).is_none() {
                let Some(payload) = pending.pop_front() else {
                    break;
                };
                budget.record(Instant::now());
                if payload["op"].as_i64() == Some(3) {
                    session.presence = Some(payload["d"].clone());
                }
                if let Err(error) = write.send(Message::Text(payload.to_string().into())).await {
                    warn!(%error, "gateway command send failed");
                    return Outcome::Reconnect { resumable: true };
                }
            }
            let throttled = if pending.is_empty() {
                None
            } else {
                budget.command_wait(Instant::now())
            };
            if let Some(wait) = throttled {
                debug!(queued = pending.len(), ?wait, "gateway send budget spent; holding commands");
            }
            tokio::select! {
                command = commands.recv(), if !commands.is_closed() => {
                    let Some(payload) = command else {
                        continue;
                    };
                    // The user came back: tell Discord now, not at the next beat.
                    if payload["op"].as_i64() == Some(QOS_HEARTBEAT) {
                        if !*established {
                            continue;
                        }
                        let payload = heartbeat_payload(session.seq, &self.activity);
                        budget.record(Instant::now());
                        if write.send(Message::Text(payload.to_string().into())).await.is_err() {
                            return Outcome::Reconnect { resumable: true };
                        }
                        awaiting_ack = true;
                        continue;
                    }
                    enqueue(&mut pending, payload);
                }
                _ = tokio::time::sleep(throttled.unwrap_or(Duration::ZERO)), if throttled.is_some() => {}
                _ = heartbeat.tick() => {
                    if awaiting_ack {
                        warn!("gateway heartbeat not acknowledged; reconnecting");
                        return Outcome::Reconnect { resumable: true };
                    }
                    let payload = heartbeat_payload(session.seq, &self.activity);
                    budget.record(Instant::now());
                    if let Err(error) = write.send(Message::Text(payload.to_string().into())).await {
                        warn!(%error, "gateway heartbeat send failed");
                        return Outcome::Reconnect { resumable: true };
                    }
                    awaiting_ack = true;
                }
                message = read.next() => {
                    let Some(message) = message else {
                        warn!("gateway connection ended without a close");
                        return Outcome::Reconnect { resumable: true };
                    };
                    let message = match message {
                        Ok(message) => message,
                        Err(error) => {
                            warn!(%error, "gateway read failed");
                            return Outcome::Reconnect { resumable: true };
                        }
                    };
                    let message = match message {
                        Message::Binary(bytes) => match inflate.feed(&bytes) {
                            Ok(Some(text)) => Message::Text(text.into()),
                            Ok(None) => continue,
                            Err(error) => {
                                warn!(%error, "gateway sent a frame that does not inflate");
                                return Outcome::Reconnect { resumable: false };
                            }
                        },
                        other => other,
                    };
                    match message {
                        Message::Text(text) => {
                            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                                debug!("gateway sent non-JSON text frame");
                                continue;
                            };
                            let op = value["op"].as_i64().unwrap_or(-1);
                            match op {
                                0 => {
                                    if let Some(seq) = value["s"].as_u64() {
                                        session.seq = Some(seq);
                                    }
                                    let name = value["t"].as_str().unwrap_or("UNKNOWN").to_owned();
                                    let data = value["d"].clone();
                                    if name == "READY" {
                                        session.id = data["session_id"].as_str().map(str::to_owned);
                                        session.resume_url = data["resume_gateway_url"]
                                            .as_str()
                                            .map(str::to_owned);
                                        session.qos_token = data["qos_token"].as_str().map(str::to_owned);
                                        *established = true;
                                        info!(
                                            user = data.pointer("/user/username").and_then(|value| value.as_str()).unwrap_or("?"),
                                            "gateway ready"
                                        );
                                    }
                                    if name == "RESUMED" {
                                        *established = true;
                                        info!(seq = ?session.seq, "gateway resumed");
                                        let _ = events.send(GatewayEvent::Resumed);
                                    }
                                    dump_dispatch(&name, &data);
                                    let _ = events.send(GatewayEvent::Dispatch {
                                        name,
                                        data,
                                    });
                                }
                                1 => {
                                    let payload = heartbeat_payload(session.seq, &self.activity);
                                    budget.record(Instant::now());
                                    if write.send(Message::Text(payload.to_string().into())).await.is_err() {
                                        return Outcome::Reconnect { resumable: true };
                                    }
                                    awaiting_ack = true;
                                }
                                7 => {
                                    info!("gateway asked us to reconnect");
                                    let _ = events.send(GatewayEvent::Reconnect);
                                    return Outcome::Reconnect { resumable: true };
                                }
                                9 => {
                                    let resumable = value["d"].as_bool().unwrap_or(false);
                                    warn!(resumable, "gateway invalidated the session");
                                    let _ = events.send(GatewayEvent::InvalidSession { resumable });
                                    return Outcome::Reconnect { resumable };
                                }
                                10 => {}
                                11 => {
                                    awaiting_ack = false;
                                }
                                _ => {}
                            }
                        }
                        Message::Binary(_) => {}
                        Message::Close(frame) => {
                            let (code, reason) = close_parts(frame);
                            error!(?code, reason, "gateway closed");
                            let outcome = fatal_or_reconnect(code, reason.clone());
                            let _ = events.send(GatewayEvent::Closed {
                                code,
                                reason,
                                fatal: matches!(outcome, Outcome::Fatal),
                            });
                            return outcome;
                        }
                        Message::Ping(payload) => {
                            let _ = write.send(Message::Pong(payload)).await;
                        }
                        Message::Pong(_) => {}
                    }
                }
            }
        }
    }
}

/// Development aid: with `KUBECORD_DUMP_DISPATCH=<dir>` every dispatch is
/// written to `<dir>/<EVENT>.json` (the latest payload per event name), so
/// reducers can be written against real payload shapes. The files contain
/// account data; the directory is the caller's responsibility.
fn dump_dispatch(name: &str, data: &Value) {
    let Some(directory) = std::env::var_os("KUBECORD_DUMP_DISPATCH") else {
        return;
    };
    let path = std::path::Path::new(&directory).join(format!("{name}.json"));
    if let Ok(bytes) = serde_json::to_vec_pretty(data) {
        let _ = std::fs::write(path, bytes);
    }
}

/// The gateway URL, with the query the web client sends: JSON over one
/// zlib stream.
fn gateway_url(base: &str) -> String {
    format!(
        "{}/?encoding=json&v={GATEWAY_VERSION}&compress=zlib-stream",
        base.trim_end_matches('/')
    )
}

/// A heartbeat as the web client sends it: op 40, with whether the user is
/// at the app.
fn heartbeat_payload(seq: Option<u64>, activity: &Activity) -> Value {
    json!({"op": QOS_HEARTBEAT, "d": {"seq": seq, "qos": activity.qos()}})
}

/// The identify the web client sends, field for field and in its order: its
/// capabilities (which shape READY and some events), its properties plus the
/// connection's, and no cached client state.
fn identify_payload(
    token: &Token,
    properties: &SuperProperties,
    activity: &Activity,
    session: &Session,
) -> String {
    #[derive(serde::Serialize)]
    struct Properties<'a> {
        #[serde(flatten)]
        base: &'a SuperProperties,
        is_fast_connect: bool,
        gateway_connect_reasons: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        installation_id: Option<String>,
    }
    let properties = Properties {
        base: properties,
        is_fast_connect: false,
        gateway_connect_reasons: if session.identifies == 0 {
            "COLD_START"
        } else {
            "NO_REASONS"
        },
        installation_id: kc_core::super_properties::installation_id(),
    };
    let payload = json!({
        "op": 2,
        "d": {
            "token": token.expose(),
            "capabilities": kc_core::super_properties::CAPABILITIES,
            "properties": properties,
            // Until the account's status is known the web client says
            // "unknown"; after that, what it last set.
            "presence": session.presence.clone().unwrap_or_else(|| json!({
                "status": "unknown",
                "since": 0,
                "activities": [],
                "afk": false
            })),
            "compress": false,
            "client_state": { "guild_versions": {} },
            "qos_token": qos_token(activity.active(), session.qos_token.as_deref()),
        }
    });
    payload.to_string()
}

/// The identify's `qos_token`: base64 of a `QosToken` protobuf holding
/// whether the user is active (field 1) and the token READY last gave
/// (field 2, its bytes as sent).
fn qos_token(active: bool, derived: Option<&str>) -> String {
    use base64::Engine;
    let engine = &base64::engine::general_purpose::STANDARD;
    let mut bytes = vec![0x0a];
    if active {
        bytes.extend([0x02, 0x08, 0x01]);
    } else {
        bytes.push(0x00);
    }
    if let Some(derived) = derived.and_then(|token| engine.decode(token).ok())
        && !derived.is_empty()
    {
        bytes.push(0x12);
        let mut length = derived.len();
        loop {
            let byte = (length & 0x7f) as u8;
            length >>= 7;
            if length == 0 {
                bytes.push(byte);
                break;
            }
            bytes.push(byte | 0x80);
        }
        bytes.extend(derived);
    }
    engine.encode(bytes)
}

fn close_parts(frame: Option<CloseFrame>) -> (Option<u16>, String) {
    match frame {
        Some(frame) => (Some(frame.code.into()), frame.reason.to_string()),
        None => (None, String::new()),
    }
}

fn fatal_or_reconnect(code: Option<u16>, reason: String) -> Outcome {
    match code {
        // Authentication failed, invalid shard, invalid intents,
        // disallowed intents, invalid API version.
        Some(4004 | 4010 | 4011 | 4013 | 4014) => Outcome::Fatal,
        _ => {
            warn!(?code, reason, "gateway connection closed; reconnecting");
            Outcome::Reconnect { resumable: true }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_stop_short_of_the_limit_leaving_room_for_heartbeats() {
        let mut budget = SendBudget::default();
        let start = Instant::now();
        for _ in 0..SEND_LIMIT - HEARTBEAT_RESERVE - 1 {
            budget.record(start);
        }
        assert_eq!(budget.command_wait(start), None, "one command slot left");
        budget.record(start);
        let wait = budget.command_wait(start).expect("commands must wait now");
        assert!(wait <= SEND_WINDOW && wait > SEND_WINDOW - Duration::from_secs(1));
        // Once the window has passed, the old sends age out.
        assert_eq!(budget.command_wait(start + SEND_WINDOW), None);
    }

    #[test]
    fn queued_commands_fold_where_only_the_newest_matters() {
        let mut pending = VecDeque::new();
        let subscribe = |guild: &str, channel: &str| {
            json!({"op": 37, "d": {"subscriptions": {guild: {"typing": true, "channels": {channel: [[0, 99]]}}}}})
        };
        enqueue(&mut pending, subscribe("1", "10"));
        enqueue(&mut pending, subscribe("1", "11"));
        enqueue(&mut pending, subscribe("2", "20"));
        enqueue(&mut pending, json!({"op": 37, "d": {"subscriptions": {"1": {"typing": false}}}}));
        enqueue(&mut pending, json!({"op": 3, "d": {"status": "idle"}}));
        enqueue(&mut pending, json!({"op": 3, "d": {"status": "online"}}));
        enqueue(&mut pending, json!({"op": 4, "d": {"guild_id": "1", "channel_id": "5"}}));
        enqueue(&mut pending, json!({"op": 4, "d": {"guild_id": "1", "channel_id": null}}));
        // One bulk subscription, channels merged per server; the newest
        // presence and voice state only.
        assert_eq!(pending.len(), 3, "{pending:?}");
        let subscriptions = &pending.iter().find(|p| p["op"] == 37).unwrap()["d"]["subscriptions"];
        assert!(subscriptions["1"]["channels"]["10"].is_array() && subscriptions["1"]["channels"]["11"].is_array());
        assert_eq!(subscriptions["1"]["typing"], false);
        assert!(subscriptions["2"]["channels"]["20"].is_array());
        assert_eq!(pending.iter().find(|p| p["op"] == 3).unwrap()["d"]["status"], "online");
        assert!(pending.iter().find(|p| p["op"] == 4).unwrap()["d"]["channel_id"].is_null());
    }

    fn session() -> Session {
        Session {
            id: None,
            resume_url: None,
            seq: None,
            qos_token: None,
            identifies: 0,
            presence: None,
        }
    }

    #[test]
    fn identify_matches_the_web_client() {
        let token = Token::new("abc");
        let activity = Activity::default();
        activity.focused.store(true, Ordering::Relaxed);
        let text = identify_payload(&token, &SuperProperties::new(), &activity, &session());
        // Field order follows the web client's.
        let order: Vec<_> = ["\"token\"", "\"capabilities\"", "\"properties\"", "\"presence\"", "\"compress\"", "\"client_state\"", "\"qos_token\""]
            .iter()
            .map(|key| text.find(key).unwrap())
            .collect();
        assert!(order.is_sorted(), "{text}");
        assert!(text.starts_with(r#"{"op":2,"d":{"token":"abc""#), "{text}");
        let payload: Value = serde_json::from_str(&text).unwrap();
        let d = &payload["d"];
        assert_eq!(d["capabilities"], 1_767_421);
        assert_eq!(d["compress"], false);
        assert!(d.get("intents").is_none());
        assert_eq!(d["properties"]["browser"], "Chrome");
        assert_eq!(d["properties"]["client_app_state"], "focused");
        assert_eq!(d["properties"]["is_fast_connect"], false);
        assert_eq!(d["properties"]["gateway_connect_reasons"], "COLD_START");
        assert_eq!(d["client_state"], json!({"guild_versions": {}}));
        assert_eq!(d["qos_token"], "CgIIAQ==");
        assert_eq!(d["presence"]["status"], "unknown");

        let mut later = session();
        later.identifies = 1;
        later.qos_token = Some("AQID".into());
        later.presence = Some(json!({"status": "dnd", "since": 0, "activities": [], "afk": false}));
        let mut unfocused = SuperProperties::new();
        unfocused.client_app_state = "unfocused";
        let payload: Value =
            serde_json::from_str(&identify_payload(&token, &unfocused, &Activity::default(), &later)).unwrap();
        assert_eq!(payload["d"]["properties"]["client_app_state"], "unfocused");
        // Once only: the properties do not repeat a key.
        assert_eq!(
            identify_payload(&token, &unfocused, &Activity::default(), &later).matches("client_app_state").count(),
            1
        );
        assert_eq!(payload["d"]["properties"]["gateway_connect_reasons"], "NO_REASONS");
        assert_eq!(payload["d"]["presence"]["status"], "dnd");
        use base64::Engine;
        let token = base64::engine::general_purpose::STANDARD
            .decode(payload["d"]["qos_token"].as_str().unwrap())
            .unwrap();
        assert_eq!(token, [0x0a, 0x00, 0x12, 0x03, 1, 2, 3]);
    }

    #[test]
    fn heartbeats_carry_whether_the_user_is_there() {
        let activity = Activity::default();
        assert_eq!(
            heartbeat_payload(Some(7), &activity),
            json!({"op": 40, "d": {"seq": 7, "qos": {"active": false, "ver": 31, "reasons": []}}})
        );
        activity.focused.store(true, Ordering::Relaxed);
        activity.rtc_connected.store(true, Ordering::Relaxed);
        assert_eq!(
            heartbeat_payload(None, &activity)["d"]["qos"]["reasons"],
            json!(["foregrounded", "rtc_connected"])
        );
        assert_eq!(
            gateway_url("wss://gateway.discord.gg/"),
            "wss://gateway.discord.gg/?encoding=json&v=9&compress=zlib-stream"
        );
    }

    #[test]
    fn coming_back_sends_a_heartbeat_at_once() {
        let (handle, mut sent) = GatewayHandle::detached();
        handle.set_focused(true);
        assert_eq!(sent.try_recv().unwrap()["op"], 40);
        handle.set_rtc_connected(true);
        handle.set_focused(false);
        assert!(sent.try_recv().is_err(), "still active: nothing to say");
    }

    #[test]
    fn fatal_close_codes() {
        assert_eq!(
            fatal_or_reconnect(Some(4004), String::new()),
            Outcome::Fatal
        );
        assert!(matches!(
            fatal_or_reconnect(Some(1000), String::new()),
            Outcome::Reconnect { resumable: true }
        ));
    }
}
