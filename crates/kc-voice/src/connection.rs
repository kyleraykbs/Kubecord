//! Voice gateway, UDP, SRTP, RTP and DAVE state machine.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::sink::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use kc_core::Snowflake;
use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::dave::Dave;
use crate::error::{Error, Result};
use crate::rtp::{self, H264Depacketizer, MAX_PAYLOAD, PT_H264, PT_OPUS, RtpHeader};
use crate::srtp::{Mode, Srtp};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type WsWrite = SplitSink<WsStream, Message>;
type WsRead = SplitStream<WsStream>;

const VOICE_VERSION: u8 = 9;

/// Everything needed to open one voice or stream connection.
#[derive(Clone, Debug)]
pub struct VoiceParams {
    /// Guild id for a guild voice channel, channel id for a DM call, or the
    /// stream's `rtc_server_id` for a Go Live connection.
    pub server_id: Snowflake,
    pub channel_id: Snowflake,
    pub user_id: Snowflake,
    pub session_id: String,
    pub token: String,
    pub endpoint: String,
    /// Offer a video stream (camera for a voice connection, screen for a
    /// stream connection).
    pub video: bool,
    /// Stream connections identify the offered stream as `screen`.
    pub screen: bool,
    /// DAVE group id: channel id for voice, `rtc_server_id - 1` for streams.
    pub dave_channel_id: Snowflake,
}

#[derive(Clone, Debug)]
pub enum VoiceEvent {
    Ready {
        ssrc: u32,
        video_ssrc: Option<u32>,
        rtx_ssrc: Option<u32>,
        mode: String,
        dave_version: u16,
    },
    Audio {
        user_id: Snowflake,
        ssrc: u32,
        opus: Vec<u8>,
        sequence: u16,
        timestamp: u32,
    },
    Video {
        user_id: Snowflake,
        ssrc: u32,
        frame: Vec<u8>,
        keyframe: bool,
    },
    Speaking {
        user_id: Snowflake,
        ssrc: u32,
        speaking: bool,
    },
    ClientConnect {
        user_id: Snowflake,
    },
    ClientDisconnect {
        user_id: Snowflake,
    },
    DaveReady,
    KeyframeRequest {
        user_id: Snowflake,
        ssrc: u32,
    },
    Closed {
        code: Option<u16>,
        reason: String,
    },
    Error(String),
}

enum Command {
    SendAudio { opus: Vec<u8>, frame_ms: u32 },
    SendVideo { frame: Vec<u8>, frame_ms: u32 },
    SetSpeaking(u8),
    SetVideo(bool),
    Disconnect,
}

/// Command handle to a running voice connection.
#[derive(Clone)]
pub struct VoiceHandle {
    tx: mpsc::UnboundedSender<Command>,
}

impl VoiceHandle {
    pub fn send_audio(&self, opus: Vec<u8>, frame_ms: u32) {
        let _ = self.tx.send(Command::SendAudio { opus, frame_ms });
    }

    pub fn send_video(&self, frame: Vec<u8>, frame_ms: u32) {
        let _ = self.tx.send(Command::SendVideo { frame, frame_ms });
    }

    pub fn set_speaking(&self, flags: u8) {
        let _ = self.tx.send(Command::SetSpeaking(flags));
    }

    pub fn set_video(&self, enabled: bool) {
        let _ = self.tx.send(Command::SetVideo(enabled));
    }

    pub async fn disconnect(&self) {
        let _ = self.tx.send(Command::Disconnect);
    }
}

/// Opens a voice connection. Returns immediately; progress is reported through
/// `events`.
pub async fn connect(
    params: VoiceParams,
    events: mpsc::UnboundedSender<VoiceEvent>,
) -> Result<VoiceHandle> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        if let Err(error) = run(params, rx, events.clone()).await {
            warn!(%error, "voice connection ended");
            let _ = events.send(VoiceEvent::Error(error.to_string()));
            let _ = events.send(VoiceEvent::Closed {
                code: None,
                reason: error.to_string(),
            });
        }
    });
    Ok(VoiceHandle { tx })
}

struct SessionState {
    params: VoiceParams,
    write: WsWrite,
    read: WsRead,
    socket: UdpSocket,
    remote: SocketAddr,
    srtp: Srtp,
    audio_ssrc: u32,
    video_ssrc: Option<u32>,
    rtx_ssrc: Option<u32>,
    dave: Option<Dave>,
    users: HashSet<u64>,
    audio_ssrc_to_user: HashMap<u32, Snowflake>,
    video_ssrc_to_user: HashMap<u32, Snowflake>,
    depacketizers: HashMap<u32, H264Depacketizer>,
    seq_audio: u16,
    seq_video: u16,
    ts_audio: u32,
    ts_video: u32,
    speaking: u8,
    video_enabled: bool,
    last_binary_seq: u16,
    events: mpsc::UnboundedSender<VoiceEvent>,
}

async fn run(
    params: VoiceParams,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<VoiceEvent>,
) -> Result<()> {
    let host = params
        .endpoint
        .split(':')
        .next()
        .unwrap_or(&params.endpoint)
        .to_owned();
    let url = format!("wss://{host}/?v={VOICE_VERSION}&encoding=json");
    info!(%url, "voice connecting");

    let (stream, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(&url),
    )
    .await
    .map_err(|_| Error::Message("voice websocket connect timed out".into()))?
    .map_err(|error| Error::Message(format!("voice websocket: {error}")))?;
    let (mut write, mut read) = stream.split();

    let heartbeat = wait_for_hello(&mut read).await?;

    send_json(
        &mut write,
        json!({
            "op": 0,
            "d": {
                "server_id": params.server_id,
                "channel_id": params.channel_id,
                "user_id": params.user_id,
                "session_id": params.session_id,
                "token": params.token,
                "video": params.video,
                "streams": if params.video {
                    vec![json!({
                        "type": if params.screen { "screen" } else { "video" },
                        "rid": "100",
                        "quality": 100
                    })]
                } else {
                    Vec::new()
                },
                "max_dave_protocol_version": 1,
            }
        }),
    )
    .await?;

    let ready = wait_for_opcode(&mut read, 2).await?;
    let d = &ready["d"];
    let audio_ssrc = d["ssrc"]
        .as_u64()
        .ok_or_else(|| Error::Message("voice READY has no ssrc".into()))?
        as u32;
    let ip = d["ip"]
        .as_str()
        .ok_or_else(|| Error::Message("voice READY has no ip".into()))?
        .to_owned();
    let port = d["port"]
        .as_u64()
        .ok_or_else(|| Error::Message("voice READY has no port".into()))? as u16;
    let modes: Vec<String> = d["modes"]
        .as_array()
        .map(|modes| {
            modes
                .iter()
                .filter_map(|mode| mode.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let stream = d["streams"].as_array().and_then(|streams| streams.first());
    let video_ssrc = stream
        .and_then(|stream| stream["ssrc"].as_u64())
        .map(|ssrc| ssrc as u32);
    let rtx_ssrc = stream
        .and_then(|stream| stream["rtx_ssrc"].as_u64())
        .map(|ssrc| ssrc as u32);

    let remote: SocketAddr = format!("{ip}:{port}")
        .parse()
        .map_err(|_| Error::Message(format!("voice server address {ip}:{port} is invalid")))?;
    let bind = if remote.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let socket = UdpSocket::bind(bind)
        .await
        .map_err(|error| Error::Message(format!("voice udp bind: {error}")))?;
    let local = discover_ip(&socket, remote, audio_ssrc).await?;

    let mode = modes
        .iter()
        .find_map(|mode| Mode::parse(mode))
        .ok_or_else(|| {
            Error::Message(format!("no supported voice encryption mode in {modes:?}"))
        })?;

    send_json(
        &mut write,
        json!({
            "op": 1,
            "d": {
                "protocol": "udp",
                "data": {
                    "address": local.ip().to_string(),
                    "port": local.port(),
                    "mode": mode_name(mode),
                },
                "codecs": [
                    {"name": "opus", "type": "audio", "priority": 1000, "payload_type": PT_OPUS},
                    {"name": "H264", "type": "video", "priority": 1000, "payload_type": PT_H264, "rtx_payload_type": rtp::PT_H264_RTX, "encode": true, "decode": true}
                ],
                "rtc_connection_id": Uuid::new_v4().to_string(),
            }
        }),
    )
    .await?;

    let (description, pending_json, pending_binary) =
        wait_for_session_description(&mut read).await?;
    let d = &description["d"];
    let secret_key: Vec<u8> = d["secret_key"]
        .as_array()
        .map(|key| {
            key.iter()
                .filter_map(|value| value.as_u64().map(|byte| byte as u8))
                .collect()
        })
        .unwrap_or_default();
    if secret_key.len() != 32 {
        return Err(Error::Message(format!(
            "voice secret_key has {} bytes, expected 32",
            secret_key.len()
        )));
    }
    let dave_version = d["dave_protocol_version"].as_u64().unwrap_or(0) as u16;
    let srtp = Srtp::new(mode, &secret_key)?;

    let dave = if dave_version > 0 {
        let mut session = Dave::new(
            dave_version,
            params.user_id.get(),
            params.dave_channel_id.get(),
        )?;
        send_binary(&mut write, 26, &session.key_package()?).await?;
        Some(session)
    } else {
        None
    };

    // Announce that this client is present in the session. Discord requires at
    // least one speaking payload before media flows.
    send_json(
        &mut write,
        json!({
            "op": 5,
            "d": {"speaking": 0, "delay": 0, "ssrc": audio_ssrc}
        }),
    )
    .await?;

    let _ = events.send(VoiceEvent::Ready {
        ssrc: audio_ssrc,
        video_ssrc,
        rtx_ssrc,
        mode: mode_name(mode).to_owned(),
        dave_version,
    });

    let mut state = SessionState {
        params,
        write,
        read,
        socket,
        remote,
        srtp,
        audio_ssrc,
        video_ssrc,
        rtx_ssrc,
        dave,
        users: HashSet::new(),
        audio_ssrc_to_user: HashMap::new(),
        video_ssrc_to_user: HashMap::new(),
        depacketizers: HashMap::new(),
        seq_audio: (now_millis() & 0xFFFF) as u16,
        seq_video: (now_millis() & 0xFFFF) as u16,
        ts_audio: now_millis() as u32,
        ts_video: now_millis() as u32,
        speaking: 0,
        video_enabled: false,
        last_binary_seq: 0,
        events,
    };

    // DAVE traffic can race the session description; replay anything the
    // handshake buffered before entering the live loop.
    for value in pending_json {
        state.handle_json(value).await?;
    }
    for bytes in pending_binary {
        state.handle_binary(&bytes).await?;
    }

    let mut heartbeat = tokio::time::interval(Duration::from_millis(heartbeat));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await;

    let mut buffer = vec![0u8; 2048];
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let payload = json!({"op": 3, "d": {"t": now_millis(), "seq_ack": state.last_binary_seq}});
                if send_json(&mut state.write, payload).await.is_err() {
                    break;
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                if !state.handle_command(command).await? {
                    break;
                }
            }
            message = state.read.next() => {
                let Some(message) = message else { break };
                match message {
                    Ok(Message::Text(text)) => {
                        if let Ok(value) = serde_json::from_str::<Value>(&text)
                            && !state.handle_json(value).await? { break; }
                    }
                    Ok(Message::Binary(bytes)) => {
                        if !state.handle_binary(&bytes).await? { break; }
                    }
                    Ok(Message::Close(frame)) => {
                        let (code, reason) = close_parts(frame);
                        let _ = state.events.send(VoiceEvent::Closed { code, reason });
                        return Ok(());
                    }
                    Ok(Message::Ping(payload)) => {
                        let _ = state.write.send(Message::Pong(payload)).await;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let _ = state.events.send(VoiceEvent::Error(format!("voice ws: {error}")));
                        return Ok(());
                    }
                }
            }
            received = state.socket.recv_from(&mut buffer) => {
                match received {
                    Ok((length, _)) => state.handle_udp(&buffer[..length]),
                    Err(error) => {
                        warn!(%error, "voice udp receive failed");
                    }
                }
            }
        }
    }

    let _ = state.events.send(VoiceEvent::Closed {
        code: None,
        reason: String::new(),
    });
    Ok(())
}

impl SessionState {
    async fn handle_command(&mut self, command: Command) -> Result<bool> {
        match command {
            Command::SendAudio { opus, frame_ms } => {
                if let Some(dave) = &mut self.dave {
                    let encrypted = dave.encrypt_audio(&opus)?;
                    self.send_rtp_audio(&encrypted).await?;
                } else {
                    self.send_rtp_audio(&opus).await?;
                }
                self.ts_audio = self.ts_audio.wrapping_add(frame_ms * 48);
                if self.speaking & 1 == 0 {
                    self.set_speaking(1).await?;
                }
            }
            Command::SendVideo { frame, frame_ms } => {
                if self.video_ssrc.is_none() {
                    return Ok(true);
                }
                let encrypted = match &mut self.dave {
                    Some(dave) => dave.encrypt_video(&frame)?,
                    None => frame,
                };
                let packets = rtp::packetize(&encrypted, MAX_PAYLOAD);
                for (payload, marker) in packets {
                    self.send_rtp_video(&payload, marker).await?;
                }
                self.ts_video = self.ts_video.wrapping_add(frame_ms * 90);
            }
            Command::SetSpeaking(flags) => {
                self.set_speaking(flags).await?;
            }
            Command::SetVideo(enabled) => {
                self.video_enabled = enabled;
                self.send_video_state().await?;
            }
            Command::Disconnect => return Ok(false),
        }
        Ok(true)
    }

    async fn set_speaking(&mut self, flags: u8) -> Result<()> {
        if self.speaking == flags {
            return Ok(());
        }
        self.speaking = flags;
        send_json(
            &mut self.write,
            json!({
                "op": 5,
                "d": {"speaking": flags, "delay": 0, "ssrc": self.audio_ssrc}
            }),
        )
        .await
    }

    async fn send_video_state(&mut self) -> Result<()> {
        let (video_ssrc, rtx_ssrc) = if self.video_enabled {
            (self.video_ssrc.unwrap_or(0), self.rtx_ssrc.unwrap_or(0))
        } else {
            (0, 0)
        };
        let streams = if self.video_enabled {
            self.video_ssrc
                .map(|ssrc| {
                    vec![json!({
                        "type": "video",
                        "rid": "100",
                        "ssrc": ssrc,
                        "rtx_ssrc": self.rtx_ssrc.unwrap_or(ssrc + 1),
                        "quality": 100,
                        "active": true
                    })]
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        send_json(
            &mut self.write,
            json!({
                "op": 12,
                "d": {
                    "audio_ssrc": self.audio_ssrc,
                    "video_ssrc": video_ssrc,
                    "rtx_ssrc": rtx_ssrc,
                    "streams": streams,
                }
            }),
        )
        .await
    }

    async fn send_rtp_audio(&mut self, payload: &[u8]) -> Result<()> {
        let header = RtpHeader::write(
            false,
            PT_OPUS,
            self.seq_audio,
            self.ts_audio,
            self.audio_ssrc,
        );
        self.seq_audio = self.seq_audio.wrapping_add(1);
        let packet = self.srtp.encrypt(&header, payload)?;
        self.socket
            .send_to(&packet, self.remote)
            .await
            .map_err(|error| Error::Message(format!("voice udp send: {error}")))?;
        Ok(())
    }

    async fn send_rtp_video(&mut self, payload: &[u8], marker: bool) -> Result<()> {
        let ssrc = self.video_ssrc.unwrap_or(self.audio_ssrc);
        let header = RtpHeader::write(marker, PT_H264, self.seq_video, self.ts_video, ssrc);
        self.seq_video = self.seq_video.wrapping_add(1);
        let packet = self.srtp.encrypt(&header, payload)?;
        self.socket
            .send_to(&packet, self.remote)
            .await
            .map_err(|error| Error::Message(format!("voice udp send: {error}")))?;
        Ok(())
    }

    async fn handle_json(&mut self, value: Value) -> Result<bool> {
        let op = value["op"].as_i64().unwrap_or(-1);
        let d = &value["d"];
        match op {
            5 => {
                let Some(user_id) = d["user_id"]
                    .as_str()
                    .and_then(|id| id.parse::<Snowflake>().ok())
                else {
                    return Ok(true);
                };
                let ssrc = d["ssrc"].as_u64().unwrap_or(0) as u32;
                let speaking = d["speaking"].as_u64().unwrap_or(0) != 0;
                self.audio_ssrc_to_user.insert(ssrc, user_id);
                let _ = self.events.send(VoiceEvent::Speaking {
                    user_id,
                    ssrc,
                    speaking,
                });
            }
            11 => {
                if let Some(ids) = d["user_ids"].as_array() {
                    for id in ids {
                        if let Some(id) = id.as_str().and_then(|id| id.parse::<Snowflake>().ok()) {
                            self.users.insert(id.get());
                            let _ = self.events.send(VoiceEvent::ClientConnect { user_id: id });
                        }
                    }
                }
            }
            12 => {
                if let Some(user_id) = d["user_id"]
                    .as_str()
                    .and_then(|id| id.parse::<Snowflake>().ok())
                {
                    if let Some(ssrc) = d["audio_ssrc"].as_u64() {
                        self.audio_ssrc_to_user.insert(ssrc as u32, user_id);
                    }
                    if let Some(ssrc) = d["video_ssrc"].as_u64() {
                        self.video_ssrc_to_user.insert(ssrc as u32, user_id);
                    }
                    if let Some(streams) = d["streams"].as_array() {
                        for stream in streams {
                            if let Some(ssrc) = stream["ssrc"].as_u64() {
                                self.video_ssrc_to_user.insert(ssrc as u32, user_id);
                            }
                            if let Some(ssrc) = stream["rtx_ssrc"].as_u64() {
                                self.video_ssrc_to_user.insert(ssrc as u32, user_id);
                            }
                        }
                    }
                }
            }
            13 => {
                if let Some(user_id) = d["user_id"]
                    .as_str()
                    .and_then(|id| id.parse::<Snowflake>().ok())
                {
                    self.users.remove(&user_id.get());
                    let _ = self.events.send(VoiceEvent::ClientDisconnect { user_id });
                }
            }
            21 => {
                let transition_id = d["transition_id"].as_u64().unwrap_or(0) as u16;
                let protocol_version = d["protocol_version"].as_u64().unwrap_or(0) as u16;
                if protocol_version == 0
                    && let Some(dave) = &mut self.dave
                {
                    dave.set_passthrough(true, 120);
                }
                if let Some(dave) = &mut self.dave {
                    let needs_ready = dave.prepare(transition_id, protocol_version)?;
                    if needs_ready {
                        self.send_ready(transition_id).await?;
                    }
                }
            }
            22 => {
                let transition_id = d["transition_id"].as_u64().unwrap_or(0) as u16;
                if let Some(dave) = &mut self.dave {
                    dave.execute(transition_id)?;
                }
            }
            24 => {
                let epoch = d["epoch"].as_u64().unwrap_or(0);
                let protocol_version = d["protocol_version"].as_u64().unwrap_or(0) as u16;
                if epoch == 1
                    && protocol_version > 0
                    && let Some(dave) = &mut self.dave
                {
                    dave.reinit(
                        protocol_version,
                        self.params.user_id.get(),
                        self.params.dave_channel_id.get(),
                    )?;
                    send_binary(&mut self.write, 26, &dave.key_package()?).await?;
                }
            }
            6 => {}
            _ => debug!(op, "unhandled voice gateway op"),
        }
        Ok(true)
    }

    async fn send_ready(&mut self, transition_id: u16) -> Result<()> {
        send_json(
            &mut self.write,
            json!({"op": 23, "d": {"transition_id": transition_id}}),
        )
        .await
    }

    async fn handle_binary(&mut self, bytes: &[u8]) -> Result<bool> {
        if bytes.len() < 3 {
            return Ok(true);
        }
        self.last_binary_seq = u16::from_be_bytes([bytes[0], bytes[1]]);
        let op = bytes[2];
        let body = &bytes[3..];
        match op {
            25 => {
                if let Some(dave) = &mut self.dave {
                    dave.set_external_sender(body)?;
                }
            }
            27 => {
                if body.is_empty() {
                    return Ok(true);
                }
                let op_type = body[0];
                let proposals = &body[1..];
                let mut expected: Vec<u64> = self.users.iter().copied().collect();
                expected.push(self.params.user_id.get());
                expected.sort_unstable();
                expected.dedup();
                if let Some(dave) = &mut self.dave
                    && let Some(payload) = dave.proposals(op_type, proposals, &expected)?
                {
                    send_binary(&mut self.write, 28, &payload).await?;
                }
            }
            29 | 30 => {
                if body.len() < 2 {
                    return Ok(true);
                }
                let transition_id = u16::from_be_bytes([body[0], body[1]]);
                let payload = &body[2..];
                let result = if let Some(dave) = &mut self.dave {
                    if op == 29 {
                        dave.commit(transition_id, payload)
                    } else {
                        dave.welcome(transition_id, payload)
                    }
                } else {
                    Ok(false)
                };

                match result {
                    Ok(true) => self.send_ready(transition_id).await?,
                    Ok(false) => {}
                    Err(error) => {
                        warn!(%error, transition_id, "DAVE commit/welcome rejected");
                        self.send_invalid_commit(transition_id).await?;
                    }
                }
                if self.dave.as_ref().is_some_and(|dave| dave.is_ready()) {
                    let _ = self.events.send(VoiceEvent::DaveReady);
                }
            }
            31 => {
                if body.len() >= 2 {
                    let transition_id = u16::from_be_bytes([body[0], body[1]]);
                    self.send_invalid_commit(transition_id).await?;
                }
            }
            other => debug!(op = other, "unhandled voice binary op"),
        }
        Ok(true)
    }

    async fn send_invalid_commit(&mut self, transition_id: u16) -> Result<()> {
        send_json(
            &mut self.write,
            json!({"op": 31, "d": {"transition_id": transition_id}}),
        )
        .await?;
        if let Some(dave) = &mut self.dave {
            let version = dave.version();
            dave.reinit(
                version,
                self.params.user_id.get(),
                self.params.dave_channel_id.get(),
            )?;
            send_binary(&mut self.write, 26, &dave.key_package()?).await?;
        }
        Ok(())
    }

    fn handle_udp(&mut self, packet: &[u8]) {
        let Some((header, _)) = RtpHeader::parse(packet) else {
            return;
        };
        if header.payload_type >= 200 {
            self.handle_rtcp(&packet[header.header_len..], header.ssrc);
            return;
        }

        let payload = match self.srtp.decrypt(packet) {
            Ok(payload) => payload,
            Err(error) => {
                debug!(%error, "SRTP decrypt failed");
                return;
            }
        };

        match header.payload_type {
            PT_OPUS => {
                let Some(user_id) = self.audio_ssrc_to_user.get(&header.ssrc).copied() else {
                    return;
                };
                let Some(opus) = self
                    .dave
                    .as_mut()
                    .and_then(|dave| dave.decrypt_audio(user_id.get(), &payload))
                else {
                    return;
                };
                let _ = self.events.send(VoiceEvent::Audio {
                    user_id,
                    ssrc: header.ssrc,
                    opus,
                    sequence: header.sequence,
                    timestamp: header.timestamp,
                });
            }
            PT_H264 | rtp::PT_H264_RTX => {
                let Some(user_id) = self.video_ssrc_to_user.get(&header.ssrc).copied() else {
                    return;
                };
                let payload = if header.payload_type == rtp::PT_H264_RTX {
                    // RTX packets carry a two-byte original sequence number.
                    if payload.len() < 2 {
                        return;
                    }
                    &payload[2..]
                } else {
                    &payload
                };
                let depacketizer = self.depacketizers.entry(user_id.get() as u32).or_default();
                if let Some(frame) = depacketizer.push(payload, header.timestamp, header.marker) {
                    let Some(frame) = self
                        .dave
                        .as_mut()
                        .and_then(|dave| dave.decrypt_video(user_id.get(), &frame))
                    else {
                        return;
                    };
                    let keyframe = rtp::is_keyframe(&frame);
                    let _ = self.events.send(VoiceEvent::Video {
                        user_id,
                        ssrc: header.ssrc,
                        frame,
                        keyframe,
                    });
                }
            }
            _ => {}
        }
    }

    fn handle_rtcp(&mut self, payload: &[u8], ssrc: u32) {
        if payload.len() < 8 {
            return;
        }
        // RTCP header: V/P/RC, PT, length. PT 206 = PSFB (PLI is FMT 1).
        let packet_type = payload[1];
        let fmt = payload[0] & 0x1F;
        if packet_type == 206 && fmt == 1 {
            let Some(user_id) = self.video_ssrc_to_user.get(&ssrc).copied() else {
                return;
            };
            let _ = self
                .events
                .send(VoiceEvent::KeyframeRequest { user_id, ssrc });
        }
    }
}

async fn wait_for_hello(read: &mut WsRead) -> Result<u64> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let value = tokio::time::timeout_at(deadline, next_json(read))
            .await
            .map_err(|_| Error::Message("voice HELLO timed out".into()))??;
        if value["op"].as_i64() == Some(8) {
            return value["d"]["heartbeat_interval"]
                .as_u64()
                .ok_or_else(|| Error::Message("voice HELLO has no interval".into()));
        }
    }
}

async fn wait_for_session_description(
    read: &mut WsRead,
) -> Result<(Value, Vec<Value>, Vec<Vec<u8>>)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut pending_json = Vec::new();
    let mut pending_binary = Vec::new();
    loop {
        let message = tokio::time::timeout_at(deadline, read.next())
            .await
            .map_err(|_| Error::Message("voice session description timed out".into()))?;
        match message {
            Some(Ok(Message::Text(text))) => {
                let value: Value = serde_json::from_str(&text)
                    .map_err(|error| Error::Message(format!("voice json: {error}")))?;
                match value["op"].as_i64() {
                    Some(4) => return Ok((value, pending_json, pending_binary)),
                    Some(21..=24) => pending_json.push(value),
                    _ => {}
                }
            }
            Some(Ok(Message::Binary(bytes))) => pending_binary.push(bytes.to_vec()),
            Some(Ok(Message::Close(frame))) => {
                let (_, reason) = close_parts(frame);
                return Err(Error::Message(format!("voice closed: {reason}")));
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => {
                return Err(Error::Message(format!("voice ws read: {error}")));
            }
            None => return Err(Error::Message("voice ws ended".into())),
        }
    }
}

async fn wait_for_opcode(read: &mut WsRead, opcode: i64) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let value = tokio::time::timeout_at(deadline, next_json(read))
            .await
            .map_err(|_| Error::Message(format!("voice opcode {opcode} timed out")))??;
        if value["op"].as_i64() == Some(opcode) {
            return Ok(value);
        }
        if value["op"].as_i64() == Some(-1) {
            // close frame
            return Err(Error::Message("voice socket closed".into()));
        }
    }
}

async fn next_json(read: &mut WsRead) -> Result<Value> {
    loop {
        match read.next().await {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text)
                    .map_err(|error| Error::Message(format!("voice json: {error}")));
            }
            Some(Ok(Message::Binary(_))) => continue,
            Some(Ok(Message::Close(frame))) => {
                let (_, reason) = close_parts(frame);
                return Err(Error::Message(format!("voice closed: {reason}")));
            }
            Some(Ok(_)) => continue,
            Some(Err(error)) => return Err(Error::Message(format!("voice ws read: {error}"))),
            None => return Err(Error::Message("voice ws ended".into())),
        }
    }
}

async fn send_json(write: &mut WsWrite, value: Value) -> Result<()> {
    write
        .send(Message::Text(value.to_string().into()))
        .await
        .map_err(|error| Error::Message(format!("voice ws send: {error}")))
}

async fn send_binary(write: &mut WsWrite, opcode: u8, payload: &[u8]) -> Result<()> {
    let mut bytes = Vec::with_capacity(payload.len() + 1);
    bytes.push(opcode);
    bytes.extend_from_slice(payload);
    write
        .send(Message::Binary(bytes.into()))
        .await
        .map_err(|error| Error::Message(format!("voice ws binary send: {error}")))
}

async fn discover_ip(socket: &UdpSocket, remote: SocketAddr, ssrc: u32) -> Result<SocketAddr> {
    let mut packet = [0u8; 74];
    packet[0..2].copy_from_slice(&1u16.to_be_bytes());
    packet[2..4].copy_from_slice(&70u16.to_be_bytes());
    packet[4..8].copy_from_slice(&ssrc.to_be_bytes());

    socket
        .send_to(&packet, remote)
        .await
        .map_err(|error| Error::Message(format!("voice ip discovery send: {error}")))?;

    let mut response = [0u8; 74];
    let length = tokio::time::timeout(Duration::from_secs(5), socket.recv(&mut response))
        .await
        .map_err(|_| Error::Message("voice ip discovery timed out".into()))?
        .map_err(|error| Error::Message(format!("voice ip discovery recv: {error}")))?;
    if length < 74 {
        return Err(Error::Message("short ip discovery response".into()));
    }

    let address_end = response[8..72]
        .iter()
        .position(|byte| *byte == 0)
        .map(|position| position + 8)
        .unwrap_or(72);
    let address = std::str::from_utf8(&response[8..address_end])
        .map_err(|_| Error::Message("non-UTF8 discovered address".into()))?;
    let port = u16::from_be_bytes([response[72], response[73]]);
    format!("{address}:{port}")
        .parse()
        .map_err(|_| Error::Message(format!("discovered address {address}:{port} is invalid")))
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Aes256GcmRtpsize => "aead_aes256_gcm_rtpsize",
        Mode::XChaCha20Poly1305Rtpsize => "aead_xchacha20_poly1305_rtpsize",
    }
}

fn close_parts(frame: Option<CloseFrame>) -> (Option<u16>, String) {
    match frame {
        Some(frame) => (Some(frame.code.into()), frame.reason.to_string()),
        None => (None, String::new()),
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_names_round_trip() {
        for mode in [Mode::Aes256GcmRtpsize, Mode::XChaCha20Poly1305Rtpsize] {
            assert_eq!(Mode::parse(mode_name(mode)), Some(mode));
        }
    }
}
