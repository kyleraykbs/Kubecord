//! Widgets for what a message contains: formatted text with mentions and
//! custom emoji, attachments, embeds and stickers.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

use gtk::prelude::*;
use libadwaita as adw;
use regex::Regex;

use kc_core::Snowflake;
use kc_http::cdn;
use kc_state::message::{Embed, EmbedMedia};
use kc_state::{Attachment, State, StickerItem};

use crate::discord_emoji::Segment;
use crate::markdown::format_size;
use crate::player::LoopingVideo;
use crate::rich::{self, Piece};
use crate::shell::AppState;
use crate::widgets::{first_icon, hex_color, icon_button, label, max_width, open_uri, status_dot};

thread_local! {
    /// Resolved invites by code (`None` = invalid/expired), so re-rendering a
    /// message with an invite link does not re-hit the API.
    static INVITE_CACHE: RefCell<HashMap<String, Option<ResolvedInvite>>> =
        RefCell::new(HashMap::new());
}

/// What an invite card shows, once resolved.
#[derive(Clone)]
struct ResolvedInvite {
    guild_id: Snowflake,
    name: String,
    icon: Option<String>,
    members: u32,
    online: u32,
}

/// What a media preview shows, and what clicking it does.
pub(crate) enum MediaKind {
    /// A still image; clicking opens the viewer.
    Image,
    /// A video with a poster frame. `file_url` is the directly-playable file
    /// (an uploaded attachment): clicking plays it in an in-app player. `None`
    /// for a webpage-backed video embed (e.g. YouTube), which opens externally.
    Video { file_url: Option<String> },
    /// A GIF delivered as a short video (a `gifv` embed, Tenor/Giphy MP4),
    /// played inline, muted and looping, through GStreamer.
    Gif { video_url: String },
    /// A real animated `image/gif` attachment, animated frame by frame from
    /// the GIF bytes; clicking opens it in the viewer.
    AnimatedGif,
}

pub(crate) struct Media<'a> {
    /// The image shown, normally the proxy URL.
    pub still: &'a str,
    /// What clicking opens: the original image or the video's page.
    pub target: &'a str,
    pub width: u32,
    pub height: u32,
    pub kind: MediaKind,
    pub spoiler: bool,
    pub description: Option<&'a str>,
    /// Set for a GIF that can be favourited: its media URL (the key Discord
    /// stores favourites under) and its `src`. `None` for plain images and
    /// videos, which the official client does not favourite.
    pub favorite: Option<FavoriteTarget<'a>>,
}

/// The identity of a GIF for favouriting: the URL used as Discord's map key
/// (and `src`), with its dimensions. The format (image vs video) is taken from
/// the `MediaKind`.
pub(crate) struct FavoriteTarget<'a> {
    pub url: &'a str,
    pub width: u32,
    pub height: u32,
}

const EMBED_MAX_WIDTH: i32 = 520;

/// The invite card's maximum width, so it does not stretch across the chat.
const INVITE_WIDTH: i32 = 432;

/// Largest inline media preview, in logical pixels.
const MEDIA_MAX_WIDTH: f64 = 420.0;
const MEDIA_MAX_HEIGHT: f64 = 300.0;
const INLINE_EMOJI: i32 = 22;
const JUMBO_EMOJI: i32 = 48;

/// Resolves mention ids against the store while text is rendered.
pub(crate) struct StateResolver<'a> {
    pub state: &'a State,
    pub guild: Option<Snowflake>,
}

impl rich::Resolver for StateResolver<'_> {
    fn user(&self, id: Snowflake) -> Option<String> {
        let user = self.state.users.get(&id)?;
        Some(self.state.display_name(self.guild, user))
    }

    fn role(&self, id: Snowflake) -> Option<(String, u32)> {
        let role = self.state.role(self.guild?, id)?;
        Some((role.name.clone(), role.color))
    }

    fn channel(&self, id: Snowflake) -> Option<String> {
        Some(self.state.channels.get(&id)?.display_name())
    }

    fn timestamp(&self, unix: i64, format: char) -> String {
        let Ok(time) = glib::DateTime::from_unix_local(unix) else {
            return unix.to_string();
        };
        // The date part, and whether the time follows with seconds.
        let (date, clock) = match format {
            't' => (None, Some(false)),
            'T' => (None, Some(true)),
            'd' => (Some("%x"), None),
            'D' => (Some("%e %B %Y"), None),
            'F' => (Some("%A, %e %B %Y"), Some(false)),
            's' => (Some("%x"), Some(false)),
            'S' => (Some("%x"), Some(true)),
            'R' => return relative_time(unix),
            _ => (Some("%e %B %Y"), Some(false)),
        };
        let date = date.map(|pattern| {
            time.format(pattern)
                .map(|text| text.trim().to_owned())
                .unwrap_or_default()
        });
        let clock = clock.map(|seconds| clock_of(&time, seconds));
        match (date, clock) {
            (Some(date), Some(clock)) => format!("{date} {clock}"),
            (Some(text), None) | (None, Some(text)) => text,
            (None, None) => unix.to_string(),
        }
    }
}

/// Times of day are shown on a 24-hour clock (`17:58`) instead of the
/// 12-hour default (`5:58 PM`). Set from the preferences.
static USE_24_HOUR: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_24_hour(enabled: bool) {
    USE_24_HOUR.store(enabled, Ordering::Relaxed);
}

/// A time of day as the clock setting shows it: `5:58 PM` or `17:58`
/// (with `seconds`, `5:58:07 PM` or `17:58:07`).
fn clock_text(hour: u32, minute: u32, second: Option<u32>, use_24_hour: bool) -> String {
    let seconds = second
        .map(|second| format!(":{second:02}"))
        .unwrap_or_default();
    if use_24_hour {
        format!("{hour:02}:{minute:02}{seconds}")
    } else {
        let suffix = if hour < 12 { "AM" } else { "PM" };
        let hour = (hour + 11) % 12 + 1;
        format!("{hour}:{minute:02}{seconds} {suffix}")
    }
}

/// The time of day of a local time, as the clock setting shows it.
pub(crate) fn clock_of(time: &glib::DateTime, seconds: bool) -> String {
    clock_text(
        time.hour() as u32,
        time.minute() as u32,
        seconds.then_some(time.second() as u32),
        USE_24_HOUR.load(Ordering::Relaxed),
    )
}

fn relative_time(unix: i64) -> String {
    let now = glib::DateTime::now_local().map_or(unix, |now| now.to_unix());
    let delta = unix - now;
    let (amount, unit) = match delta.abs() {
        seconds if seconds < 60 => (seconds, "second"),
        seconds if seconds < 3600 => (seconds / 60, "minute"),
        seconds if seconds < 86_400 => (seconds / 3600, "hour"),
        seconds if seconds < 2_592_000 => (seconds / 86_400, "day"),
        seconds if seconds < 31_536_000 => (seconds / 2_592_000, "month"),
        seconds => (seconds / 31_536_000, "year"),
    };
    let plural = if amount == 1 { "" } else { "s" };
    if delta < 0 {
        format!("{amount} {unit}{plural} ago")
    } else {
        format!("in {amount} {unit}{plural}")
    }
}

/// A message timestamp in local time: `2:03 PM` today, `Yesterday at
/// 2:03 PM`, otherwise the date and time (24-hour when so set).
pub(crate) fn format_time(iso: &str) -> String {
    let Some(time) = local_time(iso) else {
        return String::new();
    };
    let clock = clock_of(&time, false);
    let Ok(now) = glib::DateTime::now_local() else {
        return clock;
    };
    let day = |time: &glib::DateTime| (time.year(), time.day_of_year());
    if day(&time) == day(&now) {
        clock
    } else if now
        .add_days(-1)
        .is_ok_and(|yesterday| day(&yesterday) == day(&time))
    {
        format!("Yesterday at {clock}")
    } else {
        let date = time.format("%x").map(String::from).unwrap_or_default();
        format!("{date} {clock}")
    }
}

/// The time beside a grouped message, in the avatar's width: the clock
/// without AM/PM, which the full time in its tooltip carries.
pub(crate) fn gutter_time(iso: &str) -> String {
    let clock = clock_time(iso);
    clock
        .strip_suffix(" AM")
        .or_else(|| clock.strip_suffix(" PM"))
        .unwrap_or(&clock)
        .to_owned()
}

pub(crate) fn clock_time(iso: &str) -> String {
    local_time(iso)
        .map(|time| clock_of(&time, false))
        .unwrap_or_default()
}

/// `18 November 2026`, for the divider between days.
pub(crate) fn long_date(iso: &str) -> String {
    local_time(iso)
        .and_then(|time| time.format("%e %B %Y").ok())
        .map(|text| text.trim().to_owned())
        .unwrap_or_default()
}

/// Invite codes from any `discord.gg/…` or `discord.com/invite/…` link in text.
fn invite_codes(content: &str) -> Vec<String> {
    static INVITE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:discord\.gg|discord(?:app)?\.com/invite)/([A-Za-z0-9-]+)")
            .expect("a valid invite regex")
    });
    INVITE
        .captures_iter(content)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str().to_owned()))
        .collect()
}

/// Links in `content` to media hosted on Discord's CDN
/// (`cdn.discordapp.com` / `media.discordapp.net` attachments) that the client
/// can show itself. Discord does not always unfurl these (signed
/// `cdn.discordapp.com/attachments/...` links, as favourites taken from an
/// uploaded GIF hold, get no embed at all).
pub(crate) fn discord_media_links(content: &str) -> Vec<String> {
    static LINK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"https://(?:cdn\.discordapp\.com|media\.discordapp\.net)/(?:ephemeral-)?attachments/[^\s<>]+",
        )
        .expect("a valid media link regex")
    });
    LINK.find_iter(content)
        .map(|m| m.as_str().to_owned())
        .filter(|url| link_media_kind(url).is_some())
        .collect()
}

/// Whether `url` is served by a GIF provider (its videos are GIFs: short,
/// silent, meant to loop).
fn is_gif_host(url: &str) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .unwrap_or("")
        .to_ascii_lowercase();
    ["klipy.com", "tenor.com", "giphy.com"]
        .iter()
        .any(|provider| host == *provider || host.ends_with(&format!(".{provider}")))
}

/// Whether two links name the same thing, ignoring the query string (Discord
/// signs attachment links with `?ex=..&is=..&hm=..`, drops that from an
/// embed's `url`, and may re-sign it) and which of Discord's two media hosts
/// serves it.
pub(crate) fn same_link(a: &str, b: &str) -> bool {
    let bare = |url: &str| {
        url.split(['?', '#']).next().unwrap_or(url).replacen(
            "//media.discordapp.net/",
            "//cdn.discordapp.com/",
            1,
        )
    };
    bare(a) == bare(b)
}

/// What a Discord media link points at, from its file extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkMedia {
    Gif,
    Image,
    Video,
}

pub(crate) fn link_media_kind(url: &str) -> Option<LinkMedia> {
    let path = url.split(['?', '#']).next()?.to_ascii_lowercase();
    let extension = path.rsplit_once('.')?.1;
    match extension {
        "gif" => Some(LinkMedia::Gif),
        "png" | "jpg" | "jpeg" | "webp" | "avif" => Some(LinkMedia::Image),
        "mp4" | "webm" | "mov" | "m4v" => Some(LinkMedia::Video),
        _ => None,
    }
}

/// "Est. Nov 2026" from a guild's creation time (encoded in its id).
fn established(guild_id: Snowflake) -> String {
    let seconds = (guild_id.timestamp() / 1000) as i64;
    glib::DateTime::from_unix_local(seconds)
        .ok()
        .and_then(|date| date.format("%b %Y").ok())
        .map(|when| format!("Est. {when}"))
        .unwrap_or_default()
}

/// A value that changes when the local calendar day does.
pub(crate) fn day_key(iso: &str) -> i32 {
    local_time(iso).map_or(0, |time| time.year() * 1000 + time.day_of_year())
}

/// Whole minutes from `start` to `end` (ISO 8601 times), if both read.
pub(crate) fn minutes_between(start: &str, end: &str) -> Option<i64> {
    let start = glib::DateTime::from_iso8601(start, None).ok()?;
    let end = glib::DateTime::from_iso8601(end, None).ok()?;
    Some(end.difference(&start).as_minutes())
}

/// "a few seconds", "1 minute", "2 hours".
pub(crate) fn call_length(minutes: i64) -> String {
    match minutes {
        ..1 => "a few seconds".to_owned(),
        1 => "a minute".to_owned(),
        2..60 => format!("{minutes} minutes"),
        60..120 => "an hour".to_owned(),
        _ => format!("{} hours", minutes / 60),
    }
}

fn local_time(iso: &str) -> Option<glib::DateTime> {
    glib::DateTime::from_iso8601(iso, None)
        .ok()?
        .to_local()
        .ok()
}

/// Display size for media of the given pixel dimensions.
fn fit(width: u32, height: u32) -> (i32, i32) {
    let (width, height) = (f64::from(width.max(1)), f64::from(height.max(1)));
    let scale = (MEDIA_MAX_WIDTH / width)
        .min(MEDIA_MAX_HEIGHT / height)
        .min(1.0);
    (
        (width * scale).round().max(16.0) as i32,
        (height * scale).round().max(16.0) as i32,
    )
}

/// Marks where a link starts and ends in markup put into a text view.
const LINK_OPEN: char = '\u{e000}';
const LINK_CLOSE: char = '\u{e001}';

/// Text buffers do not take `<a>`: each anchor becomes a span (underlined
/// for web links) bracketed by [`LINK_OPEN`]/[`LINK_CLOSE`], and its target
/// is returned in order.
fn mark_anchors(markup: &str) -> (String, Vec<String>) {
    let markup = markup.replace([LINK_OPEN, LINK_CLOSE], "");
    let mut out = String::with_capacity(markup.len());
    let mut uris = Vec::new();
    let mut rest = markup.as_str();
    while let Some(start) = rest.find("<a href=\"") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "<a href=\"".len()..];
        let Some(end) = after.find("\">") else {
            rest = "";
            break;
        };
        let uri = unescape_markup(&after[..end]);
        out.push_str(if uri.starts_with("http") {
            "<span underline=\"single\">"
        } else {
            "<span>"
        });
        out.push(LINK_OPEN);
        uris.push(uri);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    (out.replace("</a>", &format!("{LINK_CLOSE}</span>")), uris)
}

fn unescape_markup(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The links in a text view, as tags over their text.
type Links = RefCell<Vec<(gtk::TextTag, String)>>;

/// Appends `markup` to `buffer`, its links tagged and listed in `links`.
fn insert_linked_markup(buffer: &gtk::TextBuffer, markup: &str, links: &Links) {
    let (markup, uris) = mark_anchors(markup);
    let start = buffer.end_iter().offset();
    buffer.insert_markup(&mut buffer.end_iter(), &markup);
    if uris.is_empty() {
        return;
    }
    let text = buffer.text(&buffer.iter_at_offset(start), &buffer.end_iter(), true);
    // Where the markers are, and the ranges they bound once removed.
    let mut markers = Vec::new();
    let mut open = Vec::new();
    let mut ranges = Vec::new();
    let mut opened = 0;
    for (index, c) in text.chars().enumerate() {
        let index = index as i32;
        if c == LINK_OPEN {
            open.push((opened, index - markers.len() as i32));
            opened += 1;
            markers.push(index);
        } else if c == LINK_CLOSE {
            if let Some((nth, from)) = open.pop() {
                ranges.push((nth, from, index - markers.len() as i32));
            }
            markers.push(index);
        }
    }
    for &index in markers.iter().rev() {
        let at = start + index;
        buffer.delete(
            &mut buffer.iter_at_offset(at),
            &mut buffer.iter_at_offset(at + 1),
        );
    }
    for (nth, from, to) in ranges {
        let Some(uri) = uris.get(nth) else {
            continue;
        };
        let Some(tag) = buffer.create_tag(None, &[]) else {
            continue;
        };
        if uri.starts_with("http") {
            tag.set_foreground_rgba(Some(&adw::StyleManager::default().accent_color_rgba()));
        }
        buffer.apply_tag(
            &tag,
            &buffer.iter_at_offset(start + from),
            &buffer.iter_at_offset(start + to),
        );
        links.borrow_mut().push((tag, uri.clone()));
    }
}

/// The link under a point of a text view, if any.
fn link_at(view: &gtk::TextView, links: &Links, x: f64, y: f64) -> Option<String> {
    let (x, y) = view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
    let iter = view.iter_at_location(x, y)?;
    let links = links.borrow();
    iter.tags().iter().find_map(|tag| {
        links
            .iter()
            .find(|(link, _)| link == tag)
            .map(|(_, uri)| uri.clone())
    })
}

/// Frames a text view is watched for after appearing or a resize.
const SETTLE_FRAMES: u32 = 30;

/// Asks a text view's parent to measure it again whenever its height (for
/// its width) differs from what it was given, for [`SETTLE_FRAMES`] frames.
fn settle_height(view: &gtk::TextView) {
    let frames = std::cell::Cell::new(0);
    view.add_tick_callback(move |view, _| {
        let width = view.width();
        if width > 0 {
            let (height, ..) = view.measure(gtk::Orientation::Vertical, width);
            if height != view.height() {
                view.queue_resize();
            }
        }
        frames.set(frames.get() + 1);
        if frames.get() < SETTLE_FRAMES {
            glib::ControlFlow::Continue
        } else {
            glib::ControlFlow::Break
        }
    });
}

/// Whether any (non-code) text in `pieces` has an emoji drawn in Discord's art.
fn has_discord_emoji(pieces: &[Piece]) -> bool {
    crate::discord_emoji::active()
        && pieces.iter().any(|piece| {
            matches!(piece, Piece::Text { text, style }
                if !style.code && !style.code_block && crate::discord_emoji::contains_emoji(text))
        })
}

impl AppState {
    /// Formatted message text. Returns `None` for empty content.
    pub(crate) fn text_widget(
        self: &Rc<Self>,
        content: &str,
        guild: Option<Snowflake>,
        state: &State,
    ) -> Option<gtk::Widget> {
        if content.trim().is_empty() {
            return None;
        }
        let pieces = rich::parse(content);
        let resolver = StateResolver { state, guild };

        if rich::is_emoji_only(&pieces) {
            return Some(self.jumbo_emoji(&pieces));
        }
        if rich::has_custom_emoji(&pieces) || has_discord_emoji(&pieces) {
            return Some(self.emoji_text(pieces, guild, &resolver));
        }

        let text = gtk::Label::new(None);
        text.set_xalign(0.0);
        text.set_wrap(true);
        text.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        text.set_selectable(true);
        text.set_markup(&rich::to_markup(&pieces, &resolver, false));
        let app = self.clone();
        text.connect_activate_link(move |text, uri| {
            if uri == "kubecord://spoiler" {
                // Reveal: render again with spoilers shown.
                if let Some(markup) = app.with_session(|session| {
                    session.store.read(|state| {
                        rich::to_markup(&pieces, &StateResolver { state, guild }, true)
                    })
                }) {
                    text.set_markup(&markup);
                }
            } else {
                app.activate_link(uri, text.upcast_ref());
            }
            glib::Propagation::Stop
        });
        Some(text.upcast())
    }

    /// Handles a link clicked in message text.
    pub(crate) fn activate_link(self: &Rc<Self>, uri: &str, anchor: &gtk::Widget) {
        if let Some(id) = uri
            .strip_prefix("kubecord://user/")
            .and_then(|id| id.parse::<Snowflake>().ok())
        {
            self.show_profile(id, anchor);
        } else if let Some(id) = uri
            .strip_prefix("kubecord://channel/")
            .and_then(|id| id.parse::<Snowflake>().ok())
        {
            let channel = self.with_session(|session| {
                session.store.read(|state| state.channels.get(&id).cloned())
            });
            if let Some(channel) = channel.flatten() {
                self.open_channel(channel);
            }
        } else if uri.starts_with("http://") || uri.starts_with("https://") {
            open_uri(Some(self.window.upcast_ref()), uri);
        }
    }

    /// A message made only of emoji is shown large.
    fn jumbo_emoji(self: &Rc<Self>, pieces: &[Piece]) -> gtk::Widget {
        let wrap = adw::WrapBox::new();
        wrap.set_child_spacing(4);
        wrap.set_line_spacing(4);
        for piece in pieces {
            match piece {
                Piece::Emoji { id, name, .. } => {
                    let image = gtk::Image::new();
                    image.set_pixel_size(JUMBO_EMOJI);
                    image.set_tooltip_text(Some(&format!(":{name}:")));
                    self.images
                        .set_image(&image, &cdn::emoji(*id, false, JUMBO_EMOJI as u32 * 2));
                    wrap.append(&image);
                }
                Piece::Text { text, .. } => {
                    for segment in crate::discord_emoji::split(text) {
                        let text = match segment {
                            Segment::Emoji(emoji) => {
                                if let Some(image) =
                                    crate::discord_emoji::image(&emoji, JUMBO_EMOJI)
                                {
                                    wrap.append(&image);
                                    continue;
                                }
                                emoji
                            }
                            Segment::Text(text) => text,
                        };
                        let text = text.trim();
                        if !text.is_empty() {
                            let emoji = gtk::Label::new(Some(text));
                            emoji.add_css_class("kc-jumbo");
                            emoji.set_selectable(true);
                            wrap.append(&emoji);
                        }
                    }
                }
                _ => {}
            }
        }
        wrap.upcast()
    }

    /// Text with emoji images inline (custom ones, and Unicode ones in
    /// Discord's art). Labels cannot embed widgets, so this uses a read-only
    /// text view with the images at child anchors; links stay clickable, and
    /// spoilers stay hidden until clicked.
    fn emoji_text(
        self: &Rc<Self>,
        pieces: Vec<Piece>,
        guild: Option<Snowflake>,
        resolver: &StateResolver,
    ) -> gtk::Widget {
        let view = gtk::TextView::new();
        view.set_editable(false);
        view.set_cursor_visible(false);
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.add_css_class("kc-rich");
        let links: Rc<Links> = Rc::default();
        let pieces = Rc::new(pieces);
        self.fill_emoji_text(&view, &pieces, resolver, false, &links);

        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_PRIMARY);
        let app = self.clone();
        let target = links.clone();
        click.connect_released(move |gesture, _, x, y| {
            let Some(view) = gesture.widget().and_downcast::<gtk::TextView>() else {
                return;
            };
            // A drag selecting text is not a click on a link.
            if view.buffer().has_selection() {
                return;
            }
            let Some(uri) = link_at(&view, &target, x, y) else {
                return;
            };
            if uri == "kubecord://spoiler" {
                app.with_session(|session| {
                    session.store.read(|state| {
                        let resolver = StateResolver { state, guild };
                        app.fill_emoji_text(&view, &pieces, &resolver, true, &target);
                    })
                });
            } else {
                app.activate_link(&uri, view.upcast_ref());
            }
        });
        view.add_controller(click);

        let motion = gtk::EventControllerMotion::new();
        let hover = links.clone();
        motion.connect_motion(move |motion, x, y| {
            let Some(view) = motion.widget().and_downcast::<gtk::TextView>() else {
                return;
            };
            let over = link_at(&view, &hover, x, y).is_some();
            view.set_cursor_from_name(Some(if over { "pointer" } else { "text" }));
        });
        view.add_controller(motion);
        // A text view first measures itself before it knows its width (so
        // as if every word wrapped), and does not ask again once it does:
        // ask on the next frame, when it has its real width.
        // A text view first measures itself before it knows its width (as if
        // every word wrapped) and does not ask its parent again once its text
        // wraps at the real width, so check for a short while after it
        // appears, and again when the window's width changes.
        let resized: Rc<RefCell<Option<(gtk::Window, glib::SignalHandlerId)>>> = Rc::default();
        let on_resize = resized.clone();
        view.connect_map(move |view| {
            settle_height(view);
            if let Some(window) = view.root().and_downcast::<gtk::Window>() {
                let weak = view.downgrade();
                let id = window.connect_default_width_notify(move |_| {
                    if let Some(view) = weak.upgrade() {
                        settle_height(&view);
                    }
                });
                on_resize.replace(Some((window, id)));
            }
        });
        view.connect_unmap(move |_| {
            if let Some((window, id)) = resized.take() {
                window.disconnect(id);
            }
        });
        view.upcast()
    }

    /// (Re)fills an [`AppState::emoji_text`] view.
    fn fill_emoji_text(
        self: &Rc<Self>,
        view: &gtk::TextView,
        pieces: &[Piece],
        resolver: &StateResolver,
        revealed: bool,
        links: &Links,
    ) {
        let buffer = view.buffer();
        buffer.set_text("");
        let table = buffer.tag_table();
        for (tag, _) in links.borrow_mut().drain(..) {
            table.remove(&tag);
        }

        let mut run: Vec<Piece> = Vec::new();
        let flush = |run: &mut Vec<Piece>| {
            if !run.is_empty() {
                insert_linked_markup(&buffer, &rich::to_markup(run, resolver, revealed), links);
                run.clear();
            }
        };
        let place = |run: &mut Vec<Piece>, image: &gtk::Image| {
            flush(run);
            let anchor = buffer.create_child_anchor(&mut buffer.end_iter());
            view.add_child_at_anchor(image, &anchor);
        };
        for piece in pieces {
            match piece {
                Piece::Emoji { id, name, .. } => {
                    let image = gtk::Image::new();
                    image.set_pixel_size(INLINE_EMOJI);
                    image.set_tooltip_text(Some(&format!(":{name}:")));
                    self.images
                        .set_image(&image, &cdn::emoji(*id, false, INLINE_EMOJI as u32 * 2));
                    place(&mut run, &image);
                }
                // Code keeps its characters; a hidden spoiler keeps its
                // emoji under the cover too.
                Piece::Text { text, style }
                    if !style.code && !style.code_block && (revealed || !style.spoiler) =>
                {
                    for segment in crate::discord_emoji::split(text) {
                        match segment {
                            Segment::Emoji(emoji) => {
                                match crate::discord_emoji::image(&emoji, INLINE_EMOJI) {
                                    Some(image) => place(&mut run, &image),
                                    None => run.push(Piece::Text {
                                        text: emoji,
                                        style: style.clone(),
                                    }),
                                }
                            }
                            Segment::Text(text) => run.push(Piece::Text {
                                text,
                                style: style.clone(),
                            }),
                        }
                    }
                }
                other => run.push(other.clone()),
            }
        }
        flush(&mut run);
    }

    /// An image, video poster, audio or file card for one attachment.
    pub(crate) fn attachment_widget(self: &Rc<Self>, attachment: &Attachment) -> gtk::Widget {
        let visual = attachment.is_image() || attachment.is_video();
        if let (true, Some(width), Some(height)) = (visual, attachment.width, attachment.height) {
            let still = if attachment.proxy_url.is_empty() {
                &attachment.url
            } else {
                &attachment.proxy_url
            };
            return self.media_preview(Media {
                still,
                target: &attachment.url,
                width,
                height,
                kind: if attachment.is_video() {
                    // Play the original file (not the transcoded proxy) in-app.
                    let file_url = if attachment.url.is_empty() {
                        attachment.proxy_url.clone()
                    } else {
                        attachment.url.clone()
                    };
                    MediaKind::Video {
                        file_url: Some(file_url),
                    }
                } else if attachment.is_gif() {
                    MediaKind::AnimatedGif
                } else {
                    MediaKind::Image
                },
                spoiler: attachment.is_spoiler(),
                description: attachment.description.as_deref(),
                // Only a real GIF attachment can be favourited.
                favorite: attachment.is_gif().then_some(FavoriteTarget {
                    url: &attachment.url,
                    width,
                    height,
                }),
            });
        }

        let card = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        card.add_css_class("kc-file");
        card.set_halign(gtk::Align::Start);
        let icon = gtk::Image::from_icon_name(if attachment.is_audio() {
            "audio-x-generic-symbolic"
        } else {
            "text-x-generic-symbolic"
        });
        icon.set_pixel_size(28);
        icon.set_valign(gtk::Align::Center);
        card.append(&icon);

        // Filename and size, left-aligned, taking the slack so the download
        // button is pushed to the right edge of the card.
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_halign(gtk::Align::Start);
        text.set_valign(gtk::Align::Center);
        text.set_hexpand(true);
        let name = gtk::LinkButton::with_label(&attachment.url, &attachment.filename);
        name.set_halign(gtk::Align::Start);
        name.add_css_class("flat");
        if let Some(label) = name.child().and_downcast::<gtk::Label>() {
            label.set_xalign(0.0);
        }
        text.append(&name);
        let size = label(&format_size(attachment.size), &["caption", "dim-label"]);
        size.set_halign(gtk::Align::Start);
        size.set_xalign(0.0);
        size.set_margin_start(8);
        text.append(&size);
        card.append(&text);

        // A Download button saves the file wherever the user chooses.
        let download = icon_button(
            first_icon(&["folder-download-symbolic", "document-save-symbolic"]),
            "Download",
        );
        download.set_valign(gtk::Align::Center);
        let app = self.clone();
        let url = if attachment.url.is_empty() {
            attachment.proxy_url.clone()
        } else {
            attachment.url.clone()
        };
        let filename = attachment.filename.clone();
        download.connect_clicked(move |_| {
            app.download_attachment(&url, &filename);
        });
        card.append(&download);
        card.upcast()
    }

    /// Opens a save dialog for an attachment and, once a destination is
    /// chosen, downloads the file there (reusing the cache download helper),
    /// reporting success or failure in the status line.
    fn download_attachment(self: &Rc<Self>, url: &str, filename: &str) {
        let dialog = gtk::FileDialog::new();
        dialog.set_title("Save Attachment");
        dialog.set_initial_name(Some(filename));
        let app = self.clone();
        let url = url.to_owned();
        let filename = filename.to_owned();
        dialog.save(
            Some(&self.window),
            gtk::gio::Cancellable::NONE,
            move |result| {
                let Ok(destination) = result else {
                    // The user cancelled the dialog.
                    return;
                };
                // The file extension lets the cache helper name its copy; it
                // does not affect the saved file.
                let extension = filename
                    .rsplit_once('.')
                    .map(|(_, ext)| ext.to_owned())
                    .unwrap_or_else(|| "bin".to_owned());
                let app = app.clone();
                let name = filename.clone();
                let images = app.images.clone();
                images.file(&url, &extension, move |path| {
                    let Some(path) = path else {
                        app.set_status(&format!("Could not download {name}"));
                        return;
                    };
                    // Copy the cached download to the chosen destination.
                    let source = gtk::gio::File::for_path(&path);
                    match source.copy(
                        &destination,
                        gtk::gio::FileCopyFlags::OVERWRITE,
                        gtk::gio::Cancellable::NONE,
                        None,
                    ) {
                        Ok(()) => app.set_status(&format!("Saved {name}")),
                        Err(error) => app.set_status(&format!("Could not save {name}: {error}")),
                    }
                });
            },
        );
    }

    /// Invite cards for any `discord.gg/…` links in `content`. Discord does not
    /// embed invites server-side for user clients, so the client resolves them.
    pub(crate) fn invite_cards(self: &Rc<Self>, content: &str) -> Vec<gtk::Widget> {
        let mut seen = std::collections::HashSet::new();
        invite_codes(content)
            .into_iter()
            .filter(|code| seen.insert(code.clone()))
            .map(|code| self.invite_card(&code))
            .collect()
    }

    /// One invite card: fills from cache, or shows a spinner while it resolves.
    fn invite_card(self: &Rc<Self>, code: &str) -> gtk::Widget {
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 8);
        frame.add_css_class("kc-invite");

        if let Some(cached) = INVITE_CACHE.with_borrow(|cache| cache.get(code).cloned()) {
            self.fill_invite_card(&frame, code, cached.as_ref());
            return max_width(&frame, INVITE_WIDTH).upcast();
        }

        let spinner = gtk::Spinner::new();
        spinner.set_halign(gtk::Align::Center);
        spinner.set_margin_top(12);
        spinner.set_margin_bottom(12);
        spinner.start();
        frame.append(&spinner);

        let Some((_, http, _)) = self.active() else {
            return max_width(&frame, INVITE_WIDTH).upcast();
        };
        let code = code.to_owned();
        let request_code = code.clone();
        let frame_weak = frame.downgrade();
        crate::video_setup::spawn_local(
            self,
            async move { http.resolve_invite(&request_code).await.map_err(|_| ()) },
            move |app, result| {
                let resolved = result.ok().map(|invite| ResolvedInvite {
                    guild_id: invite.guild.as_ref().map(|g| g.id).unwrap_or_default(),
                    name: invite
                        .guild
                        .as_ref()
                        .and_then(|g| g.name.clone())
                        .unwrap_or_else(|| "Unknown Server".to_owned()),
                    icon: invite.guild.as_ref().and_then(|g| g.icon.clone()),
                    members: invite.approximate_member_count,
                    online: invite.approximate_presence_count,
                });
                INVITE_CACHE.with_borrow_mut(|cache| {
                    cache.insert(code.clone(), resolved.clone());
                });
                if let Some(frame) = frame_weak.upgrade() {
                    while let Some(child) = frame.first_child() {
                        frame.remove(&child);
                    }
                    app.fill_invite_card(&frame, &code, resolved.as_ref());
                }
            },
        );
        max_width(&frame, INVITE_WIDTH).upcast()
    }

    /// Populates (or, on `None`, marks invalid) an invite card frame.
    fn fill_invite_card(
        self: &Rc<Self>,
        frame: &gtk::Box,
        code: &str,
        info: Option<&ResolvedInvite>,
    ) {
        let Some(info) = info else {
            frame.append(&label("Invalid or expired invite", &["dim-label"]));
            return;
        };

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let icon = info
            .icon
            .as_ref()
            .map(|hash| cdn::guild_icon(info.guild_id, hash, 96));
        row.append(&self.images.avatar(48, &info.name, icon.as_deref()));

        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_valign(gtk::Align::Center);
        text.append(&label(&info.name, &["heading"]));
        let counts = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        // The status dots default to bottom-aligned; centre them with the text.
        let online_dot = status_dot("online");
        online_dot.set_valign(gtk::Align::Center);
        counts.append(&online_dot);
        counts.append(&label(
            &format!("{} Online", info.online),
            &["caption", "dim-label"],
        ));
        let offline_dot = status_dot("offline");
        offline_dot.set_valign(gtk::Align::Center);
        counts.append(&offline_dot);
        counts.append(&label(
            &format!("{} Members", info.members),
            &["caption", "dim-label"],
        ));
        text.append(&counts);
        let est = established(info.guild_id);
        if !est.is_empty() {
            text.append(&label(&est, &["caption", "dim-label"]));
        }
        row.append(&text);
        frame.append(&row);

        // "Go to Server" when already a member, else "Join".
        let member = self
            .with_session(|session| {
                session
                    .store
                    .read(|state| state.guilds.contains_key(&info.guild_id))
            })
            .unwrap_or(false);
        let button = gtk::Button::with_label(if member { "Go to Server" } else { "Join" });
        button.add_css_class("suggested-action");
        button.set_hexpand(true);
        let app = self.clone();
        let guild_id = info.guild_id;
        let code = code.to_owned();
        button.connect_clicked(move |_| {
            if member {
                app.select_guild(guild_id);
            } else {
                app.join_server(&code);
            }
        });
        frame.append(&button);
    }

    /// Shows a Discord-hosted media link Discord did not unfurl: a GIF
    /// animates, an image shows, a video gets a play card. The size is not
    /// known until the image arrives, so the picture takes its fitted size
    /// from the first frame.
    pub(crate) fn link_media_widget(self: &Rc<Self>, url: &str) -> Option<gtk::Widget> {
        let kind = link_media_kind(url)?;
        if kind == LinkMedia::Video {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            row.add_css_class("kc-file");
            row.set_halign(gtk::Align::Start);
            let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
            play.add_css_class("circular");
            play.set_valign(gtk::Align::Center);
            play.set_tooltip_text(Some("Play"));
            let name = url
                .split(['?', '#'])
                .next()
                .and_then(|path| path.rsplit('/').next())
                .unwrap_or("Video")
                .to_owned();
            row.append(&play);
            row.append(&label(&name, &[]));
            let app = self.clone();
            let target = url.to_owned();
            play.connect_clicked(move |_| app.show_video(&target, ""));
            self.attach_media_menu(
                &row,
                crate::media_menu::MediaLinks {
                    link: url.to_owned(),
                    file: Some(url.to_owned()),
                    kind: crate::media_menu::MediaType::Video,
                },
            );
            return Some(row.upcast());
        }

        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_size_request(-1, 160);
        picture.add_css_class("kc-media");
        picture.set_overflow(gtk::Overflow::Hidden);
        // Take the fitted size once, from the first frame (a GIF swaps the
        // paintable every frame; resizing each time would be wasted work).
        let sized = Rc::new(std::cell::Cell::new(false));
        let overlay = gtk::Overlay::new();
        let clamp = max_width(&overlay, MEDIA_MAX_WIDTH as i32);
        let frame = clamp.downgrade();
        picture.connect_paintable_notify(move |picture| {
            if sized.get() {
                return;
            }
            if let Some(paintable) = picture.paintable() {
                let (width, height) = (paintable.intrinsic_width(), paintable.intrinsic_height());
                if width > 0 && height > 0 {
                    sized.set(true);
                    let (width, height) = fit(width as u32, height as u32);
                    picture.set_size_request(width, height);
                    // Narrow the frame to the picture so the GIF tag sits on it.
                    if let Some(frame) = frame.upgrade() {
                        frame.set_maximum_size(width);
                        frame.set_tightening_threshold(width);
                    }
                }
            }
        });
        if kind == LinkMedia::Gif {
            self.images.set_animated_picture(&picture, url);
        } else {
            self.images.set_picture(&picture, url);
        }

        overlay.set_child(Some(&picture));
        if kind == LinkMedia::Gif {
            let tag = gtk::Label::new(Some("GIF"));
            tag.add_css_class("kc-media-tag");
            tag.set_halign(gtk::Align::Start);
            tag.set_valign(gtk::Align::Start);
            tag.set_margin_start(6);
            tag.set_margin_top(6);
            tag.set_can_target(false);
            overlay.add_overlay(&tag);
        }
        let click = gtk::GestureClick::new();
        let app = self.clone();
        let target = url.to_owned();
        click.connect_released(move |_, _, _, _| app.show_image(&target, &target));
        picture.add_controller(click);
        picture.set_cursor_from_name(Some("pointer"));
        self.attach_media_menu(
            &picture,
            crate::media_menu::MediaLinks {
                link: url.to_owned(),
                file: Some(url.to_owned()),
                kind: if kind == LinkMedia::Gif {
                    crate::media_menu::MediaType::Gif
                } else {
                    crate::media_menu::MediaType::Image
                },
            },
        );
        Some(clamp.upcast())
    }

    /// A preview of an image, video or GIF, sized to keep its aspect.
    fn media_preview(self: &Rc<Self>, media: Media) -> gtk::Widget {
        let (display_width, display_height) = fit(media.width, media.height);
        let scale = self.window.scale_factor().max(1) as u32;
        let mut url = cdn::resized(
            media.still,
            display_width as u32 * scale,
            display_height as u32 * scale,
        );
        if matches!(media.kind, MediaKind::Video { .. }) && url.starts_with(cdn::MEDIA_BASE) {
            // The proxy renders a still frame of a video on request.
            url.push_str("&format=webp");
        }

        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk::ContentFit::Cover);
        picture.set_size_request(-1, display_height);
        picture.add_css_class("kc-media");
        picture.set_overflow(gtk::Overflow::Hidden);
        if let Some(description) = media.description.filter(|text| !text.is_empty()) {
            picture.set_alternative_text(Some(description));
            picture.set_tooltip_text(Some(description));
        }

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&picture));
        match &media.kind {
            MediaKind::Video { .. } => {
                let play = gtk::Image::from_icon_name("media-playback-start-symbolic");
                play.set_pixel_size(48);
                play.add_css_class("osd");
                play.add_css_class("circular");
                play.set_halign(gtk::Align::Center);
                play.set_valign(gtk::Align::Center);
                play.set_can_target(false);
                overlay.add_overlay(&play);
            }
            MediaKind::Gif { .. } | MediaKind::AnimatedGif => {
                let tag = gtk::Label::new(Some("GIF"));
                tag.add_css_class("kc-media-tag");
                tag.set_halign(gtk::Align::Start);
                tag.set_valign(gtk::Align::Start);
                tag.set_margin_start(6);
                tag.set_margin_top(6);
                tag.set_can_target(false);
                overlay.add_overlay(&tag);
            }
            MediaKind::Image => {}
        }

        // A favourite star over a GIF, revealed on hover like the picker tiles.
        if let Some(target) = &media.favorite {
            let format = match &media.kind {
                MediaKind::Gif { .. } => kc_http::FAVORITE_GIF_FORMAT_VIDEO,
                _ => kc_http::FAVORITE_GIF_FORMAT_IMAGE,
            };
            let gif = kc_http::Gif {
                id: String::new(),
                url: target.url.to_owned(),
                src: target.url.to_owned(),
                gif_src: target.url.to_owned(),
                preview: String::new(),
                width: target.width,
                height: target.height,
                video: format == kc_http::FAVORITE_GIF_FORMAT_VIDEO,
            };
            overlay.add_overlay(&self.gif_star(&gif));
        }

        // Loading (and, for GIFs, playing) starts when a spoiler is revealed.
        let gif_video = match &media.kind {
            MediaKind::Gif { video_url } => Some(video_url.clone()),
            _ => None,
        };
        let animated = matches!(media.kind, MediaKind::AnimatedGif);
        // The resized media proxy can hand back a still (often WebP) frame for a
        // GIF, which cannot animate. So animate from the original file, which is
        // a true animated GIF; fall back to the (resized) still otherwise.
        let animate_url = if animated && !media.target.is_empty() {
            media.target.to_owned()
        } else {
            url.clone()
        };
        let start = {
            let app = self.clone();
            let picture = picture.clone();
            let still = url.clone();
            let animate_url = animate_url.clone();
            move || {
                if animated {
                    // Animate from the original GIF, exactly like the picker tiles
                    // (which do animate). A single-frame GIF shows as a still.
                    app.images.set_animated_picture(&picture, &animate_url);
                } else {
                    app.images.set_picture(&picture, &still);
                }
                if let Some(video_url) = &gif_video {
                    app.play_gif(&picture, video_url);
                }
            }
        };

        let hidden = Rc::new(std::cell::Cell::new(media.spoiler));
        if media.spoiler {
            let cover = gtk::Label::new(Some("SPOILER"));
            cover.add_css_class("osd");
            cover.add_css_class("heading");
            cover.set_halign(gtk::Align::Fill);
            cover.set_valign(gtk::Align::Fill);
            overlay.add_overlay(&cover);
            let hidden = hidden.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |gesture, _, _, _| {
                if hidden.replace(false) {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    if let Some(cover) = gesture.widget() {
                        cover.set_visible(false);
                    }
                    start();
                }
            });
            cover.add_controller(click);
        } else {
            start();
        }

        // What a click does: open the in-app image viewer (images and real
        // GIFs, which it animates), play an uploaded video in the in-app player,
        // open a webpage-backed video embed externally, or -- for an inline
        // `gifv`, an MP4 the viewer cannot animate -- nothing (it keeps playing
        // inline instead).
        enum OnClick {
            Nothing,
            Viewer(String),
            External(String),
            PlayVideo(String),
        }
        let action = match &media.kind {
            // A real GIF opens animated in the viewer, pointed at the original
            // GIF file (the resized proxy can be a still) rather than a page.
            MediaKind::AnimatedGif => OnClick::Viewer(animate_url.clone()),
            MediaKind::Image => OnClick::Viewer(media.target.to_owned()),
            MediaKind::Video {
                file_url: Some(file),
            } => OnClick::PlayVideo(file.clone()),
            MediaKind::Video { file_url: None } => OnClick::External(media.target.to_owned()),
            MediaKind::Gif { .. } => OnClick::Nothing,
        };
        // Right-click: open, copy the link or picture, save the file.
        {
            use crate::media_menu::{MediaLinks, MediaType};
            let links = match &media.kind {
                MediaKind::Image => MediaLinks {
                    link: media.target.to_owned(),
                    file: Some(media.target.to_owned()),
                    kind: MediaType::Image,
                },
                MediaKind::AnimatedGif => MediaLinks {
                    link: media.target.to_owned(),
                    file: Some(animate_url.clone()),
                    kind: MediaType::Gif,
                },
                MediaKind::Video { file_url } => MediaLinks {
                    link: media.target.to_owned(),
                    file: file_url.clone(),
                    kind: MediaType::Video,
                },
                // A GIF made of a video (Tenor, Klipy, gifv): saved as one.
                MediaKind::Gif { video_url } => MediaLinks {
                    link: media.target.to_owned(),
                    file: Some(video_url.clone()),
                    kind: MediaType::Video,
                },
            };
            self.attach_media_menu(&picture, links);
        }
        if !matches!(action, OnClick::Nothing) {
            let click = gtk::GestureClick::new();
            let app = self.clone();
            let preview_url = url.clone();
            click.connect_released(move |_, _, _, _| {
                if hidden.get() {
                    return;
                }
                match &action {
                    OnClick::Viewer(target) => app.show_image(target, &preview_url),
                    OnClick::External(target) => open_uri(Some(app.window.upcast_ref()), target),
                    OnClick::PlayVideo(file) => app.show_video(file, &preview_url),
                    OnClick::Nothing => {}
                }
            });
            picture.add_controller(click);
            picture.set_cursor_from_name(Some("pointer"));
        }

        max_width(&overlay, display_width).upcast()
    }

    /// Downloads a GIF's video and, once its first frame is decoded, shows it
    /// in place of the still.
    pub(crate) fn play_gif(self: &Rc<Self>, picture: &gtk::Picture, video_url: &str) {
        let app = Rc::downgrade(self);
        let target = picture.downgrade();
        self.images.file(video_url, "mp4", move |path| {
            let (Some(app), Some(picture), Some(path)) = (app.upgrade(), target.upgrade(), path)
            else {
                return;
            };
            let shown = picture.downgrade();
            let player = LoopingVideo::new(&path, move |paintable| {
                if let Some(picture) = shown.upgrade() {
                    picture.set_paintable(Some(&paintable));
                }
            });
            match player {
                Some(player) => app.register_player(picture.upcast_ref(), player),
                None => tracing::debug!("gif playback unavailable; showing the still"),
            }
        });
    }

    pub(crate) fn sticker_widget(self: &Rc<Self>, sticker: &StickerItem) -> gtk::Widget {
        match cdn::sticker(sticker.id, sticker.format_type) {
            Some(url) => {
                let picture = gtk::Picture::new();
                picture.set_can_shrink(true);
                picture.set_size_request(160, 160);
                picture.set_halign(gtk::Align::Start);
                picture.set_tooltip_text(Some(&sticker.name));
                self.images.set_picture(&picture, &url);
                max_width(&picture, 160).upcast()
            }
            // Lottie stickers are vector animations with no image form.
            None => label(&format!("Sticker: {}", sticker.name), &["dim-label"]).upcast(),
        }
    }

    /// A link preview or rich embed.
    pub(crate) fn embed_widget(
        self: &Rc<Self>,
        embed: &Embed,
        guild: Option<Snowflake>,
        state: &State,
    ) -> Option<gtk::Widget> {
        let kind = embed.kind.as_deref().unwrap_or("rich");
        let media_of = |media: &Option<EmbedMedia>| -> Option<(String, u32, u32)> {
            let media = media.as_ref()?;
            let url = media.proxy_url.clone().or_else(|| media.url.clone())?;
            Some((url, media.width.unwrap_or(400), media.height.unwrap_or(300)))
        };

        // Bare media links render as the media alone, like the official client.
        if matches!(kind, "image" | "gifv") {
            let (url, width, height) =
                media_of(&embed.thumbnail).or_else(|| media_of(&embed.image))?;
            // A posted link to a .gif (e.g. sent from the GIF picker's
            // favourites) arrives as an `image` embed whose proxy thumbnail is a
            // still. Animate it from the original GIF, like an attachment.
            let animated_original = (kind == "image")
                .then(|| {
                    [&embed.thumbnail, &embed.image]
                        .into_iter()
                        .flatten()
                        .find(|media| media.is_animated_gif())
                        .and_then(|media| media.url.clone())
                })
                .flatten();
            let target = animated_original
                .clone()
                .or_else(|| embed.url.clone())
                .unwrap_or_else(|| url.clone());
            // A gifv is a short looping video with a still thumbnail.
            let video = (kind == "gifv")
                .then(|| media_of(&embed.video))
                .flatten()
                .map(|(video_url, _, _)| video_url);
            // A gifv or animated GIF can be favourited, keyed by its original
            // (non-proxy) media URL -- the same key Discord's own client and the
            // picker use. Fall back to the video/image/thumbnail original, else
            // the proxy url.
            let favorite = (kind == "gifv" || animated_original.is_some()).then(|| {
                let original = embed
                    .video
                    .as_ref()
                    .and_then(|media| media.url.clone())
                    .or_else(|| embed.image.as_ref().and_then(|media| media.url.clone()))
                    .or_else(|| embed.thumbnail.as_ref().and_then(|media| media.url.clone()))
                    .unwrap_or_else(|| url.clone());
                (original, width, height)
            });
            return Some(
                self.media_preview(Media {
                    still: &url,
                    target: &target,
                    width,
                    height,
                    kind: match video {
                        Some(video_url) => MediaKind::Gif { video_url },
                        None if animated_original.is_some() => MediaKind::AnimatedGif,
                        None => MediaKind::Image,
                    },
                    spoiler: false,
                    description: None,
                    favorite: favorite
                        .as_ref()
                        .map(|(url, width, height)| FavoriteTarget {
                            url,
                            width: *width,
                            height: *height,
                        }),
                }),
            );
        }

        // A bare link to a video file arrives as a `video` embed carrying only
        // the video (no title, thumbnail or image): show the video, not an
        // empty card. From a GIF host it is a GIF -- play it inline, muted
        // and looping; anything else gets a play button for the player.
        if kind == "video"
            && embed.title.is_none()
            && embed.description.is_none()
            && embed.thumbnail.is_none()
            && embed.image.is_none()
            && let Some(video) = &embed.video
            && let Some(source) = video.url.clone().or_else(|| video.proxy_url.clone())
        {
            let (width, height) = (video.width.unwrap_or(400), video.height.unwrap_or(300));
            let gif = is_gif_host(&source);
            return Some(self.media_preview(Media {
                still: "",
                target: &source,
                width,
                height,
                kind: if gif {
                    MediaKind::Gif {
                        video_url: source.clone(),
                    }
                } else {
                    MediaKind::Video {
                        file_url: Some(source.clone()),
                    }
                },
                spoiler: false,
                description: None,
                favorite: gif.then_some(FavoriteTarget {
                    url: &source,
                    width,
                    height,
                }),
            }));
        }

        let card = gtk::Box::new(gtk::Orientation::Vertical, 4);
        card.add_css_class("kc-embed");
        if let Some(color) = embed.color.filter(|color| *color != 0) {
            let provider = gtk::CssProvider::new();
            provider.load_from_string(&format!(
                "box.kc-embed {{ border-left-color: {}; }}",
                hex_color(color)
            ));
            #[allow(deprecated)]
            card.style_context()
                .add_provider(&provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
        }

        let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
        body.set_hexpand(true);
        if let Some(provider) = embed
            .provider
            .as_ref()
            .and_then(|provider| provider.name.as_deref())
        {
            body.append(&label(provider, &["caption", "dim-label"]));
        }
        if let Some(author) = &embed.author {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            if let Some(icon) = author
                .proxy_icon_url
                .as_deref()
                .or(author.icon_url.as_deref())
            {
                row.append(&self.images.avatar(20, &author.name, Some(icon)));
            }
            row.append(&label(&author.name, &["caption-heading"]));
            body.append(&row);
        }
        if let Some(title) = embed.title.as_deref().filter(|title| !title.is_empty()) {
            let heading = gtk::Label::new(None);
            heading.set_xalign(0.0);
            heading.set_wrap(true);
            heading.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            let escaped = glib::markup_escape_text(title);
            match embed.url.as_deref() {
                Some(url) => heading.set_markup(&format!(
                    "<a href=\"{}\"><b>{escaped}</b></a>",
                    glib::markup_escape_text(url)
                )),
                None => heading.set_markup(&format!("<b>{escaped}</b>")),
            }
            let app = self.clone();
            heading.connect_activate_link(move |heading, uri| {
                app.activate_link(uri, heading.upcast_ref());
                glib::Propagation::Stop
            });
            body.append(&heading);
        }
        if let Some(description) = embed.description.as_deref()
            && let Some(text) = self.text_widget(description, guild, state)
        {
            body.append(&text);
        }
        for field in &embed.fields {
            let name = label(&field.name, &["caption-heading"]);
            name.set_margin_top(4);
            body.append(&name);
            if let Some(value) = self.text_widget(&field.value, guild, state) {
                body.append(&value);
            }
        }

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.append(&body);
        let large_thumbnail = matches!(kind, "video" | "article") || embed.image.is_some();
        if !large_thumbnail && let Some((url, width, height)) = media_of(&embed.thumbnail) {
            let side = 80.0 / f64::from(width.max(height).max(1));
            let (thumb_width, thumb_height) = (
                (f64::from(width) * side) as i32,
                (f64::from(height) * side) as i32,
            );
            let picture = gtk::Picture::new();
            picture.set_can_shrink(true);
            picture.set_size_request(thumb_width.max(16), thumb_height.max(16));
            picture.set_valign(gtk::Align::Start);
            picture.add_css_class("kc-media");
            picture.set_overflow(gtk::Overflow::Hidden);
            self.images
                .set_picture(&picture, &cdn::resized(&url, 160, 160));
            row.append(&picture);
        }
        card.append(&row);

        let big = media_of(&embed.image).or_else(|| {
            large_thumbnail
                .then(|| media_of(&embed.thumbnail))
                .flatten()
        });
        if let Some((url, width, height)) = big {
            let target = match kind {
                "video" => embed.url.clone().unwrap_or_else(|| url.clone()),
                _ => embed
                    .image
                    .as_ref()
                    .and_then(|image| image.url.clone())
                    .unwrap_or_else(|| url.clone()),
            };
            let preview = self.media_preview(Media {
                still: &url,
                target: &target,
                width,
                height,
                kind: if kind == "video" {
                    // A webpage-backed video embed (its target is a page, not a
                    // media file), so it opens externally.
                    MediaKind::Video { file_url: None }
                } else {
                    MediaKind::Image
                },
                spoiler: false,
                description: None,
                favorite: None,
            });
            preview.set_margin_top(4);
            card.append(&preview);
        }

        if let Some(footer) = &embed.footer {
            let mut text = footer.text.clone();
            if let Some(timestamp) = embed.timestamp.as_deref() {
                let time = format_time(timestamp);
                if !time.is_empty() {
                    text = if text.is_empty() {
                        time
                    } else {
                        format!("{text} • {time}")
                    };
                }
            }
            let footer_label = label(&text, &["caption", "dim-label"]);
            footer_label.set_margin_top(4);
            card.append(&footer_label);
        }

        // Nothing to show (e.g. an embed type with no renderable parts).
        card.first_child()?;
        Some(max_width(&card, EMBED_MAX_WIDTH).upcast())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_fits_inside_the_preview_box() {
        assert_eq!(fit(4000, 3000), (400, 300));
        assert_eq!(fit(100, 50), (100, 50));
        assert_eq!(fit(840, 100), (420, 50));
    }

    #[test]
    fn anchors_become_marked_spans() {
        let (markup, uris) = mark_anchors(
            "a <a href=\"https://x.y/?a=1&amp;b=2\">link</a> b <a href=\"kubecord://user/1\">@x</a>",
        );
        assert_eq!(
            markup,
            "a <span underline=\"single\">\u{e000}link\u{e001}</span> b <span>\u{e000}@x\u{e001}</span>"
        );
        assert_eq!(uris, ["https://x.y/?a=1&b=2", "kubecord://user/1"]);
        assert_eq!(mark_anchors("plain <b>bold</b>").0, "plain <b>bold</b>");
        // Markers typed into a message cannot fake a link.
        assert_eq!(mark_anchors("x\u{e000}y").0, "xy");
    }

    #[test]
    fn clock_is_twelve_hour_unless_asked() {
        assert_eq!(clock_text(17, 58, None, false), "5:58 PM");
        assert_eq!(clock_text(0, 5, None, false), "12:05 AM");
        assert_eq!(clock_text(12, 0, None, false), "12:00 PM");
        assert_eq!(clock_text(9, 7, Some(3), false), "9:07:03 AM");
        assert_eq!(clock_text(17, 58, None, true), "17:58");
        assert_eq!(clock_text(0, 5, Some(9), true), "00:05:09");
    }

    #[test]
    fn discord_timestamps_follow_the_clock_setting() {
        let state = State::default();
        let resolver = StateResolver {
            state: &state,
            guild: None,
        };
        let unix = 1_700_000_000;
        let local = glib::DateTime::from_unix_local(unix).unwrap();
        let (hour, minute) = (local.hour() as u32, local.minute() as u32);
        use rich::Resolver;
        set_24_hour(false);
        assert_eq!(
            resolver.timestamp(unix, 't'),
            clock_text(hour, minute, None, false)
        );
        assert!(
            resolver
                .timestamp(unix, 'f')
                .ends_with(&clock_text(hour, minute, None, false))
        );
        set_24_hour(true);
        assert_eq!(
            resolver.timestamp(unix, 't'),
            clock_text(hour, minute, None, true)
        );
        set_24_hour(false);
    }
}

#[cfg(test)]
mod link_media_tests {
    use super::*;

    #[test]
    fn discord_media_links_are_found_and_classified() {
        let text = "look https://cdn.discordapp.com/attachments/1/2/wumpus.gif?ex=6b&is=6b&hm=ab and \
                    https://media.discordapp.net/attachments/3/4/a.PNG plus https://example.com/x.gif \
                    and https://cdn.discordapp.com/attachments/5/6/notes.txt";
        let links = discord_media_links(text);
        assert_eq!(links.len(), 2, "{links:?}");
        assert_eq!(link_media_kind(&links[0]), Some(LinkMedia::Gif));
        assert_eq!(link_media_kind(&links[1]), Some(LinkMedia::Image));
        assert_eq!(
            link_media_kind("https://cdn.discordapp.com/attachments/1/2/clip.mp4?ex=1"),
            Some(LinkMedia::Video)
        );
        assert!(is_gif_host("https://static.klipy.com/ii/a/d1/60/x.mp4"));
        assert!(is_gif_host("https://media.tenor.com/x.webm"));
        assert!(!is_gif_host("https://example.com/klipy.com/x.mp4"));
        assert!(!is_gif_host("https://notklipy.com/x.mp4"));
        // An embed's url has no signature, and may use the other media host.
        assert!(same_link(
            "https://cdn.discordapp.com/attachments/1/2/w.gif?ex=1&is=2&hm=3",
            "https://media.discordapp.net/attachments/1/2/w.gif"
        ));
        assert!(!same_link(
            "https://cdn.discordapp.com/attachments/1/2/w.gif",
            "https://cdn.discordapp.com/attachments/1/9/w.gif"
        ));
    }
}
