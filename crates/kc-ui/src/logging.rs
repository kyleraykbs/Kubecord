//! Logging: to the terminal, and to a log file that can be switched off or
//! made more detailed while the app runs (Settings → Debug).
//!
//! The file is `kylecord.log` in the cache directory. Each launch starts a
//! new one and keeps the previous as `kylecord.log.1`. It records what is
//! needed to see afterwards how a call went; a busy hour of calls is a few
//! hundred kilobytes. Detailed logging adds every media event and is far
//! larger, so the file stops growing at [`MAX_BYTES`].

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tracing::{Level, Metadata};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, filter, fmt};

/// The file stops growing here, whatever is being logged.
pub const MAX_BYTES: u64 = 20 * 1024 * 1024;

static FILE_ENABLED: AtomicBool = AtomicBool::new(true);
static DETAILED: AtomicBool = AtomicBool::new(false);
static DIAGNOSTICS: AtomicBool = AtomicBool::new(false);
static WRITTEN: AtomicU64 = AtomicU64::new(0);
static FILE: Mutex<Option<File>> = Mutex::new(None);

/// Whether to log performance diagnostics: the main-thread watchdogs and the
/// sidebar/member rebuild timings. Off by default; turned on in Advanced to
/// investigate a hitch. Checked before the warning is built, so it costs
/// nothing when off.
pub fn diagnostics() -> bool {
    DIAGNOSTICS.load(Ordering::Relaxed)
}

/// Turns the performance diagnostics on or off.
pub fn set_diagnostics(on: bool) {
    DIAGNOSTICS.store(on, Ordering::Relaxed);
}

/// Where the log file is.
pub fn path() -> Option<PathBuf> {
    Some(kc_core::paths::cache_dir().ok()?.join("kylecord.log"))
}

/// Bytes written to the log file since launch.
pub fn written() -> u64 {
    WRITTEN.load(Ordering::Relaxed)
}

/// Switches the log file on or off, and its detail up or down, from now on.
pub fn configure(to_file: bool, detailed: bool) {
    FILE_ENABLED.store(to_file, Ordering::Relaxed);
    if DETAILED.swap(detailed, Ordering::Relaxed) != detailed {
        tracing::callsite::rebuild_interest_cache();
    }
}

/// What the file records: the app's own messages at info and above, with
/// its media and voice code at debug when detailed; other libraries'
/// warnings.
fn file_wants(metadata: &Metadata<'_>) -> bool {
    if !FILE_ENABLED.load(Ordering::Relaxed) {
        return false;
    }
    let target = metadata.target();
    let ours = target.starts_with("kc_") || target.starts_with("kylecord");
    let level = *metadata.level();
    if !ours {
        return level <= Level::WARN;
    }
    if level <= Level::INFO {
        return true;
    }
    level == Level::DEBUG
        && DETAILED.load(Ordering::Relaxed)
        && (target.starts_with("kc_voice")
            || target.starts_with("kc_media")
            || target.starts_with("kc_ui::session"))
}

/// Writes to the log file, up to its size limit.
struct FileWriter;

impl Write for FileWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if FILE_ENABLED.load(Ordering::Relaxed)
            && WRITTEN.load(Ordering::Relaxed) < MAX_BYTES
            && let Ok(mut file) = FILE.lock()
            && let Some(file) = file.as_mut()
        {
            let _ = file.write_all(bytes);
            let total =
                WRITTEN.fetch_add(bytes.len() as u64, Ordering::Relaxed) + bytes.len() as u64;
            if total >= MAX_BYTES {
                let _ =
                    file.write_all(b"log size limit reached; nothing more is written this run\n");
            }
        }
        // Always "written": logging must never fail the caller.
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for FileWriter {
    type Writer = FileWriter;

    fn make_writer(&'a self) -> Self::Writer {
        FileWriter
    }
}

fn open_file() -> Option<File> {
    let path = path()?;
    let dir = path.parent()?;
    kc_core::paths::ensure_dir(dir).ok()?;
    let _ = std::fs::rename(&path, dir.join("kylecord.log.1"));
    let file = File::create(&path).ok()?;
    let _ = kc_core::paths::restrict(&path);
    Some(file)
}

/// Sets logging up for the process. `RUST_LOG`, when set, decides what both
/// the terminal and the file record instead.
pub fn init() {
    let prefs = crate::settings::CapturePreferences::load();
    FILE_ENABLED.store(prefs.log_to_file, Ordering::Relaxed);
    DETAILED.store(prefs.log_detailed, Ordering::Relaxed);
    // The file is opened even when logging to it is off, so it can be
    // switched on without a restart; an unused one stays empty.
    if let Ok(mut file) = FILE.lock() {
        *file = open_file();
    }

    let from_env = || EnvFilter::try_from_default_env().ok();
    let terminal = fmt::layer().with_filter(
        from_env()
            .unwrap_or_else(|| "kylecord=info,kc_core=info,kc_http=info,kc_gateway=info".into()),
    );
    let file = fmt::layer().with_ansi(false).with_writer(FileWriter);
    match from_env() {
        Some(filter) => tracing_subscriber::registry()
            .with(terminal)
            .with(file.with_filter(filter))
            .init(),
        None => tracing_subscriber::registry()
            .with(terminal)
            .with(file.with_filter(filter::dynamic_filter_fn(|metadata, _| {
                file_wants(metadata)
            })))
            .init(),
    }
}
