//! Moderating a member from the user menu: kick, ban, timeout, nickname,
//! roles, and in voice: move, disconnect, server mute and deafen. Every
//! item is shown only when `State::member_actions` allows it (permissions
//! plus the role hierarchy); a refusal from Discord is shown as a notice.

use std::cell::Cell;
use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::Snowflake;
use kc_http::MemberEdit;
use kc_state::Role;
use kc_state::moderation::MemberActions;
use kc_state::time::{millis_to_iso, unix_millis_now};

use crate::menu::{MenuAction, menu_item};
use crate::shell::AppState;
use crate::widgets::{hex_color, label};

/// Timeout lengths offered, as Discord offers them.
pub(crate) const TIMEOUTS: &[(&str, u64)] = &[
    ("60 Seconds", 60),
    ("5 Minutes", 300),
    ("10 Minutes", 600),
    ("1 Hour", 3_600),
    ("1 Day", 86_400),
    ("1 Week", 604_800),
];

/// How much of a banned user's message history to delete.
pub(crate) const BAN_HISTORY: &[(&str, u32)] = &[
    ("Don't Delete Any", 0),
    ("Previous Hour", 3_600),
    ("Previous 6 Hours", 21_600),
    ("Previous 12 Hours", 43_200),
    ("Previous 24 Hours", 86_400),
    ("Previous 3 Days", 259_200),
    ("Previous 7 Days", 604_800),
];

/// The ISO 8601 moment a timeout of `seconds` started at `now_millis`
/// ends, as `communication_disabled_until` takes it.
pub(crate) fn timeout_until(now_millis: u64, seconds: u64) -> String {
    format!(
        "{}+00:00",
        millis_to_iso(now_millis + seconds.saturating_mul(1000))
    )
}

/// The reason typed into an entry, `None` when blank.
fn reason_of(entry: &gtk::Entry) -> Option<String> {
    let text = entry.text().trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// A small filled circle in a role's colour (grey for no colour).
pub(crate) fn color_dot(color: u32) -> gtk::Box {
    let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    dot.add_css_class("kc-role-dot");
    dot.set_valign(gtk::Align::Center);
    if color != 0 {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&format!(
            "box {{ background-color: {}; }}",
            hex_color(color)
        ));
        #[allow(deprecated)]
        dot.style_context()
            .add_provider(&provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
    dot
}

/// An entry for an optional audit-log reason.
fn reason_entry() -> gtk::Entry {
    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some("Optional; shown in the audit log"));
    entry.set_max_length(512);
    entry
}

/// A caption above a field in an alert dialog.
fn field_caption(text: &str) -> gtk::Label {
    let caption = label(text, &["caption-heading"]);
    caption.set_margin_top(6);
    caption
}

/// A drop-down of the names in a table.
fn choice<T>(items: &[(&str, T)], selected: u32) -> gtk::DropDown {
    let names: Vec<&str> = items.iter().map(|(name, _)| *name).collect();
    let dropdown = gtk::DropDown::from_strings(&names);
    dropdown.set_selected(selected);
    dropdown
}

impl AppState {
    /// Shows why a moderation action failed, in Discord's own terms when
    /// they are not clear.
    pub(crate) fn report_failure(&self, what: &str, error: &kc_http::Error) {
        tracing::warn!(%error, what, "moderation request failed");
        self.notify(&format!("{what}: {}", error.describe()), None, 10);
    }

    /// Runs a moderation request; says `done` when it succeeds and why it
    /// failed otherwise.
    pub(crate) fn moderate(
        self: &Rc<Self>,
        what: &'static str,
        done: String,
        request: impl std::future::Future<Output = kc_http::Result<()>> + Send + 'static,
    ) {
        self.spawn(request, move |app, result| match result {
            Ok(()) => app.set_status(&done),
            Err(error) => app.report_failure(what, &error),
        });
    }

    /// The name of a member as the server shows it.
    pub(crate) fn member_display_name(&self, guild: Snowflake, user_id: Snowflake) -> String {
        self.with_session(|session| {
            session.store.read(|state| {
                state
                    .users
                    .get(&user_id)
                    .map(|user| state.display_name(Some(guild), user))
            })
        })
        .flatten()
        .unwrap_or_else(|| user_id.to_string())
    }

    pub(crate) fn member_actions(&self, guild: Snowflake, user_id: Snowflake) -> MemberActions {
        self.with_session(|session| {
            session
                .store
                .read(|state| state.member_actions(guild, user_id, unix_millis_now()))
        })
        .unwrap_or_default()
    }

    /// Adds the moderation items for `user_id` in `guild` to a user menu.
    /// Returns whether anything was added.
    pub(crate) fn add_moderation_items(
        self: &Rc<Self>,
        menu: &gtk::Box,
        popover: &gtk::Popover,
        guild: Snowflake,
        user_id: Snowflake,
        is_me: bool,
    ) {
        let actions = self.member_actions(guild, user_id);
        let name = self.member_display_name(guild, user_id);
        let separator = || {
            let line = gtk::Separator::new(gtk::Orientation::Horizontal);
            line.add_css_class("kc-menu-sep");
            menu.append(&line);
        };
        let add = |text: &str, danger: bool, action: MenuAction| {
            let button = menu_item(text, danger);
            let popover = popover.clone();
            let app = self.clone();
            button.connect_clicked(move |_| {
                popover.popdown();
                action(&app);
            });
            menu.append(&button);
        };

        // -- voice -------------------------------------------------------
        if actions.in_voice && (actions.server_mute || actions.server_deafen || actions.move_member)
        {
            separator();
            if actions.server_mute {
                let mute = !actions.muted;
                add(
                    if mute {
                        "Server Mute"
                    } else {
                        "Remove Server Mute"
                    },
                    false,
                    Box::new(move |app| app.voice_edit(guild, user_id, MemberEdit::Mute(mute))),
                );
            }
            if actions.server_deafen {
                let deafen = !actions.deafened;
                add(
                    if deafen {
                        "Server Deafen"
                    } else {
                        "Remove Server Deafen"
                    },
                    false,
                    Box::new(move |app| app.voice_edit(guild, user_id, MemberEdit::Deafen(deafen))),
                );
            }
            if actions.move_member {
                let more = menu_item("Move To…", false);
                let app = self.clone();
                let target = popover.clone();
                more.connect_clicked(move |_| app.show_move_targets(&target, guild, user_id));
                menu.append(&more);
                add(
                    "Disconnect",
                    true,
                    Box::new(move |app| app.voice_edit(guild, user_id, MemberEdit::Move(None))),
                );
            }
        }

        // -- membership --------------------------------------------------
        if actions.nickname || actions.roles {
            separator();
        }
        if actions.nickname {
            let nick = self
                .with_session(|session| {
                    session
                        .store
                        .read(|state| state.member(guild, user_id)?.nick.clone())
                })
                .flatten();
            add(
                "Change Nickname",
                false,
                Box::new(move |app| app.change_nickname(guild, user_id, is_me, nick.clone())),
            );
        }
        if actions.roles {
            add(
                "Roles…",
                false,
                Box::new(move |app| app.roles_dialog(guild, user_id)),
            );
        }
        if actions.timeout || actions.kick || actions.ban {
            separator();
        }
        if actions.timeout {
            if actions.timed_out {
                let shown = name.clone();
                add(
                    "Remove Timeout",
                    false,
                    Box::new(move |app| {
                        let Some((_, http, _)) = app.active() else {
                            return;
                        };
                        app.moderate(
                            "Could not remove the timeout",
                            format!("{shown}'s timeout was removed"),
                            async move {
                                http.edit_member(guild, user_id, &MemberEdit::Timeout(None), None)
                                    .await
                            },
                        );
                    }),
                );
            } else {
                let shown = name.clone();
                add(
                    &format!("Time Out {name}"),
                    true,
                    Box::new(move |app| app.timeout_dialog(guild, user_id, &shown)),
                );
            }
        }
        if actions.kick {
            let shown = name.clone();
            add(
                &format!("Kick {name}"),
                true,
                Box::new(move |app| app.kick_dialog(guild, user_id, &shown)),
            );
        }
        if actions.ban {
            let shown = name.clone();
            add(
                &format!("Ban {name}"),
                true,
                Box::new(move |app| app.ban_dialog(guild, user_id, &shown)),
            );
        }
    }

    /// Server mute, deafen, move or disconnect.
    pub(crate) fn voice_edit(
        self: &Rc<Self>,
        guild: Snowflake,
        user_id: Snowflake,
        edit: MemberEdit,
    ) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let name = self.member_display_name(guild, user_id);
        let (what, done) = match &edit {
            MemberEdit::Mute(true) => ("Could not server mute", format!("{name} is server muted")),
            MemberEdit::Mute(false) => (
                "Could not unmute",
                format!("{name} is no longer server muted"),
            ),
            MemberEdit::Deafen(true) => (
                "Could not server deafen",
                format!("{name} is server deafened"),
            ),
            MemberEdit::Deafen(false) => (
                "Could not undeafen",
                format!("{name} is no longer server deafened"),
            ),
            MemberEdit::Move(None) => ("Could not disconnect", format!("{name} was disconnected")),
            MemberEdit::Move(Some(channel)) => {
                let channel_name = self
                    .with_session(|session| {
                        session.store.read(|state| {
                            state
                                .channels
                                .get(channel)
                                .map(|channel| channel.display_name())
                        })
                    })
                    .flatten()
                    .unwrap_or_default();
                (
                    "Could not move",
                    format!("{name} was moved to {channel_name}"),
                )
            }
            _ => ("Could not change the member", format!("{name} was updated")),
        };
        self.moderate(what, done, async move {
            http.edit_member(guild, user_id, &edit, None).await
        });
    }

    /// Replaces the user menu with the voice channels the member can be
    /// moved to.
    pub(crate) fn show_move_targets(
        self: &Rc<Self>,
        popover: &gtk::Popover,
        guild: Snowflake,
        user_id: Snowflake,
    ) {
        let (targets, current) = self
            .with_session(|session| {
                session.store.read(|state| {
                    (
                        state.move_targets(guild),
                        state
                            .voice_states
                            .get(&(Some(guild), user_id))
                            .and_then(|voice| voice.channel_id),
                    )
                })
            })
            .unwrap_or_default();
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
        menu.add_css_class("kc-menu");
        menu.set_width_request(200);
        let heading = label("MOVE TO", &["kc-category"]);
        heading.set_margin_start(8);
        heading.set_margin_top(4);
        heading.set_margin_bottom(2);
        menu.append(&heading);
        let mut any = false;
        for (channel, name) in targets {
            if Some(channel) == current {
                continue;
            }
            any = true;
            let item = menu_item(&name, false);
            let app = self.clone();
            let popover_weak = popover.downgrade();
            item.connect_clicked(move |_| {
                if let Some(popover) = popover_weak.upgrade() {
                    popover.popdown();
                }
                app.voice_edit(guild, user_id, MemberEdit::Move(Some(channel)));
            });
            menu.append(&item);
        }
        if !any {
            let none = label("No other voice channel", &["dim-label"]);
            none.set_margin_start(8);
            none.set_margin_bottom(6);
            menu.append(&none);
        }
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_max_content_height(420);
        scroll.set_child(Some(&menu));
        popover.set_child(Some(&scroll));
    }

    /// Confirms a kick, with an optional reason.
    pub(crate) fn kick_dialog(self: &Rc<Self>, guild: Snowflake, user_id: Snowflake, name: &str) {
        let dialog = adw::AlertDialog::new(
            Some(&format!("Kick {name}")),
            Some(&format!(
                "Are you sure you want to kick {name} from the server? They will be able to rejoin with a new invite."
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("kick", "Kick")]);
        dialog.set_response_appearance("kick", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let fields = gtk::Box::new(gtk::Orientation::Vertical, 6);
        fields.append(&field_caption("Reason for Kick"));
        let reason = reason_entry();
        fields.append(&reason);
        dialog.set_extra_child(Some(&fields));
        let app = self.clone();
        let name = name.to_owned();
        dialog.connect_response(None, move |_, response| {
            if response != "kick" {
                return;
            }
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let reason = reason_of(&reason);
            app.moderate("Could not kick", format!("{name} was kicked"), async move {
                http.kick_member(guild, user_id, reason.as_deref()).await
            });
        });
        dialog.present(Some(&self.window));
    }

    /// Confirms a ban: reason and how much message history to delete.
    pub(crate) fn ban_dialog(self: &Rc<Self>, guild: Snowflake, user_id: Snowflake, name: &str) {
        let dialog = adw::AlertDialog::new(
            Some(&format!("Ban {name}")),
            Some(&format!(
                "Are you sure you want to ban {name}? They will be removed and cannot rejoin until unbanned."
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("ban", "Ban")]);
        dialog.set_response_appearance("ban", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let fields = gtk::Box::new(gtk::Orientation::Vertical, 6);
        fields.append(&field_caption("Delete Message History"));
        let history = choice(BAN_HISTORY, 0);
        fields.append(&history);
        fields.append(&field_caption("Reason for Ban"));
        let reason = reason_entry();
        fields.append(&reason);
        dialog.set_extra_child(Some(&fields));
        let app = self.clone();
        let name = name.to_owned();
        dialog.connect_response(None, move |_, response| {
            if response != "ban" {
                return;
            }
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let seconds = BAN_HISTORY
                .get(history.selected() as usize)
                .map_or(0, |(_, seconds)| *seconds);
            let reason = reason_of(&reason);
            app.moderate("Could not ban", format!("{name} was banned"), async move {
                http.ban_member(guild, user_id, seconds, reason.as_deref())
                    .await
            });
        });
        dialog.present(Some(&self.window));
    }

    /// Times a member out for a chosen length, with an optional reason.
    pub(crate) fn timeout_dialog(
        self: &Rc<Self>,
        guild: Snowflake,
        user_id: Snowflake,
        name: &str,
    ) {
        let dialog = adw::AlertDialog::new(
            Some(&format!("Time Out {name}")),
            Some(
                "Members in timeout cannot send messages, react, join voice or speak until it ends.",
            ),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("timeout", "Time Out")]);
        dialog.set_response_appearance("timeout", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let fields = gtk::Box::new(gtk::Orientation::Vertical, 6);
        fields.append(&field_caption("Duration"));
        let duration = choice(TIMEOUTS, 0);
        fields.append(&duration);
        fields.append(&field_caption("Reason"));
        let reason = reason_entry();
        fields.append(&reason);
        dialog.set_extra_child(Some(&fields));
        let app = self.clone();
        let name = name.to_owned();
        dialog.connect_response(None, move |_, response| {
            if response != "timeout" {
                return;
            }
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let (length, seconds) = TIMEOUTS
                .get(duration.selected() as usize)
                .copied()
                .unwrap_or(TIMEOUTS[0]);
            let until = timeout_until(unix_millis_now(), seconds);
            let reason = reason_of(&reason);
            app.moderate(
                "Could not time out",
                format!("{name} is timed out for {}", length.to_lowercase()),
                async move {
                    http.edit_member(
                        guild,
                        user_id,
                        &MemberEdit::Timeout(Some(until)),
                        reason.as_deref(),
                    )
                    .await
                },
            );
        });
        dialog.present(Some(&self.window));
    }

    /// A checklist of the roles the account may give or take; each toggle
    /// is sent at once and undone if Discord refuses.
    pub(crate) fn roles_dialog(self: &Rc<Self>, guild: Snowflake, user_id: Snowflake) {
        let name = self.member_display_name(guild, user_id);
        let (roles, held): (Vec<Role>, Vec<Snowflake>) = self
            .with_session(|session| {
                session.store.read(|state| {
                    (
                        state.assignable_roles(guild),
                        state
                            .member(guild, user_id)
                            .map(|member| member.roles.clone())
                            .unwrap_or_default(),
                    )
                })
            })
            .unwrap_or_default();

        let dialog = adw::Dialog::new();
        dialog.set_title(&format!("Roles of {name}"));
        dialog.set_content_width(380);
        dialog.set_content_height(460);
        let toasts = adw::ToastOverlay::new();
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        list.set_valign(gtk::Align::Start);
        if roles.is_empty() {
            let row = adw::ActionRow::new();
            row.set_title("No roles you can assign");
            list.append(&row);
        }
        for role in roles {
            let row = adw::ActionRow::new();
            row.set_title(&glib::markup_escape_text(&role.name));
            let check = gtk::CheckButton::new();
            check.set_active(held.contains(&role.id));
            row.add_prefix(&check);
            row.add_suffix(&color_dot(role.color));
            row.set_activatable_widget(Some(&check));
            let app = self.clone();
            let toasts = toasts.downgrade();
            let reverting = Rc::new(Cell::new(false));
            let role_id = role.id;
            let role_name = role.name.clone();
            check.connect_toggled(move |check| {
                if reverting.get() {
                    return;
                }
                let Some((_, http, _)) = app.active() else {
                    return;
                };
                let add = check.is_active();
                let check = check.downgrade();
                let toasts = toasts.clone();
                let reverting = reverting.clone();
                let role_name = role_name.clone();
                crate::video_setup::spawn_local(
                    &app,
                    async move {
                        if add {
                            http.add_member_role(guild, user_id, role_id, None).await
                        } else {
                            http.remove_member_role(guild, user_id, role_id, None).await
                        }
                    },
                    move |app, result| {
                        let Err(error) = result else {
                            return;
                        };
                        let what = if add {
                            format!("Could not give {role_name}")
                        } else {
                            format!("Could not remove {role_name}")
                        };
                        tracing::warn!(%error, "role change failed");
                        match toasts.upgrade() {
                            Some(toasts) => {
                                let toast =
                                    adw::Toast::new(&format!("{what}: {}", error.describe()));
                                toast.set_timeout(8);
                                toasts.add_toast(toast);
                            }
                            None => app.report_failure(&what, &error),
                        }
                        if let Some(check) = check.upgrade() {
                            reverting.set(true);
                            check.set_active(!add);
                            reverting.set(false);
                        }
                    },
                );
            });
            list.append(&row);
        }
        let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        body.set_margin_top(12);
        body.set_margin_bottom(12);
        body.set_margin_start(12);
        body.set_margin_end(12);
        let hint = label(
            "Only roles below your highest role can be given or removed.",
            &["caption", "dim-label"],
        );
        hint.set_ellipsize(gtk::pango::EllipsizeMode::None);
        hint.set_wrap(true);
        hint.set_max_width_chars(1);
        hint.set_hexpand(true);
        body.append(&hint);
        body.append(&list);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&body));
        toasts.set_child(Some(&scroll));
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&toasts));
        dialog.set_child(Some(&view));
        dialog.present(Some(&self.window));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_end_after_their_length_in_utc() {
        let now = kc_state::time::iso_to_unix_millis("2026-11-19T23:59:30+00:00").unwrap();
        assert_eq!(timeout_until(now, 60), "2026-11-20T00:00:30+00:00");
        assert_eq!(timeout_until(now, 604_800), "2026-11-26T23:59:30+00:00");
        let lengths: Vec<u64> = TIMEOUTS.iter().map(|(_, seconds)| *seconds).collect();
        assert_eq!(lengths, [60, 300, 600, 3_600, 86_400, 604_800]);
    }

    #[test]
    fn ban_history_stays_within_discords_week() {
        assert!(BAN_HISTORY.iter().all(|(_, seconds)| *seconds <= 604_800));
        assert_eq!(BAN_HISTORY[0].1, 0);
    }
}
