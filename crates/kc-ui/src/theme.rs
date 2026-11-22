//! Theming: the system's libadwaita colors or Discord's own palette, and the
//! optional use of Discord's own typeface (gg sans).
//!
//! gg sans is Discord's and is not shipped with kubecord. Like the sounds, it
//! is downloaded on request from discord.com's public web app (no account
//! details are sent) into the cache. Discord serves it as WOFF2, which Pango
//! refuses (it skips fonts in a WOFF wrapper), so each file is unwrapped to a
//! plain TrueType file on download.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::LazyLock;

use gtk::prelude::*;
use libadwaita as adw;

use crate::settings::CapturePreferences;
use crate::shell::AppState;

const WEB_APP: &str = "https://discord.com/app";
/// Stylesheets read at most while looking for the font. The faces sit in the
/// main stylesheet, which the page links near the end, so the list is read
/// from the end.
const MAX_STYLESHEETS: usize = 40;
/// The family name Discord's CSS (and the font files) use.
pub(crate) const FAMILY: &str = "gg sans";

static STYLESHEET: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"/assets/[A-Za-z0-9._-]+\.css").unwrap());
static FONT_FACE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"@font-face\s*\{([^}]*)\}").unwrap());

thread_local! {
    /// Discord's palette, while the system's colors are not followed.
    static PALETTE_CSS: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    /// The stylesheet that puts gg sans on every widget, while it is on.
    static FONT_CSS: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    /// The see-through background, while on.
    static TRANSPARENCY_CSS: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    /// gg sans has been registered with Pango this run (it cannot be taken
    /// back out; turning the setting off only stops using it).
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
    /// A download is under way.
    static FETCHING: Cell<bool> = const { Cell::new(false) };
}

/// Applies the colors: the system's libadwaita palette (and its light/dark
/// preference), or Discord's own palette in its dark or light theme.
pub(crate) fn apply_color_scheme(prefs: &CapturePreferences) {
    let scheme = if prefs.follow_system_scheme {
        adw::ColorScheme::Default
    } else if prefs.dark_scheme {
        adw::ColorScheme::ForceDark
    } else {
        adw::ColorScheme::ForceLight
    };
    let manager = adw::StyleManager::default();
    if manager.color_scheme() != scheme {
        manager.set_color_scheme(scheme);
    }
    let palette = (!prefs.follow_system_scheme).then(|| discord_palette(prefs.dark_scheme));
    set_palette_css(palette.as_deref());
    let see_through = prefs
        .transparent_background
        .then(|| transparency_css(prefs.background_opacity));
    set_css(&TRANSPARENCY_CSS, see_through.as_deref(), 3);
}

/// The main window's surfaces at `opacity` percent (1-100), the desktop
/// showing through the rest. Each pane is drawn once, over a clear window,
/// so the panes keep their own shades rather than stacking up.
pub(crate) fn transparency_css(opacity: u8) -> String {
    let opacity = opacity.clamp(1, 100);
    let see = |color: &str| format!("color-mix(in srgb, {color} {opacity}%, transparent)");
    format!(
        "window.kc-main-window {{ background-color: transparent; }}
window.kc-main-window .kc-title-bar {{ background-color: {header}; box-shadow: none; }}
window.kc-main-window .kc-rail {{ background-color: {rail}; }}
window.kc-main-window .kc-pane-sidebar {{ background-color: {sidebar}; }}
window.kc-main-window .kc-pane-chat {{ background-color: {chat}; }}
window.kc-main-window .kc-pane-members {{ background-color: {sidebar}; }}
window.kc-main-window .kc-pane-sidebar headerbar,
window.kc-main-window .kc-pane-chat headerbar,
window.kc-main-window .kc-pane-sidebar toolbarview,
window.kc-main-window .kc-pane-chat toolbarview,
window.kc-main-window .kc-pane-chat .sidebar-pane,
window.kc-main-window .kc-pane-chat .content-pane,
window.kc-main-window .kc-pane-sidebar list,
window.kc-main-window .kc-pane-chat list,
window.kc-main-window .kc-pane-members list {{ background-color: transparent; box-shadow: none; }}
",
        header = see("@headerbar_bg_color"),
        rail = see("color-mix(in srgb, @window_bg_color 92%, black)"),
        sidebar = see("@sidebar_bg_color"),
        chat = see("@window_bg_color"),
    )
}

/// Discord's colors, as libadwaita's palette variables (so every widget
/// follows) plus the surfaces kubecord draws itself: the server rail and the
/// message box.
pub(crate) fn discord_palette(dark: bool) -> String {
    // Discord's names: tertiary (server rail), secondary (channel list and
    // member list), primary (chat), the message box, menus, text.
    struct Palette {
        tertiary: &'static str,
        secondary: &'static str,
        primary: &'static str,
        input: &'static str,
        floating: &'static str,
        text: &'static str,
        header: &'static str,
        muted: &'static str,
        accent_text: &'static str,
        shade: &'static str,
    }
    let p = if dark {
        Palette {
            tertiary: "#1e1f22",
            secondary: "#2b2d31",
            primary: "#313338",
            input: "#383a40",
            floating: "#111214",
            text: "#dbdee1",
            header: "#f2f3f5",
            muted: "#949ba4",
            accent_text: "#949cf7",
            shade: "rgba(0, 0, 0, 0.24)",
        }
    } else {
        Palette {
            tertiary: "#e3e5e8",
            secondary: "#f2f3f5",
            primary: "#ffffff",
            input: "#ebedef",
            floating: "#ffffff",
            text: "#313338",
            header: "#060607",
            muted: "#5c5e66",
            accent_text: "#4752c4",
            shade: "rgba(0, 0, 0, 0.08)",
        }
    };
    format!(
        ":root {{
    --window-bg-color: {primary};
    --window-fg-color: {text};
    --view-bg-color: {primary};
    --view-fg-color: {text};
    --headerbar-bg-color: {primary};
    --headerbar-fg-color: {header};
    --headerbar-backdrop-color: {primary};
    --headerbar-shade-color: {shade};
    --headerbar-darker-shade-color: {shade};
    --sidebar-bg-color: {secondary};
    --sidebar-fg-color: {text};
    --sidebar-backdrop-color: {secondary};
    --sidebar-shade-color: {shade};
    --secondary-sidebar-bg-color: {secondary};
    --secondary-sidebar-fg-color: {text};
    --secondary-sidebar-backdrop-color: {secondary};
    --secondary-sidebar-shade-color: {shade};
    --card-bg-color: {secondary};
    --card-fg-color: {text};
    --card-shade-color: {shade};
    --dialog-bg-color: {primary};
    --dialog-fg-color: {text};
    --popover-bg-color: {floating};
    --popover-fg-color: {text};
    --popover-shade-color: {shade};
    --thumbnail-bg-color: {secondary};
    --thumbnail-fg-color: {text};
    --shade-color: {shade};
    --accent-bg-color: #5865f2;
    --accent-fg-color: #ffffff;
    --accent-color: {accent_text};
    --destructive-bg-color: #da373c;
    --destructive-fg-color: #ffffff;
    --destructive-color: #f23f43;
    --success-bg-color: #248046;
    --success-fg-color: #ffffff;
    --success-color: #23a55a;
    --warning-bg-color: #f0b232;
    --warning-fg-color: rgba(0, 0, 0, 0.8);
    --warning-color: #f0b232;
    --error-bg-color: #da373c;
    --error-fg-color: #ffffff;
    --error-color: #f23f43;
}}
.kc-rail {{ background-color: {tertiary}; }}
.kc-composer {{ background-color: {input}; }}
.dim-label, .kc-category {{ color: {muted}; opacity: 1; }}
",
        primary = p.primary,
        secondary = p.secondary,
        tertiary = p.tertiary,
        input = p.input,
        floating = p.floating,
        text = p.text,
        header = p.header,
        muted = p.muted,
        accent_text = p.accent_text,
        shade = p.shade,
    )
}

/// Installs (or, with `None`, removes) the palette stylesheet. It ranks just
/// above kubecord's own stylesheet so its rail and message-box colors win.
fn set_palette_css(css: Option<&str>) {
    set_css(&PALETTE_CSS, css, 1);
}

/// Installs `css` in `slot` (replacing what was there; `None` removes it),
/// `rank` above kubecord's own stylesheet.
fn set_css(
    slot: &'static std::thread::LocalKey<RefCell<Option<gtk::CssProvider>>>,
    css: Option<&str>,
    rank: u32,
) {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    slot.with_borrow_mut(|slot| {
        if let Some(old) = slot.take() {
            gtk::style_context_remove_provider_for_display(&display, &old);
        }
        if let Some(css) = css {
            let provider = gtk::CssProvider::new();
            provider.load_from_string(css);
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + rank,
            );
            *slot = Some(provider);
        }
    });
}

/// One `@font-face` of gg sans: its weight, whether italic, and its file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Face {
    pub weight: u32,
    pub italic: bool,
    pub url: String,
}

impl Face {
    fn file_name(&self) -> String {
        format!(
            "gg-sans-{}{}.ttf",
            self.weight,
            if self.italic { "-italic" } else { "" }
        )
    }
}

/// The page's stylesheets, last first (the main one, with the fonts, is
/// linked near the end).
pub(crate) fn stylesheet_urls(html: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut urls: Vec<String> = STYLESHEET
        .find_iter(html)
        .map(|found| format!("https://discord.com{}", found.as_str()))
        .filter(|url| seen.insert(url.clone()))
        .collect();
    urls.reverse();
    urls
}

/// gg sans's faces in a stylesheet (other families are ignored).
pub(crate) fn font_faces(css: &str) -> Vec<Face> {
    let property = |block: &str, name: &str| -> Option<String> {
        block.split(';').find_map(|declaration| {
            let (key, value) = declaration.split_once(':')?;
            (key.trim() == name).then(|| value.trim().trim_matches(['"', '\'']).to_owned())
        })
    };
    FONT_FACE
        .captures_iter(css)
        .filter_map(|capture| {
            let block = &capture[1];
            if property(block, "font-family")? != FAMILY {
                return None;
            }
            let weight = property(block, "font-weight")?.parse().ok()?;
            let italic = property(block, "font-style").as_deref() == Some("italic");
            let src = property(block, "src")?;
            let path = src
                .split("url(")
                .nth(1)?
                .split(')')
                .next()?
                .trim_matches(['"', '\'']);
            path.ends_with(".woff2").then(|| Face {
                weight,
                italic,
                url: if path.starts_with("http") {
                    path.to_owned()
                } else {
                    format!("https://discord.com{path}")
                },
            })
        })
        .collect()
}

fn font_dir() -> Option<PathBuf> {
    kc_core::paths::cache_dir().ok().map(|dir| dir.join("fonts"))
}

/// gg sans files already in the cache.
fn cached_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("gg-sans-") && name.ends_with(".ttf"))
        })
        .collect();
    files.sort();
    files
}

/// Downloads gg sans into `dir`, returning the files.
async fn download(dir: PathBuf) -> Result<Vec<PathBuf>, String> {
    let html = kc_http::cdn::fetch(WEB_APP)
        .await
        .map_err(|error| format!("could not load Discord's web app: {error}"))?;
    let html = String::from_utf8_lossy(&html);
    let mut faces = Vec::new();
    for url in stylesheet_urls(&html).into_iter().take(MAX_STYLESHEETS) {
        let Ok(css) = kc_http::cdn::fetch(&url).await else {
            continue;
        };
        faces = font_faces(&String::from_utf8_lossy(&css));
        if !faces.is_empty() {
            break;
        }
    }
    if faces.is_empty() {
        return Err("Discord's web app no longer lists the font where expected".to_owned());
    }
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    let mut files = Vec::new();
    for face in faces {
        let bytes = kc_http::cdn::fetch(&face.url)
            .await
            .map_err(|error| format!("could not download the font: {error}"))?;
        // Pango will not use a font inside a WOFF wrapper: unwrap it.
        let bytes = woff2_patched::convert_woff2_to_ttf(&mut bytes.as_slice())
            .map_err(|error| format!("could not unpack the font: {error}"))?;
        let path = dir.join(face.file_name());
        // Write under a temporary name so a half-written file is never used.
        let partial = path.with_extension("part");
        tokio::fs::write(&partial, &bytes)
            .await
            .and(tokio::fs::rename(&partial, &path).await)
            .map_err(|error| format!("could not save the font: {error}"))?;
        files.push(path);
    }
    Ok(files)
}

impl AppState {
    /// Uses gg sans everywhere (downloading it first if needed), or goes back
    /// to the system font.
    pub(crate) fn apply_discord_font(self: &Rc<Self>, on: bool) {
        if !on {
            set_font_css(None);
            return;
        }
        if REGISTERED.get() {
            set_font_css(Some(FAMILY));
            return;
        }
        let Some(dir) = font_dir() else {
            self.set_status("Could not use Discord's font: no cache folder");
            return;
        };
        let cached = cached_files(&dir);
        if !cached.is_empty() {
            self.register_font(&cached, false);
            return;
        }
        if FETCHING.replace(true) {
            return;
        }
        self.set_status("Downloading Discord's font…");
        crate::video_setup::spawn_local(self, download(dir), |app, result| {
            FETCHING.set(false);
            match result {
                // Still wanted? (It may have been switched off meanwhile.)
                Ok(files) if app.capture.borrow().discord_font => app.register_font(&files, true),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "could not get Discord's font");
                    app.set_status(&format!("Could not get Discord's font: {error}"));
                }
            }
        });
    }

    /// Registers the font files with the font map the window draws with and,
    /// once Pango lists the family, switches the interface to it. `announce`
    /// says so in the status line (after a download, not at every start).
    fn register_font(&self, files: &[PathBuf], announce: bool) {
        use pango::prelude::*;
        let Some(font_map) = self.window.pango_context().font_map() else {
            return;
        };
        for file in files {
            if let Err(error) = font_map.add_font_file(file) {
                tracing::debug!(%error, file = %file.display(), "could not add a font file");
            }
        }
        let listed = font_map
            .list_families()
            .iter()
            .any(|family| family.name().eq_ignore_ascii_case(FAMILY));
        if listed {
            REGISTERED.set(true);
            set_font_css(Some(FAMILY));
            if announce {
                self.set_status("Using Discord's font");
            }
        } else {
            tracing::warn!("Discord's font did not load");
            self.set_status("Could not load Discord's font");
        }
    }
}

/// Puts `family` on every widget (code keeps its monospace: more specific
/// rules win), or removes it.
fn set_font_css(family: Option<&str>) {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    FONT_CSS.with_borrow_mut(|slot| {
        if let Some(old) = slot.take() {
            gtk::style_context_remove_provider_for_display(&display, &old);
        }
        if let Some(family) = family {
            let provider = gtk::CssProvider::new();
            provider.load_from_string(&format!("* {{ font-family: \"{family}\", sans-serif; }}"));
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            *slot = Some(provider);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_palette_parses_in_both_themes() {
        // GTK needs a display; a sandboxed build (Nix) has none, so there only
        // the palette's shape is checked.
        let display = std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("DISPLAY").is_some();
        for dark in [true, false] {
            let css = discord_palette(dark);
            assert!(css.contains("--window-bg-color:") && css.contains(".kc-rail"));
            assert_eq!(css.matches('{').count(), css.matches('}').count());
            if !display {
                continue;
            }
            // Parse it with GTK, on gtk-rs's shared test thread (other tests
            // use it too; initialising GTK on this thread would take GTK's
            // main context from them).
            let errors = gtk::test_synced(move || {
                let provider = gtk::CssProvider::new();
                let errors = Rc::new(Cell::new(0));
                let count = errors.clone();
                provider.connect_parsing_error(move |_, _, error| {
                    eprintln!("palette CSS error: {error}");
                    count.set(count.get() + 1);
                });
                provider.load_from_string(&css);
                errors.get()
            });
            assert_eq!(errors, 0, "palette CSS should parse (dark={dark})");
        }
    }

    #[test]
    fn stylesheets_are_read_last_first() {
        let html = r#"<link href="/assets/a.1.css" rel="stylesheet"><link href="/assets/b.2.css">
            <link href="/assets/a.1.css"><script src="/assets/web.js"></script>"#;
        assert_eq!(
            stylesheet_urls(html),
            vec![
                "https://discord.com/assets/b.2.css",
                "https://discord.com/assets/a.1.css"
            ]
        );
    }

    #[test]
    fn gg_sans_faces_are_found_and_other_families_skipped() {
        let css = concat!(
            r#"@font-face{font-display:swap;font-family:gg sans;font-style:normal;font-weight:400;src:url("/assets/66d7.woff2") format("woff2")}"#,
            r#"@font-face{font-family:gg sans;font-style:italic;font-weight:700;src:url(/assets/ce3b.woff2) format("woff2")}"#,
            r#"@font-face{font-family:gg mono;font-weight:400;src:url("/assets/mono.woff2")}"#,
            r#".x{color:red}"#,
        );
        assert_eq!(
            font_faces(css),
            vec![
                Face {
                    weight: 400,
                    italic: false,
                    url: "https://discord.com/assets/66d7.woff2".to_owned()
                },
                Face {
                    weight: 700,
                    italic: true,
                    url: "https://discord.com/assets/ce3b.woff2".to_owned()
                },
            ]
        );
        assert_eq!(font_faces(css)[1].file_name(), "gg-sans-700-italic.ttf");
    }
}
