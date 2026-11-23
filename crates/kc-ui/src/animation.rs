//! Animated GIF playback driven by GdkPixbuf.
//!
//! Real animated `.gif` files (message attachments the proxy keeps as GIFs,
//! picker previews, and the image viewer) are decoded with
//! [`gdk_pixbuf::PixbufAnimation`] and played by advancing the animation's
//! iterator on a `glib::timeout`, setting the target `gtk::Picture`'s
//! paintable to a texture built from the current frame. Discord's `gifv`
//! embeds are short MP4s and keep using the GStreamer path in `player.rs`;
//! this module is only for images whose bytes are an animated GIF.
//!
//! A still (single-frame) GIF is handled by the ordinary texture path in
//! `images.rs`; this module reports `None` for those so the caller falls
//! back. The timer stops while the picture is unmapped (offscreen gifs do
//! not burn CPU) and is dropped with the picture.
//!
//! The live `PixbufAnimationIter` is used directly rather than pre-decoding
//! every frame: GdkPixbuf gives no reliable frame count (its
//! `on_currently_loading_frame` flags the last frame of a finished animation
//! too), so advancing against wall-clock time is the dependable way to walk a
//! looping GIF.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use gtk::gdk;
use gtk::gdk_pixbuf::{Pixbuf, PixbufAnimation, PixbufAnimationIter};
use gtk::glib;
use gtk::prelude::*;

/// The shortest a frame is shown. Many GIFs declare a 0 ms (or absurdly
/// small) delay; browsers clamp to about this, so a "0 ms" GIF animates at a
/// sane rate instead of spinning the timer.
const MIN_DELAY: Duration = Duration::from_millis(20);

/// Whether the window is focused. GIFs pause when it is not, to save work.
static WINDOW_FOCUSED: AtomicBool = AtomicBool::new(true);
/// Whether to keep animating even when the window is unfocused (a setting,
/// off by default).
static ANIMATE_UNFOCUSED: AtomicBool = AtomicBool::new(false);

/// Whether animated GIFs should be running right now.
fn awake() -> bool {
    WINDOW_FOCUSED.load(Ordering::Relaxed) || ANIMATE_UNFOCUSED.load(Ordering::Relaxed)
}

thread_local! {
    /// Every live GIF player, so a focus or setting change can pause or
    /// resume them all at once.
    static PLAYERS: RefCell<Vec<Weak<GifPlayer>>> = const { RefCell::new(Vec::new()) };
}

/// Re-evaluates every GIF player against the current awake state, pruning
/// dropped ones.
fn refresh_all() {
    let live: Vec<Rc<GifPlayer>> = PLAYERS.with_borrow_mut(|players| {
        players.retain(|weak| weak.upgrade().is_some());
        players.iter().filter_map(Weak::upgrade).collect()
    });
    for player in live {
        player.refresh();
    }
}

/// Whether any of the app's windows has the focus (the image viewer and the
/// other pop-out windows count: a GIF opened in the viewer plays there).
pub(crate) fn app_focused() -> bool {
    gtk::Window::list_toplevels()
        .into_iter()
        .filter_map(|window| window.downcast::<gtk::Window>().ok())
        .any(|window| window.is_active())
}

/// Re-checks every GIF when the scrolled window holding `picture` scrolls
/// (once per scrolled window), so one scrolled into view starts and one
/// scrolled away stops.
fn watch_scroller(picture: &gtk::Picture) {
    let Some(scroller) = picture
        .ancestor(gtk::ScrolledWindow::static_type())
        .and_downcast::<gtk::ScrolledWindow>()
    else {
        return;
    };
    if scroller.has_css_class("kc-watched-gifs") {
        return;
    }
    scroller.add_css_class("kc-watched-gifs");
    for adjustment in [scroller.vadjustment(), scroller.hadjustment()] {
        adjustment.connect_value_changed(|_| queue_refresh());
        adjustment.connect_changed(|_| queue_refresh());
    }
}

/// [`refresh_all`] once the next frame is laid out, once however many ask.
/// Positions mean something only after layout: a GIF that just arrived (or
/// whose row just grew) measured straight away can look off screen, and
/// stayed paused until something else (a channel switch) asked again. So it
/// waits for the next frame and measures on the idle after it, like the
/// looping videos (see `player::queue_update`).
fn queue_refresh() {
    thread_local! {
        static QUEUED: Cell<bool> = const { Cell::new(false) };
    }
    if QUEUED.replace(true) {
        return;
    }
    // Measures once, by whichever of the frame or the fallback timer comes
    // first.
    let measure = || {
        glib::idle_add_local_once(|| {
            if QUEUED.replace(false) {
                refresh_all();
            }
        });
    };
    // The frame of the window a shown GIF is in (a window always draws its
    // frames; a picture may not -- one not laid out yet, or gone before its
    // frame, never ticked, which left this queued for good and every GIF
    // after it frozen until clicked).
    let window = PLAYERS.with_borrow(|players| {
        players
            .iter()
            .filter_map(Weak::upgrade)
            .filter_map(|player| player.picture.upgrade())
            .filter(|picture| picture.is_mapped())
            .find_map(|picture| picture.root().and_downcast::<gtk::Window>())
    });
    if let Some(window) = window.filter(|window| window.is_mapped()) {
        window.add_tick_callback(move |_, _| {
            measure();
            glib::ControlFlow::Break
        });
    }
    // And in case no frame comes (a hidden or minimised window draws none).
    glib::timeout_add_local_once(std::time::Duration::from_millis(200), measure);
}

/// Called when one of the app's windows gains or loses focus; pauses or
/// resumes GIFs unless [`set_animate_unfocused`] keeps them running.
pub(crate) fn set_window_focused(focused: bool) {
    if WINDOW_FOCUSED.swap(focused, Ordering::Relaxed) != focused {
        refresh_all();
        // Discord's heartbeats and request headers say whether the user is
        // at the app.
        kc_core::super_properties::set_app_focused(focused);
        if let Ok(gateways) = crate::session::GATEWAYS.lock() {
            for gateway in gateways.values() {
                gateway.set_focused(focused);
            }
        }
    }
}

/// Whether one of the app's windows had the focus when last checked.
pub(crate) fn window_focused() -> bool {
    WINDOW_FOCUSED.load(Ordering::Relaxed)
}

/// Sets whether GIFs animate while the window is unfocused.
pub(crate) fn set_animate_unfocused(on: bool) {
    if ANIMATE_UNFOCUSED.swap(on, Ordering::Relaxed) != on {
        refresh_all();
    }
}

/// Whether GIF-style media should animate while the window is unfocused.
pub(crate) fn animate_unfocused() -> bool {
    ANIMATE_UNFOCUSED.load(Ordering::Relaxed)
}

/// Decodes `bytes` as a GIF animation. Returns `None` if the bytes are not a
/// multi-frame animated GIF (not a GIF at all, a single still frame, or
/// undecodable), so the caller can fall back to a single texture.
pub(crate) fn decode(bytes: &[u8]) -> Option<PixbufAnimation> {
    let stream = gtk::gio::MemoryInputStream::from_bytes(&glib::Bytes::from(bytes));
    let animation = PixbufAnimation::from_stream(&stream, gtk::gio::Cancellable::NONE).ok()?;
    (!animation.is_static_image()).then_some(animation)
}

/// Whether `bytes` begins with the GIF signature (`GIF87a` / `GIF89a`).
pub(crate) fn is_gif(bytes: &[u8]) -> bool {
    bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a")
}

/// Builds a texture from one decoded frame.
pub(crate) fn pixbuf_texture(pixbuf: &Pixbuf) -> Option<gdk::Texture> {
    let width = pixbuf.width();
    let height = pixbuf.height();
    if width <= 0 || height <= 0 {
        return None;
    }
    let format = if pixbuf.has_alpha() {
        gdk::MemoryFormat::R8g8b8a8
    } else {
        gdk::MemoryFormat::R8g8b8
    };
    let rowstride = usize::try_from(pixbuf.rowstride()).ok()?;
    let bytes = pixbuf.read_pixel_bytes();
    Some(gdk::MemoryTexture::new(width, height, format, &bytes, rowstride).upcast())
}

/// How long the frame at this iterator position is shown, clamped so a
/// zero/near-zero delay does not spin the timer.
fn frame_delay(iter: &PixbufAnimationIter) -> Duration {
    iter.delay_time().unwrap_or(MIN_DELAY).max(MIN_DELAY)
}

/// Drives an animated GIF into a `gtk::Picture`. Holds the animation and its
/// live iterator, advancing on a timer; the picture keeps the player alive
/// (see [`attach`](Self::attach)).
pub(crate) struct GifPlayer {
    picture: glib::WeakRef<gtk::Picture>,
    iter: PixbufAnimationIter,
    /// Virtual clock, advanced by each frame's delay. Using a fixed epoch
    /// start keeps playback independent of the wall clock (and testable).
    clock: Cell<SystemTime>,
    source: RefCell<Option<glib::SourceId>>,
    /// Whether the picture is currently mapped; the timer only runs then.
    running: Cell<bool>,
}

impl GifPlayer {
    /// Plays `animation` in `picture`. The picture owns the player for its
    /// lifetime, so the timer stops and the animation is freed when the
    /// picture is gone. Does nothing (returns `None`) for a still image.
    pub(crate) fn attach(picture: &gtk::Picture, animation: &PixbufAnimation) -> Option<Rc<Self>> {
        if animation.is_static_image() {
            if let Some(texture) = pixbuf_texture(&animation.static_image()?) {
                picture.set_paintable(Some(&texture));
            }
            return None;
        }
        let start = SystemTime::UNIX_EPOCH;
        let iter = animation.iter(Some(start));
        // Show the first frame right away.
        if let Some(texture) = pixbuf_texture(&iter.pixbuf()) {
            picture.set_paintable(Some(&texture));
        }
        let player = Rc::new(Self {
            picture: picture.downgrade(),
            iter,
            clock: Cell::new(start),
            source: RefCell::new(None),
            running: Cell::new(false),
        });

        // So a focus or setting change can pause/resume it.
        PLAYERS.with_borrow_mut(|players| players.push(Rc::downgrade(&player)));

        // Run only while mapped and awake: an offscreen gif (collapsed,
        // scrolled away, in a closed popover) or an unfocused window (unless
        // the setting keeps it going) stops its timer.
        let weak = Rc::downgrade(&player);
        picture.connect_map(move |_| {
            if let Some(player) = weak.upgrade() {
                player.refresh();
            }
            queue_refresh();
        });
        let weak = Rc::downgrade(&player);
        picture.connect_unmap(move |_| {
            if let Some(player) = weak.upgrade() {
                player.stop();
            }
        });
        // The picture holds the only strong reference; dropping it stops the
        // timer through `Drop`.
        let keep = player.clone();
        picture.connect_destroy(move |_| {
            keep.stop();
        });
        watch_scroller(picture);
        player.refresh();
        // And again once laid out (just added, it may not be placed yet).
        queue_refresh();
        Some(player)
    }

    /// Runs the timer when the picture is on screen (mapped, and inside its
    /// scrolled window's view) and GIFs are awake (window focused, or the
    /// unfocused-animation setting is on); otherwise stops it.
    fn refresh(self: &Rc<Self>) {
        let mapped = self
            .picture
            .upgrade()
            .is_some_and(|picture| crate::player::on_screen(picture.upcast_ref()));
        if mapped && awake() {
            self.start();
        } else {
            tracing::debug!(mapped, awake = awake(), "gif player waiting");
            self.stop();
        }
    }

    fn start(self: &Rc<Self>) {
        if self.running.replace(true) {
            return;
        }
        tracing::debug!("gif player started");
        self.schedule_next();
    }

    fn stop(&self) {
        if self.running.replace(false) {
            tracing::debug!("gif player stopped");
        }
        if let Some(source) = self.source.borrow_mut().take() {
            source.remove();
        }
    }

    fn schedule_next(self: &Rc<Self>) {
        let delay = frame_delay(&self.iter);
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(delay, move || {
            if let Some(player) = weak.upgrade() {
                player.tick();
            }
        });
        *self.source.borrow_mut() = Some(source);
    }

    fn tick(self: &Rc<Self>) {
        if !self.running.get() {
            return;
        }
        let Some(picture) = self.picture.upgrade() else {
            self.stop();
            return;
        };
        // Advance the virtual clock past this frame and move the iterator to
        // whatever should now be shown (it wraps on its own for a looping
        // GIF). Repaint only when the displayed frame actually changed.
        let next = self.clock.get() + frame_delay(&self.iter) + Duration::from_millis(1);
        self.clock.set(next);
        if self.iter.advance(next)
            && let Some(texture) = pixbuf_texture(&self.iter.pixbuf())
        {
            picture.set_paintable(Some(&texture));
        }
        // Stop if the window went unfocused or the picture scrolled out of
        // view since the last frame; a focus change, a setting change or a
        // scroll resumes it via `refresh`.
        if awake() && crate::player::on_screen(picture.upcast_ref()) {
            self.schedule_next();
        } else {
            self.stop();
        }
    }
}

impl Drop for GifPlayer {
    fn drop(&mut self) {
        if let Some(source) = self.source.borrow_mut().take() {
            source.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny two-frame animated GIF (a row of red, then a row of green),
    /// built by hand so the test needs no fixture. Each frame is 3x1 pixels
    /// of one colour from a two-entry global colour table, with a Graphic
    /// Control Extension carrying a 100 ms delay. This was verified to decode
    /// with `gdk-pixbuf`: the three details that matter are (1) the GCE is a
    /// full eight bytes ending in a block terminator, (2) the LZW code width
    /// steps up to four bits once the dictionary reaches eight entries, which
    /// happens here after the third literal, so the end code is written at
    /// four bits, and (3) a `NETSCAPE2.0` application extension marks it as
    /// looping.
    fn sample_gif() -> Vec<u8> {
        let mut gif = Vec::new();
        gif.extend_from_slice(b"GIF89a");
        // Logical screen descriptor: 3x1, global colour table flag set, a
        // 2-entry table (size field 0 → 2^(0+1) = 2 colours).
        gif.extend_from_slice(&[3, 0, 1, 0, 0b1000_0000, 0, 0]);
        // Global colour table: red, green.
        gif.extend_from_slice(&[255, 0, 0, 0, 255, 0]);
        // Application extension: NETSCAPE2.0, loop forever.
        gif.extend_from_slice(&[0x21, 0xFF, 0x0B]);
        gif.extend_from_slice(b"NETSCAPE2.0");
        gif.extend_from_slice(&[0x03, 0x01, 0x00, 0x00, 0x00]);
        for colour in [0u8, 1u8] {
            // Graphic Control Extension (8 bytes): introducer, label, block
            // size 4, packed flags, delay low/high (10 → 100 ms), transparent
            // colour index, block terminator.
            gif.extend_from_slice(&[0x21, 0xF9, 0x04, 0x00, 10, 0x00, 0x00, 0x00]);
            // Image descriptor at (0,0), 3x1, no local colour table.
            gif.extend_from_slice(&[0x2C, 0, 0, 0, 0, 3, 0, 1, 0, 0x00]);
            // LZW minimum code size 2, then one sub-block of the data.
            let (lzw, len) = lzw_row(colour);
            gif.push(0x02);
            gif.push(len);
            gif.extend_from_slice(&lzw[..len as usize]);
            gif.push(0x00); // Block terminator.
        }
        gif.push(0x3B); // Trailer.
        gif
    }

    /// LZW-encodes a 3-pixel row of colour index 0 or 1 at min code size 2:
    /// clear code (4) then the index three times at 3 bits, then the end code
    /// (5) at 4 bits (the width steps up once the dictionary reaches eight
    /// entries, after the third literal). Packed LSB-first.
    fn lzw_row(colour: u8) -> ([u8; 4], u8) {
        let colour = colour as u16;
        let codes = [(4u16, 3u32), (colour, 3), (colour, 3), (colour, 3), (5, 4)];
        let mut out = [0u8; 4];
        let mut bitpos = 0usize;
        for (code, width) in codes {
            for bit in 0..width {
                if code & (1 << bit) != 0 {
                    out[bitpos / 8] |= 1 << (bitpos % 8);
                }
                bitpos += 1;
            }
        }
        (out, bitpos.div_ceil(8) as u8)
    }

    #[test]
    fn gif_signature_is_recognised() {
        assert!(is_gif(b"GIF89a....."));
        assert!(is_gif(b"GIF87a....."));
        assert!(!is_gif(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_gif(b"GIF"));
    }

    #[gtk::test]
    fn multi_frame_gif_decodes_and_advances_with_delays() {
        let bytes = sample_gif();
        assert!(is_gif(&bytes));
        let animation = decode(&bytes).expect("animated gif decodes");
        assert!(!animation.is_static_image());

        // Walk the iterator against a virtual clock and confirm it shows more
        // than one distinct frame, each with its declared ~100 ms delay.
        let start = SystemTime::UNIX_EPOCH;
        let iter = animation.iter(Some(start));
        let first = pixbuf_texture(&iter.pixbuf()).expect("first frame decodes");
        assert_eq!((first.width(), first.height()), (3, 1));
        let first_delay = frame_delay(&iter);
        assert!(first_delay >= Duration::from_millis(100));

        // Advance past the first frame; the shown frame must change.
        let next = start + first_delay + Duration::from_millis(1);
        assert!(iter.advance(next), "iterator advances to the second frame");
        let second = pixbuf_texture(&iter.pixbuf()).expect("second frame decodes");
        assert_eq!((second.width(), second.height()), (3, 1));
        // The two frames differ (red row vs green row).
        assert_ne!(
            first.save_to_png_bytes(),
            second.save_to_png_bytes(),
            "the two frames should be different images"
        );
        assert!(frame_delay(&iter) >= Duration::from_millis(20));
    }
}
