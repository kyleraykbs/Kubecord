//! Channel management from the sidebar: create text and voice channels and
//! categories, edit a channel (name, topic, slowmode, NSFW, bitrate, user
//! limit), delete it, and edit its permission overwrites per role or
//! member (allow / inherit / deny). Gated on Manage Channels, and on Manage
//! Roles in the channel for its permissions.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::Snowflake;
use kc_http::ChannelEdit;
use kc_state::permissions::{self, GROUPS};
use kc_state::{Channel, ChannelType, Overwrite};

use crate::menu::{MenuAction, menu_item};
use crate::moderation::color_dot;
use crate::shell::AppState;
use crate::video_setup::spawn_local;
use crate::widgets::label;

/// Slowmode choices, in seconds.
const SLOWMODES: &[(&str, u32)] = &[
    ("Off", 0),
    ("5 Seconds", 5),
    ("10 Seconds", 10),
    ("15 Seconds", 15),
    ("30 Seconds", 30),
    ("1 Minute", 60),
    ("2 Minutes", 120),
    ("5 Minutes", 300),
    ("10 Minutes", 600),
    ("15 Minutes", 900),
    ("30 Minutes", 1_800),
    ("1 Hour", 3_600),
    ("2 Hours", 7_200),
    ("6 Hours", 21_600),
];

/// Voice bitrates every server may use, in bits per second.
const BITRATES: &[(&str, u32)] = &[
    ("8 kbps", 8_000),
    ("32 kbps", 32_000),
    ("64 kbps", 64_000),
    ("96 kbps", 96_000),
];

/// One permission's state in an overwrite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tri {
    Deny,
    Inherit,
    Allow,
}

impl Tri {
    pub(crate) fn of(allow: u64, deny: u64, bit: u64) -> Self {
        if allow & bit != 0 {
            Self::Allow
        } else if deny & bit != 0 {
            Self::Deny
        } else {
            Self::Inherit
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Inherit => "inherit",
            Self::Allow => "allow",
        }
    }

    fn from_name(name: &str) -> Self {
        match name {
            "deny" => Self::Deny,
            "allow" => Self::Allow,
            _ => Self::Inherit,
        }
    }
}

/// The overwrite's allow and deny after setting one permission.
pub(crate) fn set_tri(allow: u64, deny: u64, bit: u64, state: Tri) -> (u64, u64) {
    let (allow, deny) = (allow & !bit, deny & !bit);
    match state {
        Tri::Allow => (allow | bit, deny),
        Tri::Deny => (allow, deny | bit),
        Tri::Inherit => (allow, deny),
    }
}

/// Discord channel names: lower case, spaces as dashes (text channels).
pub(crate) fn text_channel_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

fn toast(overlay: &adw::ToastOverlay, text: &str) {
    let toast = adw::Toast::new(text);
    toast.set_timeout(8);
    overlay.add_toast(toast);
}

fn page_dialog(title: &str, width: i32, height: i32) -> (adw::Dialog, adw::ToastOverlay, gtk::Box) {
    let dialog = adw::Dialog::new();
    dialog.set_title(title);
    dialog.set_content_width(width);
    dialog.set_content_height(height);
    let toasts = adw::ToastOverlay::new();
    let page = gtk::Box::new(gtk::Orientation::Vertical, 14);
    page.set_margin_top(18);
    page.set_margin_bottom(24);
    page.set_margin_start(24);
    page.set_margin_end(24);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);
    scroll.set_child(Some(&page));
    crate::settings_panel::dev_scroll(&scroll);
    toasts.set_child(Some(&scroll));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&toasts));
    dialog.set_child(Some(&view));
    (dialog, toasts, page)
}

impl AppState {
    fn channel_of(&self, channel_id: Snowflake) -> Option<Channel> {
        self.with_session(|session| {
            session
                .store
                .read(|state| state.channels.get(&channel_id).cloned())
        })
        .flatten()
    }

    fn can_in_channel(&self, channel_id: Snowflake, permission: u64) -> bool {
        self.with_session(|session| {
            session
                .store
                .read(|state| state.can(channel_id, permission))
        })
        .unwrap_or(false)
    }

    /// Adds Edit, Permissions, Delete (and, for a category, Create Channel)
    /// to a channel's right-click menu when the account may.
    pub(crate) fn add_channel_items(
        self: &Rc<Self>,
        menu: &gtk::Box,
        popover: &gtk::Popover,
        channel: &Channel,
    ) {
        let Some(guild) = channel.guild_id else {
            return;
        };
        let manage = self.can_in_channel(channel.id, permissions::MANAGE_CHANNELS);
        let roles = self.can_in_channel(channel.id, permissions::MANAGE_ROLES);
        let id = channel.id;
        let add = |text: &str, danger: bool, action: MenuAction| {
            let item = menu_item(text, danger);
            let popover = popover.clone();
            let app = self.clone();
            item.connect_clicked(move |_| {
                popover.popdown();
                action(&app);
            });
            menu.append(&item);
        };
        let category = channel.kind.is_category();
        if manage || roles {
            let line = gtk::Separator::new(gtk::Orientation::Horizontal);
            line.add_css_class("kc-menu-sep");
            menu.append(&line);
        }
        if manage && category {
            add(
                "Create Channel",
                false,
                Box::new(move |app| app.create_channel_dialog(guild, Some(id), false)),
            );
        }
        if manage {
            add(
                if category {
                    "Edit Category"
                } else {
                    "Edit Channel"
                },
                false,
                Box::new(move |app| app.edit_channel_dialog(id)),
            );
        }
        if roles {
            add(
                "Permissions",
                false,
                Box::new(move |app| app.channel_permissions_dialog(id)),
            );
        }
        if manage {
            add(
                if category {
                    "Delete Category"
                } else {
                    "Delete Channel"
                },
                true,
                Box::new(move |app| app.delete_channel_confirm(id)),
            );
        }
        self.add_notification_items(menu, popover, channel, guild);
        let line = gtk::Separator::new(gtk::Orientation::Horizontal);
        line.add_css_class("kc-menu-sep");
        menu.append(&line);
        add(
            "Copy Channel ID",
            false,
            Box::new(move |app| {
                app.window.clipboard().set_text(&id.to_string());
                app.set_status("Channel ID copied");
            }),
        );
    }

    /// Mute and the notification level for a channel or category: the
    /// account's Discord settings, the same ones the official apps change.
    fn add_notification_items(
        self: &Rc<Self>,
        menu: &gtk::Box,
        popover: &gtk::Popover,
        channel: &Channel,
        guild: Snowflake,
    ) {
        use kc_state::settings::level;

        let id = channel.id;
        let (muted, current) = self
            .with_session(|session| {
                session.store.read(|state| {
                    state
                        .guild_settings
                        .get(&Some(guild))
                        .and_then(|settings| settings.channel_overrides.get(&id))
                        .map_or((false, level::INHERIT), |own| {
                            (own.mute.muted, own.message_notifications)
                        })
                })
            })
            .unwrap_or((false, level::INHERIT));
        let line = gtk::Separator::new(gtk::Orientation::Horizontal);
        line.add_css_class("kc-menu-sep");
        menu.append(&line);
        // `check`: `None` for a plain item, else whether it is the choice.
        let add =
            |text: String, check: Option<bool>, muted: bool, chosen: u8, done: &'static str| {
                let item = menu_item(&text, false);
                if let Some(checked) = check
                    && let Some(label) = item.child()
                {
                    // A check column, so the choices line up.
                    item.set_child(None::<&gtk::Widget>);
                    let mark = gtk::Image::from_icon_name("object-select-symbolic");
                    mark.set_pixel_size(12);
                    mark.set_opacity(if checked { 1.0 } else { 0.0 });
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                    row.append(&mark);
                    row.append(&label);
                    item.set_child(Some(&row));
                }
                let popover = popover.clone();
                let app = self.clone();
                item.connect_clicked(move |_| {
                    popover.popdown();
                    app.change_notification_settings(
                        Some(guild),
                        kc_http::client::channel_notification_payload(id, muted, chosen),
                        done,
                    );
                });
                menu.append(&item);
            };
        let what = if channel.kind.is_category() {
            "Category"
        } else {
            "Channel"
        };
        if muted {
            add(format!("Unmute {what}"), None, false, current, "Unmuted");
        } else {
            add(format!("Mute {what}"), None, true, current, "Muted");
        }
        for (value, text) in [
            (level::INHERIT, "Use Server Default"),
            (level::ALL_MESSAGES, "All Messages"),
            (level::ONLY_MENTIONS, "Only @mentions"),
            (level::NO_MESSAGES, "Nothing"),
        ] {
            add(
                text.to_owned(),
                Some(value == current),
                muted,
                value,
                "Notification settings changed",
            );
        }
    }

    /// Creates a text or voice channel (in `parent`, a category), or a
    /// category.
    pub(crate) fn create_channel_dialog(
        self: &Rc<Self>,
        guild: Snowflake,
        parent: Option<Snowflake>,
        category: bool,
    ) {
        let categories: Vec<(Snowflake, String)> = self
            .with_session(|session| {
                session.store.read(|state| {
                    let mut categories: Vec<&Channel> = state
                        .channels
                        .values()
                        .filter(|channel| {
                            channel.guild_id == Some(guild) && channel.kind.is_category()
                        })
                        .collect();
                    categories.sort_by_key(|channel| channel.sort_key());
                    categories
                        .into_iter()
                        .map(|channel| (channel.id, channel.display_name()))
                        .collect()
                })
            })
            .unwrap_or_default();
        let title = if category {
            "Create Category"
        } else {
            "Create Channel"
        };
        let dialog = adw::AlertDialog::new(Some(title), None);
        dialog.add_responses(&[("cancel", "Cancel"), ("create", "Create")]);
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("create"));
        dialog.set_close_response("cancel");
        let fields = gtk::Box::new(gtk::Orientation::Vertical, 6);
        // Stage channels exist only in Community servers.
        let community = self
            .with_session(|session| session.store.read(|state| state.guilds.get(&guild).is_some_and(kc_state::Guild::is_community)))
            .unwrap_or(false);
        let kind = if community {
            gtk::DropDown::from_strings(&["Text", "Voice", "Stage"])
        } else {
            gtk::DropDown::from_strings(&["Text", "Voice"])
        };
        let place = {
            let mut names = vec!["No Category".to_owned()];
            names.extend(categories.iter().map(|(_, name)| name.to_uppercase()));
            gtk::DropDown::from_strings(&names.iter().map(String::as_str).collect::<Vec<_>>())
        };
        place.set_selected(
            parent
                .and_then(|parent| categories.iter().position(|(id, _)| *id == parent))
                .map_or(0, |index| index as u32 + 1),
        );
        if !category {
            fields.append(&label("Channel Type", &["caption-heading"]));
            fields.append(&kind);
            if !community {
                let note = label(
                    "Stage channels need a Community server (turned on in Discord's server settings).",
                    &["caption", "dim-label"],
                );
                note.set_wrap(true);
                note.set_xalign(0.0);
                fields.append(&note);
            }
            let caption = label("Category", &["caption-heading"]);
            caption.set_margin_top(6);
            fields.append(&caption);
            fields.append(&place);
        }
        let caption = label(
            if category {
                "Category Name"
            } else {
                "Channel Name"
            },
            &["caption-heading"],
        );
        caption.set_margin_top(6);
        fields.append(&caption);
        let entry = gtk::Entry::new();
        entry.set_placeholder_text(Some(if category {
            "New Category"
        } else {
            "new-channel"
        }));
        entry.set_activates_default(true);
        fields.append(&entry);
        dialog.set_extra_child(Some(&fields));

        let app = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "create" {
                return;
            }
            let voice = kind.selected() == 1;
            let stage = kind.selected() == 2;
            let raw = entry.text().trim().to_owned();
            let name = if category || voice || stage {
                raw
            } else {
                text_channel_name(&raw)
            };
            if name.is_empty() {
                return;
            }
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let kind = if category {
                ChannelType::GuildCategory
            } else if stage {
                ChannelType::GuildStageVoice
            } else if voice {
                ChannelType::GuildVoice
            } else {
                ChannelType::GuildText
            }
            .as_u8();
            let parent = (!category)
                .then(|| place.selected().checked_sub(1))
                .flatten()
                .and_then(|index| categories.get(index as usize))
                .map(|(id, _)| *id);
            app.spawn(
                async move { http.create_channel(guild, &name, kind, parent).await },
                |app, result| match result {
                    Ok(_) => app.set_status("Channel created"),
                    Err(error) => app.report_failure("Could not create the channel", &error),
                },
            );
        });
        dialog.present(Some(&self.window));
    }

    /// Edits a channel's settings; each change is sent at once.
    pub(crate) fn edit_channel_dialog(self: &Rc<Self>, channel_id: Snowflake) {
        let Some(channel) = self.channel_of(channel_id) else {
            return;
        };
        let text = matches!(
            channel.kind,
            ChannelType::GuildText | ChannelType::GuildAnnouncement | ChannelType::GuildForum
        );
        let voice = matches!(
            channel.kind,
            ChannelType::GuildVoice | ChannelType::GuildStageVoice
        );
        let (dialog, toasts, page) = page_dialog(
            &format!("Edit {}", channel.display_name()),
            520,
            if channel.kind.is_category() { 260 } else { 560 },
        );
        // Sends one change; `undo` puts the control back on a refusal.
        let send = {
            let app = self.clone();
            let toasts = toasts.downgrade();
            Rc::new(move |edit: ChannelEdit, undo: Option<Box<dyn FnOnce()>>| {
                let Some((_, http, _)) = app.active() else {
                    return;
                };
                let toasts = toasts.clone();
                spawn_local(
                    &app,
                    async move { http.edit_channel(channel_id, &edit).await },
                    move |app, result| match result {
                        Ok(_) => {
                            if let Some(toasts) = toasts.upgrade() {
                                toast(&toasts, "Saved");
                            }
                        }
                        Err(error) => {
                            match toasts.upgrade() {
                                Some(toasts) => {
                                    toast(&toasts, &format!("Could not save: {}", error.describe()))
                                }
                                None => app.report_failure("Could not save the channel", &error),
                            }
                            if let Some(undo) = undo {
                                undo();
                            }
                        }
                    },
                );
            })
        };
        let reverting = Rc::new(Cell::new(false));

        let overview = adw::PreferencesGroup::new();
        overview.set_title("Overview");
        if text || voice {
            overview.set_description(Some("Slowmode: how long members wait between messages."));
        }
        let name = adw::EntryRow::new();
        name.set_title(if channel.kind.is_category() {
            "Category Name"
        } else {
            "Channel Name"
        });
        name.set_text(channel.name.as_deref().unwrap_or(""));
        name.set_show_apply_button(true);
        let save = send.clone();
        let is_text = text;
        name.connect_apply(move |row| {
            let raw = row.text().trim().to_owned();
            let name = if is_text {
                text_channel_name(&raw)
            } else {
                raw
            };
            if name.is_empty() {
                return;
            }
            save(
                ChannelEdit {
                    name: Some(name),
                    ..ChannelEdit::default()
                },
                None,
            );
        });
        overview.add(&name);

        if text {
            let topic = adw::EntryRow::new();
            topic.set_title("Channel Topic");
            topic.set_text(channel.topic.as_deref().unwrap_or(""));
            topic.set_show_apply_button(true);
            let save = send.clone();
            topic.connect_apply(move |row| {
                save(
                    ChannelEdit {
                        topic: Some(row.text().trim().to_owned()),
                        ..ChannelEdit::default()
                    },
                    None,
                );
            });
            overview.add(&topic);
        }
        if text || voice {
            let slowmode = adw::ComboRow::new();
            slowmode.set_title("Slowmode");
            slowmode.set_model(Some(&gtk::StringList::new(
                &SLOWMODES.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            )));
            let current = channel.rate_limit_per_user.unwrap_or(0);
            let index = SLOWMODES
                .iter()
                .position(|(_, seconds)| *seconds == current)
                .unwrap_or(0) as u32;
            slowmode.set_selected(index);
            let last = Rc::new(Cell::new(index));
            let save = send.clone();
            let guard = reverting.clone();
            slowmode.connect_selected_notify(move |row| {
                if guard.get() {
                    return;
                }
                let previous = last.replace(row.selected());
                let seconds = SLOWMODES
                    .get(row.selected() as usize)
                    .map_or(0, |(_, seconds)| *seconds);
                let row = row.downgrade();
                let guard = guard.clone();
                let last = last.clone();
                save(
                    ChannelEdit {
                        rate_limit_per_user: Some(seconds),
                        ..ChannelEdit::default()
                    },
                    Some(Box::new(move || {
                        if let Some(row) = row.upgrade() {
                            guard.set(true);
                            row.set_selected(previous);
                            last.set(previous);
                            guard.set(false);
                        }
                    })),
                );
            });
            overview.add(&slowmode);
        }
        if text || voice {
            let nsfw = adw::SwitchRow::new();
            nsfw.set_title("Age-Restricted Channel");
            nsfw.set_subtitle("Members must confirm they are over the legal age to see it.");
            nsfw.set_active(channel.nsfw.unwrap_or(false));
            let save = send.clone();
            let guard = reverting.clone();
            nsfw.connect_active_notify(move |row| {
                if guard.get() {
                    return;
                }
                let on = row.is_active();
                let row = row.downgrade();
                let guard = guard.clone();
                save(
                    ChannelEdit {
                        nsfw: Some(on),
                        ..ChannelEdit::default()
                    },
                    Some(Box::new(move || {
                        if let Some(row) = row.upgrade() {
                            guard.set(true);
                            row.set_active(!on);
                            guard.set(false);
                        }
                    })),
                );
            });
            overview.add(&nsfw);
        }
        page.append(&overview);

        if voice {
            let audio = adw::PreferencesGroup::new();
            audio.set_title("Voice");
            audio.set_description(Some(
                "Bitrates above 64 kbps only help listeners on good connections.",
            ));
            let bitrate = adw::ComboRow::new();
            bitrate.set_title("Bitrate");
            let mut choices: Vec<(String, u32)> = BITRATES
                .iter()
                .map(|(name, bits)| ((*name).to_owned(), *bits))
                .collect();
            let current = channel.bitrate.unwrap_or(64_000);
            if !choices.iter().any(|(_, bits)| *bits == current) {
                // A boosted server's higher bitrate stays selectable.
                choices.push((format!("{} kbps", current / 1000), current));
                choices.sort_by_key(|(_, bits)| *bits);
            }
            bitrate.set_model(Some(&gtk::StringList::new(
                &choices
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )));
            bitrate.set_selected(
                choices
                    .iter()
                    .position(|(_, bits)| *bits == current)
                    .unwrap_or(0) as u32,
            );
            let save = send.clone();
            bitrate.connect_selected_notify(move |row| {
                if let Some((_, bits)) = choices.get(row.selected() as usize) {
                    save(
                        ChannelEdit {
                            bitrate: Some(*bits),
                            ..ChannelEdit::default()
                        },
                        None,
                    );
                }
            });
            audio.add(&bitrate);
            let limit = adw::SpinRow::with_range(0.0, 99.0, 1.0);
            limit.set_title("User Limit");
            limit.set_subtitle("0 is no limit.");
            limit.set_value(f64::from(channel.user_limit.unwrap_or(0)));
            // Sent when the value settles, not on every step.
            let save = send.clone();
            let pending = Rc::new(Cell::new(0u32));
            limit.connect_value_notify(move |row| {
                let ticket = pending.get().wrapping_add(1);
                pending.set(ticket);
                let pending = pending.clone();
                let save = save.clone();
                let value = row.value() as u32;
                glib::timeout_add_local_once(std::time::Duration::from_millis(700), move || {
                    if pending.get() == ticket {
                        save(
                            ChannelEdit {
                                user_limit: Some(value),
                                ..ChannelEdit::default()
                            },
                            None,
                        );
                    }
                });
            });
            audio.add(&limit);
            page.append(&audio);
        }

        if self.can_in_channel(channel_id, permissions::MANAGE_ROLES) {
            let more = gtk::Button::with_label("Permissions…");
            more.add_css_class("pill");
            more.set_halign(gtk::Align::Center);
            let app = self.clone();
            more.connect_clicked(move |_| app.channel_permissions_dialog(channel_id));
            page.append(&more);
        }
        dialog.present(Some(&self.window));
        let shown = dialog.downgrade();
        glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
            if let Some(dialog) = shown.upgrade() {
                dialog.set_focus(None::<&gtk::Widget>);
            }
        });
    }

    /// Confirms and deletes a channel or category.
    pub(crate) fn delete_channel_confirm(self: &Rc<Self>, channel_id: Snowflake) {
        let Some(channel) = self.channel_of(channel_id) else {
            return;
        };
        let what = if channel.kind.is_category() {
            "Category"
        } else {
            "Channel"
        };
        let name = channel.display_name();
        let dialog = adw::AlertDialog::new(
            Some(&format!("Delete {what}")),
            Some(&format!(
                "Are you sure you want to delete {name}? This cannot be undone.{}",
                if channel.kind.is_category() {
                    " Its channels stay, without a category."
                } else {
                    ""
                }
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", &format!("Delete {what}"))]);
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
            app.moderate(
                "Could not delete",
                format!("{name} was deleted"),
                async move { http.delete_channel(channel_id).await },
            );
        });
        dialog.present(Some(&self.window));
    }

    /// The channel's permission overwrites: pick a role or member, then set
    /// each permission to allow, inherit or deny.
    pub(crate) fn channel_permissions_dialog(self: &Rc<Self>, channel_id: Snowflake) {
        let Some(channel) = self.channel_of(channel_id) else {
            return;
        };
        let Some(guild) = channel.guild_id else {
            return;
        };
        let (dialog, toasts, page) = page_dialog(
            &format!("Permissions — {}", channel.display_name()),
            560,
            680,
        );
        let state = Rc::new(PermissionsState {
            channel: channel_id,
            guild,
            toasts,
            overwrites: RefCell::new(channel.permission_overwrites.clone()),
            selected: Cell::new(guild),
            rows: gtk::Box::new(gtk::Orientation::Vertical, 14),
            targets: gtk::Box::new(gtk::Orientation::Horizontal, 8),
            reverting: Cell::new(false),
        });
        page.append(&label_wrapped(
            "Overwrites change what a role or member may do in this channel only. \
             Inherit uses the server permissions; deny wins over allow for the same target.",
        ));
        page.append(&state.targets);
        page.append(&state.rows);
        self.fill_permission_targets(&state);
        self.fill_permission_rows(&state);
        dialog.present(Some(&self.window));
    }

    fn fill_permission_targets(self: &Rc<Self>, state: &Rc<PermissionsState>) {
        crate::widgets::clear_box(&state.targets);
        let guild = state.guild;
        let overwrites = state.overwrites.borrow().clone();
        // Targets: @everyone, then every overwritten role and member.
        let (named, missing): (Vec<Target>, Vec<Target>) = self
            .with_session(|session| {
                session.store.read(|store| {
                    let roles = store.roles_by_rank(guild);
                    let mut named: Vec<Target> = Vec::new();
                    for role in &roles {
                        if role.id == guild
                            || overwrites.iter().any(|overwrite| overwrite.id == role.id)
                        {
                            named.push((role.id, role.name.clone(), role.color));
                        }
                    }
                    for overwrite in &overwrites {
                        if overwrite.kind == Overwrite::MEMBER {
                            let name = store
                                .users
                                .get(&overwrite.id)
                                .map(|user| store.display_name(Some(guild), user))
                                .unwrap_or_else(|| overwrite.id.to_string());
                            named.push((overwrite.id, format!("@{name}"), 0));
                        }
                    }
                    let missing = roles
                        .iter()
                        .filter(|role| {
                            role.id != guild
                                && !overwrites.iter().any(|overwrite| overwrite.id == role.id)
                        })
                        .map(|role| (role.id, role.name.clone(), role.color))
                        .collect();
                    (named, missing)
                })
            })
            .unwrap_or_default();
        let picker = gtk::DropDown::from_strings(
            &named
                .iter()
                .map(|(_, name, _)| name.as_str())
                .collect::<Vec<_>>(),
        );
        picker.set_hexpand(true);
        picker.set_selected(
            named
                .iter()
                .position(|(id, _, _)| *id == state.selected.get())
                .unwrap_or(0) as u32,
        );
        let app = self.clone();
        let target = state.clone();
        let ids: Vec<Snowflake> = named.iter().map(|(id, _, _)| *id).collect();
        picker.connect_selected_notify(move |picker| {
            if let Some(id) = ids.get(picker.selected() as usize) {
                target.selected.set(*id);
                app.fill_permission_rows(&target);
            }
        });
        state.targets.append(&picker);

        // Add a role that has no overwrite yet.
        let add = gtk::MenuButton::new();
        add.set_icon_name("list-add-symbolic");
        add.set_tooltip_text(Some("Add a Role"));
        add.set_sensitive(!missing.is_empty());
        let popover = gtk::Popover::new();
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
        menu.add_css_class("kc-menu");
        for (id, name, color) in missing {
            let item = gtk::Button::new();
            item.add_css_class("flat");
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            content.append(&color_dot(color));
            content.append(&label(&name, &[]));
            item.set_child(Some(&content));
            let app = self.clone();
            let target = state.clone();
            let shown = popover.downgrade();
            item.connect_clicked(move |_| {
                if let Some(popover) = shown.upgrade() {
                    popover.popdown();
                }
                // An empty overwrite: it appears in the list and is saved
                // with the first change.
                target.overwrites.borrow_mut().push(Overwrite {
                    id,
                    kind: Overwrite::ROLE,
                    allow: 0,
                    deny: 0,
                });
                target.selected.set(id);
                // Rebuild after the click: this button is in the old list.
                let app = app.clone();
                let target = target.clone();
                glib::idle_add_local_once(move || {
                    app.fill_permission_targets(&target);
                    app.fill_permission_rows(&target);
                });
            });
            menu.append(&item);
        }
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_max_content_height(360);
        scroll.set_child(Some(&menu));
        popover.set_child(Some(&scroll));
        add.set_popover(Some(&popover));
        state.targets.append(&add);

        // Remove the selected overwrite (not @everyone's).
        let selected = state.selected.get();
        let remove = gtk::Button::from_icon_name("user-trash-symbolic");
        remove.set_tooltip_text(Some("Remove This Overwrite"));
        remove.set_sensitive(selected != guild);
        let app = self.clone();
        let target = state.clone();
        remove.connect_clicked(move |_| {
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let (channel, id) = (target.channel, target.selected.get());
            let target = target.clone();
            spawn_local(
                &app,
                async move { http.delete_overwrite(channel, id).await },
                move |app, result| match result {
                    Ok(()) => {
                        target
                            .overwrites
                            .borrow_mut()
                            .retain(|overwrite| overwrite.id != id);
                        target.selected.set(target.guild);
                        app.fill_permission_targets(&target);
                        app.fill_permission_rows(&target);
                        toast(&target.toasts, "Overwrite removed");
                    }
                    Err(error) => toast(
                        &target.toasts,
                        &format!("Could not remove it: {}", error.describe()),
                    ),
                },
            );
        });
        state.targets.append(&remove);
    }

    fn fill_permission_rows(self: &Rc<Self>, state: &Rc<PermissionsState>) {
        crate::widgets::clear_box(&state.rows);
        let selected = state.selected.get();
        let (allow, deny) = state
            .overwrites
            .borrow()
            .iter()
            .find(|overwrite| overwrite.id == selected)
            .map_or((0, 0), |overwrite| (overwrite.allow, overwrite.deny));
        // What the account itself has here: Discord refuses overwrites
        // that grant or deny more (unless administrator).
        let mine = self
            .with_session(|session| {
                session
                    .store
                    .read(|store| store.permissions_in(state.channel))
            })
            .unwrap_or(0);
        for (group_title, group) in GROUPS {
            let rows: Vec<_> = group.iter().filter(|info| info.in_channels).collect();
            if rows.is_empty() {
                continue;
            }
            let group_box = adw::PreferencesGroup::new();
            group_box.set_title(group_title.replace(" Server", "").as_str());
            for info in rows {
                let row = adw::ActionRow::new();
                row.set_title(info.name);
                row.set_subtitle(info.description);
                let toggles = adw::ToggleGroup::new();
                toggles.add_css_class("round");
                for (name, icon, tooltip) in [
                    ("deny", "window-close-symbolic", "Deny"),
                    ("inherit", "list-remove-symbolic", "Inherit"),
                    ("allow", "object-select-symbolic", "Allow"),
                ] {
                    let toggle = adw::Toggle::new();
                    toggle.set_name(Some(name));
                    toggle.set_icon_name(Some(icon));
                    toggle.set_tooltip(tooltip);
                    toggles.add(toggle);
                }
                toggles.set_valign(gtk::Align::Center);
                toggles.set_active_name(Some(Tri::of(allow, deny, info.bit).name()));
                if mine & info.bit == 0 {
                    toggles.set_sensitive(false);
                    row.set_tooltip_text(Some("You don't have this permission here yourself"));
                }
                let bit = info.bit;
                let app = self.clone();
                let target = state.clone();
                toggles.connect_active_name_notify(move |toggles| {
                    if target.reverting.get() {
                        return;
                    }
                    let wanted = Tri::from_name(toggles.active_name().as_deref().unwrap_or(""));
                    app.set_overwrite_bit(&target, bit, wanted, toggles);
                });
                row.add_suffix(&toggles);
                group_box.add(&row);
            }
            state.rows.append(&group_box);
        }
    }

    fn set_overwrite_bit(
        self: &Rc<Self>,
        state: &Rc<PermissionsState>,
        bit: u64,
        wanted: Tri,
        toggles: &adw::ToggleGroup,
    ) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let target = state.selected.get();
        let (kind, allow, deny) = state
            .overwrites
            .borrow()
            .iter()
            .find(|overwrite| overwrite.id == target)
            .map_or((Overwrite::ROLE, 0, 0), |overwrite| {
                (overwrite.kind, overwrite.allow, overwrite.deny)
            });
        let before = Tri::of(allow, deny, bit);
        let (new_allow, new_deny) = set_tri(allow, deny, bit, wanted);
        let save = |allow: u64, deny: u64| {
            let mut overwrites = state.overwrites.borrow_mut();
            match overwrites
                .iter_mut()
                .find(|overwrite| overwrite.id == target)
            {
                Some(overwrite) => {
                    overwrite.allow = allow;
                    overwrite.deny = deny;
                }
                None => overwrites.push(Overwrite {
                    id: target,
                    kind,
                    allow,
                    deny,
                }),
            }
        };
        save(new_allow, new_deny);
        let channel = state.channel;
        let state = state.clone();
        let toggles = toggles.downgrade();
        spawn_local(
            self,
            async move {
                http.set_overwrite(channel, target, kind, new_allow, new_deny)
                    .await
            },
            move |_, result| {
                if let Err(error) = result {
                    toast(
                        &state.toasts,
                        &format!("Could not save: {}", error.describe()),
                    );
                    let mut overwrites = state.overwrites.borrow_mut();
                    if let Some(overwrite) = overwrites
                        .iter_mut()
                        .find(|overwrite| overwrite.id == target)
                    {
                        let (allow, deny) = set_tri(overwrite.allow, overwrite.deny, bit, before);
                        overwrite.allow = allow;
                        overwrite.deny = deny;
                    }
                    if let Some(toggles) = toggles.upgrade() {
                        state.reverting.set(true);
                        toggles.set_active_name(Some(before.name()));
                        state.reverting.set(false);
                    }
                }
            },
        );
    }
}

fn label_wrapped(text: &str) -> gtk::Label {
    let text = label(text, &["dim-label"]);
    text.set_ellipsize(gtk::pango::EllipsizeMode::None);
    text.set_wrap(true);
    text.set_max_width_chars(1);
    text.set_hexpand(true);
    text
}

/// A role or member to pick: id, name, colour.
type Target = (Snowflake, String, u32);

/// The permissions dialog's state.
struct PermissionsState {
    channel: Snowflake,
    guild: Snowflake,
    toasts: adw::ToastOverlay,
    /// The overwrites as last sent (or added locally, not yet saved).
    overwrites: RefCell<Vec<Overwrite>>,
    /// The role or member being edited (`guild` is `@everyone`).
    selected: Cell<Snowflake>,
    rows: gtk::Box,
    targets: gtk::Box,
    reverting: Cell<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tri_state_toggles_move_a_bit_between_allow_and_deny() {
        let view = permissions::VIEW_CHANNEL;
        let send = permissions::SEND_MESSAGES;
        let (allow, deny) = set_tri(view, 0, send, Tri::Deny);
        assert_eq!((allow, deny), (view, send));
        assert_eq!(Tri::of(allow, deny, send), Tri::Deny);
        let (allow, deny) = set_tri(allow, deny, send, Tri::Allow);
        assert_eq!((allow, deny), (view | send, 0));
        let (allow, deny) = set_tri(allow, deny, view, Tri::Inherit);
        assert_eq!((allow, deny), (send, 0));
        assert_eq!(Tri::of(allow, deny, view), Tri::Inherit);
        assert_eq!(Tri::from_name(Tri::Allow.name()), Tri::Allow);
    }

    #[test]
    fn text_channel_names_are_lower_case_with_dashes() {
        assert_eq!(text_channel_name("  Off Topic  Chat "), "off-topic-chat");
        assert_eq!(text_channel_name("général"), "général");
    }
}
