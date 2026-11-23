//! Server Settings: Community (turning it on or off, its rules and updates
//! channels), and the owner's Transfer & Delete page.

use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::Snowflake;
use kc_http::{CommunityChange, RoleEdit};
use kc_state::{ChannelType, permissions};

use super::{Panel, boxed_list, page_box, section_title, wrapped};
use crate::shell::AppState;
use crate::video_setup::spawn_local;
use crate::widgets::clear_box;

/// What Discord's Enable Community takes away from @everyone when asked.
const MODERATION: u64 = permissions::ADMINISTRATOR
    | permissions::KICK_MEMBERS
    | permissions::BAN_MEMBERS
    | permissions::MANAGE_CHANNELS
    | permissions::MANAGE_GUILD
    | permissions::MANAGE_ROLES
    | permissions::MANAGE_MESSAGES
    | permissions::MANAGE_WEBHOOKS
    | permissions::MANAGE_GUILD_EXPRESSIONS
    | permissions::MENTION_EVERYONE
    | permissions::MODERATE_MEMBERS;

/// How a server becomes a Community server.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CommunitySetup {
    /// `None`: Discord makes a new channel.
    pub rules: Option<Snowflake>,
    pub updates: Option<Snowflake>,
    pub mentions_only: bool,
    pub strip_everyone: bool,
}

impl CommunitySetup {
    /// What a new server gets: new channels and Discord's recommendations.
    pub(crate) const RECOMMENDED: Self = Self {
        rules: None,
        updates: None,
        mentions_only: true,
        strip_everyone: true,
    };
}

/// A server's text channels, `(id, "#name")`, in sidebar order.
fn text_channels(app: &AppState, guild: Snowflake) -> Vec<(Snowflake, String)> {
    app.with_session(|session| {
        session.store.read(|state| {
            state
                .guild_channels(guild)
                .into_iter()
                .filter(|channel| channel.kind == ChannelType::GuildText)
                .map(|channel| (channel.id, format!("#{}", channel.display_name())))
                .collect()
        })
    })
    .unwrap_or_default()
}

/// A drop-down of text channels; with `create`, "Create One for Me" first.
fn channel_row(title: &str, channels: &[(Snowflake, String)], current: Option<Snowflake>, create: bool) -> adw::ComboRow {
    let row = adw::ComboRow::new();
    row.set_title(title);
    let mut names: Vec<&str> = Vec::new();
    if create {
        names.push("Create One for Me");
    }
    names.extend(channels.iter().map(|(_, name)| name.as_str()));
    row.set_model(Some(&gtk::StringList::new(&names)));
    let offset = u32::from(create);
    if let Some(index) = current.and_then(|id| channels.iter().position(|(channel, _)| *channel == id)) {
        row.set_selected(index as u32 + offset);
    }
    row
}

/// The channel a row picks (`None`: create one).
fn picked(row: &adw::ComboRow, channels: &[(Snowflake, String)], create: bool) -> Option<Snowflake> {
    let index = row.selected() as usize;
    let index = if create { index.checked_sub(1)? } else { index };
    channels.get(index).map(|(id, _)| *id)
}

impl AppState {
    /// Makes `guild` a Community server, then (if asked) takes moderation
    /// permissions from @everyone. `done` gets an error to show, if any.
    pub(crate) fn enable_community(
        self: &Rc<Self>,
        guild: Snowflake,
        setup: CommunitySetup,
        done: impl FnOnce(&Rc<AppState>, Result<(), String>) + 'static,
    ) {
        let Some((_, http, store)) = self.active() else {
            return;
        };
        let Some((features, level, everyone)) = store.read(|state| {
            let server = state.guilds.get(&guild)?;
            let everyone = server.roles.iter().find(|role| role.id == guild).map(|role| role.permissions);
            Some((server.features.clone(), server.verification_level.unwrap_or(0), everyone))
        }) else {
            done(self, Err("the server isn't loaded yet".to_owned()));
            return;
        };
        let change = CommunityChange {
            enable: true,
            features,
            verification_level: level,
            rules_channel: setup.rules,
            updates_channel: setup.updates,
            mentions_only: setup.mentions_only,
        };
        let role_http = http.clone();
        self.with_mfa(
            "turn on Community",
            move |mfa| {
                let (http, change) = (http.clone(), change.clone());
                async move { http.set_community(guild, &change, mfa.as_deref()).await }
            },
            move |app, result| {
                if let Err(error) = result {
                    done(app, Err(error.describe()));
                    return;
                }
                match everyone.filter(|perms| setup.strip_everyone && perms & MODERATION != 0) {
                    Some(perms) => {
                        let edit = RoleEdit { permissions: Some(perms & !MODERATION), ..RoleEdit::default() };
                        spawn_local(
                            app,
                            async move { role_http.edit_role(guild, guild, &edit).await },
                            move |app, result| {
                                done(app, result.map(|_| ()).map_err(|error| {
                                    format!("Community is on, but @everyone kept its permissions: {}", error.describe())
                                }))
                            },
                        );
                    }
                    None => done(app, Ok(())),
                }
            },
        );
    }

    /// For a server just created: waits for it to arrive, then enables
    /// Community with Discord's recommendations.
    pub(crate) fn enable_community_when_ready(self: &Rc<Self>, guild: Snowflake, tries: u32) {
        let loaded = self
            .with_session(|session| session.store.read(|state| state.guilds.get(&guild).is_some_and(|server| !server.roles.is_empty())))
            .unwrap_or(false);
        if !loaded {
            if tries == 0 {
                self.set_status("The server was made, but Community couldn't be turned on: it didn't load in time");
                return;
            }
            let app = self.clone();
            glib::timeout_add_local_once(Duration::from_millis(250), move || app.enable_community_when_ready(guild, tries - 1));
            return;
        }
        self.enable_community(guild, CommunitySetup::RECOMMENDED, |app, result| match result {
            Ok(()) => app.set_status("Community server ready"),
            Err(error) => app.set_status(&format!("Could not turn on Community: {error}")),
        });
    }

    // -- the Community page ----------------------------------------------

    pub(super) fn community_page(self: &Rc<Self>, panel: &Rc<Panel>) -> gtk::Box {
        let page = page_box();
        page.append(&section_title("Community"));
        page.append(&panel.community);
        self.fill_community(panel);
        page
    }

    pub(super) fn fill_community(self: &Rc<Self>, panel: &Rc<Panel>) {
        let guild = panel.guild;
        let Some((community, rules, updates)) = self
            .with_session(|session| {
                session.store.read(|state| {
                    state
                        .guilds
                        .get(&guild)
                        .map(|server| (server.is_community(), server.rules_channel_id, server.public_updates_channel_id))
                })
            })
            .flatten()
        else {
            return;
        };
        // Leave the page alone while someone is choosing in it.
        if panel.community.first_child().is_some() && panel.community.has_css_class(if community { "kc-on" } else { "kc-off" }) {
            return;
        }
        clear_box(&panel.community);
        panel.community.remove_css_class("kc-on");
        panel.community.remove_css_class("kc-off");
        panel.community.add_css_class(if community { "kc-on" } else { "kc-off" });
        let channels = text_channels(self, guild);

        if community {
            panel.community.append(&wrapped(
                "This is a Community server: it can have stage and announcement channels, a welcome screen, \
                 member screening and Server Insights.",
                &["dim-label"],
            ));
            let list = boxed_list();
            list.set_margin_top(12);
            let rules_row = channel_row("Rules Channel", &channels, rules, false);
            let updates_row = channel_row("Community Updates Channel", &channels, updates, false);
            for (row, field) in [(&rules_row, "rules_channel_id"), (&updates_row, "public_updates_channel_id")] {
                let (app, panel, channels) = (self.clone(), panel.clone(), channels.clone());
                row.connect_selected_notify(move |row| {
                    let Some(channel) = picked(row, &channels, false) else {
                        return;
                    };
                    let Some((_, http, _)) = app.active() else {
                        return;
                    };
                    let panel = panel.clone();
                    spawn_local(
                        &app,
                        async move { http.edit_guild(guild, serde_json::json!({ field: channel })).await },
                        move |_, result| match result {
                            Ok(_) => panel.toast("Saved"),
                            Err(error) => panel.fail("Could not change the channel", &error),
                        },
                    );
                });
                list.append(row);
            }
            panel.community.append(&list);
            let disable = gtk::Button::with_label("Disable Community");
            disable.add_css_class("destructive-action");
            disable.add_css_class("pill");
            disable.set_halign(gtk::Align::Start);
            disable.set_margin_top(18);
            let (app, target) = (self.clone(), panel.clone());
            disable.connect_clicked(move |_| app.confirm_disable_community(&target));
            panel.community.append(&disable);
            return;
        }

        panel.community.append(&wrapped(
            "Community servers get stage and announcement channels, a welcome screen, member screening \
             and Server Insights. Turning it on also sets the verification level to at least Low (a \
             verified email) and scans everyone's media for explicit content, as Discord requires.",
            &["dim-label"],
        ));
        let list = boxed_list();
        list.set_margin_top(12);
        let rules_row = channel_row("Rules Channel", &channels, None, true);
        rules_row.set_subtitle("Where your rules and guidelines live");
        let updates_row = channel_row("Community Updates Channel", &channels, None, true);
        updates_row.set_subtitle("Where Discord sends notices for moderators");
        let mentions = adw::SwitchRow::new();
        mentions.set_title("Notify Members of Mentions Only");
        mentions.set_subtitle("The default for new members (recommended)");
        mentions.set_active(true);
        let strip = adw::SwitchRow::new();
        strip.set_title("Remove Moderation Permissions from @everyone");
        strip.set_subtitle("Administrator, kick, ban, manage, @everyone mentions… (recommended)");
        strip.set_active(true);
        list.append(&rules_row);
        list.append(&updates_row);
        list.append(&mentions);
        list.append(&strip);
        panel.community.append(&list);
        let enable = gtk::Button::with_label("Enable Community");
        enable.add_css_class("suggested-action");
        enable.add_css_class("pill");
        enable.set_halign(gtk::Align::Start);
        enable.set_margin_top(18);
        let (app, target) = (self.clone(), panel.clone());
        enable.connect_clicked(move |button| {
            button.set_sensitive(false);
            let setup = CommunitySetup {
                rules: picked(&rules_row, &channels, true),
                updates: picked(&updates_row, &channels, true),
                mentions_only: mentions.is_active(),
                strip_everyone: strip.is_active(),
            };
            let (target, button) = (target.clone(), button.clone());
            app.enable_community(guild, setup, move |_, result| {
                button.set_sensitive(true);
                match result {
                    Ok(()) => target.toast("Community is on"),
                    Err(error) => target.toast(&format!("Could not turn on Community: {error}")),
                }
            });
        });
        panel.community.append(&enable);
    }

    fn confirm_disable_community(self: &Rc<Self>, panel: &Rc<Panel>) {
        let dialog = adw::AlertDialog::new(
            Some("Disable Community?"),
            Some("Stage and announcement channels, the welcome screen, member screening and Discovery stop working. Your channels and settings stay."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("disable", "Disable Community")]);
        dialog.set_response_appearance("disable", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let parent = panel.dialog.clone();
        let (app, panel) = (self.clone(), panel.clone());
        dialog.connect_response(Some("disable"), move |_, _| {
            let guild = panel.guild;
            let features = app
                .with_session(|session| session.store.read(|state| state.guilds.get(&guild).map(|server| server.features.clone())))
                .flatten()
                .unwrap_or_default();
            let change = CommunityChange { enable: false, features, ..CommunityChange::default() };
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let panel = panel.clone();
            app.with_mfa(
                "turn off Community",
                move |mfa| {
                    let (http, change) = (http.clone(), change.clone());
                    async move { http.set_community(guild, &change, mfa.as_deref()).await }
                },
                move |_, result| match result {
                    Ok(_) => panel.toast("Community is off"),
                    Err(error) => panel.fail("Could not turn off Community", &error),
                },
            );
        });
        dialog.present(Some(&parent));
    }

    // -- the owner's page ------------------------------------------------

    pub(super) fn owner_page(self: &Rc<Self>, panel: &Rc<Panel>) -> gtk::Box {
        let page = page_box();
        page.append(&section_title("Transfer & Delete"));
        page.append(&wrapped("Only the owner can do these, and they can't be undone.", &["dim-label"]));
        let list = boxed_list();
        list.set_margin_top(12);

        let transfer = adw::ActionRow::new();
        transfer.set_title("Transfer Ownership");
        transfer.set_subtitle("Give this server to another member. You stay a member, without the owner's powers.");
        let transfer_button = gtk::Button::with_label("Transfer…");
        transfer_button.set_valign(gtk::Align::Center);
        let (app, target) = (self.clone(), panel.clone());
        transfer_button.connect_clicked(move |_| app.transfer_dialog(&target));
        transfer.add_suffix(&transfer_button);
        list.append(&transfer);

        let delete = adw::ActionRow::new();
        delete.set_title("Delete Server");
        delete.set_subtitle("Every channel, message, role and emoji goes, for everyone, for good.");
        let delete_button = gtk::Button::with_label("Delete…");
        delete_button.add_css_class("destructive-action");
        delete_button.set_valign(gtk::Align::Center);
        let (app, target) = (self.clone(), panel.clone());
        delete_button.connect_clicked(move |_| app.delete_server_dialog(&target));
        delete.add_suffix(&delete_button);
        list.append(&delete);
        page.append(&list);
        page
    }

    fn delete_server_dialog(self: &Rc<Self>, panel: &Rc<Panel>) {
        let guild = panel.guild;
        let name = self
            .with_session(|session| session.store.read(|state| state.guilds.get(&guild).map(|server| server.name.clone())))
            .flatten()
            .unwrap_or_default();
        let dialog = adw::AlertDialog::new(
            Some(&format!("Delete {name}?")),
            Some("This can't be undone. Type the server's name to confirm."),
        );
        let entry = gtk::Entry::new();
        entry.set_placeholder_text(Some(&name));
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete Server")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_response_enabled("delete", false);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        {
            let (dialog, name) = (dialog.clone(), name.clone());
            entry.connect_changed(move |entry| dialog.set_response_enabled("delete", entry.text().trim() == name.trim()));
        }
        let parent = panel.dialog.clone();
        let (app, panel) = (self.clone(), panel.clone());
        dialog.connect_response(Some("delete"), move |_, _| {
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let panel = panel.clone();
            app.with_mfa(
                "delete this server",
                move |mfa| {
                    let http = http.clone();
                    async move { http.delete_guild(guild, mfa.as_deref()).await }
                },
                move |app, result| match result {
                    Ok(()) => {
                        panel.dialog.close();
                        app.open_home();
                        app.set_status("Server deleted");
                    }
                    Err(error) => panel.fail("Could not delete the server", &error),
                },
            );
        });
        dialog.present(Some(&parent));
        entry.grab_focus();
    }

    /// Pick a member, confirm, and (for an account with an email and no
    /// two-factor) enter the code Discord emails.
    fn transfer_dialog(self: &Rc<Self>, panel: &Rc<Panel>) {
        let guild = panel.guild;
        let dialog = adw::Dialog::new();
        dialog.set_title("Transfer Ownership");
        dialog.set_content_width(420);
        dialog.set_content_height(520);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_margin_bottom(12);
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search members"));
        content.append(&search);
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&list));
        content.append(&scroll);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&content));
        dialog.set_child(Some(&view));

        let fill: Rc<dyn Fn()> = {
            let (app, list, search, dialog, panel) = (self.clone(), list.clone(), search.clone(), dialog.clone(), panel.clone());
            Rc::new(move || {
                clear_box_list(&list);
                let query = search.text().to_lowercase();
                let members: Vec<(Snowflake, String, Option<String>)> = app
                    .with_session(|session| {
                        let me = session.user.as_ref().map(|user| user.id);
                        session.store.read(|state| {
                            state
                                .guild_member_users(guild)
                                .into_iter()
                                .filter(|user| Some(user.id) != me && !user.bot.unwrap_or(false))
                                .map(|user| {
                                    (
                                        user.id,
                                        state.display_name(Some(guild), &user),
                                        Some(kc_http::cdn::user_avatar(user.id, user.avatar.as_deref(), 64)),
                                    )
                                })
                                .filter(|(_, name, _)| query.is_empty() || name.to_lowercase().contains(&query))
                                .take(50)
                                .collect()
                        })
                    })
                    .unwrap_or_default();
                if members.is_empty() {
                    let row = adw::ActionRow::new();
                    row.set_title(if query.is_empty() { "Search for a member" } else { "No one by that name" });
                    list.append(&row);
                }
                for (id, name, avatar) in members {
                    let row = adw::ActionRow::new();
                    row.set_title(&glib::markup_escape_text(&name));
                    row.add_prefix(&app.images.avatar(32, &name, avatar.as_deref()));
                    row.set_activatable(true);
                    let (app, dialog, panel) = (app.clone(), dialog.clone(), panel.clone());
                    row.connect_activated(move |_| {
                        dialog.close();
                        app.confirm_transfer(&panel, id, &name);
                    });
                    list.append(&row);
                }
            })
        };
        fill();
        {
            // Members not loaded yet are asked for as you type.
            let (app, fill) = (self.clone(), fill.clone());
            search.connect_search_changed(move |search| {
                let query = search.text().to_string();
                if query.chars().count() >= 2
                    && let Some((account, _, _)) = app.active()
                    && let Some(gateway) = crate::session::gateway(account)
                {
                    gateway.search_members(guild, &query, 10);
                }
                fill();
                let fill = fill.clone();
                glib::timeout_add_local_once(Duration::from_millis(700), move || fill());
            });
        }
        dialog.present(Some(&panel.dialog));
        search.grab_focus();
    }

    fn confirm_transfer(self: &Rc<Self>, panel: &Rc<Panel>, user: Snowflake, name: &str) {
        let guild = panel.guild;
        // An account with an email and no two-factor confirms with a code
        // Discord emails; one with two-factor confirms with that instead.
        let needs_code = self
            .with_session(|session| session.user.as_ref().map(|me| me.mfa_enabled != Some(true) && me.email.is_some()))
            .flatten()
            .unwrap_or(false);
        let dialog = adw::AlertDialog::new(
            Some(&format!("Give the Server to {name}?")),
            Some(&format!(
                "{name} becomes the owner, with every power over it, including deleting it. You can't take this back yourself."
            )),
        );
        let fields = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let agree = gtk::CheckButton::with_label(&format!("I understand the server will belong to {name}"));
        fields.append(&agree);
        let code = gtk::Entry::new();
        if needs_code {
            code.set_placeholder_text(Some("The code Discord emailed you"));
            fields.append(&code);
            let send = gtk::Button::with_label("Email Me the Code");
            send.add_css_class("flat");
            send.set_halign(gtk::Align::Start);
            let app = self.clone();
            send.connect_clicked(move |button| {
                let Some((_, http, _)) = app.active() else {
                    return;
                };
                button.set_sensitive(false);
                let button = button.clone();
                spawn_local(&app, async move { http.request_transfer_code(guild).await }, move |app, result| match result {
                    Ok(()) => {
                        button.set_label("Code Sent — Check Your Email");
                    }
                    Err(error) => {
                        button.set_sensitive(true);
                        app.set_status(&format!("Could not send the code: {}", error.describe()));
                    }
                });
            });
            fields.append(&send);
        }
        dialog.set_extra_child(Some(&fields));
        dialog.add_responses(&[("cancel", "Cancel"), ("transfer", "Transfer Ownership")]);
        dialog.set_response_appearance("transfer", adw::ResponseAppearance::Destructive);
        dialog.set_response_enabled("transfer", false);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let ready: Rc<dyn Fn()> = {
            let (dialog, agree, code) = (dialog.clone(), agree.clone(), code.clone());
            Rc::new(move || {
                dialog.set_response_enabled("transfer", agree.is_active() && (!needs_code || !code.text().trim().is_empty()));
            })
        };
        {
            let ready = ready.clone();
            agree.connect_toggled(move |_| ready());
        }
        code.connect_changed(move |_| ready());
        let parent = panel.dialog.clone();
        let (app, panel) = (self.clone(), panel.clone());
        dialog.connect_response(Some("transfer"), move |_, _| {
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let code = needs_code.then(|| code.text().trim().to_owned());
            let panel = panel.clone();
            app.with_mfa(
                "give this server away",
                move |mfa| {
                    let (http, code) = (http.clone(), code.clone());
                    async move { http.transfer_ownership(guild, user, code.as_deref(), mfa.as_deref()).await }
                },
                move |app, result| match result {
                    Ok(()) => {
                        panel.dialog.close();
                        app.set_status("Ownership transferred");
                    }
                    Err(error) => panel.fail("Could not transfer ownership", &error),
                },
            );
        });
        dialog.present(Some(&parent));
    }
}

fn clear_box_list(list: &gtk::ListBox) {
    while let Some(row) = list.first_child() {
        list.remove(&row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moderation_permissions_are_what_enable_community_strips() {
        let everyone = permissions::VIEW_CHANNEL | permissions::SEND_MESSAGES | permissions::MENTION_EVERYONE | permissions::ADMINISTRATOR;
        assert_eq!(everyone & !MODERATION, permissions::VIEW_CHANNEL | permissions::SEND_MESSAGES);
    }
}
