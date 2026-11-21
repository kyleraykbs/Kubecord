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

/// Keeps a scroll offset within the content: 0 when it fits.
pub(crate) fn clamp_scroll(value: f64, content: f64, viewport: f64) -> f64 {
    value.clamp(0.0, (content - viewport).max(0.0))
}

/// Where the image starts in the viewport along one axis: centred when
/// it is smaller, else scrolled.
fn origin(image: f64, scale: f64, viewport: f64, scroll: f64) -> f64 {
    let size = image * scale;
    if size < viewport {
        (viewport - size) / 2.0
    } else {
        -scroll
    }
}

/// A zoom state: scale and scroll offsets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Zoom {
    pub scale: f64,
    pub scroll: (f64, f64),
}

/// Zooms to `wanted` (clamped between the fitted scale and [`MAX_ZOOM`])
/// keeping the image point under `point` (viewport coordinates) where it
/// is; the scroll offsets are clamped to the zoomed image.
pub(crate) fn zoom_about(
    image: (f64, f64),
    viewport: (f64, f64),
    current: Zoom,
    point: (f64, f64),
    wanted: f64,
) -> Zoom {
    let scale = wanted.clamp(fit_scale(image, viewport), MAX_ZOOM);
    let axis = |image: f64, viewport: f64, scroll: f64, point: f64| {
        let under = (point - origin(image, current.scale, viewport, scroll)) / current.scale;
        clamp_scroll(under * scale - point, image * scale, viewport)
    };
    Zoom {
        scale,
        scroll: (
            axis(image.0, viewport.0, current.scroll.0, point.0),
            axis(image.1, viewport.1, current.scroll.1, point.1),
        ),
    }
}

/// One open viewer.
struct Viewer {
    picture: gtk::Picture,
    scroll: gtk::ScrolledWindow,
    /// Fitted to the window (follows its size) rather than a fixed scale.
    fitted: Cell<bool>,
    scale: Cell<f64>,
    /// The pointer, in the scrolled window's coordinates.
    pointer: Cell<(f64, f64)>,
    /// Scroll offsets to apply once the new size is laid out.
    pending: Cell<Option<(f64, f64)>>,
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

    fn current(&self) -> Zoom {
        if self.fitted.get() {
            let scale = self
                .image_size()
                .map_or(1.0, |image| fit_scale(image, self.viewport()));
            return Zoom {
                scale,
                scroll: (0.0, 0.0),
            };
        }
        // Offsets not yet applied (the layout has not caught up with a
        // zoom a moment ago) are the true position.
        let scroll = self.pending.get().unwrap_or_else(|| {
            (
                self.scroll.hadjustment().value(),
                self.scroll.vadjustment().value(),
            )
        });
        Zoom {
            scale: self.scale.get(),
            scroll,
        }
    }

    fn show_percent(&self) {
        let scale = self.current().scale;
        self.percent.set_label(&format!("{:.0}%", scale * 100.0));
    }

    fn fit(&self) {
        self.fitted.set(true);
        self.pending.set(None);
        self.picture.set_halign(gtk::Align::Fill);
        self.picture.set_valign(gtk::Align::Fill);
        self.picture.set_size_request(-1, -1);
        self.picture.set_content_fit(gtk::ContentFit::ScaleDown);
        self.scroll
            .set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
        self.scroll.remove_css_class("kc-viewer-zoomed");
        self.show_percent();
    }

    /// Zooms to `wanted` around `point`; at or below the fitted scale it
    /// goes back to fitted.
    fn zoom_to(&self, wanted: f64, point: (f64, f64)) {
        let Some(image) = self.image_size() else {
            return;
        };
        let viewport = self.viewport();
        let next = zoom_about(image, viewport, self.current(), point, wanted);
        if next.scale <= fit_scale(image, viewport) + 1e-6 {
            self.fit();
            return;
        }
        self.fitted.set(false);
        self.scale.set(next.scale);
        // The size request is only a minimum: where the zoomed image is
        // smaller than the viewport (one side of a tall or wide image), the
        // scrolled window still makes the picture as wide (or tall) as the
        // viewport. Contain keeps the aspect ratio in that box, which is
        // exactly the zoomed size (the other side fills the box exactly);
        // Fill stretched it. Centred, so the box is no bigger than needed.
        self.picture.set_content_fit(gtk::ContentFit::Contain);
        self.picture.set_halign(gtk::Align::Center);
        self.picture.set_valign(gtk::Align::Center);
        self.picture.set_size_request(
            (image.0 * next.scale).round() as i32,
            (image.1 * next.scale).round() as i32,
        );
        self.scroll
            .set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        self.scroll.add_css_class("kc-viewer-zoomed");
        // The adjustments only reach the new size after the next layout.
        self.pending.set(Some(next.scroll));
        self.scroll.hadjustment().set_value(next.scroll.0);
        self.scroll.vadjustment().set_value(next.scroll.1);
        self.show_percent();
    }

    fn zoom_by(&self, factor: f64, point: (f64, f64)) {
        self.zoom_to(self.current().scale * factor, point);
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
        picture.set_content_fit(gtk::ContentFit::ScaleDown);
        picture.set_halign(gtk::Align::Center);
        picture.set_valign(gtk::Align::Center);
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

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
        scroll.set_child(Some(&picture));
        scroll.set_vexpand(true);
        scroll.set_hexpand(true);

        let percent = gtk::Label::new(Some("100%"));
        percent.add_css_class("numeric");
        percent.set_width_chars(5);
        let viewer = Rc::new(Viewer {
            picture: picture.clone(),
            scroll: scroll.clone(),
            fitted: Cell::new(true),
            scale: Cell::new(1.0),
            pointer: Cell::new((0.0, 0.0)),
            pending: Cell::new(None),
            percent: percent.clone(),
        });

        // Pending scroll offsets, applied after the layout grew the range.
        for adjustment in [scroll.hadjustment(), scroll.vadjustment()] {
            let weak = Rc::downgrade(&viewer);
            adjustment.connect_changed(move |_| {
                let weak = weak.clone();
                // Never set an adjustment from its own `changed`.
                glib::idle_add_local_once(move || {
                    if let Some(viewer) = weak.upgrade()
                        && let Some((x, y)) = viewer.pending.take()
                    {
                        viewer.scroll.hadjustment().set_value(x);
                        viewer.scroll.vadjustment().set_value(y);
                    }
                });
            });
        }
        // The percentage follows the image arriving and window resizes.
        let weak = Rc::downgrade(&viewer);
        picture.connect_paintable_notify(move |_| {
            if let Some(viewer) = weak.upgrade() {
                viewer.show_percent();
            }
        });
        let weak = Rc::downgrade(&viewer);
        scroll.connect_notify_local(Some("width"), move |_, _| {
            if let Some(viewer) = weak.upgrade() {
                viewer.show_percent();
            }
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
                begin.set(viewer.current().scale);
            }
        });
        let weak = Rc::downgrade(&viewer);
        pinch.connect_scale_changed(move |gesture, delta| {
            if let Some(viewer) = weak.upgrade() {
                let point = gesture.bounding_box_center().unwrap_or(viewer.centre());
                viewer.zoom_to(start.get() * delta, point);
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
                if viewer.fitted.get() || (viewer.scale.get() - 1.0).abs() > 1e-6 {
                    viewer.zoom_to(1.0, (x, y));
                } else {
                    viewer.fit();
                }
            }
        });
        scroll.add_controller(double);

        // Drag to pan.
        let drag = gtk::GestureDrag::new();
        let origin = Rc::new(Cell::new((0.0, 0.0)));
        let weak = Rc::downgrade(&viewer);
        let from = origin.clone();
        drag.connect_drag_begin(move |_, _, _| {
            if let Some(viewer) = weak.upgrade() {
                from.set((
                    viewer.scroll.hadjustment().value(),
                    viewer.scroll.vadjustment().value(),
                ));
            }
        });
        let weak = Rc::downgrade(&viewer);
        drag.connect_drag_update(move |_, dx, dy| {
            if let Some(viewer) = weak.upgrade()
                && !viewer.fitted.get()
            {
                let (x, y) = origin.get();
                viewer.pending.set(None);
                viewer.scroll.hadjustment().set_value(x - dx);
                viewer.scroll.vadjustment().set_value(y - dy);
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
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(viewer) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            use gtk::gdk::Key;
            match key {
                Key::plus | Key::equal | Key::KP_Add => viewer.zoom_by(STEP, viewer.centre()),
                Key::minus | Key::KP_Subtract => viewer.zoom_by(1.0 / STEP, viewer.centre()),
                Key::_0 | Key::KP_0 => viewer.fit(),
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
        // above). The pointer is on image pixel (1200, 600).
        let fitted = Zoom {
            scale: 0.5,
            scroll: (0.0, 0.0),
        };
        let point = (600.0, 450.0);
        let zoomed = zoom_about(IMAGE, VIEW, fitted, point, 1.0);
        assert_eq!(zoomed.scale, 1.0);
        // Image pixel (1200, 600) at scale 1 sits under the pointer.
        assert_eq!(zoomed.scroll, (1200.0 - 600.0, 600.0 - 450.0));
        // And back: zooming out again returns to the fitted view.
        let back = zoom_about(IMAGE, VIEW, zoomed, point, 0.1);
        assert_eq!(back.scale, 0.5);
        assert_eq!(back.scroll, (0.0, 0.0));
    }

    #[test]
    fn zoom_and_scroll_stay_in_range() {
        let start = Zoom {
            scale: 1.0,
            scroll: (0.0, 0.0),
        };
        let huge = zoom_about(IMAGE, VIEW, start, (0.0, 0.0), 100.0);
        assert_eq!(huge.scale, MAX_ZOOM);
        assert_eq!(huge.scroll, (0.0, 0.0));
        // Zooming about the far corner cannot scroll past the end.
        // Scrolled to the far corner, zooming about it stays at the end.
        let end = Zoom {
            scale: 1.0,
            scroll: (1000.0, 200.0),
        };
        let corner = zoom_about(IMAGE, VIEW, end, VIEW, 2.0);
        assert_eq!(corner.scroll, (4000.0 - 1000.0, 2000.0 - 800.0));
        // About the top-left of the window at scale 1: that pixel stays.
        let middle = zoom_about(IMAGE, VIEW, start, VIEW, 2.0);
        assert_eq!(middle.scroll, (1000.0, 800.0));
        assert_eq!(clamp_scroll(-5.0, 2000.0, 1000.0), 0.0);
        assert_eq!(clamp_scroll(5000.0, 2000.0, 1000.0), 1000.0);
        assert_eq!(clamp_scroll(50.0, 500.0, 1000.0), 0.0);
    }
}
