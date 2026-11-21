//! Discord's emoji art (Twemoji) in place of the system emoji font.
//!
//! Discord's web app serves each emoji as its own SVG, under a hashed name.
//! Its scripts map an emoji's codepoints (`"./1f680.svg"`) to a module, and
//! the module to the file (`"/assets/2a419df364f6817c.svg"`). That map is read
//! once from discord.com's public web app (no account details are sent) and
//! cached; each emoji's SVG is fetched on first use, cached on disk, and drawn
//! at the size it is shown. Nothing is shipped, so the Flatpak and Nix builds
//! behave alike.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime};

use gtk::gdk;
use gtk::prelude::*;

use crate::shell::AppState;

const WEB_APP: &str = "https://discord.com/app";
/// Scripts read at most while looking for the map.
const MAX_SCRIPTS: usize = 40;
/// A cached map older than this is looked up again (a failed lookup keeps
/// using it).
const MAP_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Emoji downloads at once, on their own queue so a picker full of emoji
/// never holds up other media.
const MAX_DOWNLOADS: usize = 12;

static NAMES: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#""\./([0-9a-f-]+)\.svg"\s*:\s*"?(\d+)"?"#).unwrap());
static FILES: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?:^|[,{])\s*(\d+)\s*\(\s*\w+\s*\)\s*\{\s*\w+\.exports\s*=\s*"(/assets/[0-9a-f]+\.svg)""#,
    )
    .unwrap()
});

/// What to do once an emoji's SVG is on disk (given its path).
type Waiter = Box<dyn FnOnce(&std::path::Path)>;

thread_local! {
    /// The setting.
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    /// Where downloads run (the app's Tokio runtime).
    static RUNTIME: RefCell<Option<tokio::runtime::Handle>> = const { RefCell::new(None) };
    /// Emoji key (Twemoji's codepoint name) -> asset path, once known.
    static MAP: RefCell<Option<Rc<HashMap<String, String>>>> = const { RefCell::new(None) };
    /// Bumped whenever emoji start or stop being drawn in Discord's art, so
    /// what was built for the old style can tell.
    static GENERATION: Cell<u32> = const { Cell::new(0) };
    /// A lookup of the map is under way.
    static DISCOVERING: Cell<bool> = const { Cell::new(false) };
    /// Drawn emoji, by asset path and pixel size.
    static TEXTURES: RefCell<HashMap<(String, i32), gdk::Texture>> = RefCell::new(HashMap::new());
    /// Pictures waiting for an SVG download, by asset path.
    static WAITING: RefCell<HashMap<String, Vec<Waiter>>> = RefCell::new(HashMap::new());
}

static DOWNLOADS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(MAX_DOWNLOADS)));

/// Changes whenever emoji start or stop being drawn in Discord's art.
pub(crate) fn generation() -> u32 {
    GENERATION.get()
}

/// Whether Discord's emoji are drawn right now (chosen, and the map known).
pub(crate) fn active() -> bool {
    ENABLED.get() && MAP.with_borrow(Option::is_some)
}

/// Twemoji's name for an emoji: its codepoints in lowercase hex joined by
/// `-`, with U+FE0F dropped unless the emoji is a joined (ZWJ) sequence.
pub(crate) fn key(emoji: &str) -> String {
    let joined = emoji.contains('\u{200d}');
    emoji
        .chars()
        .filter(|c| joined || *c != '\u{fe0f}')
        .map(|c| format!("{:x}", c as u32))
        .collect::<Vec<_>>()
        .join("-")
}

fn asset_for(emoji: &str) -> Option<String> {
    MAP.with_borrow(|map| {
        let map = map.as_ref()?;
        map.get(&key(emoji)).cloned().or_else(|| {
            // Some sequences are named with all their selectors kept.
            let raw: Vec<String> = emoji.chars().map(|c| format!("{:x}", c as u32)).collect();
            map.get(&raw.join("-")).cloned()
        })
    })
}

/// Characters below the supplementary planes that are emoji by default
/// (Emoji_Presentation); any other there (©, ™, ↔, digits...) is text unless
/// followed by U+FE0F, as Discord shows it.
fn emoji_by_default(c: char) -> bool {
    matches!(c as u32,
        0x231a..=0x231b | 0x23e9..=0x23ec | 0x23f0 | 0x23f3 | 0x25fd..=0x25fe | 0x2614..=0x2615
        | 0x2648..=0x2653 | 0x267f | 0x2693 | 0x26a1 | 0x26aa..=0x26ab | 0x26bd..=0x26be
        | 0x26c4..=0x26c5 | 0x26ce | 0x26d4 | 0x26ea | 0x26f2..=0x26f3 | 0x26f5 | 0x26fa | 0x26fd
        | 0x2705 | 0x270a..=0x270b | 0x2728 | 0x274c | 0x274e | 0x2753..=0x2755 | 0x2757
        | 0x2795..=0x2797 | 0x27b0 | 0x27bf | 0x2b1b..=0x2b1c | 0x2b50 | 0x2b55)
        || c as u32 >= 0x1f000
}

/// A run of text, or one emoji Discord has art for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Segment {
    Text(String),
    Emoji(String),
}

/// Splits `text` into plain runs and emoji Discord draws, using `known` to
/// tell which sequences have art (the longest match wins).
fn split_with(text: &str, known: &dyn Fn(&str) -> bool) -> Vec<Segment> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut plain_start = 0;
    let mut i = 0;
    while i < chars.len() {
        let (start, first) = chars[i];
        let could_start = !first.is_ascii() || matches!(first, '#' | '*' | '0'..='9');
        let mut matched = None;
        if could_start {
            let longest = (chars.len() - i).min(12);
            for len in (1..=longest).rev() {
                let end = chars.get(i + len).map_or(text.len(), |(index, _)| *index);
                let candidate = &text[start..end];
                // `#`, `*` and digits are emoji only as keycaps; other
                // text-default symbols (©, ™, ↔...) need a following
                // selector or joiner.
                let text_digit = first.is_ascii() && !candidate.contains('\u{20e3}');
                let text_symbol = len == 1 && !emoji_by_default(first);
                if !text_digit && !text_symbol && known(candidate) {
                    matched = Some((len, end));
                    break;
                }
            }
        }
        match matched {
            Some((len, end)) => {
                if plain_start < start {
                    out.push(Segment::Text(text[plain_start..start].to_owned()));
                }
                out.push(Segment::Emoji(text[start..end].to_owned()));
                plain_start = end;
                i += len;
            }
            None => i += 1,
        }
    }
    if plain_start < text.len() {
        out.push(Segment::Text(text[plain_start..].to_owned()));
    }
    out
}

/// [`split_with`] against Discord's map. Empty when Discord's emoji are not
/// active.
pub(crate) fn split(text: &str) -> Vec<Segment> {
    if !active() {
        return vec![Segment::Text(text.to_owned())];
    }
    split_with(text, &|candidate| asset_for(candidate).is_some())
}

/// Whether `text` holds an emoji Discord would draw (and they are active).
pub(crate) fn contains_emoji(text: &str) -> bool {
    active()
        && split(text)
            .iter()
            .any(|segment| matches!(segment, Segment::Emoji(_)))
}

/// A picture of `emoji` in Discord's art at `size` pixels, filled once its
/// SVG is in; `None` when Discord's emoji are off or it has no art.
pub(crate) fn image(emoji: &str, size: i32) -> Option<gtk::Image> {
    if !ENABLED.get() {
        return None;
    }
    let asset = asset_for(emoji)?;
    let image = gtk::Image::new();
    image.set_pixel_size(size);
    image.set_tooltip_text(Some(emoji));
    image.set_valign(gtk::Align::Center);
    // Drawn at twice the size so it stays sharp on HiDPI screens.
    let pixels = size * 2;
    if let Some(texture) =
        TEXTURES.with_borrow(|cache| cache.get(&(asset.clone(), pixels)).cloned())
    {
        image.set_paintable(Some(&texture));
        return Some(image);
    }
    let weak = image.downgrade();
    let drawn = asset.clone();
    fetch(
        asset,
        Box::new(move |path| {
            if let (Some(image), Some(texture)) = (weak.upgrade(), draw(&drawn, path, pixels)) {
                image.set_paintable(Some(&texture));
            }
        }),
    );
    Some(image)
}

/// An inline picture of `emoji` for a text buffer, `size` pixels square, if
/// its SVG is already on disk. Otherwise `None`, and when it has art the
/// download starts and `ready` runs once it is in.
pub(crate) fn paintable(
    emoji: &str,
    size: i32,
    ready: impl FnOnce() + 'static,
) -> Option<EmojiPaintable> {
    if !active() {
        return None;
    }
    let asset = asset_for(emoji)?;
    let pixels = size * 2;
    let path = emoji_dir()?.join(asset.trim_start_matches("/assets/"));
    let cached = TEXTURES.with_borrow(|cache| cache.get(&(asset.clone(), pixels)).cloned());
    let texture = cached.or_else(|| path.exists().then(|| draw(&asset, &path, pixels)).flatten());
    match texture {
        Some(texture) => Some(EmojiPaintable::new(emoji, &texture, size)),
        None => {
            fetch(asset, Box::new(move |_| ready()));
            None
        }
    }
}

/// Downloads an emoji's SVG to the cache (once, however many ask), then
/// calls `done` with its path. Failed downloads call nothing.
fn fetch(asset: String, done: Waiter) {
    let first = WAITING.with_borrow_mut(|waiting| {
        let entry = waiting.entry(asset.clone()).or_default();
        entry.push(done);
        entry.len() == 1
    });
    if !first {
        return;
    }
    let (Some(dir), Some(runtime)) = (emoji_dir(), RUNTIME.with_borrow(Clone::clone)) else {
        WAITING.with_borrow_mut(|waiting| waiting.remove(&asset));
        return;
    };
    let url = format!("https://discord.com{asset}");
    let path = dir.join(asset.trim_start_matches("/assets/"));
    let semaphore = DOWNLOADS.clone();
    let download = runtime.spawn(async move {
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Some(path);
        }
        let _permit = semaphore.acquire_owned().await.ok()?;
        let bytes = kc_http::cdn::fetch(&url)
            .await
            .inspect_err(|error| tracing::debug!(%error, url, "could not get an emoji"))
            .ok()?;
        tokio::fs::create_dir_all(path.parent()?).await.ok()?;
        let partial = path.with_extension("part");
        tokio::fs::write(&partial, &bytes).await.ok()?;
        tokio::fs::rename(&partial, &path).await.ok()?;
        Some(path)
    });
    glib::spawn_future_local(async move {
        let path = download.await.ok().flatten();
        let waiters = WAITING
            .with_borrow_mut(|waiting| waiting.remove(&asset))
            .unwrap_or_default();
        if let Some(path) = path {
            for done in waiters {
                done(&path);
            }
        }
    });
}

/// Draws an emoji's SVG at `pixels`, remembering it.
fn draw(asset: &str, path: &std::path::Path, pixels: i32) -> Option<gdk::Texture> {
    let key = (asset.to_owned(), pixels);
    if let Some(texture) = TEXTURES.with_borrow(|cache| cache.get(&key).cloned()) {
        return Some(texture);
    }
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, pixels, pixels, true)
        .inspect_err(|error| tracing::debug!(%error, "could not draw an emoji"))
        .ok()?;
    let texture = crate::animation::pixbuf_texture(&pixbuf)?;
    TEXTURES.with_borrow_mut(|cache| cache.insert(key, texture.clone()));
    Some(texture)
}

fn emoji_dir() -> Option<PathBuf> {
    kc_core::paths::cache_dir()
        .ok()
        .map(|dir| dir.join("emoji"))
}

fn map_path() -> Option<PathBuf> {
    emoji_dir().map(|dir| dir.join("map.json"))
}

/// The cached map, and whether it is fresh.
fn cached_map() -> Option<(HashMap<String, String>, bool)> {
    let path = map_path()?;
    let fresh = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < MAP_MAX_AGE);
    let map = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    Some((map, fresh))
}

/// The map from Discord's scripts.
pub(crate) fn read_map(scripts: &[String]) -> HashMap<String, String> {
    let mut names: HashMap<String, String> = HashMap::new();
    let mut files: HashMap<String, String> = HashMap::new();
    for script in scripts {
        for capture in NAMES.captures_iter(script) {
            names.insert(capture[1].to_owned(), capture[2].to_owned());
        }
        for capture in FILES.captures_iter(script) {
            files.insert(capture[1].to_owned(), capture[2].to_owned());
        }
    }
    names
        .into_iter()
        .filter_map(|(name, module)| Some((name, files.get(&module)?.clone())))
        .collect()
}

/// Looks the map up in Discord's web app: the main script holds the names,
/// the emoji vendor script the files.
async fn discover() -> Result<HashMap<String, String>, String> {
    let html = kc_http::cdn::fetch(WEB_APP)
        .await
        .map_err(|error| format!("could not load Discord's web app: {error}"))?;
    let html = String::from_utf8_lossy(&html);
    let mut urls = crate::sounds::script_urls(&html);
    // The emoji vendor script right after the main one.
    urls.sort_by_key(|url| (!url.contains("/assets/web."), !url.contains("vnd-emoji")));
    let mut scripts = Vec::new();
    for url in urls.into_iter().take(MAX_SCRIPTS) {
        if let Ok(bytes) = kc_http::cdn::fetch(&url).await {
            scripts.push(String::from_utf8_lossy(&bytes).into_owned());
        }
        let map = read_map(&scripts);
        if map.len() > 1000 {
            return Ok(map);
        }
    }
    Err("Discord's web app no longer lists its emoji where expected".to_owned())
}

impl AppState {
    /// Draws emoji with Discord's art, or goes back to the system's. The map
    /// comes from the cache, else from Discord's web app (once a week).
    pub(crate) fn apply_discord_emoji(self: &Rc<Self>, on: bool) {
        RUNTIME.with_borrow_mut(|slot| *slot = Some(self.runtime.handle().clone()));
        let was = active();
        ENABLED.set(on);
        if on && MAP.with_borrow(Option::is_none) {
            if let Some((map, fresh)) = cached_map() {
                MAP.with_borrow_mut(|slot| *slot = Some(Rc::new(map)));
                if !fresh {
                    self.refresh_emoji_map();
                }
            } else {
                self.refresh_emoji_map();
            }
        }
        if was != active() {
            self.redraw_for_emoji();
        }
    }

    fn refresh_emoji_map(self: &Rc<Self>) {
        if DISCOVERING.replace(true) {
            return;
        }
        crate::video_setup::spawn_local(self, discover(), |app, result| {
            DISCOVERING.set(false);
            match result {
                Ok(map) => {
                    if let Some(path) = map_path() {
                        let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
                        if let Ok(json) = serde_json::to_vec(&map) {
                            let _ = std::fs::write(path, json);
                        }
                    }
                    tracing::info!(emoji = map.len(), "Discord's emoji map ready");
                    let was = active();
                    MAP.with_borrow_mut(|slot| *slot = Some(Rc::new(map)));
                    if was != active() {
                        app.redraw_for_emoji();
                    }
                }
                Err(error) => tracing::warn!(%error, "could not get Discord's emoji"),
            }
        });
    }

    /// Re-renders what shows emoji: the open channel and the people lists.
    fn redraw_for_emoji(self: &Rc<Self>) {
        GENERATION.set(GENERATION.get().wrapping_add(1));
        self.redraw_times();
        self.render_members();
        self.render_user_bar();
        self.queue_composer_emoji();
    }
}

mod imp {
    use std::cell::{Cell, RefCell};

    use gtk::gdk;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;

    #[derive(Default)]
    pub struct EmojiPaintable {
        pub emoji: RefCell<String>,
        pub texture: RefCell<Option<gdk::Texture>>,
        pub size: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EmojiPaintable {
        const NAME: &'static str = "KcEmojiPaintable";
        type Type = super::EmojiPaintable;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for EmojiPaintable {}

    impl PaintableImpl for EmojiPaintable {
        fn flags(&self) -> gdk::PaintableFlags {
            gdk::PaintableFlags::STATIC_SIZE | gdk::PaintableFlags::STATIC_CONTENTS
        }

        fn intrinsic_width(&self) -> i32 {
            self.size.get()
        }

        fn intrinsic_height(&self) -> i32 {
            self.size.get()
        }

        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            if let Some(texture) = &*self.texture.borrow() {
                texture.snapshot(snapshot, width, height);
            }
        }
    }
}

glib::wrapper! {
    /// An emoji drawn inline in a text buffer: shown at `size` pixels from a
    /// sharper texture, and remembering the emoji it stands for.
    pub struct EmojiPaintable(ObjectSubclass<imp::EmojiPaintable>)
        @implements gdk::Paintable;
}

impl EmojiPaintable {
    fn new(emoji: &str, texture: &gdk::Texture, size: i32) -> Self {
        let paintable: Self = glib::Object::new();
        let imp = gtk::subclass::prelude::ObjectSubclassIsExt::imp(&paintable);
        imp.emoji.replace(emoji.to_owned());
        imp.texture.replace(Some(texture.clone()));
        imp.size.set(size);
        paintable
    }

    /// The emoji this stands for.
    pub(crate) fn emoji(&self) -> String {
        gtk::subclass::prelude::ObjectSubclassIsExt::imp(self)
            .emoji
            .borrow()
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_follow_twemoji_names() {
        assert_eq!(key("🚀"), "1f680");
        assert_eq!(key("❤️"), "2764");
        assert_eq!(key("👍🏽"), "1f44d-1f3fd");
        assert_eq!(key("🏳️‍🌈"), "1f3f3-fe0f-200d-1f308");
        assert_eq!(key("#️⃣"), "23-20e3");
    }

    #[test]
    fn text_splits_around_emoji_but_not_text_symbols() {
        let known = |candidate: &str| {
            [
                "1f680",
                "2764",
                "1f44d-1f3fd",
                "1f44d",
                "a9",
                "23-20e3",
                "1f3f3-fe0f-200d-1f308",
            ]
            .contains(&key(candidate).as_str())
        };
        assert_eq!(
            split_with("go 🚀 now", &known),
            vec![
                Segment::Text("go ".into()),
                Segment::Emoji("🚀".into()),
                Segment::Text(" now".into())
            ]
        );
        // A skin tone stays with its emoji; a ZWJ sequence is one emoji.
        assert_eq!(split_with("👍🏽", &known), vec![Segment::Emoji("👍🏽".into())]);
        assert_eq!(split_with("🏳️‍🌈", &known), vec![Segment::Emoji("🏳️‍🌈".into())]);
        // ❤ with its selector is emoji; © (text by default) and a bare digit
        // stay text.
        assert_eq!(split_with("❤️", &known), vec![Segment::Emoji("❤️".into())]);
        assert_eq!(
            split_with("© 2026 #1", &known),
            vec![Segment::Text("© 2026 #1".into())]
        );
        assert_eq!(split_with("#️⃣", &known), vec![Segment::Emoji("#️⃣".into())]);
    }

    #[test]
    fn the_map_joins_names_to_files() {
        let names = r#"{"./1f600.svg":"705800","./1f680.svg":"442304"}"#;
        let files =
            r#"{705800(e){e.exports="/assets/aaa.svg"},442304(e){e.exports="/assets/2a41.svg"}}"#;
        let map = read_map(&[names.to_owned(), files.to_owned()]);
        assert_eq!(
            map.get("1f680").map(String::as_str),
            Some("/assets/2a41.svg")
        );
        assert_eq!(map.len(), 2);
    }
}
