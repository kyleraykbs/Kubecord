use std::collections::VecDeque;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use kc_core::super_properties::SuperProperties;
use kc_core::{Snowflake, Token};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::{DEFAULT_GATEWAY, GATEWAY_VERSION};

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
        Some(14) => {
            if let Some(queued) = pending
                .iter_mut()
                .find(|queued| queued["op"].as_i64() == Some(14) && queued["d"]["guild_id"] == guild)
            {
                let mut merged = queued.clone();
                if let (Some(into), Some(from)) = (
                    merged["d"]["channels"].as_object_mut(),
                    payload["d"]["channels"].as_object(),
                ) {
                    for (channel, ranges) in from {
                        into.insert(channel.clone(), ranges.clone());
                    }
                }
                for key in ["typing", "activities", "threads"] {
                    if !payload["d"][key].is_null() {
                        merged["d"][key] = payload["d"][key].clone();
                    }
                }
                *queued = merged;
                return;
            }
        }
        _ => {}
    }
    pending.push_back(payload);
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
    properties: SuperProperties,
}

struct Session {
    id: Option<String>,
    resume_url: Option<String>,
    seq: Option<u64>,
}

/// A handle for sending payloads (voice state, streams, presence) over the
/// gateway connection owned by the run loop.
#[derive(Clone)]
pub struct GatewayHandle {
    tx: mpsc::UnboundedSender<Value>,
}

impl GatewayHandle {
    /// A handle attached to no connection: what it sends arrives on the
    /// returned receiver. For tests of what would be sent.
    pub fn detached() -> (Self, mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
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
    pub fn update_presence(&self, status: &str, custom: Option<(&str, Option<&str>)>, afk: bool) {
        let activities = match custom {
            Some((text, emoji)) => {
                let mut activity = json!({
                    "type": 4,
                    "name": "Custom Status",
                    "state": text,
                });
                if let Some(emoji) = emoji {
                    activity["emoji"] = json!({ "name": emoji });
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
        let mut channels = serde_json::Map::new();
        channels.insert(channel_id.to_string(), json!([[0, 99], [100, 199]]));
        self.send(json!({
            "op": 14,
            "d": {
                "guild_id": guild_id,
                "channels": channels,
                "typing": true,
                "activities": true,
                "threads": true,
            }
        }));
    }
}

impl Gateway {
    pub fn new(token: Token) -> Self {
        Self {
            token,
            properties: SuperProperties::new(),
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
        let task = tokio::spawn(self.run_with(event_tx, command_rx));
        (GatewayHandle { tx: command_tx }, event_rx, task)
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
            (Some(_), Some(resume)) => format!(
                "{}/?v={GATEWAY_VERSION}&encoding=json",
                resume.trim_end_matches('/')
            ),
            _ => format!("{DEFAULT_GATEWAY}/?v={GATEWAY_VERSION}&encoding=json"),
        };
        debug!(url, "gateway connecting");

        let (stream, _) = match tokio_tungstenite::connect_async(&url).await {
            Ok(connection) => connection,
            Err(error) => {
                warn!(%error, "gateway connect failed");
                return Outcome::Reconnect { resumable: true };
            }
        };
        let (mut write, mut read) = stream.split();

        // HELLO must arrive first; wait for it before starting the heartbeat.
        let hello = loop {
            match read.next().await {
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
            let payload = identify_payload(&self.token, &self.properties);
            if let Err(error) = write.send(Message::Text(payload.to_string().into())).await {
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
                    enqueue(&mut pending, payload);
                }
                _ = tokio::time::sleep(throttled.unwrap_or(Duration::ZERO)), if throttled.is_some() => {}
                _ = heartbeat.tick() => {
                    if awaiting_ack {
                        warn!("gateway heartbeat not acknowledged; reconnecting");
                        return Outcome::Reconnect { resumable: true };
                    }
                    let payload = json!({ "op": 1, "d": session.seq });
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
                                    let payload = json!({ "op": 1, "d": session.seq });
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
                        Message::Binary(_) => {
                            // zlib-stream frames are only sent when the
                            // connection advertised compression; we do not.
                        }
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
                        Message::Pong(_) | Message::Frame(_) => {}
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

fn identify_payload(token: &Token, properties: &SuperProperties) -> Value {
    json!({
        "op": 2,
        "d": {
            "token": token.expose(),
            "capabilities": kc_core::super_properties::CAPABILITIES,
            "properties": properties,
            "compress": false,
            "client_state": {
                "guild_hashes": {},
                "highest_last_message_id": "0",
                "read_state_version": 0,
                "user_guild_settings_version": -1,
                "user_settings_version": -1
            },
            "presence": {
                "status": "online",
                "since": 0,
                "afk": false,
                "activities": []
            }
        }
    })
}

fn close_parts(
    frame: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>,
) -> (Option<u16>, String) {
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
        let subscribe = |channel: &str| {
            json!({"op": 14, "d": {"guild_id": "1", "channels": {channel: [[0, 99]]}, "typing": true}})
        };
        enqueue(&mut pending, subscribe("10"));
        enqueue(&mut pending, subscribe("11"));
        enqueue(&mut pending, json!({"op": 14, "d": {"guild_id": "2", "channels": {"20": [[0, 99]]}}}));
        enqueue(&mut pending, json!({"op": 3, "d": {"status": "idle"}}));
        enqueue(&mut pending, json!({"op": 3, "d": {"status": "online"}}));
        enqueue(&mut pending, json!({"op": 4, "d": {"guild_id": "1", "channel_id": "5"}}));
        enqueue(&mut pending, json!({"op": 4, "d": {"guild_id": "1", "channel_id": null}}));
        // One subscription per server, channels merged; the newest presence
        // and voice state only.
        assert_eq!(pending.len(), 4, "{pending:?}");
        let merged = pending.iter().find(|p| p["d"]["guild_id"] == "1" && p["op"] == 14).unwrap();
        assert!(merged["d"]["channels"]["10"].is_array() && merged["d"]["channels"]["11"].is_array());
        assert_eq!(pending.iter().find(|p| p["op"] == 3).unwrap()["d"]["status"], "online");
        assert!(pending.iter().find(|p| p["op"] == 4).unwrap()["d"]["channel_id"].is_null());
    }

    #[test]
    fn identify_has_user_client_shape() {
        let token = Token::new("abc");
        let payload = identify_payload(&token, &SuperProperties::new());
        assert_eq!(payload["op"], 2);
        assert_eq!(payload["d"]["token"], "abc");
        assert_eq!(payload["d"]["compress"], false);
        assert_eq!(payload["d"]["properties"]["browser"], "Chrome");
        assert!(payload["d"].get("intents").is_none());
        assert_eq!(payload["d"]["capabilities"], 4605);
        assert_eq!(payload["d"]["client_state"]["guild_hashes"], json!({}));
        assert_eq!(payload["d"]["client_state"]["highest_last_message_id"], "0");
        assert_eq!(payload["d"]["client_state"]["user_settings_version"], -1);
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
