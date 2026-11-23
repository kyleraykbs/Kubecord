//! Stage channels: speakers up front, the audience listening, hands raised
//! to speak, and moderators who start the stage, set its topic, invite
//! people up and move them back.
//!
//! As Discord documents them: a stage is live while it has a stage instance
//! (`POST/PATCH/DELETE /stage-instances`); someone in the audience is
//! `suppress`ed, and raises a hand with `request_to_speak_timestamp`
//! (`PATCH /guilds/{guild}/voice-states/@me`); a moderator (Manage
//! Channels, Mute Members and Move Members) changes others' `suppress`
//! (`PATCH /guilds/{guild}/voice-states/{user}`). While suppressed, the
//! microphone here stays off.

use std::cell::Cell;
use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::Snowflake;
use kc_http::cdn;
use kc_state::{ChannelType, permissions};

use crate::session::Command;
use crate::shell::AppState;
use crate::video_setup::spawn_local;
use crate::widgets::{clear_box, label};

/// What a stage moderator needs, all three.
const MODERATOR: u64 = permissions::MANAGE_CHANNELS | permissions::MUTE_MEMBERS | permissions::MOVE_MEMBERS;

thread_local! {
    /// What the voice connection was last told about suppression.
    static SUPPRESSED: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Someone on the stage.
#[derive(Clone, Debug)]
struct OnStage {
    id: Snowflake,
    name: String,
    avatar: String,
    speaker: bool,
    hand: bool,
    muted: bool,
    is_self: bool,
}

/// The stage as shown.
struct StageView {
    guild_id: Snowflake,
    channel_id: Snowflake,
    name: String,
    topic: Option<String>,
    people: Vec<OnStage>,
    joined: bool,
    moderator: bool,
}

impl AppState {
    /// Whether `channel_id` is a stage channel.
    pub(crate) fn is_stage(&self, channel_id: Snowflake) -> bool {
        self.with_session(|session| {
            session
                .store
                .read(|state| state.channels.get(&channel_id).map(|channel| channel.kind == ChannelType::GuildStageVoice))
        })
        .flatten()
        .unwrap_or(false)
    }

    fn stage_view(&self, channel_id: Snowflake) -> Option<StageView> {
        self.with_session(|session| {
            let joined = session.voice.as_ref().is_some_and(|voice| voice.channel_id == channel_id);
            let me = session.user.as_ref().map(|user| user.id);
            session.store.read(|state| {
                let channel = state.channels.get(&channel_id)?;
                let guild_id = channel.guild_id?;
                let mut people: Vec<OnStage> = state
                    .voice_states
                    .values()
                    .filter(|voice| voice.channel_id == Some(channel_id))
                    .filter_map(|voice| {
                        let user = state.users.get(&voice.user_id)?;
                        Some(OnStage {
                            id: user.id,
                            name: state.display_name(Some(guild_id), user),
                            avatar: cdn::user_avatar(user.id, user.avatar.as_deref(), 128),
                            speaker: !voice.suppress,
                            hand: voice.suppress && voice.request_to_speak_timestamp.is_some(),
                            muted: voice.mute || voice.self_mute,
                            is_self: Some(user.id) == me,
                        })
                    })
                    .collect();
                people.sort_by_key(|person| person.name.to_lowercase());
                Some(StageView {
                    guild_id,
                    channel_id,
                    name: channel.display_name(),
                    topic: state.stage_instance(channel_id).map(|stage| stage.topic.clone()),
                    people,
                    joined,
                    moderator: state.permissions_in(channel_id) & MODERATOR == MODERATOR,
                })
            })
        })
        .flatten()
    }

    /// Keeps the microphone off while this account is in a stage's
    /// audience (after each voice state change).
    pub(crate) fn sync_stage_suppression(&self) {
        let Some(account) = self.model.borrow().active else {
            return;
        };
        let suppressed = self
            .with_session(|session| {
                let voice = session.voice.as_ref()?;
                let me = session.user.as_ref()?.id;
                session.store.read(|state| {
                    let channel = state.channels.get(&voice.channel_id)?;
                    if channel.kind != ChannelType::GuildStageVoice {
                        return Some(false);
                    }
                    // Until Discord says otherwise, a stage's newcomer listens.
                    Some(state.voice_states.get(&(channel.guild_id, me)).is_none_or(|own| own.suppress))
                })
            })
            .flatten()
            .unwrap_or(false);
        if SUPPRESSED.get() != Some(suppressed) {
            SUPPRESSED.set(Some(suppressed));
            let _ = self.cmd_tx.try_send(Command::SetSuppressed { account, suppressed });
        }
    }

    /// The stage: who is speaking, who is listening, hands raised, and what
    /// this account can do.
    pub(crate) fn render_stage_view(self: &Rc<Self>, channel_id: Snowflake) {
        clear_box(&self.voice_view);
        self.voice_tiles.borrow_mut().clear();
        let Some(view) = self.stage_view(channel_id) else {
            return;
        };
        self.voice_view.set_valign(gtk::Align::Fill);
        let page = gtk::Box::new(gtk::Orientation::Vertical, 14);
        page.set_margin_top(18);
        page.set_margin_bottom(18);
        page.set_margin_start(18);
        page.set_margin_end(18);

        // The topic, and the moderator's controls for it.
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        header.add_css_class("card");
        header.add_css_class("kc-stage-header");
        let icon = gtk::Image::from_icon_name("audio-input-microphone-symbolic");
        icon.set_pixel_size(28);
        header.append(&icon);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        let topic = label(view.topic.as_deref().unwrap_or("Nothing is live on this stage"), &["title-4"]);
        topic.set_xalign(0.0);
        topic.set_wrap(true);
        text.append(&topic);
        let speakers = view.people.iter().filter(|person| person.speaker).count();
        let listening = view.people.len() - speakers;
        let facts = label(
            &format!(
                "{} · {speakers} speaking · {listening} listening{}",
                view.name,
                if view.topic.is_some() { " · Live" } else { "" }
            ),
            &["caption", "dim-label"],
        );
        facts.set_xalign(0.0);
        text.append(&facts);
        header.append(&text);
        if view.moderator {
            if view.topic.is_some() {
                let edit = gtk::Button::with_label("Edit Topic");
                edit.set_valign(gtk::Align::Center);
                let (app, current) = (self.clone(), view.topic.clone().unwrap_or_default());
                edit.connect_clicked(move |_| app.stage_topic_dialog(channel_id, Some(current.clone())));
                header.append(&edit);
                let end = gtk::Button::with_label("End Stage");
                end.add_css_class("destructive-action");
                end.set_valign(gtk::Align::Center);
                let app = self.clone();
                end.connect_clicked(move |_| app.confirm_end_stage(channel_id));
                header.append(&end);
            } else {
                let start = gtk::Button::with_label("Start Stage");
                start.add_css_class("suggested-action");
                start.set_valign(gtk::Align::Center);
                let app = self.clone();
                start.connect_clicked(move |_| app.stage_topic_dialog(channel_id, None));
                header.append(&start);
            }
        }
        page.append(&header);

        // Speakers, large.
        page.append(&section("Speakers"));
        let stage = gtk::FlowBox::new();
        stage.set_selection_mode(gtk::SelectionMode::None);
        stage.set_max_children_per_line(6);
        stage.set_row_spacing(12);
        stage.set_column_spacing(12);
        let mut any = false;
        for person in view.people.iter().filter(|person| person.speaker) {
            any = true;
            let card = self.stage_person(&view, person, 72);
            self.voice_tiles.borrow_mut().insert(person.id, card.clone().upcast());
            stage.append(&card);
        }
        if !any {
            let empty = label("No one is speaking", &["dim-label"]);
            empty.set_xalign(0.0);
            page.append(&empty);
        } else {
            page.append(&stage);
        }

        // Raised hands, then the audience.
        let hands: Vec<&OnStage> = view.people.iter().filter(|person| person.hand).collect();
        if !hands.is_empty() {
            page.append(&section(&format!("Requested to Speak — {}", hands.len())));
            let list = gtk::ListBox::new();
            list.add_css_class("boxed-list");
            list.set_selection_mode(gtk::SelectionMode::None);
            for person in hands {
                let row = adw::ActionRow::new();
                row.set_title(&glib::markup_escape_text(&person.name));
                row.add_prefix(&self.images.avatar(32, &person.name, Some(&person.avatar)));
                // No hand in the icon theme.
                let hand = label("✋", &[]);
                hand.set_tooltip_text(Some("Hand raised"));
                row.add_suffix(&hand);
                if view.moderator {
                    let invite = gtk::Button::with_label("Invite to Speak");
                    invite.set_valign(gtk::Align::Center);
                    let (app, guild, user) = (self.clone(), view.guild_id, person.id);
                    invite.connect_clicked(move |button| {
                        button.set_sensitive(false);
                        app.set_speaker(guild, user, channel_id, false);
                    });
                    row.add_suffix(&invite);
                }
                list.append(&row);
            }
            page.append(&list);
        }
        page.append(&section(&format!("Audience — {listening}")));
        let audience = gtk::FlowBox::new();
        audience.set_selection_mode(gtk::SelectionMode::None);
        audience.set_max_children_per_line(10);
        audience.set_row_spacing(8);
        audience.set_column_spacing(8);
        for person in view.people.iter().filter(|person| !person.speaker) {
            audience.append(&self.stage_person(&view, person, 40));
        }
        page.append(&audience);

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&page));
        self.voice_view.append(&scroll);
        self.voice_view.append(&self.stage_actions(&view));
    }

    /// Someone's face and name; a moderator can click to move them.
    fn stage_person(self: &Rc<Self>, view: &StageView, person: &OnStage, size: i32) -> gtk::Widget {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 4);
        card.add_css_class("kc-stage-person");
        card.set_width_request(size + 32);
        let avatar = self.images.avatar(size, &person.name, Some(&person.avatar));
        avatar.set_halign(gtk::Align::Center);
        card.append(&avatar);
        let name = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        name.set_halign(gtk::Align::Center);
        if person.speaker && person.muted {
            let muted = gtk::Image::from_icon_name("microphone-disabled-symbolic");
            muted.set_pixel_size(12);
            name.append(&muted);
        }
        let text = label(&person.name, if size > 48 { &[] } else { &["caption"] });
        text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.set_max_width_chars(12);
        name.append(&text);
        card.append(&name);
        if view.moderator && !person.is_self {
            let click = gtk::GestureClick::new();
            let (app, guild, user, speaker) = (self.clone(), view.guild_id, person.id, person.speaker);
            let channel_id = view.channel_id;
            let target = card.clone();
            click.connect_released(move |_, _, _, _| {
                let popover = gtk::Popover::new();
                popover.set_parent(&target);
                popover.connect_closed(|popover| popover.unparent());
                let item = crate::menu::menu_item(if speaker { "Move to Audience" } else { "Invite to Speak" }, false);
                let (app, menu) = (app.clone(), popover.clone());
                item.connect_clicked(move |_| {
                    menu.popdown();
                    app.set_speaker(guild, user, channel_id, speaker);
                });
                popover.set_child(Some(&item));
                popover.popup();
            });
            card.add_controller(click);
            card.set_cursor_from_name(Some("pointer"));
        }
        card.upcast()
    }

    /// The bar along the bottom: join, raise a hand, speak, step down.
    fn stage_actions(self: &Rc<Self>, view: &StageView) -> gtk::Widget {
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        bar.set_halign(gtk::Align::Center);
        bar.set_margin_bottom(14);
        let (guild, channel_id) = (view.guild_id, view.channel_id);
        if !view.joined {
            let join = gtk::Button::with_label("Join Stage");
            join.add_css_class("suggested-action");
            join.add_css_class("pill");
            let app = self.clone();
            join.connect_clicked(move |button| {
                button.set_sensitive(false);
                let channel = app
                    .with_session(|session| session.store.read(|state| state.channels.get(&channel_id).cloned()))
                    .flatten();
                if let Some(channel) = channel {
                    app.join_voice_channel(&channel);
                }
            });
            bar.append(&join);
            return bar.upcast();
        }
        let Some(me) = view.people.iter().find(|person| person.is_self) else {
            return bar.upcast();
        };
        if me.speaker {
            let down = gtk::Button::with_label("Move to Audience");
            down.add_css_class("pill");
            let app = self.clone();
            down.connect_clicked(move |_| app.set_stage_self(guild, channel_id, Some(true), None));
            bar.append(&down);
        } else {
            let hand = gtk::Button::with_label(if me.hand { "Lower Hand" } else { "Request to Speak" });
            hand.add_css_class("pill");
            if !me.hand {
                hand.add_css_class("suggested-action");
            }
            let (app, raise) = (self.clone(), !me.hand);
            hand.connect_clicked(move |_| app.set_stage_self(guild, channel_id, None, Some(raise)));
            bar.append(&hand);
            // Moderators step up themselves; others once invited (Discord
            // refuses it until then).
            if view.moderator || me.hand {
                let speak = gtk::Button::with_label(if view.moderator { "Become a Speaker" } else { "Start Speaking" });
                speak.add_css_class("pill");
                if !view.moderator {
                    speak.set_tooltip_text(Some("Works once a moderator invites you up"));
                }
                let app = self.clone();
                speak.connect_clicked(move |_| app.set_stage_self(guild, channel_id, Some(false), Some(false)));
                bar.append(&speak);
            }
        }
        bar.upcast()
    }

    fn set_stage_self(self: &Rc<Self>, guild: Snowflake, channel_id: Snowflake, suppress: Option<bool>, hand: Option<bool>) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        spawn_local(self, async move { http.set_stage_self(guild, channel_id, suppress, hand).await }, |app, result| {
            if let Err(error) = result {
                app.set_status(&format!("Could not change your place on the stage: {error}"));
            }
        });
    }

    fn set_speaker(self: &Rc<Self>, guild: Snowflake, user: Snowflake, channel_id: Snowflake, suppress: bool) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        spawn_local(
            self,
            async move { http.set_stage_speaker(guild, user, channel_id, suppress).await },
            move |app, result| {
                if let Err(error) = result {
                    app.set_status(&format!(
                        "Could not {}: {error}",
                        if suppress { "move them to the audience" } else { "invite them to speak" }
                    ));
                }
            },
        );
    }

    /// Starts the stage (`current: None`) or changes its topic.
    fn stage_topic_dialog(self: &Rc<Self>, channel_id: Snowflake, current: Option<String>) {
        let starting = current.is_none();
        let dialog = adw::AlertDialog::new(
            Some(if starting { "Start a Stage" } else { "Edit Topic" }),
            Some("What is the stage about?"),
        );
        let fields = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let entry = gtk::Entry::new();
        entry.set_max_length(120);
        entry.set_placeholder_text(Some("Topic"));
        entry.set_activates_default(true);
        if let Some(current) = &current {
            entry.set_text(current);
        }
        fields.append(&entry);
        let notify = gtk::CheckButton::with_label("Notify server members");
        if starting {
            fields.append(&notify);
        }
        dialog.set_extra_child(Some(&fields));
        dialog.add_responses(&[("cancel", "Cancel"), ("go", if starting { "Start Stage" } else { "Save" })]);
        dialog.set_response_appearance("go", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("go"));
        dialog.set_close_response("cancel");
        let app = self.clone();
        let focus = entry.clone();
        dialog.connect_response(Some("go"), move |_, _| {
            let topic = entry.text().trim().to_owned();
            if topic.is_empty() {
                app.set_status("A stage needs a topic");
                return;
            }
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let notify = notify.is_active();
            spawn_local(
                &app,
                async move {
                    if starting {
                        http.start_stage(channel_id, &topic, notify).await
                    } else {
                        http.edit_stage(channel_id, &topic).await
                    }
                },
                move |app, result| {
                    if let Err(error) = result {
                        app.set_status(&format!("Could not {} the stage: {error}", if starting { "start" } else { "change" }));
                    }
                },
            );
        });
        dialog.present(Some(&self.window));
        focus.grab_focus();
    }

    fn confirm_end_stage(self: &Rc<Self>, channel_id: Snowflake) {
        let dialog = adw::AlertDialog::new(Some("End the Stage?"), Some("Everyone stays in the channel, but the stage stops being live."));
        dialog.add_responses(&[("cancel", "Cancel"), ("end", "End Stage")]);
        dialog.set_response_appearance("end", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let app = self.clone();
        dialog.connect_response(Some("end"), move |_, _| {
            let Some((_, http, _)) = app.active() else {
                return;
            };
            spawn_local(&app, async move { http.end_stage(channel_id).await }, |app, result| {
                if let Err(error) = result {
                    app.set_status(&format!("Could not end the stage: {error}"));
                }
            });
        });
        dialog.present(Some(&self.window));
    }
}

fn section(text: &str) -> gtk::Label {
    let heading = label(&text.to_uppercase(), &["caption-heading", "dim-label"]);
    heading.set_xalign(0.0);
    heading
}
