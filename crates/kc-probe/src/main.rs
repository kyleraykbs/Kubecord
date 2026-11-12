use std::str::FromStr;

use anyhow::{Context, Result, bail};
use kc_core::{Snowflake, Token};
use kc_gateway::{Gateway, GatewayEvent};
use kc_http::HttpClient;
use kc_state::Store;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kc_probe=info,kc_gateway=info,kc_http=info".into()),
        )
        .with_target(false)
        .init();

    let mut args: Vec<String> = std::env::args().skip(1).collect();

    let token = match std::env::var("KYLECORD_TOKEN") {
        Ok(token) if !token.trim().is_empty() => Token::new(token),
        _ => {
            let index = args
                .iter()
                .position(|arg| arg == "--token")
                .context("set KYLECORD_TOKEN or pass --token <token>")?;
            if index + 1 >= args.len() {
                bail!("--token needs a value");
            }
            let value = args.remove(index + 1);
            args.remove(index);
            Token::new(value)
        }
    };

    let command = args.first().cloned().unwrap_or_default();
    let filtered = args;
    let command = command.as_str();

    let http = HttpClient::new(token.clone());

    match command {
        "user" | "login" => {
            let user = http.current_user().await?;
            println!("{} ({}) id={}", user.display_name(), user.username, user.id);
        }
        "guilds" => {
            let guilds = http.current_user_guilds().await?;
            for guild in guilds {
                println!("{}  {}", guild.id, guild.name);
            }
        }
        "channels" => {
            let guild_id = parse_id(filtered.get(1))?;
            let mut channels = http.guild_channels(guild_id).await?;
            channels.sort_by_key(|channel| channel.sort_key());
            for channel in channels {
                if channel.kind.is_category() {
                    println!("[{}] {}", channel.id, channel.display_name());
                } else {
                    println!(
                        "  {:<20} {}  {}",
                        channel.display_name(),
                        channel.id,
                        match channel.kind {
                            kc_state::ChannelType::GuildText => "text",
                            kc_state::ChannelType::GuildVoice => "voice",
                            kc_state::ChannelType::GuildAnnouncement => "announcement",
                            other => {
                                tracing::debug!(?other, "channel");
                                "other"
                            }
                        }
                    );
                }
            }
        }
        "dms" => {
            // DM channels only arrive over the gateway; this command watches
            // READY and prints them.
            watch(token, true).await?;
        }
        "messages" => {
            let channel_id = parse_id(filtered.get(1))?;
            let limit = filtered
                .get(2)
                .and_then(|value| value.parse::<u8>().ok())
                .unwrap_or(50);
            let mut messages = http.messages(channel_id, limit, None).await?;
            messages.reverse();
            for message in messages {
                let edited = if message.edited_timestamp.is_some() {
                    " (edited)"
                } else {
                    ""
                };
                println!(
                    "[{}] {}{}: {}",
                    message.timestamp,
                    message.author.display_name(),
                    edited,
                    message.content
                );
            }
        }
        "send" => {
            let channel_id = parse_id(filtered.get(1))?;
            let content = filtered.get(2).context("send <channel> <text>")?;
            let message = http.send_message(channel_id, content, None).await?;
            println!("sent {}", message.id);
        }
        "watch" => {
            watch(token, false).await?;
        }
        "voice" => {
            let guild_id = parse_id(filtered.get(1))?;
            let channel_id = parse_id(filtered.get(2))?;
            let camera = filtered.iter().any(|arg| arg == "--camera");
            let stream = filtered.iter().any(|arg| arg == "--stream");
            let screen = filtered.iter().any(|arg| arg == "--screen");
            voice(token, guild_id, channel_id, camera, stream, screen).await?;
        }
        other => bail!(
            "unknown command {other:?}; try user|guilds|channels|dms|messages|send|watch|voice"
        ),
    }

    Ok(())
}

fn parse_id(value: Option<&String>) -> Result<Snowflake> {
    let value = value.context("missing id argument")?;
    Snowflake::from_str(value).map_err(Into::into)
}

async fn watch(token: Token, dms_only: bool) -> Result<()> {
    let store = Store::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let gateway = Gateway::new(token);
    tokio::spawn(gateway.run(tx));

    let mut changes = store.subscribe();

    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    GatewayEvent::Dispatch { name, data } => {
                        store.apply(&name, &data);
                        if name == "READY" && dms_only {
                            store.read(|state| {
                                for channel in state.dm_channels() {
                                    println!("{}  {}", channel.id, channel.display_name());
                                }
                            });
                        }
                        if name == "MESSAGE_CREATE" {
                            let author = data.pointer("/author/username").and_then(|value| value.as_str()).unwrap_or("?");
                            let content = data["content"].as_str().unwrap_or("");
                            let channel = data["channel_id"].as_str().unwrap_or("?");
                            println!("[{channel}] {author}: {content}");
                        }
                    }
                    GatewayEvent::Hello { heartbeat_interval } => {
                        eprintln!("gateway ready, heartbeat {heartbeat_interval} ms");
                    }
                    GatewayEvent::Resumed => eprintln!("resumed"),
                    GatewayEvent::InvalidSession { resumable } => eprintln!("invalid session (resumable={resumable})"),
                    GatewayEvent::Reconnect => eprintln!("reconnect requested"),
                    GatewayEvent::Closed { code, reason } => {
                        eprintln!("gateway closed: {code:?} {reason}");
                        break;
                    }
                }
            }
            change = changes.recv() => {
                if let Ok(change) = change {
                    tracing::trace!(?change, "store change");
                }
            }
        }
    }
    Ok(())
}

async fn voice(
    token: Token,
    guild_id: Snowflake,
    channel_id: Snowflake,
    camera: bool,
    stream: bool,
    screen: bool,
) -> Result<()> {
    use kc_voice::{VoiceEvent, VoiceParams};

    let (gateway, mut events, _task) = Gateway::new(token).start();

    let mut me: Option<Snowflake> = None;
    let mut session_id: Option<String> = None;
    let mut server_token: Option<String> = None;
    let mut endpoint: Option<String> = None;

    while me.is_none() {
        match events.recv().await {
            Some(GatewayEvent::Dispatch { name, data }) if name == "READY" => {
                me = data["user"]["id"].as_str().and_then(|id| id.parse().ok());
                println!(
                    "logged in as {}",
                    data["user"]["username"].as_str().unwrap_or("?")
                );
            }
            Some(GatewayEvent::Closed { code, reason }) => {
                bail!("gateway closed: {code:?} {reason}");
            }
            Some(_) => {}
            None => bail!("gateway ended before READY"),
        }
    }

    gateway.update_voice_state(Some(guild_id), Some(channel_id), false, false, true);
    println!("joining voice channel {channel_id}");

    while session_id.is_none() || server_token.is_none() || endpoint.is_none() {
        match events.recv().await {
            Some(GatewayEvent::Dispatch { name, data }) => match name.as_str() {
                "VOICE_STATE_UPDATE" => {
                    let user_id = data["user_id"]
                        .as_str()
                        .and_then(|id| id.parse::<Snowflake>().ok());
                    if user_id == me {
                        session_id = data["session_id"].as_str().map(str::to_owned);
                    }
                }
                "VOICE_SERVER_UPDATE" => {
                    server_token = data["token"].as_str().map(str::to_owned);
                    endpoint = data["endpoint"].as_str().map(str::to_owned);
                }
                _ => {}
            },
            Some(GatewayEvent::Closed { code, reason }) => {
                bail!("gateway closed while joining: {code:?} {reason}");
            }
            Some(_) => {}
            None => bail!("gateway ended while joining"),
        }
    }

    let session_for_stream = session_id.clone();
    let params = VoiceParams {
        server_id: guild_id,
        channel_id,
        user_id: me.unwrap(),
        session_id: session_id.unwrap(),
        token: server_token.unwrap(),
        endpoint: endpoint.unwrap(),
        video: true,
        screen: false,
        dave_channel_id: channel_id,
    };

    let (voice_tx, mut voice_events) = mpsc::unbounded_channel();
    let handle = kc_voice::connect(params, voice_tx).await?;
    let mut audio = None;
    let mut camera_source = None;
    let mut stream_handle: Option<kc_voice::VoiceHandle> = None;
    let mut stream_source = None;
    let mut stream_rtc_id: Option<Snowflake> = None;
    let mut stream_server: Option<(String, String)> = None;
    let mut stream_started = false;
    let session_id_for_stream = session_for_stream;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("leaving");
                gateway.update_voice_state(Some(guild_id), None, false, false, false);
                let _ = handle;
                break;
            }
            event = voice_events.recv() => {
                match event {
                    Some(VoiceEvent::Ready { ssrc, video_ssrc, rtx_ssrc, mode, dave_version }) => {
                        println!("voice ready: ssrc={ssrc} video={video_ssrc:?} rtx={rtx_ssrc:?} mode={mode} dave={dave_version}");
                        match kc_media::audio::VoiceAudio::start(handle.clone()) {
                            Ok(engine) => audio = Some(engine),
                            Err(error) => eprintln!("audio unavailable: {error}"),
                        }
                        if stream {
                            println!("creating go-live stream");
                            gateway.stream_create(guild_id, channel_id);
                        } else if camera {
                            let settings = kc_media::video::VideoSettings::default();
                            match kc_media::video::VideoSource::camera(None, settings, {
                                let handle = handle.clone();
                                move |frame| handle.send_video(frame.data, frame.duration_ms)
                            }) {
                                Ok(source) => {
                                    camera_source = Some(source);
                                    handle.set_video(true);
                                    println!("camera streaming");
                                }
                                Err(error) => eprintln!("camera unavailable: {error}"),
                            }
                        }
                    }
                    Some(VoiceEvent::Audio { user_id, opus, timestamp, .. }) => {
                        if let Some(audio) = &audio {
                            audio.push_remote(user_id.get(), &opus, timestamp);
                        }
                    }
                    Some(VoiceEvent::Video { user_id, frame, keyframe, .. }) => {
                        println!("video from {user_id}: {} bytes keyframe={keyframe}", frame.len());
                    }
                    Some(VoiceEvent::Speaking { user_id, speaking, .. }) => {
                        if speaking { println!("{user_id} speaking"); }
                    }
                    Some(VoiceEvent::DaveReady) => println!("DAVE ready"),
                    Some(VoiceEvent::KeyframeRequest { user_id, .. }) => {
                        println!("keyframe requested by {user_id}");
                    }
                    Some(VoiceEvent::Error(error)) => eprintln!("voice error: {error}"),
                    Some(VoiceEvent::Closed { code, reason }) => {
                        println!("voice closed: {code:?} {reason}");
                        break;
                    }
                    Some(VoiceEvent::ClientConnect { user_id }) => println!("{user_id} connected"),
                    Some(VoiceEvent::ClientDisconnect { user_id }) => println!("{user_id} disconnected"),
                    None => break,
                }
            }
            gateway_event = events.recv() => {
                match gateway_event {
                    Some(GatewayEvent::Dispatch { name, data }) => {
                        match name.as_str() {
                            "STREAM_CREATE" => {
                                let key = data["stream_key"].as_str().unwrap_or_default();
                                if stream_key_owner(key) == me {
                                    stream_rtc_id = data["rtc_server_id"].as_str().and_then(|id| id.parse().ok());
                                    println!("stream created: {key}");
                                }
                            }
                            "STREAM_SERVER_UPDATE" => {
                                if let (Some(token), Some(endpoint)) =
                                    (data["token"].as_str(), data["endpoint"].as_str())
                                {
                                    stream_server = Some((token.to_owned(), endpoint.to_owned()));
                                }
                            }
                            _ => {}
                        }

                        if stream
                            && !stream_started
                            && let (Some(rtc_id), Some((stream_token, stream_endpoint))) =
                                (stream_rtc_id, stream_server.clone())
                            && let Some(session_id) = session_id_for_stream.clone()
                        {
                            stream_started = true;
                            let params = VoiceParams {
                                server_id: rtc_id,
                                channel_id,
                                user_id: me.unwrap(),
                                session_id,
                                token: stream_token,
                                endpoint: stream_endpoint,
                                video: true,
                                screen: true,
                                dave_channel_id: Snowflake::new(rtc_id.get().saturating_sub(1)),
                            };
                            let (stream_tx, mut stream_events) = mpsc::unbounded_channel();
                            match kc_voice::connect(params, stream_tx).await {
                                Ok(stream_connection) => {
                                    let cap_handle = stream_connection.clone();
                                    stream_handle = Some(stream_connection);
                                    tokio::spawn(async move {
                                        while let Some(event) = stream_events.recv().await {
                                            match event {
                                                VoiceEvent::Ready { ssrc, video_ssrc, .. } => {
                                                    println!("stream ready: ssrc={ssrc} video={video_ssrc:?}");
                                                }
                                                VoiceEvent::Error(error) => eprintln!("stream error: {error}"),
                                                VoiceEvent::Closed { code, reason } => {
                                                    println!("stream closed: {code:?} {reason}");
                                                    break;
                                                }
                                                _ => {}
                                            }
                                        }
                                    });

                                    let settings = kc_media::video::VideoSettings::default();
                                    let source = if screen {
                                        match kc_media::portal::request().await {
                                            Ok(portal) => kc_media::video::VideoSource::screen(
                                                portal,
                                                settings,
                                                move |frame| cap_handle.send_video(frame.data, frame.duration_ms),
                                            ),
                                            Err(error) => Err(error),
                                        }
                                    } else {
                                        kc_media::video::VideoSource::camera(
                                            None,
                                            settings,
                                            move |frame| cap_handle.send_video(frame.data, frame.duration_ms),
                                        )
                                    };
                                    match source {
                                        Ok(source) => {
                                            stream_source = Some(source);
                                            println!("stream source started");
                                        }
                                        Err(error) => eprintln!("stream source unavailable: {error}"),
                                    }
                                }
                                Err(error) => eprintln!("stream connection failed: {error}"),
                            }
                        }
                    }
                    Some(GatewayEvent::Closed { code, reason }) => {
                        eprintln!("gateway closed: {code:?} {reason}");
                        break;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }

    drop(stream_source);
    drop(camera_source);
    drop(stream_handle);
    drop(audio);
    Ok(())
}

fn stream_key_owner(key: &str) -> Option<Snowflake> {
    key.rsplit(':').next().and_then(|id| id.parse().ok())
}
