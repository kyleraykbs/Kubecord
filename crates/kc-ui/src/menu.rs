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
type MenuAction = Box<dyn Fn(&Rc<AppState>)>;

/// What the menu needs to know about a user to decide which items to show.
struct UserContext {
    is_me: bool,
    bot: bool,
    relationship: Option<u8>,
    guild: Option<Snowflake>,
    nick: Option<String>,
    owner: bool,
    /// In a voice call together, so local volume/mute make sense.
    in_voice: bool,
    streaming: bool,
    watching: bool,
    /// A camera (`false`) or stream (`true`) picture is arriving for them.
    video: Option<bool>,
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
                let owner = guild
                    .and_then(|guild| state.guilds.get(&guild))
                    .and_then(|guild| guild.owner_id)
                    == account_user;
                Some(UserContext {
                    is_me: account_user == Some(user_id),
                    bot: user.bot == Some(true),
                    relationship: state.relationships.get(&user_id).map(|r| r.kind),
                    guild,
                    nick: guild.and_then(|guild| state.member(guild, user_id)?.nick.clone()),
                    owner,
                    in_voice,
                    streaming,
                    watching,
                    video: None,
                })
            })
        })
        .flatten()
    }

    fn show_user_menu(self: &Rc<Self>, user_id: Snowflake, anchor: &gtk::Widget, x: f64, y: f64) {
        let Some((account, _, _)) = self.active() else {
            return;
        };
        let account_user = self
            .with_session(|session| session.user.as_ref().map(|u| u.id))
            .flatten();
        let Some(mut ctx) = self.user_context(account_user, user_id) else {
            return;
        };
        // The video maps live on the UI side, not in the store.
        ctx.video = {
            let videos = self.videos.borrow();
            if videos.contains_key(&(account, user_id, true)) {
                Some(true)
            } else if videos.contains_key(&(account, user_id, false)) {
                Some(false)
            } else {
                None
            }
        };

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

        // -- voice: local volume and mute --------------------------------
        if ctx.in_voice && !ctx.is_me {
            separator();
            menu.append(&self.volume_control(account, user_id));
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
        if let Some(is_stream) = ctx.video {
            let what = if is_stream {
                "Focus Stream"
            } else {
                "Focus Camera"
            };
            add(
                what,
                false,
                Box::new(move |app| app.set_focused_video(Some((user_id, is_stream)))),
                self,
            );
        }

        // -- guild nickname ----------------------------------------------
        if let Some(guild) = ctx.guild
            && (ctx.is_me || ctx.owner)
        {
            let nick = ctx.nick.clone();
            let is_me = ctx.is_me;
            add(
                "Change Nickname",
                false,
                Box::new(move |app| {
                    app.change_nickname(guild, user_id, is_me, nick.clone());
                }),
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
    }

    /// A labelled slider for one person's local playback volume (0–200%).
    fn volume_control(
        self: &Rc<Self>,
        account: crate::session::AccountId,
        user_id: Snowflake,
    ) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
        row.set_margin_start(8);
        row.set_margin_end(8);
        row.set_margin_top(4);
        row.append(&label("User Volume", &["caption", "dim-label"]));
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 200.0, 1.0);
        scale.set_hexpand(true);
        scale.set_draw_value(false);
        let current = self
            .user_volumes
            .borrow()
            .get(&user_id)
            .map(|volume| (*volume as f64 * 100.0).clamp(0.0, 200.0))
            .unwrap_or(100.0);
        scale.set_value(current);
        scale.add_mark(100.0, gtk::PositionType::Bottom, None);
        let app = self.clone();
        scale.connect_value_changed(move |scale| {
            app.set_user_volume(account, user_id, scale.value() / 100.0);
        });
        row.append(&scale);
        row
    }

    /// Records and applies a per-user playback volume (1.0 = unchanged).
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
        crate::video_setup::spawn_local(
            self,
            async move { http.accept_friend(user_id).await },
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

    /// Says Discord wants a captcha, which kylecord cannot show, and offers
    /// to finish in the browser where the user is signed in.
    pub(crate) fn captcha_needed(&self, text: &str, url: &str) {
        let toast = adw::Toast::builder()
            .title(text)
            .button_label("Open in Browser")
            .timeout(10)
            .build();
        let window = self.window.clone();
        let url = url.to_owned();
        toast.connect_button_clicked(move |_| {
            gtk::UriLauncher::new(&url).launch(
                Some(&window),
                None::<&gtk::gio::Cancellable>,
                |_| {},
            );
        });
        self.toasts.add_toast(toast);
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

    /// Opens a small dialog to change a guild nickname (own or, for the
    /// owner, someone else's). An empty value clears it.
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
                    Err(error) => app.set_status(&format!("Could not change nickname: {error}")),
                },
            );
        });
        dialog.present(Some(&self.window));
    }
}
