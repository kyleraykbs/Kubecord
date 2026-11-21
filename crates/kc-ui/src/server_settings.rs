//! Server Settings: Overview, Roles, Members, Bans, Invites and Audit Log,
//! each shown only with the permission it needs. Changes apply as they
//! are made, like the app's own settings; a refusal from Discord appears
//! as a toast inside the dialog and the control goes back to what it was.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use serde_json::{Value, json};

use kc_core::Snowflake;
use kc_http::{Invite, RoleEdit, cdn};
use kc_state::permissions::{self, GROUPS};
use kc_state::time::{iso_to_unix_millis, unix_millis_now};
use kc_state::{ChannelType, Role, StoreEvent};

use crate::moderation::color_dot;
use crate::shell::AppState;
use crate::video_setup::spawn_local;
use crate::widgets::{clear_box, label};

/// Inactive (AFK) timeouts Discord offers, in seconds.
const AFK_TIMEOUTS: &[(&str, u32)] = &[
    ("1 Minute", 60),
    ("5 Minutes", 300),
    ("15 Minutes", 900),
    ("30 Minutes", 1_800),
    ("1 Hour", 3_600),
];

/// Invite lifetimes, in seconds (0: never).
pub(crate) const INVITE_AGES: &[(&str, u32)] = &[
    ("30 Minutes", 1_800),
    ("1 Hour", 3_600),
    ("6 Hours", 21_600),
    ("12 Hours", 43_200),
    ("1 Day", 86_400),
    ("7 Days", 604_800),
    ("Never", 0),
];

/// Invite use limits (0: none).
pub(crate) const INVITE_USES: &[(&str, u32)] = &[
    ("No Limit", 0),
    ("1 Use", 1),
    ("5 Uses", 5),
    ("10 Uses", 10),
    ("25 Uses", 25),
    ("50 Uses", 50),
    ("100 Uses", 100),
];

/// The pages, in order, with the permission that shows each.
const PAGES: &[(&str, &str, &str, u64)] = &[
    (
        "overview",
        "Overview",
        "preferences-system-symbolic",
        permissions::MANAGE_GUILD,
    ),
    (
        "roles",
        "Roles",
        "system-users-symbolic",
        permissions::MANAGE_ROLES,
    ),
    (
        "members",
        "Members",
        "avatar-default-symbolic",
        permissions::KICK_MEMBERS
            | permissions::BAN_MEMBERS
            | permissions::MODERATE_MEMBERS
            | permissions::MANAGE_ROLES
            | permissions::MANAGE_NICKNAMES,
    ),
    (
        "invites",
        "Invites",
        "mail-send-symbolic",
        permissions::MANAGE_GUILD,
    ),
    (
        "bans",
        "Bans",
        "action-unavailable-symbolic",
        permissions::BAN_MEMBERS,
    ),
    (
        "audit",
        "Audit Log",
        "document-open-recent-symbolic",
        permissions::VIEW_AUDIT_LOG,
    ),
];

/// Whether the account sees Server Settings at all.
pub(crate) fn settings_permissions() -> u64 {
    PAGES.iter().fold(0, |all, page| all | page.3)
}

/// Positions to send to move the role at `index` of `ranked` (highest
/// first) one place up or down: the two roles swap places. Equal positions
/// (Discord allows them) are split so the order really changes.
pub(crate) fn swap_positions(
    ranked: &[Role],
    index: usize,
    up: bool,
) -> Option<Vec<(Snowflake, i32)>> {
    let other = if up { index.checked_sub(1)? } else { index + 1 };
    let (moving, neighbour) = (ranked.get(index)?, ranked.get(other)?);
    let (mut to, mut from) = (neighbour.position, moving.position);
    if to == from {
        if up {
            to += 1;
        } else {
            from += 1;
        }
    }
    Some(vec![(moving.id, to), (neighbour.id, from)])
}

/// "Expires in 3 hours", "Never expires", "Expired".
pub(crate) fn expiry_text(expires_at: Option<&str>, now_millis: u64) -> String {
    let Some(until) = expires_at.and_then(iso_to_unix_millis) else {
        return "Never expires".to_owned();
    };
    if until <= now_millis {
        return "Expired".to_owned();
    }
    let minutes = (until - now_millis) / 60_000;
    let (amount, unit) = if minutes >= 1_440 {
        (minutes / 1_440, "day")
    } else if minutes >= 60 {
        (minutes / 60, "hour")
    } else {
        (minutes.max(1), "minute")
    };
    let plural = if amount == 1 { "" } else { "s" };
    format!("Expires in {amount} {unit}{plural}")
}

/// What an audit log entry did, as a sentence after the actor's name.
/// `name_of` turns an id (user, role or channel) into a name.
pub(crate) fn describe_audit_entry(
    entry: &Value,
    name_of: &dyn Fn(&str) -> Option<String>,
) -> String {
    let target = entry["target_id"]
        .as_str()
        .map(|id| name_of(id).unwrap_or_else(|| id.to_owned()))
        .unwrap_or_default();
    let changed = |key: &str| {
        entry["changes"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|change| change["key"] == key)
    };
    let named = |key: &str| {
        changed(key)
            .and_then(|change| {
                change["new_value"]
                    .as_str()
                    .or(change["old_value"].as_str())
            })
            .map(str::to_owned)
            .unwrap_or_else(|| target.clone())
    };
    let channel = || {
        entry
            .pointer("/options/channel_id")
            .and_then(Value::as_str)
            .map(|id| name_of(id).unwrap_or_else(|| id.to_owned()))
            .unwrap_or_default()
    };
    let text = match entry["action_type"].as_u64().unwrap_or(0) {
        1 => "updated the server".to_owned(),
        10 => format!("created channel {}", named("name")),
        11 => format!("updated channel {target}"),
        12 => format!("deleted channel {}", named("name")),
        13..=15 => format!("changed the permissions of channel {target}"),
        20 => format!("kicked {target}"),
        21 => "pruned inactive members".to_owned(),
        22 => format!("banned {target}"),
        23 => format!("unbanned {target}"),
        24 => {
            if let Some(change) = changed("communication_disabled_until") {
                if change["new_value"].is_null() {
                    format!("removed the timeout of {target}")
                } else {
                    format!("timed out {target}")
                }
            } else if changed("nick").is_some() {
                format!("changed the nickname of {target}")
            } else if changed("mute").is_some() {
                format!("changed the server mute of {target}")
            } else if changed("deaf").is_some() {
                format!("changed the server deafen of {target}")
            } else {
                format!("updated {target}")
            }
        }
        25 => {
            let roles = |key: &str| {
                changed(key)
                    .and_then(|change| change["new_value"].as_array())
                    .map(|roles| {
                        roles
                            .iter()
                            .filter_map(|role| role["name"].as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .filter(|names| !names.is_empty())
            };
            match (roles("$add"), roles("$remove")) {
                (Some(added), None) => format!("gave {target} {added}"),
                (None, Some(removed)) => format!("removed {removed} from {target}"),
                _ => format!("changed the roles of {target}"),
            }
        }
        26 => format!("moved members to {}", channel()),
        27 => "disconnected a member from voice".to_owned(),
        28 => format!("added the bot {target}"),
        30 => format!("created role {}", named("name")),
        31 => format!("updated role {target}"),
        32 => format!("deleted role {}", named("name")),
        40 => format!("created invite {}", named("code")),
        41 => format!("updated invite {}", named("code")),
        42 => format!("revoked invite {}", named("code")),
        50..=52 => "changed a webhook".to_owned(),
        60..=62 => "changed an emoji".to_owned(),
        72 => format!("deleted a message by {target} in {}", channel()),
        73 => format!("deleted messages in {target}"),
        74 => format!("pinned a message by {target}"),
        75 => format!("unpinned a message by {target}"),
        80..=82 => "changed an integration".to_owned(),
        83..=85 => "changed a stage".to_owned(),
        90..=92 => "changed a sticker".to_owned(),
        100..=102 => "changed an event".to_owned(),
        110..=112 => "changed a thread".to_owned(),
        other => format!("did something (action {other})"),
    };
    match entry["reason"].as_str().filter(|reason| !reason.is_empty()) {
        Some(reason) => format!("{text} — “{reason}”"),
        None => text,
    }
}

/// `(id, name)` pairs.
type IdNames = Vec<(String, String)>;

/// What the dialog keeps while open.
pub(crate) struct Panel {
    guild: Snowflake,
    toasts: adw::ToastOverlay,
    dialog: adw::Dialog,
    roles: gtk::Box,
    members: gtk::Box,
    member_query: RefCell<String>,
    bans: gtk::Box,
    ban_query: RefCell<String>,
    bans_loaded: RefCell<Vec<kc_http::Ban>>,
    invites: gtk::Box,
    audit: gtk::Box,
}

impl Panel {
    fn toast(&self, text: &str) {
        let toast = adw::Toast::new(text);
        toast.set_timeout(6);
        self.toasts.add_toast(toast);
    }

    fn fail(&self, what: &str, error: &kc_http::Error) {
        tracing::warn!(%error, what, "server settings request failed");
        let toast = adw::Toast::new(&format!("{what}: {}", error.describe()));
        toast.set_timeout(10);
        self.toasts.add_toast(toast);
    }
}

fn page_box() -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 14);
    page.set_margin_top(24);
    page.set_margin_bottom(32);
    page.set_margin_start(24);
    page.set_margin_end(24);
    page
}

fn page_scroll(page: &gtk::Box) -> gtk::ScrolledWindow {
    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(760);
    clamp.set_child(Some(page));
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_child(Some(&clamp));
    crate::settings_panel::dev_scroll(&scroll);
    scroll
}

fn section_title(text: &str) -> gtk::Label {
    let title = gtk::Label::new(Some(text));
    title.set_xalign(0.0);
    title.add_css_class("title-3");
    title
}

fn wrapped(text: &str, classes: &[&str]) -> gtk::Label {
    let text = label(text, classes);
    text.set_ellipsize(gtk::pango::EllipsizeMode::None);
    text.set_wrap(true);
    text.set_max_width_chars(1);
    text.set_hexpand(true);
    text
}

fn boxed_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list
}

fn placeholder_row(list: &gtk::ListBox, text: &str) {
    let row = adw::ActionRow::new();
    row.set_title(text);
    row.add_css_class("dim-label");
    list.append(&row);
}

fn string_list<T>(items: &[(&str, T)]) -> gtk::StringList {
    gtk::StringList::new(&items.iter().map(|(name, _)| *name).collect::<Vec<_>>())
}

impl AppState {
    /// Opens Server Settings on its first page the account may see, or on
    /// `page` when given and allowed.
    pub(crate) fn show_server_settings(
        self: &Rc<Self>,
        guild: Snowflake,
        page: Option<&str>,
    ) -> Option<Rc<Panel>> {
        let (name, granted) = self
            .with_session(|session| {
                session.store.read(|state| {
                    Some((
                        state.guilds.get(&guild)?.name.clone(),
                        state.guild_permissions(guild),
                    ))
                })
            })
            .flatten()?;
        let shown: Vec<&(&str, &str, &str, u64)> = PAGES
            .iter()
            .filter(|(_, _, _, needs)| granted & needs != 0)
            .collect();
        if shown.is_empty() {
            return None;
        }

        let dialog = adw::Dialog::new();
        dialog.set_title(&format!("{name} — Server Settings"));
        dialog.set_content_width(960);
        dialog.set_content_height(
            std::env::var("KUBECORD_WINDOW_HEIGHT")
                .ok()
                .and_then(|height| height.parse::<i32>().ok())
                .map_or(700, |height| height - 20),
        );
        dialog.set_width_request(360);
        dialog.set_height_request(400);

        let panel = Rc::new(Panel {
            guild,
            toasts: adw::ToastOverlay::new(),
            dialog: dialog.clone(),
            roles: gtk::Box::new(gtk::Orientation::Vertical, 0),
            members: gtk::Box::new(gtk::Orientation::Vertical, 0),
            member_query: RefCell::new(String::new()),
            bans: gtk::Box::new(gtk::Orientation::Vertical, 0),
            ban_query: RefCell::new(String::new()),
            bans_loaded: RefCell::new(Vec::new()),
            invites: gtk::Box::new(gtk::Orientation::Vertical, 0),
            audit: gtk::Box::new(gtk::Orientation::Vertical, 0),
        });

        let pages = gtk::Stack::new();
        pages.set_transition_type(gtk::StackTransitionType::Crossfade);
        for (id, _, _, _) in &shown {
            let content = match *id {
                "overview" => self.overview_page(&panel),
                "roles" => self.roles_page(&panel),
                "members" => self.members_page(&panel),
                "invites" => self.invites_page(&panel),
                "bans" => self.bans_page(&panel),
                _ => self.audit_page(&panel),
            };
            pages.add_named(&page_scroll(&content), Some(id));
        }

        let content_view = adw::ToolbarView::new();
        content_view.add_top_bar(&adw::HeaderBar::new());
        panel.toasts.set_child(Some(&pages));
        content_view.set_content(Some(&panel.toasts));
        let content_page = adw::NavigationPage::new(&content_view, shown[0].1);

        let categories = gtk::ListBox::new();
        categories.add_css_class("navigation-sidebar");
        for (_, title, icon, _) in &shown {
            let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            row_box.set_margin_top(6);
            row_box.set_margin_bottom(6);
            row_box.set_margin_start(4);
            row_box.append(&gtk::Image::from_icon_name(icon));
            row_box.append(&label(title, &[]));
            categories.append(&row_box);
        }
        let heading = name.to_uppercase();
        categories.set_header_func(move |row, _| {
            if row.index() != 0 {
                row.set_header(None::<&gtk::Widget>);
                return;
            }
            let header = label(&heading, &["caption-heading", "dim-label"]);
            header.set_margin_start(12);
            header.set_margin_top(6);
            header.set_margin_bottom(4);
            row.set_header(Some(&header));
        });

        let split = adw::NavigationSplitView::new();
        let sidebar_view = adw::ToolbarView::new();
        sidebar_view.add_top_bar(&adw::HeaderBar::new());
        sidebar_view.set_content(Some(&categories));
        split.set_sidebar(Some(&adw::NavigationPage::new(
            &sidebar_view,
            "Server Settings",
        )));
        split.set_content(Some(&content_page));
        split.set_min_sidebar_width(200.0);
        split.set_max_sidebar_width(240.0);
        split.set_show_content(true);

        let ids: Vec<(&'static str, &'static str)> = shown
            .iter()
            .map(|(id, title, _, _)| (*id, *title))
            .collect();
        let target_split = split.clone();
        let app = self.clone();
        let opened = panel.clone();
        let loaded: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        categories.connect_row_selected(move |_, row| {
            let Some((id, title)) = row.and_then(|row| ids.get(row.index() as usize)) else {
                return;
            };
            pages.set_visible_child_name(id);
            content_page.set_title(title);
            target_split.set_show_content(true);
            // Pages that come from the network load when first shown.
            if !loaded.borrow().contains(id) {
                loaded.borrow_mut().push(id);
                match *id {
                    "bans" => app.load_bans(&opened),
                    "invites" => app.load_invites(&opened),
                    "audit" => app.load_audit(&opened),
                    _ => {}
                }
            }
        });
        let initial = page
            .and_then(|page| shown.iter().position(|(id, _, _, _)| *id == page))
            .unwrap_or(0);
        if let Some(row) = categories.row_at_index(initial as i32) {
            categories.select_row(Some(&row));
            dialog.set_focus(Some(&row));
        }

        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            600.0,
            adw::LengthUnit::Sp,
        ));
        breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
        dialog.add_breakpoint(breakpoint);
        dialog.set_child(Some(&split));

        // Keep the lists current while the dialog is open.
        if let Some((_, _, store)) = self.active() {
            let mut events = store.subscribe();
            let app = self.clone();
            let weak = Rc::downgrade(&panel);
            let listener = glib::MainContext::default().spawn_local(async move {
                while let Ok(event) = events.recv().await {
                    let Some(panel) = weak.upgrade() else {
                        break;
                    };
                    match event {
                        StoreEvent::GuildUpdated(id) if id == guild => {
                            app.fill_roles(&panel);
                            app.fill_members(&panel);
                        }
                        StoreEvent::GuildMembersUpdated(id) if id == guild => {
                            app.fill_members(&panel)
                        }
                        StoreEvent::BansUpdated(id) if id == guild => app.load_bans(&panel),
                        _ => {}
                    }
                }
            });
            let holder = RefCell::new(Some(listener));
            let keep = panel.clone();
            dialog.connect_closed(move |_| {
                if let Some(listener) = holder.borrow_mut().take() {
                    listener.abort();
                }
                let _ = &keep;
            });
        }
        dialog.present(Some(&self.window));
        Some(panel)
    }

    // -- overview --------------------------------------------------------

    fn overview_page(self: &Rc<Self>, panel: &Rc<Panel>) -> gtk::Box {
        let page = page_box();
        page.append(&section_title("Server Overview"));
        let guild_id = panel.guild;
        let Some((guild, voice, text)) = self
            .with_session(|session| {
                session.store.read(|state| {
                    let guild = state.guilds.get(&guild_id)?.clone();
                    let mut channels: Vec<_> = state
                        .channels
                        .values()
                        .filter(|channel| channel.guild_id == Some(guild_id))
                        .cloned()
                        .collect();
                    channels.sort_by_key(|channel| channel.sort_key());
                    let of = |kind: ChannelType| -> Vec<(Snowflake, String)> {
                        channels
                            .iter()
                            .filter(|channel| channel.kind == kind)
                            .map(|channel| (channel.id, channel.display_name()))
                            .collect()
                    };
                    Some((
                        guild,
                        of(ChannelType::GuildVoice),
                        of(ChannelType::GuildText),
                    ))
                })
            })
            .flatten()
        else {
            return page;
        };

        // Icon.
        let icon_group = adw::PreferencesGroup::new();
        let icon_row = adw::ActionRow::new();
        icon_row.set_title("Server Icon");
        icon_row.set_subtitle("At least 512×512 is best. PNG, JPEG, GIF or WebP.");
        let icon_url = guild
            .icon
            .as_deref()
            .map(|hash| cdn::guild_icon(guild_id, hash, 128));
        let avatar = self.images.avatar(56, &guild.name, icon_url.as_deref());
        avatar.set_margin_top(6);
        avatar.set_margin_bottom(6);
        icon_row.add_prefix(&avatar);
        let upload = gtk::Button::with_label("Upload Image");
        upload.set_valign(gtk::Align::Center);
        let remove = gtk::Button::with_label("Remove");
        remove.set_valign(gtk::Align::Center);
        remove.add_css_class("flat");
        remove.set_visible(guild.icon.is_some());
        icon_row.add_suffix(&upload);
        icon_row.add_suffix(&remove);
        icon_group.add(&icon_row);
        page.append(&icon_group);
        {
            let app = self.clone();
            let target = panel.clone();
            upload.connect_clicked(move |_| app.choose_guild_icon(&target));
            let app = self.clone();
            let target = panel.clone();
            remove.connect_clicked(move |_| {
                app.patch_guild(&target, json!({ "icon": null }), "Icon removed");
            });
        }

        // Name and description.
        let about = adw::PreferencesGroup::new();
        let name = adw::EntryRow::new();
        name.set_title("Server Name");
        name.set_text(&guild.name);
        name.set_show_apply_button(true);
        let app = self.clone();
        let target = panel.clone();
        name.connect_apply(move |row| {
            let text = row.text().trim().to_owned();
            if text.chars().count() < 2 {
                target.toast("A server name needs at least 2 characters");
                return;
            }
            app.patch_guild(&target, json!({ "name": text }), "Server name saved");
        });
        about.add(&name);
        let description = adw::EntryRow::new();
        description.set_title("Description");
        description.set_text(guild.description.as_deref().unwrap_or(""));
        description.set_show_apply_button(true);
        let app = self.clone();
        let target = panel.clone();
        description.connect_apply(move |row| {
            let text = row.text().trim().to_owned();
            let value = if text.is_empty() {
                Value::Null
            } else {
                json!(text)
            };
            app.patch_guild(
                &target,
                json!({ "description": value }),
                "Description saved",
            );
        });
        about.add(&description);
        page.append(&about);

        // Inactive channel and timeout.
        let afk = adw::PreferencesGroup::new();
        afk.set_title("Inactive Channel");
        afk.set_description(Some(
            "Members idle in voice longer than the timeout are moved here and muted.",
        ));
        let mut afk_names = vec!["No Inactive Channel".to_owned()];
        afk_names.extend(voice.iter().map(|(_, name)| name.clone()));
        let afk_channel = adw::ComboRow::new();
        afk_channel.set_title("Inactive Channel");
        afk_channel.set_model(Some(&gtk::StringList::new(
            &afk_names.iter().map(String::as_str).collect::<Vec<_>>(),
        )));
        afk_channel.set_selected(
            guild
                .afk_channel_id
                .and_then(|id| voice.iter().position(|(channel, _)| *channel == id))
                .map_or(0, |index| index as u32 + 1),
        );
        let app = self.clone();
        let target = panel.clone();
        let voice_ids: Vec<Snowflake> = voice.iter().map(|(id, _)| *id).collect();
        afk_channel.connect_selected_notify(move |row| {
            let value = match row.selected() {
                0 => Value::Null,
                index => voice_ids
                    .get(index as usize - 1)
                    .map_or(Value::Null, |id| json!(id.to_string())),
            };
            app.patch_guild(
                &target,
                json!({ "afk_channel_id": value }),
                "Inactive channel saved",
            );
        });
        afk.add(&afk_channel);
        let afk_timeout = adw::ComboRow::new();
        afk_timeout.set_title("Inactive Timeout");
        afk_timeout.set_model(Some(&string_list(AFK_TIMEOUTS)));
        afk_timeout.set_selected(
            AFK_TIMEOUTS
                .iter()
                .position(|(_, seconds)| Some(*seconds) == guild.afk_timeout)
                .unwrap_or(1) as u32,
        );
        let app = self.clone();
        let target = panel.clone();
        afk_timeout.connect_selected_notify(move |row| {
            let seconds = AFK_TIMEOUTS
                .get(row.selected() as usize)
                .map_or(300, |(_, seconds)| *seconds);
            app.patch_guild(
                &target,
                json!({ "afk_timeout": seconds }),
                "Inactive timeout saved",
            );
        });
        afk.add(&afk_timeout);
        page.append(&afk);

        // System messages and notifications.
        let messages = adw::PreferencesGroup::new();
        messages.set_title("Messages and Notifications");
        messages.set_description(Some(
            "Where Discord posts welcome and boost messages, and what members who \
             have not chosen their own notification settings get.",
        ));
        let mut system_names = vec!["No System Messages".to_owned()];
        system_names.extend(text.iter().map(|(_, name)| format!("#{name}")));
        let system = adw::ComboRow::new();
        system.set_title("System Messages Channel");
        system.set_model(Some(&gtk::StringList::new(
            &system_names.iter().map(String::as_str).collect::<Vec<_>>(),
        )));
        system.set_selected(
            guild
                .system_channel_id
                .and_then(|id| text.iter().position(|(channel, _)| *channel == id))
                .map_or(0, |index| index as u32 + 1),
        );
        let app = self.clone();
        let target = panel.clone();
        let text_ids: Vec<Snowflake> = text.iter().map(|(id, _)| *id).collect();
        system.connect_selected_notify(move |row| {
            let value = match row.selected() {
                0 => Value::Null,
                index => text_ids
                    .get(index as usize - 1)
                    .map_or(Value::Null, |id| json!(id.to_string())),
            };
            app.patch_guild(
                &target,
                json!({ "system_channel_id": value }),
                "System messages channel saved",
            );
        });
        messages.add(&system);
        let notifications = adw::ComboRow::new();
        notifications.set_title("Default Notifications");
        notifications.set_model(Some(&gtk::StringList::new(&[
            "All Messages",
            "Only @mentions",
        ])));
        notifications.set_selected(u32::from(
            guild.default_message_notifications.unwrap_or(0).min(1),
        ));
        let app = self.clone();
        let target = panel.clone();
        notifications.connect_selected_notify(move |row| {
            app.patch_guild(
                &target,
                json!({ "default_message_notifications": row.selected() }),
                "Default notifications saved",
            );
        });
        messages.add(&notifications);
        page.append(&messages);
        page
    }

    fn patch_guild(self: &Rc<Self>, panel: &Rc<Panel>, body: Value, done: &'static str) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let guild = panel.guild;
        let panel = panel.clone();
        spawn_local(
            self,
            async move { http.edit_guild(guild, body).await },
            move |_, result| match result {
                Ok(_) => panel.toast(done),
                Err(error) => panel.fail("Could not save", &error),
            },
        );
    }

    fn choose_guild_icon(self: &Rc<Self>, panel: &Rc<Panel>) {
        let dialog = gtk::FileDialog::new();
        dialog.set_title("Server Icon");
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Images"));
        for mime in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
            filter.add_mime_type(mime);
        }
        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));
        let app = self.clone();
        let panel = panel.clone();
        dialog.open(
            Some(&self.window),
            gtk::gio::Cancellable::NONE,
            move |result| {
                let Some(path) = result.ok().and_then(|file| file.path()) else {
                    return;
                };
                let Some((_, http, _)) = app.active() else {
                    return;
                };
                let guild = panel.guild;
                let panel = panel.clone();
                spawn_local(
                    &app,
                    async move {
                        let bytes = tokio::fs::read(&path)
                            .await
                            .map_err(|error| error.to_string())?;
                        if bytes.len() > 10 * 1024 * 1024 {
                            return Err("the image is larger than 10 MB".to_owned());
                        }
                        http.edit_guild(guild, json!({ "icon": kc_http::image_data_uri(&bytes) }))
                            .await
                            .map_err(|error| error.describe())
                    },
                    move |_, result| match result {
                        Ok(_) => panel.toast("Server icon saved"),
                        Err(error) => panel.toast(&format!("Could not set the icon: {error}")),
                    },
                );
            },
        );
    }

    // -- roles -----------------------------------------------------------

    fn roles_page(self: &Rc<Self>, panel: &Rc<Panel>) -> gtk::Box {
        let page = page_box();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let title = section_title("Roles");
        title.set_hexpand(true);
        header.append(&title);
        let create = gtk::Button::with_label("Create Role");
        create.add_css_class("suggested-action");
        create.add_css_class("pill");
        let app = self.clone();
        let target = panel.clone();
        create.connect_clicked(move |_| app.create_role(&target));
        header.append(&create);
        page.append(&header);
        page.append(&wrapped(
            "Members get the permissions of all their roles. Roles higher in the list win \
             where they differ; you can only edit, assign and move roles below your highest role.",
            &["dim-label"],
        ));
        page.append(&panel.roles);
        self.fill_roles(panel);
        page
    }

    fn fill_roles(self: &Rc<Self>, panel: &Rc<Panel>) {
        clear_box(&panel.roles);
        let guild = panel.guild;
        let rows: Vec<(Role, bool, usize)> = self
            .with_session(|session| {
                session.store.read(|state| {
                    state
                        .roles_by_rank(guild)
                        .into_iter()
                        .map(|role| {
                            let count = state
                                .members
                                .iter()
                                .filter(|((member_guild, _), member)| {
                                    *member_guild == guild && member.roles.contains(&role.id)
                                })
                                .count();
                            let manageable = state.can_manage_role(guild, &role);
                            (role, manageable, count)
                        })
                        .collect()
                })
            })
            .unwrap_or_default();
        let ranked: Rc<Vec<Role>> = Rc::new(
            rows.iter()
                .map(|(role, _, _)| role.clone())
                .filter(|role| role.id != guild)
                .collect(),
        );
        let list = boxed_list();
        for (role, manageable, count) in &rows {
            let everyone = role.id == guild;
            let row = adw::ActionRow::new();
            row.set_title(&glib::markup_escape_text(&role.name));
            let mut details = Vec::new();
            if role.managed {
                details.push("Managed by an integration".to_owned());
            }
            if !everyone {
                details.push(format!(
                    "{count} known member{}",
                    if *count == 1 { "" } else { "s" }
                ));
            } else {
                details.push("Default permissions for everyone".to_owned());
            }
            if !manageable {
                details.push("Above your highest role".to_owned());
            }
            row.set_subtitle(&details.join(" · "));
            row.add_prefix(&color_dot(role.color));
            if !everyone {
                let index = ranked.iter().position(|ranked| ranked.id == role.id);
                for up in [true, false] {
                    let button = gtk::Button::from_icon_name(if up {
                        "go-up-symbolic"
                    } else {
                        "go-down-symbolic"
                    });
                    button.add_css_class("flat");
                    button.set_valign(gtk::Align::Center);
                    button.set_tooltip_text(Some(if up { "Move Up" } else { "Move Down" }));
                    // Both roles must be below yours.
                    let neighbour = index.and_then(|index| {
                        if up {
                            index.checked_sub(1)
                        } else {
                            Some(index + 1)
                        }
                    });
                    let neighbour_ok =
                        neighbour
                            .and_then(|other| ranked.get(other))
                            .is_some_and(|other| {
                                rows.iter()
                                    .any(|(role, manageable, _)| role.id == other.id && *manageable)
                            });
                    button.set_sensitive(*manageable && neighbour_ok);
                    let app = self.clone();
                    let target = panel.clone();
                    let ranked = ranked.clone();
                    button.connect_clicked(move |_| {
                        if let Some(index) = index
                            && let Some(positions) = swap_positions(&ranked, index, up)
                        {
                            app.reorder_roles(&target, positions);
                        }
                    });
                    row.add_suffix(&button);
                }
            }
            if *manageable {
                row.set_activatable(true);
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                let app = self.clone();
                let target = panel.clone();
                let role_id = role.id;
                row.connect_activated(move |_| app.role_editor(&target, role_id));
            } else {
                row.add_css_class("dim-label");
            }
            list.append(&row);
        }
        panel.roles.append(&list);
    }

    fn reorder_roles(self: &Rc<Self>, panel: &Rc<Panel>, positions: Vec<(Snowflake, i32)>) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let guild = panel.guild;
        let panel = panel.clone();
        spawn_local(
            self,
            async move { http.reorder_roles(guild, &positions).await },
            move |_, result| {
                if let Err(error) = result {
                    panel.fail("Could not move the role", &error);
                }
            },
        );
    }

    fn create_role(self: &Rc<Self>, panel: &Rc<Panel>) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let guild = panel.guild;
        let panel = panel.clone();
        spawn_local(
            self,
            async move {
                http.create_role(
                    guild,
                    &RoleEdit {
                        name: Some("new role".to_owned()),
                        ..RoleEdit::default()
                    },
                )
                .await
            },
            move |app, result| match result {
                Ok(role) => {
                    // The gateway's GUILD_ROLE_CREATE may come later.
                    if let Some((_, _, store)) = app.active() {
                        store.write(|state| {
                            if let Some(guild) = state.guilds.get_mut(&guild)
                                && !guild.roles.iter().any(|known| known.id == role.id)
                            {
                                guild.roles.push(role.clone());
                            }
                        });
                    }
                    app.fill_roles(&panel);
                    app.role_editor(&panel, role.id);
                }
                Err(error) => panel.fail("Could not create a role", &error),
            },
        );
    }

    /// Edits one role: name, colour, display options and permissions. Each
    /// change is sent at once.
    pub(crate) fn role_editor(self: &Rc<Self>, panel: &Rc<Panel>, role_id: Snowflake) {
        let guild = panel.guild;
        let Some((role, granted)) = self
            .with_session(|session| {
                session.store.read(|state| {
                    Some((
                        state.role(guild, role_id)?.clone(),
                        state.guild_permissions(guild),
                    ))
                })
            })
            .flatten()
        else {
            return;
        };
        let everyone = role.id == guild;
        let dialog = adw::Dialog::new();
        dialog.set_title(&format!("Edit Role — {}", role.name));
        dialog.set_content_width(560);
        dialog.set_content_height(640);
        let toasts = adw::ToastOverlay::new();
        let page = page_box();
        let editor = Rc::new(RoleEditor {
            guild,
            role: role_id,
            toasts: toasts.clone(),
            permissions: Cell::new(role.permissions),
            reverting: Cell::new(false),
        });

        if !everyone {
            let display = adw::PreferencesGroup::new();
            display.set_title("Display");
            let name = adw::EntryRow::new();
            name.set_title("Role Name");
            name.set_text(&role.name);
            name.set_show_apply_button(true);
            let app = self.clone();
            let target = editor.clone();
            let shown = dialog.downgrade();
            name.connect_apply(move |row| {
                let text = row.text().trim().to_owned();
                if text.is_empty() {
                    return;
                }
                if let Some(dialog) = shown.upgrade() {
                    dialog.set_title(&format!("Edit Role — {text}"));
                }
                app.edit_role(
                    &target,
                    RoleEdit {
                        name: Some(text),
                        ..RoleEdit::default()
                    },
                    None,
                );
            });
            display.add(&name);

            let color_row = adw::ActionRow::new();
            color_row.set_title("Role Colour");
            color_row.set_subtitle("Members use the colour of their highest coloured role.");
            let color_button = gtk::ColorDialogButton::new(Some(gtk::ColorDialog::new()));
            color_button.set_valign(gtk::Align::Center);
            let rgba = |color: u32| {
                gtk::gdk::RGBA::new(
                    ((color >> 16) & 0xff) as f32 / 255.0,
                    ((color >> 8) & 0xff) as f32 / 255.0,
                    (color & 0xff) as f32 / 255.0,
                    1.0,
                )
            };
            color_button.set_rgba(&rgba(if role.color == 0 {
                0x99aab5
            } else {
                role.color
            }));
            let clear = gtk::Button::from_icon_name("edit-clear-symbolic");
            clear.set_tooltip_text(Some("No Colour"));
            clear.add_css_class("flat");
            clear.set_valign(gtk::Align::Center);
            color_row.add_suffix(&clear);
            color_row.add_suffix(&color_button);
            let app = self.clone();
            let target = editor.clone();
            color_button.connect_rgba_notify(move |button| {
                if target.reverting.get() {
                    return;
                }
                let rgba = button.rgba();
                let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
                let color =
                    channel(rgba.red()) << 16 | channel(rgba.green()) << 8 | channel(rgba.blue());
                app.edit_role(
                    &target,
                    RoleEdit {
                        color: Some(color),
                        ..RoleEdit::default()
                    },
                    None,
                );
            });
            let app = self.clone();
            let target = editor.clone();
            clear.connect_clicked(move |_| {
                app.edit_role(
                    &target,
                    RoleEdit {
                        color: Some(0),
                        ..RoleEdit::default()
                    },
                    None,
                );
            });
            display.add(&color_row);

            for (title, subtitle, initial, hoist) in [
                (
                    "Display Separately",
                    "Show members with this role in their own group in the member list.",
                    role.hoist,
                    true,
                ),
                (
                    "Allow Anyone to @mention This Role",
                    "Members without Mention Everyone can still ping it.",
                    role.mentionable,
                    false,
                ),
            ] {
                let row = adw::SwitchRow::new();
                row.set_title(title);
                row.set_subtitle(subtitle);
                row.set_active(initial);
                let app = self.clone();
                let target = editor.clone();
                row.connect_active_notify(move |row| {
                    if target.reverting.get() {
                        return;
                    }
                    let on = row.is_active();
                    let edit = if hoist {
                        RoleEdit {
                            hoist: Some(on),
                            ..RoleEdit::default()
                        }
                    } else {
                        RoleEdit {
                            mentionable: Some(on),
                            ..RoleEdit::default()
                        }
                    };
                    let row = row.downgrade();
                    app.edit_role(
                        &target,
                        edit,
                        Some(Box::new(move || {
                            if let Some(row) = row.upgrade() {
                                row.set_active(!on);
                            }
                        })),
                    );
                });
                display.add(&row);
            }
            page.append(&display);
        }

        for (group_title, group) in GROUPS {
            let group_box = adw::PreferencesGroup::new();
            group_box.set_title(group_title);
            for permission in *group {
                let row = adw::SwitchRow::new();
                row.set_title(permission.name);
                row.set_subtitle(permission.description);
                row.set_active(role.permissions & permission.bit != 0);
                // Discord refuses to grant what you do not have yourself.
                if granted & permission.bit == 0 {
                    row.set_sensitive(false);
                    row.set_tooltip_text(Some("You don't have this permission yourself"));
                }
                let bit = permission.bit;
                let app = self.clone();
                let target = editor.clone();
                row.connect_active_notify(move |row| {
                    if target.reverting.get() {
                        return;
                    }
                    let on = row.is_active();
                    let before = target.permissions.get();
                    let after = if on { before | bit } else { before & !bit };
                    target.permissions.set(after);
                    let row = row.downgrade();
                    let undo = target.clone();
                    app.edit_role(
                        &target,
                        RoleEdit {
                            permissions: Some(after),
                            ..RoleEdit::default()
                        },
                        Some(Box::new(move || {
                            undo.permissions.set(undo.permissions.get() ^ bit);
                            if let Some(row) = row.upgrade() {
                                row.set_active(!on);
                            }
                        })),
                    );
                });
                group_box.add(&row);
            }
            page.append(&group_box);
        }

        if !everyone {
            let delete = gtk::Button::with_label("Delete Role");
            delete.add_css_class("destructive-action");
            delete.add_css_class("pill");
            delete.set_halign(gtk::Align::Center);
            delete.set_margin_top(12);
            let app = self.clone();
            let target = panel.clone();
            let shown = dialog.downgrade();
            let name = role.name.clone();
            delete.connect_clicked(move |_| {
                app.confirm_delete_role(&target, role_id, &name, shown.clone());
            });
            page.append(&delete);
        }

        let scroll = page_scroll(&page);
        toasts.set_child(Some(&scroll));
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&toasts));
        dialog.set_child(Some(&view));
        dialog.present(Some(&panel.dialog));
        // Focus lands in the name field, selecting it; start unfocused.
        let shown = dialog.downgrade();
        glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
            if let Some(dialog) = shown.upgrade() {
                dialog.set_focus(None::<&gtk::Widget>);
            }
        });
    }

    /// Sends a role edit; `undo` puts the control back if Discord refuses.
    fn edit_role(
        self: &Rc<Self>,
        editor: &Rc<RoleEditor>,
        edit: RoleEdit,
        undo: Option<Box<dyn FnOnce()>>,
    ) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let (guild, role) = (editor.guild, editor.role);
        let editor = editor.clone();
        spawn_local(
            self,
            async move { http.edit_role(guild, role, &edit).await },
            move |_, result| {
                if let Err(error) = result {
                    tracing::warn!(%error, "role edit failed");
                    let toast = adw::Toast::new(&format!("Could not save: {}", error.describe()));
                    toast.set_timeout(10);
                    editor.toasts.add_toast(toast);
                    if let Some(undo) = undo {
                        editor.reverting.set(true);
                        undo();
                        editor.reverting.set(false);
                    }
                }
            },
        );
    }

    fn confirm_delete_role(
        self: &Rc<Self>,
        panel: &Rc<Panel>,
        role: Snowflake,
        name: &str,
        editor: glib::WeakRef<adw::Dialog>,
    ) {
        let dialog = adw::AlertDialog::new(
            Some(&format!("Delete {name}")),
            Some("Are you sure you want to delete this role? This cannot be undone."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete Role")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let host = panel.dialog.clone();
        let app = self.clone();
        let panel = panel.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "delete" {
                return;
            }
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let guild = panel.guild;
            let panel = panel.clone();
            let editor = editor.clone();
            spawn_local(
                &app,
                async move { http.delete_role(guild, role).await },
                move |_, result| match result {
                    Ok(()) => {
                        if let Some(editor) = editor.upgrade() {
                            editor.close();
                        }
                        panel.toast("Role deleted");
                    }
                    Err(error) => panel.fail("Could not delete the role", &error),
                },
            );
        });
        dialog.present(Some(&host));
    }

    // -- members ---------------------------------------------------------

    fn members_page(self: &Rc<Self>, panel: &Rc<Panel>) -> gtk::Box {
        let page = page_box();
        page.append(&section_title("Server Members"));
        page.append(&wrapped(
            "Members kubecord knows about. Search to ask Discord for more; \
             use the menu on a member to moderate them.",
            &["dim-label"],
        ));
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search members"));
        let app = self.clone();
        let target = panel.clone();
        search.connect_search_changed(move |entry| {
            let query = entry.text().trim().to_owned();
            *target.member_query.borrow_mut() = query.clone();
            if !query.is_empty()
                && let Some((account, _, _)) = app.active()
                && let Some(gateway) = crate::session::gateway(account)
            {
                gateway.search_members(target.guild, &query, 100);
            }
            app.fill_members(&target);
        });
        page.append(&search);
        page.append(&panel.members);
        self.fill_members(panel);
        page
    }

    fn fill_members(self: &Rc<Self>, panel: &Rc<Panel>) {
        clear_box(&panel.members);
        let guild = panel.guild;
        let query = panel.member_query.borrow().to_lowercase();
        let members: Vec<(Snowflake, String, String, Option<String>, String)> = self
            .with_session(|session| {
                session.store.read(|state| {
                    let mut members: Vec<_> = state
                        .guild_member_users(guild)
                        .into_iter()
                        .filter_map(|user| {
                            let name = state.display_name(Some(guild), &user);
                            let matches = query.is_empty()
                                || name.to_lowercase().contains(&query)
                                || user.username.to_lowercase().contains(&query);
                            if !matches {
                                return None;
                            }
                            let member = state.member(guild, user.id);
                            let roles = state
                                .roles_by_rank(guild)
                                .into_iter()
                                .filter(|role| {
                                    member.is_some_and(|member| member.roles.contains(&role.id))
                                })
                                .map(|role| role.name)
                                .collect::<Vec<_>>()
                                .join(", ");
                            let timed_out =
                                member.is_some_and(|member| member.timed_out_at(unix_millis_now()));
                            let detail = match (roles.is_empty(), timed_out) {
                                (_, true) if roles.is_empty() => "Timed out".to_owned(),
                                (_, true) => format!("Timed out · {roles}"),
                                (true, false) => user.username.clone(),
                                (false, false) => roles,
                            };
                            Some((
                                user.id,
                                name,
                                user.username.clone(),
                                Some(cdn::user_avatar(user.id, user.avatar.as_deref(), 64)),
                                detail,
                            ))
                        })
                        .collect();
                    members.sort_by_key(|(_, name, _, _, _)| name.to_lowercase());
                    members.truncate(200);
                    members
                })
            })
            .unwrap_or_default();
        let list = boxed_list();
        if members.is_empty() {
            placeholder_row(&list, "No members found");
        }
        for (user_id, name, username, avatar, detail) in members {
            let row = adw::ActionRow::new();
            row.set_title(&glib::markup_escape_text(&name));
            row.set_subtitle(&glib::markup_escape_text(&detail));
            row.set_tooltip_text(Some(&username));
            row.add_prefix(&self.images.avatar(32, &name, avatar.as_deref()));
            let more = gtk::Button::from_icon_name("view-more-symbolic");
            more.add_css_class("flat");
            more.set_valign(gtk::Align::Center);
            more.set_tooltip_text(Some("Member Actions"));
            let app = self.clone();
            // Act on this dialog's server, not whatever is selected behind
            // the (non-modal) settings window.
            let guild = panel.guild;
            more.connect_clicked(move |button| {
                let anchor = button.clone().upcast::<gtk::Widget>();
                app.show_user_menu_in(user_id, &anchor, 16.0, 16.0, Some(guild));
            });
            row.add_suffix(&more);
            self.attach_user_menu_in(&row, user_id, Some(panel.guild));
            list.append(&row);
        }
        panel.members.append(&list);
    }

    // -- bans ------------------------------------------------------------

    fn bans_page(self: &Rc<Self>, panel: &Rc<Panel>) -> gtk::Box {
        let page = page_box();
        page.append(&section_title("Bans"));
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search bans by username or id"));
        let app = self.clone();
        let target = panel.clone();
        search.connect_search_changed(move |entry| {
            *target.ban_query.borrow_mut() = entry.text().trim().to_lowercase();
            app.fill_bans(&target);
        });
        page.append(&search);
        let loading = boxed_list();
        placeholder_row(&loading, "Loading…");
        panel.bans.append(&loading);
        page.append(&panel.bans);
        page
    }

    fn load_bans(self: &Rc<Self>, panel: &Rc<Panel>) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let guild = panel.guild;
        if let Some(bans) = crate::demo::fake_bans() {
            *panel.bans_loaded.borrow_mut() = bans;
            self.fill_bans(panel);
            return;
        }
        let panel = panel.clone();
        spawn_local(
            self,
            async move { http.bans(guild).await },
            move |app, result| match result {
                Ok(bans) => {
                    *panel.bans_loaded.borrow_mut() = bans;
                    app.fill_bans(&panel);
                }
                Err(error) => {
                    clear_box(&panel.bans);
                    let list = boxed_list();
                    placeholder_row(&list, &format!("Could not load bans: {}", error.describe()));
                    panel.bans.append(&list);
                }
            },
        );
    }

    fn fill_bans(self: &Rc<Self>, panel: &Rc<Panel>) {
        clear_box(&panel.bans);
        let query = panel.ban_query.borrow().clone();
        let bans = panel.bans_loaded.borrow().clone();
        let list = boxed_list();
        let shown: Vec<_> = bans
            .into_iter()
            .filter(|ban| {
                query.is_empty()
                    || ban.user.username.to_lowercase().contains(&query)
                    || ban.user.display_name().to_lowercase().contains(&query)
                    || ban.user.id.to_string() == query
            })
            .collect();
        if shown.is_empty() {
            placeholder_row(
                &list,
                if query.is_empty() {
                    "No bans"
                } else {
                    "No bans match"
                },
            );
        }
        for ban in shown {
            let row = adw::ActionRow::new();
            let name = ban.user.display_name().to_owned();
            row.set_title(&glib::markup_escape_text(&name));
            row.set_subtitle(&glib::markup_escape_text(&format!(
                "{} · {}",
                ban.user.username,
                ban.reason.as_deref().unwrap_or("No reason given")
            )));
            row.add_prefix(&self.images.avatar(
                32,
                &name,
                Some(&cdn::user_avatar(
                    ban.user.id,
                    ban.user.avatar.as_deref(),
                    64,
                )),
            ));
            let revoke = gtk::Button::with_label("Revoke Ban");
            revoke.set_valign(gtk::Align::Center);
            let app = self.clone();
            let target = panel.clone();
            let user = ban.user.id;
            revoke.connect_clicked(move |_| app.confirm_unban(&target, user, &name));
            row.add_suffix(&revoke);
            list.append(&row);
        }
        panel.bans.append(&list);
    }

    fn confirm_unban(self: &Rc<Self>, panel: &Rc<Panel>, user: Snowflake, name: &str) {
        let dialog = adw::AlertDialog::new(
            Some(&format!("Revoke the Ban of {name}")),
            Some("They will be able to join again with an invite."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("unban", "Revoke Ban")]);
        dialog.set_response_appearance("unban", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("unban"));
        dialog.set_close_response("cancel");
        let host = panel.dialog.clone();
        let app = self.clone();
        let panel = panel.clone();
        let name = name.to_owned();
        dialog.connect_response(None, move |_, response| {
            if response != "unban" {
                return;
            }
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let guild = panel.guild;
            let panel = panel.clone();
            let name = name.clone();
            spawn_local(
                &app,
                async move { http.unban_member(guild, user, None).await },
                move |app, result| match result {
                    Ok(()) => {
                        panel.toast(&format!("{name} is no longer banned"));
                        panel
                            .bans_loaded
                            .borrow_mut()
                            .retain(|ban| ban.user.id != user);
                        app.fill_bans(&panel);
                    }
                    Err(error) => panel.fail("Could not revoke the ban", &error),
                },
            );
        });
        dialog.present(Some(&host));
    }

    // -- invites ---------------------------------------------------------

    fn invites_page(self: &Rc<Self>, panel: &Rc<Panel>) -> gtk::Box {
        let page = page_box();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let title = section_title("Invites");
        title.set_hexpand(true);
        header.append(&title);
        let create = gtk::Button::with_label("Create Invite");
        create.add_css_class("pill");
        let app = self.clone();
        let guild = panel.guild;
        create.connect_clicked(move |_| app.invite_dialog(guild));
        header.append(&create);
        page.append(&header);
        let loading = boxed_list();
        placeholder_row(&loading, "Loading…");
        panel.invites.append(&loading);
        page.append(&panel.invites);
        page
    }

    fn load_invites(self: &Rc<Self>, panel: &Rc<Panel>) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let guild = panel.guild;
        if let Some(invites) = crate::demo::fake_invites() {
            self.fill_invites(panel, invites);
            return;
        }
        let panel = panel.clone();
        spawn_local(
            self,
            async move { http.guild_invites(guild).await },
            move |app, result| match result {
                Ok(invites) => app.fill_invites(&panel, invites),
                Err(error) => {
                    clear_box(&panel.invites);
                    let list = boxed_list();
                    placeholder_row(
                        &list,
                        &format!("Could not load invites: {}", error.describe()),
                    );
                    panel.invites.append(&list);
                }
            },
        );
    }

    fn fill_invites(self: &Rc<Self>, panel: &Rc<Panel>, invites: Vec<Invite>) {
        clear_box(&panel.invites);
        let list = boxed_list();
        if invites.is_empty() {
            placeholder_row(&list, "No invites");
        }
        let now = unix_millis_now();
        for invite in invites {
            let row = adw::ActionRow::new();
            row.set_title(&invite.code);
            let inviter = invite
                .inviter
                .as_ref()
                .map(|user| user.display_name().to_owned())
                .unwrap_or_else(|| "Unknown".to_owned());
            let channel = invite
                .channel
                .as_ref()
                .and_then(|channel| channel.name.clone())
                .map(|name| format!("#{name}"))
                .unwrap_or_default();
            let uses = if invite.max_uses == 0 {
                format!("{} uses", invite.uses)
            } else {
                format!("{}/{} uses", invite.uses, invite.max_uses)
            };
            row.set_subtitle(&glib::markup_escape_text(&format!(
                "{inviter} · {channel} · {uses} · {}",
                expiry_text(invite.expires_at.as_deref(), now)
            )));
            let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
            copy.add_css_class("flat");
            copy.set_valign(gtk::Align::Center);
            copy.set_tooltip_text(Some("Copy Link"));
            let link = format!("https://discord.gg/{}", invite.code);
            let target = panel.clone();
            copy.connect_clicked(move |button| {
                button.clipboard().set_text(&link);
                target.toast("Invite link copied");
            });
            row.add_suffix(&copy);
            let revoke = gtk::Button::with_label("Revoke");
            revoke.add_css_class("flat");
            revoke.add_css_class("error");
            revoke.set_valign(gtk::Align::Center);
            let app = self.clone();
            let target = panel.clone();
            let code = invite.code.clone();
            revoke.connect_clicked(move |_| {
                let Some((_, http, _)) = app.active() else {
                    return;
                };
                let code = code.clone();
                let target = target.clone();
                spawn_local(
                    &app,
                    async move { http.delete_invite(&code, None).await },
                    move |app, result| match result {
                        Ok(()) => {
                            target.toast("Invite revoked");
                            app.load_invites(&target);
                        }
                        Err(error) => target.fail("Could not revoke the invite", &error),
                    },
                );
            });
            row.add_suffix(&revoke);
            list.append(&row);
        }
        panel.invites.append(&list);
    }

    /// Invite People: choose how long the link lasts and how often it can
    /// be used, then create and copy it.
    pub(crate) fn invite_dialog(self: &Rc<Self>, guild: Snowflake) {
        let channels: Vec<(Snowflake, String)> = self
            .with_session(|session| {
                let selected = session.selected_channel;
                session.store.read(|state| {
                    let mut channels: Vec<_> = state
                        .channels
                        .values()
                        .filter(|channel| {
                            channel.guild_id == Some(guild)
                                && !channel.kind.is_category()
                                && !channel.kind.is_thread()
                                && state.can(channel.id, permissions::VIEW_CHANNEL)
                                && state.can(channel.id, permissions::CREATE_INSTANT_INVITE)
                        })
                        .collect();
                    channels.sort_by_key(|channel| {
                        (
                            Some(channel.id) != selected,
                            !channel.kind.is_text(),
                            channel.sort_key(),
                        )
                    });
                    channels
                        .into_iter()
                        .map(|channel| {
                            let prefix = if channel.kind.is_text() { "#" } else { "🔊 " };
                            (channel.id, format!("{prefix}{}", channel.display_name()))
                        })
                        .collect()
                })
            })
            .unwrap_or_default();
        if channels.is_empty() {
            self.set_status("You can't create invites in any channel here");
            return;
        }
        let dialog = adw::Dialog::new();
        dialog.set_title("Invite People");
        dialog.set_content_width(420);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.set_margin_top(12);
        body.set_margin_bottom(24);
        body.set_margin_start(24);
        body.set_margin_end(24);
        body.append(&wrapped(
            "Create a link others can use to join this server.",
            &["dim-label"],
        ));
        let caption = |text: &str| {
            let caption = label(text, &["caption-heading"]);
            caption.set_margin_top(8);
            caption
        };
        body.append(&caption("Channel"));
        let names: Vec<&str> = channels.iter().map(|(_, name)| name.as_str()).collect();
        let channel = gtk::DropDown::from_strings(&names);
        body.append(&channel);
        body.append(&caption("Expire After"));
        let age = gtk::DropDown::new(Some(string_list(INVITE_AGES)), gtk::Expression::NONE);
        age.set_selected(5);
        body.append(&age);
        body.append(&caption("Max Number of Uses"));
        let uses = gtk::DropDown::new(Some(string_list(INVITE_USES)), gtk::Expression::NONE);
        body.append(&uses);
        let link = gtk::Entry::new();
        link.set_editable(false);
        link.set_margin_top(14);
        link.set_visible(false);
        link.set_secondary_icon_name(Some("edit-copy-symbolic"));
        link.set_secondary_icon_tooltip_text(Some("Copy"));
        link.connect_icon_press(|entry, _| {
            entry.clipboard().set_text(&entry.text());
        });
        body.append(&link);
        let create = gtk::Button::with_label("Create Link");
        create.add_css_class("suggested-action");
        create.add_css_class("pill");
        create.set_halign(gtk::Align::Center);
        create.set_margin_top(14);
        body.append(&create);
        let app = self.clone();
        create.connect_clicked(move |button| {
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let Some(channel_id) = channels.get(channel.selected() as usize).map(|(id, _)| *id)
            else {
                return;
            };
            let max_age = INVITE_AGES
                .get(age.selected() as usize)
                .map_or(86_400, |(_, seconds)| *seconds);
            let max_uses = INVITE_USES
                .get(uses.selected() as usize)
                .map_or(0, |(_, count)| *count);
            button.set_sensitive(false);
            let button = button.downgrade();
            let link = link.downgrade();
            spawn_local(
                &app,
                async move { http.create_invite_with(channel_id, max_age, max_uses).await },
                move |app, result| {
                    if let Some(button) = button.upgrade() {
                        button.set_sensitive(true);
                    }
                    match result {
                        Ok(code) if !code.is_empty() => {
                            let url = format!("https://discord.gg/{code}");
                            if let Some(link) = link.upgrade() {
                                link.set_text(&url);
                                link.set_visible(true);
                                link.clipboard().set_text(&url);
                            }
                            app.set_status("Invite link copied");
                        }
                        Ok(_) => app.set_status("Discord returned no invite"),
                        Err(error) => app.report_failure("Could not create an invite", &error),
                    }
                },
            );
        });
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&body));
        dialog.set_child(Some(&view));
        dialog.present(Some(&self.window));
    }

    // -- audit log -------------------------------------------------------

    fn audit_page(self: &Rc<Self>, panel: &Rc<Panel>) -> gtk::Box {
        let page = page_box();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let title = section_title("Audit Log");
        title.set_hexpand(true);
        header.append(&title);
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.add_css_class("flat");
        refresh.set_tooltip_text(Some("Refresh"));
        let app = self.clone();
        let target = panel.clone();
        refresh.connect_clicked(move |_| app.load_audit(&target));
        header.append(&refresh);
        page.append(&header);
        page.append(&wrapped(
            "The last 50 changes made in this server.",
            &["dim-label"],
        ));
        let loading = boxed_list();
        placeholder_row(&loading, "Loading…");
        panel.audit.append(&loading);
        page.append(&panel.audit);
        page
    }

    fn load_audit(self: &Rc<Self>, panel: &Rc<Panel>) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let guild = panel.guild;
        if let Some(log) = crate::demo::fake_audit_log() {
            self.fill_audit(panel, &log);
            return;
        }
        let panel = panel.clone();
        spawn_local(
            self,
            async move { http.audit_log(guild, 50).await },
            move |app, result| match result {
                Ok(log) => app.fill_audit(&panel, &log),
                Err(error) => {
                    clear_box(&panel.audit);
                    let list = boxed_list();
                    placeholder_row(
                        &list,
                        &format!("Could not load the audit log: {}", error.describe()),
                    );
                    panel.audit.append(&list);
                }
            },
        );
    }

    fn fill_audit(self: &Rc<Self>, panel: &Rc<Panel>, log: &Value) {
        clear_box(&panel.audit);
        let guild = panel.guild;
        let users: Vec<kc_state::User> = log["users"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|user| serde_json::from_value(user.clone()).ok())
            .collect();
        let (roles, channels): (IdNames, IdNames) = self
            .with_session(|session| {
                session.store.read(|state| {
                    (
                        state
                            .roles_by_rank(guild)
                            .into_iter()
                            .map(|role| (role.id.to_string(), role.name))
                            .collect(),
                        state
                            .channels
                            .values()
                            .filter(|channel| channel.guild_id == Some(guild))
                            .map(|channel| (channel.id.to_string(), channel.display_name()))
                            .collect(),
                    )
                })
            })
            .unwrap_or_default();
        let name_of = |id: &str| -> Option<String> {
            users
                .iter()
                .find(|user| user.id.to_string() == id)
                .map(|user| user.display_name().to_owned())
                .or_else(|| {
                    roles
                        .iter()
                        .find(|(role, _)| role == id)
                        .map(|(_, name)| name.clone())
                })
                .or_else(|| {
                    channels
                        .iter()
                        .find(|(channel, _)| channel == id)
                        .map(|(_, name)| format!("#{name}"))
                })
        };
        let list = boxed_list();
        let entries = log["audit_log_entries"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if entries.is_empty() {
            placeholder_row(&list, "Nothing has been logged yet");
        }
        for entry in entries {
            let actor = entry["user_id"]
                .as_str()
                .and_then(name_of)
                .unwrap_or_else(|| "Someone".to_owned());
            let when = entry["id"]
                .as_str()
                .and_then(|id| id.parse::<Snowflake>().ok())
                .map(|id| {
                    crate::content::format_time(&format!(
                        "{}+00:00",
                        kc_state::time::millis_to_iso(id.timestamp())
                    ))
                })
                .unwrap_or_default();
            let row = adw::ActionRow::new();
            row.set_title(&glib::markup_escape_text(&format!(
                "{actor} {}",
                describe_audit_entry(&entry, &name_of)
            )));
            row.set_title_lines(3);
            row.set_subtitle(&when);
            if let Some(user) = entry["user_id"]
                .as_str()
                .and_then(|id| users.iter().find(|user| user.id.to_string() == id))
            {
                row.add_prefix(&self.images.avatar(
                    32,
                    user.display_name(),
                    Some(&cdn::user_avatar(user.id, user.avatar.as_deref(), 64)),
                ));
            }
            list.append(&row);
        }
        panel.audit.append(&list);
    }
}

/// The role being edited, with its permissions as last sent.
struct RoleEditor {
    guild: Snowflake,
    role: Snowflake,
    toasts: adw::ToastOverlay,
    permissions: Cell<u64>,
    reverting: Cell<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(id: u64, position: i32) -> Role {
        Role {
            id: Snowflake::new(id),
            position,
            ..Role::default()
        }
    }

    #[test]
    fn moving_a_role_swaps_it_with_its_neighbour() {
        let ranked = [role(1, 3), role(2, 2), role(3, 1)];
        assert_eq!(
            swap_positions(&ranked, 1, true),
            Some(vec![(Snowflake::new(2), 3), (Snowflake::new(1), 2)])
        );
        assert_eq!(
            swap_positions(&ranked, 1, false),
            Some(vec![(Snowflake::new(2), 1), (Snowflake::new(3), 2)])
        );
        assert_eq!(swap_positions(&ranked, 0, true), None);
        assert_eq!(swap_positions(&ranked, 2, false), None);
        // Tied positions still change the order.
        let tied = [role(1, 2), role(2, 2)];
        assert_eq!(
            swap_positions(&tied, 1, true),
            Some(vec![(Snowflake::new(2), 3), (Snowflake::new(1), 2)])
        );
    }

    #[test]
    fn invite_expiry_reads_naturally() {
        let now = iso_to_unix_millis("2026-11-19T12:00:00+00:00").unwrap();
        assert_eq!(expiry_text(None, now), "Never expires");
        assert_eq!(
            expiry_text(Some("2026-11-19T11:00:00+00:00"), now),
            "Expired"
        );
        assert_eq!(
            expiry_text(Some("2026-11-19T12:30:00+00:00"), now),
            "Expires in 30 minutes"
        );
        assert_eq!(
            expiry_text(Some("2026-11-19T13:00:00+00:00"), now),
            "Expires in 1 hour"
        );
        assert_eq!(
            expiry_text(Some("2026-11-26T12:00:00+00:00"), now),
            "Expires in 7 days"
        );
    }

    #[test]
    fn audit_entries_become_sentences() {
        let names = |id: &str| match id {
            "9" => Some("Ada".to_owned()),
            "5" => Some("#general".to_owned()),
            _ => None,
        };
        let kick = json!({"action_type": 20, "target_id": "9", "reason": "spam"});
        assert_eq!(describe_audit_entry(&kick, &names), "kicked Ada — “spam”");
        let timeout = json!({"action_type": 24, "target_id": "9", "changes": [
            {"key": "communication_disabled_until", "new_value": "2030-01-01T00:00:00+00:00"}]});
        assert_eq!(describe_audit_entry(&timeout, &names), "timed out Ada");
        let roles = json!({"action_type": 25, "target_id": "9", "changes": [
            {"key": "$add", "new_value": [{"id": "1", "name": "Mods"}]}]});
        assert_eq!(describe_audit_entry(&roles, &names), "gave Ada Mods");
        let deleted = json!({"action_type": 12, "target_id": "77", "changes": [
            {"key": "name", "old_value": "old-chat"}]});
        assert_eq!(
            describe_audit_entry(&deleted, &names),
            "deleted channel old-chat"
        );
        let message = json!({"action_type": 72, "target_id": "9", "options": {"channel_id": "5", "count": "1"}});
        assert_eq!(
            describe_audit_entry(&message, &names),
            "deleted a message by Ada in #general"
        );
        assert_eq!(
            describe_audit_entry(&json!({"action_type": 999}), &names),
            "did something (action 999)"
        );
    }

    #[test]
    fn every_page_needs_a_permission() {
        assert!(PAGES.iter().all(|page| page.3 != 0));
        assert_ne!(settings_permissions() & permissions::VIEW_AUDIT_LOG, 0);
    }
}
