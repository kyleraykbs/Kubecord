//! The channel sidebar: a guild's channel tree or the direct-message list,
//! plus navigation between guilds, channels and the home view.

use std::collections::HashSet;
use std::rc::Rc;

use gtk::prelude::*;

use kc_core::Snowflake;
use kc_http::cdn;
use kc_state::{Channel, ChannelType};

use crate::session::Command;
use crate::shell::AppState;
use crate::voice::is_voice_only;
use crate::widgets::{avatar_with_status, badge, clear_box, first_icon, label};

/// One sidebar row of a guild channel.
struct ChannelRow {
    channel: Channel,
    unread: bool,
    mentions: u32,
    muted: bool,
    voice: Vec<VoiceUser>,
}

struct VoiceUser {
    id: Snowflake,
    name: String,
    avatar: String,
    muted: bool,
    deafened: bool,
    streaming: bool,
    video: bool,
}

struct DmRow {
    channel: Channel,
    title: String,
    subtitle: String,
    icon: Option<String>,
    status: Option<String>,
    unread: bool,
    mentions: u32,
}

impl AppState {
    /// Redraws the sidebar on the next idle, however many events asked.
    pub(crate) fn queue_sidebar(self: &Rc<Self>) {
        if self.sidebar_redraw_queued.replace(true) {
            return;
        }
        let app = self.clone();
        glib::idle_add_local_once(move || {
            app.sidebar_redraw_queued.set(false);
            app.render_guilds();
            app.render_channels();
        });
    }

    pub(crate) fn render_channels(self: &Rc<Self>) {
        clear_box(&self.channel_box);
        self.voice_rows.borrow_mut().clear();
        let Some((dm_mode, guild_id, selected)) = self.with_session(|session| {
            (
                session.dm_mode,
                session.selected_guild,
                session.selected_channel,
            )
        }) else {
            return;
        };

        if dm_mode || guild_id.is_none() {
            self.guild_name_label.set_label("Direct Messages");
            self.render_dm_list(selected);
        } else if let Some(guild_id) = guild_id {
            self.render_guild_channels(guild_id, selected);
        }
    }

    fn render_dm_list(self: &Rc<Self>, selected: Option<Snowflake>) {
        let rows: Vec<DmRow> = self
            .with_session(|session| {
                session.store.read(|state| {
                    state
                        .dm_channels()
                        .into_iter()
                        .map(|channel| {
                            let recipient = channel.dm_recipient();
                            let presence = recipient.and_then(|user| state.presence(user.id));
                            let subtitle = match recipient {
                                Some(_) => presence
                                    .and_then(|presence| presence.summary())
                                    .unwrap_or_default(),
                                None => {
                                    let count = channel.recipients.as_ref().map_or(0, Vec::len) + 1;
                                    format!("{count} members")
                                }
                            };
                            DmRow {
                                title: state.channel_title(channel),
                                subtitle,
                                icon: match recipient {
                                    Some(user) => {
                                        Some(cdn::user_avatar(user.id, user.avatar.as_deref(), 64))
                                    }
                                    None => channel
                                        .icon
                                        .as_deref()
                                        .map(|hash| cdn::channel_icon(channel.id, hash, 64)),
                                },
                                status: recipient.map(|user| state.status_of(user.id).to_owned()),
                                unread: state.unread(channel.id),
                                mentions: state.mention_count(channel.id),
                                channel: channel.clone(),
                            }
                        })
                        .collect()
                })
            })
            .unwrap_or_default();

        let pending = self
            .with_session(|session| {
                session.store.read(|state| {
                    state
                        .relationships_of(kc_state::Relationship::INCOMING_REQUEST)
                        .len() as u32
                })
            })
            .unwrap_or(0);

        let friends = gtk::Button::new();
        friends.add_css_class("flat");
        friends.add_css_class("kc-channel");
        if selected.is_none() {
            friends.add_css_class("kc-selected");
        }
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let icon = gtk::Image::from_icon_name(first_icon(&[
            "system-users-symbolic",
            "avatar-default-symbolic",
        ]));
        icon.set_pixel_size(20);
        icon.set_margin_start(6);
        icon.set_margin_end(6);
        content.append(&icon);
        let name = label("Friends", &["kc-channel-name"]);
        name.set_hexpand(true);
        content.append(&name);
        content.append(&badge(pending));
        friends.set_child(Some(&content));
        friends.set_height_request(40);
        let app = self.clone();
        friends.connect_clicked(move |_| app.open_home());
        self.channel_box.append(&friends);

        let heading = label("DIRECT MESSAGES", &["kc-category"]);
        heading.set_margin_top(12);
        heading.set_margin_start(6);
        heading.set_margin_bottom(4);
        self.channel_box.append(&heading);

        for row in rows {
            let button = gtk::Button::new();
            button.add_css_class("flat");
            button.add_css_class("kc-channel");
            if selected == Some(row.channel.id) {
                button.add_css_class("kc-selected");
            }
            if row.unread {
                button.add_css_class("kc-unread");
            }
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let avatar = self.images.avatar(32, &row.title, row.icon.as_deref());
            match &row.status {
                Some(status) => content.append(&avatar_with_status(&avatar, status)),
                None => content.append(&avatar),
            }
            let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
            text.set_valign(gtk::Align::Center);
            text.set_hexpand(true);
            text.append(&label(&row.title, &["kc-channel-name"]));
            if !row.subtitle.is_empty() {
                text.append(&label(&row.subtitle, &["caption", "dim-label"]));
            }
            content.append(&text);
            content.append(&badge(row.mentions));
            button.set_child(Some(&content));
            let app = self.clone();
            let channel = row.channel.clone();
            button.connect_clicked(move |_| app.open_channel(channel.clone()));
            self.channel_box.append(&button);
        }
    }

    fn render_guild_channels(self: &Rc<Self>, guild_id: Snowflake, selected: Option<Snowflake>) {
        let Some((name, sections, collapsed)) = self.with_session(|session| {
            session.store.read(|state| {
                let name = state
                    .guilds
                    .get(&guild_id)
                    .map(|guild| guild.name.clone())
                    .unwrap_or_default();
                let sections: Vec<(Option<Channel>, Vec<ChannelRow>)> = state
                    .channel_tree(guild_id)
                    .into_iter()
                    .map(|section| {
                        let rows = section
                            .channels
                            .into_iter()
                            .map(|channel| {
                                let voice = if channel.kind.is_voice() {
                                    let mut users: Vec<VoiceUser> = state
                                        .guild_voice_states(guild_id)
                                        .into_iter()
                                        .filter(|voice| voice.channel_id == Some(channel.id))
                                        .filter_map(|voice| {
                                            let user = state.users.get(&voice.user_id)?;
                                            Some(VoiceUser {
                                                id: user.id,
                                                name: state.display_name(Some(guild_id), user),
                                                avatar: cdn::user_avatar(
                                                    user.id,
                                                    user.avatar.as_deref(),
                                                    48,
                                                ),
                                                muted: voice.mute || voice.self_mute,
                                                deafened: voice.deaf || voice.self_deaf,
                                                streaming: voice.self_stream,
                                                video: voice.self_video,
                                            })
                                        })
                                        .collect();
                                    users.sort_by_key(|user| user.name.to_lowercase());
                                    users
                                } else {
                                    Vec::new()
                                };
                                ChannelRow {
                                    unread: state.unread(channel.id),
                                    mentions: state.mention_count(channel.id),
                                    muted: state.is_muted(channel.id),
                                    voice,
                                    channel,
                                }
                            })
                            .collect();
                        (section.category, rows)
                    })
                    .collect();
                (name, sections, session.collapsed.clone())
            })
        }) else {
            return;
        };

        self.guild_name_label
            .set_label(if name.is_empty() { "Server" } else { &name });

        if sections.is_empty() {
            let empty = label("No channels", &["dim-label"]);
            empty.set_halign(gtk::Align::Center);
            empty.set_margin_top(24);
            self.channel_box.append(&empty);
            return;
        }

        for (category, rows) in sections {
            let mut hidden = false;
            if let Some(category) = &category {
                hidden = collapsed.contains(&category.id);
                let header = gtk::Button::new();
                header.add_css_class("flat");
                header.add_css_class("kc-category");
                header.set_margin_top(10);
                let content = gtk::Box::new(gtk::Orientation::Horizontal, 2);
                let arrow = gtk::Image::from_icon_name(if hidden {
                    "pan-end-symbolic"
                } else {
                    "pan-down-symbolic"
                });
                arrow.set_pixel_size(12);
                content.append(&arrow);
                let name = gtk::Label::new(Some(
                    &category.name.clone().unwrap_or_default().to_uppercase(),
                ));
                name.set_xalign(0.0);
                name.set_hexpand(true);
                name.set_ellipsize(gtk::pango::EllipsizeMode::End);
                content.append(&name);
                header.set_child(Some(&content));
                let app = self.clone();
                let id = category.id;
                header.connect_clicked(move |_| {
                    app.with_session_mut(|session| {
                        if !session.collapsed.remove(&id) {
                            session.collapsed.insert(id);
                        }
                    });
                    app.render_channels();
                });
                self.channel_box.append(&header);
            }

            for row in rows {
                let is_selected = selected == Some(row.channel.id);
                // A collapsed category still shows what needs attention.
                if hidden && !is_selected && !row.unread && row.voice.is_empty() {
                    continue;
                }
                self.channel_box
                    .append(&self.channel_button(&row, is_selected));
                for user in &row.voice {
                    self.channel_box.append(&self.voice_user_row(user));
                }
            }
        }
    }

    fn channel_button(self: &Rc<Self>, row: &ChannelRow, selected: bool) -> gtk::Button {
        let channel = &row.channel;
        let button = gtk::Button::new();
        button.add_css_class("flat");
        button.add_css_class("kc-channel");
        if selected {
            button.add_css_class("kc-selected");
        }
        if row.unread {
            button.add_css_class("kc-unread");
        }
        if row.muted && !selected {
            button.add_css_class("kc-muted");
        }

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        if channel.kind.is_thread() {
            content.set_margin_start(18);
        }
        content.append(&channel_glyph(channel));
        let name = label(&channel.display_name(), &["kc-channel-name"]);
        name.set_hexpand(true);
        content.append(&name);
        content.append(&badge(row.mentions));
        button.set_child(Some(&content));

        let voice = is_voice_only(channel);
        let app = self.clone();
        let target = channel.clone();
        button.connect_clicked(move |_| {
            if voice {
                app.voice_channel_clicked(target.clone());
            } else {
                app.open_channel(target.clone());
            }
        });
        if voice {
            button.set_tooltip_text(Some("Double-click to join"));
        }

        // Right-click: join or leave a voice channel, mark a text channel read.
        let menu = gtk::GestureClick::new();
        menu.set_button(gtk::gdk::BUTTON_SECONDARY);
        let app = self.clone();
        let anchor = button.clone();
        let target = channel.clone();
        menu.connect_pressed(move |_, _, _, _| {
            let popover = gtk::Popover::new();
            popover.set_parent(&anchor);
            let item = if voice {
                let connected = app
                    .with_session(|session| {
                        session
                            .voice
                            .as_ref()
                            .is_some_and(|voice| voice.channel_id == target.id)
                    })
                    .unwrap_or(false);
                let item = gtk::Button::with_label(if connected {
                    "Disconnect"
                } else {
                    "Join Voice"
                });
                let app = app.clone();
                let inner = popover.clone();
                let target = target.clone();
                item.connect_clicked(move |_| {
                    inner.popdown();
                    if connected {
                        app.leave_voice();
                    } else {
                        app.join_voice_channel(&target);
                    }
                });
                item
            } else {
                let item = gtk::Button::with_label("Mark As Read");
                let app = app.clone();
                let inner = popover.clone();
                let id = target.id;
                item.connect_clicked(move |_| {
                    inner.popdown();
                    app.ack_channel(id);
                });
                item
            };
            item.add_css_class("flat");
            popover.set_child(Some(&item));
            popover.connect_closed(|popover| popover.unparent());
            popover.popup();
        });
        button.add_controller(menu);
        button
    }

    fn voice_user_row(self: &Rc<Self>, user: &VoiceUser) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_margin_start(34);
        row.set_margin_top(2);
        row.set_margin_bottom(2);
        row.set_margin_end(8);
        row.add_css_class("kc-voice-row");
        if self.speaking.borrow().contains(&user.id) {
            row.add_css_class("kc-speaking");
        }
        self.voice_rows
            .borrow_mut()
            .insert(user.id, row.clone().upcast());
        let avatar = self.images.avatar(22, &user.name, Some(&user.avatar));
        row.append(&avatar);
        let name = label(&user.name, &["caption", "kc-voice-name"]);
        name.set_hexpand(true);
        row.append(&name);
        if user.streaming {
            let live = gtk::Label::new(Some("LIVE"));
            live.add_css_class("kc-badge");
            row.append(&live);
        }
        for (shown, icon) in [
            (user.video, "camera-video-symbolic"),
            (user.muted, "microphone-sensitivity-muted-symbolic"),
            (user.deafened, "audio-volume-muted-symbolic"),
        ] {
            if shown {
                let image = gtk::Image::from_icon_name(icon);
                image.set_pixel_size(12);
                image.add_css_class("dim-label");
                row.append(&image);
            }
        }
        row
    }

    // -- navigation ------------------------------------------------------

    /// Shows the friends page and the direct-message list.
    pub(crate) fn open_home(self: &Rc<Self>) {
        self.with_session_mut(|session| {
            session.dm_mode = true;
            session.selected_guild = None;
            session.selected_channel = None;
            session.reply_to = None;
            session.editing = None;
        });
        self.render_all();
    }

    pub(crate) fn select_guild(self: &Rc<Self>, guild_id: Snowflake) {
        let target = self.with_session_mut(|session| {
            session.dm_mode = false;
            session.selected_guild = Some(guild_id);
            session.selected_channel = None;
            let remembered = session.last_channel.get(&guild_id).copied();
            session.store.read(|state| {
                remembered
                    .and_then(|id| state.channels.get(&id).cloned())
                    .or_else(|| {
                        // Otherwise the first text channel the account can read.
                        state
                            .channel_tree(guild_id)
                            .into_iter()
                            .flat_map(|section| section.channels)
                            .find(|channel| {
                                matches!(
                                    channel.kind,
                                    ChannelType::GuildText | ChannelType::GuildAnnouncement
                                )
                            })
                    })
            })
        });
        match target.flatten() {
            Some(channel) => self.open_channel(channel),
            None => self.render_all(),
        }
    }

    pub(crate) fn open_channel(self: &Rc<Self>, channel: Channel) {
        let Some((active, http, store)) = self.active() else {
            return;
        };
        let is_voice_only = channel.kind.is_voice() && !channel.kind.is_text();
        let (fetch, subscribe) = self
            .with_session_mut(|session| {
                let already_selected = session.selected_channel == Some(channel.id);
                session.selected_channel = Some(channel.id);
                if !already_selected {
                    session.last_typing = None;
                    session.reply_to = None;
                    session.editing = None;
                }
                if channel.is_private() {
                    session.dm_mode = true;
                    session.selected_guild = None;
                } else {
                    session.dm_mode = false;
                    if let Some(guild_id) = channel.guild_id {
                        session.selected_guild = Some(guild_id);
                        session.last_channel.insert(guild_id, channel.id);
                    }
                }
                let fetch = !is_voice_only && session.fetched.insert(channel.id);
                let subscribe = channel.guild_id.is_some() && session.subscribed.insert(channel.id);
                (fetch, subscribe)
            })
            .unwrap_or((false, false));

        if let (true, Some(guild_id)) = (subscribe, channel.guild_id) {
            let _ = self.cmd_tx.try_send(Command::SubscribeChannel {
                account: active,
                guild_id,
                channel_id: channel.id,
            });
        }
        if fetch {
            self.with_session_mut(|session| {
                session.history.entry(channel.id).or_default().loading = true;
            });
            let _ = self.cmd_tx.try_send(Command::FetchHistory {
                account: active,
                http,
                store,
                channel: channel.id,
            });
        }
        self.render_all();
        if !is_voice_only {
            self.ack_channel(channel.id);
            self.composer.grab_focus();
        }
    }

    /// Marks a channel read up to its newest message, locally and on Discord.
    pub(crate) fn ack_channel(self: &Rc<Self>, channel: Snowflake) {
        let Some((_, http, store)) = self.active() else {
            return;
        };
        let (newest, needed) = store.read(|state| {
            let newest = state.last_message_id(channel);
            let read = state
                .read_states
                .get(&channel)
                .and_then(|read| read.last_message_id);
            (
                newest,
                newest.is_some() && (read < newest || state.mention_count(channel) > 0),
            )
        });
        let (Some(message_id), true) = (newest, needed) else {
            return;
        };
        store.mark_read(channel, message_id);
        let _ = self.cmd_tx.try_send(Command::Ack {
            http,
            channel,
            message_id,
        });
        self.queue_sidebar();
    }

    /// Ids of the channels currently listed in the sidebar, in order.
    pub(crate) fn visible_channel_ids(&self) -> Vec<Snowflake> {
        self.with_session(|session| {
            session.store.read(|state| match session.selected_guild {
                Some(guild_id) if !session.dm_mode => {
                    let collapsed: &HashSet<Snowflake> = &session.collapsed;
                    state
                        .channel_tree(guild_id)
                        .into_iter()
                        .filter(|section| {
                            section
                                .category
                                .as_ref()
                                .is_none_or(|category| !collapsed.contains(&category.id))
                        })
                        .flat_map(|section| section.channels)
                        .filter(|channel| channel.kind.is_text() && !channel.kind.is_voice())
                        .map(|channel| channel.id)
                        .collect()
                }
                _ => state
                    .dm_channels()
                    .iter()
                    .map(|channel| channel.id)
                    .collect(),
            })
        })
        .unwrap_or_default()
    }
}

/// The small symbol in front of a channel name.
pub(crate) fn channel_glyph(channel: &Channel) -> gtk::Widget {
    let icon = match channel.kind {
        ChannelType::GuildVoice => Some("audio-volume-high-symbolic"),
        ChannelType::GuildStageVoice => Some("audio-input-microphone-symbolic"),
        ChannelType::GuildForum | ChannelType::GuildMedia => Some("view-list-symbolic"),
        kind if kind.is_thread() => Some("mail-reply-sender-symbolic"),
        _ => None,
    };
    match icon {
        Some(icon) => {
            let image = gtk::Image::from_icon_name(icon);
            image.set_pixel_size(16);
            image.add_css_class("dim-label");
            image.upcast()
        }
        None => {
            let text = match channel.kind {
                ChannelType::GuildAnnouncement => "📣",
                _ => "#",
            };
            let glyph = gtk::Label::new(Some(text));
            glyph.add_css_class("dim-label");
            glyph.set_width_request(16);
            glyph.upcast()
        }
    }
}
