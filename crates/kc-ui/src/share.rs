//! Sharing messages: pick a range of messages (as long as wanted: the list
//! loads older ones while an end is dragged up), add single messages outside
//! it or redact ones inside it with a right-click, then export the result as
//! a picture or as text, to the clipboard or a file.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use kc_core::Snowflake;
use kc_state::Message;

use crate::content::StateResolver;
use crate::shell::AppState;
use crate::widgets::label;

/// Pointer this close to the list's top or bottom edge scrolls it while
/// dragging.
const EDGE: f64 = 48.0;
/// Messages fetched at most for an export of messages no longer loaded.
const MAX_FETCH_PAGES: usize = 100;
/// Rendered at most this tall in one go (a texture's limit).
const TILE: i32 = 4096;
/// Pictures taller than this are rendered at 1× instead of 2×.
const MAX_HIRES_HEIGHT: i32 = 30_000;
/// A picture taller than this is saved in parts.
const PART_HEIGHT: i32 = 30_000;
/// How long a picture is laid out before it is drawn.
const SETTLE: Duration = Duration::from_millis(300);
/// The longest a picture waits on images and GIFs still loading.
const MEDIA_WAIT: Duration = Duration::from_secs(5);
/// Frames tried before giving up on drawing a picture.
const MAX_CAPTURE_FRAMES: u32 = 20;
/// Width of an exported picture (before scaling).
const RENDER_WIDTH: i32 = 760;

/// How a message takes part in the share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    Out,
    In,
    /// In the range but taken out: shown blocked out (or left out).
    Redacted,
    /// Only in an export: this many messages between two shared ones were
    /// not shared (the message given with it is the first of them).
    Skipped(usize),
}

/// What to export from `messages` (oldest first, every message from the
/// first shared to the last): the shared ones, with a [`Mark::Skipped`]
/// wherever shared messages are not neighbours.
pub(crate) fn export_items(
    messages: Vec<Message>,
    selection: &Selection,
    leave_out_redacted: bool,
) -> Vec<(Message, Mark)> {
    let mut out: Vec<(Message, Mark)> = Vec::new();
    let mut skipped: Option<(Message, usize)> = None;
    for message in messages {
        let mark = selection.mark(message.id);
        let hidden = mark == Mark::Out || (mark == Mark::Redacted && leave_out_redacted);
        if hidden {
            // Only between shared messages (not before the first).
            if !out.is_empty() {
                match &mut skipped {
                    Some((_, count)) => *count += 1,
                    None => skipped = Some((message, 1)),
                }
            }
            continue;
        }
        if let Some((first, count)) = skipped.take() {
            out.push((first, Mark::Skipped(count)));
        }
        out.push((message, mark));
    }
    out
}

/// "1 message skipped", "12 messages skipped".
fn skipped_text(count: usize) -> String {
    if count == 1 {
        "1 message skipped".to_owned()
    } else {
        format!("{count} messages skipped")
    }
}

/// What is chosen: a range, single messages added outside it, and ones
/// redacted inside it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Selection {
    /// The first message clicked, while waiting for the last.
    pub anchor: Option<Snowflake>,
    /// The message under the pointer while picking (previewed as the end).
    pub hover: Option<Snowflake>,
    /// The chosen range, oldest first (inclusive).
    pub range: Option<(Snowflake, Snowflake)>,
    pub included: BTreeSet<Snowflake>,
    pub excluded: BTreeSet<Snowflake>,
}

impl Selection {
    /// The range shown: the chosen one, or while picking, the first click to
    /// the pointer.
    pub fn shown_range(&self) -> Option<(Snowflake, Snowflake)> {
        self.range.or_else(|| {
            let anchor = self.anchor?;
            let other = self.hover.unwrap_or(anchor);
            Some((anchor.min(other), anchor.max(other)))
        })
    }

    fn in_range(&self, id: Snowflake) -> bool {
        self.shown_range()
            .is_some_and(|(start, end)| start <= id && id <= end)
    }

    pub fn mark(&self, id: Snowflake) -> Mark {
        if self.in_range(id) {
            if self.excluded.contains(&id) {
                Mark::Redacted
            } else {
                Mark::In
            }
        } else if self.included.contains(&id) {
            Mark::In
        } else {
            Mark::Out
        }
    }

    /// A right-click: inside the range it redacts (or restores), outside it
    /// adds (or removes) the message.
    pub fn toggle(&mut self, id: Snowflake) {
        let set = if self.in_range(id) {
            &mut self.excluded
        } else {
            &mut self.included
        };
        if !set.remove(&id) {
            set.insert(id);
        }
    }

    /// A click while picking: the first one starts the range, the second
    /// ends it. Does nothing once a range is chosen.
    pub fn click(&mut self, id: Snowflake) {
        if self.range.is_some() {
            return;
        }
        match self.anchor {
            None => {
                self.anchor = Some(id);
                self.hover = Some(id);
            }
            Some(anchor) => self.finish(anchor, id),
        }
    }

    /// Chooses the range from `a` to `b` (either order).
    pub fn finish(&mut self, a: Snowflake, b: Snowflake) {
        self.range = Some((a.min(b), a.max(b)));
        self.anchor = None;
        self.hover = None;
        // Messages now inside the range are in it; those added outside it
        // stay added only while outside.
        let range = self.range;
        self.included
            .retain(|id| !range.is_some_and(|(start, end)| start <= *id && *id <= end));
    }

    /// Whether anything would be shared.
    pub fn has_any(&self) -> bool {
        self.range.is_some() || !self.included.is_empty()
    }

    /// The oldest and newest messages taking part.
    pub fn bounds(&self) -> Option<(Snowflake, Snowflake)> {
        let mut ids: Vec<Snowflake> = self.included.iter().copied().collect();
        if let Some((start, end)) = self.range {
            ids.push(start);
            ids.push(end);
        }
        Some((*ids.iter().min()?, *ids.iter().max()?))
    }
}

/// What a drag on the list is doing.
#[derive(Clone, Copy, Debug)]
enum Drag {
    /// Sweeping out a range from the first message pressed.
    Picking,
    /// Moving one end of the chosen range; `fixed` is the other end.
    End { fixed: Snowflake },
}

/// How to export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExportOptions {
    pub picture: bool,
    pub times: bool,
    pub leave_out_redacted: bool,
    pub markdown: bool,
    pub high_resolution: bool,
}

/// Where an export goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Clipboard,
    File,
}

/// Share mode while it is on.
struct Share {
    channel: Snowflake,
    selection: Selection,
    bar: gtk::Box,
    prompt: gtk::Label,
    export: gtk::MenuButton,
    top: gtk::Box,
    bottom: gtk::Box,
    controllers: Vec<gtk::EventController>,
    key: gtk::EventController,
    position: Option<glib::SignalHandlerId>,
    scrolled: Option<glib::SignalHandlerId>,
    drag: Option<Drag>,
    /// The pointer during a drag, in the overlay's coordinates.
    pointer: (f64, f64),
    autoscroll: Option<glib::SourceId>,
}

thread_local! {
    static SHARE: RefCell<Option<Share>> = const { RefCell::new(None) };
    /// The demo saves here without asking.
    static DEMO_FOLDER: RefCell<Option<std::path::PathBuf>> = const { RefCell::new(None) };
}

/// Whether share mode is on.
pub(crate) fn active() -> bool {
    SHARE.with_borrow(Option::is_some)
}

fn active_channel() -> Option<Snowflake> {
    SHARE.with_borrow(|share| share.as_ref().map(|share| share.channel))
}

fn selection() -> Option<Selection> {
    SHARE.with_borrow(|share| share.as_ref().map(|share| share.selection.clone()))
}

fn with_selection(change: impl FnOnce(&mut Selection)) {
    SHARE.with_borrow_mut(|share| {
        if let Some(share) = share {
            change(&mut share.selection);
        }
    });
}

/// A grab handle for one end of the range.
fn handle(tooltip: &str) -> gtk::Box {
    let handle = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    handle.add_css_class("kc-share-handle");
    let icon = gtk::Image::from_icon_name("list-drag-handle-symbolic");
    handle.append(&icon);
    handle.set_tooltip_text(Some(tooltip));
    handle.set_cursor_from_name(Some("ns-resize"));
    handle
}

/// The text of one message for a text export: its content with mentions
/// named, then its attachments and embeds.
fn message_text(message: &Message, resolver: &StateResolver) -> String {
    let pieces = crate::rich::parse(&message.content);
    let mut text = crate::rich::to_plain(&pieces, resolver, true);
    for attachment in &message.attachments {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("[{}] {}", attachment.filename, attachment.url));
    }
    for embed in &message.embeds {
        let title = embed.title.as_deref().or(embed.url.as_deref());
        if let Some(title) = title {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("[embed] {title}"));
        }
    }
    text
}

/// The text export of `items` (oldest first). `name` and `body` give each
/// message's author and text; `time` its time.
pub(crate) fn format_text(
    items: &[(Message, Mark)],
    name: &dyn Fn(&Message) -> String,
    body: &dyn Fn(&Message) -> String,
    time: &dyn Fn(&Message) -> String,
    system: &dyn Fn(&Message) -> Option<String>,
    options: ExportOptions,
) -> String {
    let mut out = String::new();
    let mut last_author: Option<Snowflake> = None;
    for (message, mark) in items {
        match mark {
            Mark::Out => continue,
            Mark::Skipped(count) => {
                if options.markdown {
                    out.push_str(&format!("\n*··· {} ···*\n", skipped_text(*count)));
                } else {
                    out.push_str(&format!("··· {} ···\n", skipped_text(*count)));
                }
                last_author = None;
                continue;
            }
            Mark::Redacted if options.leave_out_redacted => continue,
            Mark::Redacted => {
                if options.markdown {
                    out.push_str("\n*[redacted message]*\n");
                } else {
                    out.push_str("[redacted message]\n");
                }
                last_author = None;
                continue;
            }
            Mark::In => {}
        }
        // A call, pin, join...: its sentence on a line of its own.
        if let Some(sentence) = system(message) {
            if options.markdown {
                out.push_str(&format!("\n*{sentence}*"));
                if options.times {
                    out.push_str(&format!(" · {}", time(message)));
                }
                out.push('\n');
            } else {
                if options.times {
                    out.push_str(&format!("[{}] ", time(message)));
                }
                out.push_str(&format!("{sentence}\n"));
            }
            last_author = None;
            continue;
        }
        let mut text = body(message);
        // Earlier versions the message logger kept, oldest first.
        if !message.edits.is_empty() {
            let earlier: Vec<String> = message
                .edits
                .iter()
                .map(|edit| {
                    if options.markdown {
                        format!("~~{}~~ *(edited)*", edit.content)
                    } else {
                        format!("[before edit] {}", edit.content)
                    }
                })
                .collect();
            text = format!("{}\n{text}", earlier.join("\n"));
        }
        // Kept by the message logger after it was deleted.
        if message.deleted {
            text = if options.markdown {
                format!("{text} *(deleted)*")
            } else {
                format!("{text} [deleted]")
            };
        }
        if options.markdown {
            if last_author != Some(message.author.id) {
                out.push('\n');
                out.push_str(&format!("**{}**", name(message)));
                if options.times {
                    out.push_str(&format!(" · {}", time(message)));
                }
                out.push('\n');
            }
            out.push_str(&text);
            out.push('\n');
        } else {
            let mut lines = text.lines();
            let first = lines.next().unwrap_or_default();
            if options.times {
                out.push_str(&format!("[{}] ", time(message)));
            }
            out.push_str(&format!("{}: {first}\n", name(message)));
            for line in lines {
                out.push_str(&format!("    {line}\n"));
            }
        }
        last_author = Some(message.author.id);
    }
    out.trim_start_matches('\n').to_owned()
}

impl AppState {
    /// The share button: turns share mode on, or off.
    pub(crate) fn toggle_share_mode(self: &Rc<Self>) {
        if active() {
            self.exit_share_mode();
        } else {
            self.enter_share_mode();
        }
    }

    fn enter_share_mode(self: &Rc<Self>) {
        let Some(channel) = self.chat.borrow().channel else {
            return;
        };
        self.hide_toolbar();

        // The bar: what to do, Export once something is chosen, Cancel.
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        bar.add_css_class("kc-share-bar");
        bar.set_valign(gtk::Align::Start);
        bar.set_margin_top(8);
        bar.set_margin_start(12);
        bar.set_margin_end(12);
        bar.append(&gtk::Image::from_icon_name("emblem-shared-symbolic"));
        let prompt = gtk::Label::new(None);
        prompt.set_wrap(true);
        prompt.set_xalign(0.0);
        prompt.set_hexpand(true);
        prompt.set_width_chars(12);
        bar.append(&prompt);
        let export = gtk::MenuButton::new();
        export.set_label("Export");
        export.add_css_class("suggested-action");
        export.set_valign(gtk::Align::Center);
        export.set_visible(false);
        export.set_popover(Some(&self.export_popover()));
        bar.append(&export);
        let cancel = gtk::Button::from_icon_name("window-close-symbolic");
        cancel.set_tooltip_text(Some("Stop Sharing (Esc)"));
        cancel.add_css_class("circular");
        cancel.add_css_class("flat");
        cancel.set_valign(gtk::Align::Center);
        let app = self.clone();
        cancel.connect_clicked(move |_| app.exit_share_mode());
        bar.append(&cancel);
        self.message_overlay.add_overlay(&bar);

        let top = handle("Drag to move the start");
        let bottom = handle("Drag to move the end");
        self.message_overlay.add_overlay(&top);
        self.message_overlay.add_overlay(&bottom);

        // The handles sit on the range's edges.
        let app = Rc::downgrade(self);
        let (top_ref, bottom_ref) = (top.downgrade(), bottom.downgrade());
        let position = self
            .message_overlay
            .connect_get_child_position(move |overlay, child| {
                let app = app.upgrade()?;
                let top = top_ref.upgrade()?;
                let bottom = bottom_ref.upgrade()?;
                let is_top = child == top.upcast_ref::<gtk::Widget>();
                if !is_top && child != bottom.upcast_ref::<gtk::Widget>() {
                    return None;
                }
                let (width, _) = child.preferred_size();
                let (width, height) = (width.width(), child.preferred_size().1.height());
                let x = (overlay.width() - width) / 2;
                let y = app
                    .range_edge(is_top, overlay)
                    .map(|edge| edge.round() as i32 - height / 2)
                    // Off the range (or it is not loaded): out of sight.
                    .filter(|y| *y > -height && *y < overlay.height())
                    .unwrap_or(-10_000);
                Some(gtk::gdk::Rectangle::new(x, y, width, height))
            });
        let overlay = self.message_overlay.downgrade();
        let scrolled = self
            .messages_scroll
            .vadjustment()
            .connect_value_changed(move |_| {
                if let Some(overlay) = overlay.upgrade() {
                    overlay.queue_allocate();
                }
            });

        // Input on the list: press and drag to pick or move an end, a
        // right-click to add or redact, motion to preview the range.
        let drag = gtk::GestureDrag::new();
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        let app = self.clone();
        drag.connect_drag_begin(move |gesture, x, y| {
            if app.share_drag_begin(x, y) {
                gesture.set_state(gtk::EventSequenceState::Claimed);
            } else {
                gesture.set_state(gtk::EventSequenceState::Denied);
            }
        });
        let app = self.clone();
        drag.connect_drag_update(move |gesture, dx, dy| {
            if let Some((x, y)) = gesture.start_point() {
                app.share_drag_update(x + dx, y + dy);
            }
        });
        let app = self.clone();
        drag.connect_drag_end(move |gesture, dx, dy| {
            if let Some((x, y)) = gesture.start_point() {
                app.share_drag_end(x + dx, y + dy);
            }
        });
        let right = gtk::GestureClick::new();
        right.set_button(gtk::gdk::BUTTON_SECONDARY);
        right.set_propagation_phase(gtk::PropagationPhase::Capture);
        let app = self.clone();
        right.connect_pressed(move |gesture, _, x, y| {
            if let Some(id) = app.share_row_at(x, y, false) {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                with_selection(|selection| selection.toggle(id));
                app.apply_share_marks();
            }
        });
        let motion = gtk::EventControllerMotion::new();
        let app = self.clone();
        motion.connect_motion(move |_, x, y| {
            let picking = SHARE.with_borrow(|share| {
                share
                    .as_ref()
                    .is_some_and(|share| share.selection.anchor.is_some() && share.drag.is_none())
            });
            if picking && let Some(id) = app.share_row_at(x, y, true) {
                let changed = SHARE.with_borrow_mut(|share| {
                    let share = share.as_mut()?;
                    (share.selection.hover != Some(id)).then(|| share.selection.hover = Some(id))
                });
                if changed.is_some() {
                    app.apply_share_marks();
                }
            }
        });
        let controllers: Vec<gtk::EventController> = vec![drag.upcast(), right.upcast(), motion.upcast()];
        for controller in &controllers {
            self.message_overlay.add_controller(controller.clone());
        }
        // Escape cancels.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let app = self.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                app.exit_share_mode();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        let key: gtk::EventController = keys.upcast();
        self.window.add_controller(key.clone());

        self.messages_list.add_css_class("kc-share-active");
        self.share_button.add_css_class("suggested-action");
        SHARE.with_borrow_mut(|share| {
            *share = Some(Share {
                channel,
                selection: Selection::default(),
                bar,
                prompt,
                export,
                top,
                bottom,
                controllers,
                key,
                position: Some(position),
                scrolled: Some(scrolled),
                drag: None,
                pointer: (0.0, 0.0),
                autoscroll: None,
            });
        });
        self.apply_share_marks();
    }

    /// Leaves share mode, putting everything back.
    pub(crate) fn exit_share_mode(self: &Rc<Self>) {
        let Some(mut share) = SHARE.with_borrow_mut(Option::take) else {
            return;
        };
        // The close button had the focus: removed with the bar, it would
        // pass the focus to a message row, and the list would scroll to show
        // that row. The message box takes it instead.
        self.composer.grab_focus();
        // Removing the "Redacted" notes changes heights; the message at the
        // top of the view stays where it is.
        let anchor = self.top_visible_row();
        if let Some(source) = share.autoscroll.take() {
            source.remove();
        }
        for controller in &share.controllers {
            self.message_overlay.remove_controller(controller);
        }
        self.window.remove_controller(&share.key);
        if let Some(id) = share.position.take() {
            self.message_overlay.disconnect(id);
        }
        if let Some(id) = share.scrolled.take() {
            self.messages_scroll.vadjustment().disconnect(id);
        }
        self.message_overlay.remove_overlay(&share.bar);
        self.message_overlay.remove_overlay(&share.top);
        self.message_overlay.remove_overlay(&share.bottom);
        self.messages_list.remove_css_class("kc-share-active");
        self.share_button.remove_css_class("suggested-action");
        for entry in self.chat.borrow().rows.iter() {
            mark_body(&entry.body, Mark::Out, false, false);
        }
        if let Some((row, offset)) = anchor {
            let app = self.clone();
            glib::idle_add_local_once(move || app.restore_top_row(&row, offset));
        }
    }

    /// The first row showing at the top of the message list, and how far
    /// its top is above the view's top.
    fn top_visible_row(&self) -> Option<(gtk::ListBoxRow, f64)> {
        if self.chat.borrow().at_bottom {
            return None;
        }
        let value = self.messages_scroll.vadjustment().value();
        let content = self.messages_scroll.child()?;
        let chat = self.chat.borrow();
        chat.rows.iter().find_map(|entry| {
            let bounds = entry.row.compute_bounds(&content)?;
            let bottom = f64::from(bounds.y() + bounds.height());
            (bottom > value).then(|| (entry.row.clone(), value - f64::from(bounds.y())))
        })
    }

    /// Scrolls so `row`'s top is `offset` above the view's top again.
    fn restore_top_row(&self, row: &gtk::ListBoxRow, offset: f64) {
        let Some(content) = self.messages_scroll.child() else {
            return;
        };
        let Some(bounds) = row.compute_bounds(&content) else {
            return;
        };
        let adjustment = self.messages_scroll.vadjustment();
        let max = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
        adjustment.set_value((f64::from(bounds.y()) + offset).clamp(adjustment.lower(), max));
    }

    /// Leaves share mode if it was for another channel than `channel`.
    pub(crate) fn share_channel_changed(self: &Rc<Self>, channel: Snowflake) {
        if active_channel().is_some_and(|shared| shared != channel) {
            self.exit_share_mode();
        }
    }

    /// Marks a row just built (rows come and go as the list scrolls).
    pub(crate) fn mark_share_row(&self, body: &gtk::Box, id: Snowflake) {
        if let Some(selection) = selection() {
            let range = selection.shown_range();
            mark_body(
                body,
                selection.mark(id),
                range.is_some_and(|(start, _)| start == id),
                range.is_some_and(|(_, end)| end == id),
            );
        }
    }

    /// Shows the selection on every row, and updates the bar and handles.
    fn apply_share_marks(self: &Rc<Self>) {
        let Some(selection) = selection() else {
            return;
        };
        let range = selection.shown_range();
        for entry in self.chat.borrow().rows.iter() {
            mark_body(
                &entry.body,
                selection.mark(entry.id),
                range.is_some_and(|(start, _)| start == entry.id),
                range.is_some_and(|(_, end)| end == entry.id),
            );
        }
        SHARE.with_borrow(|share| {
            let Some(share) = share else {
                return;
            };
            share.prompt.set_text(if selection.range.is_some() {
                "Drag the handles to change the range. Right-click redacts a message in it, or picks a single one outside it."
            } else if selection.anchor.is_some() {
                "Now click the last message. Right-click picks single messages."
            } else {
                "Click the first message to share, or drag across messages. Right-click picks single messages."
            });
            share.export.set_visible(selection.has_any());
            let chosen = selection.range.is_some();
            share.top.set_visible(chosen);
            share.bottom.set_visible(chosen);
        });
        self.message_overlay.queue_allocate();
    }

    /// The y (in `overlay`'s coordinates) of the range's top or bottom
    /// edge, when that row is loaded.
    fn range_edge(&self, top: bool, overlay: &gtk::Overlay) -> Option<f32> {
        let (start, end) = selection()?.range?;
        let id = if top { start } else { end };
        let chat = self.chat.borrow();
        let entry = chat.rows.iter().find(|entry| entry.id == id)?;
        let bounds = entry.body.compute_bounds(overlay)?;
        Some(if top {
            bounds.y()
        } else {
            bounds.y() + bounds.height()
        })
    }

    /// The message at `(x, y)` of the overlay. With `clamp`, a point above
    /// or below the rows gives the first or last.
    fn share_row_at(&self, x: f64, y: f64, clamp: bool) -> Option<Snowflake> {
        let point = self
            .message_overlay
            .compute_point(&self.messages_list, &gtk::graphene::Point::new(x as f32, y as f32))?;
        let chat = self.chat.borrow();
        let row = self.messages_list.row_at_y(point.y() as i32);
        match row {
            Some(row) => chat.rows.iter().find(|entry| entry.row == row).map(|entry| entry.id),
            None if clamp => {
                if point.y() < 0.0 {
                    chat.rows.first().map(|entry| entry.id)
                } else {
                    chat.rows.last().map(|entry| entry.id)
                }
            }
            None => None,
        }
    }

    /// A press on the overlay: on a handle it starts moving that end; on a
    /// message while picking, it starts (or ends) the range. False leaves
    /// the press to the widgets under it (the bar, the jump button).
    fn share_drag_begin(self: &Rc<Self>, x: f64, y: f64) -> bool {
        let point = gtk::graphene::Point::new(x as f32, y as f32);
        let (bar, top, bottom, selection) = match SHARE.with_borrow(|share| {
            share.as_ref().map(|share| {
                (share.bar.clone(), share.top.clone(), share.bottom.clone(), share.selection.clone())
            })
        }) {
            Some(parts) => parts,
            None => return false,
        };
        let inside = |widget: &gtk::Box| {
            widget.is_visible()
                && widget
                    .compute_bounds(&self.message_overlay)
                    .is_some_and(|bounds| bounds.contains_point(&point))
        };
        if inside(&bar) {
            return false;
        }
        let drag = if let Some((start, end)) = selection.range {
            if inside(&top) {
                Some(Drag::End { fixed: end })
            } else if inside(&bottom) {
                Some(Drag::End { fixed: start })
            } else {
                // Clicks with a range chosen do nothing (but are kept from
                // the messages' own links and buttons).
                return self.share_row_at(x, y, false).is_some();
            }
        } else {
            let id = self.share_row_at(x, y, false);
            let Some(id) = id else {
                return false;
            };
            if selection.anchor.is_some() {
                with_selection(|selection| selection.click(id));
                self.apply_share_marks();
                None
            } else {
                with_selection(|selection| selection.click(id));
                self.apply_share_marks();
                Some(Drag::Picking)
            }
        };
        SHARE.with_borrow_mut(|share| {
            if let Some(share) = share {
                share.drag = drag;
                share.pointer = (x, y);
            }
        });
        if drag.is_some() {
            self.start_autoscroll();
        }
        true
    }

    fn share_drag_update(self: &Rc<Self>, x: f64, y: f64) {
        SHARE.with_borrow_mut(|share| {
            if let Some(share) = share {
                share.pointer = (x, y);
            }
        });
        self.follow_pointer();
    }

    /// Moves what is being dragged to the message under the pointer.
    fn follow_pointer(self: &Rc<Self>) {
        let Some((drag, (x, y))) = SHARE.with_borrow(|share| {
            let share = share.as_ref()?;
            Some((share.drag?, share.pointer))
        }) else {
            return;
        };
        let Some(id) = self.share_row_at(x, y, true) else {
            return;
        };
        let changed = SHARE.with_borrow_mut(|share| {
            let selection = &mut share.as_mut()?.selection;
            let before = (selection.hover, selection.range);
            match drag {
                Drag::Picking => selection.hover = Some(id),
                Drag::End { fixed } => selection.range = Some((fixed.min(id), fixed.max(id))),
            }
            Some(before != (selection.hover, selection.range))
        });
        if changed == Some(true) {
            self.apply_share_marks();
        }
    }

    fn share_drag_end(self: &Rc<Self>, x: f64, y: f64) {
        self.share_drag_update(x, y);
        let drag = SHARE.with_borrow_mut(|share| {
            let share = share.as_mut()?;
            if let Some(source) = share.autoscroll.take() {
                source.remove();
            }
            share.drag.take()
        });
        // A sweep across messages chooses the range; a plain click waits
        // for the second one.
        if let Some(Drag::Picking) = drag {
            with_selection(|selection| {
                if let (Some(anchor), Some(hover)) = (selection.anchor, selection.hover)
                    && anchor != hover
                {
                    selection.finish(anchor, hover);
                }
            });
            self.apply_share_marks();
        }
    }

    /// While dragging near the top or bottom, scrolls the list (loading
    /// older messages at the top) and keeps the dragged end under the
    /// pointer.
    fn start_autoscroll(self: &Rc<Self>) {
        let app = self.clone();
        let source = glib::timeout_add_local(Duration::from_millis(30), move || {
            let Some((_, y)) = SHARE.with_borrow(|share| share.as_ref().map(|share| share.pointer)) else {
                return glib::ControlFlow::Break;
            };
            let height = f64::from(app.message_overlay.height());
            let step = if y < EDGE {
                -(EDGE - y).min(EDGE) * 0.6 - 2.0
            } else if y > height - EDGE {
                (y - (height - EDGE)).min(EDGE) * 0.6 + 2.0
            } else {
                0.0
            };
            if step != 0.0 {
                let adjustment = app.messages_scroll.vadjustment();
                let max = adjustment.upper() - adjustment.page_size();
                adjustment.set_value((adjustment.value() + step).clamp(adjustment.lower(), max));
                app.follow_pointer();
            }
            glib::ControlFlow::Continue
        });
        SHARE.with_borrow_mut(|share| {
            if let Some(share) = share
                && let Some(old) = share.autoscroll.replace(source)
            {
                old.remove();
            }
        });
    }

    // -- export ----------------------------------------------------------

    /// The Export menu: picture or text, a few options, then copy or save.
    fn export_popover(self: &Rc<Self>) -> gtk::Popover {
        let popover = gtk::Popover::new();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(6);
        content.set_margin_end(6);
        content.set_width_request(280);

        let heading = label("Export", &["heading"]);
        heading.set_xalign(0.0);
        content.append(&heading);

        let formats = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        formats.add_css_class("linked");
        formats.set_homogeneous(true);
        let picture = gtk::ToggleButton::with_label("Picture");
        let text = gtk::ToggleButton::with_label("Text");
        text.set_group(Some(&picture));
        picture.set_active(true);
        formats.append(&picture);
        formats.append(&text);
        content.append(&formats);

        let times = gtk::CheckButton::with_label("Show times");
        times.set_active(true);
        content.append(&times);
        let high_resolution = gtk::CheckButton::with_label("High resolution (2×)");
        high_resolution.set_active(true);
        content.append(&high_resolution);
        let style_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        style_row.append(&label("Style", &[]));
        let style = gtk::DropDown::from_strings(&["Plain text", "Markdown"]);
        style.set_hexpand(true);
        style_row.append(&style);
        style_row.set_visible(false);
        content.append(&style_row);
        let redacted_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        redacted_row.append(&label("Redacted", &[]));
        let redacted = gtk::DropDown::from_strings(&["Blocked out", "Left out"]);
        redacted.set_hexpand(true);
        redacted_row.append(&redacted);
        content.append(&redacted_row);

        // Picture and text options show for their own format.
        let (hires, style_ref) = (high_resolution.clone(), style_row.clone());
        picture.connect_toggled(move |picture| {
            hires.set_visible(picture.is_active());
            style_ref.set_visible(!picture.is_active());
        });

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.set_homogeneous(true);
        let copy = gtk::Button::with_label("Copy to Clipboard");
        let save = gtk::Button::with_label("Save as File…");
        save.add_css_class("suggested-action");
        actions.append(&copy);
        actions.append(&save);
        content.append(&actions);
        popover.set_child(Some(&content));

        let read = move || ExportOptions {
            picture: picture.is_active(),
            times: times.is_active(),
            leave_out_redacted: redacted.selected() == 1,
            markdown: style.selected() == 1,
            high_resolution: high_resolution.is_active(),
        };
        let read = Rc::new(read);
        for (button, target) in [(copy, Target::Clipboard), (save, Target::File)] {
            let app = self.clone();
            let read = read.clone();
            let popover = popover.downgrade();
            button.connect_clicked(move |_| {
                if let Some(popover) = popover.upgrade() {
                    popover.popdown();
                }
                app.export_share(read(), target);
            });
        }
        popover
    }

    /// Gathers the shared messages (fetching any no longer loaded), then
    /// exports them.
    fn export_share(self: &Rc<Self>, options: ExportOptions, target: Target) {
        let (Some(selection), Some(channel)) = (selection(), active_channel()) else {
            return;
        };
        let Some((low, high)) = selection.bounds() else {
            return;
        };
        let Some((_, http, store)) = self.active() else {
            return;
        };
        let loaded: Option<Vec<Message>> = store.read(|state| {
            let messages = state.messages.get(&channel)?.messages();
            // The store keeps one unbroken stretch: it has them all when that
            // stretch covers both bounds.
            let covered = messages.first()?.id <= low && messages.last()?.id >= high;
            covered.then(|| {
                messages
                    .iter()
                    .filter(|message| low <= message.id && message.id <= high)
                    .cloned()
                    .collect()
            })
        });
        let leave_out = options.leave_out_redacted;
        let items =
            move |messages: Vec<Message>| export_items(messages, &selection, leave_out);
        if let Some(messages) = loaded {
            self.finish_export(items(messages), options, target);
            return;
        }
        self.set_status("Getting the messages to share…");
        crate::video_setup::spawn_local(
            self,
            async move {
                let mut found: Vec<Message> = Vec::new();
                let mut after = Snowflake::new(low.get().saturating_sub(1));
                for _ in 0..MAX_FETCH_PAGES {
                    let mut page = http.messages_after(channel, 100, after).await?;
                    page.sort_by_key(|message| message.id);
                    let full = page.len() == 100;
                    let Some(last) = page.last().map(|message| message.id) else {
                        break;
                    };
                    found.extend(page.into_iter().filter(|message| message.id <= high));
                    if last >= high || !full {
                        break;
                    }
                    after = last;
                }
                // Deleted messages the logger kept are not on Discord any more.
                found.extend(crate::logger::deleted_between(channel, low, high));
                found.sort_by_key(|message| message.id);
                found.dedup_by_key(|message| message.id);
                let mut edits = crate::logger::edits_between(channel, low, high);
                for message in &mut found {
                    if message.edits.is_empty()
                        && let Some(earlier) = edits.remove(&message.id)
                    {
                        message.edits = earlier;
                    }
                }
                Ok::<_, kc_http::Error>(found)
            },
            move |app, result| match result {
                Ok(messages) => {
                    app.set_status("");
                    app.finish_export(items(messages), options, target);
                }
                Err(error) => app.set_status(&format!("Could not get the messages: {error}")),
            },
        );
    }

    fn finish_export(self: &Rc<Self>, items: Vec<(Message, Mark)>, options: ExportOptions, target: Target) {
        if items.is_empty() {
            self.set_status("Nothing to share");
            return;
        }
        if options.picture {
            self.export_picture(items, options, target);
        } else {
            self.export_text(&items, options, target);
        }
    }

    fn export_text(self: &Rc<Self>, items: &[(Message, Mark)], options: ExportOptions, target: Target) {
        let guild = self
            .with_session(|session| session.selected_guild.filter(|_| !session.dm_mode))
            .flatten();
        let text = self
            .with_session(|session| {
                session.store.read(|state| {
                    let resolver = StateResolver { state, guild };
                    format_text(
                        items,
                        &|message| state.display_name(guild, &message.author),
                        &|message| message_text(message, &resolver),
                        &|message| crate::content::format_time(&message.timestamp),
                        &|message| {
                            message.is_system().then(|| {
                                let (_, markup) = self.system_text(message, guild, state);
                                gtk::pango::parse_markup(&markup, '\0')
                                    .map(|(_, text, _)| text.to_string())
                                    .unwrap_or(markup)
                            })
                        },
                        options,
                    )
                })
            })
            .unwrap_or_default();
        match target {
            Target::Clipboard => {
                self.window.clipboard().set_text(&text);
                self.set_status("Copied the messages");
            }
            Target::File => {
                let extension = if options.markdown { "md" } else { "txt" };
                self.save_files(
                    vec![glib::Bytes::from_owned(text.into_bytes())],
                    &self.share_file_name(),
                    extension,
                );
            }
        }
    }

    /// A file name for the export: the channel's name and today's date.
    fn share_file_name(&self) -> String {
        let channel = self
            .with_session(|session| {
                let id = session.selected_channel?;
                session.store.read(|state| state.channels.get(&id).map(|channel| channel.display_name()))
            })
            .flatten()
            .unwrap_or_else(|| "messages".to_owned());
        let date = glib::DateTime::now_local()
            .ok()
            .and_then(|now| now.format("%Y-%m-%d").ok())
            .map(|date| date.to_string())
            .unwrap_or_default();
        let safe: String = channel
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
            .collect();
        format!("{safe}-{date}")
    }

    /// Asks where to save, then writes `parts` there (numbered when more
    /// than one).
    fn save_files(self: &Rc<Self>, parts: Vec<glib::Bytes>, stem: &str, extension: &str) {
        if let Some(folder) = DEMO_FOLDER.with_borrow(Clone::clone) {
            for (index, bytes) in parts.iter().enumerate() {
                let path = folder.join(format!("{stem}-{index}.{extension}"));
                if let Err(error) = std::fs::write(&path, bytes) {
                    eprintln!("demo: could not save {}: {error}", path.display());
                } else {
                    eprintln!("demo: saved {}", path.display());
                }
            }
            return;
        }
        let dialog = gtk::FileDialog::new();
        dialog.set_initial_name(Some(&format!("{stem}.{extension}")));
        let app = self.clone();
        let extension = extension.to_owned();
        dialog.save(Some(&self.window), gtk::gio::Cancellable::NONE, move |result| {
            let Ok(file) = result else {
                return;
            };
            let app = app.clone();
            let extension = extension.clone();
            glib::spawn_future_local(async move {
                let count = parts.len();
                let files: Vec<gtk::gio::File> = if count == 1 {
                    vec![file]
                } else {
                    let stem = file
                        .basename()
                        .map(|name| name.to_string_lossy().trim_end_matches(&format!(".{extension}")).to_owned())
                        .unwrap_or_else(|| "messages".to_owned());
                    let folder = file.parent();
                    (1..=count)
                        .filter_map(|index| {
                            folder.as_ref().map(|folder| folder.child(format!("{stem}-{index}.{extension}")))
                        })
                        .collect()
                };
                for (file, bytes) in files.iter().zip(parts) {
                    if let Err((_, error)) = file
                        .replace_contents_future(bytes, None, false, gtk::gio::FileCreateFlags::REPLACE_DESTINATION)
                        .await
                    {
                        app.set_status(&format!("Could not save: {error}"));
                        return;
                    }
                }
                app.set_status(&if count == 1 {
                    "Saved the messages".to_owned()
                } else {
                    format!("Saved the messages in {count} pictures")
                });
            });
        });
    }

    /// Builds the shared messages as they look in the chat (redacted ones
    /// blocked out), renders them once laid out, then copies or saves the
    /// picture.
    fn export_picture(self: &Rc<Self>, items: Vec<(Message, Mark)>, options: ExportOptions, target: Target) {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("kc-share-render");
        if !options.times {
            root.add_css_class("kc-no-times");
        }
        root.set_size_request(RENDER_WIDTH, -1);
        let guild = self
            .with_session(|session| session.selected_guild.filter(|_| !session.dm_mode))
            .flatten();
        self.with_session(|session| {
            session.store.read(|state| {
                let mut previous: Option<&Message> = None;
                for (message, mark) in &items {
                    match mark {
                        Mark::Out => continue,
                        Mark::Skipped(count) => {
                            root.append(&skipped_row(*count));
                            previous = None;
                        }
                        Mark::Redacted if options.leave_out_redacted => continue,
                        Mark::Redacted => {
                            root.append(&redacted_row(message));
                            previous = None;
                        }
                        Mark::In => {
                            let headed = !previous
                                .is_some_and(|previous| crate::chat::continues(previous, message));
                            let body = self.share_body(message, headed, guild, state);
                            body.add_css_class("kc-message");
                            if headed {
                                body.add_css_class("kc-first");
                            }
                            root.append(&body);
                            previous = Some(message);
                        }
                    }
                }
            });
        });

        // Laid out at full size in the window, behind a one-pixel view (GTK
        // does not draw what sits in something transparent).
        let holder = gtk::ScrolledWindow::new();
        holder.set_child(Some(&root));
        holder.set_can_target(false);
        holder.set_policy(gtk::PolicyType::External, gtk::PolicyType::External);
        holder.set_halign(gtk::Align::Start);
        holder.set_valign(gtk::Align::Start);
        holder.set_size_request(1, 1);
        holder.set_propagate_natural_width(false);
        holder.set_propagate_natural_height(false);
        self.message_overlay.add_overlay(&holder);
        self.set_status("Drawing the picture…");
        let app = self.clone();
        let drawn = move |result: Result<Vec<gtk::gdk::Texture>, String>| {
            app.message_overlay.remove_overlay(&holder);
            let parts = match result {
                Ok(parts) => parts,
                Err(error) => {
                    app.set_status(&format!("Could not draw the picture: {error}"));
                    return;
                }
            };
            match target {
                Target::Clipboard => {
                    if parts.len() > 1 {
                        app.set_status("Too long to copy as one picture: save it as a file instead");
                        return;
                    }
                    app.window.clipboard().set_texture(&parts[0]);
                    app.set_status("Copied the picture");
                }
                Target::File => {
                    let pngs = parts.iter().map(|part| part.save_to_png_bytes()).collect();
                    app.save_files(pngs, &app.share_file_name(), "png");
                }
            }
        };
        let window = self.window.clone();
        let high_resolution = options.high_resolution;
        when_media_loaded(&root, move |root| {
            capture_after_paint(&root, high_resolution, move |captured| {
                drawn(captured.and_then(|(node, width, height, scale)| {
                    render_parts(&window, &node, width, height, scale)
                }));
            });
        });
    }
}

impl AppState {
    /// The demo: share mode driven through its own input handlers (two
    /// clicks, a redaction inside the range, a message added outside it),
    /// then both exports saved to `folder`.
    pub(crate) fn demo_share(self: &Rc<Self>, folder: Option<std::path::PathBuf>) {
        self.enter_share_mode();
        let centre = |index_from_end: usize| -> Option<(f64, f64, Snowflake)> {
            let chat = self.chat.borrow();
            let entry = chat.rows.iter().rev().nth(index_from_end)?;
            let bounds = entry.body.compute_bounds(&self.message_overlay)?;
            Some((
                f64::from(bounds.x() + bounds.width() / 2.0),
                f64::from(bounds.y() + bounds.height() / 2.0),
                entry.id,
            ))
        };
        let click = |index_from_end: usize| {
            if let Some((x, y, _)) = centre(index_from_end) {
                self.share_drag_begin(x, y);
                self.share_drag_end(x, y);
            }
        };
        click(7);
        click(3);
        if let (Some((x, y, _)), Some((x2, y2, _))) = (centre(5), centre(0)) {
            for (x, y) in [(x, y), (x2, y2)] {
                if let Some(id) = self.share_row_at(x, y, false) {
                    with_selection(|selection| selection.toggle(id));
                }
            }
            self.apply_share_marks();
        }
        if let Some(selection) = selection() {
            eprintln!(
                "demo: range {:?}, redacted {:?}, added {:?}",
                selection.range, selection.excluded, selection.included
            );
        }
        // Drag the bottom handle down onto the last message, once laid out.
        let app = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(300), move || {
            let bottom = SHARE.with_borrow(|share| share.as_ref().map(|share| share.bottom.clone()));
            let (Some(bottom), Some(last)) = (bottom, centre_of_last(&app)) else {
                return;
            };
            if let Some(bounds) = bottom.compute_bounds(&app.message_overlay) {
                let (x, y) = (
                    f64::from(bounds.x() + bounds.width() / 2.0),
                    f64::from(bounds.y() + bounds.height() / 2.0),
                );
                let before = selection().and_then(|selection| selection.range);
                app.share_drag_begin(x, y);
                app.share_drag_update(x, last);
                app.share_drag_end(x, last);
                let after = selection().and_then(|selection| selection.range);
                eprintln!("demo: dragged the end: {before:?} -> {after:?}");
            }
        });
        // KUBECORD_SHARE_CANCEL: the close button clicked (as a real click
        // does, with the focus on it), and the reading position compared.
        if std::env::var_os("KUBECORD_SHARE_CANCEL").is_some() {
            let app = self.clone();
            // `up`: scrolled up first, off the bottom.
            if std::env::var("KUBECORD_SHARE_CANCEL").as_deref() == Ok("up") {
                let adjustment = self.messages_scroll.vadjustment();
                glib::timeout_add_local_once(Duration::from_millis(900), move || {
                    adjustment.set_value(adjustment.value() - 300.0);
                });
            }
            glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                let adjustment = app.messages_scroll.vadjustment();
                let before = adjustment.value();
                let gap = |adjustment: &gtk::Adjustment| adjustment.upper() - adjustment.page_size() - adjustment.value();
                eprintln!("demo: before cancel: {} from the bottom", gap(&adjustment));
                let cancel = SHARE.with_borrow(|share| {
                    share.as_ref().and_then(|share| share.bar.last_child()).and_downcast::<gtk::Button>()
                });
                if let Some(cancel) = cancel {
                    cancel.grab_focus();
                    cancel.emit_clicked();
                }
                let app = app.clone();
                glib::timeout_add_local_once(Duration::from_millis(500), move || {
                    let after = app.messages_scroll.vadjustment().value();
                    eprintln!("demo: after cancel: {} from the bottom", gap(&app.messages_scroll.vadjustment()));
                    eprintln!("demo: cancel scroll {before} -> {after} (focus {:?})", gtk::prelude::GtkWindowExt::focus(&app.window).map(|w| w.type_().name()));
                });
            });
        }
        DEMO_FOLDER.with_borrow_mut(|slot| *slot = folder.clone());
        if folder.is_some() {
            let options = ExportOptions {
                picture: true,
                times: true,
                leave_out_redacted: false,
                markdown: false,
                high_resolution: true,
            };
            self.export_share(options, Target::File);
            self.export_share(ExportOptions { picture: false, ..options }, Target::File);
            self.export_share(
                ExportOptions {
                    picture: false,
                    markdown: true,
                    leave_out_redacted: true,
                    ..options
                },
                Target::File,
            );
        }
    }
}

/// The y of the last row's middle, in the overlay's coordinates.
fn centre_of_last(app: &AppState) -> Option<f64> {
    let chat = app.chat.borrow();
    let bounds = chat.rows.last()?.body.compute_bounds(&app.message_overlay)?;
    Some(f64::from(bounds.y() + bounds.height() / 2.0))
}

/// Shows a row's part in the share: classes for the highlight, and a
/// "Redacted" note (not blurred) under a redacted message.
fn mark_body(body: &gtk::Box, mark: Mark, first: bool, last: bool) {
    let mut child = body.last_child();
    let note = loop {
        let Some(widget) = child else {
            break None;
        };
        if widget.has_css_class("kc-redacted-note") {
            break Some(widget);
        }
        child = widget.prev_sibling();
    };
    match (mark == Mark::Redacted, note) {
        (true, None) => {
            let note = gtk::Label::new(Some("Redacted"));
            note.add_css_class("kc-redacted-note");
            note.set_halign(gtk::Align::Start);
            note.set_margin_start(56);
            body.append(&note);
        }
        (false, Some(note)) => body.remove(&note),
        _ => {}
    }
    for (class, on) in [
        ("kc-share-selected", mark == Mark::In),
        ("kc-share-redacted", mark == Mark::Redacted),
        ("kc-share-first", first),
        ("kc-share-last", last),
    ] {
        if on {
            body.add_css_class(class);
        } else {
            body.remove_css_class(class);
        }
    }
}

/// The line in a picture between shared messages that were not
/// neighbours: how many were in between.
fn skipped_row(count: usize) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.set_margin_top(10);
    row.set_margin_bottom(10);
    row.set_margin_start(16);
    row.set_margin_end(16);
    let left = gtk::Separator::new(gtk::Orientation::Horizontal);
    left.set_hexpand(true);
    left.set_valign(gtk::Align::Center);
    let right = gtk::Separator::new(gtk::Orientation::Horizontal);
    right.set_hexpand(true);
    right.set_valign(gtk::Align::Center);
    row.append(&left);
    row.append(&label(&skipped_text(count), &["kc-divider"]));
    row.append(&right);
    row
}

/// A redacted message in a picture: bars where its author and text were,
/// as long as the text was (roughly).
fn redacted_row(message: &Message) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    row.add_css_class("kc-message");
    row.add_css_class("kc-first");
    let avatar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    avatar.add_css_class("kc-redacted-avatar");
    avatar.set_size_request(40, 40);
    avatar.set_valign(gtk::Align::Start);
    row.append(&avatar);
    let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
    column.set_margin_top(4);
    let bar = |width: i32| {
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        bar.add_css_class("kc-redacted-bar");
        bar.set_size_request(width, 12);
        bar.set_halign(gtk::Align::Start);
        bar
    };
    // Said in words, so it reads as redacted rather than missing.
    let note = gtk::Label::new(Some("Redacted"));
    note.add_css_class("kc-redacted-note");
    note.set_halign(gtk::Align::Start);
    column.append(&note);
    let length = message.content.chars().count().max(12) as i32;
    let mut left = length * 7;
    while left > 0 {
        let width = left.min(560);
        column.append(&bar(width));
        left -= width;
        if column.observe_children().n_items() > 6 {
            break;
        }
    }
    row.append(&column);
    row
}

/// Calls `ready` once the pictures in `root` have loaded (a GIF or image
/// still downloading would be left out), or after [`MEDIA_WAIT`] for any
/// that never do.
fn when_media_loaded(root: &gtk::Box, ready: impl FnOnce(gtk::Box) + 'static) {
    let started = std::time::Instant::now();
    let root = root.clone();
    let mut ready = Some(ready);
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let waited = started.elapsed();
        if waited < SETTLE || (waited < MEDIA_WAIT && media_pending(root.upcast_ref())) {
            return glib::ControlFlow::Continue;
        }
        if let Some(ready) = ready.take() {
            ready(root.clone());
        }
        glib::ControlFlow::Break
    });
}

/// Whether a picture under `widget` is still waiting for its image. A
/// spoiler not revealed never loads, so it is not waited for.
fn media_pending(widget: &gtk::Widget) -> bool {
    if let Some(picture) = widget.downcast_ref::<gtk::Picture>()
        && picture.paintable().is_none()
        && !picture.has_css_class("kc-unrevealed")
    {
        return true;
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        if media_pending(&widget) {
            return true;
        }
        child = widget.next_sibling();
    }
    false
}

/// What `root` looks like, taken right after the window draws a frame,
/// with its size and scale. In between, a playing GIF or video that moved
/// on a frame has thrown the last drawing away and there is nothing to
/// take, so a fixed moment was a gamble with GIFs on screen.
type Captured = (gtk::gsk::RenderNode, i32, i32, f64);

fn capture_after_paint(root: &gtk::Box, high_resolution: bool, done: impl FnOnce(Result<Captured, String>) + 'static) {
    let Some(clock) = root.frame_clock() else {
        done(capture(root, high_resolution));
        return;
    };
    let handler = Rc::new(RefCell::new(None));
    let done = RefCell::new(Some(done));
    let frames = std::cell::Cell::new(0);
    let weak = root.downgrade();
    let slot = handler.clone();
    let id = clock.connect_after_paint(move |clock| {
        let Some(root) = weak.upgrade() else {
            return;
        };
        frames.set(frames.get() + 1);
        let captured = capture(&root, high_resolution);
        if captured.is_err() && frames.get() < MAX_CAPTURE_FRAMES {
            root.queue_draw();
            return;
        }
        if let Some(id) = slot.borrow_mut().take() {
            clock.disconnect(id);
        }
        // Drawn outside the frame, once it is finished.
        if let Some(done) = done.borrow_mut().take() {
            glib::idle_add_local_once(move || done(captured));
        }
    });
    *handler.borrow_mut() = Some(id);
    root.queue_draw();
}

/// The drawing `root` was last drawn with, padding included.
fn capture(root: &gtk::Box, high_resolution: bool) -> Result<Captured, String> {
    // The whole box, padding included: `width()`/`height()` leave the
    // padding out, and the paintable scales the box into whatever size it
    // is drawn at, which squashed the picture (more so vertically).
    let bounds = root.compute_bounds(root).ok_or("nothing was laid out")?;
    let width = bounds.width().ceil() as i32;
    let height = bounds.height().ceil() as i32;
    if width <= 0 || height <= 0 {
        return Err("nothing was laid out".to_owned());
    }
    let mut scale = if high_resolution { 2.0 } else { 1.0 };
    if (f64::from(height) * scale) as i32 > MAX_HIRES_HEIGHT {
        scale = 1.0;
    }
    let paintable = gtk::WidgetPaintable::new(Some(root));
    let snapshot = gtk::Snapshot::new();
    snapshot.scale(scale as f32, scale as f32);
    paintable.snapshot(&snapshot, f64::from(width), f64::from(height));
    let node = snapshot.to_node().ok_or("nothing to draw")?;
    Ok((node, width, height, scale))
}

/// Renders `node` to pictures at most [`PART_HEIGHT`] tall each.
fn render_parts(
    window: &libadwaita::ApplicationWindow,
    node: &gtk::gsk::RenderNode,
    width: i32,
    height: i32,
    scale: f64,
) -> Result<Vec<gtk::gdk::Texture>, String> {
    let renderer = window.renderer().ok_or("no renderer")?;
    let full_width = (f64::from(width) * scale).ceil() as i32;
    let full_height = (f64::from(height) * scale).ceil() as i32;
    let stride = full_width as usize * 4;
    let mut parts = Vec::new();
    let mut part_top = 0;
    while part_top < full_height {
        let part_height = (full_height - part_top).min(PART_HEIGHT);
        let mut pixels = vec![0u8; stride * part_height as usize];
        let mut top = 0;
        while top < part_height {
            let tile = (part_height - top).min(TILE);
            let area = gtk::graphene::Rect::new(0.0, (part_top + top) as f32, full_width as f32, tile as f32);
            let texture = renderer.render_texture(node, Some(&area));
            let offset = top as usize * stride;
            texture.download(&mut pixels[offset..offset + stride * tile as usize], stride);
            top += tile;
        }
        let bytes = glib::Bytes::from_owned(pixels);
        let texture = gtk::gdk::MemoryTexture::new(
            full_width,
            part_height,
            gtk::gdk::MemoryFormat::B8g8r8a8Premultiplied,
            &bytes,
            stride,
        );
        parts.push(texture.upcast());
        part_top += part_height;
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> Snowflake {
        Snowflake::new(n)
    }

    #[test]
    fn two_clicks_choose_a_range_and_right_clicks_toggle() {
        let mut selection = Selection::default();
        selection.click(id(30));
        assert_eq!(selection.range, None);
        selection.hover = Some(id(10));
        assert_eq!(selection.mark(id(20)), Mark::In);
        selection.click(id(10));
        assert_eq!(selection.range, Some((id(10), id(30))));
        // Inside the range: redact, then restore.
        selection.toggle(id(20));
        assert_eq!(selection.mark(id(20)), Mark::Redacted);
        selection.toggle(id(20));
        assert_eq!(selection.mark(id(20)), Mark::In);
        // Outside: add, then remove.
        assert_eq!(selection.mark(id(50)), Mark::Out);
        selection.toggle(id(50));
        assert_eq!(selection.mark(id(50)), Mark::In);
        assert_eq!(selection.bounds(), Some((id(10), id(50))));
        selection.toggle(id(50));
        assert_eq!(selection.mark(id(50)), Mark::Out);
    }

    #[test]
    fn moving_an_end_past_the_other_swaps_them() {
        let mut selection = Selection::default();
        selection.finish(id(10), id(20));
        // Dragging the start (the end 20 stays) down past 20.
        let fixed = id(20);
        let moved = id(25);
        selection.range = Some((fixed.min(moved), fixed.max(moved)));
        assert_eq!(selection.range, Some((id(20), id(25))));
    }

    #[test]
    fn messages_not_next_to_each_other_say_how_many_are_between() {
        let message = |n: u64| -> Message {
            serde_json::from_value(serde_json::json!({
                "id": n.to_string(), "channel_id": "1", "content": "",
                "author": {"id": "1", "username": "a"}, "timestamp": "2026-11-21T10:00:00+00:00"
            }))
            .unwrap()
        };
        let mut selection = Selection::default();
        // Picked singly: 2 and 6; a range 8..=9 with 9 redacted.
        selection.toggle(id(2));
        selection.toggle(id(6));
        selection.finish(id(8), id(9));
        selection.toggle(id(9));
        let all: Vec<Message> = (1..=10).map(message).collect();
        let marks = |items: Vec<(Message, Mark)>| -> Vec<(u64, Mark)> {
            items.into_iter().map(|(message, mark)| (message.id.get(), mark)).collect()
        };
        assert_eq!(
            marks(export_items(all.clone(), &selection, false)),
            vec![
                (2, Mark::In),
                (3, Mark::Skipped(3)),
                (6, Mark::In),
                (7, Mark::Skipped(1)),
                (8, Mark::In),
                (9, Mark::Redacted),
            ]
        );
        // Redacted ones left out are not shown, so they are not at the end
        // either (nothing follows them).
        assert_eq!(
            marks(export_items(all, &selection, true)),
            vec![(2, Mark::In), (3, Mark::Skipped(3)), (6, Mark::In), (7, Mark::Skipped(1)), (8, Mark::In)]
        );
        assert_eq!(skipped_text(1), "1 message skipped");
    }

    #[test]
    fn edit_history_comes_before_the_text() {
        let mut message: Message = serde_json::from_value(serde_json::json!({
            "id": "1", "channel_id": "1", "content": "Meet at 6.",
            "author": {"id": "7", "username": "linus"}, "timestamp": "2026-11-21T10:00:00+00:00"
        }))
        .unwrap();
        message.edits = vec![kc_state::MessageEdit { content: "Meet at 5.".to_owned(), at: String::new() }];
        let items = vec![(message, Mark::In)];
        let name = |m: &Message| m.author.username.clone();
        let body = |m: &Message| m.content.clone();
        let time = |_: &Message| "10:00".to_owned();
        let system = |_: &Message| None;
        let mut options = ExportOptions {
            picture: false,
            times: false,
            leave_out_redacted: false,
            markdown: false,
            high_resolution: false,
        };
        assert_eq!(
            format_text(&items, &name, &body, &time, &system, options),
            "linus: [before edit] Meet at 5.\n    Meet at 6.\n"
        );
        options.markdown = true;
        assert_eq!(
            format_text(&items, &name, &body, &time, &system, options),
            "**linus**\n~~Meet at 5.~~ *(edited)*\nMeet at 6.\n"
        );
    }

    #[test]
    fn text_export_in_both_styles() {
        let message = |n: u64, author: u64, text: &str| -> Message {
            serde_json::from_value(serde_json::json!({
                "id": n.to_string(), "channel_id": "1", "content": text,
                "author": {"id": author.to_string(), "username": format!("user{author}")},
                "timestamp": "2026-11-21T10:00:00+00:00"
            }))
            .unwrap()
        };
        let mut call = message(5, 8, "");
        call.kind = 3;
        let mut gone = message(4, 8, "bye");
        gone.deleted = true;
        let items = vec![
            (message(1, 7, "hello\nthere"), Mark::In),
            (message(2, 7, "again"), Mark::In),
            (message(3, 8, "secret"), Mark::Redacted),
            (gone, Mark::In),
            (message(5, 8, ""), Mark::Skipped(2)),
            (call, Mark::In),
        ];
        let name = |m: &Message| m.author.username.clone();
        let body = |m: &Message| m.content.clone();
        let time = |_: &Message| "10:00".to_owned();
        let system = |m: &Message| (m.kind == 3).then(|| format!("{} started a call.", m.author.username));
        let mut options = ExportOptions {
            picture: false,
            times: true,
            leave_out_redacted: false,
            markdown: false,
            high_resolution: false,
        };
        assert_eq!(
            format_text(&items, &name, &body, &time, &system, options),
            "[10:00] user7: hello\n    there\n[10:00] user7: again\n[redacted message]\n[10:00] user8: bye [deleted]\n··· 2 messages skipped ···\n[10:00] user8 started a call.\n"
        );
        options.markdown = true;
        options.leave_out_redacted = true;
        options.times = false;
        assert_eq!(
            format_text(&items, &name, &body, &time, &system, options),
            "**user7**\nhello\nthere\nagain\n\n**user8**\nbye *(deleted)*\n\n*··· 2 messages skipped ···*\n\n*user8 started a call.*\n"
        );
    }
}
