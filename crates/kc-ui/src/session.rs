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

impl VoiceUiState {
    /// How the account's own entry in `channel_id` shows mute and deafen:
    /// what was chosen here, without waiting for Discord's echo. `None`
    /// when the account is not in that channel.
    pub fn own_flags(&self, channel_id: Snowflake) -> Option<(bool, bool)> {
        (self.channel_id == channel_id).then_some((self.muted || self.deafened, self.deafened))
    }
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
    /// When slowmode lets the account post again, per channel.
    pub slowmode_until: HashMap<Snowflake, Instant>,
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
            slowmode_until: HashMap::new(),
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
    /// Local playback volume for one participant's voice: `0.0` silences
    /// them, `1.0` is unchanged.
    SetUserVolume {
        user_id: Snowflake,
        volume: f32,
    },
    /// Local playback volume for one participant's shared stream sound,
    /// kept apart from their voice.
    SetStreamVolume {
        user_id: Snowflake,
        volume: f32,
    },
    /// Gain on the user's own outgoing shared-stream sound (what viewers
    /// hear), as a linear multiplier; `1.0` is unchanged. Takes effect live
    /// on a running stream and is kept for the next one.
    SetStreamAudioGain {
        gain: f32,
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
    /// Stops the account loop: its voice/stream are torn down and its
    /// gateway socket and task are closed. Used when logging an account out.
    StopAccount {
        account: AccountId,
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
    SetStreamVolume {
        account: AccountId,
        user_id: Snowflake,
        volume: f32,
    },
    SetStreamAudioGain {
        account: AccountId,
        gain: f32,
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
        /// It ended because the connection could not be kept.
        lost: bool,
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
        /// Whether the call is ringing anyone at all (when we call, our
        /// ring stops once nobody is rung any more).
        anyone: bool,
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
        Command::StopAccount { account } => {
            // Dropping the voice sender closes the account loop's command
            // channel, so the loop ends, tears down any call and aborts the
            // gateway task. Removing the gateway handle too stops any
            // further gateway sends immediately.
            if let Ok(mut registry) = VOICE_COMMANDS.lock() {
                registry.remove(&account);
            }
            if let Ok(mut registry) = GATEWAYS.lock() {
                registry.remove(&account);
            }
        }
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
        Command::SetStreamVolume {
            account,
            user_id,
            volume,
        } => {
            forward_voice(
                account,
                VoiceCommand::SetStreamVolume { user_id, volume },
                ui,
            )
            .await;
        }
        Command::SetStreamAudioGain { account, gain } => {
            forward_voice(account, VoiceCommand::SetStreamAudioGain { gain }, ui).await;
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
        let (handle, mut gateway_rx, gateway_task) = Gateway::new(token).start();
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
            stream_audio_gain: 1.0,
            stream_pending: false,
            streaming: false,
            stream_generation: 0,
            stream_attempt: 0,
            watch: None,
            watch_generation: 0,
            user_volumes: HashMap::new(),
            stream_volumes: HashMap::new(),
            voice_retries: 0,
            relocating: false,
            call_lost: false,
            stream_retries: 0,
            stream_carry: None,
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
                                    if voice.connection.is_some() {
                                        rejoin_after_new_session(
                                            &mut voice,
                                            account,
                                            &ui,
                                            &control_tx,
                                        )
                                        .await;
                                    }
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
                                                anyone: !ringing.is_empty(),
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
                                                anyone: false,
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
                    handle_voice_control(&mut voice, control, account, &ui, &control_tx).await;
                }
            }
        }

        teardown_voice(&mut voice, account, &ui, true, "the account stopped").await;
        if let Ok(mut registry) = VOICE_COMMANDS.lock() {
            registry.remove(&account);
        }
        if let Ok(mut registry) = GATEWAYS.lock() {
            registry.remove(&account);
        }
        // The gateway reconnects on its own after a drop, so dropping its
        // handle is not enough: abort the task so a stopped account leaves
        // no socket or heartbeat behind.
        gateway_task.abort();
    });
}

/// Live voice state for one account, owned by its account loop.
struct VoiceRuntime {
    /// The call is ending because it could not be kept (for the "call
    /// disconnected" sound rather than the leave sound).
    call_lost: bool,
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
    /// Linear multiplier for the user's own outgoing shared-stream sound, so
    /// viewers can hear it louder. `1.0` is unchanged. Applied when a stream
    /// starts and live while one runs.
    stream_audio_gain: f32,
    stream_pending: bool,
    streaming: bool,
    stream_generation: u64,
    /// Counts Go Live requests, to tell a stale start timeout from the
    /// current one.
    stream_attempt: u64,
    /// Someone else's stream being watched.
    watch: Option<Watch>,
    watch_generation: u64,
    /// Local per-user voice playback volumes, re-applied after each
    /// (re)connect.
    user_volumes: HashMap<Snowflake, f32>,
    /// Local per-user stream (screen-share sound) playback volumes, kept
    /// apart from their voice and re-applied after each (re)connect.
    stream_volumes: HashMap<Snowflake, f32>,
    /// Reconnects of the call since it was last ready.
    voice_retries: u32,
    /// The call lost its voice server and waits for the gateway to name
    /// the next one; the connection object is kept meanwhile.
    relocating: bool,
    /// Reconnects of the Go Live connection since it was last ready.
    stream_retries: u32,
    /// A Go Live whose capture keeps running while its connection is
    /// replaced; the next stream connection takes it over.
    stream_carry: Option<StreamConnection>,
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
    /// The call's voice connection closed (`code`: the server's close
    /// code, `None` when the connection was simply lost).
    Closed {
        generation: u64,
        code: Option<u16>,
        reason: String,
    },
    /// The call's voice connection finished its handshake.
    VoiceReady {
        generation: u64,
    },
    /// Time to connect the call to its server again.
    RetryVoice {
        generation: u64,
    },
    /// The gateway never named a new voice server for the call.
    RelocateTimeout {
        generation: u64,
    },
    StreamReady {
        generation: u64,
    },
    StreamClosed {
        generation: u64,
        code: Option<u16>,
        reason: String,
    },
    /// Time to connect the Go Live to its server again.
    RetryStream {
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
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) {
    match control {
        VoiceControl::Closed {
            generation,
            code,
            reason,
        } => {
            if generation != voice.generation || voice.connection.is_none() {
                return;
            }
            let action = voice_close_action(code);
            tracing::warn!(
                ?code,
                reason,
                ?action,
                relocating = voice.relocating,
                "the call's voice connection closed"
            );
            if voice.relocating {
                // The gateway is about to name the next server.
                return;
            }
            match action {
                CloseAction::Retry => {
                    if schedule_voice_retry(voice, control_tx) {
                        tell_reconnecting(account, ui).await;
                    } else {
                        give_up_call(
                            voice,
                            account,
                            ui,
                            "the voice server could not be reached again",
                        )
                        .await;
                    }
                }
                CloseAction::AwaitServer => {
                    // An invalid voice session: ask the gateway for a
                    // fresh one by stating the call again. (Not after a
                    // plain disconnect: that may be a kick, and the gateway
                    // says so on its own.)
                    if code == Some(4006) {
                        announce_voice_state(voice);
                    }
                    await_relocation(voice, control_tx);
                    tell_reconnecting(account, ui).await;
                }
                CloseAction::End => {
                    let why = format!(
                        "the voice server ended the call ({} {reason})",
                        code.unwrap_or(0)
                    );
                    give_up_call(voice, account, ui, &why).await;
                }
            }
        }
        VoiceControl::VoiceReady { generation } => {
            if generation != voice.generation {
                return;
            }
            let reconnected = voice.voice_retries > 0;
            voice.voice_retries = 0;
            if let Some(connection) = &voice.connection {
                tracing::info!(channel_id = %connection.channel_id, reconnected, "call connected");
                // A camera carried over from the previous connection.
                if let Some(camera) = &connection.camera {
                    connection.handle.set_video(true);
                    camera.force_keyframe();
                }
            }
            restart_carried_stream(voice, control_tx);
        }
        VoiceControl::RetryVoice { generation } => {
            if generation == voice.generation && voice.connection.is_some() && !voice.relocating {
                reconnect_voice(voice, account, ui, control_tx, "the connection dropped").await;
            }
        }
        VoiceControl::RelocateTimeout { generation } => {
            if generation == voice.generation && voice.relocating && voice.connection.is_some() {
                give_up_call(
                    voice,
                    account,
                    ui,
                    "Discord named no voice server to move the call to",
                )
                .await;
            }
        }
        VoiceControl::RetryStream { generation } => {
            if generation == voice.stream_generation && voice.stream_connection.is_some() {
                reconnect_stream(voice, account, ui, control_tx).await;
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
            // The capture said it failed, or has simply gone quiet.
            let stopped = stream
                && source.is_some_and(|source| {
                    source.failed().is_some()
                        || source
                            .quiet_for()
                            .is_some_and(|quiet| quiet >= CAPTURE_STOPPED_AFTER)
                });
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
            if stopped {
                // Viewers would sit on a loading screen for ever.
                tracing::warn!("the stream's capture stopped producing frames; ending the stream");
                let _ = ui
                    .send(UiEvent::StreamError {
                        account,
                        message: "The screen capture stopped, so the stream was ended. Go Live again to restart it."
                            .to_owned(),
                    })
                    .await;
                stop_stream(voice, account, ui, true).await;
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
                && let Some(connection) = &voice.stream_connection
            {
                voice.stream_retries = 0;
                // A capture carried over from the previous connection:
                // viewers need a full frame to start from.
                if let Some(source) = &connection.source {
                    source.force_keyframe();
                }
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
        VoiceControl::StreamClosed {
            generation,
            code,
            reason,
        } => {
            let Some(connection) = &voice.stream_connection else {
                return;
            };
            if generation != voice.stream_generation {
                return;
            }
            // A running capture is worth keeping when the stream's server
            // merely dropped the connection.
            let capturing = connection.source.is_some();
            let action = voice_close_action(code);
            tracing::warn!(
                ?code,
                reason,
                ?action,
                capturing,
                "the Go Live connection closed"
            );
            if action == CloseAction::Retry && capturing && voice.stream_retries < VOICE_RETRIES {
                let delay = Duration::from_secs(1 << voice.stream_retries);
                voice.stream_retries += 1;
                let control = control_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let _ = control.send(VoiceControl::RetryStream { generation });
                });
                return;
            }
            let message = if reason.is_empty() {
                "The stream's connection closed.".to_owned()
            } else {
                format!("The stream's connection closed: {reason}")
            };
            let _ = ui.send(UiEvent::StreamError { account, message }).await;
            // Deleted on Discord too (harmless if it is gone already), so
            // the next Go Live is answered.
            stop_stream(voice, account, ui, true).await;
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
                    let gain = voice.stream_audio_gain;
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
                                gain,
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
    let session_id = data["session_id"].as_str().map(str::to_owned);
    match channel_id {
        None => {
            // Discord reports the old voice state as `channel_id: null` while
            // a join to a different channel is still pending. Tearing down on
            // that stale event clears pending_join, so the later self state
            // with the new channel never connects and the UI stays on "Join
            // Voice". Only an established connection means a real leave.
            if voice.connection.is_some() {
                // Left from another client, kicked, the channel deleted,
                // or the session that held the call is gone.
                tracing::warn!(?session_id, ours = ?voice.session_id, "Discord says we are no longer in the call");
                teardown_voice(
                    voice,
                    account,
                    ui,
                    true,
                    "Discord removed us from the channel",
                )
                .await;
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
            if let Some(connection) = &mut voice.connection {
                if session_id.is_some() && session_id != voice.session_id {
                    tracing::info!(%channel_id, "the call has a new voice session");
                    voice.session_id = session_id;
                }
                if connection.channel_id != channel_id {
                    // Moved to another channel (by a moderator, or from
                    // another client). The new server details follow.
                    tracing::info!(from = %connection.channel_id, to = %channel_id, "moved to another voice channel");
                    connection.channel_id = channel_id;
                    stop_watching(voice, account, ui, true).await;
                    // A Go Live belongs to the channel it started in.
                    stop_stream(voice, account, ui, true).await;
                    await_relocation(voice, control_tx);
                    let _ = ui
                        .send(UiEvent::VoiceJoined {
                            account,
                            channel_id,
                        })
                        .await;
                }
                return;
            }
            voice.session_id = session_id;
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
    let token = data["token"].as_str().map(str::to_owned);
    let endpoint = data["endpoint"].as_str().map(str::to_owned);
    if voice.connection.is_some() && voice.pending_join.is_none() {
        // The call's voice server changed under it (Discord moves calls
        // between servers), or this answers a rejoin.
        let Some(endpoint) = endpoint else {
            // No server yet: another update names it.
            tracing::warn!("the call's voice server is being reassigned");
            if token.is_some() {
                voice.server_token = token;
            }
            if !voice.relocating {
                await_relocation(voice, control_tx);
            }
            return;
        };
        let changed = Some(&endpoint) != voice.endpoint.as_ref()
            || (token.is_some() && token != voice.server_token);
        tracing::info!(%endpoint, changed, relocating = voice.relocating, "voice server update during the call");
        voice.endpoint = Some(endpoint);
        if token.is_some() {
            voice.server_token = token;
        }
        if changed || voice.relocating {
            voice.voice_retries = 0;
            reconnect_voice(voice, account, ui, control_tx, "the voice server changed").await;
        }
        return;
    }
    if token.is_some() {
        voice.server_token = token;
    }
    if endpoint.is_some() {
        voice.endpoint = endpoint;
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
    let token = data["token"].as_str().map(str::to_owned);
    let endpoint = data["endpoint"].as_str().map(str::to_owned);
    let moved = voice.stream_connection.is_some()
        && endpoint.is_some()
        && (endpoint != voice.stream_endpoint || (token.is_some() && token != voice.stream_token));
    if token.is_some() {
        voice.stream_token = token;
    }
    if endpoint.is_some() {
        voice.stream_endpoint = endpoint;
    }
    if moved {
        // The stream's server changed under it: keep the capture going on
        // a connection to the new one.
        tracing::info!(endpoint = ?voice.stream_endpoint, "Go Live server changed");
        voice.stream_retries = 0;
        reconnect_stream(voice, account, ui, control_tx).await;
        return;
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
    voice.stream_pending = false;
    if !connect_stream(voice, account, ui, control_tx).await {
        voice.stream_pending = true;
    }
}

/// Opens the Go Live connection from what the gateway sent for it. A
/// capture carried over from an earlier connection (`stream_carry`) moves
/// to it; otherwise the screen or camera is opened. Returns false while
/// the gateway's details are still incomplete.
async fn connect_stream(
    voice: &mut VoiceRuntime,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) -> bool {
    let (Some(key), Some(rtc_id), Some(token), Some(endpoint), Some(session_id), Some(user_id)) = (
        voice.stream_key.clone(),
        voice.stream_rtc_id,
        voice.stream_token.clone(),
        voice.stream_endpoint.clone(),
        voice.session_id.clone(),
        voice.self_user_id,
    ) else {
        return false;
    };
    let Some(connection) = &voice.connection else {
        return false;
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
    let carried = voice.stream_carry.take();
    tracing::info!(%rtc_id, %key, endpoint = %params.endpoint, carried = carried.is_some(), "stream connecting");
    voice.stream_generation = voice.stream_generation.wrapping_add(1);
    let generation = voice.stream_generation;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<VoiceEvent>();
    match kc_voice::connect(params, event_tx).await {
        Ok(new_handle) => {
            debug!(%key, %rtc_id, "go live stream connecting");
            // A running capture keeps its handle; it now leads here.
            let (handle, carried) = match carried {
                Some(carried) => {
                    carried.handle.retarget(&new_handle).disconnect_now();
                    (carried.handle.clone(), Some(carried))
                }
                None => (new_handle, None),
            };
            let (source, audio) = carried
                .map(|carried| (carried.source, carried.audio))
                .unwrap_or((None, None));
            let carrying = source.is_some();
            voice.stream_connection = Some(StreamConnection {
                handle: handle.clone(),
                source,
                audio,
                key,
            });

            // Readiness enables the video track and tells the UI.
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
                        // A Closed always follows; it decides what next.
                        VoiceEvent::Error(message) => {
                            tracing::warn!(%message, generation, "stream connection failed");
                        }
                        VoiceEvent::Closed { code, reason } => {
                            let _ = control.send(VoiceControl::StreamClosed {
                                generation,
                                code,
                                reason,
                            });
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
            if carrying {
                return true;
            }
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
                        let control = source_control.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                            if watchdog_frames.load(Ordering::Relaxed) == 0 {
                                let _ = control.send(VoiceControl::StreamSource {
                                    generation,
                                    result: Err("screen capture produced no frames".to_owned()),
                                });
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
            true
        }
        Err(error) => {
            let _ = ui
                .send(UiEvent::StreamError {
                    account,
                    message: format!("screen share connect failed: {error}"),
                })
                .await;
            stop_stream(voice, account, ui, false).await;
            true
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
    let Some(params) = voice_params(voice, guild_id, channel_id) else {
        return;
    };

    tracing::info!(%channel_id, ?guild_id, endpoint = %params.endpoint, "voice connecting from gateway state");
    voice.pending_join = None;
    voice.voice_retries = 0;
    voice.relocating = false;
    voice.generation = voice.generation.wrapping_add(1);
    let generation = voice.generation;

    let (event_tx, event_rx) = mpsc::unbounded_channel::<VoiceEvent>();
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
                }
                for (user_id, volume) in &voice.stream_volumes {
                    audio.set_volume(stream_audio_key(*user_id), *volume);
                }
            }
            if voice.muted || voice.deafened {
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
            spawn_voice_events(
                event_rx, audio, generation, channel_id, account, ui, control_tx,
            );
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

/// What a voice connection to `channel_id` needs from the gateway, once
/// it has all arrived: our session, the voice server and its token.
fn voice_params(
    voice: &VoiceRuntime,
    guild_id: Option<Snowflake>,
    channel_id: Snowflake,
) -> Option<VoiceParams> {
    Some(VoiceParams {
        server_id: guild_id.unwrap_or(channel_id),
        channel_id,
        user_id: voice.self_user_id?,
        session_id: voice.session_id.clone()?,
        token: voice.server_token.clone()?,
        endpoint: voice.endpoint.clone()?,
        video: true,
        screen: false,
        dave_channel_id: channel_id,
        receive_only: false,
    })
}

/// Reads one voice connection's events: sound and video to the speakers
/// and tiles, and its state changes to the account loop.
fn spawn_voice_events(
    mut event_rx: mpsc::UnboundedReceiver<VoiceEvent>,
    audio: Option<Arc<VoiceAudio>>,
    generation: u64,
    channel_id: Snowflake,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) {
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
                    let _ = control.send(VoiceControl::VoiceReady { generation });
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
                // A Closed always follows; it decides what happens next.
                VoiceEvent::Error(message) => {
                    tracing::warn!(%message, generation, "voice connection failed");
                }
                VoiceEvent::Closed { code, reason } => {
                    let _ = control.send(VoiceControl::Closed {
                        generation,
                        code,
                        reason,
                    });
                    break;
                }
                _ => {}
            }
        }
        videos.end_all();
    });
}

/// Moves the call to a new voice connection without leaving it: the
/// server moved, the old connection dropped, or the gateway session was
/// replaced. The microphone, the camera and the playback carry on; every
/// holder of the connection's handle follows it to the new one.
async fn reconnect_voice(
    voice: &mut VoiceRuntime,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
    why: &str,
) {
    let Some((guild_id, channel_id)) = voice
        .connection
        .as_ref()
        .map(|connection| (connection.guild_id, connection.channel_id))
    else {
        return;
    };
    let Some(params) = voice_params(voice, guild_id, channel_id) else {
        tracing::warn!(
            why,
            "cannot reconnect the call: the gateway has not sent the voice server yet"
        );
        return;
    };
    tracing::info!(why, %channel_id, endpoint = %params.endpoint, attempt = voice.voice_retries, "voice reconnecting");
    voice.relocating = false;
    voice.generation = voice.generation.wrapping_add(1);
    let generation = voice.generation;
    let (event_tx, event_rx) = mpsc::unbounded_channel::<VoiceEvent>();
    match kc_voice::connect(params, event_tx).await {
        Ok(new_handle) => {
            let Some(connection) = &mut voice.connection else {
                return;
            };
            let previous = connection.handle.retarget(&new_handle);
            previous.disconnect_now();
            if voice.muted || voice.deafened {
                connection.handle.set_speaking(0);
            }
            spawn_voice_events(
                event_rx,
                connection.audio.clone(),
                generation,
                channel_id,
                account,
                ui,
                control_tx,
            );
        }
        Err(error) => {
            tracing::warn!(%error, "voice reconnect failed to start");
            schedule_voice_retry(voice, control_tx);
        }
    }
}

/// How many times in a row a dropped call is reconnected before it is
/// given up.
const VOICE_RETRIES: u32 = 5;

/// How long a call waits for the gateway to name its new voice server
/// after the old one let it go, or after the gateway session was replaced.
const RELOCATE_WAIT: Duration = Duration::from_secs(15);

/// Schedules the next attempt to reconnect the call to the same server,
/// with a growing pause (1, 2, 4, 8, 16 s). Returns false once the
/// attempts are used up.
fn schedule_voice_retry(
    voice: &mut VoiceRuntime,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) -> bool {
    if voice.voice_retries >= VOICE_RETRIES {
        return false;
    }
    let delay = Duration::from_secs(1 << voice.voice_retries);
    voice.voice_retries += 1;
    let generation = voice.generation;
    let control = control_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = control.send(VoiceControl::RetryVoice { generation });
    });
    true
}

/// Waits for the gateway to say where the call goes next (a new voice
/// server, a new session for us, or that we left).
fn await_relocation(voice: &mut VoiceRuntime, control_tx: &mpsc::UnboundedSender<VoiceControl>) {
    voice.relocating = true;
    let generation = voice.generation;
    let control = control_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(RELOCATE_WAIT).await;
        let _ = control.send(VoiceControl::RelocateTimeout { generation });
    });
}

/// What to do when the voice server closes the call's connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseAction {
    /// Connect to the same server again.
    Retry,
    /// Wait for the gateway to name a server (or to say we left).
    AwaitServer,
    /// The call is over.
    End,
}

fn voice_close_action(code: Option<u16>) -> CloseAction {
    match code {
        // Disconnected (kicked, channel gone, or moved to another server)
        // and call terminated (also sent when the voice server changes):
        // the gateway says which. Session no longer valid: a new gateway
        // session brings a new one.
        Some(4006 | 4014 | 4022) => CloseAction::AwaitServer,
        // Protocol and authentication failures, unknown server, unknown
        // encryption mode, E2EE required, bad request, rate limited:
        // connecting again cannot help.
        Some(4001..=4005 | 4011 | 4012 | 4016 | 4017 | 4020 | 4021) => CloseAction::End,
        // Network loss (no close frame, 1006), a normal or going-away close,
        // session timeout (4009), voice server crashed (4015), anything new.
        _ => CloseAction::Retry,
    }
}

/// Shows that the call is being put back together.
async fn tell_reconnecting(account: AccountId, ui: &async_channel::Sender<UiEvent>) {
    let _ = ui
        .send(UiEvent::VoiceError {
            account,
            message: "Voice connection lost; reconnecting…".to_owned(),
        })
        .await;
}

/// Ends a call that could not be kept: tells Discord we left, so nobody
/// sees us sitting there silent, and says why.
async fn give_up_call(
    voice: &mut VoiceRuntime,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    why: &str,
) {
    let guild_id = voice
        .connection
        .as_ref()
        .and_then(|connection| connection.guild_id);
    voice
        .handle
        .update_voice_state(guild_id, None, voice.muted, voice.deafened, false);
    voice.call_lost = true;
    teardown_voice(voice, account, ui, true, why).await;
    let _ = ui
        .send(UiEvent::VoiceError {
            account,
            message: format!("Disconnected from voice: {why}"),
        })
        .await;
}

/// Connects the Go Live again to the server it had, keeping its capture.
async fn reconnect_stream(
    voice: &mut VoiceRuntime,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) {
    let Some(connection) = voice.stream_connection.take() else {
        return;
    };
    tracing::info!(attempt = voice.stream_retries, "Go Live reconnecting");
    voice.stream_carry = Some(connection);
    if !connect_stream(voice, account, ui, control_tx).await {
        tracing::warn!("Go Live cannot reconnect: its server details are gone");
        stop_stream(voice, account, ui, true).await;
    }
}

/// After the call moved to a new gateway session, Discord has dropped the
/// old session's Go Live. Asks for a new one; the capture carried over
/// goes live on it when it is ready.
fn restart_carried_stream(
    voice: &mut VoiceRuntime,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) {
    if voice.stream_carry.is_none() || voice.stream_pending || voice.stream_connection.is_some() {
        return;
    }
    let Some((guild_id, channel_id)) = voice
        .connection
        .as_ref()
        .map(|connection| (connection.guild_id, connection.channel_id))
    else {
        return;
    };
    tracing::info!(?guild_id, %channel_id, "creating the go live stream again");
    voice.stream_pending = true;
    voice.stream_attempt = voice.stream_attempt.wrapping_add(1);
    give_up_unanswered_stream(voice.stream_attempt, control_tx);
    voice.stream_key = None;
    voice.stream_rtc_id = None;
    voice.stream_token = None;
    voice.stream_endpoint = None;
    voice.handle.stream_create(guild_id, channel_id);
    if let Some(user_id) = voice.self_user_id {
        voice
            .handle
            .stream_pause(&stream_key(guild_id, channel_id, user_id), false);
    }
}

/// The gateway started a new session (READY) while a call was up. The
/// voice state belonged to the old session; state the call again so
/// Discord sends a new session and server for it, and keep everything
/// running until then. A Go Live moves to a stream created anew.
async fn rejoin_after_new_session(
    voice: &mut VoiceRuntime,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    control_tx: &mpsc::UnboundedSender<VoiceControl>,
) {
    let Some(channel_id) = voice
        .connection
        .as_ref()
        .map(|connection| connection.channel_id)
    else {
        return;
    };
    tracing::warn!(%channel_id, "the gateway started a new session during a call; joining it again");
    voice.session_id = None;
    stop_watching(voice, account, ui, false).await;
    match voice.stream_connection.take() {
        Some(stream) if stream.source.is_some() => {
            voice.stream_carry = Some(stream);
            voice.stream_key = None;
            voice.stream_rtc_id = None;
            voice.stream_token = None;
            voice.stream_endpoint = None;
            // Events of the old stream connection no longer apply.
            voice.stream_generation = voice.stream_generation.wrapping_add(1);
        }
        other => {
            voice.stream_connection = other;
            stop_stream(voice, account, ui, true).await;
        }
    }
    announce_voice_state(voice);
    await_relocation(voice, control_tx);
    tell_reconnecting(account, ui).await;
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
                teardown_voice(voice, account, ui, true, "joining another channel").await;
            }
            voice.pending_join = Some((guild_id, channel_id));
            voice.session_id = None;
            voice.server_token = None;
            voice.endpoint = None;
            // Do not announce a camera on join; Discord shows a camera tile
            // for `self_video: true` even before any frames exist, and it
            // fails with 2015 if no track follows. `voice.camera` is only
            // set while a camera runs.
            announce_voice_state(voice);
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
                teardown_voice(voice, account, ui, true, "left").await;
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
            // Others see the muted icon only from the gateway voice state.
            announce_voice_state(voice);
        }
        VoiceCommand::SetUserVolume { user_id, volume } => {
            voice.user_volumes.insert(user_id, volume);
            if let Some(audio) = voice.connection.as_ref().and_then(|c| c.audio.as_ref()) {
                audio.set_volume(user_id.get(), volume);
            }
        }
        VoiceCommand::SetStreamVolume { user_id, volume } => {
            voice.stream_volumes.insert(user_id, volume);
            if let Some(audio) = voice.connection.as_ref().and_then(|c| c.audio.as_ref()) {
                // The same person's shared stream sound rides a separate key.
                audio.set_volume(stream_audio_key(user_id), volume);
            }
        }
        VoiceCommand::SetStreamAudioGain { gain } => {
            // Kept so the next stream starts at it, and applied live to one
            // already running.
            voice.stream_audio_gain = gain;
            if let Some(audio) = voice
                .stream_connection
                .as_ref()
                .and_then(|connection| connection.audio.as_ref())
            {
                audio.set_gain(gain);
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
            announce_voice_state(voice);
        }
        VoiceCommand::SetCameraDevice {
            access,
            node_id,
            settings,
        } => {
            voice.camera = true;
            let (self_mute, self_deaf) = self_flags(voice.muted, voice.deafened);
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
                                self_mute,
                                self_deaf,
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
                                self_mute,
                                self_deaf,
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
            let (self_mute, self_deaf) = self_flags(voice.muted, voice.deafened);
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
                        self_mute,
                        self_deaf,
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

/// The `self_mute` / `self_deaf` pair Discord shows for these choices.
/// Deafening also mutes, as the official client reports it.
fn self_flags(muted: bool, deafened: bool) -> (bool, bool) {
    (muted || deafened, deafened)
}

/// Sends the account's main-gateway voice state (op 4) for the call it is
/// in or joining: the channel, self mute and deafen, and the camera. This
/// is what other people see; the voice server's speaking flags are not.
fn announce_voice_state(voice: &VoiceRuntime) {
    let target = voice
        .connection
        .as_ref()
        .map(|connection| (connection.guild_id, connection.channel_id))
        .or(voice.pending_join);
    let Some((guild_id, channel_id)) = target else {
        return;
    };
    let (self_mute, self_deaf) = self_flags(voice.muted, voice.deafened);
    voice.handle.update_voice_state(
        guild_id,
        Some(channel_id),
        self_mute,
        self_deaf,
        voice.camera,
    );
}

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
    // The stream's playback volume chosen for this person, to set on the
    // mixer entry once its first packet has created it.
    let stream_volume = voice.stream_volumes.get(&owner).copied();
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
                // Apply the saved stream volume once its first packet has
                // created the mixer entry (new entries default to 100%).
                let mut volume_pending = stream_volume;
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
                                let key = stream_audio_key(user_id);
                                audio.push_remote(key, sequence, &opus);
                                if let Some(volume) = volume_pending.take() {
                                    audio.set_volume(key, volume);
                                }
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

/// A stream whose capture has produced nothing for this long is taken to
/// have lost it. A healthy capture resends a still screen every second.
const CAPTURE_STOPPED_AFTER: Duration = Duration::from_secs(10);

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
        || voice.stream_carry.is_some()
        || key.is_some();
    voice.stream_pending = false;
    voice.streaming = false;
    voice.stream_camera = None;
    voice.stream_parent = None;
    voice.stream_capture = None;
    voice.stream_retries = 0;
    for connection in [voice.stream_connection.take(), voice.stream_carry.take()]
        .into_iter()
        .flatten()
    {
        // Disconnect first: stopping the sound share can take a moment.
        connection.handle.disconnect().await;
        drop(connection.source);
        drop(connection.audio);
    }
    voice.stream_key = None;
    voice.stream_rtc_id = None;
    voice.stream_token = None;
    voice.stream_endpoint = None;
    voice.stream_generation = voice.stream_generation.wrapping_add(1);
    ClearedStream { key, had_any }
}

/// Ends the call locally (and any stream or watching with it). `why` is
/// logged, so the log tells how each call ended.
async fn teardown_voice(
    voice: &mut VoiceRuntime,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    notify: bool,
    why: &str,
) {
    let had_connection = voice.connection.is_some() || voice.pending_join.is_some();
    if had_connection {
        tracing::info!(why, streaming = voice.streaming, "call ended");
    }
    voice.relocating = false;
    voice.voice_retries = 0;
    stop_watching(voice, account, ui, true).await;
    stop_stream(voice, account, ui, notify).await;
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
    let lost = std::mem::take(&mut voice.call_lost);
    if notify && had_connection {
        let _ = ui.send(UiEvent::VoiceLeft { account, lost }).await;
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
            stream_audio_gain: 1.0,
            stream_pending: false,
            streaming: false,
            stream_generation: 0,
            stream_attempt: 0,
            watch: None,
            watch_generation: 0,
            user_volumes: HashMap::new(),
            stream_volumes: HashMap::new(),
            voice_retries: 0,
            relocating: false,
            call_lost: false,
            stream_retries: 0,
            stream_carry: None,
        }
    }

    /// What the interface was told, by kind.
    fn told(rx: &async_channel::Receiver<UiEvent>) -> Vec<&'static str> {
        let mut kinds = Vec::new();
        while let Ok(event) = rx.try_recv() {
            kinds.push(match event {
                UiEvent::StreamError { .. } => "error",
                UiEvent::StreamStopped { .. } => "stopped",
                UiEvent::VoiceLeft { .. } => "left",
                UiEvent::VoiceJoined { .. } => "joined",
                UiEvent::VoiceError { .. } => "voice-error",
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
            &mpsc::unbounded_channel().0,
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
            &mpsc::unbounded_channel().0,
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
            &mpsc::unbounded_channel().0,
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
            &mpsc::unbounded_channel().0,
        )
        .await;
        assert_eq!(told(&ui_rx), ["error", "stopped"]);
        assert!(!voice.stream_pending);
    }

    /// The `(channel_id, self_mute, self_deaf, self_video)` of each voice
    /// state (op 4) sent so far.
    fn voice_states_sent(
        rx: &mut mpsc::UnboundedReceiver<serde_json::Value>,
    ) -> Vec<(serde_json::Value, bool, bool, bool)> {
        let mut sent = Vec::new();
        while let Ok(payload) = rx.try_recv() {
            if payload["op"] == 4 {
                let d = &payload["d"];
                sent.push((
                    d["channel_id"].clone(),
                    d["self_mute"].as_bool().unwrap(),
                    d["self_deaf"].as_bool().unwrap(),
                    d["self_video"].as_bool().unwrap(),
                ));
            }
        }
        sent
    }

    #[tokio::test]
    async fn mute_and_deafen_are_announced_to_discord() {
        let (handle, mut sent) = GatewayHandle::detached();
        let (ui_tx, _ui_rx) = async_channel::unbounded();
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        let mut voice = runtime(handle);
        voice.pending_join = None;

        // Not in a call: nothing to tell anyone.
        apply_voice_command(
            &mut voice,
            VoiceCommand::SetMute(true),
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert!(voice_states_sent(&mut sent).is_empty());

        // Joining carries the current choice.
        apply_voice_command(
            &mut voice,
            VoiceCommand::Join {
                guild_id: Some(Snowflake(5)),
                channel_id: Snowflake(12),
            },
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert_eq!(
            voice_states_sent(&mut sent),
            [(json!(Snowflake(12)), true, false, false)]
        );

        // Each toggle while in the call is sent; deafening also mutes.
        let steps = [
            (VoiceCommand::SetMute(false), (false, false)),
            (VoiceCommand::SetDeafen(true), (true, true)),
            (VoiceCommand::SetMute(false), (true, true)),
            (VoiceCommand::SetDeafen(false), (false, false)),
        ];
        for (command, (mute, deaf)) in steps {
            apply_voice_command(&mut voice, command, 0, &ui_tx, &control_tx).await;
            assert_eq!(
                voice_states_sent(&mut sent),
                [(json!(Snowflake(12)), mute, deaf, false)]
            );
        }
    }

    #[test]
    fn own_entry_shows_the_local_choice() {
        let voice = VoiceUiState {
            guild_id: None,
            channel_id: Snowflake(12),
            muted: false,
            deafened: true,
            camera: false,
            streaming: false,
            watching: None,
        };
        assert_eq!(voice.own_flags(Snowflake(12)), Some((true, true)));
        assert_eq!(voice.own_flags(Snowflake(13)), None);
    }

    #[test]
    fn a_users_voice_and_stream_audio_ride_separate_keys() {
        // The mixer keeps a person's voice and their shared stream sound on
        // different keys, so turning one down must not move the other.
        let user = Snowflake(7);
        assert_ne!(user.get(), stream_audio_key(user));
    }

    #[tokio::test]
    async fn voice_and_stream_volumes_are_stored_independently() {
        let (handle, _sent) = GatewayHandle::detached();
        let mut voice = runtime(handle);
        let (ui_tx, _ui_rx) = async_channel::unbounded();
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        let user = Snowflake(7);
        // Setting the voice volume leaves the stream volume at its default.
        apply_voice_command(
            &mut voice,
            VoiceCommand::SetUserVolume {
                user_id: user,
                volume: 0.3,
            },
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert_eq!(voice.user_volumes.get(&user), Some(&0.3));
        assert_eq!(voice.stream_volumes.get(&user), None);
        // Setting the stream volume does not disturb the voice one.
        apply_voice_command(
            &mut voice,
            VoiceCommand::SetStreamVolume {
                user_id: user,
                volume: 1.8,
            },
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert_eq!(voice.user_volumes.get(&user), Some(&0.3));
        assert_eq!(voice.stream_volumes.get(&user), Some(&1.8));
    }

    #[tokio::test]
    async fn stream_audio_gain_is_kept_on_the_runtime() {
        // The outgoing boost is stored even with no stream running, so the
        // next stream starts at it (ScreenAudio::start reads this field).
        let (handle, _sent) = GatewayHandle::detached();
        let mut voice = runtime(handle);
        assert_eq!(voice.stream_audio_gain, 1.0);
        let (ui_tx, _ui_rx) = async_channel::unbounded();
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        apply_voice_command(
            &mut voice,
            VoiceCommand::SetStreamAudioGain { gain: 2.5 },
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert_eq!(voice.stream_audio_gain, 2.5);
    }

    /// A voice runtime in a call in channel 12 of guild 5, its connection
    /// pointed at a server that is not there.
    async fn in_a_call() -> (VoiceRuntime, mpsc::UnboundedReceiver<serde_json::Value>) {
        let (handle, sent) = GatewayHandle::detached();
        let mut voice = runtime(handle);
        voice.pending_join = None;
        voice.session_id = Some("session".to_owned());
        voice.server_token = Some("token".to_owned());
        voice.endpoint = Some("127.0.0.1:9".to_owned());
        let params = voice_params(&voice, Some(Snowflake(5)), Snowflake(12)).unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let handle = kc_voice::connect(params, event_tx).await.unwrap();
        voice.connection = Some(VoiceConnection {
            handle,
            audio: None,
            camera: None,
            guild_id: Some(Snowflake(5)),
            channel_id: Snowflake(12),
        });
        voice.generation = 3;
        (voice, sent)
    }

    #[test]
    fn voice_close_codes_decide_what_follows() {
        for code in [None, Some(1000), Some(1006), Some(4009), Some(4015)] {
            assert_eq!(voice_close_action(code), CloseAction::Retry, "{code:?}");
        }
        for code in [4006, 4014, 4022] {
            assert_eq!(voice_close_action(Some(code)), CloseAction::AwaitServer);
        }
        for code in [4004, 4011, 4016, 4017, 4021] {
            assert_eq!(voice_close_action(Some(code)), CloseAction::End);
        }
    }

    #[tokio::test]
    async fn a_new_voice_server_moves_the_call_without_leaving() {
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        let (mut voice, _sent) = in_a_call().await;
        handle_server_update(
            &mut voice,
            &json!({"guild_id": "5", "token": "new-token", "endpoint": "127.0.0.1:10"}),
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert!(voice.connection.is_some());
        assert_eq!(voice.generation, 4, "a new connection was made");
        assert_eq!(voice.endpoint.as_deref(), Some("127.0.0.1:10"));
        assert_eq!(voice.server_token.as_deref(), Some("new-token"));
        assert!(!told(&ui_rx).contains(&"left"));

        // The same server again changes nothing.
        handle_server_update(
            &mut voice,
            &json!({"guild_id": "5", "token": "new-token", "endpoint": "127.0.0.1:10"}),
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert_eq!(voice.generation, 4);
    }

    #[tokio::test]
    async fn a_dropped_voice_connection_is_retried_then_given_up() {
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        let (mut voice, mut sent) = in_a_call().await;
        let closed = |generation| VoiceControl::Closed {
            generation,
            code: Some(1006),
            reason: String::new(),
        };
        for attempt in 1..=VOICE_RETRIES {
            let current = closed(voice.generation);
            handle_voice_control(&mut voice, current, 0, &ui_tx, &control_tx).await;
            assert_eq!(voice.voice_retries, attempt);
            assert!(voice.connection.is_some());
        }
        assert!(!told(&ui_rx).contains(&"left"));
        // A close from an older connection is not this call's business.
        handle_voice_control(&mut voice, closed(1), 0, &ui_tx, &control_tx).await;
        assert!(voice.connection.is_some());
        // Out of attempts: the call ends, and Discord is told we left.
        let current = closed(voice.generation);
        handle_voice_control(&mut voice, current, 0, &ui_tx, &control_tx).await;
        assert!(voice.connection.is_none());
        assert!(told(&ui_rx).contains(&"left"));
        assert_eq!(
            voice_states_sent(&mut sent)
                .last()
                .map(|state| state.0.clone()),
            Some(serde_json::Value::Null)
        );
    }

    #[tokio::test]
    async fn a_disconnect_waits_for_the_gateway_to_say_where_next() {
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        let (mut voice, _sent) = in_a_call().await;
        let generation = voice.generation;
        handle_voice_control(
            &mut voice,
            VoiceControl::Closed {
                generation,
                code: Some(4014),
                reason: "Disconnected.".to_owned(),
            },
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert!(voice.relocating && voice.connection.is_some());
        // The gateway names the new server: the call moves there.
        handle_server_update(
            &mut voice,
            &json!({"guild_id": "5", "token": "token", "endpoint": "127.0.0.1:9"}),
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert!(!voice.relocating);
        assert_eq!(voice.generation, generation + 1);
        // A timeout armed before the move does nothing.
        handle_voice_control(
            &mut voice,
            VoiceControl::RelocateTimeout { generation },
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert!(voice.connection.is_some());
        assert!(!told(&ui_rx).contains(&"left"));
    }

    #[tokio::test]
    async fn a_fatal_close_or_no_new_server_ends_the_call() {
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        let (mut voice, _sent) = in_a_call().await;
        let generation = voice.generation;
        handle_voice_control(
            &mut voice,
            VoiceControl::Closed {
                generation,
                code: Some(4014),
                reason: String::new(),
            },
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        handle_voice_control(
            &mut voice,
            VoiceControl::RelocateTimeout { generation },
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert!(voice.connection.is_none());
        assert!(told(&ui_rx).contains(&"left"));

        let (mut voice, _sent) = in_a_call().await;
        let generation = voice.generation;
        handle_voice_control(
            &mut voice,
            VoiceControl::Closed {
                generation,
                code: Some(4004),
                reason: "Authentication failed".to_owned(),
            },
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert!(voice.connection.is_none());
        assert!(told(&ui_rx).contains(&"left"));
    }

    #[tokio::test]
    async fn a_new_gateway_session_rejoins_the_call() {
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        let (mut voice, mut sent) = in_a_call().await;
        voice.muted = true;
        rejoin_after_new_session(&mut voice, 0, &ui_tx, &control_tx).await;
        assert!(voice.relocating && voice.connection.is_some());
        assert!(voice.session_id.is_none());
        assert_eq!(
            voice_states_sent(&mut sent),
            [(json!(Snowflake(12)), true, false, false)]
        );
        // Discord answers with the new session's state, then its server.
        handle_voice_state(
            &mut voice,
            &json!({"user_id": "1", "channel_id": "12", "session_id": "fresh"}),
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert_eq!(voice.session_id.as_deref(), Some("fresh"));
        let generation = voice.generation;
        handle_server_update(
            &mut voice,
            &json!({"guild_id": "5", "token": "token", "endpoint": "127.0.0.1:9"}),
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert_eq!(voice.generation, generation + 1);
        assert!(!voice.relocating);
        assert!(!told(&ui_rx).contains(&"left"));
    }

    #[tokio::test]
    async fn being_removed_from_the_channel_ends_the_call() {
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        let (mut voice, _sent) = in_a_call().await;
        handle_voice_state(
            &mut voice,
            &json!({"user_id": "1", "channel_id": null}),
            0,
            &ui_tx,
            &control_tx,
        )
        .await;
        assert!(voice.connection.is_none());
        assert_eq!(told(&ui_rx), ["left"]);
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
