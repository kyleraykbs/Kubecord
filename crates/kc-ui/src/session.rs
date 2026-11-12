//! Per-account session state and the Tokio side of the UI bridge.
//!
//! All Discord I/O lives behind [`Command`]s that the GTK thread sends over an
//! `async_channel`; the Tokio dispatcher and per-account tasks answer over a
//! second `async_channel` as [`UiEvent`]s. No GTK type crosses this boundary.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use kc_core::{Account, Snowflake, Token};
use kc_gateway::session::GatewayHandle;
use kc_gateway::{Gateway, GatewayEvent};
use kc_http::HttpClient;
use kc_media::audio::VoiceAudio;
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
            connection: ConnState::Connecting,
            voice: None,
            account,
        }
    }
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
                    if let Some(VoiceControl::Closed { generation }) = control
                        && generation == voice.generation && voice.connection.is_some() {
                            teardown_voice(&mut voice, account, &ui, true).await;
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
}

struct VoiceConnection {
    handle: VoiceHandle,
    audio: Option<Arc<VoiceAudio>>,
    camera: Option<VideoSource>,
    guild_id: Option<Snowflake>,
}

/// Signals from a voice connection's consumer task back to the account loop.
enum VoiceControl {
    Closed { generation: u64 },
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
            if voice.connection.is_some() || voice.pending_join.is_some() {
                teardown_voice(voice, account, ui, true).await;
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

            let mut connection = VoiceConnection {
                handle: handle.clone(),
                audio: audio.clone(),
                camera: None,
                guild_id,
            };
            if voice.camera
                && let Err(error) = start_camera(&mut connection)
            {
                voice.camera = false;
                let _ = ui
                    .send(UiEvent::VoiceError {
                        account,
                        message: format!("camera: {error}"),
                    })
                    .await;
            }
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
            let _ = ui
                .send(UiEvent::VoiceError {
                    account,
                    message: format!("voice connect failed: {error}"),
                })
                .await;
        }
    }
}

fn start_camera(connection: &mut VoiceConnection) -> Result<(), String> {
    let handle = connection.handle.clone();
    let source = VideoSource::camera(None, VideoSettings::default(), move |frame| {
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
            voice
                .handle
                .update_voice_state(guild_id, Some(channel_id), false, false, true);
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
        VoiceCommand::SetCamera(enabled) => {
            voice.camera = enabled;
            match &mut voice.connection {
                Some(connection) => {
                    if enabled {
                        if connection.camera.is_none() {
                            if let Err(error) = start_camera(connection) {
                                voice.camera = false;
                                let _ = ui
                                    .send(UiEvent::VoiceError {
                                        account,
                                        message: format!("camera: {error}"),
                                    })
                                    .await;
                            }
                        } else {
                            connection.handle.set_video(true);
                        }
                    } else {
                        connection.camera = None;
                        connection.handle.set_video(false);
                    }
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
    }
}

async fn teardown_voice(
    voice: &mut VoiceRuntime,
    account: AccountId,
    ui: &async_channel::Sender<UiEvent>,
    notify: bool,
) {
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
