//! The soundboard: Discord's sounds and servers' own, played to the voice
//! channel you are in (and heard when others play them).
//!
//! As the web client does it: Discord's sounds from
//! `/soundboard-default-sounds`, a server's from gateway op 31 (answered by
//! SOUNDBOARD_SOUNDS), playing with `POST
//! /channels/{id}/send-soundboard-sound`; others' plays arrive as
//! VOICE_CHANNEL_EFFECT_SEND and the sound comes from the CDN. Your own
//! play is heard here at once, as on the web.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use kc_core::Snowflake;
use kc_http::cdn;
use kc_state::{SoundboardSound, permissions};

use crate::shell::AppState;
use crate::video_setup::spawn_local;
use crate::widgets::label;

thread_local! {
    /// Discord's sounds, fetched the first time the soundboard opens.
    static DEFAULTS: RefCell<Option<Rc<Vec<SoundboardSound>>>> = const { RefCell::new(None) };
    /// Sounds playing here (kept so they play to the end).
    static PLAYING: RefCell<Vec<kc_media::sound::Sound>> = const { RefCell::new(Vec::new()) };
}

fn sound_url(sound_id: Snowflake) -> String {
    format!("https://cdn.discordapp.com/soundboard-sounds/{sound_id}")
}

/// Demos: Discord's sounds without asking Discord.
pub(crate) fn demo_defaults(sounds: Vec<SoundboardSound>) {
    DEFAULTS.with_borrow_mut(|defaults| *defaults = Some(Rc::new(sounds)));
}

impl AppState {
    /// Plays a soundboard sound here, at `volume` (0.0-1.0).
    pub(crate) fn play_soundboard_here(self: &Rc<Self>, sound_id: Snowflake, volume: f64) {
        self.images.file(&sound_url(sound_id), "mp3", move |path| {
            let Some(path) = path else {
                return;
            };
            match kc_media::sound::play(&path, volume.clamp(0.0, 1.0), false) {
                Ok(sound) => PLAYING.with_borrow_mut(|playing| {
                    playing.retain(|sound| !sound.finished());
                    playing.push(sound);
                }),
                Err(error) => tracing::warn!(%error, "soundboard sound did not play"),
            }
        });
    }

    /// Someone played a sound in a voice channel: heard if we are in it,
    /// not deafened, and it wasn't us (we heard ours already).
    pub(crate) fn soundboard_played(self: &Rc<Self>, channel_id: Snowflake, user_id: Snowflake, sound_id: Snowflake, volume: f64) {
        let hear = self
            .with_session(|session| {
                let voice = session.voice.as_ref()?;
                let me = session.user.as_ref()?.id;
                Some(voice.channel_id == channel_id && !voice.deafened && user_id != me)
            })
            .flatten()
            .unwrap_or(false);
        if hear {
            self.play_soundboard_here(sound_id, volume);
        }
    }

    /// The soundboard, pointing at `anchor` (for the voice channel we are in).
    pub(crate) fn show_soundboard(self: &Rc<Self>, anchor: &gtk::Widget) -> Option<gtk::Popover> {
        let (account, _, _) = self.active()?;
        let (channel_id, guild_id) = self.with_session(|session| {
            let voice = session.voice.as_ref()?;
            Some((voice.channel_id, voice.guild_id))
        })??;
        let (can_play, external, guilds) = self.with_session(|session| {
            session.store.read(|state| {
                let nitro = state.me.as_ref().is_some_and(kc_state::User::has_nitro);
                let granted = state.permissions_in(channel_id);
                let private = guild_id.is_none();
                (
                    private || granted & permissions::USE_SOUNDBOARD != 0,
                    nitro && (private || granted & permissions::USE_EXTERNAL_SOUNDS != 0),
                    state.ordered_guilds().into_iter().map(|guild| guild.id).collect::<Vec<_>>(),
                )
            })
        })?;
        // The servers whose sounds can be played here: this one, and with
        // Nitro the rest.
        let sources: Vec<Snowflake> = guild_id.into_iter().chain(guilds.into_iter().filter(|_| external)).collect();
        let missing: Vec<Snowflake> = self
            .with_session(|session| {
                session.store.read(|state| {
                    sources
                        .iter()
                        .copied()
                        .filter(|id| state.guilds.get(id).is_some_and(|guild| guild.soundboard_sounds.is_empty()))
                        .collect()
                })
            })
            .unwrap_or_default();
        if let Some(gateway) = crate::session::gateway(account) {
            gateway.request_soundboard_sounds(&missing);
        }

        let popover = gtk::Popover::new();
        popover.set_parent(anchor);
        popover.set_position(gtk::PositionType::Top);
        popover.connect_closed(|popover| popover.unparent());
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_size_request(380, 420);
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Find a sound"));
        content.append(&search);
        let hint = label(
            if can_play { "Click to play to the channel · right-click to hear it yourself" } else { "You can't use the soundboard here" },
            &["caption", "dim-label"],
        );
        hint.set_xalign(0.0);
        content.append(&hint);
        let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&list));
        content.append(&scroll);
        popover.set_child(Some(&content));

        let fill: Rc<dyn Fn()> = {
            let (app, list, search, sources) = (self.clone(), list.clone(), search.clone(), sources.clone());
            Rc::new(move || {
                while let Some(child) = list.first_child() {
                    list.remove(&child);
                }
                let query = search.text().to_lowercase();
                let mut sections: Vec<(String, Vec<SoundboardSound>)> = Vec::new();
                if let Some(defaults) = DEFAULTS.with_borrow(Clone::clone) {
                    sections.push(("Discord".to_owned(), defaults.as_ref().clone()));
                }
                app.with_session(|session| {
                    session.store.read(|state| {
                        for id in &sources {
                            if let Some(guild) = state.guilds.get(id) {
                                sections.push((guild.name.clone(), guild.soundboard_sounds.clone()));
                            }
                        }
                    })
                });
                let mut any = false;
                for (name, sounds) in sections {
                    let shown: Vec<&SoundboardSound> = sounds
                        .iter()
                        .filter(|sound| sound.available && (query.is_empty() || sound.name.to_lowercase().contains(&query)))
                        .collect();
                    if shown.is_empty() {
                        continue;
                    }
                    any = true;
                    let heading = label(&name.to_uppercase(), &["caption-heading", "dim-label"]);
                    heading.set_xalign(0.0);
                    list.append(&heading);
                    let grid = gtk::FlowBox::new();
                    grid.set_selection_mode(gtk::SelectionMode::None);
                    grid.set_homogeneous(true);
                    grid.set_min_children_per_line(2);
                    grid.set_max_children_per_line(2);
                    for sound in shown {
                        grid.append(&app.sound_button(sound, channel_id, can_play));
                    }
                    list.append(&grid);
                }
                if !any {
                    let empty = label("Loading sounds…", &["dim-label"]);
                    empty.set_margin_top(24);
                    list.append(&empty);
                }
            })
        };
        fill();
        {
            let fill = fill.clone();
            search.connect_search_changed(move |_| fill());
        }
        // Servers' sounds come in on the gateway; redraw as they do.
        if let Some((_, _, store)) = self.active() {
            let mut events = store.subscribe();
            let (fill, weak) = (fill.clone(), popover.downgrade());
            glib::MainContext::default().spawn_local(async move {
                while let Ok(event) = events.recv().await {
                    if weak.upgrade().is_none_or(|popover| !popover.is_visible()) {
                        break;
                    }
                    if matches!(event, kc_state::StoreEvent::GuildUpdated(_)) {
                        fill();
                    }
                }
            });
        }
        if DEFAULTS.with_borrow(Option::is_none)
            && let Some((_, http, _)) = self.active()
        {
            let fill = fill.clone();
            spawn_local(self, async move { http.default_soundboard_sounds().await }, move |app, result| match result {
                Ok(sounds) => {
                    DEFAULTS.with_borrow_mut(|defaults| *defaults = Some(Rc::new(sounds)));
                    fill();
                }
                Err(error) => app.set_status(&format!("Could not load Discord's sounds: {error}")),
            });
        }
        if std::env::var_os("KUBECORD_DEMO").is_some() {
            popover.set_autohide(false);
        }
        popover.popup();
        search.grab_focus();
        Some(popover)
    }

    fn sound_button(self: &Rc<Self>, sound: &SoundboardSound, channel_id: Snowflake, can_play: bool) -> gtk::Widget {
        let button = gtk::Button::new();
        button.add_css_class("kc-sound");
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        match (sound.emoji_id, sound.emoji_name.as_deref()) {
            (Some(id), _) => {
                let image = gtk::Picture::new();
                image.set_size_request(18, 18);
                image.set_can_shrink(true);
                self.images.set_picture(&image, &cdn::emoji(id, false, 32));
                content.append(&image);
            }
            (None, Some(emoji)) => content.append(&label(emoji, &[])),
            _ => content.append(&gtk::Image::from_icon_name("audio-volume-high-symbolic")),
        }
        let name = label(&sound.name, &[]);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_hexpand(true);
        name.set_xalign(0.0);
        content.append(&name);
        button.set_child(Some(&content));
        button.set_tooltip_text(Some(&sound.name));
        button.set_sensitive(can_play);
        let (app, chosen) = (self.clone(), sound.clone());
        button.connect_clicked(move |_| app.play_to_channel(channel_id, &chosen));
        // A right-click previews it here only.
        let preview = gtk::GestureClick::new();
        preview.set_button(3);
        let (app, id, volume) = (self.clone(), sound.sound_id, sound.volume);
        preview.connect_pressed(move |_, _, _, _| app.play_soundboard_here(id, volume));
        button.add_controller(preview);
        button.upcast()
    }

    fn play_to_channel(self: &Rc<Self>, channel_id: Snowflake, sound: &SoundboardSound) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        self.play_soundboard_here(sound.sound_id, sound.volume);
        let sound = sound.clone();
        spawn_local(self, async move { http.send_soundboard_sound(channel_id, &sound).await }, |app, result| {
            if let Err(error) = result {
                app.set_status(&format!("Could not play the sound: {error}"));
            }
        });
    }
}
