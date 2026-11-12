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
        other => bail!("unknown command {other:?}; try user|guilds|channels|dms|messages|send|watch"),
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