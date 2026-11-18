//! The member list, the channel header, the user bar and profile popovers.

use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::Snowflake;
use kc_http::cdn;
use kc_state::{ChannelType, MemberListItem};

use crate::session;
use crate::shell::AppState;
use crate::widgets::{avatar_with_status, clear_box, hex_color, label, status_dot, status_label};

/// Rows drawn before the list is cut off; the server sends 200 at a time.
const MAX_MEMBER_ROWS: usize = 400;

enum MemberRow {
    Header(String),
    Member {
        id: Snowflake,
        name: String,
        avatar: String,
        color: Option<u32>,
        status: String,
        summary: Option<String>,
        bot: bool,
    },
}

impl AppState {
    /// Fills the chat header from the selected channel.
    pub(crate) fn render_channel_header(self: &Rc<Self>) {
        let info = self.with_session(|session| {
            let channel_id = session.selected_channel?;
            session.store.read(|state| {
                let channel = state.channels.get(&channel_id)?;
                let title = state.channel_title(channel);
                let guild = channel
                    .guild_id
                    .and_then(|id| state.guilds.get(&id))
                    .map(|guild| guild.name.clone());
                Some((
                    channel.kind,
                    title,
                    channel.topic.clone().unwrap_or_default(),
                    guild,
                ))
            })
        });
        let dm_mode = self.with_session(|session| session.dm_mode).unwrap_or(true);

        match info.flatten() {
            Some((kind, title, topic, guild)) => {
                let icon = match kind {
                    ChannelType::Dm => "avatar-default-symbolic",
                    ChannelType::GroupDm => "system-users-symbolic",
                    ChannelType::GuildVoice | ChannelType::GuildStageVoice => {
                        "audio-volume-high-symbolic"
                    }
                    kind if kind.is_thread() => "mail-reply-sender-symbolic",
                    _ => "",
                };
                self.header_icon.set_visible(!icon.is_empty());
                self.header_icon.set_icon_name(Some(icon));
                let prefix = if icon.is_empty() { "# " } else { "" };
                self.channel_title_label
                    .set_label(&format!("{prefix}{title}"));
                let first_line = topic.lines().next().unwrap_or("");
                self.channel_topic_label.set_label(first_line);
                self.channel_topic_label
                    .set_tooltip_text((!topic.is_empty()).then_some(topic.as_str()));
                let text_channel = kind.is_text();
                self.search_entry.set_visible(text_channel);
                self.pins_button.set_visible(text_channel);
                // A call button for DMs and group DMs; reflects whether a
                // call in this channel is already up.
                let is_dm = matches!(kind, ChannelType::Dm | ChannelType::GroupDm);
                self.dm_call_button.set_visible(is_dm);
                if is_dm {
                    let in_call = self
                        .with_session(|session| {
                            let channel = session.selected_channel;
                            session
                                .voice
                                .as_ref()
                                .map(|voice| Some(voice.channel_id) == channel)
                        })
                        .flatten()
                        .unwrap_or(false);
                    self.dm_call_button.set_tooltip_text(Some(if in_call {
                        "Disconnect"
                    } else {
                        "Start Voice Call"
                    }));
                    self.dm_call_button.set_icon_name(if in_call {
                        "call-stop-symbolic"
                    } else {
                        "call-start-symbolic"
                    });
                }
                self.window.set_title(Some(&match guild {
                    Some(guild) => format!("{title} — {guild}"),
                    None => title,
                }));
            }
            None => {
                self.header_icon.set_visible(dm_mode);
                self.header_icon
                    .set_icon_name(Some("system-users-symbolic"));
                self.channel_title_label
                    .set_label(if dm_mode { "Friends" } else { "" });
                self.channel_topic_label.set_label("");
                self.search_entry.set_visible(false);
                self.pins_button.set_visible(false);
                self.window.set_title(Some("kylecord"));
            }
        }
    }

    /// The account's own name, avatar and status in the sidebar.
    pub(crate) fn render_user_bar(self: &Rc<Self>) {
        let info = self.with_session(|session| {
            let connection = session.connection.label();
            let ready = matches!(session.connection, session::ConnState::Ready);
            session.store.read(|state| {
                let user = state.me.clone().or_else(|| session.user.clone());
                let status = state.status.clone().unwrap_or_else(|| "online".to_owned());
                let custom = state.custom_status.as_ref().and_then(|custom| {
                    let text = format!(
                        "{} {}",
                        custom.emoji_name.as_deref().unwrap_or(""),
                        custom.text.as_deref().unwrap_or("")
                    );
                    let text = text.trim().to_owned();
                    (!text.is_empty()).then_some(text)
                });
                (user, status, custom, connection, ready)
            })
        });
        let Some((user, status, custom, connection, ready)) = info else {
            self.user_name_label.set_label("Not signed in");
            self.user_status_label.set_label("");
            return;
        };

        // Rebuild the avatar overlay so the status dot reflects the present.
        let mut child = self.user_avatar_slot.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            if widget != *self.user_avatar.upcast_ref::<gtk::Widget>() {
                self.user_avatar_slot.remove_overlay(&widget);
            }
        }

        match user {
            Some(user) => {
                let name = user.display_name().to_owned();
                self.user_avatar.set_text(Some(&name));
                self.images.set_avatar(
                    &self.user_avatar,
                    &cdn::user_avatar(user.id, user.avatar.as_deref(), 64),
                );
                self.user_name_label.set_label(&name);
            }
            None => self.user_name_label.set_label("Connecting…"),
        }
        if ready {
            let shown = if status == "invisible" {
                "offline"
            } else {
                &status
            };
            self.user_avatar_slot.add_overlay(&status_dot(shown));
            self.user_status_label
                .set_label(&custom.unwrap_or_else(|| status_label(&status).to_owned()));
        } else {
            self.user_status_label.set_label(&connection);
        }
    }

    /// Changes the account's status here and for its other sessions.
    pub(crate) fn set_own_status(self: &Rc<Self>, status: &'static str) {
        let Some((account, http, store)) = self.active() else {
            return;
        };
        let custom = store.read(|state| {
            state.custom_status.as_ref().map(|custom| {
                (
                    custom.text.clone().unwrap_or_default(),
                    custom.emoji_name.clone(),
                )
            })
        });
        if let Some(gateway) = session::gateway(account) {
            gateway.update_presence(
                status,
                custom
                    .as_ref()
                    .map(|(text, emoji)| (text.as_str(), emoji.as_deref())),
                false,
            );
        }
        store.write(|state| state.status = Some(status.to_owned()));
        self.render_user_bar();
        self.spawn(
            async move { http.set_status(status).await },
            |app, result| {
                if let Err(error) = result {
                    app.set_status(&format!("Could not save status: {error}"));
                }
            },
        );
    }

    /// Sets (or clears, with empty text) the custom status.
    pub(crate) fn set_custom_status(self: &Rc<Self>, text: String) {
        let Some((account, http, store)) = self.active() else {
            return;
        };
        let text = text.trim().to_owned();
        let status =
            store.read(|state| state.status.clone().unwrap_or_else(|| "online".to_owned()));
        if let Some(gateway) = session::gateway(account) {
            gateway.update_presence(
                &status,
                (!text.is_empty()).then_some((text.as_str(), None)),
                false,
            );
        }
        store.write(|state| {
            state.custom_status = (!text.is_empty()).then(|| kc_state::CustomStatus {
                text: Some(text.clone()),
                emoji_name: None,
                emoji_id: None,
            });
        });
        self.render_user_bar();
        self.spawn(
            async move {
                http.set_custom_status((!text.is_empty()).then_some(text.as_str()), None)
                    .await
            },
            |app, result| {
                if let Err(error) = result {
                    app.set_status(&format!("Could not save custom status: {error}"));
                }
            },
        );
    }

    /// Renders the member panel for the selected guild channel, or the
    /// recipients of a group DM.
    pub(crate) fn render_members(self: &Rc<Self>) {
        clear_box(&self.members_box);

        let data = self.with_session(|session| {
            let channel_id = session.selected_channel?;
            session.store.read(|state| {
                let channel = state.channels.get(&channel_id)?;
                let member = |id: Snowflake, guild: Option<Snowflake>| -> Option<MemberRow> {
                    let user = state.users.get(&id)?;
                    Some(MemberRow::Member {
                        id,
                        name: state.display_name(guild, user),
                        avatar: cdn::user_avatar(id, user.avatar.as_deref(), 64),
                        color: guild.and_then(|guild| state.member_color(guild, id)),
                        status: state.status_of(id).to_owned(),
                        summary: state.presence(id).and_then(|presence| presence.summary()),
                        bot: user.bot == Some(true),
                    })
                };

                if channel.kind == ChannelType::GroupDm {
                    let mut rows = vec![MemberRow::Header("MEMBERS".to_owned())];
                    let mut ids: Vec<Snowflake> = channel
                        .recipients
                        .iter()
                        .flatten()
                        .map(|user| user.id)
                        .collect();
                    ids.extend(state.my_id());
                    rows.extend(ids.into_iter().filter_map(|id| member(id, None)));
                    return Some((rows, None));
                }
                let guild_id = channel.guild_id?;
                let Some(list) = state.member_lists.get(&guild_id) else {
                    // Nothing synced yet: show the members we happen to know.
                    let mut rows = vec![MemberRow::Header("MEMBERS".to_owned())];
                    rows.extend(
                        state
                            .guild_member_users(guild_id)
                            .into_iter()
                            .take(MAX_MEMBER_ROWS)
                            .filter_map(|user| member(user.id, Some(guild_id))),
                    );
                    return Some((rows, None));
                };
                let rows = list
                    .rows()
                    .take(MAX_MEMBER_ROWS)
                    .filter_map(|item| match item {
                        MemberListItem::Group { id, count } => {
                            let count = list.group_counts.get(id).copied().unwrap_or(*count);
                            let name = match id.as_str() {
                                "online" => "Online".to_owned(),
                                "offline" => "Offline".to_owned(),
                                role => role
                                    .parse()
                                    .ok()
                                    .and_then(|role| state.role(guild_id, role))
                                    .map_or_else(|| "Members".to_owned(), |role| role.name.clone()),
                            };
                            Some(MemberRow::Header(format!(
                                "{} — {count}",
                                name.to_uppercase()
                            )))
                        }
                        MemberListItem::Member { user_id } => member(*user_id, Some(guild_id)),
                    })
                    .collect();
                Some((rows, Some((list.member_count, list.online_count))))
            })
        });

        let Some((rows, counts)) = data.flatten() else {
            self.members_toggle.set_visible(false);
            self.split.set_show_sidebar(false);
            return;
        };
        self.members_toggle.set_visible(true);
        self.members_guard.set(true);
        self.members_toggle.set_active(self.show_members.get());
        self.members_guard.set(false);
        self.split.set_show_sidebar(self.show_members.get());
        if !self.show_members.get() {
            return;
        }

        match counts {
            Some((members, online)) => {
                self.members_count_label
                    .set_label(&format!("{members} members · {online} online"));
                self.members_count_label.set_visible(true);
            }
            None => self.members_count_label.set_visible(false),
        }

        for row in rows {
            match row {
                MemberRow::Header(text) => {
                    let heading = label(&text, &["kc-category"]);
                    heading.set_margin_top(12);
                    heading.set_margin_start(6);
                    heading.set_margin_bottom(2);
                    self.members_box.append(&heading);
                }
                MemberRow::Member {
                    id,
                    name,
                    avatar,
                    color,
                    status,
                    summary,
                    bot,
                } => {
                    let button = gtk::Button::new();
                    button.add_css_class("flat");
                    button.add_css_class("kc-channel");
                    if status == "offline" {
                        button.add_css_class("kc-muted");
                    }
                    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                    let picture = self.images.avatar(32, &name, Some(&avatar));
                    content.append(&avatar_with_status(&picture, &status));
                    let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
                    text.set_valign(gtk::Align::Center);
                    text.set_hexpand(true);
                    let name_label = gtk::Label::new(None);
                    name_label.set_xalign(0.0);
                    name_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                    let escaped = glib::markup_escape_text(&name);
                    match color {
                        Some(color) => name_label.set_markup(&format!(
                            "<span foreground=\"{}\">{escaped}</span>",
                            hex_color(color)
                        )),
                        None => name_label.set_markup(&escaped),
                    }
                    text.append(&name_label);
                    if let Some(summary) = summary {
                        text.append(&label(&summary, &["caption", "dim-label"]));
                    }
                    content.append(&text);
                    if bot {
                        let tag = gtk::Label::new(Some("APP"));
                        tag.add_css_class("kc-tag");
                        tag.set_valign(gtk::Align::Center);
                        content.append(&tag);
                    }
                    button.set_child(Some(&content));
                    let app = self.clone();
                    button.connect_clicked(move |button| app.show_profile(id, button.upcast_ref()));
                    self.attach_user_menu(&button, id);
                    self.members_box.append(&button);
                }
            }
        }
    }

    /// A popover with what is known about a user, anchored to `anchor`.
    pub(crate) fn show_profile(self: &Rc<Self>, user_id: Snowflake, anchor: &gtk::Widget) {
        let Some((_, http, store)) = self.active() else {
            return;
        };
        let guild = self
            .with_session(|session| session.selected_guild.filter(|_| !session.dm_mode))
            .flatten();
        let Some((name, username, avatar, status, summary, roles, is_me, bot, relationship)) =
            store.read(|state| {
                let user = state.users.get(&user_id)?;
                let roles: Vec<(String, u32)> = guild
                    .and_then(|guild| {
                        let member = state.member(guild, user_id)?;
                        let mut roles: Vec<&kc_state::Role> = member
                            .roles
                            .iter()
                            .filter_map(|role| state.role(guild, *role))
                            .collect();
                        roles.sort_by_key(|role| std::cmp::Reverse(role.position));
                        Some(
                            roles
                                .into_iter()
                                .map(|role| (role.name.clone(), role.color))
                                .collect(),
                        )
                    })
                    .unwrap_or_default();
                Some((
                    state.display_name(guild, user),
                    user.username.clone(),
                    cdn::user_avatar(user_id, user.avatar.as_deref(), 160),
                    state.status_of(user_id).to_owned(),
                    state
                        .presence(user_id)
                        .and_then(|presence| presence.summary()),
                    roles,
                    state.my_id() == Some(user_id),
                    user.bot == Some(true),
                    state
                        .relationships
                        .get(&user_id)
                        .map(|relationship| relationship.kind),
                ))
            })
        else {
            return;
        };

        // A floating profile card, like Discord's user popout.
        let popover = adw::Dialog::new();
        popover.set_title("Profile");
        popover.set_content_width(360);
        popover.add_css_class("kc-profile");
        let _ = anchor;

        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_width_request(280);
        content.set_margin_top(4);
        content.set_margin_bottom(8);
        content.set_margin_start(8);
        content.set_margin_end(8);

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let picture = self.images.avatar(64, &name, Some(&avatar));
        top.append(&avatar_with_status(&picture, &status));
        let names = gtk::Box::new(gtk::Orientation::Vertical, 0);
        names.set_valign(gtk::Align::Center);
        let title = label(&name, &["title-3"]);
        title.set_selectable(true);
        names.append(&title);
        let handle = label(&username, &["dim-label"]);
        handle.set_selectable(true);
        names.append(&handle);
        if bot {
            let tag = gtk::Label::new(Some("APP"));
            tag.add_css_class("kc-tag");
            tag.set_halign(gtk::Align::Start);
            names.append(&tag);
        }
        top.append(&names);
        content.append(&top);

        content.append(&label(
            &summary.unwrap_or_else(|| status_label(&status).to_owned()),
            &["caption"],
        ));

        // Bio and pronouns arrive from the profile endpoint.
        let about = gtk::Label::new(None);
        about.set_xalign(0.0);
        about.set_wrap(true);
        about.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        about.set_max_width_chars(36);
        about.set_selectable(true);
        about.set_visible(false);
        content.append(&about);

        if !roles.is_empty() {
            content.append(&label("ROLES", &["kc-category"]));
            let wrap = libadwaita::WrapBox::new();
            wrap.set_child_spacing(4);
            wrap.set_line_spacing(4);
            for (role, color) in roles {
                let chip = gtk::Label::new(None);
                chip.add_css_class("kc-reaction");
                let escaped = glib::markup_escape_text(&role);
                if color != 0 {
                    chip.set_markup(&format!(
                        "<span foreground=\"{}\">●</span> {escaped}",
                        hex_color(color)
                    ));
                } else {
                    chip.set_markup(&escaped);
                }
                wrap.append(&chip);
            }
            content.append(&wrap);
        }

        // Member since: the guild join date in a guild, else account age.
        let since = store.read(|state| {
            guild
                .and_then(|guild| state.member(guild, user_id)?.joined_at.clone())
                .and_then(|iso| kc_state::time::iso_to_unix_millis(&iso))
                .unwrap_or_else(|| user_id.timestamp())
        });
        content.append(&label("MEMBER SINCE", &["kc-category"]));
        content.append(&label(&friendly_date_ms(since), &["caption"]));

        // Mutual servers and friends, filled in once the profile arrives.
        let mutuals = label("", &["caption", "dim-label"]);
        mutuals.set_xalign(0.0);
        mutuals.set_visible(false);
        content.append(&mutuals);

        // A personal note, saved on Enter.
        let note = gtk::Entry::new();
        note.set_placeholder_text(Some("Click to add a note"));
        note.add_css_class("kc-note");
        if !is_me {
            content.append(&label("NOTE", &["kc-category"]));
            content.append(&note);
            let app = self.clone();
            note.connect_activate(move |note| {
                let text = note.text().to_string();
                let Some((_, http, _)) = app.active() else {
                    return;
                };
                app.spawn(
                    async move { http.set_note(user_id, &text).await },
                    |app, result| {
                        if let Err(error) = result {
                            app.set_status(&format!("Could not save note: {error}"));
                        }
                    },
                );
            });
        }

        if !is_me && !bot {
            let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            actions.set_margin_top(4);
            let message = gtk::Button::with_label("Message");
            message.add_css_class("suggested-action");
            message.set_hexpand(true);
            let app = self.clone();
            let parent = popover.clone();
            message.connect_clicked(move |_| {
                parent.close();
                app.open_dm_with(user_id);
            });
            actions.append(&message);

            use kc_state::Relationship;
            let (text, add) = match relationship {
                Some(Relationship::FRIEND) => ("Remove Friend", false),
                Some(Relationship::INCOMING_REQUEST) => ("Accept Request", true),
                Some(Relationship::OUTGOING_REQUEST) => ("Cancel Request", false),
                Some(Relationship::BLOCKED) => ("Unblock", false),
                _ => ("Add Friend", true),
            };
            let friend = gtk::Button::with_label(text);
            let app = self.clone();
            let parent = popover.clone();
            friend.connect_clicked(move |_| {
                parent.close();
                if add {
                    let Some((_, http, _)) = app.active() else {
                        return;
                    };
                    app.spawn(
                        async move { http.accept_friend(user_id).await },
                        |app, result| match result {
                            Ok(()) => app.set_status("Friend request sent"),
                            Err(error) => app.set_status(&format!("Request failed: {error}")),
                        },
                    );
                } else {
                    app.remove_relationship(user_id);
                }
            });
            actions.append(&friend);
            content.append(&actions);
        }

        if is_me {
            let edit = gtk::Button::with_label("Edit Profile");
            edit.add_css_class("suggested-action");
            edit.set_margin_top(4);
            let app = self.clone();
            let parent = popover.clone();
            edit.connect_clicked(move |_| {
                parent.close();
                app.edit_profile_dialog();
            });
            content.append(&edit);
        }

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_propagate_natural_height(true);
        scroller.set_max_content_height(640);
        scroller.set_child(Some(&content));
        popover.set_child(Some(&scroller));
        popover.present(Some(&self.window));

        let about = glib::SendWeakRef::from(about.downgrade());
        let mutuals = glib::SendWeakRef::from(mutuals.downgrade());
        let note_ref = glib::SendWeakRef::from(note.downgrade());
        self.spawn(
            async move { http.user_profile(user_id, guild).await },
            move |_, result| {
                let Ok(profile) = result else {
                    return;
                };
                // Mutual servers and friends.
                if let Some(mutuals) = mutuals.upgrade() {
                    let servers = profile
                        .get("mutual_guilds")
                        .and_then(|value| value.as_array())
                        .map(|list| list.len())
                        .unwrap_or(0);
                    let friends = profile
                        .get("mutual_friends")
                        .and_then(|value| value.as_array())
                        .map(|list| list.len())
                        .unwrap_or(0);
                    let mut parts = Vec::new();
                    if friends > 0 {
                        parts.push(format!(
                            "{friends} mutual friend{}",
                            if friends == 1 { "" } else { "s" }
                        ));
                    }
                    if servers > 0 {
                        parts.push(format!(
                            "{servers} mutual server{}",
                            if servers == 1 { "" } else { "s" }
                        ));
                    }
                    if !parts.is_empty() {
                        mutuals.set_label(&parts.join(" · "));
                        mutuals.set_visible(true);
                    }
                }
                // Prefill the saved note.
                if let (Some(note), Some(text)) = (
                    note_ref.upgrade(),
                    profile
                        .pointer("/notes/note")
                        .or_else(|| profile.pointer("/note/note"))
                        .and_then(|value| value.as_str()),
                ) {
                    note.set_text(text);
                }
                let Some(about) = about.upgrade() else {
                    return;
                };
                let bio = profile
                    .pointer("/guild_member_profile/bio")
                    .and_then(|bio| bio.as_str())
                    .filter(|bio| !bio.is_empty())
                    .or_else(|| {
                        profile
                            .pointer("/user_profile/bio")
                            .and_then(|bio| bio.as_str())
                    })
                    .or_else(|| profile.pointer("/user/bio").and_then(|bio| bio.as_str()))
                    .unwrap_or("");
                let pronouns = profile
                    .pointer("/user_profile/pronouns")
                    .and_then(|pronouns| pronouns.as_str())
                    .unwrap_or("");
                let text = [pronouns, bio]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if !text.is_empty() {
                    about.set_label(&text);
                    about.set_visible(true);
                }
            },
        );
    }

    /// Edits the account's own profile: display name, pronouns and bio.
    pub(crate) fn edit_profile_dialog(self: &Rc<Self>) {
        let Some((_, http, store)) = self.active() else {
            return;
        };
        let (my_id, current_name) = store.read(|state| {
            let user = state.me.clone();
            (
                user.as_ref().map(|user| user.id),
                user.map(|user| user.display_name().to_owned())
                    .unwrap_or_default(),
            )
        });
        let Some(my_id) = my_id else {
            return;
        };

        let dialog = adw::Dialog::new();
        dialog.set_title("Edit Profile");
        dialog.set_content_width(380);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);

        content.append(&label("DISPLAY NAME", &["kc-category"]));
        let name = gtk::Entry::new();
        name.set_text(&current_name);
        content.append(&name);

        content.append(&label("PRONOUNS", &["kc-category"]));
        let pronouns = gtk::Entry::new();
        pronouns.set_placeholder_text(Some("e.g. they/them"));
        content.append(&pronouns);

        content.append(&label("ABOUT ME", &["kc-category"]));
        let bio_view = gtk::TextView::new();
        bio_view.set_wrap_mode(gtk::WrapMode::WordChar);
        bio_view.set_size_request(-1, 90);
        bio_view.add_css_class("kc-bio-edit");
        let bio_scroll = gtk::ScrolledWindow::new();
        bio_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        bio_scroll.set_child(Some(&bio_view));
        content.append(&bio_scroll);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.set_halign(gtk::Align::End);
        buttons.set_margin_top(8);
        let cancel = gtk::Button::with_label("Cancel");
        let parent = dialog.clone();
        cancel.connect_clicked(move |_| {
            parent.close();
        });
        let save = gtk::Button::with_label("Save");
        save.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&save);
        content.append(&buttons);

        dialog.set_child(Some(&content));
        dialog.present(Some(&self.window));

        // Prefill pronouns and bio from the live profile.
        let pronouns_ref = glib::SendWeakRef::from(pronouns.downgrade());
        let bio_ref = glib::SendWeakRef::from(bio_view.downgrade());
        self.spawn(
            async move { http.user_profile(my_id, None).await },
            move |_, result| {
                let Ok(profile) = result else {
                    return;
                };
                if let Some(entry) = pronouns_ref.upgrade() {
                    let value = profile
                        .pointer("/user_profile/pronouns")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    entry.set_text(value);
                }
                if let Some(view) = bio_ref.upgrade() {
                    let value = profile
                        .pointer("/user_profile/bio")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    view.buffer().set_text(value);
                }
            },
        );

        let app = self.clone();
        save.connect_clicked(move |_| {
            let new_name = name.text().trim().to_string();
            let new_pronouns = pronouns.text().trim().to_string();
            let buffer = bio_view.buffer();
            let new_bio = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .trim()
                .to_string();
            dialog.close();
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let rename = (new_name != current_name && !new_name.is_empty()).then_some(new_name);
            app.spawn(
                async move {
                    if let Some(name) = rename {
                        http.set_global_name(&name).await?;
                    }
                    http.set_profile(&new_bio, &new_pronouns).await
                },
                |app, result| match result {
                    Ok(()) => app.set_status("Profile updated"),
                    Err(error) => app.set_status(&format!("Could not update profile: {error}")),
                },
            );
        });
    }
}

/// Formats a Unix-millisecond instant like `Nov 11, 2026`.
fn friendly_date_ms(millis: u64) -> String {
    glib::DateTime::from_unix_utc((millis / 1000) as i64)
        .and_then(|date| date.format("%b %e, %Y"))
        .map(|date| date.trim().replace("  ", " "))
        .unwrap_or_default()
}
