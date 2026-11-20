//! The message composer: sending, replies, edits, uploads, typing
//! notifications, the emoji and GIF pickers, and autocomplete.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gtk::prelude::*;

use kc_core::Snowflake;
use kc_http::{Outgoing, cdn};
use kc_state::ChannelType;

use crate::app::TYPING_THROTTLE;
use crate::session::{Command, Upload};
use crate::shell::AppState;
use crate::widgets::{clear_box, clear_list, icon_button, label, snippet};

const MIB: usize = 1024 * 1024;
/// Rows offered by autocomplete.
const COMPLETIONS: usize = 8;
/// Custom emoji drawn in the picker before the search has to narrow it.
const PICKER_EMOJI: usize = 400;
/// How many GIF-picker tiles animate at once. The rest show a still preview,
/// so a wall of results does not run dozens of frame timers.
const GIF_PICKER_ANIMATED: usize = 16;

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
struct Completion {
    /// Shown in the list.
    title: String,
    detail: String,
    icon: Option<String>,
    /// Text placed in the composer.
    display: String,
    /// What `display` becomes when the message is sent.
    raw: String,
}

thread_local! {
    /// `display -> raw` pairs for mentions and emoji inserted through
    /// autocomplete or the picker, applied when the message is sent.
    static TOKENS: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
    static COMPLETIONS_SHOWN: std::cell::RefCell<Vec<Completion>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl AppState {
    pub(crate) fn connect_composer(self: &Rc<Self>) {
        let app = self.clone();
        self.composer.buffer().connect_changed(move |_| {
            app.on_composer_changed();
        });

        let keys = gtk::EventControllerKey::new();
        let app = self.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            use gtk::gdk::Key;
            let completing = app.completion.is_visible();
            match key {
                Key::Return | Key::KP_Enter => {
                    if modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
                        return glib::Propagation::Proceed;
                    }
                    if completing {
                        app.accept_completion();
                    } else {
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
        self.completion_list.connect_row_activated(move |_, _| {
            app.accept_completion();
        });

        self.build_emoji_picker();
        self.build_gif_picker();
    }

    pub(crate) fn composer_text(&self) -> String {
        let buffer = self.composer.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }

    pub(crate) fn insert_composer_text(&self, text: &str) {
        self.composer.buffer().insert_at_cursor(text);
        self.composer.grab_focus();
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
                Some((
                    format!("Message {prefix}{title}"),
                    state.can_send(channel_id),
                    context,
                ))
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
    }

    fn on_composer_changed(self: &Rc<Self>) {
        let text = self.composer_text();
        self.composer_hint.set_visible(text.is_empty());
        self.update_completion();
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

    pub(crate) fn send_current(self: &Rc<Self>) {
        let typed = self.composer_text();
        let mut content = typed.trim().to_owned();
        // Swap display tokens (`@Name`, `:emoji:`) for what Discord expects.
        TOKENS.with_borrow(|tokens| {
            for (display, raw) in tokens {
                content = content.replace(display, raw);
            }
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
        if content.is_empty() && uploads.is_empty() {
            return;
        }
        if content.chars().count() > 2000 {
            self.set_status("Messages are limited to 2000 characters");
            return;
        }

        let mention_reply = self.context_ping.is_active();
        self.composer.buffer().set_text("");
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
                Err(error) => match &nonce {
                    // The row stays, now marked failed with Retry / Delete.
                    Some(nonce) => {
                        if let Some((_, _, store)) = app.active() {
                            store.set_send_state(channel, nonce, kc_state::SendState::Failed);
                        }
                        app.set_status(&format!("Message not sent: {error}"));
                    }
                    // No optimistic row (an attachments-only or
                    // jumped-to-present send); just report it.
                    None => app.set_status(&format!("Message not sent: {error}")),
                },
            },
        );
    }

    /// Re-sends a message whose send failed, dropping the failed row first.
    pub(crate) fn retry_send(self: &Rc<Self>, channel: Snowflake, nonce: &str) {
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

    /// Puts unsent text back unless the user has already typed something new.
    fn restore_draft(&self, text: &str) {
        if self.composer_text().is_empty() {
            self.composer.buffer().set_text(text);
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
            self.composer.buffer().set_text(&content);
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
            self.composer.buffer().set_text("");
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

    fn add_upload(self: &Rc<Self>, name: String, bytes: Vec<u8>) {
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
        for (index, upload) in uploads.iter().enumerate() {
            let chip = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            chip.add_css_class("card");
            let name = label(&snippet(&upload.name, 28), &["caption"]);
            name.set_margin_start(8);
            name.set_tooltip_text(Some(&format!(
                "{} ({})",
                upload.name,
                crate::markdown::format_size(upload.bytes.len() as u64)
            )));
            chip.append(&name);
            let remove = icon_button("window-close-symbolic", "Remove");
            let app = self.clone();
            remove.connect_clicked(move |_| {
                app.with_session_mut(|session| {
                    if index < session.uploads.len() {
                        session.uploads.remove(index);
                    }
                });
                app.render_upload_tray();
            });
            chip.append(&remove);
            self.upload_tray.append(&chip);
        }
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
        let built = Rc::new(std::cell::Cell::new(false));
        let ensure_standard = {
            let app = self.clone();
            let standard = standard.clone();
            let stack = stack.clone();
            move || {
                if stack.visible_child_name().as_deref() == Some("standard") && !built.replace(true)
                {
                    app.fill_standard_emoji(&standard);
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
            if in_dm {
                stack.set_visible_child_name("standard");
            } else {
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
    fn fill_standard_emoji(self: &Rc<Self>, page: &gtk::Box) {
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search emoji"));
        page.append(&search);
        let groups = gtk::Box::new(gtk::Orientation::Vertical, 6);
        page.append(&picker_scroll(&groups));

        let query = Rc::new(std::cell::RefCell::new(String::new()));
        let mut parts: Vec<(gtk::Label, gtk::FlowBox, Rc<Vec<String>>)> = Vec::new();
        for group in emojis::Group::iter() {
            let heading = label(&group_title(group).to_uppercase(), &["kc-category"]);
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
                let button = gtk::Button::with_label(emoji.as_str());
                button.add_css_class("flat");
                button.add_css_class("kc-picker-emoji");
                button.add_css_class("kc-standard-emoji");
                let code = emoji.shortcode().unwrap_or(emoji.name());
                button.set_tooltip_text(Some(&format!(":{code}:")));
                let text = emoji.as_str();
                let app = self.clone();
                button.connect_clicked(move |_| app.insert_composer_text(text));
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

        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search Tenor"));
        content.append(&search);

        let flow = gtk::FlowBox::new();
        flow.set_selection_mode(gtk::SelectionMode::None);
        flow.set_max_children_per_line(2);
        flow.set_min_children_per_line(2);
        flow.set_column_spacing(6);
        flow.set_row_spacing(6);
        flow.set_valign(gtk::Align::Start);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_min_content_height(360);
        scroll.set_min_content_width(380);
        scroll.set_child(Some(&flow));
        content.append(&scroll);
        popover.set_child(Some(&content));

        let app = self.clone();
        let entry = search.clone();
        popover.connect_show(move |_| {
            entry.set_text("");
            app.load_gifs(String::new());
        });
        let app = self.clone();
        search.connect_search_changed(move |entry| {
            app.load_gifs(entry.text().trim().to_owned());
        });
        self.gif_button.set_popover(Some(&popover));
        GIF_FLOW.with_borrow_mut(|slot| *slot = Some(flow));
    }

    fn load_gifs(self: &Rc<Self>, query: String) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let asked = query.clone();
        GIF_QUERY.with_borrow_mut(|current| *current = query.clone());
        self.spawn(
            async move {
                if query.is_empty() {
                    http.gif_trending().await
                } else {
                    http.gif_search(&query).await
                }
            },
            move |app, result| {
                // A slower, older search must not overwrite a newer one.
                if GIF_QUERY.with_borrow(|current| *current != asked) {
                    return;
                }
                match result {
                    Ok(gifs) => app.show_gifs(gifs),
                    Err(error) => app.set_status(&format!("Could not load GIFs: {error}")),
                }
            },
        );
    }

    fn show_gifs(self: &Rc<Self>, gifs: Vec<kc_http::Gif>) {
        let Some(flow) = GIF_FLOW.with_borrow(Clone::clone) else {
            return;
        };
        while let Some(child) = flow.first_child() {
            flow.remove(&child);
        }
        for (index, gif) in gifs.into_iter().take(40).enumerate() {
            let picture = gtk::Picture::new();
            picture.set_can_shrink(true);
            picture.set_content_fit(gtk::ContentFit::Cover);
            picture.set_size_request(180, 110);
            picture.add_css_class("kc-media");
            picture.set_overflow(gtk::Overflow::Hidden);
            // Animate the tile from the GIF itself (the picker asks for
            // `media_format=tinygif`, so `src`/`gif_src` are animated GIFs).
            // Cap the number of animated tiles: a wall of gifs is a lot of
            // timers, so the rest fall back to their still preview. (The
            // animated ones still pause when the popover closes, via the
            // unmap handling in `set_animated_picture`.)
            let animate = index < GIF_PICKER_ANIMATED;
            let animated = if gif.gif_src.is_empty() {
                &gif.src
            } else {
                &gif.gif_src
            };
            let preview = if gif.preview.is_empty() {
                &gif.src
            } else {
                &gif.preview
            };
            if animate && !animated.is_empty() {
                self.images.set_animated_picture(&picture, animated);
            } else {
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
            flow.insert(&button, -1);
        }
    }

    /// Development aid: opens the GIF picker and fills it with made-up
    /// animated results (the demo REST points nowhere, so trending/search
    /// cannot load). Returns the picker popover so the demo can save it.
    #[cfg(debug_assertions)]
    pub(crate) fn demo_gif_picker(self: &Rc<Self>) -> Option<gtk::Popover> {
        let url =
            "https://upload.wikimedia.org/wikipedia/commons/2/2c/Rotating_earth_%28large%29.gif";
        let gifs = (0..6)
            .map(|index| kc_http::Gif {
                id: index.to_string(),
                url: url.to_owned(),
                src: url.to_owned(),
                gif_src: url.to_owned(),
                preview: url.to_owned(),
                width: 400,
                height: 400,
            })
            .collect();
        let popover = self.gif_button.popover();
        self.gif_button.popup();
        self.show_gifs(gifs);
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
        let before = buffer
            .text(&buffer.start_iter(), &cursor, false)
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

    fn update_completion(self: &Rc<Self>) {
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
                                        detail: user.username.clone(),
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
                                display: format!("#{}", channel.display_name()),
                                raw: format!("<#{}>", channel.id),
                            })
                            .collect(),
                        _ => state
                            .sendable_emojis(guild, nitro)
                            .into_iter()
                            .flat_map(|(_, emojis)| emojis)
                            .filter(|emoji| emoji.name.to_lowercase().contains(&query))
                            .take(COMPLETIONS)
                            .map(|emoji| Completion {
                                title: format!(":{}:", emoji.name),
                                detail: String::new(),
                                icon: Some(cdn::emoji(emoji.id, false, 48)),
                                display: format!(":{}:", emoji.name),
                                raw: emoji.mention(),
                            })
                            .collect(),
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

        if matches.is_empty() {
            self.completion.popdown();
            return;
        }
        clear_list(&self.completion_list);
        for completion in &matches {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.set_margin_top(2);
            row.set_margin_bottom(2);
            if let Some(icon) = &completion.icon {
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
        let (Some((display, raw)), Some((_, _, offset))) = (chosen, self.completion_token()) else {
            self.completion.popdown();
            return;
        };
        let buffer = self.composer.buffer();
        let mut start = buffer.iter_at_offset(offset);
        let mut end = buffer.iter_at_mark(&buffer.get_insert());
        buffer.delete(&mut start, &mut end);
        buffer.insert(&mut start, &format!("{display} "));
        remember_token(display, raw);
        self.completion.popdown();
    }
}

thread_local! {
    static GIF_FLOW: std::cell::RefCell<Option<gtk::FlowBox>> = const { std::cell::RefCell::new(None) };
    static GIF_QUERY: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
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
