//! Per-account session state and the Tokio side of the UI bridge.
//!
//! All Discord I/O lives behind [`Command`]s that the GTK thread sends over an
//! `async_channel`; the Tokio dispatcher and per-account tasks answer over a
//! second `async_channel` as [`UiEvent`]s. No GTK type crosses this boundary.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use kc_core::{Account, Snowflake, Token};
use kc_gateway::session::GatewayHandle;
use kc_gateway::{Gateway, GatewayEvent};
use kc_http::HttpClient;
use kc_media::audio::{ScreenAudio, VoiceAudio};
use kc_media::delay::DelayedSender;
use kc_media::portal::CameraAccess;
use kc_media::video::{VideoSettings, VideoSource};
use kc_state::{Guild, Store, StoreEvent, User};
use kc_voice::{self, VoiceEvent, VoiceHandle, VoiceParams};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Sessions are keyed by a hash of the token, which is known before the first
/// READY while a user id is not.
pub type AccountId = u64;

pub fn account_id(token: &Token) -> AccountId {
    let mut hasher = DefaultHasher::new();
    token.expose().hash(&mut hasher);
    hasher.finish()
}

/// What the header's connection label shows.
#[derive(Clone, Debug, Default)]
pub enum ConnState {
    #[default]
    Connecting,
    Connected,
    Ready,
    Disconnected,
}

impl ConnState {
    pub fn label(&self) -> String {
        match self {
            Self::Connecting => "Connecting…".to_owned(),
            Self::Connected | Self::Ready => "Connected".to_owned(),
            Self::Disconnected => "Offline".to_owned(),
        }
    }
}

/// What the voice control bar shows for the active voice connection.
#[derive(Clone, Debug)]
pub struct VoiceUiState {
    pub guild_id: Option<Snowflake>,
    pub channel_id: Snowflake,
    pub muted: bool,
    pub deafened: bool,
    pub camera: bool,
    pub streaming: bool,
}

/// Everything one account owns on the GTK side.
pub struct Session {
    pub account: Account,
    pub http: HttpClient,
    pub store: Store,
    pub user: Option<User>,
    pub guilds: Vec<Guild>,
    pub selected_guild: Option<Snowflake>,
    pub selected_channel: Option<Snowflake>,
    pub dm_mode: bool,
    pub last_typing: Option<Instant>,
    /// Channels whose history has already been requested this run.
    pub fetched: HashSet<Snowflake>,
    /// Guilds whose channel lists were requested through REST.
    pub loaded_guild_channels: HashSet<Snowflake>,
    pub loading_guild_channels: HashSet<Snowflake>,
    pub connection: ConnState,
    pub voice: Option<VoiceUiState>,
}

impl Session {
    pub fn new(account: Account) -> Self {
        Self {
            http: HttpClient::new(account.token.clone()),
            store: Store::new(),
            user: None,
            guilds: Vec::new(),
            selected_guild: None,
            selected_channel: None,
            dm_mode: false,
            last_typing: None,
            fetched: HashSet::new(),
            loading_guild_channels: HashSet::new(),
            loaded_guild_channels: HashSet::new(),
            connection: ConnState::Connecting,
            voice: None,
            account,
        }
    }
}

/// Why the UI asked for the camera list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraPurpose {
    Camera,
    Stream,
}

/// Voice work for one account, processed by its account loop.
pub enum VoiceCommand {
    Join {
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
    },
    Leave,
    SetMute(bool),
    SetDeafen(bool),
    SetCamera(bool),
    SetCameraDevice {
        access: CameraAccess,
        node_id: u32,
        settings: VideoSettings,
    },
    StartStream {
        parent: Option<Arc<ashpd::WindowIdentifier>>,
        settings: VideoSettings,
        delay_ms: u64,
    },
    StartStreamCamera {
        access: CameraAccess,
        node_id: u32,
        settings: VideoSettings,
        delay_ms: u64,
    },
    StopStream,
    Subscribe {
        guild_id: Snowflake,
        channel_id: Snowflake,
    },
}

/// Voice command senders for running accounts, filled by [`spawn_account`].
pub static VOICE_COMMANDS: LazyLock<
    Mutex<HashMap<AccountId, mpsc::UnboundedSender<VoiceCommand>>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// GTK-thread work queue.
pub enum Command {
    Login {
        token: Token,
        label: Option<String>,
    },
    StartAccount {
        account: AccountId,
        token: Token,
        store: Store,
    },
    FetchHistory {
        account: AccountId,
        http: HttpClient,
        store: Store,
        channel: Snowflake,
    },
    SendMessage {
        account: AccountId,
        http: HttpClient,
        channel: Snowflake,
        content: String,
        reply_to: Option<Snowflake>,
    },
    SendTyping {
        account: AccountId,
        http: HttpClient,
        channel: Snowflake,
    },
    Ack {
        http: HttpClient,
        channel: Snowflake,
        message_id: Snowflake,
    },
    RefreshGuilds {
        account: AccountId,
        http: HttpClient,
    },
    FetchGuildChannels {
        account: AccountId,
        http: HttpClient,
        store: Store,
        guild: Snowflake,
    },
    JoinVoice {
        account: AccountId,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
    },
    LeaveVoice {
        account: AccountId,
    },
    SetMute {
        account: AccountId,
        muted: bool,
    },
    SetDeafen {
        account: AccountId,
        deafened: bool,
    },
    SetCamera {
        account: AccountId,
        camera: bool,
    },
    SetCameraDevice {
        account: AccountId,
        access: CameraAccess,
        node_id: u32,
        settings: VideoSettings,
    },
    StartStream {
        account: AccountId,
        parent: Option<Arc<ashpd::WindowIdentifier>>,
        settings: VideoSettings,
        delay_ms: u64,
    },
    StartStreamCamera {
        account: AccountId,
        access: CameraAccess,
        node_id: u32,
        settings: VideoSettings,
        delay_ms: u64,
    },
    StopStream {
        account: AccountId,
    },
    SubscribeChannel {
        account: AccountId,
        guild_id: Snowflake,
        channel_id: Snowflake,
    },
}

/// Tokio-to-GTK notifications.
pub enum UiEvent {
    LoginSuccess {
        user: User,
        token: Token,
        label: Option<String>,
    },
    LoginFailed {
        message: String,
    },
    Connected {
        account: AccountId,
    },
    Ready {
        account: AccountId,
        user: Option<User>,
    },
    Resumed {
        account: AccountId,
    },
    Guilds {
        account: AccountId,
        guilds: Vec<Guild>,
    },
    GuildChannelsFailed {
        account: AccountId,
        guild: Snowflake,
        message: String,
    },
    StoreEvent {
        account: AccountId,
        event: StoreEvent,
    },
    GatewayClosed {
        account: AccountId,
        code: Option<u16>,
        reason: String,
    },
    VoiceJoined {
        account: AccountId,
        channel_id: Snowflake,
    },
    VoiceLeft {
        account: AccountId,
    },
    VoiceSpeaking {
        account: AccountId,
        user_id: Snowflake,
        speaking: bool,
    },
    VoiceError {
        account: AccountId,
        message: String,
    },
    StreamStarted {
        account: AccountId,
    },
    StreamStopped {
        account: AccountId,
    },
    StreamError {
        account: AccountId,
        message: String,
    },
    CameraChoices {
        account: AccountId,
        purpose: CameraPurpose,
        access: CameraAccess,
        cameras: Vec<kc_media::portal::CameraChoice>,
    },
    CameraUnavailable {
        account: AccountId,
        purpose: CameraPurpose,
        message: String,
    },
    CameraFailed {
        account: AccountId,
    },
    CameraStarted {
        account: AccountId,
    },
    CameraStopped {
        account: AccountId,
    },
    Error {
        account: Option<AccountId>,
        message: String,
    },
}

/// Spawns the command dispatcher on the app's runtime.
pub fn spawn_dispatcher(
    runtime: &tokio::runtime::Runtime,
    commands: async_channel::Receiver<Command>,
    ui: async_channel::Sender<UiEvent>,
) {
    runtime.spawn(async move {
        while let Ok(command) = commands.recv().await {
            tokio::spawn(run_command(command, ui.clone()));
        }
    });
}

async fn run_command(command: Command, ui: async_channel::Sender<UiEvent>) {
    match command {
        Command::Login { token, label } => {
            let http = HttpClient::new(token.clone());
            match http.current_user().await {
                Ok(user) => {
                    let _ = ui.send(UiEvent::LoginSuccess { user, token, label }).await;
                }
                Err(error) => {
                    let _ = ui
                        .send(UiEvent::LoginFailed {
                            message: describe_http_error(&error),
                        })
                        .await;
                }
            }
        }
        Command::StartAccount {
            account,
            token,
            store,
        } => spawn_account(account, token, store, ui),
        Command::FetchHistory {
            account,
            http,
            store,
            channel,
        } => match http.messages(channel, 50, None).await {
            Ok(messages) => store.load_history(channel, messages),
            Err(error) => send_http_error(&ui, Some(account), &error).await,
        },
        Command::SendMessage {
            account,
            http,
            channel,
            content,
            reply_to,
        } => {
            if let Err(error) = http.send_message(channel, &content, reply_to).await {
                send_http_error(&ui, Some(account), &error).await;
            }
        }
        Command::SendTyping {
            account,
            http,
            channel,
        } => {
            if let Err(error) = http.send_typing(channel).await {
                if error.is_unauthorized() {
                    send_http_error(&ui, Some(account), &error).await;
                } else {
                    debug!(%error, "typing indicator failed");
                }
            }
        }
        Command::Ack {
            http,
            channel,
            message_id,
        } => {
            if let Err(error) = http.ack_message(channel, message_id).await {
                debug!(%error, "ack failed");
            }
        }
        Command::RefreshGuilds { account, http } => match http.current_user_guilds().await {
            Ok(guilds) => {
                let _ = ui.send(UiEvent::Guilds { account, guilds }).await;
            }
            Err(error) => send_http_error(&ui, Some(account), &error).await,
        },
        Command::FetchGuildChannels {
            account,
            http,
            store,
            guild,
        } => match http.guild_channels(guild).await {
            Ok(channels) => store.load_guild_channels(guild, channels),
            Err(error) => {
                let _ = ui
                    .send(UiEvent::GuildChannelsFailed {
                        account,
                        guild,
                        message: describe_http_error(&error),
                    })
                    .await;
            }
        },
        Command::JoinVoice {
            account,
            guild_id,
            channel_id,
        } => {
            forward_voice(
                account,
                VoiceCommand::Join {
                    guild_id,
                    channel_id,
                },
                ui,
            )
            .await;
        }
        Command::LeaveVoice { account } => {
            forward_voice(account, VoiceCommand::Leave, ui).await;
        }
        Command::SetMute { account, muted } => {
            forward_voice(account, VoiceCommand::SetMute(muted), ui).await;
        }
        Command::SetDeafen { account, deafened } => {
            forward_voice(account, VoiceCommand::SetDeafen(deafened), ui).await;
        }
        Command::SetCamera { account, camera } => {
            forward_voice(account, VoiceCommand::SetCamera(camera), ui).await;
        }
        Command::StartStream {
            account,
            parent,
            settings,
            delay_ms,
        } => {
            forward_voice(
                account,
                VoiceCommand::StartStream {
                    parent,
                    settings,
                    delay_ms,
                },
                ui,
            )
            .await;
        }
        Command::SetCameraDevice {
            account,
            access,
            node_id,
            settings,
        } => {
            forward_voice(
                account,
                VoiceCommand::SetCameraDevice {
                    access,
                    node_id,
                    settings,
                },
                ui,
            )
            .await;
        }
        Command::StartStreamCamera {
            account,
            access,
            node_id,
            settings,
            delay_ms,
        } => {
            forward_voice(
                account,
                VoiceCommand::StartStreamCamera {
                    access,
                    node_id,
                    settings,
                    delay_ms,
                },
                ui,
            )
            .await;
        }
        Command::StopStream { account } => {
            forward_voice(account, VoiceCommand::StopStream, ui).await;
        }
        Command::SubscribeChannel {
            account,
            guild_id,
            channel_id,
        } => {
            forward_voice(
                account,
                VoiceCommand::Subscribe {
                    guild_id,
                    channel_id,
                },
                ui,
            )
            .await;
        }
    }
}

/// Hands a voice command to the account loop that owns the gateway.
async fn forward_voice(
    account: AccountId,
    command: VoiceCommand,
    ui: async_channel::Sender<UiEvent>,
) {
    let sender = VOICE_COMMANDS
        .lock()
        .ok()
        .and_then(|registry| registry.get(&account).cloned());
    let Some(sender) = sender else {
        let _ = ui
            .send(UiEvent::Error {
                account: Some(account),
                message: "account is not running".to_owned(),
            })
            .await;
        return;
    };
    if sender.send(command).is_err() {
        let _ = ui
            .send(UiEvent::Error {
                account: Some(account),
                message: "account is not running".to_owned(),
            })
            .await;
    }
}

fn spawn_account(
    account: AccountId,
    token: Token,
    store: Store,
    ui: async_channel::Sender<UiEvent>,
) {
    // Forward every store change to the GTK thread. Lagged messages are fine:
    // the UI re-reads the store for each event it handles.
    let store_ui = ui.clone();
    let mut events = store.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if store_ui
                        .send(UiEvent::StoreEvent { account, event })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    debug!(skipped, "store events lagged");
                }
                Err(RecvError::Closed) => break,
            }
        }
    });

    let (voice_tx, mut voice_rx) = mpsc::unbounded_channel::<VoiceCommand>();
    if let Ok(mut registry) = VOICE_COMMANDS.lock() {
        registry.insert(account, voice_tx);
    }

    tokio::spawn(async move {
        let (handle, mut gateway_rx, _gateway_task) = Gateway::new(token).start();
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<VoiceControl>();
        let mut voice = VoiceRuntime {
            handle,
            self_user_id: None,
            pending_join: None,
            session_id: None,
            server_token: None,
            endpoint: None,
            connection: None,
            muted: false,
            deafened: false,
            camera: false,
            generation: 0,
            stream_key: None,
            stream_rtc_id: None,
            stream_token: None,
            stream_endpoint: None,
            stream_connection: None,
            stream_camera: None,
            stream_parent: None,
            stream_settings: VideoSettings::default(),
            stream_delay_ms: 0,
            stream_pending: false,
            streaming: false,
            stream_generation: 0,
        };

        loop {
            tokio::select! {
                event = gateway_rx.recv() => {
                    let Some(event) = event else { break };
                    match event {
                        GatewayEvent::Hello { .. } => {
                            let _ = ui.send(UiEvent::Connected { account }).await;
                        }
                        GatewayEvent::Dispatch { name, data } => {
                            store.apply(&name, &data);
                            match name.as_str() {
                                "READY" => {
                                    voice.self_user_id =
                                        serde_json::from_value(data["user"]["id"].clone()).ok();
                                }
                                "VOICE_STATE_UPDATE" => {
                                    handle_voice_state(
                                        &mut voice,
                                        &data,
                                        account,
                                        &ui,
                                        &control_tx,
                                    )
                                    .await;
                                }
                                "VOICE_SERVER_UPDATE" => {
                                    handle_server_update(
                                        &mut voice,
                                        &data,
                                        account,
                                        &ui,
                                        &control_tx,
                                    )
                                    .await;
                                }
                                "STREAM_CREATE" => {
                                    handle_stream_create(
                                        &mut voice,
                                        &data,
                                        account,
                                        &ui,
                                        &control_tx,
                                    )
                                    .await;
                                }
                                "STREAM_SERVER_UPDATE" => {
                                    handle_stream_server_update(
                                        &mut voice,
                                        &data,
                                        account,
                                        &ui,
                                        &control_tx,
                                    )
                                    .await;
                                }
                                "STREAM_DELETE" => {
                                    handle_stream_delete(
                                        &mut voice,
                                        &data,
                                        account,
                                        &ui,
                                    )
                                    .await;
                                }
                                _ => {}
                            }
                            if name == "READY" {
                                let user =
                                    serde_json::from_value::<User>(data["user"].clone()).ok();
                                let _ = ui.send(UiEvent::Ready { account, user }).await;
                            }
                        }
                        GatewayEvent::Resumed => {
                            let _ = ui.send(UiEvent::Resumed { account }).await;
                        }
                        GatewayEvent::Reconnect => {}
                        GatewayEvent::InvalidSession { resumable } => {
                            warn!(account, resumable, "gateway invalidated the session");
                            let _ = ui
                                .send(UiEvent::Error {
                                    account: Some(account),
                                    message: "gateway session invalidated".to_owned(),
                                })
                                .await;
                        }
                        GatewayEvent::Closed { code, reason } => {
                            let _ = ui
                                .send(UiEvent::GatewayClosed {
                                    account,
                                    code,
                                    reason,
                                })
                                .await;
                            break;
                        }
                    }
                }
                command = voice_rx.recv() => {
                    let Some(command) = command else { break };
                    apply_voice_command(&mut voice, command, account, &ui, &control_tx).await;
                }
                control = control_rx.recv() => {
                    let Some(control) = control else { break };
                    match control {
                        VoiceControl::Closed { generation } => {
                            if generation == voice.generation && voice.connection.is_some() {
                                teardown_voice(&mut voice, account, &ui, true).await;
                            }
                        }
                        VoiceControl::ForceCameraKeyframe { generation } => {
                            if generation == voice.generation
                                && let Some(connection) = &voice.connection
                                && let Some(source) = &connection.camera
                            {
                                source.force_keyframe();
                            }
                        }
                        VoiceControl::StreamReady { generation } => {
                            if generation == voice.stream_generation
                                && voice.stream_connection.is_some()
                            {
                                voice.streaming = true;
                                let _ = ui.send(UiEvent::StreamStarted { account }).await;
                            }
                        }
                        VoiceControl::StreamClosed { generation } => {
                            if generation == voice.stream_generation
                                && voice.stream_connection.is_some()
                            {
                                teardown_stream(&mut voice, account, &ui, true).await;
                            }
                        }
                        VoiceControl::StreamSource { generation, result } => {
                            if generation != voice.stream_generation {
                                continue;
                            }
                            match result {
                                Ok(source) => {
                                    let delay_ms = voice.stream_delay_ms;
                                    if let Some(connection) = &mut voice.stream_connection {
                                        tracing::info!("stream source ready");
                                        connection.source = Some(source);
                                        if connection.audio.is_none() {
                                            match ScreenAudio::start(
                                                connection.handle.clone(),
                                                128_000,
                                                delay_ms,
                                            ) {
                                                Ok(audio) => connection.audio = Some(audio),
                                                Err(error) => {
                                                    let _ = ui
                                                        .send(UiEvent::Error {
                                                            account: Some(account),
                                                            message: format!(
                                                                "screen audio unavailable: {error}"
                                                            ),
                                                        })
                                                        .await;
                                                }
                                            }
                                        }
                                    } else {
                                        tracing::warn!("stream source ready but no stream connection");
                                    }
                                }
                                Err(message) => {
                                    let _ =
                                        ui.send(UiEvent::StreamError { account, message }).await;
                                    teardown_stream(&mut voice, account, &ui, true).await;
                                }
                            }
                        }
                        VoiceControl::ForceKeyframe { generation } => {
                            if generation == voice.stream_generation
                                && let Some(connection) = &voice.stream_connection
                                && let Some(source) = &connection.source
                            {
                                source.force_keyframe();
                            }
                        }
                    }
                }
            }
        }

        teardown_voice(&mut voice, account, &ui, true).await;
        if let Ok(mut registry) = VOICE_COMMANDS.lock() {
            registry.remove(&account);
        }
    });
}

/// Live voice state for one account, owned by its account loop.
struct VoiceRuntime {
    handle: GatewayHandle,
    self_user_id: Option<Snowflake>,
    pending_join: Option<(Option<Snowflake>, Snowflake)>,
    session_id: Option<String>,
    server_token: Option<String>,
    endpoint: Option<String>,
    connection: Option<VoiceConnection>,
    muted: bool,
    deafened: bool,
    camera: bool,
    generation: u64,
    stream_key: Option<String>,
    stream_rtc_id: Option<Snowflake>,
    stream_token: Option<String>,
    stream_endpoint: Option<String>,
    stream_connection: Option<StreamConnection>,
    stream_camera: Option<(CameraAccess, u32)>,
    stream_parent: Option<Arc<ashpd::WindowIdentifier>>,
    stream_settings: VideoSettings,
    stream_delay_ms: u64,
    stream_pending: bool,
    streaming: bool,
    stream_generation: u64,
}

struct VoiceConnection {
    handle: VoiceHandle,
    audio: Option<Arc<VoiceAudio>>,
    camera: Option<VideoSource>,
    guild_id: Option<Snowflake>,
    channel_id: Snowflake,
}

struct StreamConnection {
    handle: VoiceHandle,
    source: Option<VideoSource>,
    audio: Option<ScreenAudio>,
    key: String,
}

/// Signals from a voice connection's consumer task back to the account loop.
enum VoiceControl {
    Closed {
        generation: u64,
    },
    StreamReady {
        generation: u64,
    },
    StreamClosed {
        generation: u64,
    },
    StreamSource {
        generation: u64,
        result: Result<VideoSource, String>,
    },
    ForceKeyframe {
        generation: u64,
    },
    ForceCameraKeyframe {
        generation: u64,
    },
}

async fn handle_voice_state(
    voice: &mut VoiceRuntime,
    data: &serde_json::Value,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) {
    let user_id: Option<Snowflake> = serde_json::from_value(data["user_id"].clone()).ok();
    if user_id.is_none() || user_id != voice.self_user_id {
        return;
    }
    let channel_id: Option<Snowflake> = serde_json::from_value(data["channel_id"].clone()).ok();
    match channel_id {
        None => {
            // Discord reports the old voice state as `channel_id: null` while
            // a join to a different channel is still pending. Tearing down on
            // that stale event clears pending_join, so the later self state
            // with the new channel never connects and the UI stays on "Join
            // Voice". Only an established connection means a real leave.
            if voice.connection.is_some() {
                teardown_voice(voice, account, ui, true).await;
            } else if voice.pending_join.is_some() {
                tracing::debug!("ignoring stale self voice state with no channel during join");
            }
        }
        Some(channel_id) => {
            if let Some((_, expected)) = voice.pending_join
                && channel_id != expected
            {
                return;
            }
            voice.session_id = data["session_id"].as_str().map(str::to_owned);
            maybe_connect(voice, account, ui, control_tx).await;
        }
    }
}

async fn handle_server_update(
    voice: &mut VoiceRuntime,
    data: &serde_json::Value,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) {
    let guild_id: Option<Snowflake> = serde_json::from_value(data["guild_id"].clone()).ok();
    let target = voice
        .pending_join
        .map(|(guild_id, _)| guild_id)
        .or_else(|| {
            voice
                .connection
                .as_ref()
                .map(|connection| connection.guild_id)
        });
    let Some(target) = target else { return };
    if guild_id != target {
        return;
    }
    if let Some(token) = data["token"].as_str() {
        voice.server_token = Some(token.to_owned());
    }
    if let Some(endpoint) = data["endpoint"].as_str() {
        voice.endpoint = Some(endpoint.to_owned());
    }
    maybe_connect(voice, account, ui, control_tx).await;
}

/// Returns the user id that owns a stream key such as
/// `guild:<guild>:<channel>:<user>` or `call:<channel>:<user>`.
fn stream_key_owner(key: &str) -> Option<Snowflake> {
    key.rsplit(':').next().and_then(|id| id.parse().ok())
}

/// Splits a stream key into `(guild, channel, owner)`. The guild is absent for
/// DM calls.
fn stream_key_target(key: &str) -> Option<(Option<Snowflake>, Snowflake, Snowflake)> {
    let mut parts = key.split(':');
    match parts.next()? {
        "guild" => {
            let guild = parts.next()?.parse().ok()?;
            let channel = parts.next()?.parse().ok()?;
            let user = parts.next()?.parse().ok()?;
            Some((Some(guild), channel, user))
        }
        "call" => {
            let channel = parts.next()?.parse().ok()?;
            let user = parts.next()?.parse().ok()?;
            Some((None, channel, user))
        }
        _ => None,
    }
}

async fn handle_stream_create(
    voice: &mut VoiceRuntime,
    data: &serde_json::Value,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) {
    if !voice.stream_pending {
        return;
    }
    let Some(self_user_id) = voice.self_user_id else {
        return;
    };
    let Some(key) = data["stream_key"].as_str() else {
        return;
    };
    let Some((guild_id, channel_id, owner)) = stream_key_target(key) else {
        return;
    };
    if owner != self_user_id {
        return;
    }
    let Some(connection) = &voice.connection else {
        return;
    };
    if channel_id != connection.channel_id || guild_id != connection.guild_id {
        return;
    }
    voice.stream_key = Some(key.to_owned());
    voice.stream_rtc_id = data["rtc_server_id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .or_else(|| data["rtc_server_id"].as_u64().map(Snowflake::new));
    tracing::info!(key, rtc = ?voice.stream_rtc_id, "STREAM_CREATE");
    maybe_start_stream(voice, account, ui, control_tx).await;
}

async fn handle_stream_server_update(
    voice: &mut VoiceRuntime,
    data: &serde_json::Value,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) {
    if let Some(key) = data["stream_key"].as_str() {
        match voice.stream_key.as_deref() {
            Some(ours) if ours != key => return,
            Some(_) => {}
            None => {
                // Without a key from STREAM_CREATE the owner is all we can
                // check before accepting the token.
                if stream_key_owner(key) != voice.self_user_id {
                    return;
                }
            }
        }
    }
    if voice.stream_key.is_none() && !voice.stream_pending && voice.stream_connection.is_none() {
        return;
    }
    if let Some(token) = data["token"].as_str() {
        voice.stream_token = Some(token.to_owned());
    }
    if let Some(endpoint) = data["endpoint"].as_str() {
        voice.stream_endpoint = Some(endpoint.to_owned());
    }
    maybe_start_stream(voice, account, ui, control_tx).await;
}

async fn handle_stream_delete(
    voice: &mut VoiceRuntime,
    data: &serde_json::Value,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
) {
    let Some(key) = data["stream_key"].as_str() else {
        return;
    };
    let ours = voice.stream_key.as_deref() == Some(key)
        || voice
            .stream_connection
            .as_ref()
            .is_some_and(|connection| connection.key == key);
    if ours {
        teardown_stream(voice, account, ui, true).await;
    }
}

async fn maybe_start_stream(
    voice: &mut VoiceRuntime,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) {
    if !voice.stream_pending || voice.stream_connection.is_some() {
        return;
    }
    let (Some(key), Some(rtc_id), Some(token), Some(endpoint), Some(session_id), Some(user_id)) = (
        voice.stream_key.clone(),
        voice.stream_rtc_id,
        voice.stream_token.clone(),
        voice.stream_endpoint.clone(),
        voice.session_id.clone(),
        voice.self_user_id,
    ) else {
        return;
    };
    let Some(connection) = &voice.connection else {
        return;
    };
    let channel_id = connection.channel_id;
    let dave_channel_id = Snowflake::new(rtc_id.get().saturating_sub(1));
    let params = VoiceParams {
        server_id: rtc_id,
        channel_id,
        user_id,
        session_id,
        token,
        endpoint,
        video: true,
        screen: true,
        dave_channel_id,
    };
    tracing::info!(%rtc_id, %key, endpoint = %params.endpoint, "stream connecting");
    voice.stream_pending = false;
    voice.stream_generation = voice.stream_generation.wrapping_add(1);
    let generation = voice.stream_generation;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<VoiceEvent>();
    match kc_voice::connect(params, event_tx).await {
        Ok(handle) => {
            debug!(%key, %rtc_id, "go live stream connecting");
            voice.stream_connection = Some(StreamConnection {
                handle: handle.clone(),
                source: None,
                audio: None,
                key,
            });

            // Readiness enables the video track and tells the UI.
            let ui_events = ui.clone();
            let control = control_tx.clone();
            let ready_handle = handle.clone();
            tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    match event {
                        VoiceEvent::Ready {
                            ssrc,
                            video_ssrc,
                            rtx_ssrc,
                            mode,
                            dave_version,
                        } => {
                            tracing::info!(
                                ssrc,
                                ?video_ssrc,
                                ?rtx_ssrc,
                                mode,
                                dave_version,
                                "stream voice ready"
                            );
                            ready_handle.set_video(true);
                            let _ = control.send(VoiceControl::StreamReady { generation });
                        }
                        VoiceEvent::DaveReady => {
                            tracing::info!("stream DAVE ready");
                        }
                        VoiceEvent::ForceKeyframe => {
                            let _ = control.send(VoiceControl::ForceKeyframe { generation });
                        }
                        VoiceEvent::Error(message) => {
                            tracing::warn!(%message, "stream voice error");
                            let _ = ui_events
                                .send(UiEvent::StreamError { account, message })
                                .await;
                            let _ = control.send(VoiceControl::StreamClosed { generation });
                            break;
                        }
                        VoiceEvent::Closed { .. } => {
                            let _ = control.send(VoiceControl::StreamClosed { generation });
                            break;
                        }
                        _ => {}
                    }
                }
            });

            // The portal picker may block on user input; keep the account loop
            // responsive while it runs. A Go Live can use the selected camera
            // instead of the screen-cast portal.
            let source_control = control_tx.clone();
            let source_handle = handle.clone();
            let source_ui = ui.clone();
            let camera_source = voice.stream_camera.clone();
            let screen_parent = voice.stream_parent.clone();
            let video_settings = voice.stream_settings.clone();
            let stream_delay_ms = voice.stream_delay_ms;
            let video_send = source_handle.clone();
            let video_delay = DelayedSender::start(
                Duration::from_millis(stream_delay_ms),
                move |data: Vec<u8>, frame_ms| video_send.send_video(data, frame_ms),
            );
            tokio::spawn(async move {
                let result = if let Some((access, node_id)) = camera_source {
                    access
                        .select(node_id)
                        .ok_or_else(|| "selected camera is no longer available".to_owned())
                        .and_then(|camera| {
                            let video_delay = video_delay.clone();
                            VideoSource::camera_portal(
                                camera,
                                video_settings.clone(),
                                move |frame| {
                                    video_delay.send(frame.data, frame.duration_ms);
                                },
                            )
                            .map_err(|error| error.to_string())
                        })
                } else {
                    let request = tokio::time::timeout(
                        std::time::Duration::from_secs(120),
                        kc_media::portal::request(screen_parent.as_deref()),
                    )
                    .await;
                    match request {
                        Ok(Ok(portal)) => {
                            let first_frame = Arc::new(AtomicBool::new(false));
                            let frame_count = Arc::new(AtomicUsize::new(0));
                            let watchdog_frames = frame_count.clone();
                            let video_delay = video_delay.clone();
                            let result = VideoSource::screen(
                                portal,
                                video_settings.clone(),
                                move |frame| {
                                    frame_count.fetch_add(1, Ordering::Relaxed);
                                    if !first_frame.swap(true, Ordering::Relaxed) {
                                        tracing::info!(
                                            bytes = frame.data.len(),
                                            "stream first encoded video frame"
                                        );
                                    }
                                    video_delay.send(frame.data, frame.duration_ms);
                                },
                            )
                            .map_err(|error| error.to_string());
                            if result.is_ok() {
                                let ui_events = source_ui.clone();
                                let control = source_control.clone();
                                tokio::spawn(async move {
                                    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                                    if watchdog_frames.load(Ordering::Relaxed) == 0 {
                                        let _ = ui_events
                                            .send(UiEvent::StreamError {
                                                account,
                                                message: "screen capture produced no frames"
                                                    .to_owned(),
                                            })
                                            .await;
                                        let _ = control
                                            .send(VoiceControl::StreamClosed { generation });
                                    }
                                });
                            }
                            result
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(%error, "screen capture source failed");
                            Err(error.to_string())
                        }
                        Err(_) => Err("screen picker timed out".to_owned()),
                    }
                };
                let _ = source_control.send(VoiceControl::StreamSource { generation, result });
            });
        }
        Err(error) => {
            let _ = ui
                .send(UiEvent::StreamError {
                    account,
                    message: format!("screen share connect failed: {error}"),
                })
                .await;
            stop_stream(voice, account, ui, false).await;
        }
    }
}

async fn maybe_connect(
    voice: &mut VoiceRuntime,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) {
    let Some((guild_id, channel_id)) = voice.pending_join else {
        return;
    };
    if voice.connection.is_some() {
        return;
    }
    let Some(user_id) = voice.self_user_id else {
        return;
    };
    let (Some(session_id), Some(token), Some(endpoint)) = (
        voice.session_id.clone(),
        voice.server_token.clone(),
        voice.endpoint.clone(),
    ) else {
        return;
    };

    tracing::info!(%channel_id, ?guild_id, "voice connecting from gateway state");
    let params = VoiceParams {
        server_id: guild_id.unwrap_or(channel_id),
        channel_id,
        user_id,
        session_id,
        token,
        endpoint,
        video: true,
        screen: false,
        dave_channel_id: channel_id,
    };
    voice.pending_join = None;
    voice.generation = voice.generation.wrapping_add(1);
    let generation = voice.generation;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<VoiceEvent>();
    match kc_voice::connect(params, event_tx).await {
        Ok(handle) => {
            tracing::info!(%channel_id, "voice connection started");
            let audio = match VoiceAudio::start(handle.clone()) {
                Ok(audio) => Some(Arc::new(audio)),
                Err(error) => {
                    let _ = ui
                        .send(UiEvent::VoiceError {
                            account,
                            message: format!("voice audio unavailable: {error}"),
                        })
                        .await;
                    None
                }
            };
            if let Some(audio) = &audio {
                audio.set_deafened(voice.deafened);
                audio.set_mic_enabled(!voice.muted && !voice.deafened);
            }
            if voice.muted {
                handle.set_speaking(0);
            }

            let connection = VoiceConnection {
                handle: handle.clone(),
                audio: audio.clone(),
                camera: None,
                guild_id,
                channel_id,
            };
            voice.connection = Some(connection);
            let _ = ui
                .send(UiEvent::VoiceJoined {
                    account,
                    channel_id,
                })
                .await;

            let ui = ui.clone();
            let control = control_tx.clone();
            tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    match event {
                        VoiceEvent::Audio {
                            user_id,
                            opus,
                            timestamp,
                            ..
                        } => {
                            if let Some(audio) = &audio {
                                audio.push_remote(user_id.get(), &opus, timestamp);
                            }
                        }
                        VoiceEvent::Speaking {
                            user_id, speaking, ..
                        } => {
                            let _ = ui
                                .send(UiEvent::VoiceSpeaking {
                                    account,
                                    user_id,
                                    speaking,
                                })
                                .await;
                        }
                        VoiceEvent::Ready { .. } => {
                            let _ = ui
                                .send(UiEvent::VoiceJoined {
                                    account,
                                    channel_id,
                                })
                                .await;
                        }
                        VoiceEvent::ForceKeyframe => {
                            let _ = control.send(VoiceControl::ForceCameraKeyframe { generation });
                        }
                        VoiceEvent::Error(message) => {
                            let _ = ui.send(UiEvent::VoiceError { account, message }).await;
                            let _ = control.send(VoiceControl::Closed { generation });
                        }
                        VoiceEvent::Closed { .. } => {
                            let _ = control.send(VoiceControl::Closed { generation });
                        }
                        _ => {}
                    }
                }
            });
        }
        Err(error) => {
            tracing::warn!(%error, "voice connect failed");
            let _ = ui
                .send(UiEvent::VoiceError {
                    account,
                    message: format!("voice connect failed: {error}"),
                })
                .await;
        }
    }
}

async fn start_camera(
    connection: &mut VoiceConnection,
    access: &CameraAccess,
    node_id: u32,
    settings: VideoSettings,
) -> Result<(), String> {
    let camera = access
        .select(node_id)
        .ok_or_else(|| "selected camera is no longer available".to_owned())?;
    debug!(camera = %camera.name, "starting camera");
    let handle = connection.handle.clone();
    let source = VideoSource::camera_portal(camera, settings, move |frame| {
        handle.send_video(frame.data, frame.duration_ms);
    })
    .map_err(|error| error.to_string())?;
    connection.camera = Some(source);
    connection.handle.set_video(true);
    Ok(())
}

async fn apply_voice_command(
    voice: &mut VoiceRuntime,
    command: VoiceCommand,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) {
    match command {
        VoiceCommand::Join {
            guild_id,
            channel_id,
        } => {
            if voice.connection.is_some() || voice.pending_join.is_some() {
                teardown_voice(voice, account, ui, true).await;
            }
            voice.pending_join = Some((guild_id, channel_id));
            voice.session_id = None;
            voice.server_token = None;
            voice.endpoint = None;
            // Do not announce a camera on join; Discord shows a camera tile
            // for `self_video: true` even before any frames exist, and it
            // fails with 2015 if no track follows.
            let self_video = voice.camera;
            voice.handle.update_voice_state(
                guild_id,
                Some(channel_id),
                voice.muted,
                voice.deafened,
                self_video,
            );
            maybe_connect(voice, account, ui, control_tx).await;
        }
        VoiceCommand::Leave => {
            let guild_id = voice
                .connection
                .as_ref()
                .and_then(|connection| connection.guild_id)
                .or_else(|| voice.pending_join.and_then(|(guild_id, _)| guild_id));
            if voice.connection.is_some() || voice.pending_join.is_some() {
                teardown_voice(voice, account, ui, true).await;
            }
            voice
                .handle
                .update_voice_state(guild_id, None, voice.muted, voice.deafened, false);
        }
        VoiceCommand::SetMute(muted) => {
            voice.muted = muted;
            if let Some(connection) = &voice.connection {
                if muted {
                    connection.handle.set_speaking(0);
                }
                if let Some(audio) = &connection.audio {
                    audio.set_mic_enabled(!muted && !voice.deafened);
                }
            }
        }
        VoiceCommand::SetDeafen(deafened) => {
            voice.deafened = deafened;
            if let Some(connection) = &voice.connection {
                if let Some(audio) = &connection.audio {
                    audio.set_deafened(deafened);
                    audio.set_mic_enabled(!voice.muted && !deafened);
                }
                if deafened {
                    connection.handle.set_speaking(0);
                }
            }
        }
        VoiceCommand::SetCameraDevice {
            access,
            node_id,
            settings,
        } => {
            voice.camera = true;
            let gateway = voice.handle.clone();
            let (guild_id, channel_id) = match &voice.connection {
                Some(connection) => (connection.guild_id, connection.channel_id),
                None => (None, Snowflake::default()),
            };
            match &mut voice.connection {
                Some(connection) => match start_camera(connection, &access, node_id, settings).await {
                    Ok(()) => {
                        gateway.update_voice_state(
                            guild_id,
                            Some(channel_id),
                            voice.muted,
                            voice.deafened,
                            true,
                        );
                        let _ = ui.send(UiEvent::CameraStarted { account }).await;
                    }
                    Err(error) => {
                        voice.camera = false;
                        let _ = ui
                            .send(UiEvent::VoiceError {
                                account,
                                message: format!("camera: {error}"),
                            })
                            .await;
                        let _ = ui.send(UiEvent::CameraFailed { account }).await;
                        gateway.update_voice_state(
                            guild_id,
                            Some(channel_id),
                            voice.muted,
                            voice.deafened,
                            false,
                        );
                    }
                },
                None => {
                    voice.camera = false;
                    let _ = ui
                        .send(UiEvent::VoiceError {
                            account,
                            message: "join a voice channel before enabling the camera"
                                .to_owned(),
                        })
                        .await;
                    let _ = ui.send(UiEvent::CameraFailed { account }).await;
                }
            }
        }
        VoiceCommand::SetCamera(enabled) => {
            voice.camera = enabled;
            let gateway = voice.handle.clone();
            let (guild_id, channel_id) = match &voice.connection {
                Some(connection) => (connection.guild_id, connection.channel_id),
                None => (None, Snowflake::default()),
            };
            match &mut voice.connection {
                Some(connection) => {
                    let started = if enabled {
                        if connection.camera.is_none() {
                            voice.camera = false;
                            let _ = ui
                                .send(UiEvent::VoiceError {
                                    account,
                                    message: "choose a camera first".to_owned(),
                                })
                                .await;
                            false
                        } else {
                            connection.handle.set_video(true);
                            true
                        }
                    } else {
                        connection.camera = None;
                        connection.handle.set_video(false);
                        false
                    };
                    // Camera is a main-gateway voice state, not only the
                    // voice-server opcode 12. Without this the other side
                    // shows a permanently loading camera tile.
                    gateway.update_voice_state(
                        guild_id,
                        Some(channel_id),
                        voice.muted,
                        voice.deafened,
                        started,
                    );
                    let event = if started {
                        UiEvent::CameraStarted { account }
                    } else {
                        UiEvent::CameraStopped { account }
                    };
                    let _ = ui.send(event).await;
                }
                None => {
                    if enabled {
                        voice.camera = false;
                        let _ = ui
                            .send(UiEvent::VoiceError {
                                account,
                                message: "join a voice channel before enabling the camera"
                                    .to_owned(),
                            })
                            .await;
                    }
                }
            }
        }
        VoiceCommand::StartStream {
            parent,
            settings,
            delay_ms,
        } => {
            let target = voice
                .connection
                .as_ref()
                .map(|connection| (connection.guild_id, connection.channel_id));
            let Some((guild_id, channel_id)) = target else {
                let _ = ui
                    .send(UiEvent::VoiceError {
                        account,
                        message: "join a voice channel first".to_owned(),
                    })
                    .await;
                return;
            };
            if voice.streaming || voice.stream_pending || voice.stream_connection.is_some() {
                return;
            }
            let Some(guild_id) = guild_id else {
                let _ = ui
                    .send(UiEvent::VoiceError {
                        account,
                        message: "screen sharing needs a guild voice channel".to_owned(),
                    })
                    .await;
                return;
            };
            tracing::info!(%guild_id, %channel_id, "creating go live stream");
            voice.stream_camera = None;
            voice.stream_parent = parent;
            voice.stream_settings = settings;
            voice.stream_delay_ms = delay_ms;
            voice.stream_pending = true;
            voice.stream_key = None;
            voice.stream_rtc_id = None;
            voice.stream_token = None;
            voice.stream_endpoint = None;
            voice.handle.stream_create(guild_id, channel_id);
            // DVS sends STREAM_SET_PAUSED(false) immediately after create;
            // without it the stream is announced but never actually starts.
            if let Some(user_id) = voice.self_user_id {
                let key = format!("guild:{guild_id}:{channel_id}:{user_id}");
                voice.handle.stream_pause(&key, false);
            }
        }
        VoiceCommand::StartStreamCamera {
            access,
            node_id,
            settings,
            delay_ms,
        } => {
            let target = voice
                .connection
                .as_ref()
                .map(|connection| (connection.guild_id, connection.channel_id));
            let Some((Some(guild_id), channel_id)) = target else {
                let _ = ui
                    .send(UiEvent::VoiceError {
                        account,
                        message: "screen sharing needs a guild voice channel".to_owned(),
                    })
                    .await;
                return;
            };
            if voice.streaming || voice.stream_pending || voice.stream_connection.is_some() {
                return;
            }
            tracing::info!(%guild_id, %channel_id, node_id, "creating camera go live stream");
            voice.stream_camera = Some((access, node_id));
            voice.stream_parent = None;
            voice.stream_settings = settings;
            voice.stream_delay_ms = delay_ms;
            voice.stream_pending = true;
            voice.stream_key = None;
            voice.stream_rtc_id = None;
            voice.stream_token = None;
            voice.stream_endpoint = None;
            voice.handle.stream_create(guild_id, channel_id);
            if let Some(user_id) = voice.self_user_id {
                let key = format!("guild:{guild_id}:{channel_id}:{user_id}");
                voice.handle.stream_pause(&key, false);
            }
        }
        VoiceCommand::StopStream => {
            stop_stream(voice, account, ui, true).await;
        }
        VoiceCommand::Subscribe {
            guild_id,
            channel_id,
        } => {
            voice.handle.subscribe_channel(guild_id, channel_id);
        }
    }
}

/// Ends a Go Live stream, deleting it on Discord when a key is known.
async fn stop_stream(
    voice: &mut VoiceRuntime,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    notify: bool,
) {
    let had_stream = clear_stream(voice).await;
    if let Some(key) = had_stream.key {
        voice.handle.stream_delete(&key);
    }
    if notify && had_stream.had_any {
        let _ = ui.send(UiEvent::StreamStopped { account }).await;
    }
}

/// Tears a stream down without asking Discord to delete it, for remote closes.
async fn teardown_stream(
    voice: &mut VoiceRuntime,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    notify: bool,
) {
    let had_stream = clear_stream(voice).await;
    if notify && had_stream.had_any {
        let _ = ui.send(UiEvent::StreamStopped { account }).await;
    }
}

struct ClearedStream {
    key: Option<String>,
    had_any: bool,
}

/// Drops the source, disconnects the stream connection and resets all stream
/// state. Returns the last known stream key so callers can delete it.
async fn clear_stream(voice: &mut VoiceRuntime) -> ClearedStream {
    let key = voice
        .stream_key
        .clone()
        .or_else(|| voice.stream_connection.as_ref().map(|c| c.key.clone()));
    let had_any = voice.streaming
        || voice.stream_pending
        || voice.stream_connection.is_some()
        || key.is_some();
    voice.stream_pending = false;
    voice.streaming = false;
    voice.stream_camera = None;
    voice.stream_parent = None;
    if let Some(connection) = voice.stream_connection.take() {
        drop(connection.source);
        drop(connection.audio);
        connection.handle.disconnect().await;
    }
    voice.stream_key = None;
    voice.stream_rtc_id = None;
    voice.stream_token = None;
    voice.stream_endpoint = None;
    voice.stream_generation = voice.stream_generation.wrapping_add(1);
    ClearedStream { key, had_any }
}

async fn teardown_voice(
    voice: &mut VoiceRuntime,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    notify: bool,
) {
    stop_stream(voice, account, ui, notify).await;
    let had_connection = voice.connection.is_some() || voice.pending_join.is_some();
    if let Some(connection) = voice.connection.take() {
        connection.handle.disconnect().await;
        drop(connection.camera);
        drop(connection.audio);
    }
    voice.pending_join = None;
    voice.session_id = None;
    voice.server_token = None;
    voice.endpoint = None;
    voice.muted = false;
    voice.deafened = false;
    voice.camera = false;
    voice.generation = voice.generation.wrapping_add(1);
    if notify && had_connection {
        let _ = ui.send(UiEvent::VoiceLeft { account }).await;
    }
}

fn describe_http_error(error: &kc_http::Error) -> String {
    if error.is_unauthorized() {
        "token rejected".to_owned()
    } else {
        format!("{error}")
    }
}

async fn send_http_error(
    ui: &async_channel::Sender<UiEvent>,
    account: Option<AccountId>,
    error: &kc_http::Error,
) {
    let _ = ui
        .send(UiEvent::Error {
            account,
            message: describe_http_error(error),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn runtime(handle: GatewayHandle) -> VoiceRuntime {
        VoiceRuntime {
            handle,
            self_user_id: Some(Snowflake(1)),
            pending_join: Some((Some(Snowflake(5)), Snowflake(12))),
            session_id: None,
            server_token: None,
            endpoint: None,
            connection: None,
            muted: false,
            deafened: false,
            camera: false,
            generation: 0,
            stream_key: None,
            stream_rtc_id: None,
            stream_token: None,
            stream_endpoint: None,
            stream_connection: None,
            stream_camera: None,
            stream_parent: None,
            stream_settings: VideoSettings::default(),
            stream_delay_ms: 0,
            stream_pending: false,
            streaming: false,
            stream_generation: 0,
        }
    }

    #[tokio::test]
    async fn stale_null_voice_state_does_not_cancel_a_pending_join() {
        let (handle, _events, task) = Gateway::new(Token::new("test")).start();
        task.abort();
        let (ui_tx, _ui_rx) = async_channel::unbounded();
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        let mut voice = runtime(handle);

        // Discord sends the old state as channel_id: null before the target
        // state. This must not clear pending_join.
        handle_voice_state(
            &mut voice,
            &json!({"user_id": "1", "channel_id": null}),
            0,
            &ui_tx,
            &control_tx,
        )
        .await;

        assert_eq!(
            voice.pending_join,
            Some((Some(Snowflake(5)), Snowflake(12)))
        );

        // The target state records the session id.
        handle_voice_state(
            &mut voice,
            &json!({"user_id": "1", "channel_id": "12", "session_id": "abc"}),
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert_eq!(voice.session_id.as_deref(), Some("abc"));
    }
}
