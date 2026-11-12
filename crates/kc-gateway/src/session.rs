use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use kc_core::super_properties::SuperProperties;
use kc_core::Token;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::{DEFAULT_GATEWAY, GATEWAY_VERSION};

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
    /// Fatal close (bad token, invalid intents, ...). The loop stops.
    Closed { code: Option<u16>, reason: String },
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

impl Gateway {
    pub fn new(token: Token) -> Self {
        Self {
            token,
            properties: SuperProperties::new(),
        }
    }

    /// Connects and publishes events until the connection ends. Reconnects with
    /// backoff internally; returns only on a fatal close.
    pub async fn run(self, events: mpsc::UnboundedSender<GatewayEvent>) {
        let mut session = Session {
            id: None,
            resume_url: None,
            seq: None,
        };
        let mut backoff_ms = 1_000u64;

        loop {
            let outcome = self.connect_once(&mut session, &events).await;
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
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        if value["op"].as_i64() == Some(10) {
                            break value["d"]["heartbeat_interval"]
                                .as_u64()
                                .unwrap_or(41_250);
                        }
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

        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    if awaiting_ack {
                        warn!("gateway heartbeat not acknowledged; reconnecting");
                        return Outcome::Reconnect { resumable: true };
                    }
                    let payload = json!({ "op": 1, "d": session.seq });
                    if let Err(error) = write.send(Message::Text(payload.to_string().into())).await {
                        warn!(%error, "gateway heartbeat send failed");
                        return Outcome::Reconnect { resumable: true };
                    }
                    awaiting_ack = true;
                }
                message = read.next() => {
                    let Some(message) = message else {
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
                                        info!(
                                            user = data.pointer("/user/username").and_then(|value| value.as_str()).unwrap_or("?"),
                                            "gateway ready"
                                        );
                                    }
                                    let _ = events.send(GatewayEvent::Dispatch {
                                        name,
                                        data,
                                    });
                                }
                                1 => {
                                    let payload = json!({ "op": 1, "d": session.seq });
                                    if write.send(Message::Text(payload.to_string().into())).await.is_err() {
                                        return Outcome::Reconnect { resumable: true };
                                    }
                                    awaiting_ack = true;
                                }
                                7 => {
                                    let _ = events.send(GatewayEvent::Reconnect);
                                    return Outcome::Reconnect { resumable: true };
                                }
                                9 => {
                                    let resumable = value["d"].as_bool().unwrap_or(false);
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
                            let _ = events.send(GatewayEvent::Closed {
                                code,
                                reason: reason.clone(),
                            });
                            return fatal_or_reconnect(code, reason);
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

fn identify_payload(token: &Token, properties: &SuperProperties) -> Value {
    json!({
        "op": 2,
        "d": {
            "token": token.expose(),
            "capabilities": 0,
            "properties": properties,
            "compress": false,
            "client_state": {
                "guild_versions": {}
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

fn close_parts(frame: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>) -> (Option<u16>, String) {
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
            debug!(?code, reason, "gateway connection ended; reconnecting");
            Outcome::Reconnect { resumable: true }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identify_has_user_client_shape() {
        let token = Token::new("abc");
        let payload = identify_payload(&token, &SuperProperties::new());
        assert_eq!(payload["op"], 2);
        assert_eq!(payload["d"]["token"], "abc");
        assert_eq!(payload["d"]["compress"], false);
        assert_eq!(payload["d"]["properties"]["browser"], "Discord Client");
        assert!(payload["d"].get("intents").is_none());
        assert_eq!(payload["d"]["client_state"]["guild_versions"], json!({}));
    }

    #[test]
    fn fatal_close_codes() {
        assert_eq!(fatal_or_reconnect(Some(4004), String::new()), Outcome::Fatal);
        assert!(matches!(
            fatal_or_reconnect(Some(1000), String::new()),
            Outcome::Reconnect { resumable: true }
        ));
    }
}