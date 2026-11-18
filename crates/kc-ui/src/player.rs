//! Inline playback of GIF-style videos (Discord's `gifv` embeds, which
//! providers such as Tenor and Klipy serve as short MP4s): muted, looping,
//! and running only while visible in a focused window.

use std::cell::Cell;
use std::path::Path;
use std::rc::{Rc, Weak};

use gstreamer as gst;
use gstreamer::prelude::*;
use gtk::gdk;
use gtk::prelude::*;

use crate::shell::AppState;

/// A muted, looping playbin that renders into a `GdkPaintable`.
pub(crate) struct LoopingVideo {
    pipeline: gst::Element,
    _watch: gst::bus::BusWatchGuard,
    playing: Cell<bool>,
    failed: Rc<Cell<bool>>,
}

impl LoopingVideo {
    /// Builds a paused pipeline for a local video file. `on_ready` runs once,
    /// on the GTK thread, when the first frame has been decoded; until then
    /// the paintable is empty, so callers keep showing a still.
    pub(crate) fn new(
        path: &Path,
        on_ready: impl FnOnce(gdk::Paintable) + 'static,
    ) -> Option<Rc<Self>> {
        let sink = gst::ElementFactory::make("gtk4paintablesink")
            .build()
            .ok()?;
        let paintable = sink.property::<gdk::Paintable>("paintable");
        let uri = glib::filename_to_uri(path, None).ok()?;
        let pipeline = gst::ElementFactory::make("playbin")
            .property("uri", uri.as_str())
            .property("video-sink", &sink)
            .property("mute", true)
            .build()
            .ok()?;
        // Video only: no audio decoding and no subtitle parsing.
        pipeline.set_property_from_str("flags", "video");

        let failed = Rc::new(Cell::new(false));
        let weak = pipeline.downgrade();
        let mut on_ready = Some(on_ready);
        let failed_flag = failed.clone();
        let watch = pipeline
            .bus()?
            .add_watch_local(move |_, message| {
                let Some(pipeline) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                match message.view() {
                    gst::MessageView::AsyncDone(_) => {
                        if let Some(ready) = on_ready.take() {
                            tracing::debug!("gif first frame ready");
                            ready(paintable.clone());
                        }
                    }
                    gst::MessageView::Eos(_) => {
                        // Loop: back to the start without leaving PLAYING.
                        let _ = pipeline.seek_simple(
                            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                            gst::ClockTime::ZERO,
                        );
                    }
                    gst::MessageView::Error(error) => {
                        tracing::debug!(error = %error.error(), "gif playback failed");
                        failed_flag.set(true);
                        let _ = pipeline.set_state(gst::State::Null);
                    }
                    _ => {}
                }
                glib::ControlFlow::Continue
            })
            .ok()?;

        // Preroll so the first frame is ready before the video is shown.
        pipeline.set_state(gst::State::Paused).ok()?;
        Some(Rc::new(Self {
            pipeline,
            _watch: watch,
            playing: Cell::new(false),
            failed,
        }))
    }

    fn set_playing(&self, playing: bool) {
        if self.failed.get() || self.playing.replace(playing) == playing {
            return;
        }
        let state = if playing {
            gst::State::Playing
        } else {
            gst::State::Paused
        };
        tracing::debug!(playing, "gif playback state");
        let _ = self.pipeline.set_state(state);
    }
}

impl Drop for LoopingVideo {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Whether `widget` intersects the viewport of the scrolled window it is in.
/// Widgets outside any scrolled window count as visible while mapped.
fn on_screen(widget: &gtk::Widget) -> bool {
    if !widget.is_mapped() {
        return false;
    }
    let Some(scroller) = widget.ancestor(gtk::ScrolledWindow::static_type()) else {
        return true;
    };
    let Some(bounds) = widget.compute_bounds(&scroller) else {
        return false;
    };
    bounds.y() + bounds.height() > 0.0
        && bounds.y() < scroller.height() as f32
        && bounds.x() + bounds.width() > 0.0
        && bounds.x() < scroller.width() as f32
}

impl AppState {
    /// Starts managing a player shown in `widget`. The widget keeps the
    /// player alive; when the widget is gone, so is the pipeline.
    pub(crate) fn register_player(self: &Rc<Self>, widget: &gtk::Widget, player: Rc<LoopingVideo>) {
        self.players
            .borrow_mut()
            .push((widget.downgrade(), Rc::downgrade(&player)));
        // The handler owns the strong reference for the widget's lifetime.
        widget.connect_unrealize(move |_| {
            let _ = &player;
        });
        let app = Rc::downgrade(self);
        widget.connect_map(move |_| queue_update(&app));
        let app = Rc::downgrade(self);
        widget.connect_unmap(move |_| queue_update(&app));
        self.queue_players_update();
    }

    /// Re-evaluates which players should run, on the next idle.
    pub(crate) fn queue_players_update(self: &Rc<Self>) {
        queue_update(&Rc::downgrade(self));
    }

    fn update_players(&self) {
        let active = self.window.is_active();
        self.players.borrow_mut().retain(|(widget, player)| {
            let (Some(widget), Some(player)) = (widget.upgrade(), player.upgrade()) else {
                return false;
            };
            player.set_playing(active && on_screen(&widget));
            true
        });
    }
}

fn queue_update(app: &Weak<AppState>) {
    let Some(app) = app.upgrade() else {
        return;
    };
    if app.players_update_queued.replace(true) {
        return;
    }
    let weak = Rc::downgrade(&app);
    // Layout has to settle before positions mean anything.
    glib::idle_add_local_once(move || {
        if let Some(app) = weak.upgrade() {
            app.players_update_queued.set(false);
            app.update_players();
        }
    });
}
