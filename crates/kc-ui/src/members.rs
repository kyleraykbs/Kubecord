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
                self.set_window_title(&match guild {
                    Some(guild) => format!("{title} — {guild}"),
                    None => title,
                });
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
                self.set_window_title("kylecord");
            }
        }
    }

    /// The account's own name, avatar and status in the sidebar.
    pub(crate) fn render_user_bar(self: &Rc<Self>) {
        let info = self.with_session(|session| {
            let connection = session.connection.label();
            let ready = matches!(session.connection, session::ConnState::Ready);
            let fallback = self.account_name(&session.account);
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
                (user, status, custom, connection, ready, fallback)
            })
        });
        let Some((user, status, custom, connection, ready, fallback)) = info else {
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
            // Not heard from Discord yet (or never will: signed out).
            None => self.user_name_label.set_label(&fallback),
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
        let started = std::time::Instant::now();
        self.render_members_inner();
        let elapsed = started.elapsed();
        if elapsed >= std::time::Duration::from_millis(16) && crate::logging::diagnostics() {
            tracing::warn!(
                millis = elapsed.as_millis() as u64,
                "rebuilding the member list blocked the main thread"
            );
        }
    }

    fn render_members_inner(self: &Rc<Self>) {
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

        let group = self
            .with_session(|session| {
                let id = session.selected_channel?;
                session.store.read(|state| {
                    state
                        .channels
                        .get(&id)
                        .filter(|channel| channel.kind == ChannelType::GroupDm)
                        .map(|_| id)
                })
            })
            .flatten();
        if let Some(group) = group {
            let add = gtk::Button::with_label("Add Friends to Group");
            add.add_css_class("pill");
            add.set_margin_top(8);
            add.set_margin_start(6);
            add.set_margin_end(6);
            let app = self.clone();
            add.connect_clicked(move |_| app.group_dm_dialog(Some(group)));
            self.members_box.append(&add);
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
pub(crate) fn friendly_date_ms(millis: u64) -> String {
    glib::DateTime::from_unix_utc((millis / 1000) as i64)
        .and_then(|date| date.format("%b %e, %Y"))
        .map(|date| date.trim().replace("  ", " "))
        .unwrap_or_default()
}
