//! The server (guild) header dropdown and the actions behind it.

use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::Snowflake;
use kc_state::permissions;

use crate::shell::AppState;
use crate::widgets::label;

/// What a server-menu item does when chosen.
type MenuAction = Box<dyn Fn(&Rc<AppState>)>;

/// What the server menu needs to decide which items to show.
struct ServerInfo {
    name: String,
    owner: bool,
    manage_channels: bool,
    can_edit: bool,
    muted: bool,
    /// A channel to make invites against (the open one, or the first).
    invite_channel: Option<Snowflake>,
}

impl AppState {
    fn server_info(&self, guild_id: Snowflake) -> Option<ServerInfo> {
        self.with_session(|session| {
            let selected = session.selected_channel;
            session.store.read(|state| {
                let guild = state.guilds.get(&guild_id)?;
                let me = state.my_id()?;
                let owner = guild.owner_id == Some(me);
                let roles = state
                    .member(guild_id, me)
                    .map(|member| member.roles.clone())
                    .unwrap_or_default();
                let perms = permissions::base(guild, me, &roles);
                let admin = owner || perms & permissions::ADMINISTRATOR != 0;
                // A text channel to point invites at: the open one, else any.
                let invite_channel = selected
                    .filter(|id| {
                        state
                            .channels
                            .get(id)
                            .is_some_and(|c| c.guild_id == Some(guild_id) && c.kind.is_text())
                    })
                    .or_else(|| {
                        let mut texts: Vec<&kc_state::Channel> = state
                            .channels
                            .values()
                            .filter(|c| c.guild_id == Some(guild_id) && c.kind.is_text())
                            .collect();
                        texts.sort_by_key(|c| c.position.unwrap_or(0));
                        texts.first().map(|c| c.id)
                    });
                Some(ServerInfo {
                    name: guild.name.clone(),
                    owner,
                    manage_channels: admin || perms & permissions::MANAGE_CHANNELS != 0,
                    can_edit: admin,
                    muted: state.guild_muted(guild_id),
                    invite_channel,
                })
            })
        })
        .flatten()
    }

    /// Builds the dropdown shown by the server-name button.
    pub(crate) fn build_server_menu(self: &Rc<Self>, guild_id: Snowflake) -> gtk::Popover {
        let popover = gtk::Popover::new();
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
        menu.add_css_class("kc-menu");
        menu.set_width_request(230);

        let Some(info) = self.server_info(guild_id) else {
            menu.append(&label("Server unavailable", &["dim-label"]));
            popover.set_child(Some(&menu));
            return popover;
        };

        let add = {
            let popover = popover.clone();
            let menu = menu.clone();
            move |text: &str, danger: bool, action: MenuAction, app: &Rc<AppState>| {
                let button = gtk::Button::new();
                button.add_css_class("flat");
                let text_label = label(text, &[]);
                text_label.set_xalign(0.0);
                text_label.set_halign(gtk::Align::Start);
                text_label.set_hexpand(true);
                button.set_child(Some(&text_label));
                if danger {
                    button.add_css_class("kc-danger");
                }
                let popover = popover.clone();
                let app = app.clone();
                button.connect_clicked(move |_| {
                    popover.popdown();
                    action(&app);
                });
                menu.append(&button);
            }
        };
        let separator = {
            let menu = menu.clone();
            move || {
                let line = gtk::Separator::new(gtk::Orientation::Horizontal);
                line.add_css_class("kc-menu-sep");
                menu.append(&line);
            }
        };

        if let Some(channel) = info.invite_channel {
            add(
                "Invite People",
                false,
                Box::new(move |app| app.create_invite(channel)),
                self,
            );
        }
        if info.manage_channels {
            separator();
            add(
                "Create Channel",
                false,
                Box::new(move |app| app.create_channel_dialog(guild_id, false)),
                self,
            );
            add(
                "Create Category",
                false,
                Box::new(move |app| app.create_channel_dialog(guild_id, true)),
                self,
            );
        }

        separator();
        if info.can_edit {
            add(
                "Server Settings",
                false,
                Box::new(move |app| app.server_settings_dialog(guild_id)),
                self,
            );
        }
        let muted = info.muted;
        add(
            if muted {
                "Unmute Server"
            } else {
                "Mute Server"
            },
            false,
            Box::new(move |app| app.set_guild_muted(guild_id, !muted)),
            self,
        );

        separator();
        add(
            "Copy Server ID",
            false,
            Box::new(move |app| {
                app.window.clipboard().set_text(&guild_id.to_string());
                app.set_status("Server ID copied");
            }),
            self,
        );
        if !info.owner {
            let name = info.name.clone();
            add(
                "Leave Server",
                true,
                Box::new(move |app| app.leave_guild_confirm(guild_id, &name)),
                self,
            );
        }

        popover.set_child(Some(&menu));
        popover
    }

    /// Creates an invite for a channel and copies its link to the clipboard.
    pub(crate) fn create_invite(self: &Rc<Self>, channel: Snowflake) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        self.spawn(
            async move { http.create_invite(channel).await },
            |app, result| match result {
                Ok(code) if !code.is_empty() => {
                    let link = format!("https://discord.gg/{code}");
                    app.window.clipboard().set_text(&link);
                    app.set_status(&format!("Invite copied: {link}"));
                }
                Ok(_) => app.set_status("Could not create an invite"),
                Err(error) => app.set_status(&format!("Could not create an invite: {error}")),
            },
        );
    }

    /// Prompts for a name and creates a channel (or a category).
    fn create_channel_dialog(self: &Rc<Self>, guild_id: Snowflake, category: bool) {
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

        let entry = gtk::Entry::new();
        entry.set_placeholder_text(Some(if category {
            "category-name"
        } else {
            "new-channel"
        }));
        entry.set_margin_top(8);
        dialog.set_extra_child(Some(&entry));

        let app = self.clone();
        let entry_weak = entry.downgrade();
        dialog.connect_response(None, move |dialog, response| {
            if response != "create" {
                return;
            }
            let Some(entry) = entry_weak.upgrade() else {
                return;
            };
            let name = entry.text().trim().to_string();
            dialog.close();
            if name.is_empty() {
                return;
            }
            let Some((_, http, _)) = app.active() else {
                return;
            };
            // 4 is a category; 0 a text channel.
            let kind = if category { 4 } else { 0 };
            app.spawn(
                async move { http.create_channel(guild_id, &name, kind, None).await },
                |app, result| match result {
                    Ok(_) => app.set_status("Channel created"),
                    Err(error) => app.set_status(&format!("Could not create channel: {error}")),
                },
            );
        });
        dialog.present(Some(&self.window));
    }

    /// A small Server Settings dialog: rename the guild.
    fn server_settings_dialog(self: &Rc<Self>, guild_id: Snowflake) {
        let current = self
            .with_session(|session| {
                session
                    .store
                    .read(|state| state.guilds.get(&guild_id).map(|guild| guild.name.clone()))
            })
            .flatten()
            .unwrap_or_default();

        let dialog = adw::AlertDialog::new(Some("Server Settings"), None);
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");

        let fields = gtk::Box::new(gtk::Orientation::Vertical, 6);
        fields.set_margin_top(8);
        fields.append(&label("Server Name", &["caption", "dim-label"]));
        let entry = gtk::Entry::new();
        entry.set_text(&current);
        fields.append(&entry);
        dialog.set_extra_child(Some(&fields));

        let app = self.clone();
        let entry_weak = entry.downgrade();
        dialog.connect_response(None, move |dialog, response| {
            if response != "save" {
                return;
            }
            let Some(entry) = entry_weak.upgrade() else {
                return;
            };
            let name = entry.text().trim().to_string();
            dialog.close();
            if name.is_empty() || name == current {
                return;
            }
            let Some((_, http, _)) = app.active() else {
                return;
            };
            app.spawn(
                async move {
                    http.edit_guild(guild_id, serde_json::json!({ "name": name }))
                        .await
                },
                |app, result| match result {
                    Ok(_) => app.set_status("Server updated"),
                    Err(error) => app.set_status(&format!("Could not update server: {error}")),
                },
            );
        });
        dialog.present(Some(&self.window));
    }

    /// Mutes or unmutes a whole server.
    fn set_guild_muted(self: &Rc<Self>, guild_id: Snowflake, muted: bool) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        self.spawn(
            async move {
                http.patch_guild_settings(guild_id, serde_json::json!({ "muted": muted }))
                    .await
            },
            move |app, result| match result {
                Ok(()) => app.set_status(if muted {
                    "Server muted"
                } else {
                    "Server unmuted"
                }),
                Err(error) => app.set_status(&format!("Could not change notifications: {error}")),
            },
        );
    }

    /// Confirms before leaving a guild.
    fn leave_guild_confirm(self: &Rc<Self>, guild_id: Snowflake, name: &str) {
        let dialog = adw::AlertDialog::new(
            Some("Leave Server"),
            Some(&format!("Are you sure you want to leave {name}?")),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("leave", "Leave Server")]);
        dialog.set_response_appearance("leave", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let app = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "leave" {
                return;
            }
            let Some((_, http, _)) = app.active() else {
                return;
            };
            app.spawn(
                async move { http.leave_guild(guild_id).await },
                |app, result| match result {
                    Ok(()) => app.set_status("Left the server"),
                    Err(error) => app.set_status(&format!("Could not leave: {error}")),
                },
            );
        });
        dialog.present(Some(&self.window));
    }
}
