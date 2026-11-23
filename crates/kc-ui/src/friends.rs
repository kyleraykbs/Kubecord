//! The friends page shown on the home view.

use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::Snowflake;
use kc_http::cdn;
use kc_state::Relationship;

use crate::session::FriendsTab;
use crate::shell::AppState;
use crate::widgets::{
    avatar_with_status, clear_box, empty_state, icon_button, label, status_label,
};

struct FriendRow {
    id: Snowflake,
    name: String,
    username: String,
    avatar: String,
    status: String,
    summary: Option<String>,
    kind: u8,
}

impl AppState {
    pub(crate) fn render_friends(self: &Rc<Self>) {
        clear_box(&self.friends_box);
        let Some(tab) = self.with_session(|session| session.friends_tab) else {
            return;
        };

        let rows: Vec<FriendRow> = self
            .with_session(|session| {
                session.store.read(|state| {
                    let kinds: &[u8] = match tab {
                        FriendsTab::Online | FriendsTab::All => &[Relationship::FRIEND],
                        FriendsTab::Pending => &[
                            Relationship::INCOMING_REQUEST,
                            Relationship::OUTGOING_REQUEST,
                        ],
                        FriendsTab::Blocked => &[Relationship::BLOCKED],
                    };
                    kinds
                        .iter()
                        .flat_map(|kind| state.relationships_of(*kind))
                        .map(|(relationship, user)| FriendRow {
                            id: user.id,
                            name: state.display_name(None, user),
                            username: user.username.clone(),
                            avatar: cdn::user_avatar(user.id, user.avatar.as_deref(), 80),
                            status: state.status_of(user.id).to_owned(),
                            summary: state
                                .presence(user.id)
                                .and_then(|presence| presence.summary()),
                            kind: relationship.kind,
                        })
                        .filter(|row| tab != FriendsTab::Online || row.status != "offline")
                        .collect()
                })
            })
            .unwrap_or_default();

        // Tabs.
        let tabs = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        tabs.add_css_class("linked");
        tabs.set_halign(gtk::Align::Start);
        let mut group: Option<gtk::ToggleButton> = None;
        for (value, text) in [
            (FriendsTab::Online, "Online"),
            (FriendsTab::All, "All"),
            (FriendsTab::Pending, "Pending"),
            (FriendsTab::Blocked, "Blocked"),
        ] {
            let button = gtk::ToggleButton::with_label(text);
            button.set_active(tab == value);
            match &group {
                Some(first) => button.set_group(Some(first)),
                None => group = Some(button.clone()),
            }
            let app = self.clone();
            button.connect_toggled(move |button| {
                if button.is_active() {
                    app.with_session_mut(|session| session.friends_tab = value);
                    app.render_friends();
                }
            });
            tabs.append(&button);
        }

        // Add friend.
        let add_entry = gtk::Entry::new();
        add_entry.set_placeholder_text(Some("Add a friend by username"));
        add_entry.set_hexpand(true);
        let add_button = gtk::Button::with_label("Send Request");
        add_button.add_css_class("suggested-action");
        let submit = {
            let app = self.clone();
            let entry = add_entry.clone();
            move || {
                let username = entry.text().trim().trim_start_matches('@').to_owned();
                if username.is_empty() {
                    return;
                }
                entry.set_text("");
                app.send_friend_request(username);
            }
        };
        let on_activate = submit.clone();
        add_entry.connect_activate(move |_| on_activate());
        add_button.connect_clicked(move |_| submit());
        let add_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        add_row.append(&add_entry);
        add_row.append(&add_button);

        let top = gtk::Box::new(gtk::Orientation::Vertical, 12);
        top.set_margin_top(16);
        top.set_margin_start(16);
        top.set_margin_end(16);
        top.set_margin_bottom(12);
        top.append(&tabs);
        top.append(&add_row);
        self.friends_box.append(&top);

        if rows.is_empty() {
            let (title, description) = match tab {
                FriendsTab::Online => ("No One Is Online", "Friends who are online appear here."),
                FriendsTab::All => ("No Friends Yet", "Send a request with the box above."),
                FriendsTab::Pending => ("No Pending Requests", ""),
                FriendsTab::Blocked => ("No Blocked Users", ""),
            };
            self.friends_box
                .append(&empty_state("system-users-symbolic", title, description));
            return;
        }

        let heading = match tab {
            FriendsTab::Online => "ONLINE",
            FriendsTab::All => "ALL FRIENDS",
            FriendsTab::Pending => "PENDING",
            FriendsTab::Blocked => "BLOCKED",
        };
        let count = label(&format!("{heading} — {}", rows.len()), &["kc-category"]);
        count.set_margin_start(20);
        count.set_margin_bottom(6);
        self.friends_box.append(&count);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("boxed-list");
        list.set_margin_start(16);
        list.set_margin_end(16);
        list.set_margin_bottom(16);
        for row in rows {
            list.append(&self.friend_row(&row));
        }
        self.friends_box.append(&list);
    }

    fn friend_row(self: &Rc<Self>, friend: &FriendRow) -> adw::ActionRow {
        let row = adw::ActionRow::new();
        row.set_title(&glib::markup_escape_text(&friend.name));
        let subtitle = match friend.kind {
            Relationship::INCOMING_REQUEST => "Incoming friend request".to_owned(),
            Relationship::OUTGOING_REQUEST => "Outgoing friend request".to_owned(),
            Relationship::BLOCKED => friend.username.clone(),
            _ => friend
                .summary
                .clone()
                .unwrap_or_else(|| status_label(&friend.status).to_owned()),
        };
        row.set_subtitle(&glib::markup_escape_text(&subtitle));

        let avatar = self.images.avatar(36, &friend.name, Some(&friend.avatar));
        if friend.kind == Relationship::FRIEND {
            row.add_prefix(&avatar_with_status(&avatar, &friend.status));
        } else {
            row.add_prefix(&avatar);
        }

        let id = friend.id;
        let action = |icon: &str, tooltip: &str| {
            let button = icon_button(icon, tooltip);
            button.set_valign(gtk::Align::Center);
            button
        };
        match friend.kind {
            Relationship::FRIEND => {
                let message = action("mail-send-symbolic", "Message");
                let app = self.clone();
                message.connect_clicked(move |_| app.open_dm_with(id));
                row.add_suffix(&message);
                let remove = action("user-trash-symbolic", "Remove Friend");
                let app = self.clone();
                let name = friend.name.clone();
                remove.connect_clicked(move |_| {
                    app.confirm_relationship_removal(id, &format!("Remove {name} as a friend?"));
                });
                row.add_suffix(&remove);
                row.set_activatable(true);
                let app = self.clone();
                row.connect_activated(move |_| app.open_dm_with(id));
            }
            Relationship::INCOMING_REQUEST => {
                let accept = action("object-select-symbolic", "Accept");
                accept.add_css_class("success");
                let app = self.clone();
                accept.connect_clicked(move |_| {
                    let Some((_, http, _)) = app.active() else {
                        return;
                    };
                    app.with_captcha(
                        "Discord wants to make sure you're human before it accepts this friend request.",
                        move |captcha| {
                            let http = http.clone();
                            async move { http.accept_friend(id, captcha.as_ref()).await }
                        },
                        move |app, result| match result {
                            Ok(()) => {}
                            Err(error) if error.is_captcha() => app.captcha_needed(
                                "Discord wants a captcha solved to accept this friend request.",
                                &format!("https://discord.com/users/{id}"),
                            ),
                            Err(error) => app.set_status(&format!("Could not accept: {error}")),
                        },
                    );
                });
                row.add_suffix(&accept);
                let decline = action("window-close-symbolic", "Decline");
                let app = self.clone();
                decline.connect_clicked(move |_| app.remove_relationship(id));
                row.add_suffix(&decline);
            }
            Relationship::OUTGOING_REQUEST => {
                let cancel = action("window-close-symbolic", "Cancel Request");
                let app = self.clone();
                cancel.connect_clicked(move |_| app.remove_relationship(id));
                row.add_suffix(&cancel);
            }
            _ => {
                let unblock = gtk::Button::with_label("Unblock");
                unblock.set_valign(gtk::Align::Center);
                let app = self.clone();
                unblock.connect_clicked(move |_| app.remove_relationship(id));
                row.add_suffix(&unblock);
            }
        }
        row
    }

    fn send_friend_request(self: &Rc<Self>, username: String) {
        self.guard(crate::safety::Action::FriendRequest, move |app| app.request_friend(username));
    }

    /// Sends a friend request by username (after the safety limits allowed it).
    fn request_friend(self: &Rc<Self>, username: String) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let shown = username.clone();
        self.with_captcha(
            "Discord wants to make sure you're human before it sends your friend request.",
            move |captcha| {
                let http = http.clone();
                let username = username.clone();
                async move { http.add_friend(&username, captcha.as_ref()).await }
            },
            move |app, result| match result {
                Ok(()) => app.set_status(&format!("Friend request sent to {shown}")),
                Err(error) if error.is_captcha() => app.captcha_needed(
                    "Discord wants a captcha solved to send this friend request.",
                    "https://discord.com/channels/@me",
                ),
                Err(error) => app.set_status(&format!("Could not send request: {error}")),
            },
        );
    }

    pub(crate) fn remove_relationship(self: &Rc<Self>, user_id: Snowflake) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        self.spawn(
            async move { http.remove_relationship(user_id).await },
            |app, result| {
                if let Err(error) = result {
                    app.set_status(&format!("Request failed: {error}"));
                }
            },
        );
    }

    fn confirm_relationship_removal(self: &Rc<Self>, user_id: Snowflake, question: &str) {
        let dialog = adw::AlertDialog::new(Some(question), None);
        dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let app = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "remove" {
                app.remove_relationship(user_id);
            }
        });
        dialog.present(Some(&self.window));
    }

    /// Opens the DM with a user, creating the channel when none exists.
    pub(crate) fn open_dm_with(self: &Rc<Self>, user_id: Snowflake) {
        let Some((_, http, store)) = self.active() else {
            return;
        };
        if let Some(channel) = store.read(|state| state.dm_with(user_id).cloned()) {
            self.open_channel(channel);
            return;
        }
        self.guard(crate::safety::Action::NewDm, move |app| app.create_dm_with(http, store, user_id));
    }

    /// Opens a new DM with a user (after the safety limits allowed it).
    fn create_dm_with(self: &Rc<Self>, http: kc_http::HttpClient, store: kc_state::Store, user_id: Snowflake) {
        self.spawn(
            async move { http.create_dm(user_id).await },
            move |app, result| match result {
                Ok(channel) => {
                    // The gateway also announces the channel; storing it now
                    // lets the view open without waiting for that.
                    store.apply(
                        "CHANNEL_CREATE",
                        &serde_json::to_value(&channel).unwrap_or_default(),
                    );
                    let stored = store.read(|state| state.channels.get(&channel.id).cloned());
                    app.open_channel(stored.unwrap_or(channel));
                }
                Err(error) => app.set_status(&format!("Could not open the conversation: {error}")),
            },
        );
    }
}
