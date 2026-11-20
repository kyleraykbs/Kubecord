//! The right-click user context menu, shared by the member list, voice
//! tiles, sidebar voice rows and message authors. Built fresh on each click
//! so it reflects the current relationship, voice and video state.

use std::rc::Rc;

use gtk::gdk;
use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::Snowflake;
use kc_state::Relationship;

use crate::session::Command;
use crate::shell::AppState;
use crate::widgets::label;

/// What a menu item does when chosen.
pub(crate) type MenuAction = Box<dyn Fn(&Rc<AppState>)>;

/// What the menu needs to know about a user to decide which items to show.
struct UserContext {
    is_me: bool,
    bot: bool,
    relationship: Option<u8>,
    guild: Option<Snowflake>,
    /// In a voice call together, so local volume/mute make sense.
    in_voice: bool,
    streaming: bool,
    watching: bool,
}

/// One item of a context menu: a flat button with a left-aligned label,
/// red when it does something destructive.
pub(crate) fn menu_item(text: &str, danger: bool) -> gtk::Button {
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
    button
}

impl AppState {
    /// Parents a popover to the window and points it at `(x, y)` given in
    /// `anchor`'s coordinates. The window outlives any sidebar or list
    /// rebuild, so a redraw can't orphan the menu mid-display and wedge the
    /// input grab (which froze the UI when a voice row was rebuilt under an
    /// open menu).
    pub(crate) fn anchor_popover(
        &self,
        popover: &gtk::Popover,
        anchor: &impl IsA<gtk::Widget>,
        x: f64,
        y: f64,
    ) {
        let parent: gtk::Widget = self.window.clone().upcast();
        let (px, py) = anchor
            .compute_point(&parent, &gtk::graphene::Point::new(x as f32, y as f32))
            .map(|point| (point.x() as f64, point.y() as f64))
            .unwrap_or((x, y));
        popover.set_parent(&parent);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_valign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(px as i32, py as i32, 1, 1)));
        popover.connect_closed(|popover| popover.unparent());
    }

    /// Attaches the right-click user menu to a widget standing for `user_id`.
    pub(crate) fn attach_user_menu(
        self: &Rc<Self>,
        widget: &impl IsA<gtk::Widget>,
        user_id: Snowflake,
    ) {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        let app = self.clone();
        let anchor = widget.clone().upcast::<gtk::Widget>().downgrade();
        gesture.connect_pressed(move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            if let Some(anchor) = anchor.upgrade() {
                app.show_user_menu(user_id, &anchor, x, y);
            }
        });
        widget.add_controller(gesture);
    }

    fn user_context(
        &self,
        account_user: Option<Snowflake>,
        user_id: Snowflake,
    ) -> Option<UserContext> {
        self.with_session(|session| {
            let guild = session.selected_guild.filter(|_| !session.dm_mode);
            let voice = session.voice.as_ref();
            let watching = voice.and_then(|voice| voice.watching) == Some(user_id);
            session.store.read(|state| {
                let user = state.users.get(&user_id)?;
                let in_voice = voice.is_some_and(|voice| {
                    state
                        .voice_states
                        .values()
                        .any(|vs| vs.channel_id == Some(voice.channel_id) && vs.user_id == user_id)
                });
                let streaming = state
                    .voice_states
                    .values()
                    .any(|vs| vs.user_id == user_id && vs.self_stream);
                Some(UserContext {
                    is_me: account_user == Some(user_id),
                    bot: user.bot == Some(true),
                    relationship: state.relationships.get(&user_id).map(|r| r.kind),
                    guild,
                    in_voice,
                    streaming,
                    watching,
                })
            })
        })
        .flatten()
    }

    /// Shows the user menu at `(x, y)` of `anchor`, and returns it.
    pub(crate) fn show_user_menu(
        self: &Rc<Self>,
        user_id: Snowflake,
        anchor: &gtk::Widget,
        x: f64,
        y: f64,
    ) -> Option<gtk::Popover> {
        let (account, _, _) = self.active()?;
        let account_user = self
            .with_session(|session| session.user.as_ref().map(|u| u.id))
            .flatten();
        let ctx = self.user_context(account_user, user_id)?;

        let popover = gtk::Popover::new();
        self.anchor_popover(&popover, anchor, x, y);

        let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
        menu.add_css_class("kc-menu");
        menu.set_width_request(200);

        // Builds a menu item that dismisses the popover, then acts.
        let add = {
            let popover = popover.clone();
            let menu = menu.clone();
            move |text: &str, danger: bool, action: MenuAction, app: &Rc<AppState>| {
                let button = menu_item(text, danger);
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

        let anchor_weak = anchor.downgrade();
        add(
            "Profile",
            false,
            Box::new(move |app| {
                if let Some(anchor) = anchor_weak.upgrade() {
                    app.show_profile(user_id, &anchor);
                }
            }),
            self,
        );
        add(
            "Mention",
            false,
            Box::new(move |app| app.insert_composer_text(&format!("<@{user_id}> "))),
            self,
        );
        if !ctx.is_me {
            add(
                "Message",
                false,
                Box::new(move |app| app.open_dm_with(user_id)),
                self,
            );
            if !ctx.bot {
                add(
                    "Start a Call",
                    false,
                    Box::new(move |app| app.start_call_with(user_id)),
                    self,
                );
            }
        }

        // -- voice: your own video preview ---------------------------------
        if ctx.in_voice && ctx.is_me {
            separator();
            let preview = gtk::CheckButton::with_label("Preview My Camera and Stream");
            preview.set_tooltip_text(Some(
                "Show your own video in the call. Turning it off saves work on a slow machine.",
            ));
            preview.set_active(self.capture.borrow().self_preview);
            let app = self.clone();
            preview.connect_toggled(move |check| app.set_self_preview(check.is_active()));
            menu.append(&preview);
        }

        // -- voice: local volume and mute --------------------------------
        if ctx.in_voice && !ctx.is_me {
            separator();
            menu.append(&self.volume_control(account, user_id));
            // A second slider for their shared stream's sound, offered while
            // they are screen-sharing (their stream rides a separate mixer
            // entry from their voice).
            if ctx.streaming {
                menu.append(&self.stream_volume_control(account, user_id));
            }
            let muted = self
                .user_volumes
                .borrow()
                .get(&user_id)
                .is_some_and(|volume| *volume <= 0.0);
            add(
                if muted { "Unmute" } else { "Mute" },
                false,
                Box::new(move |app| {
                    let volume = if muted { 1.0 } else { 0.0 };
                    app.set_user_volume(account, user_id, volume);
                }),
                self,
            );
        }

        // -- video -------------------------------------------------------
        if ctx.streaming && !ctx.watching && !ctx.is_me {
            add(
                "Watch Stream",
                false,
                Box::new(move |app| {
                    let _ = app
                        .cmd_tx
                        .try_send(Command::WatchStream { account, user_id });
                }),
                self,
            );
        }
        // Each picture arriving for them (their stream, their camera) can
        // be enlarged, or moved into a window of its own and back.
        for is_stream in [true, false] {
            let key = (account, user_id, is_stream);
            if !self.videos.borrow().contains_key(&key) {
                continue;
            }
            let what = if is_stream { "Stream" } else { "Camera" };
            if self.popouts.borrow().contains_key(&key) {
                add(
                    &format!("Pop {what} Back In"),
                    false,
                    Box::new(move |app| app.pop_in_video(key)),
                    self,
                );
                continue;
            }
            add(
                &format!("Focus {what}"),
                false,
                Box::new(move |app| app.set_focused_video(Some((user_id, is_stream)))),
                self,
            );
            let title = self
                .with_session(|session| {
                    session.store.read(|state| {
                        state
                            .users
                            .get(&user_id)
                            .map(|user| user.display_name().to_owned())
                    })
                })
                .flatten()
                .map(|name| format!("{name}'s {}", what.to_lowercase()))
                .unwrap_or_else(|| what.to_owned());
            add(
                &format!("Pop Out {what}"),
                false,
                Box::new(move |app| app.pop_out_video(key, &title)),
                self,
            );
        }

        // -- moderation: nickname, roles, voice, timeout, kick, ban ------
        if let Some(guild) = ctx.guild {
            self.add_moderation_items(&menu, &popover, guild, user_id, ctx.is_me);
        }

        // -- the open group DM, when the account owns it ------------------
        if !ctx.is_me
            && let Some(group) = self.owned_group_with(user_id)
        {
            separator();
            add(
                "Remove From Group",
                true,
                Box::new(move |app| app.remove_from_group(group, user_id)),
                self,
            );
        }

        // -- relationships -----------------------------------------------
        if !ctx.is_me && !ctx.bot {
            separator();
            let (text, add_friend) = match ctx.relationship {
                Some(Relationship::FRIEND) => ("Remove Friend", false),
                Some(Relationship::INCOMING_REQUEST) => ("Accept Friend Request", true),
                Some(Relationship::OUTGOING_REQUEST) => ("Cancel Friend Request", false),
                Some(Relationship::BLOCKED) => ("Unblock", false),
                _ => ("Add Friend", true),
            };
            let danger = matches!(text, "Remove Friend");
            add(
                text,
                danger,
                Box::new(move |app| {
                    if add_friend {
                        app.befriend_user(user_id);
                    } else {
                        app.remove_relationship(user_id);
                    }
                }),
                self,
            );
            if ctx.relationship != Some(Relationship::BLOCKED) {
                add(
                    "Block",
                    true,
                    Box::new(move |app| app.block_user(user_id)),
                    self,
                );
            }
        }

        // -- always ------------------------------------------------------
        separator();
        let anchor_weak = anchor.downgrade();
        add(
            "Copy User ID",
            false,
            Box::new(move |_| {
                if let Some(anchor) = anchor_weak.upgrade() {
                    anchor.clipboard().set_text(&user_id.to_string());
                }
            }),
            self,
        );

        popover.set_child(Some(&menu));
        popover.popup();
        Some(popover)
    }

    /// A labelled slider for one person's local voice playback volume, from
    /// silent up to the boost allowed in Settings → Features.
    fn volume_control(
        self: &Rc<Self>,
        account: crate::session::AccountId,
        user_id: Snowflake,
    ) -> gtk::Box {
        let start = self
            .user_volumes
            .borrow()
            .get(&user_id)
            .copied()
            .unwrap_or(1.0);
        let app = self.clone();
        self.playback_slider("User Volume", start, move |volume| {
            app.set_user_volume(account, user_id, volume);
        })
    }

    /// A labelled slider for the sound of one person's shared stream, on the
    /// same scale as the voice slider but a separate, remembered value.
    fn stream_volume_control(
        self: &Rc<Self>,
        account: crate::session::AccountId,
        user_id: Snowflake,
    ) -> gtk::Box {
        let start = self
            .stream_volumes
            .borrow()
            .get(&user_id)
            .copied()
            .unwrap_or(1.0);
        let app = self.clone();
        self.playback_slider("Stream Volume", start, move |volume| {
            app.set_stream_volume(account, user_id, volume);
        })
    }

    /// Builds one playback-volume slider: `start` is the current value
    /// (1.0 = unchanged); `on_change` is called with the new fraction. The
    /// top is the boost allowed in Settings → Features.
    fn playback_slider(
        self: &Rc<Self>,
        caption: &str,
        start: f32,
        on_change: impl Fn(f64) + 'static,
    ) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
        row.set_margin_start(8);
        row.set_margin_end(8);
        row.set_margin_top(4);
        row.append(&label(caption, &["caption", "dim-label"]));
        let most = f64::from(self.capture.borrow().volume_boost_cap) * 100.0;
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, most, 1.0);
        scale.set_hexpand(true);
        scale.set_draw_value(false);
        let current = (f64::from(start) * 100.0).clamp(0.0, most);
        scale.set_value(current);
        let percent = gtk::Label::new(Some(&format!("{current:.0}%")));
        percent.add_css_class("caption");
        percent.add_css_class("dim-label");
        percent.set_halign(gtk::Align::End);
        let mut mark = 100.0;
        while mark <= most {
            scale.add_mark(mark, gtk::PositionType::Bottom, None);
            mark += 100.0;
        }
        let shown = percent.clone();
        scale.connect_value_changed(move |scale| {
            shown.set_label(&format!("{:.0}%", scale.value()));
        });
        scale.connect_value_changed(move |scale| {
            on_change(scale.value() / 100.0);
        });
        row.append(&scale);
        row.append(&percent);
        row
    }

    /// Records and applies a per-user voice playback volume (1.0 =
    /// unchanged).
    pub(crate) fn set_user_volume(
        self: &Rc<Self>,
        account: crate::session::AccountId,
        user_id: Snowflake,
        volume: f64,
    ) {
        self.user_volumes
            .borrow_mut()
            .insert(user_id, volume as f32);
        let _ = self.cmd_tx.try_send(Command::SetUserVolume {
            account,
            user_id,
            volume: volume as f32,
        });
    }

    /// Records and applies a per-user stream (screen-share sound) playback
    /// volume (1.0 = unchanged), kept apart from their voice.
    pub(crate) fn set_stream_volume(
        self: &Rc<Self>,
        account: crate::session::AccountId,
        user_id: Snowflake,
        volume: f64,
    ) {
        self.stream_volumes
            .borrow_mut()
            .insert(user_id, volume as f32);
        let _ = self.cmd_tx.try_send(Command::SetStreamVolume {
            account,
            user_id,
            volume: volume as f32,
        });
    }

    /// Sends a friend request to a user already known to the client.
    pub(crate) fn befriend_user(self: &Rc<Self>, user_id: Snowflake) {
        self.befriend_user_then(user_id, |_| {});
    }

    /// Sends (or accepts) a friend request; `done` learns whether Discord
    /// took it.
    pub(crate) fn befriend_user_then(
        self: &Rc<Self>,
        user_id: Snowflake,
        done: impl FnOnce(bool) + 'static,
    ) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        self.with_captcha(
            "Discord wants to make sure you're human before it sends your friend request.",
            move |captcha| {
                let http = http.clone();
                async move { http.accept_friend(user_id, captcha.as_ref()).await }
            },
            move |app, result| match result {
                Ok(()) => {
                    app.set_status("Friend request sent");
                    done(true);
                }
                Err(error) => {
                    if error.is_captcha() {
                        app.captcha_needed(
                            "Discord wants a captcha solved to send this friend request.",
                            &format!("https://discord.com/users/{user_id}"),
                        );
                    } else {
                        app.set_status(&format!("Friend request failed: {error}"));
                    }
                    done(false);
                }
            },
        );
    }

    /// Shows or hides your own camera and stream in the call, from the tile
    /// menu or from Settings.
    pub(crate) fn set_self_preview(self: &Rc<Self>, show: bool) {
        self.update_prefs(|prefs| prefs.self_preview = show);
    }

    /// Says Discord wants a captcha, which kylecord cannot show, and offers
    /// to finish in the browser where the user is signed in.
    pub(crate) fn captcha_needed(&self, text: &str, url: &str) {
        let window = self.window.clone();
        let url = url.to_owned();
        let open = Box::new(move || {
            gtk::UriLauncher::new(&url).launch(
                Some(&window),
                None::<&gtk::gio::Cancellable>,
                |_| {},
            );
        });
        self.notify(text, Some(("Open in Browser", open)), 10);
    }

    /// Blocks a user.
    pub(crate) fn block_user(self: &Rc<Self>, user_id: Snowflake) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        self.spawn(
            async move { http.block_user(user_id).await },
            |app, result| match result {
                Ok(()) => app.set_status("Blocked"),
                Err(error) => app.set_status(&format!("Could not block: {error}")),
            },
        );
    }

    /// Opens a small dialog to change a guild nickname (own, or someone
    /// else's with Manage Nicknames). An empty value clears it.
    pub(crate) fn change_nickname(
        self: &Rc<Self>,
        guild: Snowflake,
        user_id: Snowflake,
        is_me: bool,
        current: Option<String>,
    ) {
        let dialog = adw::AlertDialog::new(Some("Change Nickname"), None);
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");

        let entry = gtk::Entry::new();
        entry.set_text(current.as_deref().unwrap_or(""));
        entry.set_placeholder_text(Some("Nickname"));
        entry.set_margin_top(8);
        dialog.set_extra_child(Some(&entry));

        let app = self.clone();
        let entry_weak = entry.downgrade();
        dialog.connect_response(None, move |dialog, response| {
            if response != "save" {
                return;
            }
            let Some(entry) = entry_weak.upgrade() else {
                return;
            };
            let nick = entry.text().to_string();
            dialog.close();
            let Some((_, http, _)) = app.active() else {
                return;
            };
            app.spawn(
                async move { http.set_nickname(guild, user_id, is_me, &nick).await },
                |app, result| match result {
                    Ok(()) => app.set_status("Nickname updated"),
                    Err(error) => app.report_failure("Could not change the nickname", &error),
                },
            );
        });
        dialog.present(Some(&self.window));
    }
}
