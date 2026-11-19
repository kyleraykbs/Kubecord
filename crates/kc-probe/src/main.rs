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
        "playback-test" => {
            playback_test().await?;
        }
        "video-test" => {
            // Optional argument: lose every Nth packet (0 for none).
            let lose_every = filtered
                .get(1)
                .and_then(|value| value.parse().ok())
                .unwrap_or(41);
            video_test(lose_every).await?;
        }
        "befriend" => {
            // Sends a friend request to a DM recipient by username, the way
            // the profile's Add Friend button does, and shows Discord's
            // answer.
            let name = filtered.get(1).context("usage: befriend <username>")?;
            let channels = http.get_raw("/users/@me/channels").await?;
            let id = channels
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|channel| channel["recipients"].as_array().into_iter().flatten())
                .find(|user| user["username"].as_str() == Some(name.as_str()))
                .and_then(|user| user["id"].as_str())
                .and_then(|id| id.parse::<u64>().ok())
                .context("no DM recipient with that username")?;
            match http.accept_friend(kc_core::Snowflake(id), None).await {
                Ok(()) => println!("friend request sent"),
                Err(error) => println!("failed: {error} (captcha: {})", error.is_captcha()),
            }
        }
        "raw" => {
            let path = filtered.get(1).context("usage: raw /api/path")?;
            let value = http.get_raw(path).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
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
        "screen" => {
            let portal = kc_media::portal::request(None).await?;
            let (tx, rx) = std::sync::mpsc::channel();
            match kc_media::video::VideoSource::screen(
                portal,
                kc_media::video::VideoSettings::default(),
                move |frame| {
                    let _ = tx.send(frame.data.len());
                },
            ) {
                Ok(_source) => {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                    let mut frames = 0;
                    while std::time::Instant::now() < deadline {
                        if rx
                            .recv_timeout(std::time::Duration::from_millis(500))
                            .is_ok()
                        {
                            frames += 1;
                        }
                    }
                    eprintln!("screen frames in 3s: {frames}");
                }
                Err(error) => eprintln!("screen pipeline failed: {error}"),
            }
        }
        "camera" => {
            let Some(camera) = kc_media::portal::request_camera().await? else {
                bail!("no camera from portal");
            };
            let (tx, rx) = std::sync::mpsc::channel();
            match kc_media::video::VideoSource::camera_portal(
                camera,
                kc_media::video::VideoSettings::default(),
                move |frame| {
                    let _ = tx.send(frame.data.len());
                },
            ) {
                Ok(_source) => {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                    let mut frames = 0;
                    while std::time::Instant::now() < deadline {
                        if rx
                            .recv_timeout(std::time::Duration::from_millis(500))
                            .is_ok()
                        {
                            frames += 1;
                        }
                    }
                    println!("camera frames in 3s: {frames}");
                }
                Err(error) => println!("camera pipeline failed: {error}"),
            }
        }
        "camera-timing" => {
            // Runs a camera through the real encode pipeline and reports how
            // evenly frames come out: `camera-timing [seconds] [name part]
            // [cpu|auto] [kbit/s] [auto|mjpeg|raw] [keyframe seconds, 0 = on
            // demand]`. Local only.
            let seconds: u64 = filtered.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
            let wanted = filtered.get(2).map(|name| name.to_lowercase());
            let Some(access) = kc_media::portal::request_cameras().await? else {
                bail!("no camera from portal");
            };
            for camera in &access.cameras {
                println!("camera: {} (node {})", camera.name, camera.node_id);
            }
            let camera = access
                .cameras
                .iter()
                .find(|camera| {
                    wanted
                        .as_ref()
                        .is_some_and(|wanted| camera.name.to_lowercase().contains(wanted))
                })
                .and_then(|camera| access.select(camera.node_id))
                .or_else(|| access.preferred())
                .context("no camera")?;
            println!("using: {}", camera.name);
            let mut settings = kc_media::video::VideoSettings::default();
            if filtered.get(3).is_some_and(|flag| flag == "cpu") {
                settings.encoder = kc_media::video::EncoderPreference::Cpu;
            }
            if let Some(kbps) = filtered.get(4).and_then(|kbps| kbps.parse().ok()) {
                settings.bitrate_kbps = kbps;
            }
            if let Some(seconds) = filtered.get(6).and_then(|seconds| seconds.parse().ok()) {
                settings.keyframe_secs = seconds;
            }
            settings.camera_format = match filtered.get(5).map(String::as_str) {
                Some("mjpeg") => kc_media::video::CameraFormat::Mjpeg,
                Some("raw") => kc_media::video::CameraFormat::Raw,
                _ => kc_media::video::CameraFormat::Auto,
            };
            println!(
                "settings: {}x{} {} fps, {} kbit/s, {:?}",
                settings.width,
                settings.height,
                settings.fps,
                settings.bitrate_kbps,
                settings.encoder
            );
            let (tx, rx) = std::sync::mpsc::channel();
            let mut dump = std::env::var("KYLECORD_DUMP_FRAMES")
                .ok()
                .and_then(|path| std::fs::File::create(path).ok());
            let source =
                kc_media::video::VideoSource::camera_portal(camera, settings, move |frame| {
                    // `KYLECORD_DUMP_FRAMES=<file>` also writes every encoded
                    // frame, each after its length as four little-endian bytes.
                    if let Some(file) = dump.as_mut() {
                        use std::io::Write;
                        let _ = file.write_all(&(frame.data.len() as u32).to_le_bytes());
                        let _ = file.write_all(&frame.data);
                    }
                    let key = kc_media::video::is_keyframe(&frame.data);
                    let _ = tx.send((std::time::Instant::now(), frame.data.len(), key));
                })?;
            let start = std::time::Instant::now();
            let mut last: Option<std::time::Instant> = None;
            let mut per_second = vec![0u32; seconds as usize + 1];
            let mut bytes_per_second = vec![0usize; seconds as usize + 1];
            // Frames with real content; a repeated picture encodes to ~150 B.
            let mut real_per_second = vec![0u32; seconds as usize + 1];
            let (mut frames, mut keys, mut max_key, mut max_gap) = (0u32, 0u32, 0usize, 0u128);
            while start.elapsed().as_secs() < seconds {
                let Ok((at, size, key)) = rx.recv_timeout(std::time::Duration::from_millis(200))
                else {
                    continue;
                };
                frames += 1;
                let second = at.duration_since(start).as_secs() as usize;
                if let Some(slot) = per_second.get_mut(second) {
                    *slot += 1;
                    bytes_per_second[second] += size;
                    if size > 400 {
                        real_per_second[second] += 1;
                    }
                }
                if key {
                    keys += 1;
                    max_key = max_key.max(size);
                }
                if let Some(last) = last {
                    let gap = at.duration_since(last).as_millis();
                    max_gap = max_gap.max(gap);
                    if gap > 70 {
                        println!(
                            "gap {gap} ms at {:.2}s (frame {size} B, key {key})",
                            at.duration_since(start).as_secs_f64()
                        );
                    }
                }
                last = Some(at);
            }
            drop(source);
            println!(
                "frames {frames}, keyframes {keys}, largest keyframe {max_key} B, max gap {max_gap} ms"
            );
            println!("fps per second: {per_second:?}");
            println!("changed frames per second: {real_per_second:?}");
            println!(
                "kbit per second: {:?}",
                bytes_per_second
                    .iter()
                    .map(|bytes| bytes * 8 / 1000)
                    .collect::<Vec<_>>()
            );
        }
        "monitor" => {
            let (tx, rx) = std::sync::mpsc::channel();
            let _capture = kc_media::audio::Capture::start_monitor(move |frame| {
                let _ = tx.send(frame.len());
            })?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let mut frames = 0;
            while std::time::Instant::now() < deadline {
                if rx
                    .recv_timeout(std::time::Duration::from_millis(250))
                    .is_ok()
                {
                    frames += 1;
                }
            }
            println!("monitor frames in 2s: {frames}");
        }
        "cache" => {
            let guild_id = parse_id(filtered.get(1))?;
            cache(token, guild_id).await?;
        }
        "voice" => {
            let guild_id = parse_id(filtered.get(1))?;
            let channel_id = parse_id(filtered.get(2))?;
            let camera = filtered.iter().any(|arg| arg == "--camera");
            let stream = filtered.iter().any(|arg| arg == "--stream");
            let screen = filtered.iter().any(|arg| arg == "--screen");
            let audio = !filtered.iter().any(|arg| arg == "--no-audio");
            voice(token, guild_id, channel_id, camera, stream, screen, audio).await?;
        }
        "watch-stream" => {
            let guild_id = parse_id(filtered.get(1))?;
            let channel_id = parse_id(filtered.get(2))?;
            let owner = parse_id(filtered.get(3))?;
            let seconds = filtered
                .get(4)
                .and_then(|value| value.parse().ok())
                .unwrap_or(20);
            watch_stream(token, guild_id, channel_id, owner, seconds).await?;
        }
        other => bail!(
            "unknown command {other:?}; try user|guilds|channels|dms|messages|send|watch|voice"
        ),
    }

    Ok(())
}

/// Runs encoded test video through the receive path (packets, loss,
/// retransmission, frame assembly, decoding) without a network.
async fn video_test(lose_every: u32) -> Result<()> {
    use kc_voice::video_rx::{Assembled, Packet, VideoStream};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let frames = kc_media::decode::test_pattern(240, 60)?;
    let keyframes = frames
        .iter()
        .filter(|frame| kc_voice::rtp::is_keyframe(frame))
        .count();
    println!("encoded {} frames ({keyframes} keyframes)", frames.len());

    let feed = Arc::new(kc_media::decode::VideoFeed::default());
    let start = Instant::now();
    let mut stream = VideoStream::new(1, start);
    let mut sequence = 65_000u16;
    let mut sent = 0u32;
    let (mut repaired, mut lost, mut held, mut admitted, mut requested) = (0, 0, 0, 0u64, 0);
    // Packets "retransmitted" a little later: (when, packet).
    let mut late: Vec<(Instant, Packet)> = Vec::new();
    let mut decoder = None;

    for (index, frame) in frames.iter().enumerate() {
        let now = start + Duration::from_millis(index as u64 * 33);
        let mut ready = Vec::new();
        late.retain(|(due, packet)| {
            if *due <= now {
                stream.push(packet.clone(), now, &mut ready);
                false
            } else {
                true
            }
        });
        for (payload, marker) in kc_voice::rtp::packetize(frame, 1200) {
            let packet = Packet {
                sequence,
                timestamp: index as u32 * 3000,
                marker,
                payload,
            };
            sequence = sequence.wrapping_add(1);
            sent += 1;
            // Every Nth packet goes missing; every other one of those is
            // retransmitted 60 ms later, the rest never arrive.
            if lose_every > 0 && sent.is_multiple_of(lose_every) {
                if (sent / lose_every).is_multiple_of(2) {
                    repaired += 1;
                    late.push((now + Duration::from_millis(60), packet));
                }
                continue;
            }
            stream.push(packet, now, &mut ready);
        }
        stream.poll(now, &mut ready);
        let _ = stream.missing(now);
        for item in ready {
            match item {
                Assembled::Lost => lost += 1,
                Assembled::Frame(frame, _) => {
                    if stream.admit(&frame) {
                        admitted += 1;
                        let keyframe = kc_voice::rtp::is_keyframe(&frame);
                        feed.push(frame, keyframe);
                    } else {
                        held += 1;
                    }
                }
            }
        }
        if stream.should_request_keyframe(now, Duration::from_millis(600)) {
            requested += 1;
        }
        // The display attaches late, as it does in the app.
        if index == 20 {
            decoder = Some(kc_media::decode::VideoDecoder::start_headless(
                feed.clone(),
            )?);
        }
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let decoded = decoder
        .as_ref()
        .map(|decoder| decoder.decoded())
        .unwrap_or(0);
    println!(
        "packets sent {sent}, retransmitted {repaired}, losses {lost}, frames held for a keyframe {held}, keyframe requests {requested}"
    );
    println!("frames passed to the decoder {admitted}, decoded {decoded}");
    if decoded != admitted {
        bail!("decoder produced {decoded} of {admitted} frames");
    }
    Ok(())
}

/// Plays three seconds of a synthetic speaker through the real receive path
/// (jitter buffer, decoder, mixer, sound device) and records the output
/// device's monitor to check that it came out unbroken. Arrival times are
/// jittered and some packets are dropped, as on a real network.
///
/// Point it at a null sink to run silently:
/// `KYLECORD_PLAYBACK_DEVICE=<sink> KYLECORD_MONITOR_DEVICE=<sink>.monitor`.
async fn playback_test() -> Result<()> {
    use std::sync::{Arc, Mutex};
    const FRAME: usize = 1920;

    let levels: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = levels.clone();
    let _monitor = kc_media::audio::Capture::start_monitor(move |frame| {
        let energy: f64 = frame.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
        if let Ok(mut levels) = sink.lock() {
            levels.push((energy / frame.len() as f64).sqrt());
        }
    })?;
    let remote = kc_media::receive::RemoteAudio::start()?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let mut encoder = kc_media::audio::OpusEncoder::new()?;
    let total = 150usize;
    let mut dropped = 0;
    let start = tokio::time::Instant::now();
    for index in 0..total {
        let pcm: Vec<i16> = (0..FRAME / 2)
            .flat_map(|sample| {
                let time = (index * FRAME / 2 + sample) as f32 / 48_000.0;
                let value = ((time * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
                [value, value]
            })
            .collect();
        let packet = encoder.encode(&pcm)?;
        // Deterministic jitter of up to +/-15 ms around each 20 ms slot.
        let jitter = ((index * 7919) % 31) as i64 - 15;
        let due = (index as i64 * 20 + jitter).max(0) as u64;
        tokio::time::sleep_until(start + std::time::Duration::from_millis(due)).await;
        if index % 25 == 24 {
            dropped += 1;
            continue;
        }
        remote.push(1, index as u16, &packet);
    }
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;

    let stats = remote.stats();
    let levels = levels
        .lock()
        .map(|levels| levels.clone())
        .unwrap_or_default();
    let voiced: Vec<bool> = levels.iter().map(|level| *level > 200.0).collect();
    let first = voiced.iter().position(|v| *v);
    let last = voiced.iter().rposition(|v| *v);
    println!(
        "sent {} packets ({} dropped on purpose)",
        total - dropped,
        dropped
    );
    println!("receiver: {stats:?}");
    match (first, last) {
        (Some(first), Some(last)) => {
            let span = &voiced[first..=last];
            let gaps = span.iter().filter(|v| !**v).count();
            let mut longest = 0;
            let mut run = 0;
            for v in span {
                run = if *v { 0 } else { run + 1 };
                longest = longest.max(run);
            }
            println!(
                "monitor: tone heard for {} ms of a {} ms span; {} silent frames inside it, longest gap {} ms",
                (span.len() - gaps) * 20,
                span.len() * 20,
                gaps,
                longest * 20
            );
        }
        _ => println!("monitor: no tone heard ({} frames captured)", levels.len()),
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

/// Joins a voice channel muted and without a microphone, watches `owner`'s
/// stream for a while and reports what video arrived and decoded. Cameras
/// in the channel are counted too.
async fn watch_stream(
    token: Token,
    guild_id: Snowflake,
    channel_id: Snowflake,
    owner: Snowflake,
    seconds: u64,
) -> Result<()> {
    use kc_media::decode::{VideoDecoder, VideoFeed};
    use kc_voice::{VoiceEvent, VoiceParams};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// One sender's video: its feed, decoder, keyframes and bytes so far.
    struct Video {
        feed: Arc<VideoFeed>,
        decoder: Option<VideoDecoder>,
        keyframes: u64,
        bytes: usize,
    }
    #[derive(Default)]
    struct Videos(HashMap<Snowflake, Video>);
    impl Videos {
        fn push(&mut self, label: &str, user_id: Snowflake, frame: Vec<u8>, keyframe: bool) {
            let video = self.0.entry(user_id).or_insert_with(|| {
                println!(
                    "{label}: first frame from {user_id} ({} bytes, keyframe={keyframe})",
                    frame.len()
                );
                let feed = Arc::new(VideoFeed::default());
                let decoder = VideoDecoder::start_headless(feed.clone())
                    .map_err(|error| eprintln!("decoder: {error}"))
                    .ok();
                Video {
                    feed,
                    decoder,
                    keyframes: 0,
                    bytes: 0,
                }
            });
            video.keyframes += u64::from(keyframe);
            video.bytes += frame.len();
            video.feed.push(frame, keyframe);
        }

        fn report(&self, label: &str) {
            for (user_id, video) in &self.0 {
                println!(
                    "{label} {user_id}: {} frames ({} keyframes, {} kB), {} decoded",
                    video.feed.pushed(),
                    video.keyframes,
                    video.bytes / 1000,
                    video
                        .decoder
                        .as_ref()
                        .map(|decoder| decoder.decoded())
                        .unwrap_or(0)
                );
            }
        }
    }

    let (gateway, mut events, _task) = Gateway::new(token).start();
    let mut me: Option<Snowflake> = None;
    let mut session_id: Option<String> = None;
    let mut server: Option<(String, String)> = None;
    let key = format!("guild:{guild_id}:{channel_id}:{owner}");

    let (voice_tx, mut voice_events) = mpsc::unbounded_channel();
    let (stream_tx, mut stream_events) = mpsc::unbounded_channel();
    let mut stream_tx = Some(stream_tx);
    let mut voice_handle: Option<kc_voice::VoiceHandle> = None;
    let mut stream_handle: Option<kc_voice::VoiceHandle> = None;
    let mut stream_rtc: Option<Snowflake> = None;
    let mut stream_server: Option<(String, String)> = None;
    let mut cameras = Videos::default();
    let mut streams = Videos::default();
    let mut stream_audio = 0u64;

    let deadline = tokio::time::sleep(std::time::Duration::from_secs(seconds + 15));
    tokio::pin!(deadline);
    let mut report = tokio::time::interval(std::time::Duration::from_secs(4));

    loop {
        tokio::select! {
            _ = &mut deadline => break,
            _ = tokio::signal::ctrl_c() => break,
            _ = report.tick() => {
                cameras.report("camera");
                streams.report("stream");
            }
            event = events.recv() => {
                let Some(event) = event else { bail!("gateway ended") };
                let GatewayEvent::Dispatch { name, data } = event else { continue };
                match name.as_str() {
                    "READY" => {
                        me = data["user"]["id"].as_str().and_then(|id| id.parse().ok());
                        println!("logged in; joining voice channel {channel_id}");
                        gateway.update_voice_state(Some(guild_id), Some(channel_id), true, false, false);
                    }
                    "VOICE_STATE_UPDATE" => {
                        let user_id = data["user_id"].as_str().and_then(|id| id.parse::<Snowflake>().ok());
                        if user_id == me && session_id.is_none() {
                            session_id = data["session_id"].as_str().map(str::to_owned);
                        }
                    }
                    "VOICE_SERVER_UPDATE" => {
                        if let (Some(token), Some(endpoint)) = (data["token"].as_str(), data["endpoint"].as_str()) {
                            server = Some((token.to_owned(), endpoint.to_owned()));
                        }
                    }
                    "STREAM_CREATE" if data["stream_key"].as_str() == Some(key.as_str()) => {
                        stream_rtc = data["rtc_server_id"].as_str().and_then(|id| id.parse().ok());
                        println!("stream create: rtc={stream_rtc:?} viewers={}", data["viewer_ids"]);
                    }
                    "STREAM_SERVER_UPDATE" if data["stream_key"].as_str() == Some(key.as_str()) => {
                        if let (Some(token), Some(endpoint)) = (data["token"].as_str(), data["endpoint"].as_str()) {
                            stream_server = Some((token.to_owned(), endpoint.to_owned()));
                        }
                    }
                    "STREAM_DELETE" if data["stream_key"].as_str() == Some(key.as_str()) => {
                        println!("stream delete: reason={}", data["reason"]);
                    }
                    _ => {}
                }
                if voice_handle.is_none()
                    && let (Some(user_id), Some(session), Some((token, endpoint))) = (me, session_id.clone(), server.clone())
                {
                    let params = VoiceParams {
                        server_id: guild_id,
                        channel_id,
                        user_id,
                        session_id: session,
                        token,
                        endpoint,
                        video: true,
                        screen: false,
                        dave_channel_id: channel_id,
                        receive_only: false,
                    };
                    voice_handle = Some(kc_voice::connect(params, voice_tx.clone()).await?);
                }
                if stream_handle.is_none()
                    && let (Some(user_id), Some(session), Some(rtc_id), Some((token, endpoint))) =
                        (me, session_id.clone(), stream_rtc, stream_server.clone())
                    && let Some(events) = stream_tx.take()
                {
                    let params = VoiceParams {
                        server_id: rtc_id,
                        channel_id,
                        user_id,
                        session_id: session,
                        token,
                        endpoint,
                        video: true,
                        screen: true,
                        dave_channel_id: Snowflake::new(rtc_id.get().saturating_sub(1)),
                        receive_only: true,
                    };
                    println!("connecting to the stream");
                    stream_handle = Some(kc_voice::connect(params, events).await?);
                }
            }
            event = voice_events.recv() => match event {
                Some(VoiceEvent::Ready { ssrc, mode, dave_version, .. }) => {
                    println!("voice ready: ssrc={ssrc} mode={mode} dave={dave_version}; asking to watch {key}");
                    gateway.stream_watch(&key);
                }
                Some(VoiceEvent::Video { user_id, frame, keyframe, .. }) => {
                    cameras.push("camera", user_id, frame, keyframe);
                }
                Some(VoiceEvent::VideoEnded { user_id }) => println!("camera ended: {user_id}"),
                Some(VoiceEvent::Error(error)) => bail!("voice error: {error}"),
                Some(VoiceEvent::Closed { code, reason }) => bail!("voice closed: {code:?} {reason}"),
                Some(_) => {}
                None => bail!("voice ended"),
            },
            event = stream_events.recv() => match event {
                Some(VoiceEvent::Ready { ssrc, mode, dave_version, .. }) => {
                    println!("stream ready: ssrc={ssrc} mode={mode} dave={dave_version}");
                }
                Some(VoiceEvent::DaveReady) => println!("stream DAVE ready"),
                Some(VoiceEvent::Video { user_id, frame, keyframe, .. }) => {
                    streams.push("stream", user_id, frame, keyframe);
                }
                Some(VoiceEvent::VideoEnded { user_id }) => println!("stream video ended: {user_id}"),
                Some(VoiceEvent::Audio { .. }) => stream_audio += 1,
                Some(VoiceEvent::Error(error)) => {
                    println!("stream error: {error}");
                    break;
                }
                Some(VoiceEvent::Closed { code, reason }) => {
                    println!("stream closed: {code:?} {reason}");
                    break;
                }
                Some(_) => {}
                None => {}
            },
        }
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    cameras.report("camera");
    streams.report("stream");
    println!("stream audio packets: {stream_audio}");
    if let Some(handle) = stream_handle {
        handle.disconnect().await;
        gateway.stream_delete(&key);
    }
    if let Some(handle) = voice_handle {
        handle.disconnect().await;
    }
    gateway.update_voice_state(Some(guild_id), None, false, false, false);
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    Ok(())
}

async fn voice(
    token: Token,
    guild_id: Snowflake,
    channel_id: Snowflake,
    camera: bool,
    stream: bool,
    screen: bool,
    with_audio: bool,
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
        receive_only: false,
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
                        if with_audio {
                            let on_speaking = |speaking: bool| {
                                println!("microphone {}", if speaking { "speaking" } else { "quiet" });
                            };
                            match kc_media::audio::VoiceAudio::start(handle.clone(), on_speaking) {
                                Ok(engine) => audio = Some(engine),
                                Err(error) => eprintln!("audio unavailable: {error}"),
                            }
                        }
                        if stream {
                            println!("creating go-live stream");
                            gateway.stream_create(Some(guild_id), channel_id);
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
                    Some(VoiceEvent::Audio { user_id, opus, sequence, .. }) => {
                        if let Some(audio) = &audio {
                            audio.push_remote(user_id.get(), sequence, &opus);
                        }
                    }
                    Some(VoiceEvent::Video { user_id, frame, keyframe, .. }) => {
                        println!("video from {user_id}: {} bytes keyframe={keyframe}", frame.len());
                    }
                    Some(VoiceEvent::Speaking { user_id, speaking, .. }) => {
                        println!("{user_id} {}", if speaking { "speaking" } else { "stopped" });
                    }
                    Some(VoiceEvent::VideoEnded { user_id }) => println!("video ended: {user_id}"),
                    Some(VoiceEvent::DaveReady) => println!("DAVE ready"),
                    Some(VoiceEvent::ForceKeyframe) => println!("force keyframe requested"),
                    Some(VoiceEvent::Ping { millis }) => println!("ping: {millis:?} ms"),
                    Some(VoiceEvent::VideoNetwork { sent, lost, remb_kbps }) => {
                        println!("video network: sent {sent} lost {lost} remb {remb_kbps} kbit/s");
                    }
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
                                receive_only: false,
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
                                        match kc_media::portal::request(None).await {
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

async fn cache(token: Token, guild_id: Snowflake) -> Result<()> {
    let store = Store::new();
    let (gateway, mut events, _task) = Gateway::new(token).start();
    let _ = gateway;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(12);
    let mut channels_seen_at: Option<tokio::time::Instant> = None;
    let mut subscribed = false;
    loop {
        if !store.read(|state| state.guild_channels(guild_id).is_empty())
            && channels_seen_at.is_none()
        {
            // Keep consuming for a moment after READY so READY_SUPPLEMENTAL
            // voice states and member sync arrive before printing.
            channels_seen_at = Some(tokio::time::Instant::now());
        }
        if !subscribed && channels_seen_at.is_some() {
            let voice_channel = store.read(|state| {
                state
                    .guild_channels(guild_id)
                    .into_iter()
                    .find(|channel| channel.kind.is_voice() && !channel.kind.is_text())
                    .map(|channel| channel.id)
            });
            if let Some(channel_id) = voice_channel {
                gateway.subscribe_channel(guild_id, channel_id);
                subscribed = true;
            }
        }
        if channels_seen_at.is_some_and(|at| at.elapsed() > std::time::Duration::from_secs(3)) {
            break;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(GatewayEvent::Dispatch { name, data })) => {
                store.apply(&name, &data);
                if name == "READY" || name == "GUILD_CREATE" {
                    let (guilds, channels) = store.read(|state| {
                        (
                            state.ordered_guilds().len(),
                            state.guild_channels(guild_id).len(),
                        )
                    });
                    eprintln!("{name}: guilds={guilds} target_channels={channels}");
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }

    store.read(|state| {
        let guild = state.guild(guild_id);
        println!(
            "guild: {} ({})",
            guild.map(|guild| guild.name.as_str()).unwrap_or("?"),
            guild_id
        );
        let channels = state.guild_channels(guild_id);
        println!("channels: {}", channels.len());
        for channel in channels {
            println!(
                "  {:<24} {} {:?}",
                channel.display_name(),
                channel.id,
                channel.kind
            );
        }
        for channel in state.guild_channels(guild_id) {
            if channel.kind.is_voice() && !channel.kind.is_text() {
                let users = state.users_in_channel_voice(guild_id, channel.id);
                if !users.is_empty() {
                    println!(
                        "voice {}: {}",
                        channel.display_name(),
                        users
                            .iter()
                            .map(|user| user.display_name())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }
    });
    Ok(())
}
