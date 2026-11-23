//! Message requests: DMs from people you haven't accepted. They stay out of
//! the DM list, under "Message Requests" (with Discord's spam guesses on a
//! tab of their own), and a request's DM offers Accept and Ignore in place
//! of the message box.
//!
//! Accept is `PUT /channels/{id}/recipients/@me` with `consent_status: 2`,
//! Ignore `DELETE` on the same path; the first message of each comes from
//! `/users/@me/message-requests/supplemental-data`. The channel updates
//! that follow arrive on the gateway.

use std::collections::HashMap;
use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::Snowflake;
use kc_http::cdn;
use kc_state::Message;

use crate::shell::AppState;
use crate::video_setup::spawn_local;
use crate::widgets::{clear_box, empty_state, label};

/// One request, as listed.
#[derive(Clone)]
struct Request {
    channel_id: Snowflake,
    title: String,
    avatar: Option<String>,
}

impl AppState {
    /// How many requests there are (not spam, spam).
    pub(crate) fn message_request_counts(&self) -> (usize, usize) {
        self.with_session(|session| {
            session
                .store
                .read(|state| (state.message_requests(false).len(), state.message_requests(true).len()))
        })
        .unwrap_or((0, 0))
    }

    fn requests(&self, spam: bool) -> Vec<Request> {
        self.with_session(|session| {
            session.store.read(|state| {
                state
                    .message_requests(spam)
                    .into_iter()
                    .map(|channel| Request {
                        channel_id: channel.id,
                        title: state.channel_title(channel),
                        avatar: channel
                            .dm_recipient()
                            .map(|user| cdn::user_avatar(user.id, user.avatar.as_deref(), 64)),
                    })
                    .collect()
            })
        })
        .unwrap_or_default()
    }

    /// The Message Requests dialog.
    pub(crate) fn show_message_requests(self: &Rc<Self>) {
        let dialog = adw::Dialog::new();
        dialog.set_title("Message Requests");
        dialog.set_content_width(520);
        dialog.set_content_height(600);
        let stack = adw::ViewStack::new();
        let requests = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let spam = gtk::Box::new(gtk::Orientation::Vertical, 8);
        for (page, name, title, icon) in [
            (&requests, "requests", "Requests", "mail-unread-symbolic"),
            (&spam, "spam", "Spam", "dialog-warning-symbolic"),
        ] {
            page.set_margin_top(12);
            page.set_margin_bottom(12);
            page.set_margin_start(12);
            page.set_margin_end(12);
            let scroll = gtk::ScrolledWindow::new();
            scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
            scroll.set_child(Some(page));
            stack.add_titled_with_icon(&scroll, Some(name), title, icon);
        }
        let switcher = adw::ViewSwitcher::new();
        switcher.set_stack(Some(&stack));
        switcher.set_policy(adw::ViewSwitcherPolicy::Wide);
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&switcher));
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&stack));
        dialog.set_child(Some(&view));

        let fill = {
            let (app, requests, spam, dialog) = (self.clone(), requests.clone(), spam.clone(), dialog.clone());
            Rc::new(move |previews: &HashMap<Snowflake, Message>| {
                app.fill_requests(&requests, false, previews, &dialog);
                app.fill_requests(&spam, true, previews, &dialog);
            })
        };
        fill(&HashMap::new());

        // The first message of each, to see what it is before accepting.
        let ids: Vec<Snowflake> = self
            .requests(false)
            .into_iter()
            .chain(self.requests(true))
            .map(|request| request.channel_id)
            .collect();
        if !ids.is_empty()
            && !crate::demo::fake_rest()
            && let Some((_, http, _)) = self.active()
        {
            let fill = fill.clone();
            spawn_local(self, async move { http.message_request_previews(&ids).await }, move |_, result| {
                if let Ok(previews) = result {
                    fill(&previews.into_iter().collect());
                }
            });
        }
        dialog.present(Some(&self.window));
    }

    fn fill_requests(self: &Rc<Self>, page: &gtk::Box, spam: bool, previews: &HashMap<Snowflake, Message>, dialog: &adw::Dialog) {
        clear_box(page);
        let requests = self.requests(spam);
        if requests.is_empty() {
            page.append(&empty_state(
                if spam { "dialog-warning-symbolic" } else { "mail-unread-symbolic" },
                if spam { "No Spam" } else { "No Message Requests" },
                if spam {
                    "Requests Discord thinks are spam land here."
                } else {
                    "DMs from people you haven't accepted yet land here."
                },
            ));
            return;
        }
        let note = label(
            if spam {
                "Discord thinks these are spam. Be careful with links and files."
            } else {
                "They can't see whether you've read their message until you accept."
            },
            &["dim-label", "caption"],
        );
        note.set_xalign(0.0);
        note.set_wrap(true);
        page.append(&note);
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        for request in requests {
            let row = adw::ActionRow::new();
            row.set_title(&glib::markup_escape_text(&request.title));
            if let Some(message) = previews.get(&request.channel_id) {
                let preview = if message.content.is_empty() { "Sent an attachment".to_owned() } else { message.content.clone() };
                row.set_subtitle(&glib::markup_escape_text(&preview));
                row.set_subtitle_lines(2);
            }
            row.add_prefix(&self.images.avatar(36, &request.title, request.avatar.as_deref()));
            row.set_activatable(true);
            row.set_tooltip_text(Some("Open the conversation"));
            let (app, target, id) = (self.clone(), dialog.clone(), request.channel_id);
            row.connect_activated(move |_| {
                target.close();
                app.open_request(id);
            });
            let ignore = gtk::Button::with_label("Ignore");
            ignore.set_valign(gtk::Align::Center);
            let (app, id) = (self.clone(), request.channel_id);
            ignore.connect_clicked(move |button| {
                button.set_sensitive(false);
                app.answer_request(id, false);
            });
            row.add_suffix(&ignore);
            let accept = gtk::Button::with_label("Accept");
            accept.add_css_class("suggested-action");
            accept.set_valign(gtk::Align::Center);
            let (app, id) = (self.clone(), request.channel_id);
            accept.connect_clicked(move |button| {
                button.set_sensitive(false);
                app.answer_request(id, true);
            });
            row.add_suffix(&accept);
            list.append(&row);
        }
        page.append(&list);
    }

    /// Demos: opens a request's DM.
    pub(crate) fn open_request_demo(self: &Rc<Self>, channel_id: Snowflake) {
        self.open_request(channel_id);
    }

    fn open_request(self: &Rc<Self>, channel_id: Snowflake) {
        let channel = self
            .with_session(|session| session.store.read(|state| state.channels.get(&channel_id).cloned()))
            .flatten();
        if let Some(channel) = channel {
            self.open_channel(channel);
        }
    }

    /// Accepts or ignores a request.
    pub(crate) fn answer_request(self: &Rc<Self>, channel_id: Snowflake, accept: bool) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        spawn_local(
            self,
            async move {
                if accept {
                    http.accept_message_request(channel_id).await
                } else {
                    http.reject_message_request(channel_id).await
                }
            },
            move |app, result| match result {
                Ok(()) => app.set_status(if accept { "Request accepted" } else { "Request ignored" }),
                Err(error) => app.set_status(&format!(
                    "Could not {} the request: {error}",
                    if accept { "accept" } else { "ignore" }
                )),
            },
        );
    }

    /// In a request's DM: who it's from, with Accept and Ignore, in place
    /// of the message box. Returns whether it is one.
    pub(crate) fn render_request_bar(self: &Rc<Self>) -> bool {
        let request = self
            .with_session(|session| {
                let channel = session.selected_channel?;
                session.store.read(|state| {
                    let channel = state.channels.get(&channel)?;
                    channel.is_request().then(|| (channel.id, state.channel_title(channel), channel.is_spam == Some(true)))
                })
            })
            .flatten();
        let Some((channel_id, title, spam)) = request else {
            self.request_bar.set_visible(false);
            return false;
        };
        clear_box(&self.request_bar);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        let heading = label(&format!("{title} wants to message you"), &["heading"]);
        heading.set_xalign(0.0);
        heading.set_wrap(true);
        text.append(&heading);
        let note = label(
            if spam {
                "Discord thinks this is spam. Accept only if you know them."
            } else {
                "Accept to reply. Ignoring removes the conversation; they won't be told."
            },
            &["caption", "dim-label"],
        );
        note.set_xalign(0.0);
        note.set_wrap(true);
        text.append(&note);
        self.request_bar.append(&text);
        let ignore = gtk::Button::with_label("Ignore");
        ignore.set_valign(gtk::Align::Center);
        let app = self.clone();
        ignore.connect_clicked(move |button| {
            button.set_sensitive(false);
            app.answer_request(channel_id, false);
        });
        self.request_bar.append(&ignore);
        let accept = gtk::Button::with_label("Accept");
        accept.add_css_class("suggested-action");
        accept.set_valign(gtk::Align::Center);
        let app = self.clone();
        accept.connect_clicked(move |button| {
            button.set_sensitive(false);
            app.answer_request(channel_id, true);
        });
        self.request_bar.append(&accept);
        self.request_bar.set_visible(true);
        true
    }
}
