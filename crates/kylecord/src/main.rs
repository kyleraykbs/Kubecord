use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// What the log file records unless `RUST_LOG` says otherwise: enough to
/// see afterwards how a call went (video sent, packets asked for again,
/// keyframe requests, damaged camera frames).
const FILE_FILTER: &str = "kylecord=info,kc_core=info,kc_http=info,kc_gateway=info,kc_voice=info,kc_media=info,kc_ui=info";

/// Opens `kylecord.log` in the cache directory, keeping the previous run's
/// as `kylecord.log.1`.
fn log_file() -> Option<std::fs::File> {
    let dir = kc_core::paths::cache_dir().ok()?;
    kc_core::paths::ensure_dir(&dir).ok()?;
    let path = dir.join("kylecord.log");
    let _ = std::fs::rename(&path, dir.join("kylecord.log.1"));
    let file = std::fs::File::create(&path).ok()?;
    let _ = kc_core::paths::restrict(&path);
    Some(file)
}

fn main() -> glib::ExitCode {
    let from_env = || EnvFilter::try_from_default_env().ok();
    let terminal = fmt::layer().with_filter(
        from_env()
            .unwrap_or_else(|| "kylecord=info,kc_core=info,kc_http=info,kc_gateway=info".into()),
    );
    let file = log_file().map(|file| {
        fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .with_filter(from_env().unwrap_or_else(|| FILE_FILTER.into()))
    });
    tracing_subscriber::registry()
        .with(terminal)
        .with(file)
        .init();

    kc_ui::run()
}
