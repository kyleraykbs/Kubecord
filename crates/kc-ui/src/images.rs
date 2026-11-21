//! Asynchronous image loading for avatars, icons, emoji and media previews.
//!
//! Downloads and decoding run on the Tokio runtime; finished textures are
//! handed back to the GTK main loop. Images are cached in memory for the
//! session and on disk under the XDG cache directory.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gtk::gdk;
use gtk::prelude::*;
use libadwaita as adw;

/// Textures kept in memory before the least recently loaded are released.
const MEMORY_ENTRIES: usize = 1500;
/// Disk cache entries untouched for this long are deleted at startup.
const DISK_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// A URL that failed is not requested again for this long. Media posted a
/// moment ago is sometimes not yet available from the proxy.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// How many media file downloads run at once. Requests beyond this queue and
/// start in request order, so the GIFs currently in view (built first) load
/// first instead of a whole grid racing and starving each other. Roughly the
/// number of tiles visible in the picker at a time.
const MAX_CONCURRENT_FILES: usize = 6;

type Callback = Box<dyn FnOnce(&gdk::Texture)>;
type FileCallback = Box<dyn FnOnce(Option<PathBuf>)>;

struct Inner {
    runtime: Arc<tokio::runtime::Runtime>,
    directory: Option<PathBuf>,
    memory: RefCell<HashMap<String, gdk::Texture>>,
    order: RefCell<VecDeque<String>>,
    waiting: RefCell<HashMap<String, Vec<Callback>>>,
    failed: RefCell<HashMap<String, Instant>>,
    /// Downloads to a file (for video playback) in flight, by URL.
    files_waiting: RefCell<HashMap<String, Vec<FileCallback>>>,
    /// File downloads not yet started, in request order (url, extension), and
    /// how many are currently running, so no more than [`MAX_CONCURRENT_FILES`]
    /// run at once and the earliest-requested start first.
    file_queue: RefCell<VecDeque<(String, String)>>,
    file_active: Cell<usize>,
    file_results: async_channel::Sender<(String, Option<PathBuf>)>,
    results: async_channel::Sender<(String, Option<gdk::Texture>)>,
}

/// Shared image cache. Cheap to clone; lives on the GTK main thread.
#[derive(Clone)]
pub struct ImageCache {
    inner: Rc<Inner>,
}

impl ImageCache {
    pub fn new(runtime: Arc<tokio::runtime::Runtime>) -> Self {
        let directory = kc_core::paths::cache_dir()
            .ok()
            .map(|dir| dir.join("images"))
            .filter(|dir| std::fs::create_dir_all(dir).is_ok());
        if let Some(directory) = directory.clone() {
            runtime.spawn_blocking(move || prune(&directory));
        }

        let (results, receiver) = async_channel::unbounded::<(String, Option<gdk::Texture>)>();
        let (file_results, file_receiver) = async_channel::unbounded::<(String, Option<PathBuf>)>();
        let inner = Rc::new(Inner {
            runtime,
            directory,
            memory: RefCell::new(HashMap::new()),
            order: RefCell::new(VecDeque::new()),
            waiting: RefCell::new(HashMap::new()),
            failed: RefCell::new(HashMap::new()),
            results,
            files_waiting: RefCell::new(HashMap::new()),
            file_queue: RefCell::new(VecDeque::new()),
            file_active: Cell::new(0),
            file_results,
        });

        let weak = Rc::downgrade(&inner);
        glib::MainContext::default().spawn_local(async move {
            while let Ok((url, path)) = file_receiver.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                let callbacks = inner
                    .files_waiting
                    .borrow_mut()
                    .remove(&url)
                    .unwrap_or_default();
                for callback in callbacks {
                    callback(path.clone());
                }
                // Free the slot this download held and start the next queued one.
                inner
                    .file_active
                    .set(inner.file_active.get().saturating_sub(1));
                inner.pump_files();
            }
        });

        let weak = Rc::downgrade(&inner);
        glib::MainContext::default().spawn_local(async move {
            while let Ok((url, texture)) = receiver.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                let callbacks = inner.waiting.borrow_mut().remove(&url).unwrap_or_default();
                match texture {
                    Some(texture) => {
                        inner.remember(&url, &texture);
                        for callback in callbacks {
                            callback(&texture);
                        }
                    }
                    None => {
                        inner.failed.borrow_mut().insert(url, Instant::now());
                    }
                }
            }
        });

        Self { inner }
    }

    /// Calls `callback` with the image at `url`, immediately when cached.
    /// A URL that failed is retried only after [`RETRY_AFTER`].
    pub fn load(&self, url: &str, callback: impl FnOnce(&gdk::Texture) + 'static) {
        if let Some(texture) = self.inner.memory.borrow().get(url) {
            callback(texture);
            return;
        }
        if self
            .inner
            .failed
            .borrow()
            .get(url)
            .is_some_and(|failed| failed.elapsed() < RETRY_AFTER)
        {
            return;
        }
        let first = {
            let mut waiting = self.inner.waiting.borrow_mut();
            let entry = waiting.entry(url.to_owned()).or_default();
            entry.push(Box::new(callback));
            entry.len() == 1
        };
        if !first {
            return;
        }

        let url = url.to_owned();
        let path = self
            .inner
            .directory
            .as_ref()
            .map(|dir| dir.join(cache_name(&url)));
        let results = self.inner.results.clone();
        self.inner.runtime.spawn(async move {
            let texture = fetch_texture(&url, path).await;
            let _ = results.send((url, texture)).await;
        });
    }

    /// Downloads `url` into the disk cache and calls `done` with the local
    /// path, or `None` when the download failed. Used for media that is
    /// played rather than decoded (GIF-style videos).
    pub fn file(&self, url: &str, extension: &str, done: impl FnOnce(Option<PathBuf>) + 'static) {
        if let Some(local) = local_path(url) {
            done(local.exists().then_some(local));
            return;
        }
        if self.inner.directory.is_none() {
            done(None);
            return;
        }
        let first = {
            let mut waiting = self.inner.files_waiting.borrow_mut();
            let entry = waiting.entry(url.to_owned()).or_default();
            entry.push(Box::new(done));
            entry.len() == 1
        };
        // A second request for the same URL just waits on the first.
        if !first {
            return;
        }
        // Queue in request order and start it if a slot is free.
        self.inner
            .file_queue
            .borrow_mut()
            .push_back((url.to_owned(), extension.to_owned()));
        self.inner.pump_files();
    }

    /// A round avatar that shows initials until the image arrives.
    pub fn avatar(&self, size: i32, name: &str, url: Option<&str>) -> adw::Avatar {
        let avatar = adw::Avatar::new(size, Some(name), true);
        if let Some(url) = url {
            self.set_avatar(&avatar, url);
        }
        avatar
    }

    pub fn set_avatar(&self, avatar: &adw::Avatar, url: &str) {
        let weak = avatar.downgrade();
        self.load(url, move |texture| {
            if let Some(avatar) = weak.upgrade() {
                avatar.set_custom_image(Some(texture));
            }
        });
    }

    /// Fills `picture` once the image is available.
    pub fn set_picture(&self, picture: &gtk::Picture, url: &str) {
        let weak = picture.downgrade();
        self.load(url, move |texture| {
            if let Some(picture) = weak.upgrade() {
                picture.set_paintable(Some(texture));
            }
        });
    }

    /// Fills `picture` with the image at `url`, animating it if the bytes are
    /// an animated GIF (a `gtk::Picture` driven frame by frame; see
    /// `animation`), otherwise showing a still texture. The player is owned by
    /// the picture, so it stops and frees its frames when the picture is gone.
    /// Use for media that may be a real animated GIF (attachments, picker
    /// previews, the viewer); `set_picture` stays the still-only path.
    pub(crate) fn set_animated_picture(&self, picture: &gtk::Picture, url: &str) {
        let weak = picture.downgrade();
        let debug_url = url.to_owned();
        // Download to the disk cache under a `.gif` name (reuses dedup and the
        // on-disk cache), then decode the bytes on the main thread.
        self.file(url, "gif", move |path| {
            let Some(picture) = weak.upgrade() else {
                return;
            };
            let Some(path) = path else {
                tracing::debug!(url = %debug_url, "animated picture: download failed");
                return;
            };
            let Ok(bytes) = std::fs::read(&path) else {
                tracing::debug!(url = %debug_url, "animated picture: read failed");
                return;
            };
            let gif = crate::animation::is_gif(&bytes);
            let animation = gif.then(|| crate::animation::decode(&bytes)).flatten();
            tracing::debug!(
                url = %debug_url,
                bytes = bytes.len(),
                gif,
                animated = animation.is_some(),
                "animated picture decoded"
            );
            if let Some(animation) = animation {
                // Multi-frame GIF: the player drives the picture and holds
                // itself alive for the picture's lifetime (see `attach`). A
                // single frame returns None, already painted as a still.
                crate::animation::GifPlayer::attach(&picture, &animation);
                return;
            }
            // Not an animated GIF: show it as a plain texture.
            if let Some(texture) = decode_blocking(bytes) {
                picture.set_paintable(Some(&texture));
            }
        });
    }

    /// Fills `image` (icon-sized, e.g. an inline emoji) once available.
    pub fn set_image(&self, image: &gtk::Image, url: &str) {
        let weak = image.downgrade();
        self.load(url, move |texture| {
            if let Some(image) = weak.upgrade() {
                image.set_paintable(Some(texture));
            }
        });
    }
}

impl Inner {
    /// Starts queued file downloads up to the concurrency limit, in the order
    /// they were requested.
    fn pump_files(&self) {
        let Some(directory) = self.directory.clone() else {
            return;
        };
        while self.file_active.get() < MAX_CONCURRENT_FILES {
            let Some((url, extension)) = self.file_queue.borrow_mut().pop_front() else {
                break;
            };
            self.file_active.set(self.file_active.get() + 1);
            let path = directory.join(format!("{}.{extension}", cache_name(&url)));
            let results = self.file_results.clone();
            self.runtime.spawn(async move {
                let ready = if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                    Some(path)
                } else {
                    match kc_http::cdn::fetch(&url).await {
                        // Write under a temporary name so a half-written file is
                        // never mistaken for a complete one.
                        Ok(bytes) => {
                            let partial = path.with_extension("part");
                            let written = tokio::fs::write(&partial, &bytes).await.is_ok()
                                && tokio::fs::rename(&partial, &path).await.is_ok();
                            written.then_some(path)
                        }
                        Err(error) => {
                            tracing::debug!(%error, url, "media download failed");
                            None
                        }
                    }
                };
                let _ = results.send((url, ready)).await;
            });
        }
    }

    fn remember(&self, url: &str, texture: &gdk::Texture) {
        let mut memory = self.memory.borrow_mut();
        let mut order = self.order.borrow_mut();
        if memory.insert(url.to_owned(), texture.clone()).is_none() {
            order.push_back(url.to_owned());
        }
        while order.len() > MEMORY_ENTRIES {
            if let Some(old) = order.pop_front() {
                memory.remove(&old);
            }
        }
    }
}

async fn fetch_texture(url: &str, path: Option<PathBuf>) -> Option<gdk::Texture> {
    // A copy kept on disk (the message log's media): read it in place.
    if let Some(local) = local_path(url) {
        return decode(tokio::fs::read(local).await.ok()?).await;
    }
    if let Some(path) = &path
        && let Ok(bytes) = tokio::fs::read(path).await
    {
        if let Some(texture) = decode(bytes).await {
            return Some(texture);
        }
        // A cached file that no longer decodes (truncated, or written by an
        // older build that kept error pages) is replaced by a fresh copy.
        let _ = tokio::fs::remove_file(path).await;
    }
    let bytes = match kc_http::cdn::fetch(url).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::debug!(%error, url, "image download failed");
            return None;
        }
    };
    let texture = decode(bytes.clone()).await;
    match (&texture, &path) {
        (Some(_), Some(path)) => {
            let _ = tokio::fs::write(path, &bytes).await;
        }
        (None, _) => tracing::debug!(url, "image could not be decoded"),
        _ => {}
    }
    texture
}

/// Decodes on the blocking pool: a large image takes long enough to stall
/// the runtime's workers.
async fn decode(bytes: Vec<u8>) -> Option<gdk::Texture> {
    tokio::task::spawn_blocking(move || decode_blocking(bytes))
        .await
        .ok()
        .flatten()
}

/// GDK reads PNG, JPEG and TIFF itself and everything else through
/// GdkPixbuf, whose WebP loader is a separate package most systems lack.
/// Discord and its GIF providers serve WebP for previews and video posters,
/// so WebP is decoded here instead.
fn decode_blocking(bytes: Vec<u8>) -> Option<gdk::Texture> {
    if is_webp(&bytes) {
        return decode_webp(&bytes);
    }
    gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok()
}

fn is_webp(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
}

/// The first frame of a still or animated WebP.
fn decode_webp(bytes: &[u8]) -> Option<gdk::Texture> {
    let mut decoder = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes)).ok()?;
    let (width, height) = decoder.dimensions();
    let mut pixels = vec![0; decoder.output_buffer_size()?];
    decoder.read_image(&mut pixels).ok()?;
    let (format, channels) = if decoder.has_alpha() {
        (gdk::MemoryFormat::R8g8b8a8, 4)
    } else {
        (gdk::MemoryFormat::R8g8b8, 3)
    };
    let texture = gdk::MemoryTexture::new(
        i32::try_from(width).ok()?,
        i32::try_from(height).ok()?,
        format,
        &glib::Bytes::from_owned(pixels),
        width as usize * channels,
    );
    Some(texture.upcast())
}

/// Stable file name for a URL (FNV-1a, so it survives toolchain upgrades).
/// The file a `file://` URL names (any query a resize added is dropped).
pub(crate) fn local_path(url: &str) -> Option<PathBuf> {
    let path = url.strip_prefix("file://")?;
    Some(PathBuf::from(path.split('?').next().unwrap_or(path)))
}

fn cache_name(url: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in url.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn prune(directory: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > DISK_MAX_AGE);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes a small RGBA image so the test does not depend on a fixture.
    fn sample_webp() -> Vec<u8> {
        let pixels: Vec<u8> = [
            [255, 0, 0, 255],
            [0, 255, 0, 128],
            [0, 0, 255, 255],
            [9, 9, 9, 0],
        ]
        .concat();
        let mut out = Vec::new();
        image_webp::WebPEncoder::new(&mut out)
            .encode(&pixels, 2, 2, image_webp::ColorType::Rgba8)
            .expect("encode");
        out
    }

    #[test]
    fn webp_is_recognised_and_decoded() {
        let webp = sample_webp();
        assert!(is_webp(&webp));
        assert!(!is_webp(b"\x89PNG\r\n\x1a\n...."));
        let texture = decode_webp(&webp).expect("webp decodes");
        assert_eq!((texture.width(), texture.height()), (2, 2));
        assert!(decode_webp(b"RIFF\0\0\0\0WEBPgarbage").is_none());
    }

    #[test]
    fn cache_names_are_stable_and_distinct() {
        assert_eq!(cache_name("a"), cache_name("a"));
        assert_ne!(cache_name("a"), cache_name("b"));
        assert_eq!(cache_name("a").len(), 16);
    }
}
