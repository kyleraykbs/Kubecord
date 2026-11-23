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

/// What a dragged voice participant carries, before their user id.
const VOICE_DRAG: &str = "kc-voice-user:";

/// One sidebar row of a guild channel.
struct ChannelRow {
    channel: Channel,
    unread: bool,
    mentions: u32,
    muted: bool,
    voice: Vec<VoiceUser>,
}

#[derive(Hash)]
struct VoiceUser {
    id: Snowflake,
    name: String,
    avatar: String,
    muted: bool,
    deafened: bool,
    server_muted: bool,
    server_deafened: bool,
    streaming: bool,
    video: bool,
}

/// Feeds the display-relevant fields of a channel row into `hasher`, so a
/// redraw that would look the same is skipped. Deliberately excludes
/// `Channel::last_message_id` and other fields that change on every message
/// but do not change the row's appearance.
fn hash_channel_row(row: &ChannelRow, hasher: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    row.channel.id.hash(hasher);
    row.channel.name.hash(hasher);
    row.channel.kind.hash(hasher);
    row.channel.nsfw.hash(hasher);
    row.unread.hash(hasher);
    row.mentions.hash(hasher);
    row.muted.hash(hasher);
    row.voice.hash(hasher);
}

struct DmRow {
    channel: Channel,
    title: String,
    subtitle: kc_state::StatusLine,
    icon: Option<String>,
    status: Option<String>,
    unread: bool,
    mentions: u32,
}

impl AppState {
    /// Redraws the sidebar shortly after, coalescing however many events
    /// asked. A short delay (rather than the next idle) collapses the bursts
    /// of presence/read-state events a busy account streams into one rebuild.
    pub(crate) fn queue_sidebar(self: &Rc<Self>) {
        if self.sidebar_redraw_queued.replace(true) {
            return;
        }
        let app = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(80), move || {
            app.sidebar_redraw_queued.set(false);
            let t0 = std::time::Instant::now();
            app.render_guilds();
            let rail = t0.elapsed();
            let t1 = std::time::Instant::now();
            app.render_channels();
            let channels = t1.elapsed();
            if (rail + channels) >= std::time::Duration::from_millis(16)
                && crate::logging::diagnostics()
            {
                tracing::warn!(
                    rail_ms = rail.as_millis() as u64,
                    channels_ms = channels.as_millis() as u64,
                    "rebuilding the sidebar blocked the main thread"
                );
            }
        });
    }

    pub(crate) fn render_channels(self: &Rc<Self>) {
        let shown = self.with_session(|session| {
            (
                session.dm_mode,
                session.selected_guild,
                session.selected_channel,
            )
        });
        // The list is rebuilt on every click, voice state and unread
        // change. Rebuilding the same list must leave it where it was
        // scrolled to; another server's or the DMs' list starts at the top.
        let list = shown.map(|(dm_mode, guild_id, _)| (dm_mode, guild_id));
        let same_list = list.is_some() && self.sidebar_shown.replace(list) == list;
        let adjustment = self
            .channel_box
            .ancestor(gtk::ScrolledWindow::static_type())
            .and_downcast::<gtk::ScrolledWindow>()
            .map(|scroll| scroll.vadjustment());
        let kept = adjustment
            .as_ref()
            .map(|adjustment| if same_list { adjustment.value() } else { 0.0 });

        // A focused row about to be removed would hand the focus on, and
        // the viewport scrolls to whatever takes it.
        if GtkWindowExt::focus(&self.window)
            .is_some_and(|focus| focus.is_ancestor(&self.channel_box))
        {
            GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
        }
        self.fill_channels(shown);

        // Put the list back now, and again once the new rows have been
        // measured, since the viewport clamps the value to its old height
        // until then.
        if let (Some(adjustment), Some(value)) = (adjustment, kept) {
            adjustment.set_value(value);
            glib::idle_add_local_once(move || {
                if adjustment.value() != value {
                    adjustment.set_value(value);
                }
            });
        }
    }

    fn fill_channels(self: &Rc<Self>, shown: Option<(bool, Option<Snowflake>, Option<Snowflake>)>) {
        // The clear lives inside each builder, after its fingerprint check,
        // so a redraw that would show the same list leaves it (and its
        // scroll position and hover) untouched.
        let Some((dm_mode, guild_id, selected)) = shown else {
            clear_box(&self.channel_box);
            self.voice_rows.borrow_mut().clear();
            self.channels_fingerprint.set(None);
            return;
        };

        if dm_mode || guild_id.is_none() {
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
                                    .and_then(|presence| presence.status_line())
                                    .unwrap_or_default(),
                                None => {
                                    let count = channel.recipients.as_ref().map_or(0, Vec::len) + 1;
                                    kc_state::StatusLine {
                                        emoji: None,
                                        text: format!("{count} members"),
                                    }
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

        // Skip the rebuild when the DM list would look the same (a message or
        // presence event that changed nothing shown here). Hashes only the
        // displayed fields, not the raw channel.
        let fingerprint = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            "dm".hash(&mut hasher);
            selected.hash(&mut hasher);
            pending.hash(&mut hasher);
            for row in &rows {
                row.channel.id.hash(&mut hasher);
                row.channel.kind.hash(&mut hasher);
                row.title.hash(&mut hasher);
                row.subtitle.hash(&mut hasher);
                row.icon.hash(&mut hasher);
                row.status.hash(&mut hasher);
                row.unread.hash(&mut hasher);
                row.mentions.hash(&mut hasher);
            }
            hasher.finish()
        };
        if self.channels_fingerprint.get() == Some(fingerprint) {
            return;
        }
        self.channels_fingerprint.set(Some(fingerprint));

        self.guild_name_label.set_label("Direct Messages");
        // No server menu in the DM view.
        self.guild_menu.set_popover(gtk::Popover::NONE);
        self.guild_menu.set_sensitive(false);
        self.guild_menu_arrow.set_visible(false);
        clear_box(&self.channel_box);
        self.voice_rows.borrow_mut().clear();

        let friends = gtk::Button::new();
        friends.set_focus_on_click(false);
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
        content.append(&row_badge(pending));
        friends.set_child(Some(&content));
        friends.set_height_request(40);
        let app = self.clone();
        friends.connect_clicked(move |_| app.open_home());
        self.channel_box.append(&friends);

        let heading_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        heading_row.set_margin_top(12);
        heading_row.set_margin_bottom(4);
        let heading = label("DIRECT MESSAGES", &["kc-category"]);
        heading.set_margin_start(6);
        heading.set_hexpand(true);
        heading_row.append(&heading);
        let create = gtk::Button::from_icon_name("list-add-symbolic");
        create.set_focus_on_click(false);
        create.add_css_class("flat");
        create.add_css_class("circular");
        create.add_css_class("kc-dm-create");
        create.set_valign(gtk::Align::Center);
        create.set_tooltip_text(Some("Create DM"));
        let app = self.clone();
        create.connect_clicked(move |_| app.group_dm_dialog(None));
        heading_row.append(&create);
        self.channel_box.append(&heading_row);

        for row in rows {
            let button = gtk::Button::new();
            button.set_focus_on_click(false);
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
            if row.subtitle.emoji.is_some() || !row.subtitle.text.is_empty() {
                // Ellipsised; hovering shows the full status (the widget's
                // tooltip), its emoji drawn even when it is a server's.
                text.append(&crate::widgets::status_line_widget(
                    &self.images,
                    &row.subtitle,
                    &["caption", "dim-label"],
                ));
            }
            content.append(&text);
            content.append(&row_badge(row.mentions));
            // Room for the × laid over the row's end.
            let room = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            room.set_size_request(20, -1);
            content.append(&room);
            button.set_child(Some(&content));
            let app = self.clone();
            let channel = row.channel.clone();
            button.connect_clicked(move |_| app.open_channel(channel.clone()));
            // Right-click a one-to-one DM for the usual user menu.
            if let Some(recipient) = row.channel.dm_recipient().map(|user| user.id) {
                self.attach_user_menu(&button, recipient);
            } else if row.channel.kind == ChannelType::GroupDm {
                self.attach_group_menu(&button, row.channel.id);
            }
            // The × sits over the row rather than in it: a button takes the
            // clicks of what is inside it first, which left the × dead.
            let close = self.dm_close_button(&row.channel);
            close.set_halign(gtk::Align::End);
            close.set_margin_end(8);
            let item = gtk::Overlay::new();
            item.add_css_class("kc-dm-row");
            item.set_child(Some(&button));
            item.add_overlay(&close);
            self.channel_box.append(&item);
        }
    }

    fn render_guild_channels(self: &Rc<Self>, guild_id: Snowflake, selected: Option<Snowflake>) {
        let Some((name, sections, collapsed)) = self.with_session(|session| {
            let me = session.user.as_ref().map(|user| user.id);
            let own_flags = session.voice.as_ref();
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
                                            let (self_mute, self_deaf) = own_flags
                                                .filter(|_| Some(user.id) == me)
                                                .and_then(|own| own.own_flags(channel.id))
                                                .unwrap_or((voice.self_mute, voice.self_deaf));
                                            Some(VoiceUser {
                                                id: user.id,
                                                name: state.display_name(Some(guild_id), user),
                                                avatar: cdn::user_avatar(
                                                    user.id,
                                                    user.avatar.as_deref(),
                                                    48,
                                                ),
                                                muted: voice.mute || self_mute,
                                                deafened: voice.deaf || self_deaf,
                                                server_muted: voice.mute,
                                                server_deafened: voice.deaf,
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

        // Skip the rebuild when it would draw the same list (a message or
        // presence event elsewhere). Hashes only what is drawn, by field.
        let fingerprint = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            "guild".hash(&mut hasher);
            guild_id.hash(&mut hasher);
            selected.hash(&mut hasher);
            name.hash(&mut hasher);
            let mut folders: Vec<Snowflake> = collapsed.iter().copied().collect();
            folders.sort_unstable();
            folders.hash(&mut hasher);
            for (category, rows) in &sections {
                category.as_ref().map(|c| (c.id, &c.name)).hash(&mut hasher);
                for row in rows {
                    hash_channel_row(row, &mut hasher);
                }
            }
            hasher.finish()
        };
        if self.channels_fingerprint.get() == Some(fingerprint) {
            return;
        }
        self.channels_fingerprint.set(Some(fingerprint));

        self.guild_menu.set_sensitive(true);
        self.guild_menu_arrow.set_visible(true);
        let menu = self.build_server_menu(guild_id);
        self.guild_menu.set_popover(Some(&menu));
        clear_box(&self.channel_box);
        self.voice_rows.borrow_mut().clear();

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
                header.set_focus_on_click(false);
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
                self.attach_channel_menu(&header, category);
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
        button.set_widget_name(&format!("channel-{}", channel.id));
        // A click must not move the focus into a list that is about to be
        // rebuilt (see `render_channels`).
        button.set_focus_on_click(false);
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
        content.append(&row_badge(row.mentions));
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
            // A participant dragged here is moved (with Move Members).
            if let Some(guild) = channel.guild_id {
                let drop = gtk::DropTarget::new(String::static_type(), gtk::gdk::DragAction::MOVE);
                let app = self.clone();
                let target = channel.id;
                drop.connect_drop(move |_, value, _, _| {
                    let Some(user) = value
                        .get::<String>()
                        .ok()
                        .and_then(|text| text.strip_prefix(VOICE_DRAG)?.parse::<Snowflake>().ok())
                    else {
                        return false;
                    };
                    let allowed = app
                        .with_session(|session| {
                            session.store.read(|state| {
                                state
                                    .move_targets(guild)
                                    .iter()
                                    .any(|(channel, _)| *channel == target)
                            })
                        })
                        .unwrap_or(false);
                    if !allowed {
                        app.set_status("You can't move people into that channel");
                        return false;
                    }
                    app.voice_edit(guild, user, kc_http::MemberEdit::Move(Some(target)));
                    true
                });
                button.add_controller(drop);
            }
        }

        // Right-click: join or leave a voice channel, mark a text channel read.
        let menu = gtk::GestureClick::new();
        menu.set_button(gtk::gdk::BUTTON_SECONDARY);
        let app = self.clone();
        let anchor = button.clone();
        let target = channel.clone();
        menu.connect_pressed(move |_, _, x, y| {
            let popover = gtk::Popover::new();
            app.anchor_popover(&popover, &anchor, x, y);
            let items = gtk::Box::new(gtk::Orientation::Vertical, 2);
            items.add_css_class("kc-menu");
            items.set_width_request(200);
            let item = if voice {
                let connected = app
                    .with_session(|session| {
                        session
                            .voice
                            .as_ref()
                            .is_some_and(|voice| voice.channel_id == target.id)
                    })
                    .unwrap_or(false);
                let item = crate::menu::menu_item(
                    if connected {
                        "Disconnect"
                    } else {
                        "Join Voice"
                    },
                    false,
                );
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
                let item = crate::menu::menu_item("Mark As Read", false);
                let app = app.clone();
                let inner = popover.clone();
                let id = target.id;
                item.connect_clicked(move |_| {
                    inner.popdown();
                    app.ack_channel(id);
                });
                item
            };
            items.append(&item);
            app.add_channel_items(&items, &popover, &target);
            popover.set_child(Some(&items));
            popover.popup();
        });
        button.add_controller(menu);
        button
    }

    /// Right-click on a category header: create, edit, permissions, delete.
    fn attach_channel_menu(self: &Rc<Self>, widget: &gtk::Button, channel: &Channel) {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
        let app = self.clone();
        let anchor = widget.clone();
        let target = channel.clone();
        gesture.connect_pressed(move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let popover = gtk::Popover::new();
            app.anchor_popover(&popover, &anchor, x, y);
            let items = gtk::Box::new(gtk::Orientation::Vertical, 2);
            items.add_css_class("kc-menu");
            items.set_width_request(200);
            app.add_channel_items(&items, &popover, &target);
            popover.set_child(Some(&items));
            popover.popup();
        });
        widget.add_controller(gesture);
    }

    /// The small × shown on a DM row while it is hovered: closes a DM, or
    /// (after asking) leaves a group. Its click does not reach the row.
    fn dm_close_button(self: &Rc<Self>, channel: &Channel) -> gtk::Button {
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("circular");
        close.add_css_class("kc-dm-close");
        close.set_valign(gtk::Align::Center);
        close.set_focus_on_click(false);
        let group = channel.kind == ChannelType::GroupDm;
        close.set_tooltip_text(Some(if group { "Leave Group" } else { "Close DM" }));
        let app = self.clone();
        let id = channel.id;
        close.connect_clicked(move |_| {
            if group {
                app.confirm_leave_group(id);
            } else {
                app.close_private_channel(id);
            }
        });
        close
    }

    /// Right-click on a group DM: add friends, rename, leave.
    fn attach_group_menu(self: &Rc<Self>, widget: &gtk::Button, channel_id: Snowflake) {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
        let app = self.clone();
        let anchor = widget.downgrade();
        gesture.connect_pressed(move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            if let Some(anchor) = anchor.upgrade() {
                app.show_group_menu(channel_id, &anchor, x, y);
            }
        });
        widget.add_controller(gesture);
    }

    /// Shows a group DM's menu at `(x, y)` of `anchor`, and returns it.
    pub(crate) fn show_group_menu(
        self: &Rc<Self>,
        channel_id: Snowflake,
        anchor: &impl IsA<gtk::Widget>,
        x: f64,
        y: f64,
    ) -> gtk::Popover {
        let popover = gtk::Popover::new();
        self.anchor_popover(&popover, anchor, x, y);
        let items = gtk::Box::new(gtk::Orientation::Vertical, 2);
        items.add_css_class("kc-menu");
        items.set_width_request(200);
        self.add_group_items(&items, &popover, channel_id);
        popover.set_child(Some(&items));
        popover.popup();
        popover
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
        for (shown, server, icon, tooltip) in [
            (user.video, false, "camera-video-symbolic", "Camera"),
            (
                user.muted,
                user.server_muted,
                "microphone-sensitivity-muted-symbolic",
                "Muted",
            ),
            (
                user.deafened,
                user.server_deafened,
                "audio-volume-muted-symbolic",
                "Deafened",
            ),
        ] {
            if shown {
                let image = gtk::Image::from_icon_name(icon);
                image.set_pixel_size(12);
                if server {
                    image.add_css_class("kc-server-muted");
                    image.set_tooltip_text(Some(&format!("Server {tooltip}")));
                } else {
                    image.add_css_class("dim-label");
                    image.set_tooltip_text(Some(tooltip));
                }
                row.append(&image);
            }
        }
        // Left-click opens the profile, right-click the full user menu.
        let click = gtk::GestureClick::new();
        let app = self.clone();
        let id = user.id;
        click.connect_released(move |gesture, _, _, _| {
            if let Some(widget) = gesture.widget() {
                app.show_profile(id, &widget);
            }
        });
        row.add_controller(click);
        row.set_cursor_from_name(Some("pointer"));
        self.attach_user_menu(&row, user.id);
        // Dragging someone onto another voice channel moves them, when the
        // account may (Move Members).
        let drag = gtk::DragSource::new();
        drag.set_actions(gtk::gdk::DragAction::MOVE);
        let app = self.clone();
        drag.connect_prepare(move |_, _, _| {
            let guild = app
                .with_session(|session| session.selected_guild.filter(|_| !session.dm_mode))
                .flatten()?;
            if !app.member_actions(guild, id).move_member {
                return None;
            }
            Some(gtk::gdk::ContentProvider::for_value(
                &format!("{VOICE_DRAG}{id}").to_value(),
            ))
        });
        row.add_controller(drag);
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
        // The web client asks for the server's voice channel statuses as it
        // opens one.
        if let Some((account, _, _)) = self.active()
            && let Some(gateway) = crate::session::gateway(account)
        {
            gateway.request_channel_info(guild_id);
        }
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
        self.withdraw_channel_notification(active, channel.id);
        // Opening a DM, the web client asks whether a call is going on in it.
        if channel.is_private()
            && self.with_session(|session| session.selected_channel != Some(channel.id)).unwrap_or(true)
            && let Some(gateway) = crate::session::gateway(active)
        {
            gateway.call_connect(channel.id);
        }
        // A voice channel's chat counts as open while it shows beside the
        // call.
        let is_voice_only =
            channel.kind.is_voice() && !channel.kind.is_text() && !self.side_chat.get();
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
                limit: crate::chat::initial_page(self.window.height()),
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

/// A mention or request count at the end of a row, centred on it (the
/// plain badge sits in a corner, as on the server icons).
fn row_badge(count: u32) -> gtk::Label {
    let badge = badge(count);
    badge.set_valign(gtk::Align::Center);
    badge
}
