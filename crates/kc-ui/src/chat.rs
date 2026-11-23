//! The message list: rows, grouping, scroll-back, reactions and message
//! actions.

use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::Snowflake;
use kc_http::cdn;
use kc_state::{Message, Reaction, SendState, State, permissions};

use crate::content::{day_key, format_time, gutter_time, long_date};
use crate::rich;
use crate::shell::AppState;
use crate::widgets::{clear_box, clear_list, empty_state, hex_color, icon_button, label, snippet};

/// Consecutive messages from one author within this window share a header.
const GROUP_WINDOW_MS: u64 = 7 * 60 * 1000;
/// Rows built when a channel is opened; older ones load on scroll.
const INITIAL_ROWS: usize = 60;
/// Rows built at a time from messages already loaded; also the page for a
/// jump to a message (the messages around it).
const PAGE: u8 = 50;

/// How many messages the web client fetches when a channel opens: enough to
/// fill its window, by its own formula (the window less 145 px, at least
/// 400, over a 91 px message, plus 5%), between 10 and 30. Fetching a fixed
/// 50 every time is a pattern no browser makes.
pub(crate) fn initial_page(window_height: i32) -> u8 {
    let viewport = f64::from((window_height - 145).max(400));
    (1.05 * (viewport / 91.0).ceil()).ceil().clamp(10.0, 30.0) as u8
}

/// The page fetched when scrolling past what is loaded: twice the first, as
/// the web client did in a capture (10, then 20).
fn scroll_page(window_height: i32) -> u8 {
    initial_page(window_height) * 2
}

pub(crate) struct RowEntry {
    pub id: Snowflake,
    pub(crate) row: gtk::ListBoxRow,
    /// The message itself, without the day divider above it.
    pub(crate) body: gtk::Box,
}

/// Which message the floating toolbar belongs to.
#[derive(Default)]
pub(crate) struct Hover {
    message: Option<Snowflake>,
    anchor: Option<glib::WeakRef<gtk::Box>>,
    /// The pointer is over the toolbar itself.
    in_toolbar: bool,
    /// A menu or emoji chooser from the toolbar is open.
    menu_open: bool,
    /// Bumped by every hover event; a pending hide only runs if unchanged.
    generation: u64,
}

/// How long the toolbar outlives the pointer leaving its message, so the
/// pointer can travel onto it.
const TOOLBAR_GRACE: std::time::Duration = std::time::Duration::from_millis(150);

/// The rows currently in the message list, oldest first.
#[derive(Default)]
pub(crate) struct ChatView {
    pub channel: Option<Snowflake>,
    pub rows: Vec<RowEntry>,
    /// The view follows new messages while the user is at the bottom.
    pub at_bottom: bool,
    /// `(upper, value)` of the scroll adjustment before older rows were
    /// prepended, to restore the reading position afterwards.
    restore: Option<(f64, f64)>,
    /// A scroll-back fetch is scheduled for the next idle.
    older_queued: bool,
    /// [`AppState::queue_scroll_settle`] is scheduled for the next idle.
    settle_queued: bool,
    /// Drafts of messages shown optimistically, by nonce, kept so a failed
    /// send can be retried.
    pub(crate) pending_sends: std::collections::HashMap<String, crate::composer::OutgoingDraft>,
    /// A page of newer messages is being fetched for a window that stops
    /// short of the present.
    newer_loading: bool,
    /// "Jump to Present", shown while the window stops short of the present.
    present: Option<gtk::Button>,
    /// A message to jump to once this channel's first page of history is in
    /// (set when a notification opens a channel that is still loading).
    pending_jump: Option<(Snowflake, Snowflake)>,
    /// The message just jumped to, held in the middle of the view until
    /// this time while the rows around it are still taking their heights
    /// (images, embeds, wrapped text) and older messages load above it, or
    /// until the user scrolls. By id: its row can be rebuilt meanwhile.
    pin: Option<(Snowflake, std::time::Instant)>,
    /// The message jumped to and when its highlight began; a row rebuilt for
    /// it meanwhile takes the highlight up where it was.
    flash: Option<(Snowflake, std::time::Instant)>,
}

/// How long a message jumped to is held in the middle of the view.
const PIN_TIME: std::time::Duration = std::time::Duration::from_secs(4);
/// A message jumped to within this many rows of the oldest one loaded has
/// too little above it to be centred: older messages are fetched at once.
const NEAR_TOP_ROWS: usize = 12;
/// How long the highlight on a message jumped to stays full, before it fades
/// (over the time in `.kc-flash-out`'s transition).
const FLASH_HOLD: std::time::Duration = std::time::Duration::from_millis(1200);
const FLASH_FADE: std::time::Duration = std::time::Duration::from_millis(2000);

/// Highlights a message jumped to, where it is in its course since
/// `started`: full for a moment, then fading out. A row rebuilt mid-way picks
/// it up where it was.
fn show_flash(body: &gtk::Box, started: std::time::Instant) {
    let elapsed = started.elapsed();
    body.remove_css_class("kc-flash");
    body.remove_css_class("kc-flash-out");
    if elapsed >= FLASH_HOLD + FLASH_FADE {
        return;
    }
    let target = body.downgrade();
    let clear_after = |body: &gtk::Box, left: std::time::Duration| {
        let target = body.downgrade();
        glib::timeout_add_local_once(left, move || {
            if let Some(body) = target.upgrade()
                && !body.has_css_class("kc-flash")
            {
                body.remove_css_class("kc-flash-out");
            }
        });
    };
    if elapsed < FLASH_HOLD {
        body.add_css_class("kc-flash");
        glib::timeout_add_local_once(FLASH_HOLD - elapsed, move || {
            let Some(body) = target.upgrade() else { return };
            // `.kc-flash-out` carries the transition back to the normal colour.
            body.add_css_class("kc-flash-out");
            body.remove_css_class("kc-flash");
            clear_after(&body, FLASH_FADE);
        });
    } else {
        // Rebuilt during the fade: shown as already faded.
        body.add_css_class("kc-flash-out");
        clear_after(body, FLASH_HOLD + FLASH_FADE - elapsed);
    }
}

/// A small flat text button used for inline actions like Retry / Delete.
fn link_button(text: &str) -> gtk::Button {
    let button = gtk::Button::with_label(text);
    button.add_css_class("flat");
    button.add_css_class("kc-link-button");
    button.set_focus_on_click(false);
    button
}

/// Whether `message` continues the group started by `previous`.
/// The first widget under `widget` (itself included) with `class`.
fn find_class(widget: &gtk::Widget, class: &str) -> Option<gtk::Widget> {
    if widget.has_css_class(class) {
        return Some(widget.clone());
    }
    let mut child = widget.first_child();
    while let Some(next) = child {
        if let Some(found) = find_class(&next, class) {
            return Some(found);
        }
        child = next.next_sibling();
    }
    None
}

/// The label text of a menu item (for the demo).
fn menu_item_text(button: &gtk::Button) -> Option<String> {
    let mut stack = vec![button.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = stack.pop() {
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            return Some(label.text().to_string());
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            child = next.next_sibling();
            stack.push(next);
        }
    }
    None
}

/// What the logger's right-click menu offers for one message.
struct LoggedMenu {
    channel_id: Snowflake,
    message_id: Snowflake,
    /// A deleted message's text, to copy (empty: no Copy Text).
    content: String,
    deleted: bool,
    edits: bool,
}

/// Whether the part of `body` at `(x, y)` has a right-click menu of its own
/// (a picture, a video, an avatar), which a menu on the whole message
/// should leave to it.
fn has_own_menu(body: &gtk::Widget, x: f64, y: f64) -> bool {
    let mut widget = body.pick(x, y, gtk::PickFlags::DEFAULT);
    while let Some(current) = widget {
        if current == *body {
            break;
        }
        let controllers = current.observe_controllers();
        let secondary = (0..controllers.n_items()).any(|index| {
            controllers
                .item(index)
                .and_downcast::<gtk::GestureClick>()
                .is_some_and(|click| click.button() == gtk::gdk::BUTTON_SECONDARY)
        });
        if secondary {
            return true;
        }
        widget = current.parent();
    }
    false
}

pub(crate) fn continues(previous: &Message, message: &Message) -> bool {
    previous.author.id == message.author.id
        && previous.webhook_id == message.webhook_id
        && !previous.is_system()
        && !message.is_system()
        && message.kind == Message::DEFAULT
        && message.message_reference.is_none()
        && message
            .id
            .timestamp()
            .saturating_sub(previous.id.timestamp())
            < GROUP_WINDOW_MS
        && day_key(&previous.timestamp) == day_key(&message.timestamp)
}

impl AppState {
    /// Wires the scroll behaviour of the message list. Called once.
    pub(crate) fn connect_chat(self: &Rc<Self>) {
        let adjustment = self.messages_scroll.vadjustment();

        // Content that grows or arrives (a new message, an embed filling in)
        // moves what is on screen without a scroll: re-check which looping
        // GIFs are visible then too.
        let app = self.clone();
        adjustment.connect_changed(move |adjustment| {
            app.queue_players_update();
            // Too few messages to scroll: a scroll will never reach the top,
            // so fetch older ones now (once per page; stops at the top).
            if adjustment.upper() <= adjustment.page_size() + 1.0 && app.chat.borrow().channel.is_some() {
                app.queue_load_older();
            }
        });
        let app = self.clone();
        adjustment.connect_value_changed(move |_| {
            app.queue_players_update();
            // The hovered row moves away from the toolbar; it comes back
            // with the next pointer motion.
            app.hide_toolbar();
        });
        let app = self.clone();
        adjustment.connect_value_changed(move |adjustment| {
            let distance = adjustment.upper() - adjustment.page_size() - adjustment.value();
            let at_bottom = distance < 48.0;
            let was = app.chat.borrow().at_bottom;
            app.chat.borrow_mut().at_bottom = at_bottom;
            if at_bottom && !was {
                let channel = app.chat.borrow().channel;
                if let Some(channel) = channel {
                    app.ack_channel(channel);
                }
            }
            // Refresh the jump-to-latest button every scroll, so it hides the
            // moment the bottom is reached (not only when `at_bottom` toggles,
            // which `scroll_to_bottom` pre-sets).
            app.update_present();
            if adjustment.value() < 400.0 {
                app.queue_load_older();
            }
            if distance < 400.0 {
                app.load_newer();
            }
        });

        // A round "jump to the latest" button, shown whenever the view is
        // scrolled up. It scrolls to the bottom, or reloads the newest page
        // when the window has slid away from the present.
        let present = gtk::Button::from_icon_name("go-down-symbolic");
        present.set_tooltip_text(Some("Jump to the latest messages"));
        present.add_css_class("circular");
        present.add_css_class("suggested-action");
        present.set_halign(gtk::Align::End);
        present.set_valign(gtk::Align::End);
        present.set_margin_bottom(12);
        present.set_margin_end(16);
        present.set_visible(false);
        let app = self.clone();
        present.connect_clicked(move |_| {
            let detached = app
                .chat
                .borrow()
                .channel
                .is_some_and(|channel| app.detached(channel));
            if detached {
                app.jump_to_present();
            } else {
                app.scroll_to_bottom();
            }
        });
        self.message_overlay.add_overlay(&present);
        self.chat.borrow_mut().present = Some(present);

        self.connect_toolbar();
        self.connect_cross_select();

        // The content height changes when rows are added and when images
        // finish loading; keep the reading position through both.
        let app = self.clone();
        adjustment.connect_changed(move |_| app.queue_scroll_settle());

        // Scrolling by hand (wheel, touchpad, dragging the scrollbar, the
        // keyboard) lets go of a message jumped to.
        let release = {
            let app = Rc::downgrade(self);
            move || {
                if let Some(app) = app.upgrade() {
                    app.chat.borrow_mut().pin = None;
                }
            }
        };
        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
        let let_go = release.clone();
        wheel.connect_scroll(move |_, _, _| {
            let_go();
            glib::Propagation::Proceed
        });
        self.messages_scroll.add_controller(wheel);
        let press = gtk::GestureClick::new();
        press.set_propagation_phase(gtk::PropagationPhase::Capture);
        let let_go = release.clone();
        press.connect_pressed(move |_, _, _, _| let_go());
        self.messages_scroll.vscrollbar().add_controller(press);
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, _, _, _| {
            release();
            glib::Propagation::Proceed
        });
        self.messages_scroll.add_controller(keys);
    }

    /// Puts the list back where it belongs after its content changed
    /// height: at the bottom while following, or at the same message after
    /// older rows went in above.
    ///
    /// Never done inside the adjustment's `changed` signal: the viewport
    /// emits it at the end of its size allocation, after placing the list,
    /// and the reallocation a new value asks for is dropped there. The
    /// adjustment then claimed the bottom while the list was still drawn
    /// at the old offset, so a message just sent sat below the window and
    /// scrolling down could not reach it (it was "at the bottom" already)
    /// until scrolling up moved the list again.
    fn queue_scroll_settle(self: &Rc<Self>) {
        if std::mem::replace(&mut self.chat.borrow_mut().settle_queued, true) {
            return;
        }
        let app = self.clone();
        glib::idle_add_local_once(move || {
            app.chat.borrow_mut().settle_queued = false;
            // A message jumped to stays in the middle while heights settle
            // (its row found again by id: it may have been rebuilt).
            let pinned = {
                let chat = app.chat.borrow();
                chat.pin
                    .filter(|(_, until)| std::time::Instant::now() < *until)
                    .and_then(|(id, _)| chat.rows.iter().find(|entry| entry.id == id).map(|entry| entry.row.clone()))
            };
            if let Some(row) = pinned {
                app.chat.borrow_mut().restore = None;
                app.scroll_row_to_centre(&row);
                return;
            }
            app.chat.borrow_mut().pin = None;
            let adjustment = app.messages_scroll.vadjustment();
            let restore = app.chat.borrow_mut().restore.take();
            if let Some((upper, value)) = restore {
                adjustment.set_value(value + (adjustment.upper() - upper));
            } else if app.chat.borrow().at_bottom {
                adjustment.set_value(adjustment.upper() - adjustment.page_size());
            }
        });
    }

    fn scroll_to_bottom(self: &Rc<Self>) {
        self.chat.borrow_mut().at_bottom = true;
        let adjustment = self.messages_scroll.vadjustment();
        adjustment.set_value(adjustment.upper() - adjustment.page_size());
        // Layout has not run for rows added this cycle; go again after it.
        self.queue_scroll_settle();
        // Hide the jump button now (setting the same value fires no scroll
        // event, so it would otherwise linger).
        self.update_present();
    }

    /// Shows the selected channel, or the right placeholder. Rebuilds the
    /// list only when the channel changed.
    pub(crate) fn render_messages(self: &Rc<Self>) {
        let Some((dm_mode, selected)) =
            self.with_session(|session| (session.dm_mode, session.selected_channel))
        else {
            return;
        };

        let Some(channel_id) = selected else {
            if let Some((account, ..)) = self.active() {
                crate::logger::set_viewed(account, None);
            }
            self.chat.borrow_mut().channel = None;
            if dm_mode {
                self.content_stack.set_visible_child_name("friends");
                self.render_friends();
            } else {
                self.content_stack.set_visible_child_name("chat");
                self.show_placeholder(
                    crate::widgets::first_icon(&["chat-bubbles-empty-symbolic", "user-available-symbolic"]),
                    "No Channel Selected",
                    "Pick a channel from the sidebar.",
                );
            }
            return;
        };
        self.content_stack.set_visible_child_name("chat");

        // Voice channels show the call alone; a DM with a call up shows it
        // above its chat.
        match self.call_layout(channel_id) {
            crate::voice::CallLayout::Only => {
                self.chat.borrow_mut().channel = None;
                self.render_voice_view(channel_id);
                return;
            }
            crate::voice::CallLayout::WithChat | crate::voice::CallLayout::Beside => {
                self.render_voice_view(channel_id)
            }
            crate::voice::CallLayout::Hidden => {
                self.set_call_layout(crate::voice::CallLayout::Hidden);
            }
        }

        if self.chat.borrow().channel == Some(channel_id) {
            return;
        }
        self.rebuild_messages(channel_id);
    }

    /// Redraws the open channel's messages, so their times follow a
    /// changed clock setting.
    pub(crate) fn redraw_times(self: &Rc<Self>) {
        if self.chat.borrow().channel.is_some() {
            self.chat.borrow_mut().channel = None;
            self.render_messages();
        }
    }

    fn show_placeholder(&self, icon: &str, title: &str, description: &str) {
        clear_box(&self.chat_placeholder);
        self.chat_placeholder
            .append(&empty_state(icon, title, description));
        self.message_stack.set_visible_child_name("placeholder");
    }

    fn show_message_list(&self) {
        self.message_stack.set_visible_child_name("messages");
    }

    fn rebuild_messages(self: &Rc<Self>, channel_id: Snowflake) {
        self.share_channel_changed(channel_id);
        if let Some((account, ..)) = self.active() {
            crate::logger::set_viewed(account, Some(channel_id));
            self.merge_logged(account, channel_id);
        }
        self.hide_toolbar();
        clear_list(&self.messages_list);
        {
            let mut chat = self.chat.borrow_mut();
            chat.channel = Some(channel_id);
            chat.rows.clear();
            chat.restore = None;
            chat.at_bottom = true;
        }
        let Some((_, _, store)) = self.active() else {
            return;
        };
        let loading = self
            .with_session(|session| {
                session
                    .history
                    .get(&channel_id)
                    .is_some_and(|history| history.loading)
            })
            .unwrap_or(false);

        let entries = store.read(|state| {
            let messages = state
                .messages
                .get(&channel_id)
                .map(|buffer| buffer.messages())
                .unwrap_or(&[]);
            let start = messages.len().saturating_sub(INITIAL_ROWS);
            let mut entries = Vec::with_capacity(messages.len() - start);
            for index in start..messages.len() {
                let previous = (index > 0).then(|| &messages[index - 1]);
                entries.push(self.build_entry(&messages[index], previous, state));
            }
            entries
        });

        if entries.is_empty() && !loading {
            self.show_placeholder(
                crate::widgets::first_icon(&["chat-bubbles-empty-symbolic", "user-available-symbolic"]),
                "No Messages Yet",
                "Say something to start the conversation.",
            );
        } else {
            self.show_message_list();
        }
        self.history_spinner.set_visible(loading);
        self.history_spinner.set_spinning(loading);
        for entry in &entries {
            self.messages_list.append(&entry.row);
        }
        self.chat.borrow_mut().rows = entries;
        self.scroll_to_bottom();
        self.update_present();
    }

    /// Whether the open channel's window stops short of the present.
    pub(crate) fn detached(&self, channel_id: Snowflake) -> bool {
        self.active().is_some_and(|(_, _, store)| {
            store.read(|state| {
                state
                    .messages
                    .get(&channel_id)
                    .is_some_and(|buffer| buffer.is_detached())
            })
        })
    }

    fn update_present(&self) {
        let (channel, at_bottom) = {
            let chat = self.chat.borrow();
            (chat.channel, chat.at_bottom)
        };
        // Shown whenever scrolled up at all, or when the window has slid away
        // from the present (so the latest is one click away either way).
        let show = channel.is_some_and(|channel| !at_bottom || self.detached(channel));
        if let Some(button) = &self.chat.borrow().present {
            button.set_visible(show);
        }
    }

    /// Fetches the page after the newest message shown, for a window loaded
    /// around an older message.
    fn load_newer(self: &Rc<Self>) {
        let Some(channel_id) = self.chat.borrow().channel else {
            return;
        };
        if self.chat.borrow().newer_loading || !self.detached(channel_id) {
            return;
        }
        let Some(newest_shown) = self.chat.borrow().rows.last().map(|entry| entry.id) else {
            return;
        };
        let Some((_, http, store)) = self.active() else {
            return;
        };
        self.chat.borrow_mut().newer_loading = true;
        let page = scroll_page(self.window.height());
        self.spawn(
            async move { http.messages_after(channel_id, page, newest_shown).await },
            move |app, result| {
                app.chat.borrow_mut().newer_loading = false;
                match result {
                    Ok(messages) => {
                        let full_page = messages.len() >= page as usize;
                        let mut ids: Vec<_> = messages.iter().map(|message| message.id).collect();
                        ids.sort();
                        store.write(|state| state.load_newer(channel_id, messages, full_page));
                        // Rows go in below the reading position, which stays
                        // put; scrolling on fetches the next page.
                        app.chat.borrow_mut().at_bottom = false;
                        for id in ids {
                            app.message_added(channel_id, id);
                        }
                        app.update_present();
                    }
                    Err(error) => {
                        app.set_status(&format!("Could not load newer messages: {error}"))
                    }
                }
            },
        );
    }

    /// A message is being sent to `channel_id`: if the view is on older
    /// messages, go to the present, where it will appear.
    pub(crate) fn follow_sent_message(self: &Rc<Self>, channel_id: Snowflake) {
        if self.chat.borrow().channel == Some(channel_id) && self.detached(channel_id) {
            self.jump_to_present();
        }
    }

    /// Leaves a window of older messages for the channel's newest ones.
    pub(crate) fn jump_to_present(self: &Rc<Self>) {
        let Some(channel_id) = self.chat.borrow().channel else {
            return;
        };
        let Some((_, http, store)) = self.active() else {
            return;
        };
        if let Some(button) = &self.chat.borrow().present {
            button.set_sensitive(false);
        }
        // Drop the old window now, so messages arriving meanwhile (one just
        // sent, say) are kept and merge with the page fetched below. The rows
        // on screen stay until that page is in.
        store.write(|state| state.messages.remove(&channel_id));
        self.chat.borrow_mut().channel = None;
        self.with_session_mut(|session| {
            session.history.insert(channel_id, Default::default());
        });
        let first_page = initial_page(self.window.height());
        self.spawn(
            async move { http.messages(channel_id, first_page, None).await },
            move |app, result| {
                if let Some(button) = &app.chat.borrow().present {
                    button.set_sensitive(true);
                }
                match result {
                    Ok(messages) => {
                        store.write(|state| state.load_history(channel_id, messages));
                        app.render_messages();
                    }
                    Err(error) => {
                        app.set_status(&format!("Could not load the newest messages: {error}"));
                        app.render_messages();
                    }
                }
            },
        );
    }

    fn build_entry(
        self: &Rc<Self>,
        message: &Message,
        previous: Option<&Message>,
        state: &State,
    ) -> RowEntry {
        let headed = !previous.is_some_and(|previous| continues(previous, message));
        let day = day_key(&message.timestamp);
        let divider = previous.is_some_and(|previous| day_key(&previous.timestamp) != day);
        let (row, body) = self.message_row(message, headed, divider, state);
        self.mark_share_row(&body, message.id);
        // Rebuilt while highlighted after a jump: keep the highlight.
        if let Some((id, started)) = self.chat.borrow().flash
            && id == message.id
        {
            show_flash(&body, started);
        }
        RowEntry {
            id: message.id,
            row,
            body,
        }
    }

    /// A message arrived in the open channel.
    pub(crate) fn message_added(self: &Rc<Self>, channel_id: Snowflake, message_id: Snowflake) {
        if self.chat.borrow().channel != Some(channel_id) {
            return;
        }
        let Some((_, _, store)) = self.active() else {
            return;
        };
        let position = self
            .chat
            .borrow()
            .rows
            .partition_point(|entry| entry.id < message_id);
        if self
            .chat
            .borrow()
            .rows
            .get(position)
            .is_some_and(|entry| entry.id == message_id)
        {
            self.message_updated(channel_id, message_id);
            return;
        }
        // A message older than everything shown belongs to scroll-back.
        if position == 0 && !self.chat.borrow().rows.is_empty() {
            return;
        }

        let entry = store.read(|state| {
            let buffer = state.messages.get(&channel_id)?;
            let messages = buffer.messages();
            let index = messages
                .binary_search_by_key(&message_id, |message| message.id)
                .ok()?;
            let previous = (index > 0).then(|| &messages[index - 1]);
            Some(self.build_entry(&messages[index], previous, state))
        });
        let Some(entry) = entry else {
            return;
        };
        self.show_message_list();
        self.messages_list.insert(&entry.row, position as i32);
        self.chat.borrow_mut().rows.insert(position, entry);
        self.refresh_grouping(channel_id, position + 1);

        let at_bottom = self.chat.borrow().at_bottom;
        if at_bottom {
            self.scroll_to_bottom();
            if self.window.is_active() {
                self.ack_channel(channel_id);
            }
        }
    }

    /// Rebuilds the row of an edited message (content, embeds, reactions).
    pub(crate) fn message_updated(self: &Rc<Self>, channel_id: Snowflake, message_id: Snowflake) {
        if self.chat.borrow().channel != Some(channel_id) {
            return;
        }
        let Some(position) = self
            .chat
            .borrow()
            .rows
            .iter()
            .position(|entry| entry.id == message_id)
        else {
            return;
        };
        self.rebuild_row(channel_id, position);
    }

    pub(crate) fn message_deleted(self: &Rc<Self>, channel_id: Snowflake, message_id: Snowflake) {
        if self.chat.borrow().channel != Some(channel_id) {
            return;
        }
        let removed = {
            let mut chat = self.chat.borrow_mut();
            chat.rows
                .iter()
                .position(|entry| entry.id == message_id)
                .map(|position| (position, chat.rows.remove(position)))
        };
        if let Some((position, entry)) = removed {
            self.messages_list.remove(&entry.row);
            // The next row may now start a group or lose its day divider.
            self.refresh_grouping(channel_id, position);
        }
    }

    /// Rebuilds the row at `position` if its header or divider state no
    /// longer matches its neighbours.
    fn refresh_grouping(self: &Rc<Self>, channel_id: Snowflake, position: usize) {
        if position < self.chat.borrow().rows.len() {
            self.rebuild_row(channel_id, position);
        }
    }

    fn rebuild_row(self: &Rc<Self>, channel_id: Snowflake, position: usize) {
        let Some((_, _, store)) = self.active() else {
            return;
        };
        let Some(message_id) = self.chat.borrow().rows.get(position).map(|entry| entry.id) else {
            return;
        };
        let entry = store.read(|state| {
            let messages = state.messages.get(&channel_id)?.messages();
            let index = messages
                .binary_search_by_key(&message_id, |message| message.id)
                .ok()?;
            let previous = (index > 0).then(|| &messages[index - 1]);
            Some(self.build_entry(&messages[index], previous, state))
        });
        let Some(entry) = entry else {
            return;
        };
        let old = std::mem::replace(&mut self.chat.borrow_mut().rows[position], entry);
        self.messages_list.remove(&old.row);
        let row = self.chat.borrow().rows[position].row.clone();
        self.messages_list.insert(&row, position as i32);
    }

    /// History for the open channel arrived: build what is not shown yet.
    pub(crate) fn history_loaded(self: &Rc<Self>, channel_id: Snowflake) {
        self.with_session_mut(|session| {
            session.history.entry(channel_id).or_default().loading = false;
        });
        if self.chat.borrow().channel != Some(channel_id) {
            return;
        }
        self.history_spinner.set_visible(false);
        self.history_spinner.set_spinning(false);

        let oldest_shown = self.chat.borrow().rows.first().map(|entry| entry.id);
        let Some(oldest_shown) = oldest_shown else {
            // First page for this channel.
            self.chat.borrow_mut().channel = None;
            self.render_messages();
            self.take_pending_jump(channel_id);
            return;
        };
        let Some((_, _, store)) = self.active() else {
            return;
        };
        let older = store.read(|state| {
            let Some(buffer) = state.messages.get(&channel_id) else {
                return Vec::new();
            };
            let messages = buffer.messages();
            let end = messages.partition_point(|message| message.id < oldest_shown);
            let start = end.saturating_sub(PAGE as usize);
            (start..end)
                .map(|index| {
                    let previous = (index > 0).then(|| &messages[index - 1]);
                    self.build_entry(&messages[index], previous, state)
                })
                .collect::<Vec<_>>()
        });
        if older.is_empty() {
            return;
        }
        // Scrolling far back may have unloaded the distant (newest) messages
        // from the store; drop the matching rows first (they are below the
        // viewport, so this frees memory without moving the view) and before
        // anchoring, so the scroll-restore below only accounts for the rows
        // added at the top.
        self.trim_to_store(channel_id);
        let adjustment = self.messages_scroll.vadjustment();
        self.chat.borrow_mut().restore = Some((adjustment.upper(), adjustment.value()));
        for (offset, entry) in older.iter().enumerate() {
            self.messages_list.insert(&entry.row, offset as i32);
        }
        let count = older.len();
        self.chat.borrow_mut().rows.splice(0..0, older);
        // The previously first row now has a predecessor.
        self.refresh_grouping(channel_id, count);
        self.update_present();
    }

    /// Removes rows newer than the store keeps, after a slide unloaded the
    /// distant end. A no-op while the window still reaches the present.
    fn trim_to_store(self: &Rc<Self>, channel_id: Snowflake) {
        if self.chat.borrow().channel != Some(channel_id) {
            return;
        }
        let Some((_, _, store)) = self.active() else {
            return;
        };
        let newest = store.read(|state| {
            state
                .messages
                .get(&channel_id)
                .and_then(|buffer| buffer.messages().last().map(|message| message.id))
        });
        let Some(newest) = newest else {
            return;
        };
        while self
            .chat
            .borrow()
            .rows
            .last()
            .is_some_and(|row| row.id > newest)
        {
            if let Some(entry) = self.chat.borrow_mut().rows.pop() {
                self.messages_list.remove(&entry.row);
            }
        }
    }

    /// Fetches the page of messages before the oldest one shown.
    fn load_older(self: &Rc<Self>) {
        let Some(channel_id) = self.chat.borrow().channel else {
            return;
        };
        let Some(oldest_shown) = self.chat.borrow().rows.first().map(|entry| entry.id) else {
            return;
        };
        let Some((_, http, store)) = self.active() else {
            return;
        };
        // Rows already in the store but not built yet come first.
        let cached = store.read(|state| {
            state.messages.get(&channel_id).is_some_and(|buffer| {
                buffer
                    .messages()
                    .first()
                    .is_some_and(|message| message.id < oldest_shown)
            })
        });
        if cached {
            self.history_loaded(channel_id);
            return;
        }

        let start = self
            .with_session_mut(|session| {
                let history = session.history.entry(channel_id).or_default();
                if history.loading || history.reached_top {
                    return false;
                }
                history.loading = true;
                true
            })
            .unwrap_or(false);
        if !start {
            return;
        }
        self.history_spinner.set_visible(true);
        self.history_spinner.set_spinning(true);
        let page = scroll_page(self.window.height());
        self.spawn(
            async move { http.messages(channel_id, page, Some(oldest_shown)).await },
            move |app, result| match result {
                Ok(messages) => {
                    let reached_top = messages.len() < page as usize;
                    app.with_session_mut(|session| {
                        session.history.entry(channel_id).or_default().reached_top = reached_top;
                    });
                    // Emits HistoryLoaded, which prepends the rows.
                    store.load_history(channel_id, messages);
                }
                Err(error) => {
                    app.with_session_mut(|session| {
                        session.history.entry(channel_id).or_default().loading = false;
                    });
                    app.history_spinner.set_visible(false);
                    app.set_status(&format!("Could not load older messages: {error}"));
                }
            },
        );
    }

    /// Scrolls to a message that is already shown, centring it, and flashes
    /// it. Returns false when the message is not loaded.
    pub(crate) fn jump_to_message(self: &Rc<Self>, message_id: Snowflake) -> bool {
        let found = self
            .chat
            .borrow()
            .rows
            .iter()
            .find(|entry| entry.id == message_id)
            .map(|entry| (entry.row.clone(), entry.body.clone()));
        let Some((row, body)) = found else {
            return false;
        };
        let now = std::time::Instant::now();
        let near_top = {
            let mut chat = self.chat.borrow_mut();
            chat.at_bottom = false;
            chat.restore = None;
            // Held in the middle while the rows around it settle.
            chat.pin = Some((message_id, now + PIN_TIME));
            chat.flash = Some((message_id, now));
            chat.rows.iter().position(|entry| entry.id == message_id).is_some_and(|index| index < NEAR_TOP_ROWS)
        };
        self.centre_row(&row);
        self.update_present();
        show_flash(&body, now);
        // Too few messages above it to centre it: fetch older ones now (the
        // pin keeps it centred as they go in), rather than when a scroll
        // happens to reach the top.
        if near_top {
            self.queue_load_older();
        }
        true
    }

    /// Scrolls so `row` sits in the middle of the viewport. The row may have
    /// just been built and not laid out yet (its height is still 0), so retry
    /// on idle until it has an allocation, giving up after a bounded number of
    /// frames so a never-mapped row cannot spin.
    fn centre_row(self: &Rc<Self>, row: &gtk::ListBoxRow) {
        let app = self.clone();
        let row = row.clone();
        let mut tries = 0u8;
        glib::idle_add_local(move || {
            tries += 1;
            if row.height() > 1 || tries >= 30 {
                app.scroll_row_to_centre(&row);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    /// Positions the message scroll so `row`'s centre aligns with the
    /// viewport's centre, clamped to the scrollable range.
    fn scroll_row_to_centre(&self, row: &gtk::ListBoxRow) {
        let adjustment = self.messages_scroll.vadjustment();
        // Measured within the scrolled content (the message column inside
        // the viewport), whose top is position 0 of the adjustment. Not
        // against the viewport itself: it moves with the scroll, so a row's
        // place in it is relative to where the view happened to be (and lags
        // a frame behind a new value), which sent jumps far off target.
        let content = self
            .messages_scroll
            .child()
            .and_then(|viewport| match viewport.downcast_ref::<gtk::Viewport>() {
                Some(viewport) => viewport.child(),
                None => Some(viewport),
            });
        let Some(bounds) = content.and_then(|content| row.compute_bounds(&content)) else {
            return;
        };
        let page = adjustment.page_size();
        let target = f64::from(bounds.y()) + f64::from(bounds.height()) / 2.0 - page / 2.0;
        let max = (adjustment.upper() - page).max(adjustment.lower());
        adjustment.set_value(target.clamp(adjustment.lower(), max));
    }

    // -- rows ------------------------------------------------------------

    fn message_row(
        self: &Rc<Self>,
        message: &Message,
        headed: bool,
        divider: bool,
        state: &State,
    ) -> (gtk::ListBoxRow, gtk::Box) {
        let guild = message.guild_id.or_else(|| {
            state
                .channels
                .get(&message.channel_id)
                .and_then(|channel| channel.guild_id)
        });
        let me = state.my_id();
        let mentioned = me.is_some_and(|me| {
            message.mentions.iter().any(|user| user.id == me)
                || (message.mention_everyone && message.author.id != me)
        });

        let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        if divider {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.set_margin_top(12);
            row.set_margin_start(16);
            row.set_margin_end(16);
            let left = gtk::Separator::new(gtk::Orientation::Horizontal);
            left.set_hexpand(true);
            left.set_valign(gtk::Align::Center);
            let right = gtk::Separator::new(gtk::Orientation::Horizontal);
            right.set_hexpand(true);
            right.set_valign(gtk::Align::Center);
            row.append(&left);
            row.append(&label(&long_date(&message.timestamp), &["kc-divider"]));
            row.append(&right);
            outer.append(&row);
        }

        let body = if message.is_system() {
            self.system_body(message, guild, state)
        } else {
            self.message_body(message, headed, guild, state)
        };
        body.add_css_class("kc-message");
        if headed || message.is_system() {
            body.add_css_class("kc-first");
        }
        if mentioned {
            body.add_css_class("kc-mentioned");
        }

        if !message.is_system() {
            let motion = gtk::EventControllerMotion::new();
            let app = Rc::downgrade(self);
            let message_id = message.id;
            let anchor = body.downgrade();
            motion.connect_motion(move |_, _, _| {
                if let (Some(app), Some(anchor)) = (app.upgrade(), anchor.upgrade()) {
                    app.hover_message(message_id, &anchor);
                }
            });
            let app = Rc::downgrade(self);
            motion.connect_leave(move |_| {
                if let Some(app) = app.upgrade() {
                    app.schedule_toolbar_hide();
                }
            });
            body.add_controller(motion);
        }
        outer.append(&body);

        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_selectable(false);
        row.set_child(Some(&outer));
        (row, body)
    }

    /// The icon and sentence (Pango markup) for a message Discord writes
    /// itself (joins, pins, calls...).
    pub(crate) fn system_text(
        &self,
        message: &Message,
        guild: Option<Snowflake>,
        state: &State,
    ) -> (&'static str, String) {
        let name = glib::markup_escape_text(&state.display_name(guild, &message.author));
        match message.kind {
            1 => (
                "list-add-symbolic",
                format!("<b>{name}</b> added someone to the group."),
            ),
            2 => (
                "list-remove-symbolic",
                format!("<b>{name}</b> left the group."),
            ),
            3 => (
                "call-start-symbolic",
                match message.call.as_ref().and_then(|call| {
                    let ended = call.ended_timestamp.as_deref()?;
                    crate::content::minutes_between(&message.timestamp, ended)
                }) {
                    Some(minutes) => format!(
                        "<b>{name}</b> started a call that lasted {}.",
                        crate::content::call_length(minutes)
                    ),
                    None => format!("<b>{name}</b> started a call."),
                },
            ),
            4 => (
                "document-edit-symbolic",
                format!(
                    "<b>{name}</b> changed the channel name: <b>{}</b>",
                    glib::markup_escape_text(&message.content)
                ),
            ),
            5 => (
                "image-x-generic-symbolic",
                format!("<b>{name}</b> changed the channel icon."),
            ),
            6 => (
                "view-pin-symbolic",
                format!("<b>{name}</b> pinned a message to this channel."),
            ),
            7 => (
                "go-next-symbolic",
                format!("<b>{name}</b> joined the server."),
            ),
            8..=11 => (
                "starred-symbolic",
                format!("<b>{name}</b> boosted the server!"),
            ),
            12 => (
                "list-add-symbolic",
                format!(
                    "<b>{name}</b> added <b>{}</b> to this channel.",
                    glib::markup_escape_text(&message.content)
                ),
            ),
            18 => (
                "mail-reply-sender-symbolic",
                format!(
                    "<b>{name}</b> started a thread: <b>{}</b>",
                    glib::markup_escape_text(&message.content)
                ),
            ),
            46 => ("view-list-symbolic", "A poll has ended.".to_owned()),
            _ => (
                "dialog-information-symbolic",
                if message.content.is_empty() {
                    format!("<b>{name}</b> did something this client cannot show yet.")
                } else {
                    glib::markup_escape_text(&message.content).to_string()
                },
            ),
        }
    }

    /// Joins, pins, calls and the other messages Discord writes itself.
    fn system_body(
        self: &Rc<Self>,
        message: &Message,
        guild: Option<Snowflake>,
        state: &State,
    ) -> gtk::Box {
        let (icon, text) = self.system_text(message, guild, state);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        row.add_css_class("kc-system");
        let image = gtk::Image::from_icon_name(icon);
        image.set_pixel_size(16);
        image.set_width_request(40);
        image.set_valign(gtk::Align::Start);
        image.set_margin_top(3);
        row.append(&image);
        // The time follows the sentence in the same label so the two wrap
        // together.
        let text_label = gtk::Label::new(None);
        text_label.set_markup(&format!(
            "{text}  <span size=\"small\" alpha=\"60%\">{}</span>",
            glib::markup_escape_text(&format_time(&message.timestamp))
        ));
        text_label.set_xalign(0.0);
        text_label.set_wrap(true);
        text_label.set_hexpand(true);
        row.append(&text_label);
        row
    }

    pub(crate) fn message_body(
        self: &Rc<Self>,
        message: &Message,
        headed: bool,
        guild: Option<Snowflake>,
        state: &State,
    ) -> gtk::Box {
        let body = gtk::Box::new(gtk::Orientation::Vertical, 2);

        // An app's answer to a command says who used which.
        if let Some((command, user)) = message.command_used() {
            body.append(&self.command_preview(&command, user, guild, state));
        }
        if let Some(reference) = &message.message_reference
            && message.message_snapshots.is_empty()
        {
            body.append(&self.reply_preview(message, reference.message_id, guild, state));
        }

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
        column.set_hexpand(true);

        if headed {
            let name = state.display_name(guild, &message.author);
            let avatar_url = guild
                .and_then(|guild| {
                    let hash = state.member(guild, message.author.id)?.avatar.clone()?;
                    Some(cdn::member_avatar(guild, message.author.id, &hash, 80))
                })
                .unwrap_or_else(|| {
                    cdn::user_avatar(message.author.id, message.author.avatar.as_deref(), 80)
                });
            let avatar = self.images.avatar(40, &name, Some(&avatar_url));
            avatar.set_valign(gtk::Align::Start);
            avatar.set_margin_top(2);
            let click = gtk::GestureClick::new();
            let app = self.clone();
            let user_id = message.author.id;
            click.connect_released(move |gesture, _, _, _| {
                if let Some(widget) = gesture.widget() {
                    app.show_profile(user_id, &widget);
                }
            });
            avatar.add_controller(click);
            avatar.set_cursor_from_name(Some("pointer"));
            self.attach_user_menu(&avatar, user_id);
            row.append(&avatar);

            let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let author = gtk::Label::new(None);
            author.add_css_class("kc-author");
            let escaped = glib::markup_escape_text(&name);
            match guild.and_then(|guild| state.member_color(guild, message.author.id)) {
                Some(color) => author.set_markup(&format!(
                    "<span foreground=\"{}\">{escaped}</span>",
                    hex_color(color)
                )),
                None => author.set_markup(&escaped),
            }
            header.append(&author);
            if message.author.bot == Some(true) || message.webhook_id.is_some() {
                let tag = gtk::Label::new(Some(
                    if message.webhook_id.is_some() && message.author.bot != Some(true) {
                        "WEBHOOK"
                    } else {
                        "APP"
                    },
                ));
                tag.add_css_class("kc-tag");
                tag.set_valign(gtk::Align::Center);
                header.append(&tag);
            }
            header.append(&label(
                &format_time(&message.timestamp),
                &["caption", "dim-label", "kc-time"],
            ));
            column.append(&header);
        } else {
            // Grouped rows keep the avatar gutter; the time shows on hover.
            let time = gtk::Label::new(Some(&gutter_time(&message.timestamp)));
            time.set_tooltip_text(Some(&format_time(&message.timestamp)));
            time.add_css_class("kc-gutter-time");
            time.set_width_request(40);
            time.set_valign(gtk::Align::Start);
            time.set_margin_top(3);
            row.append(&time);
        }

        // The app is still working on its answer.
        if message.flags & Message::FLAG_LOADING != 0 && message.content.is_empty() {
            let thinking = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let spinner = gtk::Spinner::new();
            spinner.start();
            thinking.append(&spinner);
            thinking.append(&label(
                &format!("{} is thinking…", state.display_name(guild, &message.author)),
                &["dim-label"],
            ));
            column.append(&thinking);
        }
        // Earlier versions (kept by the message logger), oldest first,
        // dimmed above the current text.
        if !message.edits.is_empty() {
            let history = gtk::Box::new(gtk::Orientation::Vertical, 2);
            history.add_css_class("kc-edit-history");
            for edit in &message.edits {
                if let Some(old) = self.text_widget(&edit.content, guild, state) {
                    old.add_css_class("kc-old-version");
                    old.set_tooltip_text(Some(&format!("Before an edit (written {})", format_time(&edit.at))));
                    history.append(&old);
                }
            }
            // Right-click the earlier versions to drop them from the log (a
            // deleted message's menu, on all of it, offers it too).
            if !message.deleted {
                self.attach_logged_menu(&history, message, false);
            }
            column.append(&history);
        }
        if let Some(text) = self.text_widget(&message.content, guild, state) {
            // A message that is just a link to its own media embed shows only
            // the media, as the official client does.
            // Discord's embed carries the link without its signature query, so
            // compare the two with the query left off.
            let bare_video = |embed: &kc_state::message::Embed| {
                embed.kind.as_deref() == Some("video")
                    && embed.title.is_none()
                    && embed.thumbnail.is_none()
                    && embed.image.is_none()
                    && embed.video.is_some()
            };
            let lone_media_link = (message.embeds.len() == 1
                && (matches!(message.embeds[0].kind.as_deref(), Some("image" | "gifv"))
                    || bare_video(&message.embeds[0]))
                && message.embeds[0].url.as_deref().is_some_and(|url| {
                    crate::content::same_link(url, message.content.trim())
                }))
                // A bare Discord media link Discord did not unfurl: the client
                // shows the media itself (below), so the link text goes too.
                || (message.embeds.is_empty()
                    && crate::content::discord_media_links(&message.content)
                        .first()
                        .is_some_and(|link| link == message.content.trim()));
            if !lone_media_link {
                column.append(&text);
            }
        }
        if message.edited_timestamp.is_some() {
            let edited = label("(edited)", &["caption", "dim-label"]);
            if let Some(time) = message.edited_timestamp.as_deref() {
                edited.set_tooltip_text(Some(&format_time(time)));
            }
            column.append(&edited);
        }

        for snapshot in &message.message_snapshots {
            let forwarded = gtk::Box::new(gtk::Orientation::Vertical, 2);
            forwarded.add_css_class("kc-embed");
            forwarded.append(&label("Forwarded", &["caption", "dim-label"]));
            if let Some(text) = self.text_widget(&snapshot.message.content, guild, state) {
                forwarded.append(&text);
            }
            for attachment in &snapshot.message.attachments {
                forwarded.append(&self.attachment_widget(attachment));
            }
            for embed in &snapshot.message.embeds {
                if let Some(widget) = self.embed_widget(embed, guild, state) {
                    forwarded.append(&widget);
                }
            }
            column.append(&forwarded);
        }

        for attachment in &message.attachments {
            let widget = self.attachment_widget(attachment);
            widget.set_margin_top(2);
            column.append(&widget);
        }
        if message.flags & Message::FLAG_SUPPRESS_EMBEDS == 0 {
            for embed in &message.embeds {
                if let Some(widget) = self.embed_widget(embed, guild, state) {
                    widget.set_margin_top(2);
                    column.append(&widget);
                }
            }
            // Invite links resolve into their own card (Discord does not embed
            // them server-side for user clients).
            for card in self.invite_cards(&message.content) {
                card.set_margin_top(2);
                column.append(&card);
            }
            // Links to Discord-hosted media that no embed already shows.
            for link in crate::content::discord_media_links(&message.content) {
                let same = |url: &Option<String>| {
                    url.as_deref()
                        .is_some_and(|url| crate::content::same_link(url, &link))
                };
                let covered = message.embeds.iter().any(|embed| {
                    same(&embed.url)
                        || [&embed.thumbnail, &embed.image, &embed.video]
                            .into_iter()
                            .flatten()
                            .any(|media| same(&media.url) || same(&media.proxy_url))
                });
                if !covered && let Some(widget) = self.link_media_widget(&link) {
                    widget.set_margin_top(2);
                    column.append(&widget);
                }
            }
        }
        if let Some(components) = self.components_widget(message, guild, state) {
            column.append(&components);
        }
        for sticker in &message.sticker_items {
            column.append(&self.sticker_widget(sticker));
        }
        if !message.reactions.is_empty() {
            column.append(&self.reactions_row(message));
        }
        // Deleted on Discord, kept by the message logger: says so, and a
        // right-click can take it out of the log.
        if message.deleted {
            body.add_css_class("kc-deleted");
            let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let bin = gtk::Image::from_icon_name("user-trash-symbolic");
            bin.set_pixel_size(12);
            footer.append(&bin);
            let note = gtk::Label::new(Some("Deleted"));
            note.add_css_class("caption");
            note.add_css_class("kc-deleted-note");
            footer.append(&note);
            column.append(&footer);
            self.attach_logged_menu(&body, message, true);
        }
        // Only this user sees it (Discord keeps no copy): it can be put away.
        if message.flags & Message::FLAG_EPHEMERAL != 0 {
            let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let eye = gtk::Image::from_icon_name("view-reveal-symbolic");
            eye.set_pixel_size(12);
            footer.append(&eye);
            let only_you = gtk::Label::new(Some("Only you can see this ·"));
            only_you.add_css_class("caption");
            only_you.add_css_class("dim-label");
            footer.append(&only_you);
            let dismiss = link_button("Dismiss message");
            dismiss.add_css_class("caption");
            let app = self.clone();
            let (channel_id, message_id) = (message.channel_id, message.id);
            dismiss.connect_clicked(move |_| {
                app.with_session(|session| {
                    session.store.apply(
                        "MESSAGE_DELETE",
                        &serde_json::json!({"id": message_id, "channel_id": channel_id}),
                    );
                });
            });
            footer.append(&dismiss);
            column.append(&footer);
        }

        // A message shown before Discord confirms it: greyed while sending,
        // or offering Retry / Delete if it failed.
        match message.send_state {
            SendState::Sending => {
                body.add_css_class("kc-sending");
                let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                let spinner = gtk::Spinner::new();
                spinner.set_valign(gtk::Align::Center);
                spinner.start();
                footer.append(&spinner);
                footer.append(&label("Sending…", &["caption", "dim-label"]));
                column.append(&footer);
            }
            SendState::Failed => {
                body.add_css_class("kc-sending");
                let nonce = message.nonce.as_ref().and_then(|value| value.as_str());
                let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                footer.append(&label("Couldn't send.", &["caption", "kc-send-failed"]));
                if let Some(nonce) = nonce {
                    let channel = message.channel_id;
                    let retry = link_button("Retry");
                    let app = self.clone();
                    let key = nonce.to_owned();
                    retry.connect_clicked(move |_| app.retry_send(channel, &key));
                    footer.append(&retry);
                    let delete = link_button("Delete");
                    let app = self.clone();
                    let key = nonce.to_owned();
                    delete.connect_clicked(move |_| app.discard_send(channel, &key));
                    footer.append(&delete);
                }
                column.append(&footer);
            }
            SendState::Sent => {}
        }

        row.append(&column);
        body.append(&row);
        body
    }

    /// A right-click menu for what the message logger kept of `message`
    /// on `widget`: the whole of a deleted message (`deleted`), or an
    /// edited one's earlier versions.
    fn attach_logged_menu(self: &Rc<Self>, widget: &impl IsA<gtk::Widget>, message: &Message, deleted: bool) {
        let menu = gtk::GestureClick::new();
        menu.set_button(gtk::gdk::BUTTON_SECONDARY);
        let app = self.clone();
        let logged = LoggedMenu {
            channel_id: message.channel_id,
            message_id: message.id,
            content: if deleted { message.content.clone() } else { String::new() },
            deleted,
            edits: !message.edits.is_empty(),
        };
        let anchor = widget.clone().upcast::<gtk::Widget>().downgrade();
        // Ahead of the text, which would show its own Cut/Copy/Paste
        // menu; media and avatars keep their own menus.
        menu.set_propagation_phase(gtk::PropagationPhase::Capture);
        menu.connect_pressed(move |gesture, _, x, y| {
            let Some(anchor) = anchor.upgrade() else {
                return;
            };
            if has_own_menu(&anchor, x, y) {
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            app.show_logged_menu(&logged, &anchor, x, y);
        });
        widget.add_controller(menu);
    }

    /// The menu of what the logger kept of a message: copy a deleted
    /// message's text, drop its earlier versions, or take it out of the log
    /// altogether (it then goes, as deleted messages do).
    fn show_logged_menu(self: &Rc<Self>, logged: &LoggedMenu, anchor: &gtk::Widget, x: f64, y: f64) -> gtk::Popover {
        let LoggedMenu { channel_id, message_id, .. } = *logged;
        let popover = gtk::Popover::new();
        self.anchor_popover(&popover, anchor, x, y);
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
        menu.add_css_class("kc-menu");
        menu.set_width_request(180);
        if !logged.content.is_empty() {
            let copy = crate::menu::menu_item("Copy Text", false);
            let inner = popover.clone();
            let content = logged.content.clone();
            copy.connect_clicked(move |button| {
                button.clipboard().set_text(&content);
                inner.popdown();
            });
            menu.append(&copy);
        }
        if logged.edits {
            let drop = crate::menu::menu_item("Remove Edit History", true);
            let app = self.clone();
            let inner = popover.clone();
            drop.connect_clicked(move |_| {
                inner.popdown();
                crate::logger::forget_edits(message_id);
                app.with_session(|session| {
                    session.store.write(|state| {
                        if let Some(buffer) = state.messages.get_mut(&channel_id) {
                            buffer.update_with(message_id, |message| message.edits.clear());
                        }
                    })
                });
                app.message_updated(channel_id, message_id);
            });
            menu.append(&drop);
        }
        if logged.deleted {
            let remove = crate::menu::menu_item("Remove from Log", true);
            let app = self.clone();
            let inner = popover.clone();
            remove.connect_clicked(move |_| {
                inner.popdown();
                crate::logger::forget(message_id);
                app.with_session(|session| {
                    session.store.write(|state| {
                        if let Some(buffer) = state.messages.get_mut(&channel_id) {
                            buffer.remove(message_id);
                        }
                    })
                });
                app.message_deleted(channel_id, message_id);
            });
            menu.append(&remove);
        }
        popover.set_child(Some(&menu));
        popover.popup();
        popover
    }

    /// The demo: a deleted message right-clicked on its text and on its
    /// avatar; the text gets the message's menu, the avatar keeps its own.
    pub(crate) fn demo_deleted_menu(self: &Rc<Self>) {
        let body = self
            .chat
            .borrow()
            .rows
            .iter()
            .rev()
            .map(|entry| entry.body.clone())
            .find(|body| body.has_css_class("kc-deleted"));
        let Some(body) = body else {
            eprintln!("demo: no deleted message shown");
            return;
        };
        let centre_of = |kind: glib::Type| -> Option<(f64, f64)> {
            let mut stack = vec![body.clone().upcast::<gtk::Widget>()];
            while let Some(widget) = stack.pop() {
                if widget.type_().is_a(kind) && widget != *body.upcast_ref::<gtk::Widget>() {
                    let bounds = widget.compute_bounds(&body)?;
                    return Some((
                        f64::from(bounds.x() + bounds.width() / 2.0),
                        f64::from(bounds.y() + bounds.height() / 2.0),
                    ));
                }
                let mut child = widget.first_child();
                while let Some(next) = child {
                    child = next.next_sibling();
                    stack.push(next);
                }
            }
            None
        };
        let text = centre_of(gtk::TextView::static_type()).or_else(|| centre_of(gtk::Label::static_type()));
        let avatar = centre_of(libadwaita::Avatar::static_type());
        if let Some((x, y)) = avatar {
            eprintln!("demo: avatar has its own menu: {}", has_own_menu(body.upcast_ref(), x, y));
        }
        if let Some((x, y)) = text {
            let picked = body.pick(x, y, gtk::PickFlags::DEFAULT).map(|widget| widget.type_().name());
            let own = has_own_menu(body.upcast_ref(), x, y);
            eprintln!("demo: text ({picked:?}) has its own menu: {own}");
            if !own {
                let logged = LoggedMenu {
                    channel_id: Snowflake::new(1),
                    message_id: Snowflake::new(1),
                    content: "text".to_owned(),
                    deleted: true,
                    edits: false,
                };
                self.show_logged_menu(&logged, body.upcast_ref(), x, y);
            }
        }
        // An edited message: its earlier versions dropped from the menu.
        let edited = self.chat.borrow().rows.iter().find_map(|entry| {
            let history = find_class(entry.body.upcast_ref(), "kc-edit-history")?;
            Some((entry.id, history))
        });
        let Some((message_id, history)) = edited else {
            eprintln!("demo: no edit history shown");
            return;
        };
        let Some(channel_id) = self.chat.borrow().channel else {
            return;
        };
        let logged = LoggedMenu {
            channel_id,
            message_id,
            content: String::new(),
            deleted: false,
            edits: true,
        };
        let popover = self.show_logged_menu(&logged, &history, 4.0, 4.0);
        let button = popover
            .child()
            .and_then(|menu| menu.first_child())
            .and_downcast::<gtk::Button>();
        eprintln!("demo: history menu offers {:?}", button.as_ref().and_then(menu_item_text));
        if let Some(button) = button {
            button.emit_clicked();
        }
        let left = crate::logger::edits(message_id).len();
        let rebuilt = self
            .chat
            .borrow()
            .rows
            .iter()
            .find(|entry| entry.id == message_id)
            .is_some_and(|entry| find_class(entry.body.upcast_ref(), "kc-edit-history").is_none());
        eprintln!("demo: after Remove Edit History: {left} logged, history gone from the row: {rebuilt}");
    }

    /// A message as the chat draws it, for a shared picture.
    pub(crate) fn share_body(
        self: &Rc<Self>,
        message: &Message,
        headed: bool,
        guild: Option<Snowflake>,
        state: &State,
    ) -> gtk::Box {
        if message.is_system() {
            return self.system_body(message, guild, state);
        }
        self.message_body(message, headed, guild, state)
    }

    /// The line above an app's answer: who used which command.
    fn command_preview(
        self: &Rc<Self>,
        command: &str,
        user: Option<&kc_state::User>,
        guild: Option<Snowflake>,
        state: &State,
    ) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.add_css_class("kc-reply");
        row.set_margin_start(56);
        let text = gtk::Label::new(None);
        match user {
            Some(user) => {
                let name = state.display_name(guild, user);
                row.append(&self.images.avatar(
                    16,
                    &name,
                    Some(&cdn::user_avatar(user.id, user.avatar.as_deref(), 32)),
                ));
                text.set_markup(&format!(
                    "<b>{}</b> used <span foreground=\"#00a8fc\">/{}</span>",
                    glib::markup_escape_text(&name),
                    glib::markup_escape_text(command)
                ));
            }
            None => text.set_markup(&format!(
                "Used <span foreground=\"#00a8fc\">/{}</span>",
                glib::markup_escape_text(command)
            )),
        }
        text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        row.append(&text);
        row
    }

    /// The one-line quote above a reply.
    fn reply_preview(
        self: &Rc<Self>,
        message: &Message,
        target: Option<Snowflake>,
        guild: Option<Snowflake>,
        state: &State,
    ) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.add_css_class("kc-reply");
        row.set_margin_start(56);
        let arrow = gtk::Image::from_icon_name("mail-reply-sender-symbolic");
        arrow.set_pixel_size(12);
        row.append(&arrow);

        match message.referenced_message.as_deref() {
            Some(referenced) => {
                let name = state.display_name(guild, &referenced.author);
                row.append(&self.images.avatar(
                    16,
                    &name,
                    Some(&cdn::user_avatar(
                        referenced.author.id,
                        referenced.author.avatar.as_deref(),
                        32,
                    )),
                ));
                let author = gtk::Label::new(None);
                let escaped = glib::markup_escape_text(&name);
                match guild.and_then(|guild| state.member_color(guild, referenced.author.id)) {
                    Some(color) => author.set_markup(&format!(
                        "<span foreground=\"{}\" weight=\"bold\">{escaped}</span>",
                        hex_color(color)
                    )),
                    None => author.set_markup(&format!("<b>{escaped}</b>")),
                }
                row.append(&author);
                let pieces = rich::parse(&referenced.content);
                let resolver = crate::content::StateResolver { state, guild };
                let mut preview = snippet(&rich::to_plain(&pieces, &resolver, false), 120);
                if preview.is_empty() {
                    preview = if referenced.attachments.is_empty() {
                        "Click to see message".to_owned()
                    } else {
                        "Click to see attachment".to_owned()
                    };
                }
                let text = label(&preview, &[]);
                text.set_hexpand(true);
                row.append(&text);
            }
            None => {
                row.append(&label("Original message was deleted or is not loaded", &[]));
            }
        }

        if let Some(target) = target {
            let channel_id = message.channel_id;
            let click = gtk::GestureClick::new();
            let app = self.clone();
            click.connect_released(move |_, _, _, _| {
                app.jump_or_fetch(channel_id, target);
            });
            row.add_controller(click);
            row.set_cursor_from_name(Some("pointer"));
        }
        row
    }

    fn reactions_row(self: &Rc<Self>, message: &Message) -> adw::WrapBox {
        let wrap = adw::WrapBox::new();
        wrap.set_child_spacing(4);
        wrap.set_line_spacing(4);
        wrap.set_margin_top(4);
        for reaction in &message.reactions {
            wrap.append(&self.reaction_chip(message, reaction));
        }
        wrap
    }

    fn reaction_chip(self: &Rc<Self>, message: &Message, reaction: &Reaction) -> gtk::ToggleButton {
        let chip = gtk::ToggleButton::new();
        chip.add_css_class("kc-reaction");
        chip.add_css_class("flat");
        chip.set_active(reaction.me);
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        let name = reaction.emoji.name.clone().unwrap_or_default();
        // REST key: `name:id` for custom emoji, the character otherwise.
        let key = match reaction.emoji.id {
            Some(id) => {
                let image = gtk::Image::new();
                image.set_pixel_size(18);
                self.images.set_image(&image, &cdn::emoji(id, false, 48));
                content.append(&image);
                format!("{name}:{id}")
            }
            None => {
                match crate::discord_emoji::image(&name, 18) {
                    Some(image) => content.append(&image),
                    None => content.append(&gtk::Label::new(Some(&name))),
                }
                name.clone()
            }
        };
        content.append(&gtk::Label::new(Some(&reaction.count.to_string())));
        chip.set_child(Some(&content));
        chip.set_tooltip_text(Some(&format!(":{name}:")));

        let app = self.clone();
        let (channel_id, message_id) = (message.channel_id, message.id);
        let was_mine = reaction.me;
        chip.connect_clicked(move |_| {
            app.toggle_reaction(channel_id, message_id, key.clone(), !was_mine, "Message");
        });
        chip
    }

    pub(crate) fn toggle_reaction(
        self: &Rc<Self>,
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: String,
        add: bool,
        location: &'static str,
    ) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        self.spawn(
            async move {
                if add {
                    http.add_reaction(channel_id, message_id, &emoji, location).await
                } else {
                    http.remove_reaction(channel_id, message_id, &emoji).await
                }
            },
            move |app, result| {
                if let Err(error) = result {
                    app.set_status(&format!("Reaction failed: {error}"));
                    // Put the chip back the way the server has it.
                    app.message_updated(channel_id, message_id);
                }
            },
        );
    }

    /// Fills the floating toolbar with the actions for `message`.
    fn fill_toolbar(self: &Rc<Self>, message: &Message, state: &State) {
        let toolbar = &self.toolbar;
        clear_box(toolbar);

        let (channel_id, message_id) = (message.channel_id, message.id);
        let own = state.my_id() == Some(message.author.id);
        let granted = state.permissions_in(channel_id);
        let can_send = state.can_send(channel_id);

        if granted & permissions::ADD_REACTIONS != 0 {
            let react = icon_button("face-smile-symbolic", "Add Reaction");
            let app = self.clone();
            react.connect_clicked(move |button| {
                let chooser = gtk::EmojiChooser::new();
                chooser.set_parent(button);
                let target = app.clone();
                chooser.connect_emoji_picked(move |_, emoji| {
                    target.toggle_reaction(channel_id, message_id, emoji.to_owned(), true, "Message Reaction Picker");
                });
                let target = app.clone();
                chooser.connect_closed(move |chooser| {
                    chooser.unparent();
                    target.hover.borrow_mut().menu_open = false;
                    target.schedule_toolbar_hide();
                });
                app.hover.borrow_mut().menu_open = true;
                chooser.popup();
            });
            toolbar.append(&react);
        }

        if can_send {
            let reply = icon_button("mail-reply-sender-symbolic", "Reply");
            let app = self.clone();
            reply.connect_clicked(move |_| app.start_reply(message_id));
            toolbar.append(&reply);
        }
        if own {
            let edit = icon_button("document-edit-symbolic", "Edit");
            let app = self.clone();
            edit.connect_clicked(move |_| app.start_edit(message_id));
            toolbar.append(&edit);
        }

        let more = gtk::MenuButton::new();
        more.add_css_class("flat");
        more.set_icon_name("view-more-symbolic");
        more.set_tooltip_text(Some("More"));
        let popover = gtk::Popover::new();
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let item = |text: &str| {
            let button = gtk::Button::new();
            button.add_css_class("flat");
            button.set_child(Some(&label(text, &[])));
            button
        };

        let copy = item("Copy Text");
        let content = message.content.clone();
        let inner = popover.clone();
        copy.connect_clicked(move |button| {
            button.clipboard().set_text(&content);
            inner.popdown();
        });
        menu.append(&copy);

        let link = item("Copy Message Link");
        let guild_part = message
            .guild_id
            .or_else(|| {
                state
                    .channels
                    .get(&channel_id)
                    .and_then(|channel| channel.guild_id)
            })
            .map_or_else(|| "@me".to_owned(), |guild| guild.to_string());
        let url = format!("https://discord.com/channels/{guild_part}/{channel_id}/{message_id}");
        let inner = popover.clone();
        link.connect_clicked(move |button| {
            button.clipboard().set_text(&url);
            inner.popdown();
        });
        menu.append(&link);

        let id = item("Copy Message ID");
        let inner = popover.clone();
        id.connect_clicked(move |button| {
            button.clipboard().set_text(&message_id.to_string());
            inner.popdown();
        });
        menu.append(&id);

        // The message commands apps offer here.
        let apps = self.context_commands(kc_http::Command::MESSAGE);
        if !apps.is_empty() {
            menu.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            let heading = label("Apps", &["caption", "dim-label"]);
            heading.set_xalign(0.0);
            heading.set_margin_start(8);
            menu.append(&heading);
            for (command, app_name) in apps {
                let run = item(&command.name);
                run.set_tooltip_text((!app_name.is_empty()).then_some(app_name.as_str()));
                let app = self.clone();
                let inner = popover.clone();
                run.connect_clicked(move |_| {
                    inner.popdown();
                    app.run_context_command(&command, message_id);
                });
                menu.append(&run);
            }
            menu.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        }

        let can_pin = state
            .channels
            .get(&channel_id)
            .is_some_and(|channel| channel.is_private())
            || granted & (permissions::PIN_MESSAGES | permissions::MANAGE_MESSAGES) != 0;
        if can_pin {
            let pinned = message.pinned;
            let pin = item(if pinned {
                "Unpin Message"
            } else {
                "Pin Message"
            });
            let app = self.clone();
            let inner = popover.clone();
            pin.connect_clicked(move |_| {
                inner.popdown();
                app.set_pinned(channel_id, message_id, !pinned);
            });
            menu.append(&pin);
        }

        if own || granted & permissions::MANAGE_MESSAGES != 0 {
            let delete = item("Delete Message");
            delete.add_css_class("error");
            let app = self.clone();
            let inner = popover.clone();
            delete.connect_clicked(move |_| {
                inner.popdown();
                app.confirm_delete(channel_id, message_id);
            });
            menu.append(&delete);
        }

        popover.set_child(Some(&menu));
        let app = Rc::downgrade(self);
        popover.connect_show(move |_| {
            if let Some(app) = app.upgrade() {
                app.hover.borrow_mut().menu_open = true;
            }
        });
        let app = Rc::downgrade(self);
        popover.connect_closed(move |_| {
            if let Some(app) = app.upgrade() {
                app.hover.borrow_mut().menu_open = false;
                app.schedule_toolbar_hide();
            }
        });
        more.set_popover(Some(&popover));
        toolbar.append(&more);
    }

    /// Places the toolbar over the hovered message and keeps it alive while
    /// the pointer is on it. Called once.
    fn connect_toolbar(self: &Rc<Self>) {
        let app = Rc::downgrade(self);
        let toolbar = self.toolbar.clone();
        self.message_overlay
            .connect_get_child_position(move |overlay, child| {
                if child != toolbar.upcast_ref::<gtk::Widget>() {
                    return None;
                }
                let (_, natural) = child.preferred_size();
                let (width, height) = (natural.width(), natural.height());
                let anchor = app
                    .upgrade()
                    .and_then(|app| app.hover.borrow().anchor.as_ref()?.upgrade());
                let Some(bounds) = anchor.and_then(|anchor| anchor.compute_bounds(overlay)) else {
                    // The row went away (rebuilt or scrolled out): park it.
                    return Some(gtk::gdk::Rectangle::new(-width, -height, width, height));
                };
                // Straddle the top edge of the message, like the official client.
                let x = (bounds.x() + bounds.width()) as i32 - width - 20;
                let y = bounds.y() as i32 - height / 2;
                let max_y = (overlay.height() - height).max(0);
                Some(gtk::gdk::Rectangle::new(
                    x.max(0),
                    y.clamp(0, max_y),
                    width,
                    height,
                ))
            });

        let motion = gtk::EventControllerMotion::new();
        let app = Rc::downgrade(self);
        motion.connect_enter(move |_, _, _| {
            if let Some(app) = app.upgrade() {
                let mut hover = app.hover.borrow_mut();
                hover.in_toolbar = true;
                hover.generation += 1;
            }
        });
        let app = Rc::downgrade(self);
        motion.connect_leave(move |_| {
            if let Some(app) = app.upgrade() {
                app.hover.borrow_mut().in_toolbar = false;
                app.schedule_toolbar_hide();
            }
        });
        self.toolbar.add_controller(motion);
    }

    /// The pointer is over a message: show its toolbar.
    fn hover_message(self: &Rc<Self>, message_id: Snowflake, anchor: &gtk::Box) {
        // Choosing messages to share: no message actions; a deleted
        // message has its own (right-click) menu.
        let deleted = self
            .with_session(|session| {
                let channel = session.selected_channel?;
                session.store.read(|state| {
                    state.messages.get(&channel)?.get(message_id).map(|message| message.deleted)
                })
            })
            .flatten()
            .unwrap_or(false);
        if crate::share::active() || deleted {
            return;
        }
        {
            let mut hover = self.hover.borrow_mut();
            // Any motion cancels a pending hide.
            hover.generation += 1;
            if hover.menu_open || (hover.message == Some(message_id) && self.toolbar.is_visible()) {
                return;
            }
        }
        let Some(channel_id) = self.chat.borrow().channel else {
            return;
        };
        let Some((_, _, store)) = self.active() else {
            return;
        };
        let filled = store.read(|state| {
            let message = state.messages.get(&channel_id)?.get(message_id)?;
            self.fill_toolbar(message, state);
            Some(())
        });
        if filled.is_none() {
            return;
        }
        let previous = {
            let mut hover = self.hover.borrow_mut();
            hover.message = Some(message_id);
            hover.anchor.replace(anchor.downgrade())
        };
        if let Some(previous) = previous.and_then(|previous| previous.upgrade()) {
            previous.remove_css_class("kc-hovered");
        }
        anchor.add_css_class("kc-hovered");
        self.toolbar.set_visible(true);
        self.message_overlay.queue_allocate();
    }

    /// Hides the toolbar after a short grace period, unless the pointer
    /// reached it or another message in the meantime.
    fn schedule_toolbar_hide(self: &Rc<Self>) {
        let generation = {
            let mut hover = self.hover.borrow_mut();
            hover.generation += 1;
            hover.generation
        };
        let app = Rc::downgrade(self);
        glib::timeout_add_local_once(TOOLBAR_GRACE, move || {
            let Some(app) = app.upgrade() else {
                return;
            };
            let idle = {
                let hover = app.hover.borrow();
                hover.generation == generation && !hover.in_toolbar && !hover.menu_open
            };
            if idle {
                app.hide_toolbar();
            }
        });
    }

    pub(crate) fn hide_toolbar(&self) {
        if self.hover.borrow().menu_open {
            return;
        }
        let anchor = {
            let mut hover = self.hover.borrow_mut();
            hover.message = None;
            hover.in_toolbar = false;
            hover.anchor.take()
        };
        if let Some(anchor) = anchor.and_then(|anchor| anchor.upgrade()) {
            anchor.remove_css_class("kc-hovered");
        }
        self.toolbar.set_visible(false);
    }

    /// Defers fetching older history to an idle: the scroll position changes
    /// during layout, and showing the history spinner then would resize the
    /// list in the middle of allocating it.
    fn queue_load_older(self: &Rc<Self>) {
        if std::mem::replace(&mut self.chat.borrow_mut().older_queued, true) {
            return;
        }
        let app = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(app) = app.upgrade() {
                app.chat.borrow_mut().older_queued = false;
                app.load_older();
            }
        });
    }

    fn set_pinned(self: &Rc<Self>, channel_id: Snowflake, message_id: Snowflake, pinned: bool) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        self.spawn(
            async move {
                if pinned {
                    http.pin_message(channel_id, message_id).await
                } else {
                    http.unpin_message(channel_id, message_id).await
                }
            },
            move |app, result| match result {
                Ok(()) => app.set_status(if pinned {
                    "Message pinned"
                } else {
                    "Message unpinned"
                }),
                Err(error) => app.report_failure("Could not change the pin", &error),
            },
        );
    }

    fn confirm_delete(self: &Rc<Self>, channel_id: Snowflake, message_id: Snowflake) {
        let dialog = adw::AlertDialog::new(Some("Delete Message?"), Some("This cannot be undone."));
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let app = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "delete" {
                return;
            }
            let Some((_, http, _)) = app.active() else {
                return;
            };
            app.spawn(
                async move { http.delete_message(channel_id, message_id).await },
                |app, result| {
                    if let Err(error) = result {
                        app.report_failure("Could not delete the message", &error);
                    }
                },
            );
        });
        dialog.present(Some(&self.window));
    }

    /// Shows the channel's pinned messages in a dialog.
    pub(crate) fn show_pins(self: &Rc<Self>) {
        let Some(channel_id) = self.chat.borrow().channel else {
            return;
        };
        let Some((_, http, _)) = self.active() else {
            return;
        };
        self.spawn(
            async move { http.pins(channel_id).await },
            move |app, result| match result {
                Ok(messages) => app.show_message_dialog(
                    "Pinned Messages",
                    "No pinned messages in this channel.",
                    messages,
                ),
                Err(error) => app.set_status(&format!("Could not load pins: {error}")),
            },
        );
    }

    /// A dialog listing messages (pins, search results) with a jump button.
    fn show_message_dialog(self: &Rc<Self>, title: &str, empty: &str, messages: Vec<Message>) {
        let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
        list.set_margin_top(12);
        list.set_margin_bottom(12);
        list.set_margin_start(12);
        list.set_margin_end(12);
        let dialog = adw::Dialog::new();
        dialog.set_title(title);
        dialog.set_content_width(560);
        dialog.set_content_height(520);

        if messages.is_empty() {
            list.append(&empty_state("edit-find-symbolic", empty, ""));
        }
        if let Some((_, _, store)) = self.active() {
            store.read(|state| {
                for message in &messages {
                    let guild = state
                        .channels
                        .get(&message.channel_id)
                        .and_then(|channel| channel.guild_id);
                    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
                    card.add_css_class("card");
                    let body = self.message_body(message, true, guild, state);
                    body.set_margin_top(8);
                    body.set_margin_bottom(8);
                    body.set_margin_start(8);
                    body.set_margin_end(8);
                    card.append(&body);
                    let jump = gtk::Button::with_label("Jump");
                    jump.add_css_class("flat");
                    jump.set_halign(gtk::Align::End);
                    let app = self.clone();
                    let target = message.id;
                    let channel = message.channel_id;
                    let parent = dialog.clone();
                    jump.connect_clicked(move |_| {
                        parent.close();
                        app.jump_or_fetch(channel, target);
                    });
                    card.append(&jump);
                    list.append(&card);
                }
            });
        }

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_child(Some(&list));
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&scroll));
        dialog.set_child(Some(&view));
        dialog.present(Some(&self.window));
    }

    /// Jumps to a message, loading the history around it when necessary.
    /// Jumps to `message_id` in the just-opened `channel_id`: now if its row
    /// is already shown or nothing is loading (fetching around it if needed),
    /// otherwise as soon as the channel's first page of history arrives, so
    /// that page's render cannot snap the view back to the bottom afterwards.
    pub(crate) fn jump_when_ready(self: &Rc<Self>, channel_id: Snowflake, message_id: Snowflake) {
        let loading = self
            .with_session(|session| {
                session
                    .history
                    .get(&channel_id)
                    .is_some_and(|history| history.loading)
            })
            .unwrap_or(false);
        let shown = self
            .chat
            .borrow()
            .rows
            .iter()
            .any(|row| row.id == message_id);
        if loading && !shown {
            self.chat.borrow_mut().pending_jump = Some((channel_id, message_id));
        } else {
            self.chat.borrow_mut().pending_jump = None;
            self.jump_or_fetch(channel_id, message_id);
        }
    }

    /// Runs a jump [`jump_when_ready`](Self::jump_when_ready) deferred for
    /// `channel_id`, now that its history is in.
    fn take_pending_jump(self: &Rc<Self>, channel_id: Snowflake) {
        let pending = self.chat.borrow().pending_jump;
        if let Some((channel, message)) = pending
            && channel == channel_id
        {
            self.chat.borrow_mut().pending_jump = None;
            self.jump_or_fetch(channel, message);
        }
    }

    pub(crate) fn jump_or_fetch(self: &Rc<Self>, channel_id: Snowflake, message_id: Snowflake) {
        if self.jump_to_message(message_id) {
            return;
        }
        let Some((_, http, store)) = self.active() else {
            return;
        };
        self.spawn(
            async move { http.messages_around(channel_id, PAGE, message_id).await },
            move |app, result| match result {
                Ok(messages) => {
                    // Replace the window with the page around the target, so
                    // the list stays contiguous.
                    store.write(|state| state.load_around(channel_id, messages));
                    app.with_session_mut(|session| {
                        session.history.insert(channel_id, Default::default());
                    });
                    app.chat.borrow_mut().channel = None;
                    app.render_messages();
                    // The rows exist now (built synchronously); jump_to_message
                    // waits for their layout before centring. If the target is
                    // gone (e.g. deleted), say so rather than silently nothing.
                    if !app.jump_to_message(message_id) {
                        app.set_status("Couldn't find that message.");
                    }
                    app.update_present();
                }
                Err(error) => app.set_status(&format!("Could not load that message: {error}")),
            },
        );
    }

    // -- typing ----------------------------------------------------------

    /// Someone, you included, is typing in `channel`.
    pub(crate) fn note_typing(self: &Rc<Self>, channel: Snowflake, user_id: Snowflake) {
        self.with_session_mut(|session| {
            let list = session.typing.entry(channel).or_default();
            list.retain(|(id, _)| *id != user_id);
            list.push((user_id, std::time::Instant::now()));
        });
        self.render_typing();
        let app = self.clone();
        glib::timeout_add_seconds_local_once(crate::app::TYPING_TIMEOUT_SECS + 1, move || {
            app.render_typing();
        });
    }

    /// A user stops typing once their message arrives.
    pub(crate) fn clear_typing_for(self: &Rc<Self>, channel: Snowflake, user_id: Snowflake) {
        self.with_session_mut(|session| {
            if let Some(list) = session.typing.get_mut(&channel) {
                list.retain(|(id, _)| *id != user_id);
            }
        });
        self.render_typing();
    }

    pub(crate) fn render_typing(&self) {
        let names = self
            .with_session_mut(|session| {
                let channel = session.selected_channel?;
                let me = session.user.as_ref().map(|user| user.id);
                let list = session.typing.get_mut(&channel)?;
                list.retain(|(_, seen)| {
                    seen.elapsed().as_secs() < u64::from(crate::app::TYPING_TIMEOUT_SECS)
                });
                let ids: Vec<Snowflake> = list.iter().map(|(id, _)| *id).collect();
                let guild = session.selected_guild.filter(|_| !session.dm_mode);
                Some(session.store.read(|state| {
                    // Everyone else by name, then you.
                    let mut names: Vec<String> = ids
                        .iter()
                        .filter(|id| Some(**id) != me)
                        .filter_map(|id| state.users.get(id))
                        .map(|user| state.display_name(guild, user))
                        .collect();
                    if me.is_some_and(|me| ids.contains(&me)) {
                        names.push(YOU.to_owned());
                    }
                    names
                }))
            })
            .flatten()
            .unwrap_or_default();
        self.typing_label.set_label(&typing_text(&names));
    }
}

/// Stands for the account in the typing line.
const YOU: &str = "you";

/// "A is typing…", "A and B are typing…", "A, B and you are typing…":
/// everyone by name.
fn typing_text(names: &[String]) -> String {
    let Some((last, rest)) = names.split_last() else {
        return String::new();
    };
    if rest.is_empty() {
        if last == YOU {
            return "You are typing…".to_owned();
        }
        return format!("{last} is typing…");
    }
    format!("{} and {last} are typing…", rest.join(", "))
}

#[cfg(test)]
mod typing_tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn everyone_typing_is_named() {
        assert_eq!(typing_text(&[]), "");
        assert_eq!(typing_text(&names(&["Ana"])), "Ana is typing…");
        assert_eq!(typing_text(&names(&["you"])), "You are typing…");
        assert_eq!(
            typing_text(&names(&["Ana", "you"])),
            "Ana and you are typing…"
        );
        assert_eq!(
            typing_text(&names(&["Ana", "Bo", "Cy", "Dee", "you"])),
            "Ana, Bo, Cy, Dee and you are typing…"
        );
    }
}

#[cfg(test)]
mod page_tests {
    use super::*;

    #[test]
    fn the_first_page_fills_the_window_like_the_web_client() {
        // A small window: the 400 px floor, five 91 px rows, plus 5%; at least 10.
        assert_eq!(initial_page(500), 10);
        assert_eq!(initial_page(1080), 12);
        assert_eq!(initial_page(1440), 16);
        // A very tall window stops at 30.
        assert_eq!(initial_page(4000), 30);
        assert_eq!(scroll_page(500), 20);
    }
}
