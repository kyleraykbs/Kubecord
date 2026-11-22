//! The message composer: sending, replies, edits, uploads, typing
//! notifications, the emoji and GIF pickers, and autocomplete.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk::prelude::*;

use kc_core::Snowflake;
use kc_http::{Outgoing, cdn};
use kc_state::ChannelType;

use crate::app::TYPING_THROTTLE;
use crate::session::{Command, Upload};
use crate::shell::AppState;
use crate::widgets::{clear_box, clear_list, label};

/// Emoji size in the picker, when drawn in Discord's art.
const PICKER_EMOJI_SIZE: i32 = 28;
const MIB: usize = 1024 * 1024;
/// Rows offered by autocomplete.
const COMPLETIONS: usize = 8;
/// Custom emoji drawn in the picker before the search has to narrow it.
const PICKER_EMOJI: usize = 400;
/// How many GIF-picker tiles animate at once. The rest show a still preview,
/// so a wall of results does not run dozens of frame timers.
const GIF_PICKER_ANIMATED: usize = 16;
/// How many tiles to render per scroll page. Discord's GIF endpoints return a
/// single (cursor-less) batch, so "load more on scroll" renders that batch
/// incrementally rather than building every tile up front.
const GIF_PAGE: usize = 20;

/// A guild name and its `(id, name, animated)` emoji, for the picker.
type EmojiGroup = (String, Vec<(Snowflake, String, bool)>);

/// Everything needed to send a message, kept so a failed one can be retried.
#[derive(Clone)]
pub(crate) struct OutgoingDraft {
    pub content: String,
    pub reply_to: Option<Snowflake>,
    pub mention_reply: bool,
    pub uploads: Vec<Upload>,
}

/// Low bits for an optimistic message's id, so two sent in the same
/// millisecond still sort in send order and never collide.
fn next_low() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed) & 0x3F_FFFF
}

/// One autocomplete suggestion.
pub(crate) struct Completion {
    /// Shown in the list.
    pub(crate) title: String,
    pub(crate) detail: String,
    pub(crate) icon: Option<String>,
    /// A standard emoji shown instead of an icon.
    pub(crate) glyph: Option<String>,
    /// Text placed in the composer.
    pub(crate) display: String,
    /// What `display` becomes when the message is sent.
    pub(crate) raw: String,
    /// The buffer range `display` replaces; `None` for the `@`/`#`/`:`
    /// word at the cursor.
    pub(crate) span: Option<(i32, i32)>,
    /// Whether a space follows what is put in.
    pub(crate) trail: bool,
}

/// Emoji size in the composer, drawn in Discord's art.
const COMPOSER_EMOJI: i32 = 20;

/// Corrections offered for a misspelled word.
const SPELL_SUGGESTIONS: usize = 5;

/// Typing paused this long starts a new undo step.
const UNDO_PAUSE: Duration = Duration::from_secs(1);
/// Undo steps kept.
const UNDO_LIMIT: usize = 200;

/// The composer's text at one point (emoji as text), and where the cursor
/// was, in characters of that text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Snapshot {
    text: String,
    cursor: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit {
    Insert,
    Delete,
}

/// The composer's own undo history. GTK's records text but not the emoji
/// images put in place of typed emoji, so undoing through it duplicates
/// them; this one works on the message text, which drawing emoji as images
/// never changes.
#[derive(Debug, Default)]
struct History {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    current: Snapshot,
    /// The single-character edits being grouped into one step, and when the
    /// last one happened.
    typing: Option<(Edit, Instant)>,
}

impl History {
    /// Takes in the text after an edit. Typing (or deleting) character by
    /// character is one step until a pause, a switch between typing and
    /// deleting, or the end of a word; anything else is a step of its own.
    fn record(&mut self, now: Snapshot, at: Instant) {
        if now.text == self.current.text {
            self.current.cursor = now.cursor;
            return;
        }
        let before = self.current.text.chars().count();
        let after = now.text.chars().count();
        let edit = if after > before {
            Edit::Insert
        } else {
            Edit::Delete
        };
        let single = before.abs_diff(after) == 1;
        let continues = single
            && self.typing.is_some_and(|(kind, last)| {
                kind == edit && at.saturating_duration_since(last) < UNDO_PAUSE
            });
        let word_end = edit == Edit::Insert
            && now
                .text
                .chars()
                .nth(now.cursor.saturating_sub(1))
                .is_some_and(char::is_whitespace);
        let previous = std::mem::replace(&mut self.current, now);
        if !continues {
            self.undo.push(previous);
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.typing = (single && !word_end).then_some((edit, at));
    }

    fn undo(&mut self) -> Option<Snapshot> {
        let previous = self.undo.pop()?;
        self.redo
            .push(std::mem::replace(&mut self.current, previous.clone()));
        self.typing = None;
        Some(previous)
    }

    fn redo(&mut self) -> Option<Snapshot> {
        let next = self.redo.pop()?;
        self.undo
            .push(std::mem::replace(&mut self.current, next.clone()));
        self.typing = None;
        Some(next)
    }
}

/// Text between two iterators, emoji images turned back into their emoji.
fn expanded_text(start: &gtk::TextIter, end: &gtk::TextIter) -> String {
    let slice = start.slice(end);
    if !slice.contains('\u{fffc}') {
        return slice.to_string();
    }
    let mut out = String::with_capacity(slice.len());
    let mut iter = *start;
    for c in slice.chars() {
        if c == '\u{fffc}' {
            if let Some(emoji) = iter
                .paintable()
                .and_downcast::<crate::discord_emoji::EmojiPaintable>()
            {
                out.push_str(&emoji.emoji());
            }
        } else {
            out.push(c);
        }
        iter.forward_char();
    }
    out
}

/// Puts the selection on the clipboard with its emoji images as text.
/// False (doing nothing) when it has none, so the view copies as usual.
fn copy_with_emoji(view: &gtk::TextView) -> bool {
    let Some((start, end)) = view.buffer().selection_bounds() else {
        return false;
    };
    if !start.slice(&end).contains('\u{fffc}') {
        return false;
    }
    view.clipboard().set_text(&expanded_text(&start, &end));
    true
}

thread_local! {
    /// The composer's undo history.
    static HISTORY: std::cell::RefCell<History> = std::cell::RefCell::new(History::default());
    /// Set while the composer is changed in ways the history must not
    /// record (drawing emoji, undoing, text put there by the app).
    static COMPOSER_QUIET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The misspelled word a correction menu is open for: its buffer range
    /// and text.
    static SPELL_TARGET: std::cell::RefCell<Option<(usize, usize, String)>> = const { std::cell::RefCell::new(None) };
    /// The suggestions showing only hint (option names after a command):
    /// Enter sends rather than taking one.
    static COMPLETION_HINT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// A `:` was just typed (it may close a `:shortcode:`).
    static COLON_TYPED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// A redraw of the composer's emoji is queued.
    static COMPOSER_EMOJI_QUEUED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// `display -> raw` pairs for mentions and emoji inserted through
    /// autocomplete or the picker, applied when the message is sent.
    static TOKENS: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
    static COMPLETIONS_SHOWN: std::cell::RefCell<Vec<Completion>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl AppState {
    pub(crate) fn connect_composer(self: &Rc<Self>) {
        // GTK's undo cannot follow emoji drawn as images; ours does.
        self.composer.buffer().set_enable_undo(false);
        // Right-clicking a misspelled word offers its corrections.
        self.connect_spelling_menu();
        // A typed `:` may close a `:shortcode:`.
        self.composer
            .buffer()
            .connect_insert_text(|_, _, text| {
                if text == ":" && !COMPOSER_QUIET.get() {
                    COLON_TYPED.set(true);
                }
            });
        let app = self.clone();
        self.composer.buffer().connect_changed(move |_| {
            app.on_composer_changed();
        });

        let keys = gtk::EventControllerKey::new();
        let app = self.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            use gtk::gdk::Key;
            let completing = app.completion.is_visible();
            let control = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            let shift = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
            match key {
                Key::z | Key::Z if control => {
                    app.undo_composer(shift);
                    glib::Propagation::Stop
                }
                Key::y | Key::Y if control => {
                    app.undo_composer(true);
                    glib::Propagation::Stop
                }
                Key::Return | Key::KP_Enter => {
                    if modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
                        return glib::Propagation::Proceed;
                    }
                    if completing && !COMPLETION_HINT.get() {
                        app.accept_completion();
                    } else {
                        app.completion.popdown();
                        app.send_current();
                    }
                    glib::Propagation::Stop
                }
                Key::Tab if completing => {
                    app.accept_completion();
                    glib::Propagation::Stop
                }
                Key::Up | Key::Down if completing => {
                    app.move_completion(if key == Key::Up { -1 } else { 1 });
                    glib::Propagation::Stop
                }
                Key::Up if app.composer_text().is_empty() => {
                    app.edit_last_own();
                    glib::Propagation::Stop
                }
                Key::Escape => {
                    if completing {
                        app.completion.popdown();
                    } else {
                        app.cancel_context();
                    }
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.composer.add_controller(keys);

        // Copying or cutting text with emoji images copies the emoji (the
        // view's own copy would drop them).
        self.composer.connect_copy_clipboard(|view| {
            if copy_with_emoji(view) {
                view.stop_signal_emission_by_name("copy-clipboard");
            }
        });
        self.composer.connect_cut_clipboard(|view| {
            if copy_with_emoji(view) {
                view.buffer().delete_selection(true, view.is_editable());
                view.stop_signal_emission_by_name("cut-clipboard");
            }
        });

        // Pasting an image attaches it instead of inserting nothing.
        let app = self.clone();
        self.composer.connect_paste_clipboard(move |view| {
            let clipboard = view.clipboard();
            if !clipboard
                .formats()
                .contains_type(gtk::gdk::Texture::static_type())
            {
                return;
            }
            view.stop_signal_emission_by_name("paste-clipboard");
            let app = app.clone();
            clipboard.read_texture_async(gtk::gio::Cancellable::NONE, move |result| {
                if let Ok(Some(texture)) = result {
                    let bytes = texture.save_to_png_bytes();
                    app.add_upload("image.png".to_owned(), bytes.to_vec());
                }
            });
        });

        let app = self.clone();
        self.context_cancel
            .connect_clicked(move |_| app.cancel_context());
        self.context_ping.connect_toggled(|toggle| {
            toggle.set_label(if toggle.is_active() { "@ ON" } else { "@ OFF" });
        });

        let app = self.clone();
        self.attach_button
            .connect_clicked(move |_| app.choose_files());
        let app = self.clone();
        self.send_button.connect_clicked(move |_| {
            app.send_current();
            app.composer.grab_focus();
        });

        let app = self.clone();
        self.completion_list.connect_row_activated(move |_, _| {
            app.accept_completion();
        });

        self.build_emoji_picker();
        self.build_gif_picker();
    }

    /// The composer's text, emoji drawn as images turned back into text.
    pub(crate) fn composer_text(&self) -> String {
        let buffer = self.composer.buffer();
        expanded_text(&buffer.start_iter(), &buffer.end_iter())
    }

    /// The composer's text with one character per buffer position (an emoji
    /// drawn as an image is U+FFFC), so offsets in it are buffer offsets.
    fn composer_positions(&self) -> String {
        let buffer = self.composer.buffer();
        buffer
            .slice(&buffer.start_iter(), &buffer.end_iter(), true)
            .to_string()
    }

    fn composer_snapshot(&self) -> Snapshot {
        let buffer = self.composer.buffer();
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        Snapshot {
            text: self.composer_text(),
            cursor: expanded_text(&buffer.start_iter(), &cursor).chars().count(),
        }
    }

    /// The right-click menu for misspelled words: corrections (click one to
    /// use it) and Add to Dictionary. Elsewhere the usual menu shows.
    fn connect_spelling_menu(self: &Rc<Self>) {
        let actions = gtk::gio::SimpleActionGroup::new();
        let replace = gtk::gio::SimpleAction::new("replace", Some(glib::VariantTy::STRING));
        let app = self.clone();
        replace.connect_activate(move |_, value| {
            let Some(word) = value.and_then(|value| value.get::<String>()) else {
                return;
            };
            let Some((start, end, misspelled)) = SPELL_TARGET.take() else {
                return;
            };
            let buffer = app.composer.buffer();
            let mut from = buffer.iter_at_offset(start as i32);
            let mut to = buffer.iter_at_offset(end as i32);
            // Only if the word is still where it was.
            if from.slice(&to) != misspelled {
                return;
            }
            buffer.delete(&mut from, &mut to);
            buffer.insert(&mut from, &word);
            buffer.place_cursor(&from);
        });
        actions.add_action(&replace);
        let add = gtk::gio::SimpleAction::new("add", None);
        let app = self.clone();
        add.connect_activate(move |_, _| {
            if let Some((_, _, word)) = SPELL_TARGET.take() {
                crate::spelling::add_word(&word);
                app.recheck_spelling();
            }
        });
        actions.add_action(&add);
        self.composer.insert_action_group("spell", Some(&actions));

        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_SECONDARY);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let app = self.clone();
        click.connect_pressed(move |gesture, _, x, y| {
            if let Some(menu) = app.spelling_menu(x, y) {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                menu.popup();
            }
        });
        self.composer.add_controller(click);
    }

    /// The corrections menu (not yet shown) for the misspelled word at
    /// `x, y`, if there is one there.
    pub(crate) fn spelling_menu(self: &Rc<Self>, x: f64, y: f64) -> Option<gtk::PopoverMenu> {
        if !crate::spelling::enabled() {
            return None;
        }
        let view = &self.composer;
        let (bx, by) = view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let iter = view.iter_at_location(bx, by)?;
        let offset = iter.offset().max(0) as usize;
        let text = self.composer_positions();
        let (start, end) = crate::spelling::misspelled_spans(&text)
            .into_iter()
            .find(|(start, end)| (*start..*end).contains(&offset))?;
        let word: String = text.chars().skip(start).take(end - start).collect();

        let corrections = gtk::gio::Menu::new();
        let found = crate::spelling::suggestions(&word, SPELL_SUGGESTIONS);
        if found.is_empty() {
            // No action: shown greyed out.
            corrections.append(Some("No Suggestions"), Some("spell.none"));
        }
        for suggestion in &found {
            let item = gtk::gio::MenuItem::new(Some(suggestion), None);
            item.set_action_and_target_value(Some("spell.replace"), Some(&suggestion.to_variant()));
            corrections.append_item(&item);
        }
        let menu = gtk::gio::Menu::new();
        menu.append_section(None, &corrections);
        let more = gtk::gio::Menu::new();
        more.append(Some("Add to Dictionary"), Some("spell.add"));
        menu.append_section(None, &more);
        SPELL_TARGET.set(Some((start, end, word)));

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(view);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|popover| {
            // After the chosen item's action has run.
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        Some(popover)
    }

    /// Turns a `:shortcode:` just closed at the cursor into its emoji (an
    /// undo step of its own, so Ctrl+Z gets the shortcode back).
    fn close_shortcode(self: &Rc<Self>) {
        let buffer = self.composer.buffer();
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        let before = buffer.slice(&buffer.start_iter(), &cursor, true);
        let Some((emoji, length)) = closed_shortcode(&before) else {
            return;
        };
        let mut start = buffer.iter_at_offset(cursor.offset() - length as i32);
        let mut end = cursor;
        buffer.delete(&mut start, &mut end);
        buffer.insert(&mut start, emoji);
        self.completion.popdown();
    }

    /// Puts `text` in the composer as the app (a draft, a message to edit, a
    /// cleared box): a fresh start for undo.
    pub(crate) fn set_composer_text(&self, text: &str) {
        let buffer = self.composer.buffer();
        COMPOSER_QUIET.set(true);
        buffer.set_text(text);
        COMPOSER_QUIET.set(false);
        let now = self.composer_snapshot();
        HISTORY.with_borrow_mut(|history| {
            *history = History {
                current: now,
                ..History::default()
            }
        });
    }

    /// Undoes (or redoes) the last step of typing in the composer.
    pub(crate) fn undo_composer(self: &Rc<Self>, redo: bool) {
        let step = HISTORY.with_borrow_mut(|history| {
            if redo {
                history.redo()
            } else {
                history.undo()
            }
        });
        let Some(step) = step else {
            return;
        };
        let buffer = self.composer.buffer();
        COMPOSER_QUIET.set(true);
        buffer.set_text(&step.text);
        // Placed while the text is still plain, so the offset is right; the
        // emoji become images just after and the cursor moves with them.
        buffer.place_cursor(&buffer.iter_at_offset(step.cursor as i32));
        COMPOSER_QUIET.set(false);
        self.composer.scroll_mark_onscreen(&buffer.get_insert());
    }

    /// Redraws the composer's emoji (soon, once): typed emoji become Discord's
    /// art while it is on, and back into text when it is off.
    pub(crate) fn queue_composer_emoji(self: &Rc<Self>) {
        if COMPOSER_EMOJI_QUEUED.replace(true) {
            return;
        }
        let app = self.clone();
        glib::idle_add_local_once(move || {
            COMPOSER_EMOJI_QUEUED.set(false);
            app.redraw_composer_emoji();
        });
    }

    fn redraw_composer_emoji(self: &Rc<Self>) {
        // Same text either way: nothing for the undo history.
        let was_quiet = COMPOSER_QUIET.replace(true);
        self.swap_composer_emoji();
        COMPOSER_QUIET.set(was_quiet);
    }

    fn swap_composer_emoji(self: &Rc<Self>) {
        let buffer = self.composer.buffer();
        let text = self.composer_positions();
        if !crate::discord_emoji::active() {
            if !text.contains('\u{fffc}') {
                return;
            }
            let mut iter = buffer.start_iter();
            let mut drawn = Vec::new();
            for c in text.chars() {
                if c == '\u{fffc}'
                    && let Some(emoji) = iter
                        .paintable()
                        .and_downcast::<crate::discord_emoji::EmojiPaintable>()
                {
                    drawn.push((iter.offset(), emoji.emoji()));
                }
                iter.forward_char();
            }
            for (offset, emoji) in drawn.into_iter().rev() {
                let mut start = buffer.iter_at_offset(offset);
                let mut end = buffer.iter_at_offset(offset + 1);
                buffer.delete(&mut start, &mut end);
                buffer.insert(&mut start, &emoji);
            }
            return;
        }
        let mut offset = 0;
        let mut found = Vec::new();
        for segment in crate::discord_emoji::split(&text) {
            match segment {
                crate::discord_emoji::Segment::Text(text) => offset += text.chars().count(),
                crate::discord_emoji::Segment::Emoji(emoji) => {
                    let length = emoji.chars().count();
                    found.push((offset as i32, length as i32, emoji));
                    offset += length;
                }
            }
        }
        // From the end, so earlier offsets stay right.
        for (offset, length, emoji) in found.into_iter().rev() {
            let app = self.clone();
            let Some(picture) =
                crate::discord_emoji::paintable(&emoji, COMPOSER_EMOJI, move || {
                    app.queue_composer_emoji()
                })
            else {
                continue;
            };
            let mut start = buffer.iter_at_offset(offset);
            let mut end = buffer.iter_at_offset(offset + length);
            buffer.delete(&mut start, &mut end);
            buffer.insert_paintable(&mut start, &picture);
        }
    }

    pub(crate) fn insert_composer_text(&self, text: &str) {
        self.composer.buffer().insert_at_cursor(text);
        self.composer.grab_focus();
    }

    /// Re-underlines misspelled words in the composer. Clears the marks when
    /// spell checking is off. Called on every edit and when the setting or the
    /// dictionary changes.
    pub(crate) fn recheck_spelling(&self) {
        let buffer = self.composer.buffer();
        let tag = buffer
            .tag_table()
            .lookup("kc-misspelled")
            .or_else(|| {
                buffer.create_tag(
                    Some("kc-misspelled"),
                    &[("underline", &gtk::pango::Underline::Error)],
                )
            })
            .expect("the misspelled tag");
        buffer.remove_tag(&tag, &buffer.start_iter(), &buffer.end_iter());
        if !crate::spelling::enabled() {
            return;
        }
        let text = self.composer_positions();
        for (start, end) in crate::spelling::misspelled_spans(&text) {
            let (Ok(start), Ok(end)) = (i32::try_from(start), i32::try_from(end)) else {
                continue;
            };
            buffer.apply_tag(
                &tag,
                &buffer.iter_at_offset(start),
                &buffer.iter_at_offset(end),
            );
        }
    }

    /// Updates the composer for the selected channel: placeholder text,
    /// whether sending is allowed, and the reply/edit bar.
    pub(crate) fn render_composer(self: &Rc<Self>) {
        let info = self.with_session(|session| {
            let channel_id = session.selected_channel?;
            session.store.read(|state| {
                let channel = state.channels.get(&channel_id)?;
                if channel.kind.is_voice() && !channel.kind.is_text() {
                    return None;
                }
                let title = state.channel_title(channel);
                let prefix = if channel.is_private() { "@" } else { "#" };
                let context = if let Some(id) = session.editing {
                    Some(("Editing message".to_owned(), false, id))
                } else {
                    session.reply_to.and_then(|id| {
                        let message = state.messages.get(&channel_id)?.get(id)?;
                        let name = state.display_name(channel.guild_id, &message.author);
                        Some((format!("Replying to {name}"), true, id))
                    })
                };
                let mut hint = format!("Message {prefix}{title}");
                if let Some(seconds) = state.slowmode(channel_id) {
                    hint = match session.slowmode_until.get(&channel_id) {
                        Some(until) if *until > Instant::now() => format!(
                            "Slowmode · you can send again in {}",
                            slowmode_text(until.saturating_duration_since(Instant::now()))
                        ),
                        _ => format!(
                            "{hint} · Slowmode {}",
                            slowmode_text(Duration::from_secs(seconds.into()))
                        ),
                    };
                }
                Some((hint, state.can_send(channel_id), context))
            })
        });
        let Some((hint, can_send, context)) = info.flatten() else {
            self.composer_area.set_visible(false);
            return;
        };
        self.composer_area.set_visible(true);
        self.composer_box.set_visible(can_send);
        self.blocked_label.set_visible(!can_send);
        self.composer_hint.set_label(&hint);
        self.composer_hint
            .set_visible(self.composer_text().is_empty());

        match context {
            Some((text, ping, _)) => {
                self.context_label.set_label(&text);
                self.context_ping.set_visible(ping);
                self.context_bar.set_visible(true);
            }
            None => self.context_bar.set_visible(false),
        }
        self.render_upload_tray();
        self.update_composer_corners();
    }

    /// Squares off the composer's top corners when a bar (reply/edit context or
    /// the upload tray) sits directly above it, so the two read as one rounded
    /// box instead of leaving rounded notches at the join.
    fn update_composer_corners(&self) {
        let attached = self.context_bar.is_visible() || self.upload_tray.is_visible();
        if attached {
            self.composer_box.add_css_class("kc-attached");
        } else {
            self.composer_box.remove_css_class("kc-attached");
        }
    }

    fn on_composer_changed(self: &Rc<Self>) {
        if !COMPOSER_QUIET.get() {
            let now = self.composer_snapshot();
            HISTORY.with_borrow_mut(|history| history.record(now, Instant::now()));
        }
        if COLON_TYPED.replace(false) {
            let app = self.clone();
            glib::idle_add_local_once(move || app.close_shortcode());
        }
        let text = self.composer_text();
        self.composer_hint.set_visible(text.is_empty());
        self.queue_composer_emoji();
        self.update_completion();
        self.recheck_spelling();
        if text.trim().is_empty() {
            return;
        }

        // Typing unseen: no notice goes to Discord.
        if !self.capture.borrow().send_typing {
            return;
        }
        let target = self.with_session_mut(|session| {
            let channel = session.selected_channel?;
            if session.editing.is_some() {
                return None;
            }
            let now = Instant::now();
            let due = session
                .last_typing
                .is_none_or(|last| now.duration_since(last) >= TYPING_THROTTLE);
            if !due {
                return None;
            }
            session.last_typing = Some(now);
            Some(channel)
        });
        if let (Some(Some(channel)), Some((account, http, _))) = (target, self.active()) {
            let _ = self.cmd_tx.try_send(Command::SendTyping {
                account,
                http,
                channel,
            });
            // Discord now shows you typing; so does the typing line.
            if let Some(me) = self
                .with_session(|session| session.user.as_ref().map(|user| user.id))
                .flatten()
            {
                self.note_typing(channel, me);
            }
        }
    }

    // -- sending ---------------------------------------------------------

    /// Time left before slowmode lets the account post in `channel` again.
    pub(crate) fn slowmode_left(&self, channel: Snowflake) -> Option<Duration> {
        self.with_session(|session| session.slowmode_until.get(&channel).copied())
            .flatten()
            .map(|until| until.saturating_duration_since(Instant::now()))
            .filter(|left| !left.is_zero())
    }

    /// Starts `channel`'s slowmode cooldown: `wait` when Discord named one,
    /// else the channel's own slowmode if it applies to the account. The
    /// composer's hint counts it down, once a second.
    fn start_slowmode(self: &Rc<Self>, channel: Snowflake, wait: Option<Duration>) {
        let wait = wait.or_else(|| {
            self.with_session(|session| session.store.read(|state| state.slowmode(channel)))
                .flatten()
                .map(|seconds| Duration::from_secs(seconds.into()))
        });
        let Some(wait) = wait.filter(|wait| !wait.is_zero()) else {
            return;
        };
        self.with_session_mut(|session| {
            session
                .slowmode_until
                .insert(channel, Instant::now() + wait);
        });
        self.render_composer();
        let app = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let Some(app) = app.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let running = app.slowmode_left(channel).is_some();
            if !running {
                app.clear_slowmode(channel);
            }
            if app.with_session(|session| session.selected_channel) == Some(Some(channel)) {
                app.render_composer();
            }
            if running {
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
    }

    fn clear_slowmode(&self, channel: Snowflake) {
        self.with_session_mut(|session| {
            session.slowmode_until.remove(&channel);
        });
    }

    pub(crate) fn send_current(self: &Rc<Self>) {
        let typed = self.composer_text();
        let mut content = typed.trim().to_owned();
        // Swap display tokens (`@Name`, `:emoji:`) for what Discord expects.
        TOKENS.with_borrow(|tokens| {
            for (display, raw) in tokens {
                content = content.replace(display, raw);
            }
        });
        // `:name:` typed or pasted without the list: standard emoji, then
        // this server's (or, with Nitro, any) emoji of that name.
        let server_emoji = self
            .with_session(|session| {
                let guild = session.selected_guild.filter(|_| !session.dm_mode);
                let nitro = session.user.as_ref().is_some_and(|user| user.has_nitro());
                session.store.read(|state| {
                    state
                        .sendable_emojis(guild, nitro)
                        .into_iter()
                        .flat_map(|(_, emojis)| emojis)
                        .map(|emoji| (emoji.name.clone(), emoji.mention()))
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default();
        content = expand_shortcodes(&content, &|name| {
            server_emoji
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, mention)| mention.clone())
        });

        let Some((channel, editing, reply_to, uploads)) = self
            .with_session(|session| {
                Some((
                    session.selected_channel?,
                    session.editing,
                    session.reply_to,
                    session.uploads.clone(),
                ))
            })
            .flatten()
        else {
            return;
        };
        let Some((_, http, _)) = self.active() else {
            return;
        };
        // A slash command runs instead of being sent (a built-in one turns
        // into the text to send).
        if editing.is_none() && uploads.is_empty() && content.starts_with('/') {
            match self.send_slash(&content) {
                crate::slash::Sent::Handled => {
                    if self.composer_text().is_empty() {
                        TOKENS.with_borrow_mut(Vec::clear);
                    }
                    return;
                }
                crate::slash::Sent::Text(text) => content = text,
                crate::slash::Sent::NotCommand => {}
            }
        }
        if content.is_empty() && uploads.is_empty() {
            return;
        }
        if content.chars().count() > 2000 {
            self.set_status("Messages are limited to 2000 characters");
            return;
        }
        // Slowmode: keep the text and say how long, rather than sending a
        // message Discord would refuse. (Edits are not slowed.)
        if editing.is_none()
            && let Some(left) = self.slowmode_left(channel)
        {
            self.set_status(&format!(
                "Slowmode is on: you can send again in {}",
                slowmode_text(left)
            ));
            return;
        }

        let mention_reply = self.context_ping.is_active();
        self.set_composer_text("");
        TOKENS.with_borrow_mut(Vec::clear);
        self.with_session_mut(|session| {
            session.editing = None;
            session.reply_to = None;
            session.uploads.clear();
            session.last_typing = None;
        });
        self.render_composer();

        if let Some(message_id) = editing {
            self.spawn(
                async move { http.edit_message(channel, message_id, &content).await },
                move |app, result| {
                    if let Err(error) = result {
                        app.restore_draft(&typed);
                        app.set_status(&format!("Could not edit: {error}"));
                    }
                },
            );
            return;
        }

        self.dispatch_send(
            channel,
            OutgoingDraft {
                content,
                reply_to,
                mention_reply,
                uploads,
            },
        );
    }

    /// Sends a message, showing it at once as a greyed-out "sending" row that
    /// the gateway's confirmation (matched by nonce) replaces. A failed send
    /// stays as a row offering Retry or Delete; retry calls back here.
    pub(crate) fn dispatch_send(self: &Rc<Self>, channel: Snowflake, draft: OutgoingDraft) {
        let Some((_, http, store)) = self.active() else {
            return;
        };
        // Text (even with attachments) shows optimistically; an
        // attachments-only send would be an empty bubble, so it does not,
        // and neither does a send while the view is on older messages (it
        // jumps to the present below, where the real message lands).
        let nonce = (!draft.content.is_empty() && !self.detached(channel))
            .then(|| store.read(|state| state.me.clone()))
            .flatten()
            .map(|me| {
                let id = Snowflake::from_unix_millis(kc_state::time::unix_millis_now(), next_low());
                let nonce = id.get().to_string();
                let mut message = kc_state::Message::pending(
                    id,
                    channel,
                    me,
                    draft.content.clone(),
                    nonce.clone(),
                );
                if let Some(target) = draft.reply_to {
                    message.kind = kc_state::Message::REPLY;
                    message.message_reference = Some(kc_state::MessageReference {
                        message_id: Some(target),
                        channel_id: Some(channel),
                        guild_id: None,
                    });
                    message.referenced_message = store
                        .read(|state| {
                            state
                                .messages
                                .get(&channel)
                                .and_then(|b| b.get(target).cloned())
                        })
                        .map(Box::new);
                }
                store.add_pending(channel, message);
                self.chat
                    .borrow_mut()
                    .pending_sends
                    .insert(nonce.clone(), draft.clone());
                nonce
            });
        self.follow_sent_message(channel);
        self.start_slowmode(channel, None);

        let OutgoingDraft {
            content,
            reply_to,
            mention_reply,
            uploads,
        } = draft;
        let send_nonce = nonce.clone();
        self.spawn(
            async move {
                let mut attachments = Vec::with_capacity(uploads.len());
                for (index, upload) in uploads.iter().enumerate() {
                    let mut handle = http
                        .upload_attachment(channel, &upload.name, upload.bytes.to_vec())
                        .await?;
                    // The send expects sequential ids and the cloud filename.
                    attachments.push(serde_json::json!({
                        "id": index.to_string(),
                        "filename": upload.name,
                        "uploaded_filename": handle["upload_filename"].take(),
                    }));
                }
                http.send(
                    channel,
                    &Outgoing {
                        content,
                        reply_to,
                        mention_reply,
                        attachments,
                        nonce: send_nonce,
                        ..Outgoing::default()
                    },
                )
                .await
            },
            move |app, result| match result {
                Ok(message) => {
                    // Reconcile at once (the gateway echo, if it also comes,
                    // is idempotent); drop the retry draft.
                    if let (Some(nonce), Some((_, _, store))) = (&nonce, app.active()) {
                        app.chat.borrow_mut().pending_sends.remove(nonce);
                        if let Ok(value) = serde_json::to_value(&message) {
                            store.apply("MESSAGE_CREATE", &value);
                        }
                    }
                }
                Err(error) => {
                    // Slowmode refused it: count down from Discord's own wait.
                    // Any other failure means nothing was posted, so no
                    // cooldown applies.
                    let status = match error.slowmode_wait() {
                        Some(wait) => {
                            let wait = Duration::from_secs_f64(wait.max(0.0));
                            app.start_slowmode(channel, Some(wait));
                            format!(
                                "Message not sent: slowmode is on, you can send again in {}",
                                slowmode_text(wait)
                            )
                        }
                        None => {
                            app.clear_slowmode(channel);
                            format!("Message not sent: {error}")
                        }
                    };
                    // The optimistic row stays, now marked failed with Retry /
                    // Delete; without one (an attachments-only or
                    // jumped-to-present send) it is only reported.
                    if let (Some(nonce), Some((_, _, store))) = (&nonce, app.active()) {
                        store.set_send_state(channel, nonce, kc_state::SendState::Failed);
                    }
                    app.set_status(&status);
                }
            },
        );
    }

    /// Re-sends a message whose send failed, dropping the failed row first.
    pub(crate) fn retry_send(self: &Rc<Self>, channel: Snowflake, nonce: &str) {
        if let Some(left) = self.slowmode_left(channel) {
            self.set_status(&format!(
                "Slowmode is on: you can retry in {}",
                slowmode_text(left)
            ));
            return;
        }
        let draft = self.chat.borrow_mut().pending_sends.remove(nonce);
        if let Some((_, _, store)) = self.active() {
            store.drop_pending(channel, nonce);
        }
        if let Some(draft) = draft {
            self.dispatch_send(channel, draft);
        }
    }

    /// Discards a failed message the user chose not to resend.
    pub(crate) fn discard_send(self: &Rc<Self>, channel: Snowflake, nonce: &str) {
        self.chat.borrow_mut().pending_sends.remove(nonce);
        if let Some((_, _, store)) = self.active() {
            store.drop_pending(channel, nonce);
        }
    }

    /// Puts a command that failed back, to fix and run again.
    pub(crate) fn restore_command(&self, text: &str) {
        self.restore_draft(text);
    }

    /// Puts unsent text back unless the user has already typed something new.
    fn restore_draft(&self, text: &str) {
        if self.composer_text().is_empty() {
            self.set_composer_text(text);
        }
    }

    pub(crate) fn start_reply(self: &Rc<Self>, message_id: Snowflake) {
        self.with_session_mut(|session| {
            session.reply_to = Some(message_id);
            session.editing = None;
        });
        self.context_ping.set_active(true);
        self.render_composer();
        self.composer.grab_focus();
    }

    pub(crate) fn start_edit(self: &Rc<Self>, message_id: Snowflake) {
        let content = self.with_session_mut(|session| {
            let channel = session.selected_channel?;
            let content = session.store.read(|state| {
                Some(
                    state
                        .messages
                        .get(&channel)?
                        .get(message_id)?
                        .content
                        .clone(),
                )
            })?;
            session.editing = Some(message_id);
            session.reply_to = None;
            Some(content)
        });
        if let Some(Some(content)) = content {
            self.set_composer_text(&content);
            self.render_composer();
            self.composer.grab_focus();
        }
    }

    fn edit_last_own(self: &Rc<Self>) {
        let last = self.with_session(|session| {
            let channel = session.selected_channel?;
            session.store.read(|state| {
                let me = state.my_id()?;
                state
                    .messages
                    .get(&channel)?
                    .messages()
                    .iter()
                    .rev()
                    .find(|message| message.author.id == me && !message.is_system())
                    .map(|message| message.id)
            })
        });
        if let Some(Some(id)) = last {
            self.start_edit(id);
        }
    }

    pub(crate) fn cancel_context(self: &Rc<Self>) {
        let was_editing = self
            .with_session_mut(|session| {
                let was_editing = session.editing.take().is_some();
                session.reply_to = None;
                was_editing
            })
            .unwrap_or(false);
        if was_editing {
            self.set_composer_text("");
        }
        self.render_composer();
    }

    // -- uploads ---------------------------------------------------------

    fn upload_limit(&self) -> usize {
        let premium = self
            .with_session(|session| session.user.as_ref().and_then(|user| user.premium_type))
            .flatten();
        match premium {
            Some(2) => 500 * MIB,
            Some(1 | 3) => 50 * MIB,
            _ => 10 * MIB,
        }
    }

    pub(crate) fn add_upload(self: &Rc<Self>, name: String, bytes: Vec<u8>) {
        let limit = self.upload_limit();
        if bytes.len() > limit {
            self.set_status(&format!(
                "{name} is larger than the {} MiB upload limit",
                limit / MIB
            ));
            return;
        }
        let added = self.with_session_mut(|session| {
            if session.uploads.len() >= 10 {
                return false;
            }
            session.uploads.push(Upload {
                name,
                bytes: Arc::new(bytes),
            });
            true
        });
        if added == Some(false) {
            self.set_status("A message can carry at most 10 files");
        }
        self.render_upload_tray();
    }

    fn choose_files(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::new();
        dialog.set_title("Attach Files");
        let app = self.clone();
        dialog.open_multiple(
            Some(&self.window),
            gtk::gio::Cancellable::NONE,
            move |result| {
                let Ok(files) = result else {
                    return;
                };
                for index in 0..files.n_items() {
                    let Some(file) = files.item(index).and_downcast::<gtk::gio::File>() else {
                        continue;
                    };
                    let Some(path) = file.path() else {
                        continue;
                    };
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "file".to_owned());
                    app.spawn(
                        async move {
                            tokio::fs::read(&path)
                                .await
                                .map_err(|error| error.to_string())
                        },
                        move |app, result| match result {
                            Ok(bytes) => app.add_upload(name, bytes),
                            Err(error) => app.set_status(&format!("Could not read file: {error}")),
                        },
                    );
                }
            },
        );
    }

    fn render_upload_tray(self: &Rc<Self>) {
        clear_box(&self.upload_tray);
        let uploads = self
            .with_session(|session| session.uploads.clone())
            .unwrap_or_default();
        self.upload_tray.set_visible(!uploads.is_empty());
        self.update_composer_corners();
        // Previews of files no longer attached are dropped.
        let keys: Vec<usize> = uploads.iter().map(preview_key).collect();
        PREVIEWS.with_borrow_mut(|previews| previews.retain(|key, _| keys.contains(key)));
        // Side by side, scrolling sideways when there are more than fit.
        let cards = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
        // Exactly as tall as a card (left to measure itself, the scroller
        // asked for twice that).
        scroll.set_min_content_height(UPLOAD_CARD_HEIGHT);
        scroll.set_propagate_natural_height(false);
        scroll.set_hexpand(true);
        scroll.set_vexpand(false);
        cards.set_valign(gtk::Align::Start);
        scroll.set_child(Some(&cards));
        self.upload_tray.append(&scroll);
        for (index, upload) in uploads.iter().enumerate() {
            cards.append(&self.upload_card(index, upload));
        }
    }

    /// One attached file, as it will be sent: a picture's preview (a GIF
    /// playing), or an icon for its kind, with its name and size.
    fn upload_card(self: &Rc<Self>, index: usize, upload: &Upload) -> gtk::Box {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 4);
        card.add_css_class("card");
        card.add_css_class("kc-upload-card");
        card.set_size_request(UPLOAD_CARD_MIN, -1);
        card.set_valign(gtk::Align::Start);
        card.set_tooltip_text(Some(&upload.name));

        let frame = gtk::Overlay::new();
        let kind = upload_kind(&upload.name, &upload.bytes);
        if kind == UploadKind::Picture {
            // The whole picture, at its own shape: as tall as a preview and
            // as wide as that makes it (narrower for a very wide one). Its
            // natural size is the full image, so clamps hold it to that.
            let picture = gtk::Picture::new();
            picture.set_content_fit(gtk::ContentFit::Contain);
            picture.set_can_shrink(true);
            picture.set_overflow(gtk::Overflow::Hidden);
            picture.add_css_class("kc-upload-preview");
            let across = libadwaita::Clamp::new();
            across.set_orientation(gtk::Orientation::Horizontal);
            across.set_child(Some(&picture));
            let down = libadwaita::Clamp::new();
            down.set_orientation(gtk::Orientation::Vertical);
            down.set_child(Some(&across));
            down.set_halign(gtk::Align::Center);
            let size_to = {
                let down = down.clone();
                move |width: i32, height: i32| {
                    across.set_maximum_size(width);
                    across.set_tightening_threshold(width);
                    down.set_maximum_size(height);
                    down.set_tightening_threshold(height);
                    across.set_size_request(width, height);
                }
            };
            size_to(PREVIEW_HEIGHT, PREVIEW_HEIGHT);
            picture.connect_paintable_notify(move |picture| {
                if let Some(paintable) = picture.paintable() {
                    let (width, height) = preview_fit(paintable.intrinsic_width(), paintable.intrinsic_height());
                    size_to(width, height);
                }
            });
            self.fill_upload_preview(&picture, upload);
            frame.set_child(Some(&down));
        } else {
            let icon = gtk::Image::from_icon_name(match kind {
                UploadKind::Video => "video-x-generic",
                UploadKind::Audio => "audio-x-generic",
                _ => "text-x-generic",
            });
            icon.set_pixel_size(64);
            icon.set_size_request(UPLOAD_CARD_MIN - 16, PREVIEW_HEIGHT);
            icon.add_css_class("kc-upload-preview");
            frame.set_child(Some(&icon));
        }
        let remove = gtk::Button::from_icon_name("user-trash-symbolic");
        remove.set_tooltip_text(Some("Remove"));
        remove.add_css_class("osd");
        remove.add_css_class("circular");
        remove.set_halign(gtk::Align::End);
        remove.set_valign(gtk::Align::Start);
        remove.set_margin_top(4);
        remove.set_margin_end(4);
        let app = self.clone();
        remove.connect_clicked(move |_| {
            app.with_session_mut(|session| {
                if index < session.uploads.len() {
                    session.uploads.remove(index);
                }
            });
            app.render_upload_tray();
        });
        frame.add_overlay(&remove);
        card.append(&frame);

        let name = label(&upload.name, &["caption"]);
        name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        name.set_max_width_chars(1);
        name.set_hexpand(true);
        card.append(&name);
        let size = label(
            &crate::markdown::format_size(upload.bytes.len() as u64),
            &["caption", "dim-label"],
        );
        card.append(&size);
        card
    }

    /// Shows `upload`'s picture in `picture`: a GIF animated, anything else
    /// scaled down, decoded away from the main thread (once per file).
    fn fill_upload_preview(self: &Rc<Self>, picture: &gtk::Picture, upload: &Upload) {
        if crate::animation::is_gif(&upload.bytes)
            && let Some(animation) = crate::animation::decode(&upload.bytes)
        {
            crate::animation::GifPlayer::attach(picture, &animation);
            return;
        }
        let key = preview_key(upload);
        if let Some(texture) = PREVIEWS.with_borrow(|previews| previews.get(&key).cloned()) {
            picture.set_paintable(Some(&texture));
            return;
        }
        let bytes = upload.bytes.clone();
        let target = picture.downgrade();
        let shown = glib::SendWeakRef::from(target);
        self.spawn(
            async move {
                tokio::task::spawn_blocking(move || upload_preview(&bytes))
                    .await
                    .ok()
                    .flatten()
            },
            move |_, texture| {
                let Some(texture) = texture else {
                    return;
                };
                PREVIEWS.with_borrow_mut(|previews| previews.insert(key, texture.clone()));
                if let Some(picture) = shown.upgrade() {
                    picture.set_paintable(Some(&texture));
                }
            },
        );
    }

    // -- emoji picker ----------------------------------------------------

    /// The emoji picker: Server Emoji and Standard tabs. In DMs there are
    /// no server emoji, so only Standard shows.
    fn build_emoji_picker(self: &Rc<Self>) {
        use libadwaita as adw;

        let popover = gtk::Popover::new();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_margin_top(8);
        content.set_margin_bottom(8);
        content.set_margin_start(8);
        content.set_margin_end(8);

        let stack = adw::ViewStack::new();
        let tabs = adw::InlineViewSwitcher::new();
        tabs.set_stack(Some(&stack));
        tabs.set_homogeneous(true);
        tabs.add_css_class("flat");
        content.append(&tabs);

        // Server emoji: the account's usable custom emoji.
        let server = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search server emoji"));
        server.append(&search);
        let sections = gtk::Box::new(gtk::Orientation::Vertical, 6);
        server.append(&picker_scroll(&sections));
        let server_page = stack.add_titled(&server, Some("server"), "Server Emoji");

        // Standard emoji, built the first time the tab opens.
        let standard = gtk::Box::new(gtk::Orientation::Vertical, 6);
        stack.add_titled(&standard, Some("standard"), "Standard");
        // Built for which emoji style (rebuilt when Discord's emoji are
        // switched on or off).
        let built: Rc<std::cell::Cell<Option<u32>>> = Rc::default();
        let ensure_standard = {
            let app = self.clone();
            let standard = standard.clone();
            let stack = stack.clone();
            move || {
                let style = crate::discord_emoji::generation();
                if stack.visible_child_name().as_deref() == Some("standard")
                    && built.replace(Some(style)) != Some(style)
                {
                    // Filled on the next idle so the popover opens at once
                    // (the page has ~1,900 buttons).
                    let app = app.clone();
                    let standard = standard.clone();
                    glib::idle_add_local_once(move || {
                        crate::widgets::clear_box(&standard);
                        let target = app.clone();
                        app.fill_standard_emoji(
                            &standard,
                            Rc::new(move |text: &str| target.insert_composer_text(text)),
                        );
                    });
                }
            }
        };
        let open = ensure_standard.clone();
        stack.connect_visible_child_name_notify(move |_| open());

        content.append(&stack);
        popover.set_child(Some(&content));

        let app = self.clone();
        let target = sections.clone();
        let entry = search.clone();
        popover.connect_show(move |_| {
            let in_dm = app
                .with_session(|session| session.dm_mode || session.selected_guild.is_none())
                .unwrap_or(true);
            server_page.set_visible(!in_dm);
            tabs.set_visible(!in_dm);
            // Opens on Standard; server emoji are a tab away.
            stack.set_visible_child_name("standard");
            if !in_dm {
                entry.set_text("");
                app.fill_emoji_sections(&target, "");
            }
            ensure_standard();
        });
        let app = self.clone();
        let target = sections.clone();
        search.connect_search_changed(move |entry| {
            app.fill_emoji_sections(&target, &entry.text().to_lowercase());
        });
        self.emoji_button.set_popover(Some(&popover));
    }

    /// The Unicode emoji by category, with a search over names and
    /// shortcodes.
    /// Fills `page` with the standard (Unicode) emoji picker -- search,
    /// category bar and grid; `on_pick` receives the emoji chosen.
    pub(crate) fn fill_standard_emoji(self: &Rc<Self>, page: &gtk::Box, on_pick: Rc<dyn Fn(&str)>) {
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search emoji"));
        page.append(&search);
        // A row of category icons that jump to each section (like Discord).
        let nav = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        nav.add_css_class("linked");
        nav.add_css_class("kc-emoji-nav");
        nav.set_halign(gtk::Align::Center);
        page.append(&nav);
        let groups = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let scroll = picker_scroll(&groups);
        page.append(&scroll);

        let query = Rc::new(std::cell::RefCell::new(String::new()));
        let mut parts: Vec<(gtk::Label, gtk::FlowBox, Rc<Vec<String>>)> = Vec::new();
        for group in emojis::Group::iter() {
            let heading = label(&group_title(group).to_uppercase(), &["kc-category"]);

            // The nav button that scrolls this section's heading to the top.
            let jump = match crate::discord_emoji::image(group_icon(group), 20) {
                Some(image) => {
                    let jump = gtk::Button::new();
                    jump.set_child(Some(&image));
                    jump
                }
                None => gtk::Button::with_label(group_icon(group)),
            };
            jump.add_css_class("flat");
            jump.add_css_class("kc-emoji-nav-button");
            jump.set_tooltip_text(Some(group_title(group)));
            let scroll = scroll.clone();
            let heading_target = heading.clone();
            jump.connect_clicked(move |_| {
                scroll_to_heading(&scroll, heading_target.upcast_ref());
            });
            nav.append(&jump);
            let flow = gtk::FlowBox::new();
            flow.set_selection_mode(gtk::SelectionMode::None);
            flow.set_max_children_per_line(8);
            flow.set_min_children_per_line(8);
            flow.set_homogeneous(true);
            let mut keys = Vec::new();
            // Discord's emoji go up to Unicode 15.1; newer ones may be
            // missing from the system emoji font and draw as boxes.
            for emoji in group.emojis().filter(|emoji| {
                emoji
                    .skin_tone()
                    .is_none_or(|tone| tone == emojis::SkinTone::Default)
                    && emoji.unicode_version() <= emojis::UnicodeVersion::new(15, 1)
            }) {
                let button = match crate::discord_emoji::image(emoji.as_str(), PICKER_EMOJI_SIZE) {
                    Some(image) => {
                        let button = gtk::Button::new();
                        button.set_child(Some(&image));
                        button
                    }
                    None => gtk::Button::with_label(emoji.as_str()),
                };
                button.add_css_class("flat");
                button.add_css_class("kc-picker-emoji");
                button.add_css_class("kc-standard-emoji");
                let code = emoji.shortcode().unwrap_or(emoji.name());
                button.set_tooltip_text(Some(&format!(":{code}:")));
                let text = emoji.as_str();
                let pick = on_pick.clone();
                button.connect_clicked(move |_| pick(text));
                flow.insert(&button, -1);
                let mut key = emoji.name().to_lowercase();
                for code in emoji.shortcodes() {
                    key.push(' ');
                    key.push_str(code);
                }
                keys.push(key);
            }
            let keys = Rc::new(keys);
            let matches = keys.clone();
            let wanted = query.clone();
            flow.set_filter_func(move |child| {
                let wanted = wanted.borrow();
                wanted.is_empty()
                    || usize::try_from(child.index())
                        .ok()
                        .and_then(|index| matches.get(index))
                        .is_some_and(|key| key.contains(wanted.as_str()))
            });
            groups.append(&heading);
            groups.append(&flow);
            parts.push((heading, flow, keys));
        }
        search.connect_search_changed(move |entry| {
            let text = entry.text().to_lowercase();
            *query.borrow_mut() = text.clone();
            for (heading, flow, keys) in &parts {
                flow.invalidate_filter();
                // A category with nothing left disappears, heading and all.
                let any = text.is_empty() || keys.iter().any(|key| key.contains(text.as_str()));
                heading.set_visible(any);
                flow.set_visible(any);
            }
        });
    }

    fn fill_emoji_sections(self: &Rc<Self>, sections: &gtk::Box, query: &str) {
        clear_box(sections);
        let groups: Vec<EmojiGroup> = self
            .with_session(|session| {
                let guild = session.selected_guild.filter(|_| !session.dm_mode);
                let nitro = session.user.as_ref().is_some_and(|user| user.has_nitro());
                session.store.read(|state| {
                    state
                        .sendable_emojis(guild, nitro)
                        .into_iter()
                        .map(|(guild, emojis)| {
                            (
                                guild.name.clone(),
                                emojis
                                    .into_iter()
                                    .filter(|emoji| {
                                        query.is_empty()
                                            || emoji.name.to_lowercase().contains(query)
                                    })
                                    .map(|emoji| (emoji.id, emoji.name.clone(), emoji.animated))
                                    .collect::<Vec<_>>(),
                            )
                        })
                        .filter(|(_, emojis)| !emojis.is_empty())
                        .collect()
                })
            })
            .unwrap_or_default();

        if groups.is_empty() {
            let empty = label(
                if query.is_empty() {
                    "This server has no custom emoji you can use here."
                } else {
                    "No server emoji match."
                },
                &["dim-label", "caption"],
            );
            empty.set_wrap(true);
            empty.set_margin_top(12);
            sections.append(&empty);
            return;
        }

        let mut budget = PICKER_EMOJI;
        for (guild, emojis) in groups {
            if budget == 0 {
                break;
            }
            sections.append(&label(&guild.to_uppercase(), &["kc-category"]));
            let flow = gtk::FlowBox::new();
            flow.set_selection_mode(gtk::SelectionMode::None);
            flow.set_max_children_per_line(8);
            flow.set_min_children_per_line(8);
            flow.set_homogeneous(true);
            for (id, name, animated) in emojis.into_iter().take(budget) {
                budget -= 1;
                let button = gtk::Button::new();
                button.add_css_class("flat");
                button.add_css_class("kc-picker-emoji");
                let image = gtk::Image::new();
                image.set_pixel_size(28);
                self.images.set_image(&image, &cdn::emoji(id, false, 64));
                button.set_child(Some(&image));
                button.set_tooltip_text(Some(&format!(":{name}:")));
                let app = self.clone();
                button.connect_clicked(move |_| {
                    let display = format!(":{name}:");
                    let raw = if animated {
                        format!("<a:{name}:{id}>")
                    } else {
                        format!("<:{name}:{id}>")
                    };
                    remember_token(display.clone(), raw);
                    app.insert_composer_text(&format!("{display} "));
                });
                flow.insert(&button, -1);
            }
            sections.append(&flow);
        }
    }

    // -- GIF picker ------------------------------------------------------

    fn build_gif_picker(self: &Rc<Self>) {
        let popover = gtk::Popover::new();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_margin_top(8);
        content.set_margin_bottom(8);
        content.set_margin_start(8);
        content.set_margin_end(8);

        // Two tabs: your favourites (the default) and a Tenor search.
        let tabs = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        tabs.add_css_class("linked");
        tabs.set_halign(gtk::Align::Center);
        let favorited = gtk::ToggleButton::with_label("Favorited");
        let searched = gtk::ToggleButton::with_label("Search");
        searched.set_group(Some(&favorited));
        favorited.set_active(true);
        tabs.append(&favorited);
        tabs.append(&searched);
        content.append(&tabs);

        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search your favorites"));
        content.append(&search);

        let flow = gif_flow();
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_min_content_height(360);
        scroll.set_min_content_width(380);
        scroll.set_child(Some(&flow));

        // A spinner centred over the grid while a fetch is in flight, so the
        // panel is never just blank while loading.
        let spinner = gtk::Spinner::new();
        spinner.set_halign(gtk::Align::Center);
        spinner.set_valign(gtk::Align::Center);
        spinner.set_size_request(32, 32);
        spinner.set_visible(false);
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&scroll));
        overlay.add_overlay(&spinner);
        content.append(&overlay);
        popover.set_child(Some(&content));
        GIF_SPINNER.with_borrow_mut(|slot| *slot = Some(spinner));

        // Reaching the bottom renders the next page of the buffered results.
        let app = self.clone();
        scroll.connect_edge_reached(move |_, pos| {
            if pos == gtk::PositionType::Bottom {
                app.append_gif_page();
            }
        });

        // Switching tab re-runs the current search against that tab's source
        // and updates the placeholder.
        let app = self.clone();
        let entry = search.clone();
        favorited.connect_toggled(move |tab| {
            if !tab.is_active() {
                return;
            }
            GIF_TAB.with(|t| t.set(true));
            entry.set_placeholder_text(Some("Search your favorites"));
            app.load_gifs(entry.text().trim().to_owned());
        });
        let app = self.clone();
        let entry = search.clone();
        searched.connect_toggled(move |tab| {
            if !tab.is_active() {
                return;
            }
            GIF_TAB.with(|t| t.set(false));
            entry.set_placeholder_text(Some("Search Tenor"));
            app.load_gifs(entry.text().trim().to_owned());
        });

        let app = self.clone();
        let fav_tab = favorited.clone();
        let entry = search.clone();
        popover.connect_show(move |_| {
            entry.set_text("");
            fav_tab.set_active(true); // open on the Favorited tab
            GIF_TAB.with(|t| t.set(true));
            // Refetch favourites each opening (they may have changed).
            crate::gif_favorites::clear();
            app.load_gifs(String::new());
        });
        let app = self.clone();
        search.connect_search_changed(move |entry| {
            app.load_gifs(entry.text().trim().to_owned());
        });
        self.gif_button.set_popover(Some(&popover));
        GIF_SECTIONS.with_borrow_mut(|slot| *slot = Some(flow));
    }

    fn load_gifs(self: &Rc<Self>, query: String) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let asked = query.clone();
        GIF_QUERY.with_borrow_mut(|current| *current = query.clone());
        let favorited = GIF_TAB.with(|t| t.get());
        // Only spin when a real network fetch runs -- filtering already-cached
        // favourites as you type is instant and should not flash a spinner.
        if !favorited || !crate::gif_favorites::is_loaded() {
            Self::set_gif_loading(true);
        }
        if favorited {
            // The favourites are fetched once per picker opening and cached, so
            // typing filters them without re-fetching. They have no title, but
            // a Tenor URL carries a descriptive slug, so match on that.
            // The cache lives on the GTK thread; the fetch runs on a worker
            // thread, so only fetch when not yet loaded and populate the cache
            // in `done` (on the GTK thread). `None` means "already loaded,
            // nothing fetched".
            let already = crate::gif_favorites::is_loaded();
            self.spawn(
                async move {
                    if already {
                        None
                    } else {
                        Some(http.favorite_gifs().await.unwrap_or_default())
                    }
                },
                move |app, fetched| {
                    if let Some(favorites) = fetched {
                        crate::gif_favorites::set_all(favorites);
                    }
                    if GIF_QUERY.with_borrow(|current| *current != asked) {
                        return;
                    }
                    let needle = asked.to_lowercase();
                    let matches = crate::gif_favorites::with(|cache| {
                        cache
                            .unwrap_or_default()
                            .iter()
                            .filter(|gif| {
                                needle.is_empty()
                                    || gif.url.to_lowercase().contains(&needle)
                                    || gif.src.to_lowercase().contains(&needle)
                            })
                            .cloned()
                            .collect::<Vec<_>>()
                    });
                    app.show_gifs(matches);
                },
            );
        } else if query.is_empty() {
            self.spawn(
                async move { http.gif_trending().await },
                move |app, result| {
                    if GIF_QUERY.with_borrow(|current| *current != asked) {
                        return;
                    }
                    match result {
                        Ok(gifs) => app.show_gifs(gifs),
                        Err(error) => {
                            Self::set_gif_loading(false);
                            app.set_status(&format!("Could not load GIFs: {error}"));
                        }
                    }
                },
            );
        } else {
            self.spawn(
                async move { http.gif_search(&query).await },
                move |app, result| {
                    // A slower, older search must not overwrite a newer one.
                    if GIF_QUERY.with_borrow(|current| *current != asked) {
                        return;
                    }
                    match result {
                        Ok(gifs) => app.show_gifs(gifs),
                        Err(error) => {
                            Self::set_gif_loading(false);
                            app.set_status(&format!("Could not load GIFs: {error}"));
                        }
                    }
                },
            );
        }
    }

    /// Shows or hides the GIF picker's loading spinner.
    fn set_gif_loading(loading: bool) {
        GIF_SPINNER.with_borrow(|slot| {
            if let Some(spinner) = slot {
                spinner.set_visible(loading);
                if loading {
                    spinner.start();
                } else {
                    spinner.stop();
                }
            }
        });
    }

    /// Shows a fresh set of results: buffers them and renders the first page.
    /// Subsequent pages render on scroll via `append_gif_page`.
    fn show_gifs(self: &Rc<Self>, gifs: Vec<kc_http::Gif>) {
        Self::set_gif_loading(false);
        let Some(flow) = GIF_SECTIONS.with_borrow(Clone::clone) else {
            return;
        };
        while let Some(child) = flow.first_child() {
            flow.remove(&child);
        }
        GIF_BUFFER.with_borrow_mut(|buffer| *buffer = gifs);
        GIF_RENDERED.with(|rendered| rendered.set(0));
        GIF_ANIM_BUDGET.with(|budget| budget.set(GIF_PICKER_ANIMATED));
        self.append_gif_page();
    }

    /// Renders the next page of the buffered results into the flow. A no-op once
    /// everything has been rendered. Called on first show and whenever the user
    /// scrolls to the bottom.
    fn append_gif_page(self: &Rc<Self>) {
        let Some(flow) = GIF_SECTIONS.with_borrow(Clone::clone) else {
            return;
        };
        let start = GIF_RENDERED.with(std::cell::Cell::get);
        let total = GIF_BUFFER.with_borrow(Vec::len);
        if start >= total {
            return;
        }
        let end = (start + GIF_PAGE).min(total);
        let page = GIF_BUFFER.with_borrow(|buffer| buffer[start..end].to_vec());
        for gif in &page {
            // Spend the shared animation budget so a wall of results does not
            // run dozens of frame timers; past it, tiles fall back to a still.
            let animate = GIF_ANIM_BUDGET.with(|budget| {
                let left = budget.get();
                if left > 0 {
                    budget.set(left - 1);
                    true
                } else {
                    false
                }
            });
            flow.insert(&self.gif_tile(gif, animate), -1);
        }
        GIF_RENDERED.with(|rendered| rendered.set(end));
    }

    /// Builds one picker tile. Trending/search give `src` as an animated `.gif`
    /// (the picker asks for `media_format=tinygif`); favourites saved on the
    /// official client give `src` as a video (`.mp4`/`.webm`) even for the
    /// "image" format, and carry no separate preview. So a `.gif` animates via
    /// the pixbuf player and anything else plays as a looping video (like a chat
    /// gifv). `animate` gates animation; past it a tile shows a still *if* it
    /// has an image one (video favourites have none, so they always play).
    fn gif_tile(self: &Rc<Self>, gif: &kc_http::Gif, animate: bool) -> gtk::Widget {
        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk::ContentFit::Cover);
        picture.set_size_request(180, 110);
        picture.add_css_class("kc-media");
        picture.set_overflow(gtk::Overflow::Hidden);

        let media = if gif.src.is_empty() {
            &gif.gif_src
        } else {
            &gif.src
        };
        let preview = if gif.preview.is_empty() {
            &gif.src
        } else {
            &gif.preview
        };
        // A still only helps if it is actually an image; a video URL (as
        // favourites carry) cannot be shown as a still, so such tiles must play.
        let has_still = !preview.is_empty() && !is_video_url(preview);
        if !media.is_empty() && (animate || !has_still) {
            if media.ends_with(".gif") {
                self.images.set_animated_picture(&picture, media);
            } else {
                self.play_gif(&picture, media);
            }
        } else if has_still {
            self.images.set_picture(&picture, preview);
        }

        let button = gtk::Button::new();
        button.add_css_class("flat");
        button.set_child(Some(&picture));
        let app = self.clone();
        let url = gif.url.clone();
        button.connect_clicked(move |_| {
            app.gif_button.popdown();
            app.send_text(url.clone());
        });

        // A star in the top-right corner, over the tile, toggles the favourite.
        // It sits in an overlay above the send button so it takes the click
        // itself (and does not also send the gif).
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&button));
        overlay.add_overlay(&self.gif_star(gif));
        overlay.upcast()
    }

    /// Fetches the favourite GIFs into the shared cache if not already loaded,
    /// then re-renders the open channel so the star on any chat GIF reflects
    /// the real favourited state. A no-op if the cache is already populated.
    pub(crate) fn prefetch_gif_favorites(self: &Rc<Self>) {
        if crate::gif_favorites::is_loaded() {
            return;
        }
        let Some((_, http, _)) = self.active() else {
            return;
        };
        crate::video_setup::spawn_local(
            self,
            async move { http.favorite_gifs().await.unwrap_or_default() },
            |app, favorites| {
                crate::gif_favorites::set_all(favorites);
                // Redraw so existing GIF messages pick up their star state.
                app.render_messages();
            },
        );
    }

    /// A star button (top-right overlay) that favourites or unfavourites the
    /// given GIF. It shows filled when the GIF is in the favourites cache,
    /// outline otherwise, and reveals on hover (styled by `kc-gif-star`).
    pub(crate) fn gif_star(self: &Rc<Self>, gif: &kc_http::Gif) -> gtk::Button {
        let favorited = crate::gif_favorites::is_favorited(&gif.url, &gif.src);
        let star = gtk::Button::new();
        star.add_css_class("flat");
        star.add_css_class("circular");
        star.add_css_class("osd");
        star.add_css_class("kc-gif-star");
        star.set_halign(gtk::Align::End);
        star.set_valign(gtk::Align::Start);
        star.set_margin_top(6);
        star.set_margin_end(6);
        crate::gif_favorites::set_star_icon(&star, favorited);

        let app = self.clone();
        let gif = gif.clone();
        star.connect_clicked(move |star| {
            app.toggle_gif_favorite(star, &gif);
        });
        star
    }

    /// Favourites or unfavourites `gif`, flipping the star icon optimistically,
    /// updating the shared cache, and syncing with Discord. On failure the icon
    /// is reverted and the status line explains.
    fn toggle_gif_favorite(self: &Rc<Self>, star: &gtk::Button, gif: &kc_http::Gif) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let now_favorite = !crate::gif_favorites::is_favorited(&gif.url, &gif.src);
        // Optimistic: flip the icon and the cache immediately.
        crate::gif_favorites::set_star_icon(star, now_favorite);
        if now_favorite {
            crate::gif_favorites::insert(gif.clone());
        } else {
            crate::gif_favorites::remove(&gif.url, &gif.src);
        }

        let url = gif.url.clone();
        let src = if gif.src.is_empty() {
            gif.url.clone()
        } else {
            gif.src.clone()
        };
        let format = if gif.video {
            kc_http::FAVORITE_GIF_FORMAT_VIDEO
        } else {
            kc_http::FAVORITE_GIF_FORMAT_IMAGE
        };
        let (width, height) = (gif.width, gif.height);
        let gif = gif.clone();
        let star = star.downgrade();
        crate::video_setup::spawn_local(
            self,
            async move {
                http.set_favorite_gif(&url, &src, format, width, height, now_favorite)
                    .await
            },
            move |app, result| {
                if let Err(error) = result {
                    // Revert the optimistic change.
                    if now_favorite {
                        crate::gif_favorites::remove(&gif.url, &gif.src);
                    } else {
                        crate::gif_favorites::insert(gif.clone());
                    }
                    if let Some(star) = star.upgrade() {
                        crate::gif_favorites::set_star_icon(&star, !now_favorite);
                    }
                    let verb = if now_favorite {
                        "favourite"
                    } else {
                        "unfavourite"
                    };
                    app.set_status(&format!("Could not {verb} the GIF: {error}"));
                }
            },
        );
    }

    /// Development aid: opens the GIF picker and fills it with made-up
    /// Favorites and Trending results (the demo REST points nowhere, so real
    /// favourites/trending cannot load). Returns the picker popover so the
    /// demo can save it.
    pub(crate) fn demo_gif_picker(self: &Rc<Self>) -> Option<gtk::Popover> {
        let url =
            "https://upload.wikimedia.org/wikipedia/commons/2/2c/Rotating_earth_%28large%29.gif";
        let make = |count: usize| -> Vec<kc_http::Gif> {
            (0..count)
                .map(|index| kc_http::Gif {
                    id: index.to_string(),
                    url: url.to_owned(),
                    src: url.to_owned(),
                    gif_src: url.to_owned(),
                    preview: url.to_owned(),
                    width: 400,
                    height: 400,
                    video: false,
                })
                .collect()
        };
        let popover = self.gif_button.popover();
        self.gif_button.popup();
        // `popup` runs the show handler, which clears the cache and kicks off a
        // (dead) favourites fetch. Seed the cache so that pending fetch shows
        // these made-up favourites instead of nothing, on the Favorited tab.
        GIF_QUERY.with_borrow_mut(|current| current.clear());
        GIF_TAB.with(|t| t.set(true));
        crate::gif_favorites::set_all(make(6));
        self.show_gifs(make(6));
        popover
    }

    /// Sends a message that did not come from the composer (a picked GIF).
    fn send_text(self: &Rc<Self>, content: String) {
        let Some(channel) = self
            .with_session(|session| session.selected_channel)
            .flatten()
        else {
            return;
        };
        let Some((_, http, _)) = self.active() else {
            return;
        };
        self.follow_sent_message(channel);
        self.spawn(
            async move { http.send_message(channel, &content, None).await },
            |app, result| {
                if let Err(error) = result {
                    app.set_status(&format!("Message not sent: {error}"));
                }
            },
        );
    }

    // -- autocomplete ----------------------------------------------------

    /// The `@`, `#` or `:` token that ends at the cursor, with its start
    /// offset in characters.
    fn completion_token(&self) -> Option<(char, String, i32)> {
        let buffer = self.composer.buffer();
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        // One character per position (emoji images are U+FFFC), so the
        // offset below is a buffer offset.
        let before = buffer
            .slice(&buffer.start_iter(), &cursor, true)
            .to_string();
        let start = before
            .char_indices()
            .rev()
            .take_while(|(_, ch)| !ch.is_whitespace())
            .last()
            .map(|(index, _)| index)?;
        let token = &before[start..];
        let trigger = token.chars().next()?;
        if !matches!(trigger, '@' | '#' | ':') {
            return None;
        }
        let query = &token[trigger.len_utf8()..];
        // `:` needs two characters so smileys do not open the list.
        if trigger == ':' && query.chars().count() < 2 {
            return None;
        }
        if query.contains(['@', '#', ':']) {
            return None;
        }
        let offset = before[..start].chars().count() as i32;
        Some((trigger, query.to_lowercase(), offset))
    }

    /// Shows suggestions again for what is typed now.
    pub(crate) fn refresh_completion(self: &Rc<Self>) {
        self.update_completion();
    }

    fn update_completion(self: &Rc<Self>) {
        // A command line has suggestions of its own.
        let buffer = self.composer.buffer();
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        let before = buffer.slice(&buffer.start_iter(), &cursor, true).to_string();
        if let Some((matches, hint)) = self.slash_completions(&before) {
            self.show_completions(matches, hint);
            return;
        }
        let Some((trigger, query, _)) = self.completion_token() else {
            self.completion.popdown();
            return;
        };
        let matches: Vec<Completion> = self
            .with_session(|session| {
                let channel_id = session.selected_channel?;
                let guild = session.selected_guild.filter(|_| !session.dm_mode);
                let nitro = session.user.as_ref().is_some_and(|user| user.has_nitro());
                Some(session.store.read(|state| {
                    match trigger {
                        '@' => {
                            let mut users: Vec<&kc_state::User> = match guild {
                                Some(guild) => state
                                    .guild_members
                                    .get(&guild)
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|id| state.users.get(id))
                                    .collect(),
                                None => state
                                    .channels
                                    .get(&channel_id)
                                    .and_then(|channel| channel.recipients.as_ref())
                                    .into_iter()
                                    .flatten()
                                    .collect(),
                            };
                            users.sort_by_key(|user| user.username.clone());
                            users
                                .into_iter()
                                .filter_map(|user| {
                                    let name = state.display_name(guild, user);
                                    let hit = name.to_lowercase().contains(&query)
                                        || user.username.to_lowercase().contains(&query);
                                    hit.then(|| Completion {
                                        display: format!("@{name}"),
                                        raw: format!("<@{}>", user.id),
                                        span: None,
                                        trail: true,
                                        detail: user.username.clone(),
                                        glyph: None,
                                        icon: Some(cdn::user_avatar(
                                            user.id,
                                            user.avatar.as_deref(),
                                            48,
                                        )),
                                        title: name,
                                    })
                                })
                                .take(COMPLETIONS)
                                .collect()
                        }
                        '#' => guild
                            .map(|guild| state.channel_tree(guild))
                            .unwrap_or_default()
                            .into_iter()
                            .flat_map(|section| section.channels)
                            .filter(|channel| {
                                matches!(
                                    channel.kind,
                                    ChannelType::GuildText
                                        | ChannelType::GuildAnnouncement
                                        | ChannelType::GuildForum
                                ) || channel.kind.is_thread()
                            })
                            .filter(|channel| {
                                channel.display_name().to_lowercase().contains(&query)
                            })
                            .take(COMPLETIONS)
                            .map(|channel| Completion {
                                title: format!("#{}", channel.display_name()),
                                detail: String::new(),
                                icon: None,
                                glyph: None,
                                display: format!("#{}", channel.display_name()),
                                raw: format!("<#{}>", channel.id),
                                span: None,
                                trail: true,
                            })
                            .collect(),
                        // Server emoji first, then standard ones by shortcode
                        // (those that start with the query before the rest).
                        _ => {
                            let mut found: Vec<Completion> = state
                                .sendable_emojis(guild, nitro)
                                .into_iter()
                                .flat_map(|(_, emojis)| emojis)
                                .filter(|emoji| emoji.name.to_lowercase().contains(&query))
                                .take(COMPLETIONS)
                                .map(|emoji| Completion {
                                    title: format!(":{}:", emoji.name),
                                    detail: String::new(),
                                    icon: Some(cdn::emoji(emoji.id, false, 48)),
                                    glyph: None,
                                    display: format!(":{}:", emoji.name),
                                    raw: emoji.mention(),
                                    span: None,
                                    trail: true,
                                })
                                .collect();
                            let room = COMPLETIONS.saturating_sub(found.len());
                            found.extend(standard_emoji_matches(&query, room));
                            found
                        }
                    }
                }))
            })
            .flatten()
            .unwrap_or_default();

        // Ask the gateway for members we have not seen; the list refreshes
        // when the chunk arrives and the user keeps typing.
        if trigger == '@'
            && query.chars().count() >= 2
            && matches.len() < COMPLETIONS
            && let Some(guild) = self
                .with_session(|session| session.selected_guild.filter(|_| !session.dm_mode))
                .flatten()
            && let Some((account, _, _)) = self.active()
            && let Some(gateway) = crate::session::gateway(account)
        {
            gateway.search_members(guild, &query, 10);
        }

        self.show_completions(matches, false);
    }

    /// Shows `matches` in the list above the composer (or hides it when
    /// there are none). A `hint` list leaves Enter to send.
    fn show_completions(&self, matches: Vec<Completion>, hint: bool) {
        COMPLETION_HINT.set(hint);
        if matches.is_empty() {
            self.completion.popdown();
            return;
        }
        clear_list(&self.completion_list);
        for completion in &matches {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.set_margin_top(2);
            row.set_margin_bottom(2);
            if let Some(glyph) = &completion.glyph {
                match crate::discord_emoji::image(glyph, 24) {
                    Some(image) => row.append(&image),
                    None => row.append(&label(glyph, &["kc-completion-glyph"])),
                }
            } else if let Some(icon) = &completion.icon {
                row.append(&self.images.avatar(24, &completion.title, Some(icon)));
            }
            row.append(&label(&completion.title, &[]));
            if !completion.detail.is_empty() {
                row.append(&label(&completion.detail, &["dim-label", "caption"]));
            }
            self.completion_list.append(&row);
        }
        if let Some(first) = self.completion_list.row_at_index(0) {
            self.completion_list.select_row(Some(&first));
        }
        COMPLETIONS_SHOWN.with_borrow_mut(|shown| *shown = matches);
        self.completion.popup();
    }

    fn move_completion(&self, step: i32) {
        let count = COMPLETIONS_SHOWN.with_borrow(Vec::len) as i32;
        if count == 0 {
            return;
        }
        let current = self
            .completion_list
            .selected_row()
            .map_or(0, |row| row.index());
        let next = (current + step).rem_euclid(count);
        if let Some(row) = self.completion_list.row_at_index(next) {
            self.completion_list.select_row(Some(&row));
        }
    }

    fn accept_completion(self: &Rc<Self>) {
        let index = self
            .completion_list
            .selected_row()
            .map_or(0, |row| row.index()) as usize;
        let chosen = COMPLETIONS_SHOWN
            .with_borrow(|shown| shown.get(index).map(|c| (c.display.clone(), c.raw.clone())));
        let span = COMPLETIONS_SHOWN
            .with_borrow(|shown| shown.get(index).map(|c| (c.span, c.trail)));
        let buffer = self.composer.buffer();
        // "Choose a File…" for a command's file option.
        if let (Some((_, raw)), Some((Some(span), _))) = (chosen.as_ref(), span)
            && let Some(option) = raw.strip_prefix(crate::slash::PICK_FILE)
        {
            self.completion.popdown();
            self.pick_command_file(option.to_owned(), span);
            return;
        }
        if let (Some((display, raw)), Some((Some((from, to)), trail))) = (chosen.clone(), span) {
            let mut start = buffer.iter_at_offset(from);
            let mut end = buffer.iter_at_offset(to);
            buffer.delete(&mut start, &mut end);
            buffer.insert(&mut start, &display);
            if trail {
                buffer.insert(&mut start, " ");
            }
            buffer.place_cursor(&start);
            if display != raw {
                remember_token(display, raw);
            }
            // What comes next (a subcommand, the options) may have
            // suggestions of its own.
            self.completion.popdown();
            return;
        }
        let (Some((display, raw)), Some((_, _, offset))) = (chosen, self.completion_token()) else {
            self.completion.popdown();
            return;
        };
        let mut start = buffer.iter_at_offset(offset);
        let mut end = buffer.iter_at_mark(&buffer.get_insert());
        buffer.delete(&mut start, &mut end);
        buffer.insert(&mut start, &format!("{display} "));
        if display != raw {
            remember_token(display, raw);
        }
        self.completion.popdown();
    }
}

thread_local! {
    static GIF_SECTIONS: std::cell::RefCell<Option<gtk::FlowBox>> = const { std::cell::RefCell::new(None) };
    static GIF_QUERY: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    /// Which tab is showing: true = Favorited, false = Search.
    static GIF_TAB: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    /// The full current result set; rendered into the flow a page at a time as
    /// the user scrolls to the bottom.
    static GIF_BUFFER: std::cell::RefCell<Vec<kc_http::Gif>> = const { std::cell::RefCell::new(Vec::new()) };
    /// How many tiles of `GIF_BUFFER` have been rendered so far.
    static GIF_RENDERED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Remaining animation budget across the whole current result (see
    /// `GIF_PICKER_ANIMATED`), decremented as pages render.
    static GIF_ANIM_BUDGET: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The picker's loading spinner, shown while a fetch is in flight.
    static GIF_SPINNER: std::cell::RefCell<Option<gtk::Spinner>> = const { std::cell::RefCell::new(None) };
}

/// Whether `url` points at a video the pixbuf still-loader cannot render (so a
/// tile must play it rather than fall back to a static preview).
pub(crate) fn is_video_url(url: &str) -> bool {
    let path = url.split('?').next().unwrap_or(url).to_ascii_lowercase();
    path.ends_with(".mp4")
        || path.ends_with(".webm")
        || path.ends_with(".mov")
        || path.ends_with(".gifv")
}

/// A fresh GIF-tile flow (two columns), configured as the picker wants it.
fn gif_flow() -> gtk::FlowBox {
    let flow = gtk::FlowBox::new();
    flow.set_selection_mode(gtk::SelectionMode::None);
    flow.set_max_children_per_line(2);
    flow.set_min_children_per_line(2);
    flow.set_column_spacing(6);
    flow.set_row_spacing(6);
    flow.set_valign(gtk::Align::Start);
    flow
}

/// A slowmode duration as Discord words it: "45s", "2m 5s", "1h 30m".
fn slowmode_text(left: Duration) -> String {
    let seconds = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 if seconds % 60 == 0 => format!("{}m", seconds / 60),
        60..3600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ if seconds % 3600 / 60 == 0 => format!("{}h", seconds / 3600),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// Standard emoji whose shortcode holds `query`, those starting with it
/// first, at most `limit`.
fn standard_emoji_matches(query: &str, limit: usize) -> Vec<Completion> {
    let mut hits: Vec<(bool, &str, &'static emojis::Emoji)> = emojis::iter()
        .filter(|emoji| emoji.skin_tone().is_none_or(|tone| tone == emojis::SkinTone::Default))
        .filter_map(|emoji| {
            let code = emoji.shortcodes().find(|code| code.contains(query))?;
            Some((!code.starts_with(query), code, emoji))
        })
        .collect();
    hits.sort_by_key(|(later, code, _)| (*later, code.len()));
    hits.into_iter()
        .take(limit)
        .map(|(_, code, emoji)| Completion {
            title: format!(":{code}:"),
            detail: String::new(),
            icon: None,
            glyph: Some(emoji.as_str().to_owned()),
            display: emoji.as_str().to_owned(),
            raw: emoji.as_str().to_owned(),
            span: None,
            trail: true,
        })
        .collect()
}

/// `:name:` shortcodes in `text` made emoji: standard ones become the
/// emoji, `custom(name)` gives others (server emoji) their `<:name:id>`.
/// Code (`inline` and fenced) is left alone, as Discord does.
fn expand_shortcodes(text: &str, custom: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        // Code spans pass through untouched.
        if let Some(after) = rest.strip_prefix("```") {
            let end = after.find("```").map_or(rest.len(), |end| end + 6);
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('`') {
            let end = after.find('`').map_or(rest.len(), |end| end + 2);
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        if let Some(after) = rest.strip_prefix(':') {
            let name_len = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+')))
                .unwrap_or(after.len());
            let name = &after[..name_len];
            if !name.is_empty() && after[name_len..].starts_with(':') {
                let found = emojis::get_by_shortcode(name)
                    .map(|emoji| emoji.as_str().to_owned())
                    .or_else(|| custom(name));
                if let Some(found) = found {
                    out.push_str(&found);
                    rest = &after[name_len + 1..];
                    continue;
                }
            }
        }
        let c = rest.chars().next().unwrap_or(' ');
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// The standard emoji for a `:name:` that has just been closed at the end
/// of `before` (the text before the cursor), with its length in characters.
/// Not inside inline code.
fn closed_shortcode(before: &str) -> Option<(&'static str, usize)> {
    let body = before.strip_suffix(':')?;
    let start = body.rfind(':')?;
    let name = &body[start + 1..];
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'))
        || before[..start].matches('`').count() % 2 == 1
    {
        return None;
    }
    let emoji = emojis::get_by_shortcode(name)?;
    Some((emoji.as_str(), name.chars().count() + 2))
}

/// What the autocomplete list shows (for the demo's checks).
pub(crate) fn completion_titles() -> Vec<String> {
    COMPLETIONS_SHOWN.with_borrow(|shown| shown.iter().map(|c| c.title.clone()).collect())
}

fn remember_token(display: String, raw: String) {
    TOKENS.with_borrow_mut(|tokens| {
        if !tokens.iter().any(|(known, _)| *known == display) {
            // Longer tokens first, so `@Ann` cannot clobber `@Anna`.
            tokens.push((display, raw));
            tokens.sort_by_key(|(display, _)| std::cmp::Reverse(display.len()));
        }
    });
}

impl AppState {
    /// Sets whether others see you typing, from the bar or from Settings.
    pub(crate) fn set_send_typing(self: &Rc<Self>, send: bool) {
        self.update_prefs(|prefs| prefs.send_typing = send);
        self.render_typing_toggle();
    }

    /// Shows the typing setting on the message bar's button.
    pub(crate) fn render_typing_toggle(&self) {
        let send = self.capture.borrow().send_typing;
        let (icon, tooltip) = if send {
            (
                "view-reveal-symbolic",
                "Others can see when you're typing. Click to type unseen.",
            )
        } else {
            (
                "view-conceal-symbolic",
                "You're typing unseen. Click to let others see when you're typing.",
            )
        };
        self.typing_toggle.set_icon_name(icon);
        self.typing_toggle.set_tooltip_text(Some(tooltip));
        if send {
            self.typing_toggle.remove_css_class("kc-typing-hidden");
        } else {
            self.typing_toggle.add_css_class("kc-typing-hidden");
        }
    }
}

/// The scrolled area under a picker's search entry.
fn picker_scroll(child: &gtk::Box) -> gtk::ScrolledWindow {
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_min_content_height(280);
    scroll.set_min_content_width(336);
    scroll.set_child(Some(child));
    scroll
}

/// Scrolls `scroll` so `heading` sits at the top of its viewport.
fn scroll_to_heading(scroll: &gtk::ScrolledWindow, heading: &gtk::Widget) {
    let Some(content) = scroll.child() else {
        return;
    };
    let Some(bounds) = heading.compute_bounds(&content) else {
        return;
    };
    let adjustment = scroll.vadjustment();
    let max = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
    adjustment.set_value(f64::from(bounds.y()).clamp(adjustment.lower(), max));
}

/// A representative emoji for each category, shown in the jump-to-section bar.
fn group_icon(group: emojis::Group) -> &'static str {
    match group {
        emojis::Group::SmileysAndEmotion => "😀",
        emojis::Group::PeopleAndBody => "🧑",
        emojis::Group::AnimalsAndNature => "🐻",
        emojis::Group::FoodAndDrink => "🍔",
        emojis::Group::TravelAndPlaces => "✈\u{fe0f}",
        emojis::Group::Activities => "⚽",
        emojis::Group::Objects => "💡",
        emojis::Group::Symbols => "❤\u{fe0f}",
        emojis::Group::Flags => "🏁",
    }
}

/// A Unicode emoji category's name, as Discord shows it.
fn group_title(group: emojis::Group) -> &'static str {
    match group {
        emojis::Group::SmileysAndEmotion => "Smileys & Emotion",
        emojis::Group::PeopleAndBody => "People & Body",
        emojis::Group::AnimalsAndNature => "Animals & Nature",
        emojis::Group::FoodAndDrink => "Food & Drink",
        emojis::Group::TravelAndPlaces => "Travel & Places",
        emojis::Group::Activities => "Activities",
        emojis::Group::Objects => "Objects",
        emojis::Group::Symbols => "Symbols",
        emojis::Group::Flags => "Flags",
    }
}

#[cfg(test)]
mod slowmode_tests {
    use super::*;

    #[test]
    fn slowmode_durations_read_like_discord() {
        assert_eq!(slowmode_text(Duration::from_millis(4_200)), "5s");
        assert_eq!(slowmode_text(Duration::from_secs(59)), "59s");
        assert_eq!(slowmode_text(Duration::from_secs(120)), "2m");
        assert_eq!(slowmode_text(Duration::from_secs(125)), "2m 5s");
        assert_eq!(slowmode_text(Duration::from_secs(3600)), "1h");
        assert_eq!(slowmode_text(Duration::from_secs(5400)), "1h 30m");
    }
}

/// The narrowest an attached file's card gets (room for its name).
const UPLOAD_CARD_MIN: i32 = 136;
/// How tall an attached picture's preview is.
const PREVIEW_HEIGHT: i32 = 160;
/// The widest an attached picture's preview gets (a wider picture is shown
/// whole, shorter).
const PREVIEW_MAX_WIDTH: i32 = 360;
/// The narrowest it gets (a taller picture is shown whole, narrower).
const PREVIEW_MIN_WIDTH: i32 = 40;
/// Its height: the preview, its name and size, and its padding (as
/// measured).
const UPLOAD_CARD_HEIGHT: i32 = 217;
/// The longest side of an attached picture's preview, in pixels.
const PREVIEW_SIZE: i32 = 320;

thread_local! {
    /// Attached pictures' previews, by file (the address of its bytes).
    static PREVIEWS: RefCell<HashMap<usize, gtk::gdk::Texture>> = RefCell::new(HashMap::new());
}

/// The size a picture of `width` x `height` is previewed at: whole, at its
/// own shape, [`PREVIEW_HEIGHT`] tall unless that would make it wider than
/// [`PREVIEW_MAX_WIDTH`].
fn preview_fit(width: i32, height: i32) -> (i32, i32) {
    if width <= 0 || height <= 0 {
        return (PREVIEW_HEIGHT, PREVIEW_HEIGHT);
    }
    let (width, height) = (f64::from(width), f64::from(height));
    let preview = f64::from(PREVIEW_HEIGHT);
    let across = width * preview / height;
    if across > f64::from(PREVIEW_MAX_WIDTH) {
        let max = f64::from(PREVIEW_MAX_WIDTH);
        return (PREVIEW_MAX_WIDTH, (height * max / width).round().max(1.0) as i32);
    }
    (across.round().max(f64::from(PREVIEW_MIN_WIDTH)) as i32, PREVIEW_HEIGHT)
}

fn preview_key(upload: &Upload) -> usize {
    Arc::as_ptr(&upload.bytes) as usize
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UploadKind {
    Picture,
    Video,
    Audio,
    Other,
}

/// What an attached file is, by its name (or its first bytes).
fn upload_kind(name: &str, bytes: &[u8]) -> UploadKind {
    let extension = name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "avif" | "tif" | "tiff" => UploadKind::Picture,
        "mp4" | "webm" | "mov" | "mkv" | "avi" | "m4v" => UploadKind::Video,
        "mp3" | "ogg" | "opus" | "wav" | "flac" | "m4a" | "aac" => UploadKind::Audio,
        _ if bytes.starts_with(&[0x89, b'P', b'N', b'G']) || bytes.starts_with(&[0xff, 0xd8, 0xff]) => {
            UploadKind::Picture
        }
        _ => UploadKind::Other,
    }
}

/// A picture's preview, at most [`PREVIEW_SIZE`] on its longer side (a
/// full-size photo would hold its whole size in memory just to be shown
/// small). Blocking: run away from the main thread.
fn upload_preview(bytes: &[u8]) -> Option<gtk::gdk::Texture> {
    use gtk::gdk_pixbuf::Pixbuf;
    let stream = gtk::gio::MemoryInputStream::from_bytes(&glib::Bytes::from(bytes));
    let Ok(pixbuf) = Pixbuf::from_stream_at_scale(
        &stream,
        PREVIEW_SIZE,
        PREVIEW_SIZE,
        true,
        gtk::gio::Cancellable::NONE,
    ) else {
        // WebP and others GdkPixbuf cannot read.
        return crate::images::decode_blocking(bytes.to_vec());
    };
    let format = if pixbuf.has_alpha() {
        gtk::gdk::MemoryFormat::R8g8b8a8
    } else {
        gtk::gdk::MemoryFormat::R8g8b8
    };
    Some(
        gtk::gdk::MemoryTexture::new(
            pixbuf.width(),
            pixbuf.height(),
            format,
            &pixbuf.read_pixel_bytes(),
            pixbuf.rowstride() as usize,
        )
        .upcast(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_keep_the_whole_picture_at_its_shape() {
        assert_eq!(preview_fit(1200, 400), (PREVIEW_MAX_WIDTH, 120));
        assert_eq!(preview_fit(300, 900), (53, PREVIEW_HEIGHT));
        assert_eq!(preview_fit(400, 400), (PREVIEW_HEIGHT, PREVIEW_HEIGHT));
        assert_eq!(preview_fit(10, 4000), (PREVIEW_MIN_WIDTH, PREVIEW_HEIGHT));
        assert_eq!(preview_fit(0, 0), (PREVIEW_HEIGHT, PREVIEW_HEIGHT));
    }

    fn snap(text: &str) -> Snapshot {
        Snapshot {
            text: text.to_owned(),
            cursor: text.chars().count(),
        }
    }

    #[test]
    fn shortcodes_become_emoji_outside_code() {
        let custom = |name: &str| (name == "kubecord").then(|| "<:kubecord:42>".to_owned());
        assert_eq!(
            expand_shortcodes("hi :smile: and :kubecord: :nope: 12:30:45", &custom),
            "hi 😄 and <:kubecord:42> :nope: 12:30:45"
        );
        assert_eq!(expand_shortcodes("`:smile:` ```\n:smile:\n```", &custom), "`:smile:` ```\n:smile:\n```");
        assert_eq!(expand_shortcodes(":+1: :thumbsup:", &custom), "👍 👍");
    }

    #[test]
    fn a_closing_colon_finishes_a_shortcode() {
        assert_eq!(closed_shortcode("well :smile:"), Some(("😄", 7)));
        assert_eq!(closed_shortcode("well :nope:"), None);
        assert_eq!(closed_shortcode("`code :smile:"), None);
        assert_eq!(closed_shortcode("time 12:"), None);
    }

    #[test]
    fn standard_emoji_complete_by_shortcode() {
        let found = standard_emoji_matches("smile", 5);
        assert_eq!(found.first().map(|c| c.title.as_str()), Some(":smile:"));
        assert!(found.iter().all(|c| c.glyph.is_some() && c.display == c.raw));
    }

    #[test]
    fn typing_undoes_a_word_at_a_time() {
        let start = Instant::now();
        let mut history = History::default();
        let mut text = String::new();
        for (index, c) in "hi there".chars().enumerate() {
            text.push(c);
            history.record(snap(&text), start + Duration::from_millis(100 * index as u64));
        }
        assert_eq!(history.undo().map(|step| step.text), Some("hi ".to_owned()));
        assert_eq!(history.undo().map(|step| step.text), Some(String::new()));
        assert_eq!(history.undo(), None);
        assert_eq!(history.redo().map(|step| step.text), Some("hi ".to_owned()));
        assert_eq!(history.redo().map(|step| step.text), Some("hi there".to_owned()));
        assert_eq!(history.redo(), None);
    }

    #[test]
    fn emoji_drawn_as_images_are_not_undo_steps() {
        let start = Instant::now();
        let mut history = History::default();
        history.record(snap("go 🚀"), start);
        // The same text (its emoji now an image) is no new step.
        history.record(snap("go 🚀"), start + Duration::from_millis(10));
        assert_eq!(history.undo().map(|step| step.text), Some(String::new()));
        assert_eq!(history.undo(), None);
    }

    #[test]
    fn a_pause_or_deleting_starts_a_new_step() {
        let start = Instant::now();
        let mut history = History::default();
        history.record(snap("a"), start);
        history.record(snap("ab"), start + Duration::from_secs(3));
        history.record(snap("a"), start + Duration::from_millis(3100));
        assert_eq!(history.undo().map(|step| step.text), Some("ab".to_owned()));
        assert_eq!(history.undo().map(|step| step.text), Some("a".to_owned()));
        assert_eq!(history.undo().map(|step| step.text), Some(String::new()));
    }

    #[test]
    fn a_new_edit_drops_the_redo_steps() {
        let start = Instant::now();
        let mut history = History::default();
        history.record(snap("one"), start);
        history.undo();
        history.record(snap("two"), start + Duration::from_secs(2));
        assert_eq!(history.redo(), None);
    }
}
