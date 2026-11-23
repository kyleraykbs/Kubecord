//! The image viewer: a separate, non-modal window per image (the main
//! window stays usable), opened fitted to the window. Zoom with the scroll
//! wheel (towards the pointer), a pinch, `+` / `-` / `0`, or the header
//! buttons; double-click switches between fitted and 100 %; drag to pan
//! when the image is larger than the window. Escape closes it.

use std::cell::Cell;
use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;

use crate::shell::AppState;
use crate::widgets::{icon_button, open_uri};

/// The largest zoom, as a multiple of the image's own size.
pub(crate) const MAX_ZOOM: f64 = 8.0;
/// One wheel notch or key press zooms by this factor.
const STEP: f64 = 1.25;

/// Whether a URL points at a GIF (its path, before any query string, ends
/// in `.gif`), so the viewer should animate it.
fn is_gif_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.to_lowercase().ends_with(".gif")
}

/// The scale that fits an image into a viewport without enlarging it.
pub(crate) fn fit_scale(image: (f64, f64), viewport: (f64, f64)) -> f64 {
    if image.0 <= 0.0 || image.1 <= 0.0 || viewport.0 <= 0.0 || viewport.1 <= 0.0 {
        return 1.0;
    }
    (viewport.0 / image.0).min(viewport.1 / image.1).min(1.0)
}

/// Where the image sits along one axis: wholly inside the viewport when
/// it is smaller, else covering it (no gap at either edge).
fn clamp_origin(origin: f64, size: f64, viewport: f64) -> f64 {
    if size <= viewport {
        origin.clamp(0.0, viewport - size)
    } else {
        origin.clamp(viewport - size, 0.0)
    }
}

/// Centred along one axis.
fn centred(size: f64, viewport: f64) -> f64 {
    (viewport - size) / 2.0
}

/// What the viewer shows: the scale, and where the image's top-left corner
/// is in the viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct View {
    pub scale: f64,
    pub origin: (f64, f64),
}

/// The image fitted to the viewport (never enlarged), centred.
pub(crate) fn fitted_view(image: (f64, f64), viewport: (f64, f64)) -> View {
    let scale = fit_scale(image, viewport);
    View {
        scale,
        origin: (centred(image.0 * scale, viewport.0), centred(image.1 * scale, viewport.1)),
    }
}

/// The view at `scale` (kept between the fitted scale and [`MAX_ZOOM`])
/// with the image point under `point` (viewport coordinates) still under
/// it, as far as the edges allow: a smaller image stays inside the window
/// (not pulled back to its centre), a larger one covers it.
pub(crate) fn zoom_view(image: (f64, f64), viewport: (f64, f64), current: View, point: (f64, f64), scale: f64) -> View {
    let scale = scale.clamp(fit_scale(image, viewport), MAX_ZOOM);
    let axis = |image: f64, viewport: f64, origin: f64, point: f64| {
        let under = (point - origin) / current.scale;
        clamp_origin(point - under * scale, image * scale, viewport)
    };
    View {
        scale,
        origin: (
            axis(image.0, viewport.0, current.origin.0, point.0),
            axis(image.1, viewport.1, current.origin.1, point.1),
        ),
    }
}

/// The view at `scale` with the image point under `point` exactly where
/// it was, edges or not: an animated zoom follows the pointer this way, and
/// the image settles back within the edges once it ends.
pub(crate) fn zoom_view_exact(current: View, point: (f64, f64), scale: f64) -> View {
    let axis = |origin: f64, point: f64| point - (point - origin) / current.scale * scale;
    View {
        scale,
        origin: (axis(current.origin.0, point.0), axis(current.origin.1, point.1)),
    }
}

/// `start` moved by a drag of `delta`, within the edges.
pub(crate) fn pan_view(image: (f64, f64), viewport: (f64, f64), start: View, delta: (f64, f64)) -> View {
    View {
        scale: start.scale,
        origin: (
            clamp_origin(start.origin.0 + delta.0, image.0 * start.scale, viewport.0),
            clamp_origin(start.origin.1 + delta.1, image.1 * start.scale, viewport.1),
        ),
    }
}

/// How quickly an animated zoom closes on its target: the share of the way
/// left that is covered every 1/60 s.
const EASE: f64 = 0.3;

/// One open viewer.
///
/// The picture is placed in a [`gtk::Fixed`] at the view's position and
/// size, both set in the same frame. (Zooming a picture inside a scrolled
/// window changed its size one frame and the scroll offset the next, so the
/// image jumped while zooming.)
struct Viewer {
    picture: gtk::Picture,
    fixed: gtk::Fixed,
    /// The viewport (it clips the picture; nothing in it scrolls).
    scroll: gtk::ScrolledWindow,
    /// Fitted to the window (follows its size) rather than a fixed scale.
    fitted: Cell<bool>,
    view: Cell<View>,
    /// An animated zoom: the scale it is heading to and the point it keeps
    /// still (viewport coordinates).
    target: Cell<Option<(f64, (f64, f64))>>,
    /// Where an animated zoom started (zooming out to the fitted size
    /// glides back to the centre over the way).
    zoom_start: Cell<View>,
    /// When the last animation frame ran (frame clock microseconds).
    last_frame: Cell<i64>,
    /// The pointer, in the viewport's coordinates.
    pointer: Cell<(f64, f64)>,
    percent: gtk::Label,
}

impl Viewer {
    fn image_size(&self) -> Option<(f64, f64)> {
        let paintable = self.picture.paintable()?;
        let size = (
            f64::from(paintable.intrinsic_width()),
            f64::from(paintable.intrinsic_height()),
        );
        (size.0 > 0.0 && size.1 > 0.0).then_some(size)
    }

    fn viewport(&self) -> (f64, f64) {
        (
            f64::from(self.scroll.width()),
            f64::from(self.scroll.height()),
        )
    }

    /// Puts the picture where `view` says, size and position together.
    fn apply(&self, view: View) {
        self.view.set(view);
        let Some(image) = self.image_size() else {
            return;
        };
        let (width, height) = ((image.0 * view.scale).round(), (image.1 * view.scale).round());
        self.picture.set_size_request(width.max(1.0) as i32, height.max(1.0) as i32);
        self.fixed.move_(&self.picture, view.origin.0.round(), view.origin.1.round());
        self.show_percent();
    }

    fn show_percent(&self) {
        self.percent.set_label(&format!("{:.0}%", self.view.get().scale * 100.0));
    }

    /// Fitted to the window, centred; it follows the window's size.
    fn fit(&self) {
        self.target.set(None);
        self.fitted.set(true);
        if let Some(image) = self.image_size() {
            self.apply(fitted_view(image, self.viewport()));
        }
    }

    /// The window or the image changed size: fitted stays fitted, a zoom
    /// keeps within the new edges.
    fn relayout(&self) {
        let Some(image) = self.image_size() else {
            return;
        };
        let viewport = self.viewport();
        if self.fitted.get() {
            self.apply(fitted_view(image, viewport));
        } else {
            let view = self.view.get();
            self.apply(pan_view(image, viewport, view, (0.0, 0.0)));
        }
    }

    /// Zooms to `wanted` at once around `point` (a pinch follows the
    /// fingers exactly).
    fn zoom_now(&self, wanted: f64, point: (f64, f64)) {
        let Some(image) = self.image_size() else {
            return;
        };
        self.target.set(None);
        let viewport = self.viewport();
        let next = zoom_view(image, viewport, self.view.get(), point, wanted);
        self.fitted.set(next.scale <= fit_scale(image, viewport) + 1e-6);
        self.apply(next);
    }

    /// Zooms towards `wanted` around `point`, smoothly: each frame closes
    /// part of the gap, keeping the point under the pointer still.
    fn zoom_to(self: &Rc<Self>, wanted: f64, point: (f64, f64)) {
        let Some(image) = self.image_size() else {
            return;
        };
        let wanted = wanted.clamp(fit_scale(image, self.viewport()), MAX_ZOOM);
        let running = self.target.replace(Some((wanted, point))).is_some();
        if !running || self.fitted.get() {
            self.zoom_start.set(self.view.get());
        }
        self.fitted.set(false);
        if running {
            return;
        }
        self.last_frame.set(0);
        let weak = Rc::downgrade(self);
        self.fixed.add_tick_callback(move |_, clock| {
            let Some(viewer) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let Some((wanted, point)) = viewer.target.get() else {
                return glib::ControlFlow::Break;
            };
            let Some(image) = viewer.image_size() else {
                return glib::ControlFlow::Break;
            };
            let now = clock.frame_time();
            let elapsed = match viewer.last_frame.replace(now) {
                0 => 1.0 / 60.0,
                last => ((now - last) as f64 / 1_000_000.0).clamp(0.0, 0.1),
            };
            // The same speed at any refresh rate.
            let share = 1.0 - (1.0 - EASE).powf(elapsed * 60.0);
            let current = viewer.view.get();
            let settling = (current.scale - wanted).abs() <= wanted * 0.002;
            let mut scale = current.scale + (wanted - current.scale) * share;
            let done = (scale - wanted).abs() <= wanted * 0.002;
            if done {
                scale = wanted;
            }
            let viewport = viewer.viewport();
            let mut next = if settling {
                // The zoom is over: ease back within the edges.
                let edges = pan_view(image, viewport, current, (0.0, 0.0));
                let ease = |from: f64, to: f64| from + (to - from) * share;
                View {
                    scale: current.scale,
                    origin: (ease(current.origin.0, edges.origin.0), ease(current.origin.1, edges.origin.1)),
                }
            } else {
                zoom_view_exact(current, point, scale)
            };
            // Heading back to the fitted size: the image also glides to the
            // centre, all the way there by the end.
            let fit = fit_scale(image, viewport);
            let start = viewer.zoom_start.get();
            if wanted <= fit + 1e-6 && start.scale > fit {
                let left = ((next.scale - fit) / (start.scale - fit)).clamp(0.0, 1.0);
                let centre = fitted_view(image, viewport);
                let blend = |from: f64, to: f64| to + (from - to) * left;
                next.origin = (blend(next.origin.0, centre.origin.0), blend(next.origin.1, centre.origin.1));
            }
            viewer.apply(next);
            if scale <= fit + 1e-6 && done {
                viewer.fit();
                return glib::ControlFlow::Break;
            }
            if settling {
                // Settled when within half a pixel of the edges' position.
                let edges = pan_view(image, viewport, next, (0.0, 0.0));
                let off = (edges.origin.0 - next.origin.0).abs().max((edges.origin.1 - next.origin.1).abs());
                if off < 0.5 {
                    viewer.apply(edges);
                    viewer.target.set(None);
                    return glib::ControlFlow::Break;
                }
            }
            glib::ControlFlow::Continue
        });
    }

    /// Zooms by `factor` from where a zoom under way is heading (so quick
    /// wheel notches add up).
    fn zoom_by(self: &Rc<Self>, factor: f64, point: (f64, f64)) {
        let from = self.target.get().map_or(self.view.get().scale, |(wanted, _)| wanted);
        self.zoom_to(from * factor, point);
    }

    fn centre(&self) -> (f64, f64) {
        let (width, height) = self.viewport();
        (width / 2.0, height / 2.0)
    }
}

impl AppState {
    /// Opens an image at full size in its own window (or brings an open
    /// viewer of it forward).
    pub(crate) fn show_image(self: &Rc<Self>, url: &str, preview_url: &str) {
        let key = format!("image:{url}");
        if self.present_window(&key) {
            return;
        }
        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        // Its size is the view's (set by the viewer), so it fills it.
        picture.set_content_fit(gtk::ContentFit::Fill);
        // Show the preview already in memory while the original downloads.
        self.images.set_picture(&picture, preview_url);
        if is_gif_url(url) {
            // A real GIF animates in the viewer (zoom/pan still work on the
            // animated frames); a still image keeps the plain texture path.
            self.images.set_animated_picture(&picture, url);
        } else if crate::composer::is_video_url(url) {
            // A video-backed "gif" (Klipy/Tenor `.mp4`/`.webm`/`.gifv`, or a
            // video favourite) has no pixbuf form; play it as a looping video.
            // The GStreamer paintable reports an intrinsic size, so zoom/pan
            // keep working the same as for a still.
            self.play_gif(&picture, url);
        } else {
            self.images.set_picture(&picture, url);
        }

        let fixed = gtk::Fixed::new();
        fixed.put(&picture, 0.0, 0.0);
        // A viewport that clips the picture without asking for its size
        // (External: the window does not grow to a zoomed image).
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::External, gtk::PolicyType::External);
        scroll.set_child(Some(&fixed));
        scroll.set_vexpand(true);
        scroll.set_hexpand(true);

        let percent = gtk::Label::new(Some("100%"));
        percent.add_css_class("numeric");
        percent.set_width_chars(5);
        let viewer = Rc::new(Viewer {
            picture: picture.clone(),
            fixed: fixed.clone(),
            scroll: scroll.clone(),
            fitted: Cell::new(true),
            view: Cell::new(View {
                scale: 1.0,
                origin: (0.0, 0.0),
            }),
            target: Cell::new(None),
            zoom_start: Cell::new(View {
                scale: 1.0,
                origin: (0.0, 0.0),
            }),
            last_frame: Cell::new(0),
            pointer: Cell::new((0.0, 0.0)),
            percent: percent.clone(),
        });

        // The image arriving (the full size after the preview) and the
        // window resizing lay it out again.
        let weak = Rc::downgrade(&viewer);
        picture.connect_paintable_notify(move |_| {
            if let Some(viewer) = weak.upgrade() {
                viewer.relayout();
            }
        });
        // Widgets have no size property to watch: each frame compares the
        // viewport with the last one laid out for.
        let weak = Rc::downgrade(&viewer);
        let laid_out = Cell::new((0, 0));
        scroll.add_tick_callback(move |scroll, _| {
            let Some(viewer) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let size = (scroll.width(), scroll.height());
            if size != laid_out.get() && size.0 > 0 && size.1 > 0 {
                laid_out.set(size);
                viewer.relayout();
            }
            glib::ControlFlow::Continue
        });

        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(&viewer);
        motion.connect_motion(move |_, x, y| {
            if let Some(viewer) = weak.upgrade() {
                viewer.pointer.set((x, y));
            }
        });
        scroll.add_controller(motion);

        // The wheel (with or without Ctrl) zooms instead of scrolling.
        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&viewer);
        wheel.connect_scroll(move |_, _, dy| {
            if let Some(viewer) = weak.upgrade() {
                viewer.zoom_by(STEP.powf(-dy), viewer.pointer.get());
            }
            glib::Propagation::Stop
        });
        scroll.add_controller(wheel);

        let pinch = gtk::GestureZoom::new();
        let start = Rc::new(Cell::new(1.0));
        let weak = Rc::downgrade(&viewer);
        let begin = start.clone();
        pinch.connect_begin(move |_, _| {
            if let Some(viewer) = weak.upgrade() {
                begin.set(viewer.view.get().scale);
            }
        });
        let weak = Rc::downgrade(&viewer);
        pinch.connect_scale_changed(move |gesture, delta| {
            if let Some(viewer) = weak.upgrade() {
                let point = gesture.bounding_box_center().unwrap_or(viewer.centre());
                viewer.zoom_now(start.get() * delta, point);
            }
        });
        scroll.add_controller(pinch);

        // Double-click: fitted <-> 100 %.
        let double = gtk::GestureClick::new();
        let weak = Rc::downgrade(&viewer);
        double.connect_pressed(move |_, presses, x, y| {
            if presses != 2 {
                return;
            }
            if let Some(viewer) = weak.upgrade() {
                if viewer.fitted.get() || (viewer.view.get().scale - 1.0).abs() > 1e-6 {
                    viewer.zoom_to(1.0, (x, y));
                } else {
                    let fitted = viewer.image_size().map_or(1.0, |image| fit_scale(image, viewer.viewport()));
                    viewer.zoom_to(fitted, (x, y));
                }
            }
        });
        scroll.add_controller(double);

        // Drag to pan.
        let drag = gtk::GestureDrag::new();
        let origin = Rc::new(Cell::new(None::<View>));
        let weak = Rc::downgrade(&viewer);
        let from = origin.clone();
        drag.connect_drag_begin(move |_, _, _| {
            if let Some(viewer) = weak.upgrade() {
                viewer.target.set(None);
                from.set(Some(viewer.view.get()));
            }
        });
        let weak = Rc::downgrade(&viewer);
        drag.connect_drag_update(move |_, dx, dy| {
            if let Some(viewer) = weak.upgrade()
                && !viewer.fitted.get()
                && let (Some(start), Some(image)) = (origin.get(), viewer.image_size())
            {
                viewer.apply(pan_view(image, viewer.viewport(), start, (dx, dy)));
            }
        });
        scroll.add_controller(drag);

        let header = adw::HeaderBar::new();
        let open = icon_button("web-browser-symbolic", "Open in Browser");
        let link = url.to_owned();
        let parent = self.window.clone();
        open.connect_clicked(move |_| open_uri(Some(parent.upcast_ref()), &link));
        header.pack_start(&open);
        let copy = icon_button("edit-copy-symbolic", "Copy Link");
        let link = url.to_owned();
        let app = self.clone();
        copy.connect_clicked(move |button| {
            button.clipboard().set_text(&link);
            app.tell("Link copied", 3);
        });
        header.pack_start(&copy);
        let kind = if is_gif_url(url) {
            crate::media_menu::MediaType::Gif
        } else if crate::composer::is_video_url(url) {
            crate::media_menu::MediaType::Video
        } else {
            crate::media_menu::MediaType::Image
        };
        let save = icon_button("document-save-symbolic", "Save (Ctrl+S)");
        let link = url.to_owned();
        let app = self.clone();
        save.connect_clicked(move |button| {
            if let Some(window) = button.root().and_downcast::<gtk::Window>() {
                app.save_media(&link, kind, &window);
            }
        });
        header.pack_start(&save);
        self.attach_media_menu(
            &picture,
            crate::media_menu::MediaLinks {
                link: url.to_owned(),
                file: Some(url.to_owned()),
                kind,
            },
        );
        let zoom_out = icon_button("zoom-out-symbolic", "Zoom Out (-)");
        let zoom_in = icon_button("zoom-in-symbolic", "Zoom In (+)");
        let fit = icon_button("zoom-fit-best-symbolic", "Fit to Window (0)");
        for (button, action) in [(&zoom_out, -1), (&zoom_in, 1), (&fit, 0)] {
            let weak = Rc::downgrade(&viewer);
            button.connect_clicked(move |_| {
                if let Some(viewer) = weak.upgrade() {
                    match action {
                        0 => viewer.fit(),
                        1 => viewer.zoom_by(STEP, viewer.centre()),
                        _ => viewer.zoom_by(1.0 / STEP, viewer.centre()),
                    }
                }
            });
        }
        header.pack_end(&fit);
        header.pack_end(&zoom_in);
        header.pack_end(&percent);
        header.pack_end(&zoom_out);

        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&scroll));

        let size = (
            (self.window.width() as f64 * 0.85) as i32,
            (self.window.height() as f64 * 0.85) as i32,
        );
        let (window, _) = self.aux_window(&key, "Image", size, &view);
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&viewer);
        let save_key = save.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let Some(viewer) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            use gtk::gdk::Key;
            match key {
                Key::plus | Key::equal | Key::KP_Add => viewer.zoom_by(STEP, viewer.centre()),
                Key::minus | Key::KP_Subtract => viewer.zoom_by(1.0 / STEP, viewer.centre()),
                Key::_0 | Key::KP_0 => viewer.fit(),
                Key::s | Key::S if modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) => {
                    save_key.emit_clicked();
                }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        window.add_controller(keys);
        // The window holds the viewer for as long as it is open.
        let keep = viewer.clone();
        window.connect_destroy(move |_| {
            let _ = &keep;
        });
        window.present();
        viewer.show_percent();

        // Development aid: `KUBECORD_VIEWER_WHEEL=x,y,notches` turns the
        // wheel at (x, y) once the image is in, and reports, frame by
        // frame, which image pixel is drawn under that point (from where
        // the picture actually is). Zooming about the pointer keeps it the
        // same pixel, so the drift must stay at zero.
        if let Ok(wheel) = std::env::var("KUBECORD_VIEWER_WHEEL") {
            let parts: Vec<f64> = wheel.split(',').filter_map(|part| part.trim().parse().ok()).collect();
            if let [x, y, notches] = parts[..] {
                let viewer = viewer.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(2500), move || {
                    viewer.pointer.set((x, y));
                    let drawn = move |viewer: &Viewer| -> Option<(f64, f64)> {
                        let bounds = viewer.picture.compute_bounds(&viewer.scroll)?;
                        let image = viewer.image_size()?;
                        let scale = f64::from(bounds.width()) / image.0;
                        Some(((x - f64::from(bounds.x())) / scale, (y - f64::from(bounds.y())) / scale))
                    };
                    let start = drawn(&viewer);
                    eprintln!("demo: wheel at ({x}, {y}): pixel {start:?} at {:.0}%", viewer.view.get().scale * 100.0);
                    let worst = Rc::new(Cell::new(0.0f64));
                    let frames = Rc::new(Cell::new(0u32));
                    let watch = viewer.clone();
                    let (worst_tick, frames_tick) = (worst.clone(), frames.clone());
                    let last_scale = Cell::new(viewer.view.get().scale);
                    viewer.scroll.add_tick_callback(move |_, _| {
                        // While the zoom changes the scale (not the settle
                        // after it, which is meant to move the image).
                        let scale = watch.view.get().scale;
                        let zooming = (scale - last_scale.replace(scale)).abs() > 1e-9;
                        if zooming && let (Some(start), Some(now)) = (start, drawn(&watch)) {
                            let drift = (now.0 - start.0).abs().max((now.1 - start.1).abs());
                            worst_tick.set(worst_tick.get().max(drift));
                        }
                        frames_tick.set(frames_tick.get() + 1);
                        if frames_tick.get() < 120 { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
                    });
                    let step = if notches >= 0.0 { -1.0 } else { 1.0 };
                    for index in 0..notches.abs() as u32 {
                        let viewer = viewer.clone();
                        glib::timeout_add_local_once(std::time::Duration::from_millis(u64::from(index) * 70), move || {
                            viewer.zoom_by(STEP.powf(-step), viewer.pointer.get());
                        });
                    }
                    glib::timeout_add_local_once(std::time::Duration::from_millis(2200), move || {
                        eprintln!(
                            "demo: after {} frames, {:.0}%: pixel under the pointer drifted at most {:.2} image px; now {:?}",
                            frames.get(),
                            viewer.view.get().scale * 100.0,
                            worst.get(),
                            drawn(&viewer)
                        );
                    });
                });
            }
        }
    }

    /// Development aid: zooms the open viewer of `url` about its centre.
    pub(crate) fn demo_zoom_viewer(&self, url: &str, scale: f64) {
        if let Some(window) = self.window_of(&format!("image:{url}")) {
            // Find the scrolled window and send it a zoom by key presses'
            // path: the window's keyboard handler is not reachable here, so
            // emulate with the header's zoom-in button clicks.
            let steps = (scale.ln() / STEP.ln()).round() as i32;
            if let Some(button) = find_button(window.upcast_ref(), "Zoom In (+)") {
                for _ in 0..steps.max(0) {
                    button.emit_clicked();
                }
            }
        }
    }
}

fn find_button(root: &gtk::Widget, tooltip: &str) -> Option<gtk::Button> {
    let mut child = root.first_child();
    while let Some(widget) = child {
        if let Some(button) = widget.downcast_ref::<gtk::Button>()
            && button.tooltip_text().as_deref() == Some(tooltip)
        {
            return Some(button.clone());
        }
        if let Some(found) = find_button(&widget, tooltip) {
            return Some(found);
        }
        child = widget.next_sibling();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAGE: (f64, f64) = (2000.0, 1000.0);
    const VIEW: (f64, f64) = (1000.0, 800.0);

    #[test]
    fn gif_urls_are_detected_ignoring_the_query() {
        assert!(is_gif_url("https://media.discordapp.net/a/b.gif"));
        assert!(is_gif_url(
            "https://media.discordapp.net/a/b.gif?width=100&height=80"
        ));
        assert!(is_gif_url("HTTP://x/Y.GIF"));
        assert!(!is_gif_url("https://media.discordapp.net/a/b.png"));
        assert!(!is_gif_url("https://x/gif-thing.webp?x=1"));
    }

    #[test]
    fn fitting_never_enlarges() {
        assert_eq!(fit_scale(IMAGE, VIEW), 0.5);
        assert_eq!(fit_scale((100.0, 50.0), VIEW), 1.0);
        assert_eq!(fit_scale((0.0, 0.0), VIEW), 1.0);
    }

    #[test]
    fn zooming_keeps_the_point_under_the_pointer() {
        // Fitted at 0.5: the image is 1000x500, centred vertically (150 px
        // above). The pointer is on image pixel (1100, 540), off-centre.
        let fitted = fitted_view(IMAGE, VIEW);
        assert_eq!(fitted, View { scale: 0.5, origin: (0.0, 150.0) });
        let point = (550.0, 420.0);
        // Every step of an animated zoom keeps that pixel under it, even
        // while the image is shorter than the window (where the edges allow:
        // the image never leaves a gap once it fills the window).
        let mut view = fitted;
        for scale in [0.55, 0.6, 0.75, 0.9, 1.0, 2.0, 8.0] {
            view = zoom_view(IMAGE, VIEW, view, point, scale);
            let under = ((point.0 - view.origin.0) / view.scale, (point.1 - view.origin.1) / view.scale);
            assert!((under.0 - 1100.0).abs() < 1e-9 && (under.1 - 540.0).abs() < 1e-9, "{scale}: {under:?}");
        }
        assert_eq!(view.scale, MAX_ZOOM);
        // Zooming out reaches the fitted scale (the viewer then centres it).
        let back = zoom_view(IMAGE, VIEW, view, point, 0.1);
        assert_eq!(back.scale, 0.5);
    }

    #[test]
    fn the_image_never_leaves_a_gap_or_the_window() {
        let start = View { scale: 1.0, origin: (0.0, 0.0) };
        let huge = zoom_view(IMAGE, VIEW, start, (0.0, 0.0), 100.0);
        assert_eq!(huge.scale, MAX_ZOOM);
        assert_eq!(huge.origin, (0.0, 0.0));
        // At the far corner, zooming about it keeps the image's far edges
        // at the window's.
        let end = View { scale: 1.0, origin: (-1000.0, -200.0) };
        let corner = zoom_view(IMAGE, VIEW, end, VIEW, 2.0);
        assert_eq!(corner.origin, (1000.0 - 4000.0, 800.0 - 2000.0));
        // Panning stops at the edges; a smaller image stays inside.
        let panned = pan_view(IMAGE, VIEW, start, (300.0, -5000.0));
        assert_eq!(panned.origin, (0.0, -200.0));
        let small = pan_view((400.0, 300.0), VIEW, View { scale: 1.0, origin: (0.0, 0.0) }, (900.0, 50.0));
        assert_eq!(small.origin, (600.0, 50.0));
    }
}
