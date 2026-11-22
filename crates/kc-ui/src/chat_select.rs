//! Selecting text across several messages.
//!
//! Each message's text is its own widget, and GTK selects within one widget
//! only. A drag gesture on the message list watches (without taking part)
//! while a selection stays inside one message; once the pointer moves into
//! another message it takes the drag over and selects from where it began to
//! the pointer itself: the rest of the first message, every message between
//! in full, and the last one up to the pointer. Ctrl+C copies all of it, one
//! message per line, with the author's name where a new author starts.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};
use kc_core::Snowflake;

use crate::shell::AppState;

/// A point in the selection: a message (by its index in the list) and a
/// character offset in its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Point {
    row: usize,
    offset: usize,
}

#[derive(Default)]
struct Selection {
    /// Where the drag began (in list coordinates), and the point there.
    start: Option<(f64, f64, Point)>,
    /// The drag has crossed into another message: the selection is ours.
    across: bool,
    /// The point under the pointer.
    end: Option<Point>,
    /// The text widgets given a selection, to clear later.
    marked: Vec<gtk::Widget>,
}

thread_local! {
    static SELECTION: RefCell<Selection> = RefCell::new(Selection::default());
}

/// The message text widget (label or text view) in a message's body.
fn text_of(body: &gtk::Widget) -> Option<gtk::Widget> {
    if body.has_css_class("kc-message-text") {
        return Some(body.clone());
    }
    let mut child = body.first_child();
    while let Some(widget) = child {
        if let Some(found) = text_of(&widget) {
            return Some(found);
        }
        child = widget.next_sibling();
    }
    None
}

/// How many characters a text widget holds.
fn length(text: &gtk::Widget) -> usize {
    if let Some(label) = text.downcast_ref::<gtk::Label>() {
        label.text().chars().count()
    } else if let Some(view) = text.downcast_ref::<gtk::TextView>() {
        view.buffer().char_count().max(0) as usize
    } else {
        0
    }
}

/// The character offset in `text` under `(x, y)` (in `text`'s coordinates):
/// 0 above it, the end below it.
fn offset_at(text: &gtk::Widget, x: f64, y: f64) -> usize {
    if y < 0.0 {
        return 0;
    }
    if y > f64::from(text.height()) {
        return length(text);
    }
    if let Some(label) = text.downcast_ref::<gtk::Label>() {
        let layout = label.layout();
        let (dx, dy) = label.layout_offsets();
        let scale = f64::from(gtk::pango::SCALE);
        let (_, index, trailing) =
            layout.xy_to_index(((x - f64::from(dx)) * scale) as i32, ((y - f64::from(dy)) * scale) as i32);
        let label_text = label.text();
        let index = (index.max(0) as usize).min(label_text.len());
        label_text[..index].chars().count() + trailing.max(0) as usize
    } else if let Some(view) = text.downcast_ref::<gtk::TextView>() {
        let (bx, by) = view.window_to_buffer_coords(gtk::TextWindowType::Text, x as i32, y as i32);
        view.iter_at_location(bx, by)
            .map_or_else(|| length(text), |iter| iter.offset().max(0) as usize)
    } else {
        0
    }
}

/// Selects characters `from..to` of `text` (a label or text view); an empty
/// range clears it.
fn select(text: &gtk::Widget, from: usize, to: usize) {
    if let Some(label) = text.downcast_ref::<gtk::Label>() {
        // A label draws its own selection only while focused, and only one
        // widget has the focus: the range is highlighted with a background
        // over its text instead (laid on top of the label's markup).
        if from >= to {
            label.set_attributes(None);
            return;
        }
        let text = label.text();
        let byte = |chars: usize| text.char_indices().nth(chars).map_or(text.len(), |(byte, _)| byte);
        let color = libadwaita::StyleManager::default().accent_color_rgba();
        let scale = |channel: f32| (channel.clamp(0.0, 1.0) * 65535.0) as u16;
        let attributes = gtk::pango::AttrList::new();
        let mut background =
            gtk::pango::AttrColor::new_background(scale(color.red()), scale(color.green()), scale(color.blue()));
        background.set_start_index(byte(from) as u32);
        background.set_end_index(byte(to) as u32);
        attributes.insert(background);
        let mut alpha = gtk::pango::AttrInt::new_background_alpha((0.40 * 65535.0) as u16);
        alpha.set_start_index(byte(from) as u32);
        alpha.set_end_index(byte(to) as u32);
        attributes.insert(alpha);
        label.set_attributes(Some(&attributes));
    } else if let Some(view) = text.downcast_ref::<gtk::TextView>() {
        // The same highlight as a label's (its own selection is grey while
        // unfocused), as a tag over the range.
        let buffer = view.buffer();
        let tag = buffer.tag_table().lookup(SELECTED_TAG).unwrap_or_else(|| {
            let color = libadwaita::StyleManager::default().accent_color_rgba().with_alpha(0.40);
            let tag = gtk::TextTag::builder().name(SELECTED_TAG).background_rgba(&color).build();
            buffer.tag_table().add(&tag);
            tag
        });
        let (start, end) = buffer.bounds();
        buffer.remove_tag(&tag, &start, &end);
        if from < to {
            buffer.apply_tag(&tag, &buffer.iter_at_offset(from as i32), &buffer.iter_at_offset(to as i32));
        }
        // A change of tags re-measures the view, at times as if every word
        // wrapped: put its height right.
        crate::content::settle_height(view);
    }
}

/// The text view tag that highlights a selection across messages.
const SELECTED_TAG: &str = "kc-selected-across";

/// The text of characters `from..to` of `text`, emoji included.
fn text_in(text: &gtk::Widget, from: usize, to: usize) -> String {
    if let Some(label) = text.downcast_ref::<gtk::Label>() {
        label.text().chars().skip(from).take(to.saturating_sub(from)).collect()
    } else if let Some(view) = text.downcast_ref::<gtk::TextView>() {
        let buffer = view.buffer();
        crate::content::view_text(view, &buffer.iter_at_offset(from as i32), &buffer.iter_at_offset(to as i32))
    } else {
        String::new()
    }
}

impl AppState {
    /// The message under `(x, y)` (list coordinates) and the offset in its
    /// text there.
    fn select_point(&self, x: f64, y: f64) -> Option<Point> {
        let list = self.messages_list.upcast_ref::<gtk::Widget>();
        let picked = list.pick(x, y, gtk::PickFlags::DEFAULT)?;
        let row = picked.ancestor(gtk::ListBoxRow::static_type())?;
        let chat = self.chat.borrow();
        let index = chat.rows.iter().position(|entry| *entry.row.upcast_ref::<gtk::Widget>() == row)?;
        let Some(text) = text_of(chat.rows[index].body.upcast_ref()) else {
            return Some(Point { row: index, offset: 0 });
        };
        let point = list.compute_point(&text, &gtk::graphene::Point::new(x as f32, y as f32))?;
        Some(Point {
            row: index,
            offset: offset_at(&text, f64::from(point.x()), f64::from(point.y())),
        })
    }

    /// Shows the selection from `a` to `b` (either order) across the rows.
    fn show_selection(&self, a: Point, b: Point) {
        let (first, last) = if (a.row, a.offset) <= (b.row, b.offset) { (a, b) } else { (b, a) };
        let chat = self.chat.borrow();
        let mut marked = Vec::new();
        for index in first.row..=last.row {
            let Some(entry) = chat.rows.get(index) else { continue };
            let Some(text) = text_of(entry.body.upcast_ref()) else { continue };
            let from = if index == first.row { first.offset } else { 0 };
            let to = if index == last.row { last.offset } else { length(&text) };
            select(&text, from, to);
            marked.push(text);
        }
        drop(chat);
        SELECTION.with_borrow_mut(|selection| {
            for old in selection.marked.drain(..) {
                if !marked.contains(&old) {
                    select(&old, 0, 0);
                }
            }
            selection.marked = marked;
        });
    }

    /// Clears a selection across messages.
    fn clear_selection(&self) {
        let marked = SELECTION.with_borrow_mut(|selection| {
            selection.across = false;
            selection.start = None;
            selection.end = None;
            std::mem::take(&mut selection.marked)
        });
        for text in marked {
            select(&text, 0, 0);
        }
    }

    /// The selection across messages as text, if there is one.
    fn selected_across(&self) -> Option<String> {
        let (a, b) = SELECTION.with_borrow(|selection| {
            if selection.across { Some((selection.start?.2, selection.end?)) } else { None }
        })?;
        let (first, last) = if (a.row, a.offset) <= (b.row, b.offset) { (a, b) } else { (b, a) };
        let channel = self.chat.borrow().channel?;
        let chat = self.chat.borrow();
        let ids: Vec<Snowflake> = (first.row..=last.row).filter_map(|index| chat.rows.get(index).map(|entry| entry.id)).collect();
        let authors: Vec<Option<(Snowflake, String)>> = self
            .with_session(|session| {
                let guild = session.selected_guild.filter(|_| !session.dm_mode);
                session.store.read(|state| {
                    ids.iter()
                        .map(|id| {
                            let message = state.messages.get(&channel)?.get(*id)?;
                            Some((message.author.id, state.display_name(guild, &message.author)))
                        })
                        .collect()
                })
            })
            .unwrap_or_default();
        let mut out = String::new();
        let mut previous_author = None;
        for (position, index) in (first.row..=last.row).enumerate() {
            let Some(entry) = chat.rows.get(index) else { continue };
            let Some(text) = text_of(entry.body.upcast_ref()) else { continue };
            let from = if index == first.row { first.offset } else { 0 };
            let to = if index == last.row { last.offset } else { length(&text) };
            let piece = text_in(&text, from, to);
            if piece.is_empty() {
                continue;
            }
            // The author's name where a new author starts (not when the copy
            // begins partway into a message).
            if let Some(Some((author, name))) = authors.get(position)
                && previous_author != Some(*author)
            {
                if !out.is_empty() {
                    out.push('\n');
                }
                if !(index == first.row && from > 0) {
                    out.push_str(name);
                    out.push_str(":\n");
                }
                previous_author = Some(*author);
            } else if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&piece);
        }
        (!out.is_empty()).then_some(out)
    }

    /// Wires selecting across messages. Called once.
    pub(crate) fn connect_cross_select(self: &Rc<Self>) {
        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        // Ahead of the text's own selection, which it takes over once the
        // pointer reaches another message.
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        let app = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, x, y| {
            let Some(app) = app.upgrade() else { return };
            app.clear_selection();
            let point = app.select_point(x, y);
            SELECTION.with_borrow_mut(|selection| {
                selection.start = point.map(|point| (x, y, point));
            });
        });
        let app = Rc::downgrade(self);
        drag.connect_drag_update(move |gesture, dx, dy| {
            let Some(app) = app.upgrade() else { return };
            let Some((x, y, start)) = SELECTION.with_borrow(|selection| selection.start) else {
                return;
            };
            let (x, y) = (x + dx, y + dy);
            let Some(point) = app.select_point(x, y) else { return };
            let across = SELECTION.with_borrow(|selection| selection.across);
            if !across {
                if point.row == start.row {
                    // Still inside one message: its own selection handles it.
                    return;
                }
                // Into another message: the drag is ours from here.
                gesture.set_state(gtk::EventSequenceState::Claimed);
                SELECTION.with_borrow_mut(|selection| selection.across = true);
            }
            SELECTION.with_borrow_mut(|selection| selection.end = Some(point));
            app.show_selection(start, point);
            app.scroll_near_edge(y);
        });
        self.messages_list.add_controller(drag);

        // Ctrl+C copies a selection across messages.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let app = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let Some(app) = app.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let copy = modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                && matches!(key, gdk::Key::c | gdk::Key::C | gdk::Key::Insert);
            if !copy {
                return glib::Propagation::Proceed;
            }
            match app.selected_across() {
                Some(text) => {
                    app.window.clipboard().set_text(&text);
                    glib::Propagation::Stop
                }
                None => glib::Propagation::Proceed,
            }
        });
        self.window.add_controller(keys);
    }

    /// Scrolls the list a little while a selection is dragged near its top
    /// or bottom edge (`y` in list coordinates).
    fn scroll_near_edge(&self, y: f64) {
        let Some(point) = self.messages_list.compute_point(&self.messages_scroll, &gtk::graphene::Point::new(0.0, y as f32)) else {
            return;
        };
        let height = f64::from(self.messages_scroll.height());
        let edge = 40.0;
        let y = f64::from(point.y());
        let step = if y < edge {
            -(edge - y).min(edge) / 2.0
        } else if y > height - edge {
            (y - (height - edge)).min(edge) / 2.0
        } else {
            return;
        };
        let adjustment = self.messages_scroll.vadjustment();
        adjustment.set_value(adjustment.value() + step);
    }
}

impl AppState {
    /// The demo: a selection from the middle of one message's text to the
    /// middle of another three rows later, through the same steps a drag
    /// takes; prints what Ctrl+C would copy.
    pub(crate) fn demo_select_across(&self) {
        let middle = |index_from_end: usize| -> Option<(f64, f64)> {
            let chat = self.chat.borrow();
            let entry = chat.rows.iter().rev().nth(index_from_end)?;
            let text = text_of(entry.body.upcast_ref())?;
            let bounds = text.compute_bounds(&self.messages_list)?;
            Some((f64::from(bounds.x() + bounds.width() / 3.0), f64::from(bounds.y() + bounds.height() / 2.0)))
        };
        let (Some((x1, y1)), Some((x2, y2))) = (middle(4), middle(1)) else {
            eprintln!("demo: no rows to select");
            return;
        };
        let (Some(start), Some(end)) = (self.select_point(x1, y1), self.select_point(x2, y2)) else {
            eprintln!("demo: no points");
            return;
        };
        SELECTION.with_borrow_mut(|selection| {
            selection.start = Some((x1, y1, start));
            selection.end = Some(end);
            selection.across = true;
        });
        self.show_selection(start, end);
        eprintln!("demo: selected {start:?} to {end:?}");
        eprintln!("demo: copies {:?}", self.selected_across());
    }
}
