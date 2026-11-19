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
use kc_media::decode::VideoFeed;
use kc_media::delay::DelayedSender;
use kc_media::portal::{CameraAccess, ShareSources};
use kc_media::share_audio::AudioShare;
use kc_media::video::{EncodedFrame, PreparedCapture, VideoSettings, VideoSource, is_keyframe};
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
    /// The connection dropped; the gateway is reconnecting by itself.
    Disconnected,
    /// Discord no longer accepts the account's token.
    SignedOut,
    /// Discord closed the connection for good, for another reason.
    Stopped,
}

impl ConnState {
    pub fn label(&self) -> String {
        match self {
            Self::Connecting => "Connecting…".to_owned(),
            Self::Connected | Self::Ready => "Connected".to_owned(),
            Self::Disconnected | Self::Stopped => "Offline".to_owned(),
            Self::SignedOut => "Signed out".to_owned(),
        }
    }
}

/// One report of how video we send fared: packets sent and asked for
/// again, and where an adapting capture stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoUpload {
    pub sent: u32,
    pub lost: u32,
    pub target_kbps: Option<u32>,
    pub fps: Option<u32>,
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
    /// Whose stream is being watched, if any.
    pub watching: Option<Snowflake>,
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
    /// The channel last viewed in each guild, restored when returning to it.
    pub last_channel: HashMap<Snowflake, Snowflake>,
    /// Collapsed categories.
    pub collapsed: HashSet<Snowflake>,
    /// Expanded server folders.
    pub open_folders: HashSet<i64>,
    /// Message being replied to in the composer.
    pub reply_to: Option<Snowflake>,
    /// Message being edited in the composer.
    pub editing: Option<Snowflake>,
    /// Files queued for the next message.
    pub uploads: Vec<Upload>,
    /// Scroll-back state per channel.
    pub history: HashMap<Snowflake, History>,
    /// Who is typing in each channel, and when they were last seen typing.
    pub typing: HashMap<Snowflake, Vec<(Snowflake, Instant)>>,
    pub friends_tab: FriendsTab,
    /// Channels whose member list has been requested this run.
    pub subscribed: HashSet<Snowflake>,
}

/// A file waiting in the composer's upload tray.
#[derive(Clone)]
pub struct Upload {
    pub name: String,
    pub bytes: Arc<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct History {
    /// A page of older messages is being fetched.
    pub loading: bool,
    /// The first message of the channel has been loaded.
    pub reached_top: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FriendsTab {
    #[default]
    Online,
    All,
    Pending,
    Blocked,
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
            last_channel: HashMap::new(),
            collapsed: HashSet::new(),
            open_folders: HashSet::new(),
            reply_to: None,
            editing: None,
            uploads: Vec::new(),
            history: HashMap::new(),
            typing: HashMap::new(),
            friends_tab: FriendsTab::default(),
            subscribed: HashSet::new(),
            account,
        }
    }
}

/// Why the UI asked for the camera list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraPurpose {
    Camera,
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
    /// Local playback volume for one participant (and their stream audio):
    /// `0.0` silences them, `1.0` is unchanged.
    SetUserVolume {
        user_id: Snowflake,
        volume: f32,
    },
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
        /// What the screen picker offers.
        sources: ShareSources,
        /// The capture the user previewed, to carry on as the stream.
        capture: Option<Box<PreparedCapture>>,
        /// Where the stream's sound comes from; `None` shares none.
        audio: Option<AudioShare>,
    },
    StartStreamCamera {
        access: CameraAccess,
        node_id: u32,
        settings: VideoSettings,
        delay_ms: u64,
        audio: Option<AudioShare>,
    },
    StopStream,
    /// Tune into the stream of someone in the current voice channel.
    WatchStream {
        user_id: Snowflake,
    },
    StopWatching,
    Subscribe {
        guild_id: Snowflake,
        channel_id: Snowflake,
    },
}

/// Voice command senders for running accounts, filled by [`spawn_account`].
pub static VOICE_COMMANDS: LazyLock<
    Mutex<HashMap<AccountId, mpsc::UnboundedSender<VoiceCommand>>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Gateway handles for running accounts, so the interface can send gateway
/// commands (member list ranges, member requests, presence) directly.
pub static GATEWAYS: LazyLock<Mutex<HashMap<AccountId, kc_gateway::session::GatewayHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The gateway handle of an account, when it is connected.
pub fn gateway(account: AccountId) -> Option<kc_gateway::session::GatewayHandle> {
    GATEWAYS.lock().ok()?.get(&account).cloned()
}

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
    SetUserVolume {
        account: AccountId,
        user_id: Snowflake,
        volume: f32,
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
        sources: ShareSources,
        capture: Option<Box<PreparedCapture>>,
        audio: Option<AudioShare>,
    },
    StartStreamCamera {
        account: AccountId,
        access: CameraAccess,
        node_id: u32,
        settings: VideoSettings,
        delay_ms: u64,
        audio: Option<AudioShare>,
    },
    StopStream {
        account: AccountId,
    },
    WatchStream {
        account: AccountId,
        user_id: Snowflake,
    },
    StopWatching {
        account: AccountId,
    },
    SubscribeChannel {
        account: AccountId,
        guild_id: Snowflake,
        channel_id: Snowflake,
    },
}

/// Tokio-to-GTK notifications.
/// A closure run on the GTK thread with the application state.
pub type UiTask = Box<dyn FnOnce(&std::rc::Rc<crate::shell::AppState>) + Send>;

pub enum UiEvent {
    /// Work to finish on the GTK thread after an asynchronous task.
    Run(UiTask),
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
    StoreEvent {
        account: AccountId,
        event: StoreEvent,
    },
    /// Discord closed the gateway; when `fatal` it will not come back.
    GatewayClosed {
        account: AccountId,
        code: Option<u16>,
        reason: String,
        fatal: bool,
    },
    /// How our own camera (or, with `stream`, our stream) fared on the way
    /// up over the last couple of seconds.
    VideoUpload {
        account: AccountId,
        stream: bool,
        upload: VideoUpload,
    },
    /// Discord wants the account verified before it does more (`None`
    /// once it is satisfied).
    RequiredAction {
        account: AccountId,
        action: Option<String>,
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
    /// The call's ping was measured (`None`: no reply).
    VoicePing {
        account: AccountId,
        millis: Option<u32>,
    },
    /// A DM call started or stopped ringing us.
    CallRinging {
        account: AccountId,
        channel_id: Snowflake,
        ringing: bool,
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
    /// Video from someone's camera (`stream: false`) or from the stream
    /// being watched started arriving; `feed` carries its frames.
    VideoStarted {
        account: AccountId,
        user_id: Snowflake,
        stream: bool,
        feed: Arc<VideoFeed>,
    },
    VideoStopped {
        account: AccountId,
        user_id: Snowflake,
        stream: bool,
        feed: Arc<VideoFeed>,
    },
    /// Whose stream is being watched changed (`None`: no longer watching).
    StreamWatch {
        account: AccountId,
        user_id: Option<Snowflake>,
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
        Command::SetUserVolume {
            account,
            user_id,
            volume,
        } => {
            forward_voice(account, VoiceCommand::SetUserVolume { user_id, volume }, ui).await;
        }
        Command::SetCamera { account, camera } => {
            forward_voice(account, VoiceCommand::SetCamera(camera), ui).await;
        }
        Command::StartStream {
            account,
            parent,
            settings,
            delay_ms,
            sources,
            capture,
            audio,
        } => {
            forward_voice(
                account,
                VoiceCommand::StartStream {
                    parent,
                    settings,
                    delay_ms,
                    sources,
                    capture,
                    audio,
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
            audio,
        } => {
            forward_voice(
                account,
                VoiceCommand::StartStreamCamera {
                    access,
                    node_id,
                    settings,
                    delay_ms,
                    audio,
                },
                ui,
            )
            .await;
        }
        Command::StopStream { account } => {
            forward_voice(account, VoiceCommand::StopStream, ui).await;
        }
        Command::WatchStream { account, user_id } => {
            forward_voice(account, VoiceCommand::WatchStream { user_id }, ui).await;
        }
        Command::StopWatching { account } => {
            forward_voice(account, VoiceCommand::StopWatching, ui).await;
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
        if let Ok(mut registry) = GATEWAYS.lock() {
            registry.insert(account, handle.clone());
        }
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
            stream_sources: ShareSources::ScreenOrWindow,
            stream_capture: None,
            stream_audio: Some(AudioShare::Desktop),
            stream_pending: false,
            streaming: false,
            stream_generation: 0,
            stream_attempt: 0,
            watch: None,
            watch_generation: 0,
            user_volumes: HashMap::new(),
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
                                // A call rings us when it starts (CALL_CREATE)
                                // or when we are rung again in one already
                                // going (CALL_UPDATE); it stops when they hang
                                // up, we answer elsewhere, or it ends.
                                "CALL_CREATE" | "CALL_UPDATE" => {
                                    let channel_id: Option<Snowflake> =
                                        serde_json::from_value(data["channel_id"].clone()).ok();
                                    let ringing: Vec<Snowflake> = data["ringing"]
                                        .as_array()
                                        .map(|list| {
                                            list.iter()
                                                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                                                .collect()
                                        })
                                        .unwrap_or_default();
                                    if let (Some(channel_id), Some(me)) =
                                        (channel_id, voice.self_user_id)
                                    {
                                        let in_call = voice
                                            .connection
                                            .as_ref()
                                            .is_some_and(|c| c.channel_id == channel_id);
                                        let _ = ui
                                            .send(UiEvent::CallRinging {
                                                account,
                                                channel_id,
                                                ringing: ringing.contains(&me) && !in_call,
                                            })
                                            .await;
                                    }
                                }
                                "CALL_DELETE" => {
                                    if let Ok(channel_id) =
                                        serde_json::from_value::<Snowflake>(data["channel_id"].clone())
                                    {
                                        let _ = ui
                                            .send(UiEvent::CallRinging {
                                                account,
                                                channel_id,
                                                ringing: false,
                                            })
                                            .await;
                                    }
                                }
                                _ => {}
                            }
                            if name == "READY" {
                                let user =
                                    serde_json::from_value::<User>(data["user"].clone()).ok();
                                let _ = ui.send(UiEvent::Ready { account, user }).await;
                            }
                            // READY names a pending requirement; the
                            // update also reports it being lifted.
                            let required = data["required_action"].as_str().map(str::to_owned);
                            if name == "USER_REQUIRED_ACTION_UPDATE"
                                || (name == "READY" && required.is_some())
                            {
                                let _ = ui
                                    .send(UiEvent::RequiredAction {
                                        account,
                                        action: required,
                                    })
                                    .await;
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
                        GatewayEvent::Closed {
                            code,
                            reason,
                            fatal,
                        } => {
                            let _ = ui
                                .send(UiEvent::GatewayClosed {
                                    account,
                                    code,
                                    reason,
                                    fatal,
                                })
                                .await;
                            // Otherwise the gateway reconnects and its
                            // events keep coming.
                            if fatal {
                                break;
                            }
                        }
                    }
                }
                command = voice_rx.recv() => {
                    let Some(command) = command else { break };
                    apply_voice_command(&mut voice, command, account, &ui, &control_tx).await;
                }
                control = control_rx.recv() => {
                    let Some(control) = control else { break };
                    handle_voice_control(&mut voice, control, account, &ui).await;
                }
            }
        }

        teardown_voice(&mut voice, account, &ui, true).await;
        if let Ok(mut registry) = VOICE_COMMANDS.lock() {
            registry.remove(&account);
        }
        if let Ok(mut registry) = GATEWAYS.lock() {
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
    stream_sources: ShareSources,
    stream_capture: Option<Box<PreparedCapture>>,
    stream_audio: Option<AudioShare>,
    stream_pending: bool,
    streaming: bool,
    stream_generation: u64,
    /// Counts Go Live requests, to tell a stale start timeout from the
    /// current one.
    stream_attempt: u64,
    /// Someone else's stream being watched.
    watch: Option<Watch>,
    watch_generation: u64,
    /// Local per-user playback volumes, re-applied after each (re)connect.
    user_volumes: HashMap<Snowflake, f32>,
}

/// Watching another participant's stream: its own connection, set up from
/// the gateway's answer to a watch request.
struct Watch {
    key: String,
    owner: Snowflake,
    rtc_id: Option<Snowflake>,
    token: Option<String>,
    endpoint: Option<String>,
    handle: Option<VoiceHandle>,
    generation: u64,
}

/// Whether your own camera and stream are shown to you in the call.
static SELF_PREVIEW: AtomicBool = AtomicBool::new(true);

/// Turns the preview of your own camera and stream on or off. It applies
/// to video already running: off at the next frame, on at the next keyframe.
pub(crate) fn set_self_preview(enabled: bool) {
    SELF_PREVIEW.store(enabled, Ordering::Relaxed);
}

/// Your own camera or stream, shown back to you: the frames being sent are
/// also decoded locally, like anyone else's video. It lives in the capture's
/// frame callback and ends with the capture.
struct LocalPreview {
    account: AccountId,
    user_id: Option<Snowflake>,
    stream: bool,
    ui: async_channel::Sender<UiEvent>,
    feed: Option<Arc<VideoFeed>>,
}

impl LocalPreview {
    fn new(
        account: AccountId,
        user_id: Option<Snowflake>,
        stream: bool,
        ui: async_channel::Sender<UiEvent>,
    ) -> Self {
        Self {
            account,
            user_id,
            stream,
            ui,
            feed: None,
        }
    }

    fn push(&mut self, frame: &[u8]) {
        let Some(user_id) = self.user_id else {
            return;
        };
        if !SELF_PREVIEW.load(Ordering::Relaxed) {
            self.end();
            return;
        }
        let keyframe = is_keyframe(frame);
        let feed = match &self.feed {
            Some(feed) => feed,
            // Decoding can only start from a keyframe.
            None if !keyframe => return,
            None => {
                let feed = Arc::new(VideoFeed::default());
                let _ = self.ui.try_send(UiEvent::VideoStarted {
                    account: self.account,
                    user_id,
                    stream: self.stream,
                    feed: feed.clone(),
                });
                self.feed.insert(feed)
            }
        };
        feed.push(frame.to_vec(), keyframe);
    }

    fn end(&mut self) {
        if let (Some(feed), Some(user_id)) = (self.feed.take(), self.user_id) {
            let _ = self.ui.try_send(UiEvent::VideoStopped {
                account: self.account,
                user_id,
                stream: self.stream,
                feed,
            });
        }
    }
}

impl Drop for LocalPreview {
    fn drop(&mut self) {
        self.end();
    }
}

/// The feeds for video arriving on one connection. Tells the interface when
/// a person's video starts and stops.
struct VideoFeeds {
    account: AccountId,
    stream: bool,
    ui: async_channel::Sender<UiEvent>,
    feeds: HashMap<Snowflake, Arc<VideoFeed>>,
}

impl VideoFeeds {
    fn new(account: AccountId, stream: bool, ui: async_channel::Sender<UiEvent>) -> Self {
        Self {
            account,
            stream,
            ui,
            feeds: HashMap::new(),
        }
    }

    fn push(&mut self, user_id: Snowflake, frame: Vec<u8>, keyframe: bool) {
        let feed = self.feeds.entry(user_id).or_insert_with(|| {
            let feed = Arc::new(VideoFeed::default());
            tracing::info!(%user_id, stream = self.stream, bytes = frame.len(), "first video frame");
            let _ = self.ui.try_send(UiEvent::VideoStarted {
                account: self.account,
                user_id,
                stream: self.stream,
                feed: feed.clone(),
            });
            feed
        });
        feed.push(frame, keyframe);
    }

    fn end(&mut self, user_id: Snowflake) {
        if let Some(feed) = self.feeds.remove(&user_id) {
            let _ = self.ui.try_send(UiEvent::VideoStopped {
                account: self.account,
                user_id,
                stream: self.stream,
                feed,
            });
        }
    }

    fn end_all(&mut self) {
        let users: Vec<Snowflake> = self.feeds.keys().copied().collect();
        for user_id in users {
            self.end(user_id);
        }
    }
}

/// The playback mixer's key for the sound of a stream, kept apart from the
/// same person's voice.
fn stream_audio_key(user_id: Snowflake) -> u64 {
    user_id.get() | 1 << 63
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
        result: Result<Box<VideoSource>, String>,
    },
    ForceKeyframe {
        generation: u64,
    },
    ForceCameraKeyframe {
        generation: u64,
    },
    /// A Go Live asked for `STREAM_START_TIMEOUT` ago; if Discord has not
    /// answered it, it is given up.
    StreamStartTimeout {
        attempt: u64,
    },
    /// How the camera's (or, with `stream`, the stream's) video is faring
    /// on the network, for adapting its bitrate.
    VideoNetwork {
        generation: u64,
        stream: bool,
        sent: u32,
        lost: u32,
        remb_kbps: u32,
    },
    WatchClosed {
        generation: u64,
    },
}

/// Acts on a signal from one of the account's voice connections (see
/// [`VoiceControl`]).
async fn handle_voice_control(
    voice: &mut VoiceRuntime,
    control: VoiceControl,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
) {
    match control {
        VoiceControl::Closed { generation } => {
            if generation == voice.generation && voice.connection.is_some() {
                teardown_voice(voice, account, ui, true).await;
            }
        }
        VoiceControl::VideoNetwork {
            generation,
            stream,
            sent,
            lost,
            remb_kbps,
        } => {
            let source = if stream {
                (generation == voice.stream_generation)
                    .then_some(voice.stream_connection.as_ref())
                    .flatten()
                    .and_then(|connection| connection.source.as_ref())
            } else {
                (generation == voice.generation)
                    .then_some(voice.connection.as_ref())
                    .flatten()
                    .and_then(|connection| connection.camera.as_ref())
            };
            if let Some(source) = source {
                source.network_report(sent, lost, remb_kbps);
            }
            let (target_kbps, fps) = source.map(VideoSource::adapted).unwrap_or_default();
            let _ = ui.try_send(UiEvent::VideoUpload {
                account,
                stream,
                upload: VideoUpload {
                    sent,
                    lost,
                    target_kbps,
                    fps,
                },
            });
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
            if generation == voice.stream_generation && voice.stream_connection.is_some() {
                voice.streaming = true;
                let _ = ui.send(UiEvent::StreamStarted { account }).await;
            }
        }
        VoiceControl::WatchClosed { generation } => {
            if voice
                .watch
                .as_ref()
                .is_some_and(|watch| watch.generation == generation)
            {
                stop_watching(voice, account, ui, true).await;
            }
        }
        VoiceControl::StreamClosed { generation } => {
            if generation == voice.stream_generation && voice.stream_connection.is_some() {
                // Deleted on Discord too (harmless if it is gone
                // already), so the next Go Live is answered.
                stop_stream(voice, account, ui, true).await;
            }
        }
        VoiceControl::StreamStartTimeout { attempt } => {
            if attempt == voice.stream_attempt && voice.stream_pending {
                tracing::warn!("Discord did not start the stream in time");
                let _ = ui
                    .send(UiEvent::StreamError {
                        account,
                        message: "Discord didn't start the stream. Try again.".to_owned(),
                    })
                    .await;
                stop_stream(voice, account, ui, true).await;
            }
        }
        VoiceControl::StreamSource { generation, result } => {
            if generation != voice.stream_generation {
                return;
            }
            match result {
                Ok(source) => {
                    let delay_ms = voice.stream_delay_ms;
                    let share_audio = voice.stream_audio.clone();
                    if let Some(connection) = &mut voice.stream_connection {
                        tracing::info!("stream source ready");
                        source.adapt(&voice.stream_settings);
                        source.set_keyframe_gap(Duration::from_secs(u64::from(
                            voice.stream_settings.keyframe_gap_secs,
                        )));
                        connection.source = Some(*source);
                        if connection.audio.is_none()
                            && let Some(share) = share_audio
                        {
                            match ScreenAudio::start(
                                connection.handle.clone(),
                                128_000,
                                delay_ms,
                                share,
                            ) {
                                Ok(audio) => connection.audio = Some(audio),
                                Err(error) => {
                                    let _ = ui
                                        .send(UiEvent::Error {
                                            account: Some(account),
                                            message: format!("screen audio unavailable: {error}"),
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
                    // Closing the picker is a choice, not an error.
                    if message != PICKER_CANCELLED {
                        let _ = ui.send(UiEvent::StreamError { account, message }).await;
                    }
                    // Discord already has the stream: delete it, or
                    // it never answers the next Go Live.
                    stop_stream(voice, account, ui, true).await;
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

/// Builds a stream key: `guild:{guild}:{channel}:{owner}` in a guild, or
/// `call:{channel}:{owner}` in a DM or group-DM call.
fn stream_key(guild_id: Option<Snowflake>, channel_id: Snowflake, owner: Snowflake) -> String {
    match guild_id {
        Some(guild_id) => format!("guild:{guild_id}:{channel_id}:{owner}"),
        None => format!("call:{channel_id}:{owner}"),
    }
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
    if let Some(watch) = &mut voice.watch
        && data["stream_key"].as_str() == Some(watch.key.as_str())
    {
        watch.rtc_id = data["rtc_server_id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .or_else(|| data["rtc_server_id"].as_u64().map(Snowflake::new));
        tracing::info!(key = %watch.key, rtc = ?watch.rtc_id, "watched stream created");
        maybe_start_watch(voice, account, ui, control_tx).await;
        return;
    }
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
    if let Some(watch) = &mut voice.watch
        && data["stream_key"].as_str() == Some(watch.key.as_str())
    {
        if let Some(token) = data["token"].as_str() {
            watch.token = Some(token.to_owned());
        }
        if let Some(endpoint) = data["endpoint"].as_str() {
            watch.endpoint = Some(endpoint.to_owned());
        }
        maybe_start_watch(voice, account, ui, control_tx).await;
        return;
    }
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
    if voice.watch.as_ref().is_some_and(|watch| watch.key == key) {
        let reason = data["reason"].as_str().unwrap_or("");
        tracing::info!(key, reason, "watched stream ended");
        // The server already dropped us; there is nothing to tell it.
        stop_watching(voice, account, ui, false).await;
        let message = match reason {
            "" | "user_requested" | "stream_ended" => None,
            "stream_full" => Some("That stream is full".to_owned()),
            "unauthorized" => Some("You are not allowed to watch that stream".to_owned()),
            other => Some(format!("Could not watch the stream ({other})")),
        };
        if let Some(message) = message {
            let _ = ui
                .send(UiEvent::Error {
                    account: Some(account),
                    message,
                })
                .await;
        }
        return;
    }
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
        receive_only: false,
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
                        VoiceEvent::VideoNetwork {
                            sent,
                            lost,
                            remb_kbps,
                        } => {
                            let _ = control.send(VoiceControl::VideoNetwork {
                                generation,
                                stream: true,
                                sent,
                                lost,
                                remb_kbps,
                            });
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
            let screen_sources = voice.stream_sources;
            let prepared_capture = voice.stream_capture.take();
            let video_settings = voice.stream_settings.clone();
            handle
                .set_retransmit_window(Duration::from_millis(video_settings.retransmit_ms as u64));
            handle.set_redundancy(video_settings.send_twice);
            handle.set_loss_keyframe_spacing(Duration::from_millis(u64::from(
                video_settings.loss_keyframe_ms,
            )));
            let stream_delay_ms = voice.stream_delay_ms;
            let drop_after_ms = video_settings.drop_frames_after_ms;
            let drop_after =
                (drop_after_ms > 0).then(|| Duration::from_millis(drop_after_ms as u64));
            let video_send = source_handle.clone();
            let video_delay = DelayedSender::start_with_limit_and_keep(
                Duration::from_millis(stream_delay_ms),
                drop_after,
                |data: &Vec<u8>| is_keyframe(data),
                move |data: Vec<u8>, frame_ms| video_send.send_video(data, frame_ms),
            );
            let last_frame = Arc::new(Mutex::new(Instant::now()));
            let stream_preview =
                LocalPreview::new(account, voice.self_user_id, true, source_ui.clone());
            tokio::spawn(async move {
                let result = if let Some((access, node_id)) = camera_source {
                    access
                        .select(node_id)
                        .ok_or_else(|| "selected camera is no longer available".to_owned())
                        .and_then(|camera| {
                            let video_delay = video_delay.clone();
                            let last_frame = last_frame.clone();
                            let mut preview = stream_preview;
                            VideoSource::camera_portal(
                                camera,
                                video_settings.clone(),
                                move |frame| {
                                    if let Ok(mut last) = last_frame.lock() {
                                        let gap = last.elapsed();
                                        if gap > Duration::from_millis(500) {
                                            tracing::warn!(
                                                gap_ms = gap.as_millis(),
                                                "encoded camera frame gap"
                                            );
                                        }
                                        *last = Instant::now();
                                    }
                                    preview.push(&frame.data);
                                    video_delay.send(frame.data, frame.duration_ms);
                                },
                            )
                            .map_err(|error| error.to_string())
                        })
                } else {
                    let first_frame = Arc::new(AtomicBool::new(false));
                    let frame_count = Arc::new(AtomicUsize::new(0));
                    let watchdog_frames = frame_count.clone();
                    let on_frame = {
                        let video_delay = video_delay.clone();
                        let last_frame = last_frame.clone();
                        let mut preview = stream_preview;
                        move |frame: EncodedFrame| {
                            if let Ok(mut last) = last_frame.lock() {
                                let gap = last.elapsed();
                                if gap > Duration::from_millis(500) {
                                    tracing::warn!(
                                        gap_ms = gap.as_millis(),
                                        "encoded screen frame gap"
                                    );
                                }
                                *last = Instant::now();
                            }
                            frame_count.fetch_add(1, Ordering::Relaxed);
                            if !first_frame.swap(true, Ordering::Relaxed) {
                                tracing::info!(
                                    bytes = frame.data.len(),
                                    "stream first encoded video frame"
                                );
                            }
                            preview.push(&frame.data);
                            video_delay.send(frame.data, frame.duration_ms);
                        }
                    };
                    let result = match prepared_capture {
                        // The capture previewed in the share popup carries on
                        // as the stream: same screen, no second picker. A
                        // keyframe lets viewers start at once.
                        Some(prepared) => {
                            let prepared = *prepared;
                            tracing::info!("going live with the previewed capture");
                            prepared.tap.set(on_frame);
                            prepared.source.force_keyframe();
                            Ok(prepared.source)
                        }
                        None => {
                            let request = tokio::time::timeout(
                                std::time::Duration::from_secs(120),
                                kc_media::portal::request_sources(
                                    screen_parent.as_deref(),
                                    screen_sources,
                                ),
                            )
                            .await;
                            match request {
                                Ok(Ok(portal)) => {
                                    VideoSource::screen(portal, video_settings.clone(), on_frame)
                                        .map_err(|error| error.to_string())
                                }
                                Ok(Err(kc_media::Error::Cancelled)) => {
                                    tracing::info!("screen picker cancelled");
                                    Err(PICKER_CANCELLED.to_owned())
                                }
                                Ok(Err(error)) => {
                                    tracing::warn!(%error, "screen capture source failed");
                                    Err(error.to_string())
                                }
                                Err(_) => Err("screen picker timed out".to_owned()),
                            }
                        }
                    };
                    if result.is_ok() {
                        let ui_events = source_ui.clone();
                        let control = source_control.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                            if watchdog_frames.load(Ordering::Relaxed) == 0 {
                                let _ = ui_events
                                    .send(UiEvent::StreamError {
                                        account,
                                        message: "screen capture produced no frames".to_owned(),
                                    })
                                    .await;
                                let _ = control.send(VoiceControl::StreamClosed { generation });
                            }
                        });
                    }
                    result
                };
                let _ = source_control.send(VoiceControl::StreamSource {
                    generation,
                    result: result.map(Box::new),
                });
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
        receive_only: false,
    };
    voice.pending_join = None;
    voice.generation = voice.generation.wrapping_add(1);
    let generation = voice.generation;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<VoiceEvent>();
    match kc_voice::connect(params, event_tx).await {
        Ok(handle) => {
            tracing::info!(%channel_id, "voice connection started");
            // The microphone's own speech detection lights the local ring.
            let speaking_ui = ui.clone();
            let on_speaking = move |speaking: bool| {
                let _ = speaking_ui.try_send(UiEvent::VoiceSpeaking {
                    account,
                    user_id,
                    speaking,
                });
            };
            let audio = match VoiceAudio::start(handle.clone(), on_speaking) {
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
                // Re-apply any per-user volumes chosen before this connect.
                for (user_id, volume) in &voice.user_volumes {
                    audio.set_volume(user_id.get(), *volume);
                    audio.set_volume(stream_audio_key(*user_id), *volume);
                }
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
                let mut report = tokio::time::interval(Duration::from_secs(5));
                let mut reported = kc_media::receive::ReceiveStats::default();
                let mut videos = VideoFeeds::new(account, false, ui.clone());
                loop {
                    let event = tokio::select! {
                        event = event_rx.recv() => event,
                        _ = report.tick() => {
                            // How playback coped with what the network delivered.
                            if let Some(audio) = &audio {
                                let stats = audio.receive_stats();
                                if stats != reported {
                                    debug!(?stats, "voice playback");
                                    reported = stats;
                                }
                            }
                            continue;
                        }
                    };
                    let Some(event) = event else { break };
                    match event {
                        VoiceEvent::Audio {
                            user_id,
                            opus,
                            sequence,
                            ..
                        } => {
                            if let Some(audio) = &audio {
                                audio.push_remote(user_id.get(), sequence, &opus);
                            }
                        }
                        VoiceEvent::ClientDisconnect { user_id } => {
                            if let Some(audio) = &audio {
                                audio.remove_remote(user_id.get());
                            }
                            videos.end(user_id);
                        }
                        VoiceEvent::Video {
                            user_id,
                            frame,
                            keyframe,
                            ..
                        } => videos.push(user_id, frame, keyframe),
                        VoiceEvent::VideoEnded { user_id } => videos.end(user_id),
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
                        VoiceEvent::Ping { millis } => {
                            let _ = ui.try_send(UiEvent::VoicePing { account, millis });
                        }
                        VoiceEvent::VideoNetwork {
                            sent,
                            lost,
                            remb_kbps,
                        } => {
                            let _ = control.send(VoiceControl::VideoNetwork {
                                generation,
                                stream: false,
                                sent,
                                lost,
                                remb_kbps,
                            });
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
                videos.end_all();
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
    mut settings: VideoSettings,
    mut preview: LocalPreview,
) -> Result<(), String> {
    // A camera sends keyframes only when a viewer asks or packets are lost,
    // like the official client. On a schedule, about one in six froze an
    // official-client viewer until the next one: hitches that lasted exactly
    // the keyframe interval and followed keyframes, not packet loss.
    settings.keyframe_secs = 0;
    let camera = access
        .select(node_id)
        .ok_or_else(|| "selected camera is no longer available".to_owned())?;
    debug!(camera = %camera.name, "starting camera");
    let handle = connection.handle.clone();
    handle.set_retransmit_window(Duration::from_millis(settings.retransmit_ms as u64));
    handle.set_redundancy(settings.send_twice);
    handle.set_loss_keyframe_spacing(Duration::from_millis(u64::from(settings.loss_keyframe_ms)));
    let adaptation = settings.clone();
    let keyframe_gap = Duration::from_secs(u64::from(settings.keyframe_gap_secs));
    // Opening a camera can take a few seconds while formats are tried; keep
    // it off the account loop.
    let source = tokio::task::spawn_blocking(move || {
        VideoSource::camera_portal(camera, settings, move |frame| {
            preview.push(&frame.data);
            handle.send_video(frame.data, frame.duration_ms);
        })
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    source.adapt(&adaptation);
    source.set_keyframe_gap(keyframe_gap);
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
            // Tell Discord first, so the notice is on its way even when the
            // app is about to exit.
            voice
                .handle
                .update_voice_state(guild_id, None, voice.muted, voice.deafened, false);
            if voice.connection.is_some() || voice.pending_join.is_some() {
                teardown_voice(voice, account, ui, true).await;
            }
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
        VoiceCommand::SetUserVolume { user_id, volume } => {
            voice.user_volumes.insert(user_id, volume);
            if let Some(audio) = voice.connection.as_ref().and_then(|c| c.audio.as_ref()) {
                audio.set_volume(user_id.get(), volume);
                // The same person's shared stream sound rides a separate key.
                audio.set_volume(stream_audio_key(user_id), volume);
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
            let preview = LocalPreview::new(account, voice.self_user_id, false, ui.clone());
            match &mut voice.connection {
                Some(connection) => {
                    match start_camera(connection, &access, node_id, settings, preview).await {
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
                    }
                }
                None => {
                    voice.camera = false;
                    let _ = ui
                        .send(UiEvent::VoiceError {
                            account,
                            message: "join a voice channel before enabling the camera".to_owned(),
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
            sources,
            capture,
            audio,
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
            tracing::info!(?guild_id, %channel_id, "creating go live stream");
            voice.stream_camera = None;
            voice.stream_parent = parent;
            voice.stream_settings = settings;
            voice.stream_delay_ms = delay_ms;
            voice.stream_sources = sources;
            voice.stream_capture = capture;
            voice.stream_audio = audio;
            voice.stream_pending = true;
            voice.stream_attempt = voice.stream_attempt.wrapping_add(1);
            give_up_unanswered_stream(voice.stream_attempt, control_tx);
            voice.stream_key = None;
            voice.stream_rtc_id = None;
            voice.stream_token = None;
            voice.stream_endpoint = None;
            voice.handle.stream_create(guild_id, channel_id);
            // DVS sends STREAM_SET_PAUSED(false) immediately after create;
            // without it the stream is announced but never actually starts.
            if let Some(user_id) = voice.self_user_id {
                let key = stream_key(guild_id, channel_id, user_id);
                voice.handle.stream_pause(&key, false);
            }
        }
        VoiceCommand::StartStreamCamera {
            access,
            node_id,
            settings,
            delay_ms,
            audio,
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
            tracing::info!(?guild_id, %channel_id, node_id, "creating camera go live stream");
            voice.stream_camera = Some((access, node_id));
            voice.stream_parent = None;
            voice.stream_settings = settings;
            voice.stream_delay_ms = delay_ms;
            voice.stream_capture = None;
            voice.stream_audio = audio;
            voice.stream_pending = true;
            voice.stream_attempt = voice.stream_attempt.wrapping_add(1);
            give_up_unanswered_stream(voice.stream_attempt, control_tx);
            voice.stream_key = None;
            voice.stream_rtc_id = None;
            voice.stream_token = None;
            voice.stream_endpoint = None;
            voice.handle.stream_create(guild_id, channel_id);
            if let Some(user_id) = voice.self_user_id {
                let key = stream_key(guild_id, channel_id, user_id);
                voice.handle.stream_pause(&key, false);
            }
        }
        VoiceCommand::StopStream => {
            stop_stream(voice, account, ui, true).await;
        }
        VoiceCommand::WatchStream { user_id } => {
            let Some(connection) = &voice.connection else {
                return;
            };
            let key = stream_key(connection.guild_id, connection.channel_id, user_id);
            if voice.watch.as_ref().is_some_and(|watch| watch.key == key) {
                return;
            }
            stop_watching(voice, account, ui, true).await;
            voice.watch_generation = voice.watch_generation.wrapping_add(1);
            tracing::info!(%key, "asking to watch stream");
            voice.handle.stream_watch(&key);
            voice.watch = Some(Watch {
                key,
                owner: user_id,
                rtc_id: None,
                token: None,
                endpoint: None,
                handle: None,
                generation: voice.watch_generation,
            });
            let _ = ui
                .send(UiEvent::StreamWatch {
                    account,
                    user_id: Some(user_id),
                })
                .await;
        }
        VoiceCommand::StopWatching => {
            stop_watching(voice, account, ui, true).await;
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
/// Connects to the watched stream once the gateway has sent everything
/// needed (its server id, token and endpoint).
async fn maybe_start_watch(
    voice: &mut VoiceRuntime,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) {
    let Some(watch) = &voice.watch else {
        return;
    };
    if watch.handle.is_some() {
        return;
    }
    let (Some(rtc_id), Some(token), Some(endpoint), Some(session_id), Some(user_id)) = (
        watch.rtc_id,
        watch.token.clone(),
        watch.endpoint.clone(),
        voice.session_id.clone(),
        voice.self_user_id,
    ) else {
        return;
    };
    let Some(connection) = &voice.connection else {
        return;
    };
    let audio = connection.audio.clone();
    let owner = watch.owner;
    let generation = watch.generation;
    let params = VoiceParams {
        server_id: rtc_id,
        channel_id: connection.channel_id,
        user_id,
        session_id,
        token,
        endpoint,
        video: true,
        screen: true,
        dave_channel_id: Snowflake::new(rtc_id.get().saturating_sub(1)),
        receive_only: true,
    };
    tracing::info!(%rtc_id, key = %watch.key, endpoint = %params.endpoint, "connecting to watched stream");

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<VoiceEvent>();
    match kc_voice::connect(params, event_tx).await {
        Ok(handle) => {
            if let Some(watch) = &mut voice.watch {
                watch.handle = Some(handle);
            }
            let ui = ui.clone();
            let control = control_tx.clone();
            tokio::spawn(async move {
                let mut videos = VideoFeeds::new(account, true, ui.clone());
                while let Some(event) = event_rx.recv().await {
                    match event {
                        VoiceEvent::Video {
                            user_id,
                            frame,
                            keyframe,
                            ..
                        } => videos.push(user_id, frame, keyframe),
                        VoiceEvent::VideoEnded { user_id } => videos.end(user_id),
                        // The stream's own sound (a shared application).
                        VoiceEvent::Audio {
                            user_id,
                            opus,
                            sequence,
                            ..
                        } => {
                            if let Some(audio) = &audio {
                                audio.push_remote(stream_audio_key(user_id), sequence, &opus);
                            }
                        }
                        VoiceEvent::Ready { .. } => tracing::info!("watched stream connected"),
                        VoiceEvent::DaveReady => tracing::info!("watched stream DAVE ready"),
                        VoiceEvent::Error(message) => {
                            tracing::warn!(%message, "watched stream error");
                            break;
                        }
                        VoiceEvent::Closed { code, reason } => {
                            tracing::info!(?code, %reason, "watched stream closed");
                            break;
                        }
                        _ => {}
                    }
                }
                videos.end_all();
                if let Some(audio) = &audio {
                    audio.remove_remote(stream_audio_key(owner));
                }
                let _ = control.send(VoiceControl::WatchClosed { generation });
            });
        }
        Err(error) => {
            tracing::warn!(%error, "connecting to watched stream failed");
            let _ = ui
                .send(UiEvent::Error {
                    account: Some(account),
                    message: format!("Could not connect to the stream: {error}"),
                })
                .await;
            stop_watching(voice, account, ui, true).await;
        }
    }
}

/// Stops watching. `tell_server` is false when the server ended it.
async fn stop_watching(
    voice: &mut VoiceRuntime,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    tell_server: bool,
) {
    let Some(watch) = voice.watch.take() else {
        return;
    };
    if let Some(handle) = watch.handle {
        handle.disconnect().await;
    }
    if tell_server {
        // For a viewer this leaves the stream; it only ends the owner's.
        voice.handle.stream_delete(&watch.key);
    }
    let _ = ui
        .send(UiEvent::StreamWatch {
            account,
            user_id: None,
        })
        .await;
}

/// How long Discord gets to answer a Go Live before it is given up. It
/// answers in well under a second; without a limit, an unanswered one
/// would block every later Go Live.
const STREAM_START_TIMEOUT: Duration = Duration::from_secs(15);

/// What a screen-picker result reads as when the picker was closed.
const PICKER_CANCELLED: &str = "screen picker cancelled";

fn give_up_unanswered_stream(attempt: u64, control_tx: &mpsc::UnboundedSender<VoiceControl>) {
    let control = control_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(STREAM_START_TIMEOUT).await;
        let _ = control.send(VoiceControl::StreamStartTimeout { attempt });
    });
}

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
    voice.stream_capture = None;
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
    stop_watching(voice, account, ui, true).await;
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
        "Discord no longer accepts this account's sign-in".to_owned()
    } else if error.needs_verification() {
        "Discord wants this account verified first: open Discord in a browser and follow what it asks"
            .to_owned()
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

    #[test]
    fn local_preview_starts_on_a_keyframe_and_ends_with_the_capture() {
        // An IDR slice and a P slice, as Annex-B.
        let key = [0u8, 0, 0, 1, 0x65, 1, 2, 3];
        let delta = [0u8, 0, 0, 1, 0x41, 1, 2, 3];
        let (tx, rx) = async_channel::unbounded();
        let started = |rx: &async_channel::Receiver<UiEvent>| match rx.try_recv() {
            Ok(UiEvent::VideoStarted { stream, .. }) => Some(("started", stream)),
            Ok(UiEvent::VideoStopped { stream, .. }) => Some(("stopped", stream)),
            _ => None,
        };
        set_self_preview(true);
        let mut preview = LocalPreview::new(7, Some(Snowflake(1)), true, tx);
        preview.push(&delta);
        assert!(started(&rx).is_none(), "nothing to show before a keyframe");
        preview.push(&key);
        assert_eq!(started(&rx), Some(("started", true)));
        preview.push(&delta);
        assert!(started(&rx).is_none());
        // Switched off mid-capture, then on again.
        set_self_preview(false);
        preview.push(&delta);
        assert_eq!(started(&rx), Some(("stopped", true)));
        set_self_preview(true);
        preview.push(&delta);
        assert!(started(&rx).is_none());
        preview.push(&key);
        assert_eq!(started(&rx), Some(("started", true)));
        drop(preview);
        assert_eq!(started(&rx), Some(("stopped", true)));
    }

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
            stream_sources: ShareSources::ScreenOrWindow,
            stream_capture: None,
            stream_audio: Some(AudioShare::Desktop),
            stream_pending: false,
            streaming: false,
            stream_generation: 0,
            stream_attempt: 0,
            watch: None,
            watch_generation: 0,
            user_volumes: HashMap::new(),
        }
    }

    /// What the interface was told, by kind.
    fn told(rx: &async_channel::Receiver<UiEvent>) -> Vec<&'static str> {
        let mut kinds = Vec::new();
        while let Ok(event) = rx.try_recv() {
            kinds.push(match event {
                UiEvent::StreamError { .. } => "error",
                UiEvent::StreamStopped { .. } => "stopped",
                _ => "other",
            });
        }
        kinds
    }

    /// A voice runtime whose Go Live Discord has accepted, waiting for its
    /// screen to be picked.
    fn picking_a_screen() -> VoiceRuntime {
        let (handle, _events, task) = Gateway::new(Token::new("test")).start();
        task.abort();
        let mut voice = runtime(handle);
        voice.stream_key = Some("guild:5:12:1".to_owned());
        voice.stream_generation = 7;
        voice
    }

    #[tokio::test]
    async fn closing_the_screen_picker_ends_the_stream_quietly() {
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let mut voice = picking_a_screen();
        handle_voice_control(
            &mut voice,
            VoiceControl::StreamSource {
                generation: 7,
                result: Err(PICKER_CANCELLED.to_owned()),
            },
            0,
            &ui_tx,
        )
        .await;
        // No error, and nothing left to block the next Go Live.
        assert_eq!(told(&ui_rx), ["stopped"]);
        assert!(voice.stream_key.is_none() && !voice.stream_pending && !voice.streaming);
    }

    #[tokio::test]
    async fn a_screen_that_fails_to_start_is_reported_and_ended() {
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let mut voice = picking_a_screen();
        handle_voice_control(
            &mut voice,
            VoiceControl::StreamSource {
                generation: 7,
                result: Err("screencast start: no portal".to_owned()),
            },
            0,
            &ui_tx,
        )
        .await;
        assert_eq!(told(&ui_rx), ["error", "stopped"]);
        assert!(voice.stream_key.is_none());
    }

    #[tokio::test]
    async fn an_unanswered_go_live_is_given_up() {
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let (handle, _events, task) = Gateway::new(Token::new("test")).start();
        task.abort();
        let mut voice = runtime(handle);
        voice.stream_pending = true;
        voice.stream_attempt = 3;
        // The timeout of an earlier attempt changes nothing.
        handle_voice_control(
            &mut voice,
            VoiceControl::StreamStartTimeout { attempt: 2 },
            0,
            &ui_tx,
        )
        .await;
        assert!(told(&ui_rx).is_empty());
        assert!(voice.stream_pending);
        // This attempt's does: the next Go Live is free to start.
        handle_voice_control(
            &mut voice,
            VoiceControl::StreamStartTimeout { attempt: 3 },
            0,
            &ui_tx,
        )
        .await;
        assert_eq!(told(&ui_rx), ["error", "stopped"]);
        assert!(!voice.stream_pending);
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
