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
    /// Any Server Settings page is open to the account.
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
                let perms = state.guild_permissions(guild_id);
                // A channel invites can be made for: the open one, else any.
                let invitable = |id: &Snowflake| {
                    state
                        .channels
                        .get(id)
                        .is_some_and(|c| c.guild_id == Some(guild_id) && !c.kind.is_category())
                        && state.can(*id, permissions::CREATE_INSTANT_INVITE)
                        && state.can(*id, permissions::VIEW_CHANNEL)
                };
                let invite_channel = selected.filter(invitable).or_else(|| {
                    let mut channels: Vec<&kc_state::Channel> = state
                        .channels
                        .values()
                        .filter(|c| invitable(&c.id))
                        .collect();
                    channels.sort_by_key(|c| c.sort_key());
                    channels.first().map(|c| c.id)
                });
                Some(ServerInfo {
                    name: guild.name.clone(),
                    owner,
                    manage_channels: perms & permissions::MANAGE_CHANNELS != 0,
                    can_edit: perms & crate::server_settings::settings_permissions() != 0,
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

        if info.invite_channel.is_some() {
            add(
                "Invite People",
                false,
                Box::new(move |app| app.invite_dialog(guild_id)),
                self,
            );
        }
        if info.manage_channels {
            separator();
            add(
                "Create Channel",
                false,
                Box::new(move |app| app.create_channel_dialog(guild_id, None, false)),
                self,
            );
            add(
                "Create Category",
                false,
                Box::new(move |app| app.create_channel_dialog(guild_id, None, true)),
                self,
            );
        }

        separator();
        if info.can_edit {
            add(
                "Server Settings",
                false,
                Box::new(move |app| {
                    app.show_server_settings(guild_id, None);
                }),
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

    /// Mutes or unmutes a whole server.
    fn set_guild_muted(self: &Rc<Self>, guild_id: Snowflake, muted: bool) {
        self.change_notification_settings(
            Some(guild_id),
            kc_http::client::guild_mute_payload(muted),
            if muted {
                "Server muted"
            } else {
                "Server unmuted"
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
