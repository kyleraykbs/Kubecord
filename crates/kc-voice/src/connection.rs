//! Voice gateway, UDP, SRTP, RTP and DAVE state machine.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::sink::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use kc_core::Snowflake;
use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use wreq::ws::message::{CloseFrame, Message};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::dave::{Dave, FrameRejected};
use crate::error::{Error, Result};
use crate::rtcp;
use crate::rtp::{self, MAX_PAYLOAD, PT_H264, PT_OPUS, RtpHeader};
use crate::srtp::{Mode, Srtp};
use crate::video_rx::{Assembled, Packet, VideoStream};

type WsStream = wreq::ws::WebSocket;
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
    /// Watching someone else's stream: only its owner may send media on
    /// that connection, so nothing is transmitted.
    pub receive_only: bool,
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
    /// One complete, decrypted H.264 frame (Annex-B) from a participant's
    /// camera, or from the streamer on a stream connection. The first frame
    /// after a start or a loss is always a keyframe.
    Video {
        user_id: Snowflake,
        ssrc: u32,
        frame: Vec<u8>,
        keyframe: bool,
        /// RTP timestamp (90 kHz).
        timestamp: u32,
    },
    /// The participant stopped sending video (or left).
    VideoEnded {
        user_id: Snowflake,
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
    ForceKeyframe,
    /// How sent video fared over the last couple of seconds, for adapting
    /// the bitrate: packets sent, packets the server asked for again, and
    /// the server's bandwidth estimate (zero when it has given none).
    VideoNetwork {
        sent: u32,
        lost: u32,
        remb_kbps: u32,
    },
    /// Round trip to the voice server (a heartbeat and its acknowledgement),
    /// measured every few seconds; `None` when none came back in time.
    Ping {
        millis: Option<u32>,
    },
    Closed {
        code: Option<u16>,
        reason: String,
    },
    Error(String),
}

/// A participant counts as speaking until this long after their last voice
/// packet. Clients stop sending when their user stops talking, so packet
/// arrival is the speaking signal; the gateway's speaking opcode is sent
/// once, not per utterance.
const SPEAKING_HOLD: Duration = Duration::from_millis(300);

/// How often a keyframe is asked for again while one is still needed.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(600);

/// Who a received video SSRC belongs to.
#[derive(Clone, Copy, Debug)]
struct VideoSsrc {
    user: Snowflake,
    /// The SSRC of the video itself; differs from the key for the paired
    /// retransmission SSRC.
    primary: u32,
    retransmission: bool,
}

/// What happened to received media packets, for diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ReceiveCounters {
    /// Opus packets delivered to the application.
    audio: u64,
    /// Opus silence markers (sent when a talk burst ends).
    silence: u64,
    video_packets: u64,
    /// Video frames handed to the application.
    video_frames: u64,
    /// Video packets that came as retransmissions.
    video_repaired: u64,
    /// Video packets asked for again.
    video_nacked: u64,
    /// Times video packets were lost for good (frames dropped).
    video_lost: u64,
    /// Complete frames held back while waiting for a keyframe.
    video_held: u64,
    video_keyframe_requests: u64,
    /// Video frames the end-to-end layer could not decrypt.
    video_e2ee_failed: u64,
    /// Video from an SSRC no user has been announced for.
    video_unknown_ssrc: u64,
    /// Audio from an SSRC no user has been announced for.
    unknown_ssrc: u64,
    /// Packets the transport (SRTP) layer could not authenticate.
    transport_failed: u64,
    /// Audio packets delivered more than once (same sequence number).
    duplicates: u64,
    /// Audio packets that never arrived, from gaps in sequence numbers.
    missing: u64,
    /// Frames the end-to-end (DAVE) layer saw twice. Harmless.
    e2ee_replayed: u64,
    /// Frames that arrived before the end-to-end session had their key.
    e2ee_not_ready: u64,
    /// Frames the end-to-end layer rejected as corrupt or unencrypted.
    e2ee_failed: u64,
}

/// Whether a datagram is RTCP rather than RTP. Both share the port; RFC 5761
/// reserves second-byte values 192..=223 for RTCP. A plain `>= 200` test
/// misreads RTP packets with the marker bit set (`0x80 | 120` for Opus is
/// 248), which loses the first packet of every talk burst.
fn is_rtcp(packet: &[u8]) -> bool {
    packet.len() >= 8 && packet[0] >> 6 == 2 && (192..=223).contains(&packet[1])
}

/// Retransmission history kept when the client has not chosen a window.
const DEFAULT_RETRANSMIT_WINDOW: Duration = Duration::from_secs(2);
/// Hard cap on remembered packets, well under the 16-bit sequence space so a
/// NACKed sequence number can never match a packet from a previous wrap.
const MAX_SENT_PACKETS: usize = 16_384;

/// Recently sent, already encrypted packets of one SSRC, kept so NACKs can be
/// answered. Bounded by age and by `MAX_SENT_PACKETS`.
#[derive(Default)]
struct SentPackets {
    packets: HashMap<u16, Vec<u8>>,
    order: VecDeque<(u16, std::time::Instant)>,
}

impl SentPackets {
    fn insert(&mut self, sequence: u16, packet: &[u8], window: Duration) {
        let now = std::time::Instant::now();
        while let Some((old, sent)) = self.order.front().copied() {
            if now.duration_since(sent) <= window && self.order.len() < MAX_SENT_PACKETS {
                break;
            }
            self.order.pop_front();
            self.packets.remove(&old);
        }
        if window.is_zero() {
            return;
        }
        self.packets.insert(sequence, packet.to_vec());
        self.order.push_back((sequence, now));
    }

    fn get(&self, sequence: u16) -> Option<Vec<u8>> {
        self.packets.get(&sequence).cloned()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.packets.len()
    }
}

/// A sent video packet as it was before transport encryption, so it can be
/// sent again as a retransmission.
#[derive(Clone)]
struct VideoPacket {
    payload: Vec<u8>,
    marker: bool,
    timestamp: u32,
    place: PacketPlace,
    sent: std::time::Instant,
    /// How long the socket had been quiet (no video packet) before this
    /// one left, for the log: lost packets that follow a pause point at
    /// something other than a crowded link.
    idle_before: Duration,
}

/// What a report of lost video calls for.
#[derive(Debug, PartialEq, Eq)]
enum LossKeyframe {
    /// The resends repair it.
    None,
    /// A keyframe, now.
    Now,
    /// A keyframe, once the spacing since the last one has passed.
    Owed,
}

/// Decides whether lost video needs a keyframe. Lost packets are sent
/// again, which repairs the picture in about one round trip, so a
/// keyframe is for when that cannot work: a packet is `missing` (too old
/// to still be held), or is being asked for a second time (`repeated`:
/// the resend was lost too). A keyframe for every loss fed a loop on a
/// lossy connection: several times the size of a frame, about one a
/// second, each adding load where packets were already being dropped.
fn loss_keyframe(
    missing: bool,
    repeated: bool,
    since_last: Option<Duration>,
    spacing: Duration,
) -> LossKeyframe {
    if missing {
        LossKeyframe::Now
    } else if !repeated {
        LossKeyframe::None
    } else if since_last.is_none_or(|since| since >= spacing) {
        LossKeyframe::Now
    } else {
        // Not forgotten: nothing else would end that freeze.
        LossKeyframe::Owed
    }
}

/// Every video packet sent a second time a little later, on the
/// retransmission stream, so one that is lost is usually replaced before
/// the server has to ask. Doubles the video upload.
#[derive(Default)]
struct Redundancy {
    enabled: bool,
    /// Packets whose second copy is still to go, oldest first.
    due: VecDeque<(std::time::Instant, u16)>,
}

impl Redundancy {
    /// Long enough that a short burst of loss does not take both copies,
    /// short enough to beat the round trip of being asked.
    const OFFSET: Duration = Duration::from_millis(20);

    fn sent(&mut self, sequence: u16, now: std::time::Instant) {
        if self.enabled {
            self.due.push_back((now + Self::OFFSET, sequence));
        }
    }

    /// The packets whose second copy is due.
    fn take_due(&mut self, now: std::time::Instant) -> Vec<u16> {
        let mut due = Vec::new();
        while self.due.front().is_some_and(|(at, _)| *at <= now) {
            if let Some((_, sequence)) = self.due.pop_front() {
                due.push(sequence);
            }
        }
        due
    }
}

/// Where a packet sits in its frame, for the log.
#[derive(Clone, Copy, Debug, Default)]
struct PacketPlace {
    index: u16,
    count: u16,
    keyframe: bool,
}

/// Recently sent video packets by sequence number, bounded like
/// [`SentPackets`].
#[derive(Default)]
struct SentVideo {
    packets: HashMap<u16, VideoPacket>,
    order: VecDeque<(u16, std::time::Instant)>,
}

impl SentVideo {
    fn insert(&mut self, sequence: u16, packet: VideoPacket, window: Duration) {
        let now = std::time::Instant::now();
        while let Some((old, sent)) = self.order.front().copied() {
            if now.duration_since(sent) <= window && self.order.len() < MAX_SENT_PACKETS {
                break;
            }
            self.order.pop_front();
            self.packets.remove(&old);
        }
        if window.is_zero() {
            return;
        }
        self.packets.insert(sequence, packet);
        self.order.push_back((sequence, now));
    }

    fn get(&self, sequence: u16) -> Option<VideoPacket> {
        self.packets.get(&sequence).cloned()
    }

    fn len(&self) -> usize {
        self.packets.len()
    }
}

/// The payload of a retransmission (RFC 4588): the original packet's
/// sequence number, then its payload.
fn rtx_payload(original_sequence: u16, payload: &[u8]) -> Vec<u8> {
    let mut rtx = Vec::with_capacity(payload.len() + 2);
    rtx.extend_from_slice(&original_sequence.to_be_bytes());
    rtx.extend_from_slice(payload);
    rtx
}

/// Pacing rate as a multiple of the measured video bitrate.
const PACING_FACTOR: f64 = 2.5;
/// Minimum pacing rate, used until the bitrate is known and on quiet screens.
const PACING_FLOOR_BPS: f64 = 5_000_000.0;
/// The least time between keyframes forced because packets were lost,
/// unless set otherwise. Sent on every request, 750 ms apart, they fed a
/// loop on a connection near its limit.
const LOSS_KEYFRAME_SPACING: Duration = Duration::from_secs(1);
/// How often the encoder is told how the network is treating its video.
const NETWORK_REPORT_INTERVAL: Duration = Duration::from_secs(2);
/// How often the ping is measured.
const PING_INTERVAL: Duration = Duration::from_secs(5);

/// Counters for sent video over one reporting period, logged so a viewer's
/// freeze can be matched to what the network asked of us.
#[derive(Clone, Copy, Default, PartialEq)]
struct SentStats {
    frames: u64,
    packets: u64,
    bytes: u64,
    /// Frames not sent: the pacing queue was full, or DAVE was not ready.
    dropped: u64,
    /// Video packets the server asked for again, and how many we still had.
    nack_requested: u64,
    nack_found: u64,
    audio_nack_requested: u64,
    /// Video packets asked for a second time within a second: the first
    /// resend did not arrive, or was not accepted.
    nack_repeated: u64,
    /// Packets asked for again that were the first of their frame, and
    /// that left after a pause of 5 ms or more.
    nack_first: u64,
    nack_after_pause: u64,
    /// Second copies sent (see [`Redundancy`]).
    sent_twice: u64,
    /// Keyframes viewers asked for (PLI/FIR).
    keyframe_requests: u64,
    /// The longest the pacing queue got, in milliseconds of sending.
    backlog_max_ms: u64,
    /// Highest loss a receiver report gave, in 1/256ths.
    loss_max: u32,
    /// The server's latest bandwidth estimate for us.
    remb_bps: u64,
}

/// Upper bound on packets per 1 ms pacer tick, so many tiny slice NALs cannot
/// leave back-to-back.
const PACING_MAX_PACKETS_PER_TICK: usize = 4;
/// RTP header, AEAD tag and nonce suffix added to each payload on the wire.
const PACKET_OVERHEAD: usize = 32;

/// 90 kHz RTP ticks per video frame. `frame_ms` is `1000 / fps` truncated to
/// whole milliseconds, so `frame_ms * 90` runs the media clock slow (33 ms for
/// 30 fps is 2970 ticks, not 3000: 1% slow; 4% at 60 fps). Recover the exact
/// step for the frame rates the capture settings offer.
fn video_rtp_step(frame_ms: u32) -> u32 {
    for fps in [15u32, 24, 25, 30, 50, 60] {
        if 1000 / fps == frame_ms {
            return 90_000 / fps;
        }
    }
    frame_ms * 90
}

/// One Opus frame of silence; sent unencrypted by end-to-end encryption.
const OPUS_SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];
/// Silence frames sent when speech ends (as the official clients send).
const SILENCE_FRAMES: usize = 5;

enum Command {
    SendAudio {
        opus: Vec<u8>,
        frame_ms: u32,
        speaking: u8,
    },
    SendVideo {
        frame: Vec<u8>,
        frame_ms: u32,
    },
    SetSpeaking(u8),
    /// The microphone went quiet: silence frames, then speaking off.
    EndTalkBurst {
        frame_ms: u32,
    },
    /// A microphone frame that was not sent; the clock moves on.
    SkipAudio {
        frame_ms: u32,
    },
    SetVideo(bool),
    SetRetransmitWindow(Duration),
    SetLossKeyframeSpacing(Duration),
    SetRedundancy(bool),
    Disconnect,
}

/// Command handle to a running voice connection.
/// Command handle to a running voice connection. Every clone shares the
/// connection it points at, so [`VoiceHandle::retarget`] moves all of them
/// (microphone, camera, stream video and sound) to a new connection at
/// once, without restarting what feeds them.
#[derive(Clone)]
pub struct VoiceHandle {
    tx: Arc<RwLock<mpsc::UnboundedSender<Command>>>,
}

impl VoiceHandle {
    fn new(tx: mpsc::UnboundedSender<Command>) -> Self {
        Self {
            tx: Arc::new(RwLock::new(tx)),
        }
    }

    fn send(&self, command: Command) {
        let tx = match self.tx.read() {
            Ok(tx) => tx,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = tx.send(command);
    }

    /// Points this handle and all its clones at the connection `to` drives,
    /// for a call that moved to another voice server or reconnected.
    /// Returns a handle to the connection they pointed at before, which
    /// the caller disconnects.
    pub fn retarget(&self, to: &VoiceHandle) -> VoiceHandle {
        let new = match to.tx.read() {
            Ok(tx) => tx.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let mut current = match self.tx.write() {
            Ok(tx) => tx,
            Err(poisoned) => poisoned.into_inner(),
        };
        VoiceHandle::new(std::mem::replace(&mut *current, new))
    }

    /// Whether the connection behind this handle has ended.
    pub fn is_closed(&self) -> bool {
        match self.tx.read() {
            Ok(tx) => tx.is_closed(),
            Err(poisoned) => poisoned.into_inner().is_closed(),
        }
    }

    pub fn send_audio(&self, opus: Vec<u8>, frame_ms: u32) {
        self.send(Command::SendAudio {
            opus,
            frame_ms,
            speaking: 1, // microphone
        });
    }

    /// Audio for a Go Live stream, marked SOUNDSHARE rather than microphone.
    pub fn send_soundshare_audio(&self, opus: Vec<u8>, frame_ms: u32) {
        self.send(Command::SendAudio {
            opus,
            frame_ms,
            speaking: 2,
        });
    }

    pub fn send_video(&self, frame: Vec<u8>, frame_ms: u32) {
        self.send(Command::SendVideo { frame, frame_ms });
    }

    pub fn set_speaking(&self, flags: u8) {
        self.send(Command::SetSpeaking(flags));
    }

    /// Ends a stretch of speech, as the official clients do when their
    /// user stops talking: a few Opus silence frames (`F8 FF FE`, outside
    /// end-to-end encryption, as receivers expect them), then the
    /// speaking flag off. The next [`VoiceHandle::send_audio`] turns it on
    /// again.
    pub fn end_talk_burst(&self, frame_ms: u32) {
        self.send(Command::EndTalkBurst { frame_ms });
    }

    /// Accounts for a microphone frame that was held back (muted, or
    /// below the input sensitivity), so the RTP clock keeps time with the
    /// gap and receivers place the next speech correctly.
    pub fn skip_audio(&self, frame_ms: u32) {
        self.send(Command::SkipAudio { frame_ms });
    }

    pub fn set_video(&self, enabled: bool) {
        self.send(Command::SetVideo(enabled));
    }

    /// How long sent audio and video packets stay available for
    /// retransmission. A network dropout up to this long can be repaired by
    /// answering the server's NACKs; zero disables retransmission.
    /// The least time between keyframes forced because video packets were
    /// lost. Shorter recovers sooner; too short and the extra keyframes
    /// themselves crowd a struggling connection.
    pub fn set_loss_keyframe_spacing(&self, spacing: Duration) {
        self.send(Command::SetLossKeyframeSpacing(spacing));
    }

    /// Sends every video packet twice (see [`Redundancy`]).
    pub fn set_redundancy(&self, enabled: bool) {
        self.send(Command::SetRedundancy(enabled));
    }

    pub fn set_retransmit_window(&self, window: Duration) {
        self.send(Command::SetRetransmitWindow(window));
    }

    pub async fn disconnect(&self) {
        self.disconnect_now();
    }

    /// [`VoiceHandle::disconnect`] for code that cannot wait.
    pub fn disconnect_now(&self) {
        self.send(Command::Disconnect);
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
    Ok(VoiceHandle::new(tx))
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
    /// Video SSRCs announced by other participants.
    video_ssrcs: HashMap<u32, VideoSsrc>,
    /// The media-sink wants (op 15) last sent, so a change goes out once.
    sink_wants: Option<Value>,
    /// Receive state for each participant sending video.
    video_streams: HashMap<Snowflake, VideoStream>,
    /// Who is speaking, by when their last voice packet arrived.
    voice_activity: HashMap<Snowflake, std::time::Instant>,
    received: ReceiveCounters,
    reported: ReceiveCounters,
    /// SSRCs whose audio was dropped for want of a user mapping, logged once.
    unknown_ssrcs: HashSet<u32>,
    /// Newest audio sequence number seen per SSRC.
    audio_sequences: HashMap<u32, u16>,
    seq_audio: u16,
    seq_video: u16,
    ts_audio: u32,
    ts_video: u32,
    video_queue: VecDeque<(Vec<u8>, bool, u32, PacketPlace)>,
    /// When a keyframe was last forced because packets were lost.
    last_loss_keyframe: Option<std::time::Instant>,
    /// Packets were lost too soon after that keyframe for another; one is
    /// owed once the spacing has passed.
    loss_keyframe_owed: bool,
    loss_keyframe_spacing: Duration,
    /// Video packets sent, and asked for again, since the last network
    /// report to the encoder.
    net_sent: u32,
    net_lost: u32,
    /// The server's latest bandwidth estimate for us, in kbit/s.
    net_remb_kbps: u32,
    sent_video: SentVideo,
    /// Sequence number of the next packet on the retransmission stream.
    seq_rtx: u16,
    /// Testing aid (`KUBECORD_TEST_DROP_VIDEO=n`): every nth video packet is
    /// not sent, as if lost on the way, to exercise retransmission.
    test_drop: Option<u32>,
    sent_audio: SentPackets,
    retransmit_window: Duration,
    retransmits: VecDeque<Vec<u8>>,
    redundancy: Redundancy,
    /// When the last video packet left, to measure pauses before packets.
    last_video_send: Option<std::time::Instant>,
    /// Payload bytes waiting in `video_queue`.
    video_queue_bytes: usize,
    /// What happened to the video being sent since the last report.
    sent: SentStats,
    /// The heartbeat sent and not yet acknowledged, for measuring the ping:
    /// its timestamp value and when it went out.
    ping_pending: Option<(u64, std::time::Instant)>,
    /// Video packets recently asked for again, to notice repeats.
    recent_nacks: HashMap<u16, std::time::Instant>,
    /// Pacer send credit in bytes; refilled from elapsed time at the pacing rate.
    pace_credit: f64,
    pace_last: std::time::Instant,
    /// Smoothed bitrate of the video being queued, which sets the pacing rate.
    video_rate_bps: f64,
    video_rate_window_start: std::time::Instant,
    video_rate_window_bytes: usize,
    last_backlog_warn: Option<std::time::Instant>,
    first_video_logged: bool,
    srtcp_error_logged: bool,
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
    // Abaddon connects to the endpoint exactly as Discord supplies it; the
    // modern endpoint often carries an explicit port.
    let endpoint = params
        .endpoint
        .trim_start_matches("wss://")
        .trim_end_matches('/');
    let url = format!("wss://{endpoint}/?v={VOICE_VERSION}&encoding=json");
    info!(%url, "voice connecting");

    let stream = tokio::time::timeout(
        Duration::from_secs(10),
        kc_http::browser::websocket(&url),
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
    // The web client then asks for the voice server's version (op 16; the
    // answer is ignored, like any other message before READY).
    send_json(&mut write, json!({"op": 16, "d": {}})).await?;

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
    // The server's op 4 mode is authoritative; READY only lists candidates.
    let negotiated_mode = d["mode"].as_str().and_then(Mode::parse).unwrap_or(mode);
    if negotiated_mode != mode {
        info!(
            selected = mode_name(mode),
            negotiated = mode_name(negotiated_mode),
            "voice encryption mode differs from READY"
        );
    }
    let srtp = Srtp::new(negotiated_mode, &secret_key)?;

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

    let ready_event = VoiceEvent::Ready {
        ssrc: audio_ssrc,
        video_ssrc,
        rtx_ssrc,
        mode: mode_name(negotiated_mode).to_owned(),
        dave_version,
    };

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
        video_ssrcs: HashMap::new(),
        sink_wants: None,
        video_streams: HashMap::new(),
        voice_activity: HashMap::new(),
        received: ReceiveCounters::default(),
        reported: ReceiveCounters::default(),
        unknown_ssrcs: HashSet::new(),
        audio_sequences: HashMap::new(),
        seq_audio: (now_millis() & 0xFFFF) as u16,
        seq_video: (now_millis() & 0xFFFF) as u16,
        ts_audio: now_millis() as u32,
        ts_video: now_millis() as u32,
        video_queue: VecDeque::new(),
        last_loss_keyframe: None,
        loss_keyframe_owed: false,
        loss_keyframe_spacing: LOSS_KEYFRAME_SPACING,
        net_sent: 0,
        net_lost: 0,
        net_remb_kbps: 0,
        sent_video: SentVideo::default(),
        seq_rtx: 0,
        test_drop: std::env::var("KUBECORD_TEST_DROP_VIDEO")
            .ok()
            .and_then(|every| every.parse().ok())
            .filter(|every| *every > 1)
            .inspect(|every| info!(every, "TEST: withholding video packets")),
        sent_audio: SentPackets::default(),
        retransmit_window: DEFAULT_RETRANSMIT_WINDOW,
        retransmits: VecDeque::new(),
        redundancy: Redundancy::default(),
        last_video_send: None,
        video_queue_bytes: 0,
        sent: SentStats::default(),
        ping_pending: None,
        recent_nacks: HashMap::new(),
        pace_credit: 0.0,
        pace_last: std::time::Instant::now(),
        video_rate_bps: 0.0,
        video_rate_window_start: std::time::Instant::now(),
        video_rate_window_bytes: 0,
        last_backlog_warn: None,
        first_video_logged: false,
        srtcp_error_logged: false,
        speaking: 0,
        video_enabled: false,
        last_binary_seq: 0,
        events,
    };

    // The roster, SSRC announcements and DAVE traffic all race the session
    // description; replay what the handshake buffered before entering the
    // live loop.
    for value in pending_json {
        state.handle_json(value).await?;
    }
    for bytes in pending_binary {
        state.handle_binary(&bytes).await?;
    }

    // Abaddon arms media the same way after the session description: one
    // speaking payload and one encrypted Opus silence frame. Sending only
    // `speaking: 0` leaves the server without a media source for this SSRC.
    if !state.params.receive_only {
        state.set_speaking(1).await?;
        state.send_rtp_audio(&OPUS_SILENCE).await?;
    }
    // The server sends no video to a client that has not announced its own
    // video state (even "off") and said which streams it wants to see.
    state.send_video_state().await?;
    state.update_sink_wants().await?;
    let _ = state.events.send(ready_event);

    // Heartbeats double as the ping measurement, so they go out at least
    // every `PING_INTERVAL`, more often than the server asks.
    let mut heartbeat = tokio::time::interval(Duration::from_millis(heartbeat).min(PING_INTERVAL));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await;

    let mut keepalive = tokio::time::interval(Duration::from_secs(10));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await;

    let mut video_tick = tokio::time::interval(Duration::from_millis(1));
    video_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    video_tick.tick().await;

    let mut activity = tokio::time::interval(Duration::from_millis(100));
    activity.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut network = tokio::time::interval(NETWORK_REPORT_INTERVAL);
    network.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut report = tokio::time::interval(Duration::from_secs(5));
    report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut video_repair = tokio::time::interval(Duration::from_millis(20));
    video_repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut buffer = vec![0u8; 2048];
    loop {
        tokio::select! {
            _ = activity.tick() => {
                state.expire_speakers();
                state.send_owed_keyframe();
            }
            _ = video_repair.tick(), if !state.video_streams.is_empty() => state.poll_video(),
            _ = network.tick() => {
                let (sent, lost) = (
                    std::mem::take(&mut state.net_sent),
                    std::mem::take(&mut state.net_lost),
                );
                // Also while nothing is going out though video is on: that is
                // how a capture that has stopped gets noticed.
                if sent > 0 || state.video_enabled {
                    let _ = state.events.send(VoiceEvent::VideoNetwork {
                        sent,
                        lost,
                        remb_kbps: state.net_remb_kbps,
                    });
                }
            }
            _ = report.tick() => {
                state.report_received();
                state.report_sent();
            }
            _ = heartbeat.tick() => {
                // A heartbeat still unacknowledged counts as a lost ping.
                if state.ping_pending.take().is_some() {
                    let _ = state.events.send(VoiceEvent::Ping { millis: None });
                }
                let stamp = now_millis();
                let payload = json!({"op": 3, "d": {"t": stamp, "seq_ack": state.last_binary_seq}});
                if let Err(error) = send_json(&mut state.write, payload).await {
                    warn!(%error, server = %state.params.endpoint, "voice heartbeat could not be sent");
                    break;
                }
                state.ping_pending = Some((stamp, std::time::Instant::now()));
            }
            _ = keepalive.tick() => {
                // Abaddon sends this two-byte UDP keepalive every 10 seconds so
                // the NAT mapping and the SFU's SSRC state stay alive. (The
                // server does not echo an eight-byte counter keepalive, so it
                // cannot be used to measure the ping.)
                let _ = state.socket.send_to(&[0x13, 0x37], state.remote).await;
            }
            _ = video_tick.tick() => {
                state.pace_video().await?;
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                if !state.handle_command(command).await? {
                    break;
                }
            }
            message = state.read.next() => {
                let Some(message) = message else {
                    warn!(server = %state.params.endpoint, "voice connection ended without a close");
                    break;
                };
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
                        warn!(?code, reason, server = %state.params.endpoint, "voice server closed the connection");
                        let _ = state.events.send(VoiceEvent::Closed { code, reason });
                        return Ok(());
                    }
                    Ok(Message::Ping(payload)) => {
                        let _ = state.write.send(Message::Pong(payload)).await;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        warn!(%error, server = %state.params.endpoint, "voice connection lost");
                        let _ = state.events.send(VoiceEvent::Closed {
                            code: None,
                            reason: format!("voice ws: {error}"),
                        });
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

    debug!(server = %state.params.endpoint, "voice connection finished");
    let _ = state.events.send(VoiceEvent::Closed {
        code: None,
        reason: String::new(),
    });
    Ok(())
}

impl SessionState {
    async fn handle_command(&mut self, command: Command) -> Result<bool> {
        match command {
            Command::SendAudio {
                opus,
                frame_ms,
                speaking,
            } => {
                if let Some(dave) = &mut self.dave {
                    match dave.encrypt_audio(&opus) {
                        Ok(encrypted) => self.send_rtp_audio(&encrypted).await?,
                        Err(error) => {
                            // DAVE transitions can still be pending when the
                            // first microphone frame arrives. Drop that frame
                            // rather than tearing down the whole voice session.
                            debug!(%error, "dropping audio frame until DAVE is ready");
                        }
                    }
                } else {
                    self.send_rtp_audio(&opus).await?;
                }
                self.ts_audio = self.ts_audio.wrapping_add(frame_ms * 48);
                if self.speaking != speaking {
                    self.set_speaking(speaking).await?;
                }
            }
            Command::SendVideo { frame, frame_ms } => {
                let frame_size = frame.len();
                let keyframe = rtp::is_keyframe(&frame);
                if self.video_ssrc.is_none() {
                    tracing::debug!(bytes = frame_size, "dropping video frame: no video ssrc");
                    return Ok(true);
                }
                let encrypted = match &mut self.dave {
                    Some(dave) => match dave.encrypt_video(&frame) {
                        Ok(frame) => frame,
                        Err(error) => {
                            if dave.is_ready() && !crate::dave::video_frame_fits(&frame) {
                                // Not a matter of waiting: this encoder's
                                // frames will never be sent.
                                tracing::warn!(
                                    bytes = frame_size,
                                    "video frame is in too many slices to be encrypted; dropped"
                                );
                            } else {
                                debug!(%error, "dropping video frame until DAVE is ready");
                            }
                            self.sent.dropped += 1;
                            return Ok(true);
                        }
                    },
                    None => frame,
                };
                let packets = rtp::packetize(&encrypted, MAX_PAYLOAD);
                tracing::debug!(
                    bytes = frame_size,
                    packets = packets.len(),
                    frame_ms,
                    queued = self.video_queue.len(),
                    ssrc = self.video_ssrc,
                    "queueing video frame"
                );
                if !self.first_video_logged {
                    tracing::info!(
                        bytes = frame_size,
                        packets = packets.len(),
                        frame_ms,
                        ssrc = self.video_ssrc,
                        "queueing first video frame"
                    );
                    self.first_video_logged = true;
                }
                let timestamp = self.ts_video;
                self.note_video_rate(frame_size);
                if self.video_queue.len() + packets.len() > 4096 {
                    tracing::warn!(
                        queued = self.video_queue.len(),
                        packets = packets.len(),
                        "dropping video frame: RTP pacing queue is full"
                    );
                    self.sent.dropped += 1;
                } else {
                    let backlog_ms =
                        self.video_queue_bytes as f64 / self.pacing_bytes_per_sec() * 1000.0;
                    self.sent.frames += 1;
                    self.sent.packets += packets.len() as u64;
                    self.sent.bytes += frame_size as u64;
                    self.sent.backlog_max_ms = self.sent.backlog_max_ms.max(backlog_ms as u64);
                    if backlog_ms > 250.0
                        && self
                            .last_backlog_warn
                            .is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
                    {
                        tracing::warn!(
                            queued = self.video_queue.len(),
                            backlog_ms = backlog_ms as u64,
                            pacing_kbps = (self.pacing_bytes_per_sec() * 8.0 / 1000.0) as u64,
                            "video RTP pacing backlog"
                        );
                        self.last_backlog_warn = Some(std::time::Instant::now());
                    }
                    let count = packets.len() as u16;
                    if keyframe {
                        // Seen next to resend requests, this shows whether
                        // keyframes are what goes missing.
                        info!(bytes = frame_size, packets = count, "sending a keyframe");
                    }
                    for (index, (payload, marker)) in packets.into_iter().enumerate() {
                        self.video_queue_bytes += payload.len();
                        let place = PacketPlace {
                            index: index as u16,
                            count,
                            keyframe,
                        };
                        self.video_queue
                            .push_back((payload, marker, timestamp, place));
                    }
                }
                self.ts_video = self.ts_video.wrapping_add(video_rtp_step(frame_ms));
            }
            Command::SetSpeaking(flags) => {
                self.set_speaking(flags).await?;
            }
            Command::EndTalkBurst { frame_ms } => {
                for _ in 0..SILENCE_FRAMES {
                    self.send_rtp_audio(&OPUS_SILENCE).await?;
                    self.ts_audio = self.ts_audio.wrapping_add(frame_ms * 48);
                }
                self.set_speaking(0).await?;
            }
            Command::SkipAudio { frame_ms } => {
                self.ts_audio = self.ts_audio.wrapping_add(frame_ms * 48);
            }
            Command::SetVideo(enabled) => {
                tracing::info!(enabled, video_ssrc = self.video_ssrc, "set video state");
                self.video_enabled = enabled;
                self.send_video_state().await?;
            }
            Command::SetRedundancy(enabled) => {
                tracing::info!(enabled, "sending video packets twice");
                self.redundancy.enabled = enabled;
                if !enabled {
                    self.redundancy.due.clear();
                }
            }
            Command::SetLossKeyframeSpacing(spacing) => {
                self.loss_keyframe_spacing = spacing.max(Duration::from_millis(250));
            }
            Command::SetRetransmitWindow(window) => {
                tracing::info!(
                    window_ms = window.as_millis() as u64,
                    "retransmit window set"
                );
                self.retransmit_window = window;
            }
            Command::Disconnect => return Ok(false),
        }
        Ok(true)
    }

    /// Tracks the bitrate of the video being queued (EWMA over 500 ms windows).
    fn note_video_rate(&mut self, frame_bytes: usize) {
        self.video_rate_window_bytes += frame_bytes;
        let elapsed = self.video_rate_window_start.elapsed().as_secs_f64();
        if elapsed >= 0.5 {
            let instant = self.video_rate_window_bytes as f64 * 8.0 / elapsed;
            self.video_rate_bps = if self.video_rate_bps == 0.0 {
                instant
            } else {
                self.video_rate_bps * 0.8 + instant * 0.2
            };
            self.video_rate_window_start = std::time::Instant::now();
            self.video_rate_window_bytes = 0;
        }
    }

    /// Pacing rate: 2.5x the stream's bitrate (libwebrtc's pacing factor), so
    /// a large frame is spread over a few frame intervals instead of leaving
    /// as one burst, with a floor so a scene change after a quiet screen is
    /// not held back.
    fn pacing_bytes_per_sec(&self) -> f64 {
        // Second copies go through the same pacer.
        let copies = if self.redundancy.enabled { 2.0 } else { 1.0 };
        (self.video_rate_bps * PACING_FACTOR * copies).max(PACING_FLOOR_BPS) / 8.0
    }

    /// Sends queued video at the pacing rate. The old pacer sent 1 packet/ms
    /// and jumped to 4 packets/ms (~38 Mbit/s) whenever a keyframe was queued;
    /// the SFU NACKed runs of 3-4 packets out of exactly those bursts.
    async fn pace_video(&mut self) -> Result<()> {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.pace_last).as_secs_f64();
        self.pace_last = now;
        for sequence in self.redundancy.take_due(now) {
            if let Some(original) = self.sent_video.get(sequence)
                && let Some(packet) = self.retransmission(sequence, &original)
            {
                self.sent.sent_twice += 1;
                self.retransmits.push_back(packet);
            }
        }
        let rate = self.pacing_bytes_per_sec();
        // Idle credit is capped so a quiet period cannot be spent as a burst.
        let cap = (rate * 0.005).max(MAX_PAYLOAD as f64);
        self.pace_credit = (self.pace_credit + rate * elapsed).min(cap);
        let mut sent = 0;
        while self.pace_credit > 0.0 && sent < PACING_MAX_PACKETS_PER_TICK {
            if let Some(packet) = self.retransmits.pop_front() {
                self.pace_credit -= packet.len() as f64;
                let _ = self.socket.send_to(&packet, self.remote).await;
            } else if let Some((payload, marker, timestamp, place)) = self.video_queue.pop_front() {
                self.video_queue_bytes = self.video_queue_bytes.saturating_sub(payload.len());
                self.pace_credit -= (payload.len() + PACKET_OVERHEAD) as f64;
                self.send_rtp_video(&payload, marker, timestamp, place)
                    .await?;
            } else {
                break;
            }
            sent += 1;
        }
        Ok(())
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
        let sequence = self.seq_audio;
        self.seq_audio = self.seq_audio.wrapping_add(1);
        let packet = self.srtp.encrypt(&header, payload)?;
        self.sent_audio
            .insert(sequence, &packet, self.retransmit_window);
        self.socket
            .send_to(&packet, self.remote)
            .await
            .map_err(|error| Error::Message(format!("voice udp send: {error}")))?;
        Ok(())
    }

    /// Builds the retransmission of a video packet. Discord repairs video
    /// the WebRTC way (RFC 4588): on the stream's own retransmission SSRC,
    /// with its payload type and sequence numbers, the lost packet's
    /// sequence number leading the payload. A copy of the original packet
    /// on the main SSRC is not used for repair: viewers stayed frozen until
    /// the next keyframe.
    fn retransmission(&mut self, sequence: u16, original: &VideoPacket) -> Option<Vec<u8>> {
        let (header, payload) = match (self.rtx_ssrc, self.video_ssrc) {
            (Some(rtx_ssrc), _) => {
                let rtx_sequence = self.seq_rtx;
                self.seq_rtx = self.seq_rtx.wrapping_add(1);
                (
                    RtpHeader::write(
                        original.marker,
                        rtp::PT_H264_RTX,
                        rtx_sequence,
                        original.timestamp,
                        rtx_ssrc,
                    ),
                    rtx_payload(sequence, &original.payload),
                )
            }
            // No retransmission stream was assigned: the original again.
            (None, Some(ssrc)) => (
                RtpHeader::write(original.marker, PT_H264, sequence, original.timestamp, ssrc),
                original.payload.clone(),
            ),
            (None, None) => return None,
        };
        self.srtp.encrypt(&header, &payload).ok()
    }

    async fn send_rtp_video(
        &mut self,
        payload: &[u8],
        marker: bool,
        timestamp: u32,
        place: PacketPlace,
    ) -> Result<()> {
        let ssrc = self.video_ssrc.unwrap_or(self.audio_ssrc);
        let sequence = self.seq_video;
        let header = RtpHeader::write(marker, PT_H264, sequence, timestamp, ssrc);
        self.seq_video = self.seq_video.wrapping_add(1);
        let packet = self.srtp.encrypt(&header, payload)?;
        let now = std::time::Instant::now();
        let idle_before = self
            .last_video_send
            .map(|last| now.duration_since(last))
            .unwrap_or_default();
        self.last_video_send = Some(now);
        self.sent_video.insert(
            sequence,
            VideoPacket {
                payload: payload.to_vec(),
                marker,
                timestamp,
                place,
                sent: now,
                idle_before,
            },
            self.retransmit_window,
        );
        self.redundancy.sent(sequence, now);
        self.net_sent += 1;
        if self
            .test_drop
            .is_some_and(|every| u32::from(sequence) % every == 0)
        {
            return Ok(());
        }
        self.socket
            .send_to(&packet, self.remote)
            .await
            .map_err(|error| Error::Message(format!("voice udp send: {error}")))?;
        Ok(())
    }

    async fn handle_json(&mut self, value: Value) -> Result<bool> {
        let op = value["op"].as_i64();
        let keep_going = self.handle_json_op(value).await?;
        // Someone's SSRC became known, or they left: what to ask for changed.
        if matches!(op, Some(5 | 12 | 13 | 14)) {
            self.update_sink_wants().await?;
        }
        Ok(keep_going)
    }

    async fn handle_json_op(&mut self, value: Value) -> Result<bool> {
        let op = value["op"].as_i64().unwrap_or(-1);
        let d = &value["d"];
        if op != 6 {
            debug!(
                op,
                user = d["user_id"].as_str().unwrap_or(""),
                ssrc = d["ssrc"].as_u64().or(d["audio_ssrc"].as_u64()),
                "voice gateway op"
            );
        }
        match op {
            5 => {
                let Some(user_id) = d["user_id"]
                    .as_str()
                    .and_then(|id| id.parse::<Snowflake>().ok())
                else {
                    return Ok(true);
                };
                // This announces which SSRC belongs to the user. It is not a
                // per-utterance signal: speaking is derived from the audio
                // itself, in `handle_udp`.
                let ssrc = d["ssrc"].as_u64().unwrap_or(0) as u32;
                if ssrc != 0 {
                    self.audio_ssrc_to_user.insert(ssrc, user_id);
                    self.users.insert(user_id.get());
                    debug!(%user_id, ssrc, "audio ssrc announced");
                }
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
                    if let Some(ssrc) = d["audio_ssrc"].as_u64().filter(|ssrc| *ssrc != 0) {
                        self.audio_ssrc_to_user.insert(ssrc as u32, user_id);
                        self.users.insert(user_id.get());
                        debug!(%user_id, ssrc, "audio ssrc announced with video state");
                    }
                    self.note_video_state(user_id, d);
                }
            }
            13 => {
                if let Some(user_id) = d["user_id"]
                    .as_str()
                    .and_then(|id| id.parse::<Snowflake>().ok())
                {
                    self.users.remove(&user_id.get());
                    self.end_video(user_id);
                    if self.voice_activity.remove(&user_id).is_some() {
                        let _ = self.events.send(VoiceEvent::Speaking {
                            user_id,
                            ssrc: 0,
                            speaking: false,
                        });
                    }
                    let _ = self.events.send(VoiceEvent::ClientDisconnect { user_id });
                }
            }
            14 => {
                // Session update: Abaddon refreshes the SSRC mapping here; the
                // speaking event is not guaranteed for every participant.
                if let Some(user_id) = d["user_id"]
                    .as_str()
                    .and_then(|id| id.parse::<Snowflake>().ok())
                {
                    self.users.insert(user_id.get());
                    if let Some(ssrc) = d["audio_ssrc"].as_u64() {
                        self.audio_ssrc_to_user.insert(ssrc as u32, user_id);
                    }
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
            // What the server wants from our own outgoing video.
            15 => debug!(wants = %d, "media sink wants"),
            // Heartbeat acknowledged: the time since it was sent is the ping.
            6 => {
                if let Some((stamp, sent)) = self.ping_pending
                    && d["t"].as_u64().is_none_or(|echoed| echoed == stamp)
                {
                    self.ping_pending = None;
                    let millis = sent.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
                    let _ = self.events.send(VoiceEvent::Ping {
                        millis: Some(millis),
                    });
                }
            }
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
        if is_rtcp(packet) {
            self.handle_rtcp(packet);
            return;
        }
        let Some((header, _)) = RtpHeader::parse(packet) else {
            return;
        };
        if header.ssrc == self.audio_ssrc || self.video_ssrc == Some(header.ssrc) {
            return;
        }

        let mut payload = match self.srtp.decrypt(packet) {
            Ok(payload) => payload,
            Err(error) => {
                debug!(
                    %error,
                    ssrc = header.ssrc,
                    pt = header.payload_type,
                    sequence = header.sequence,
                    len = packet.len(),
                    header_len = header.header_len,
                    payload_offset = header.payload_offset,
                    head = %packet.iter().take(24).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(""),
                    "SRTP decrypt failed"
                );
                self.received.transport_failed += 1;
                return;
            }
        };

        // The encrypted region includes the RTP extension block; the codec
        // payload starts after it. Abaddon strips this in GetPayloadOffset.
        let skip = header.payload_offset.saturating_sub(header.header_len);
        if payload.len() < skip {
            return;
        }
        payload.drain(..skip);
        // With the padding bit set, the last byte counts the padding bytes
        // (itself included) to remove from the end.
        if packet[0] & 0x20 != 0 {
            let padding = payload.last().copied().unwrap_or(0) as usize;
            if padding == 0 || padding > payload.len() {
                return;
            }
            payload.truncate(payload.len() - padding);
        }

        match header.payload_type {
            PT_OPUS => {
                let Some(user_id) = self.audio_ssrc_to_user.get(&header.ssrc).copied() else {
                    self.received.unknown_ssrc += 1;
                    if self.unknown_ssrcs.insert(header.ssrc) {
                        debug!(ssrc = header.ssrc, "audio from an ssrc with no user yet");
                    }
                    return;
                };
                // The same packet delivered twice must not be decrypted (the
                // end-to-end layer rejects reuse) or played twice.
                if let Some(last) = self.audio_sequences.insert(header.ssrc, header.sequence) {
                    match header.sequence.wrapping_sub(last) as i16 {
                        0 => {
                            self.received.duplicates += 1;
                            return;
                        }
                        ahead @ 2..=1000 => self.received.missing += ahead as u64 - 1,
                        behind if behind < 0 => {
                            // A late packet: keep the newest as the reference.
                            self.audio_sequences.insert(header.ssrc, last);
                        }
                        _ => {}
                    }
                }
                // Silence packets bypass DAVE per the protocol.
                let silence = payload == [0xF8, 0xFF, 0xFE];
                let opus = if silence {
                    self.received.silence += 1;
                    payload
                } else {
                    // Without a DAVE session the payload is plain Opus.
                    let opus = match self.dave.as_mut() {
                        Some(dave) => dave.decrypt_audio(user_id.get(), &payload),
                        None => Ok(payload),
                    };
                    let opus = match opus {
                        Ok(opus) => opus,
                        Err(reason) => {
                            match reason {
                                FrameRejected::Replayed => self.received.e2ee_replayed += 1,
                                FrameRejected::NotReady => self.received.e2ee_not_ready += 1,
                                FrameRejected::Unencrypted | FrameRejected::Corrupt => {
                                    self.received.e2ee_failed += 1;
                                    debug!(
                                        %user_id,
                                        ?reason,
                                        sequence = header.sequence,
                                        bytes = skip,
                                        "voice frame rejected by end-to-end layer"
                                    );
                                }
                            }
                            return;
                        }
                    };
                    if self.received.audio == 0 {
                        info!(%user_id, ssrc = header.ssrc, bytes = opus.len(), "first voice packet received");
                    }
                    self.received.audio += 1;
                    if self
                        .voice_activity
                        .insert(user_id, std::time::Instant::now())
                        .is_none()
                    {
                        let _ = self.events.send(VoiceEvent::Speaking {
                            user_id,
                            ssrc: header.ssrc,
                            speaking: true,
                        });
                    }
                    opus
                };
                let _ = self.events.send(VoiceEvent::Audio {
                    user_id,
                    ssrc: header.ssrc,
                    opus,
                    sequence: header.sequence,
                    timestamp: header.timestamp,
                });
            }
            PT_H264 | rtp::PT_H264_RTX => self.receive_video(&header, payload),
            _ => {}
        }
    }

    /// Records which video SSRCs a participant announced (voice opcode 12)
    /// and ends their video when they announce none.
    fn note_video_state(&mut self, user_id: Snowflake, d: &Value) {
        debug!(%user_id, state = %d, "video state");
        let on = d["video_ssrc"].as_u64().unwrap_or(0) != 0;
        let mut layers: Vec<(u32, Option<u32>)> = Vec::new();
        match d["streams"].as_array() {
            Some(streams) => {
                for stream in streams {
                    let ssrc = stream["ssrc"].as_u64().unwrap_or(0) as u32;
                    if ssrc == 0 || stream["active"].as_bool() == Some(false) {
                        continue;
                    }
                    let repair = stream["rtx_ssrc"].as_u64().unwrap_or(0) as u32;
                    layers.push((ssrc, (repair != 0).then_some(repair)));
                }
            }
            None if on => layers.push((d["video_ssrc"].as_u64().unwrap_or(0) as u32, None)),
            None => {}
        }
        self.video_ssrcs.retain(|_, info| info.user != user_id);
        if !on || layers.is_empty() {
            self.end_video(user_id);
            return;
        }
        for (primary, repair) in layers {
            self.video_ssrcs.insert(
                primary,
                VideoSsrc {
                    user: user_id,
                    primary,
                    retransmission: false,
                },
            );
            if let Some(repair) = repair {
                self.video_ssrcs.insert(
                    repair,
                    VideoSsrc {
                        user: user_id,
                        primary,
                        retransmission: true,
                    },
                );
            }
        }
    }

    /// The media-sink wants (voice op 15) the web client sends: each other
    /// participant's audio SSRC at full quality and nothing else by default,
    /// so the server sends their media (camera included) and no one else's.
    /// Watching a stream keeps asking for everything: only the streamer
    /// sends on that connection.
    fn sink_wants(&self) -> Value {
        if self.params.receive_only {
            return json!({"any": 100});
        }
        let mut ssrcs: Vec<u32> = self
            .audio_ssrc_to_user
            .iter()
            .filter(|(ssrc, user)| **ssrc != self.audio_ssrc && self.users.contains(&user.get()))
            .map(|(ssrc, _)| *ssrc)
            .collect();
        ssrcs.sort_unstable();
        let mut wants = serde_json::Map::new();
        for ssrc in ssrcs {
            wants.insert(ssrc.to_string(), json!(100));
        }
        wants.insert("any".to_owned(), json!(0));
        Value::Object(wants)
    }

    /// Sends the media-sink wants when they changed (someone's SSRC became
    /// known, or they left).
    async fn update_sink_wants(&mut self) -> Result<()> {
        let wants = self.sink_wants();
        if self.sink_wants.as_ref() == Some(&wants) {
            return Ok(());
        }
        send_json(&mut self.write, json!({"op": 15, "d": wants})).await?;
        self.sink_wants = Some(wants);
        Ok(())
    }

    fn end_video(&mut self, user_id: Snowflake) {
        self.video_ssrcs.retain(|_, info| info.user != user_id);
        if self.video_streams.remove(&user_id).is_some() {
            info!(%user_id, "video ended");
            let _ = self.events.send(VoiceEvent::VideoEnded { user_id });
        }
    }

    /// One video packet, already through transport decryption.
    fn receive_video(&mut self, header: &RtpHeader, payload: Vec<u8>) {
        self.received.video_packets += 1;
        let Some(info) = self.video_ssrcs.get(&header.ssrc).copied() else {
            self.received.video_unknown_ssrc += 1;
            if self.unknown_ssrcs.insert(header.ssrc) {
                debug!(
                    ssrc = header.ssrc,
                    payload_type = header.payload_type,
                    "video from an ssrc with no user yet"
                );
            }
            return;
        };
        let (sequence, payload) = if info.retransmission {
            // A retransmission starts with the sequence number the packet
            // was first sent under. Shorter ones are bandwidth probes.
            if payload.len() <= 2 {
                return;
            }
            self.received.video_repaired += 1;
            (
                u16::from_be_bytes([payload[0], payload[1]]),
                payload[2..].to_vec(),
            )
        } else {
            (header.sequence, payload)
        };

        let now = Instant::now();
        let mut stream = match self.video_streams.remove(&info.user) {
            Some(stream) if stream.ssrc == info.primary => stream,
            _ => {
                info!(user_id = %info.user, ssrc = info.primary, "receiving video");
                VideoStream::new(info.primary, now)
            }
        };
        let mut ready = Vec::new();
        stream.push(
            Packet {
                sequence,
                timestamp: header.timestamp,
                marker: header.marker,
                payload,
            },
            now,
            &mut ready,
        );
        self.finish_video(info.user, &mut stream, ready, now);
        self.video_streams.insert(info.user, stream);
    }

    /// Gives up on overdue retransmissions and repeats pending requests.
    fn poll_video(&mut self) {
        let now = Instant::now();
        let users: Vec<Snowflake> = self.video_streams.keys().copied().collect();
        for user_id in users {
            let Some(mut stream) = self.video_streams.remove(&user_id) else {
                continue;
            };
            let mut ready = Vec::new();
            stream.poll(now, &mut ready);
            self.finish_video(user_id, &mut stream, ready, now);
            self.video_streams.insert(user_id, stream);
        }
    }

    /// Decrypts and delivers assembled frames, then sends the sender any
    /// feedback the stream needs (missing packets, a keyframe).
    fn finish_video(
        &mut self,
        user_id: Snowflake,
        stream: &mut VideoStream,
        ready: Vec<Assembled>,
        now: Instant,
    ) {
        for item in ready {
            let (frame, timestamp) = match item {
                Assembled::Frame(frame, timestamp) => (frame, timestamp),
                Assembled::Lost => {
                    self.received.video_lost += 1;
                    continue;
                }
            };
            let frame = match self.dave.as_mut() {
                Some(dave) => dave.decrypt_video(user_id.get(), &frame),
                None => Ok(frame),
            };
            match frame {
                Ok(frame) if stream.admit(&frame) => {
                    self.received.video_frames += 1;
                    let keyframe = rtp::is_keyframe(&frame);
                    let _ = self.events.send(VoiceEvent::Video {
                        user_id,
                        ssrc: stream.ssrc,
                        frame,
                        keyframe,
                        timestamp,
                    });
                }
                Ok(_) => self.received.video_held += 1,
                Err(FrameRejected::Replayed) => {}
                Err(reason) => {
                    if self.received.video_e2ee_failed == 0 {
                        debug!(%user_id, ?reason, "video frame rejected by end-to-end layer");
                    }
                    self.received.video_e2ee_failed += 1;
                    stream.require_keyframe();
                }
            }
        }

        let missing = stream.missing(now);
        if !missing.is_empty() {
            self.received.video_nacked += missing.len() as u64;
            self.send_rtcp(&rtcp::nack(self.feedback_ssrc(), stream.ssrc, &missing));
        }
        if stream.should_request_keyframe(now, KEYFRAME_REQUEST_INTERVAL) {
            self.received.video_keyframe_requests += 1;
            self.send_rtcp(&rtcp::picture_loss(self.feedback_ssrc(), stream.ssrc));
        }
    }

    /// The SSRC our feedback about other people's video is sent under.
    fn feedback_ssrc(&self) -> u32 {
        self.video_ssrc.unwrap_or(self.audio_ssrc)
    }

    fn send_rtcp(&mut self, packet: &[u8]) {
        match self.srtp.encrypt_rtcp(packet) {
            // Feedback is best effort: if the socket is busy it is asked
            // again on the next round.
            Ok(encrypted) => {
                let _ = self.socket.try_send_to(&encrypted, self.remote);
            }
            Err(error) => debug!(%error, "could not encrypt RTCP feedback"),
        }
    }

    /// Ends the speaking state of participants whose audio stopped.
    fn expire_speakers(&mut self) {
        let events = &self.events;
        self.voice_activity.retain(|user_id, last| {
            let speaking = last.elapsed() < SPEAKING_HOLD;
            if !speaking {
                let _ = events.send(VoiceEvent::Speaking {
                    user_id: *user_id,
                    ssrc: 0,
                    speaking: false,
                });
            }
            speaking
        });
    }

    /// Sends the keyframe owed for a loss that came too soon after the
    /// last one, now that the spacing has passed.
    fn send_owed_keyframe(&mut self) {
        if self.loss_keyframe_owed
            && self
                .last_loss_keyframe
                .is_none_or(|last| last.elapsed() >= self.loss_keyframe_spacing)
        {
            self.loss_keyframe_owed = false;
            self.last_loss_keyframe = Some(std::time::Instant::now());
            info!("sending the keyframe owed for an earlier loss");
            let _ = self.events.send(VoiceEvent::ForceKeyframe);
        }
    }

    /// Logs what happened to sent video over the last period, when any was
    /// sent. A viewer's freeze shows here as keyframe requests, packets
    /// asked for again that we no longer had, a backlog, or drops.
    fn report_sent(&mut self) {
        let sent = std::mem::take(&mut self.sent);
        if sent.frames == 0 && sent.dropped == 0 {
            return;
        }
        info!(
            frames = sent.frames,
            packets = sent.packets,
            kbit = sent.bytes * 8 / 1000,
            dropped = sent.dropped,
            nack_requested = sent.nack_requested,
            nack_found = sent.nack_found,
            nack_repeated = sent.nack_repeated,
            nack_first = sent.nack_first,
            nack_after_pause = sent.nack_after_pause,
            sent_twice = sent.sent_twice,
            audio_nack_requested = sent.audio_nack_requested,
            keyframe_requests = sent.keyframe_requests,
            backlog_max_ms = sent.backlog_max_ms,
            loss_max_percent = sent.loss_max * 100 / 256,
            remb_kbit = sent.remb_bps / 1000,
            "sent video"
        );
    }

    /// Logs the receive counters when they moved since the last report.
    fn report_received(&mut self) {
        if self.received == self.reported {
            return;
        }
        let now = self.received;
        debug!(
            audio = now.audio,
            silence = now.silence,
            video_packets = now.video_packets,
            video_frames = now.video_frames,
            video_repaired = now.video_repaired,
            video_nacked = now.video_nacked,
            video_lost = now.video_lost,
            video_held = now.video_held,
            video_keyframe_requests = now.video_keyframe_requests,
            video_e2ee_failed = now.video_e2ee_failed,
            video_unknown_ssrc = now.video_unknown_ssrc,
            unknown_ssrc = now.unknown_ssrc,
            transport_failed = now.transport_failed,
            duplicates = now.duplicates,
            missing = now.missing,
            e2ee_replayed = now.e2ee_replayed,
            e2ee_not_ready = now.e2ee_not_ready,
            e2ee_failed = now.e2ee_failed,
            known_ssrcs = self.audio_ssrc_to_user.len(),
            dave_ready = self.dave.as_ref().is_some_and(|dave| dave.is_ready()),
            "received media"
        );
        self.reported = now;
    }

    fn handle_rtcp(&mut self, packet: &[u8]) {
        if packet.len() < 8 {
            return;
        }
        let cleartext = match self.srtp.decrypt_rtcp(packet) {
            Ok(packet) => packet,
            Err(error) => {
                if !self.srtcp_error_logged {
                    tracing::info!(%error, len = packet.len(), "SRTCP decrypt failed");
                    self.srtcp_error_logged = true;
                }
                // Undecrypted feedback is ciphertext past the header; parsing
                // it would retransmit and force keyframes on garbage.
                return;
            }
        };
        // One datagram usually stacks several RTCP packets (RR + NACK, NACK for
        // video + NACK for audio, ...). Walk them by their length fields; the
        // old code parsed only the first and read the rest as its payload.
        let mut offset = 0;
        while offset + 8 <= cleartext.len() && cleartext[offset] >> 6 == 2 {
            let words = u16::from_be_bytes([cleartext[offset + 2], cleartext[offset + 3]]) as usize;
            let end = (offset + (words + 1) * 4).min(cleartext.len());
            self.handle_rtcp_packet(&cleartext[offset..end]);
            offset = end;
        }
    }

    fn handle_rtcp_packet(&mut self, cleartext: &[u8]) {
        if cleartext.len() < 8 {
            return;
        }
        let packet_type = cleartext[1];
        let fmt = cleartext[0] & 0x1F;
        if packet_type == 201 {
            let report_count = (cleartext[0] & 0x1F) as usize;
            let mut offset = 8;
            for _ in 0..report_count {
                if offset + 24 > cleartext.len() {
                    break;
                }
                let media = u32::from_be_bytes([
                    cleartext[offset],
                    cleartext[offset + 1],
                    cleartext[offset + 2],
                    cleartext[offset + 3],
                ]);
                let fraction_lost = cleartext[offset + 4];
                let cumulative_lost = u32::from_be_bytes([
                    cleartext[offset + 5],
                    cleartext[offset + 6],
                    cleartext[offset + 7],
                    cleartext[offset + 8],
                ]) & 0x00FF_FFFF;
                let jitter = u32::from_be_bytes([
                    cleartext[offset + 12],
                    cleartext[offset + 13],
                    cleartext[offset + 14],
                    cleartext[offset + 15],
                ]);
                tracing::debug!(
                    media,
                    fraction_lost,
                    cumulative_lost,
                    jitter,
                    "RTCP receiver report"
                );
                if self.video_ssrc == Some(media) {
                    self.sent.loss_max = self.sent.loss_max.max(u32::from(fraction_lost));
                }
                offset += 24;
            }
        }
        match packet_type {
            205 => tracing::debug!(
                fmt,
                len = cleartext.len(),
                media = cleartext
                    .get(8..12)
                    .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])),
                "RTCP video NACK/RTPFB received"
            ),
            // Bandwidth estimates (15) come constantly; anything else here
            // is a viewer's picture feedback and worth seeing in the log.
            206 if fmt != 15 => info!(
                fmt,
                len = cleartext.len(),
                media = cleartext
                    .get(8..12)
                    .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])),
                video_ssrc = self.video_ssrc,
                rtx_ssrc = self.rtx_ssrc,
                "picture feedback received"
            ),
            _ => {}
        }
        // REMB (206/15 application-layer feedback): Discord's estimate of the
        // bandwidth available to us. If our send bitrate exceeds this, the path
        // drops packets and the viewer NACKs. FCI is "REMB", a count byte, then
        // a 6-bit exponent + 18-bit mantissa; bitrate = mantissa << exp.
        if packet_type == 206 && fmt == 15 && cleartext.len() >= 20 && &cleartext[12..16] == b"REMB"
        {
            let exp = (cleartext[17] >> 2) & 0x3F;
            let mantissa = (((cleartext[17] & 0x03) as u32) << 16)
                | ((cleartext[18] as u32) << 8)
                | (cleartext[19] as u32);
            let bitrate_bps = (mantissa as u64) << exp;
            tracing::debug!(bitrate_bps, "RTCP REMB bandwidth estimate");
            self.sent.remb_bps = bitrate_bps;
            self.net_remb_kbps = (bitrate_bps / 1000).min(u64::from(u32::MAX)) as u32;
        }
        if packet_type == 205 && fmt == 1 && cleartext.len() >= 16 {
            let media =
                u32::from_be_bytes([cleartext[8], cleartext[9], cleartext[10], cleartext[11]]);
            // The server NACKs audio as well as video; an unanswered audio
            // NACK is repeated ~7 times and the gap is heard by the viewer.
            let is_video = self.video_ssrc == Some(media);
            if !is_video && media != self.audio_ssrc {
                return;
            }
            let mut offset = 12;
            let mut requested = 0usize;
            // A video packet asked for a second time: its resend was lost too.
            let mut repeated = false;
            let mut found = 0usize;
            let mut sample = Vec::new();
            // For the log: each packet as sequence:index/count in its frame
            // (K for a keyframe) and how long ago it was first sent.
            let mut detail: Vec<String> = Vec::new();
            while offset + 4 <= cleartext.len() {
                let pid = u16::from_be_bytes([cleartext[offset], cleartext[offset + 1]]);
                let blp = u16::from_be_bytes([cleartext[offset + 2], cleartext[offset + 3]]);
                offset += 4;
                let mut want = vec![pid];
                for bit in 1..=16u16 {
                    if blp & (1 << (bit - 1)) != 0 {
                        want.push(pid.wrapping_add(bit));
                    }
                }
                for sequence in want {
                    if is_video {
                        let now = std::time::Instant::now();
                        if self
                            .recent_nacks
                            .insert(sequence, now)
                            .is_some_and(|before| {
                                now.duration_since(before) < Duration::from_secs(1)
                            })
                        {
                            self.sent.nack_repeated += 1;
                            repeated = true;
                        }
                    }
                    requested += 1;
                    if sample.len() < 8 {
                        sample.push(sequence);
                    }
                    if !is_video {
                        // Audio has no retransmission stream: the packet
                        // goes out again as it was.
                        if let Some(packet) = self.sent_audio.get(sequence) {
                            found += 1;
                            self.retransmits.push_back(packet);
                        }
                    } else if let Some(original) = self.sent_video.get(sequence) {
                        found += 1;
                        if original.place.index == 0 {
                            self.sent.nack_first += 1;
                        }
                        if original.idle_before >= Duration::from_millis(5) {
                            self.sent.nack_after_pause += 1;
                        }
                        if detail.len() < 6 {
                            // sequence:place in frame[K]@age, then the pause
                            // before it was sent and its size.
                            detail.push(format!(
                                "{sequence}:{}/{}{}@{}ms,p{:.1},b{}",
                                original.place.index,
                                original.place.count,
                                if original.place.keyframe { "K" } else { "" },
                                original.sent.elapsed().as_millis(),
                                original.idle_before.as_secs_f64() * 1000.0,
                                original.payload.len()
                            ));
                        }
                        if let Some(packet) = self.retransmission(sequence, &original) {
                            self.retransmits.push_back(packet);
                        }
                    }
                }
            }
            if self.recent_nacks.len() > 2048 {
                let now = std::time::Instant::now();
                self.recent_nacks
                    .retain(|_, at| now.duration_since(*at) < Duration::from_secs(1));
            }
            if is_video {
                self.net_lost += requested as u32;
                // Logged as it happens, so a hitch someone saw can be matched
                // to the second.
                info!(
                    requested,
                    found,
                    packets = detail.join(" "),
                    "video packets asked for again"
                );
                let now = std::time::Instant::now();
                let since_last = self.last_loss_keyframe.map(|last| now.duration_since(last));
                match loss_keyframe(
                    found < requested,
                    repeated,
                    since_last,
                    self.loss_keyframe_spacing,
                ) {
                    LossKeyframe::Now => {
                        info!(
                            missing = requested - found,
                            repeated, "a resend cannot repair this loss; sending a keyframe"
                        );
                        self.last_loss_keyframe = Some(now);
                        self.loss_keyframe_owed = false;
                        let _ = self.events.send(VoiceEvent::ForceKeyframe);
                    }
                    LossKeyframe::Owed => self.loss_keyframe_owed = true,
                    LossKeyframe::None => {}
                }
                self.sent.nack_requested += requested as u64;
                self.sent.nack_found += found as u64;
            } else {
                self.sent.audio_nack_requested += requested as u64;
            }
            // `sample` advancing across NACKs = genuine ongoing loss; the same
            // seqs repeating = our retransmits aren't landing. `found < requested`
            // = the viewer wants packets we already evicted from `sent_video`.
            tracing::debug!(
                media,
                requested,
                found,
                ?sample,
                buffered = self.sent_video.len(),
                "NACK retransmit"
            );
        }
        // PLI (206/1) and FIR (206/4) are the viewer saying "I can't decode,
        // send a keyframe". A frozen viewer repeats them every frame; the
        // encoder-side throttle in `force_keyframe` collapses the storm into at
        // most one IDR per second. The common-header media SSRC is our video
        // SSRC for PLI and is often 0 for FIR (the target lives in the FCI), so
        // accept either.
        if packet_type == 206 && (fmt == 1 || fmt == 4) {
            let media = cleartext
                .get(8..12)
                .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
            if media == self.video_ssrc || media == Some(0) {
                info!(fmt, "a viewer asked for a keyframe");
                self.sent.keyframe_requests += 1;
                let _ = self.events.send(VoiceEvent::ForceKeyframe);
            }
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
                    Some(op) if replayed_after_handshake(op) => pending_json.push(value),
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

/// Whether a JSON opcode that arrives while the handshake is still waiting
/// for the session description must be kept and handled afterwards.
///
/// Right after READY the server describes everyone already in the channel:
/// who is connected (11), which SSRCs are theirs (5, 12) and their client
/// details (18, 20), alongside the DAVE transition opcodes (21..=24). Keeping
/// only the DAVE ones meant people who were in the call before us never got
/// an SSRC mapping, and all their audio was dropped as unknown. Only the
/// handshake's own messages are left out: heartbeat acks (6) and HELLO (8).
fn replayed_after_handshake(op: i64) -> bool {
    !matches!(op, 6 | 8)
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
    fn retargeting_moves_every_clone_to_the_new_connection() {
        let (old_tx, mut old_rx) = mpsc::unbounded_channel();
        let (new_tx, mut new_rx) = mpsc::unbounded_channel();
        let handle = VoiceHandle::new(old_tx);
        // As the microphone and the camera hold it.
        let microphone = handle.clone();
        let previous = handle.retarget(&VoiceHandle::new(new_tx));
        microphone.set_speaking(1);
        assert!(matches!(new_rx.try_recv(), Ok(Command::SetSpeaking(1))));
        assert!(old_rx.try_recv().is_err());
        // The old connection can still be told to go.
        previous.disconnect_now();
        assert!(matches!(old_rx.try_recv(), Ok(Command::Disconnect)));
        assert!(new_rx.try_recv().is_err());
    }

    #[test]
    fn mode_names_round_trip() {
        for mode in [Mode::Aes256GcmRtpsize, Mode::XChaCha20Poly1305Rtpsize] {
            assert_eq!(Mode::parse(mode_name(mode)), Some(mode));
        }
    }

    #[test]
    fn rtp_step_matches_the_real_frame_rate() {
        assert_eq!(video_rtp_step(33), 3000); // 30 fps
        assert_eq!(video_rtp_step(16), 1500); // 60 fps
        assert_eq!(video_rtp_step(41), 3750); // 24 fps
        assert_eq!(video_rtp_step(66), 6000); // 15 fps
        assert_eq!(video_rtp_step(7), 630); // unknown rate falls back
    }

    #[test]
    fn retransmissions_lead_with_the_original_sequence_number() {
        assert_eq!(rtx_payload(0x1234, &[9, 8, 7]), [0x12, 0x34, 9, 8, 7]);
        let mut sent = SentVideo::default();
        let packet = VideoPacket {
            payload: vec![1, 2, 3],
            marker: true,
            timestamp: 90_000,
            place: PacketPlace::default(),
            sent: std::time::Instant::now(),
            idle_before: Duration::ZERO,
        };
        sent.insert(7, packet, Duration::from_secs(1));
        let kept = sent.get(7).unwrap();
        assert_eq!(
            (kept.payload, kept.marker, kept.timestamp),
            (vec![1, 2, 3], true, 90_000)
        );
        assert!(sent.get(8).is_none());
        sent.insert(8, kept_packet(), Duration::ZERO);
        assert_eq!(sent.len(), 0);
    }

    #[test]
    fn a_keyframe_is_only_for_loss_a_resend_cannot_repair() {
        let second = Duration::from_secs(1);
        let recent = Some(Duration::from_millis(300));
        // Resent packets repair an ordinary loss: no keyframe.
        assert_eq!(
            loss_keyframe(false, false, None, second),
            LossKeyframe::None
        );
        assert_eq!(
            loss_keyframe(false, false, recent, second),
            LossKeyframe::None
        );
        // A packet no longer held cannot be resent: a keyframe at once.
        assert_eq!(
            loss_keyframe(true, false, recent, second),
            LossKeyframe::Now
        );
        // Asked for again, the resend was lost too: a keyframe, spaced out.
        assert_eq!(loss_keyframe(false, true, None, second), LossKeyframe::Now);
        assert_eq!(
            loss_keyframe(false, true, Some(second), second),
            LossKeyframe::Now
        );
        assert_eq!(
            loss_keyframe(false, true, recent, second),
            LossKeyframe::Owed
        );
    }

    #[test]
    fn second_copies_come_due_a_little_after_the_first() {
        let start = std::time::Instant::now();
        let mut redundancy = Redundancy::default();
        // Off: nothing is scheduled.
        redundancy.sent(1, start);
        assert!(
            redundancy
                .take_due(start + Duration::from_secs(1))
                .is_empty()
        );

        redundancy.enabled = true;
        redundancy.sent(10, start);
        redundancy.sent(11, start + Duration::from_millis(5));
        assert!(
            redundancy
                .take_due(start + Duration::from_millis(19))
                .is_empty()
        );
        // Each copy waits its own 20 ms, in the order sent.
        assert_eq!(redundancy.take_due(start + Duration::from_millis(20)), [10]);
        assert_eq!(redundancy.take_due(start + Duration::from_millis(40)), [11]);
        assert!(
            redundancy
                .take_due(start + Duration::from_secs(1))
                .is_empty()
        );
    }

    fn kept_packet() -> VideoPacket {
        VideoPacket {
            payload: vec![0],
            marker: false,
            timestamp: 0,
            place: PacketPlace::default(),
            sent: std::time::Instant::now(),
            idle_before: Duration::ZERO,
        }
    }

    #[test]
    fn sent_packets_respect_the_window() {
        let mut sent = SentPackets::default();
        sent.insert(1, b"a", Duration::from_secs(60));
        sent.insert(2, b"b", Duration::from_secs(60));
        assert_eq!(sent.get(1).as_deref(), Some(&b"a"[..]));
        assert_eq!(sent.len(), 2);

        // A zero window drops the history and stores nothing new.
        sent.insert(3, b"c", Duration::ZERO);
        assert_eq!(sent.len(), 0);
        assert!(sent.get(3).is_none());
    }

    #[test]
    fn sent_packets_are_capped_below_the_sequence_space() {
        let mut sent = SentPackets::default();
        for sequence in 0..(MAX_SENT_PACKETS as u32 + 100) {
            sent.insert(sequence as u16, b"x", Duration::from_secs(3600));
        }
        assert_eq!(sent.len(), MAX_SENT_PACKETS);
        assert!(sent.get(0).is_none());
    }

    #[test]
    fn roster_messages_survive_the_handshake() {
        // Speaking, clients connect, video/SSRC state, flags, platform, DAVE.
        for op in [5, 11, 12, 13, 18, 20, 21, 22, 24] {
            assert!(replayed_after_handshake(op), "op {op} must be replayed");
        }
        assert!(!replayed_after_handshake(6));
        assert!(!replayed_after_handshake(8));
    }

    #[test]
    fn rtp_with_the_marker_bit_is_not_rtcp() {
        let rtp = |second: u8| [0x80, second, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
        // Opus (120) and H.264 (101), with and without the marker bit.
        for second in [120, 0x80 | 120, 101, 0x80 | 101, 0x80 | 102] {
            assert!(!is_rtcp(&rtp(second)), "payload byte {second} is RTP");
        }
        // Sender report, receiver report, feedback.
        for second in [200, 201, 205, 206] {
            assert!(is_rtcp(&rtp(second)), "packet type {second} is RTCP");
        }
        assert!(!is_rtcp(&[0x80, 200]), "too short to be either");
    }
}
