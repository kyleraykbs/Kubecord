//! A shared cache of the user's favourite GIFs, used by both the GIF picker
//! (`composer.rs`) and the star on GIF messages in chat (`content.rs`).
//!
//! Discord keeps favourites in the account's `FrecencyUserSettings` proto;
//! `HttpClient::favorite_gifs` reads them and `set_favorite_gif` writes them.
//! This cache holds the last-read list so the star shows the right state and
//! toggling updates it without a round-trip. It is thread-local because the
//! whole UI runs on the GTK main thread.
//!
//! Favourites are matched by URL: Discord's map key is the GIF's media URL,
//! which the picker stores as both `url` and `src`. A chat GIF's URL differs
//! from the picker's only by CDN/proxy wrapping, so a favourite is considered
//! present if either its `url` or `src` matches the queried url or src.

use gtk::prelude::*;
use kc_http::Gif;

/// The filled-star icon shown when a GIF is favourited.
const STAR_FILLED: &[&str] = &["starred-symbolic", "star-filled", "star-new-symbolic"];
/// The outline-star icon shown when a GIF is not favourited.
const STAR_OUTLINE: &[&str] = &["non-starred-symbolic", "star-outline", "star-new-symbolic"];

/// Points `star` at the filled or outline star icon and updates its tooltip.
pub(crate) fn set_star_icon(star: &gtk::Button, favorited: bool) {
    let icon = if favorited {
        crate::widgets::first_icon(STAR_FILLED)
    } else {
        crate::widgets::first_icon(STAR_OUTLINE)
    };
    if crate::widgets::icon_available(icon) {
        star.set_icon_name(icon);
    } else {
        star.set_label(if favorited { "★" } else { "☆" });
    }
    star.set_tooltip_text(Some(if favorited {
        "Remove from favourites"
    } else {
        "Add to favourites"
    }));
    // A favourited star stays faintly visible even without hover.
    if favorited {
        star.add_css_class("kc-favorited");
    } else {
        star.remove_css_class("kc-favorited");
    }
}

thread_local! {
    /// The favourites last fetched, or `None` if never fetched this run.
    static FAVORITES: std::cell::RefCell<Option<Vec<Gif>>> =
        const { std::cell::RefCell::new(None) };
}

/// Whether the favourites have been fetched at least once this run.
pub(crate) fn is_loaded() -> bool {
    FAVORITES.with_borrow(|cache| cache.is_some())
}

/// Replaces the whole cache (after a fresh fetch).
pub(crate) fn set_all(gifs: Vec<Gif>) {
    FAVORITES.with_borrow_mut(|cache| *cache = Some(gifs));
}

/// Forgets the cache, so the next read re-fetches.
pub(crate) fn clear() {
    FAVORITES.with_borrow_mut(|cache| *cache = None);
}

/// Reads the cache under a closure, avoiding a clone.
pub(crate) fn with<R>(read: impl FnOnce(Option<&[Gif]>) -> R) -> R {
    FAVORITES.with_borrow(|cache| read(cache.as_deref()))
}

/// Whether a favourite matching `url`/`src` is cached. An empty argument never
/// matches. Compares against each cached favourite's `url` and `src`.
pub(crate) fn is_favorited(url: &str, src: &str) -> bool {
    FAVORITES.with_borrow(|cache| {
        cache.as_deref().is_some_and(|gifs| {
            gifs.iter()
                .any(|gif| matches(gif, url) || matches(gif, src))
        })
    })
}

/// Whether `gif` is stored under `needle` (its `url` or `src`).
fn matches(gif: &Gif, needle: &str) -> bool {
    !needle.is_empty() && (gif.url == needle || gif.src == needle)
}

/// Records a newly favourited GIF in the cache (no-op if not yet loaded, so
/// the next fetch is authoritative). Does not duplicate an existing entry.
pub(crate) fn insert(gif: Gif) {
    FAVORITES.with_borrow_mut(|cache| {
        if let Some(gifs) = cache.as_mut()
            && !gifs
                .iter()
                .any(|existing| existing.url == gif.url && !gif.url.is_empty())
        {
            gifs.push(gif);
        }
    });
}

/// Removes the favourite matching `url`/`src` from the cache.
pub(crate) fn remove(url: &str, src: &str) {
    FAVORITES.with_borrow_mut(|cache| {
        if let Some(gifs) = cache.as_mut() {
            gifs.retain(|gif| !(matches(gif, url) || matches(gif, src)));
        }
    });
}
