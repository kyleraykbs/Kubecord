//! The voice channel view (who is connected, who is speaking) and joining
//! a voice channel from the sidebar.

use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::prelude::*;

use kc_core::Snowflake;
use kc_http::cdn;
use kc_state::Channel;

use crate::session::{AccountId, Command};
use crate::shell::AppState;
use crate::widgets::{clear_box, empty_state, label};

const TILE_AVATAR: i32 = 80;
/// Size of a camera picture in a tile (16:9).
const TILE_VIDEO: (i32, i32) = (288, 162);

/// One person connected to a voice channel.
struct Participant {
    id: Snowflake,
    name: String,
    avatar: String,
    muted: bool,
    deafened: bool,
    video: bool,
    streaming: bool,
    is_self: bool,
}

/// Whether a channel is voice-only (no text history of its own).
pub(crate) fn is_voice_only(channel: &Channel) -> bool {
    channel.kind.is_voice() && !channel.kind.is_text()
}

impl AppState {
    /// A click on a voice channel in the sidebar: the first click shows the
    /// channel, a second click within the desktop's double-click time joins
    /// it. Timed here rather than by a gesture because the sidebar is
    /// rebuilt after the first click, replacing the button that was clicked.
    pub(crate) fn voice_channel_clicked(self: &Rc<Self>, channel: Channel) {
        let window = gtk::Settings::default()
            .map(|settings| settings.gtk_double_click_time())
            .unwrap_or(400)
            .max(1) as u64;
        let now = Instant::now();
        let double = self.last_channel_click.get().is_some_and(|(id, at)| {
            id == channel.id && now.duration_since(at) <= Duration::from_millis(window)
        });
        if double {
            self.last_channel_click.set(None);
            self.join_voice_channel(&channel);
        } else {
            self.last_channel_click.set(Some((channel.id, now)));
            self.open_channel(channel);
        }
    }

    /// Connects to a voice channel and shows it. Joining the channel the
    /// account is already in does nothing.
    pub(crate) fn join_voice_channel(self: &Rc<Self>, channel: &Channel) {
        let Some((account, _, _)) = self.active() else {
            return;
        };
        let already = self
            .with_session(|session| {
                session
                    .voice
                    .as_ref()
                    .is_some_and(|voice| voice.channel_id == channel.id)
            })
            .unwrap_or(false);
        if self.with_session(|session| session.selected_channel) != Some(Some(channel.id)) {
            self.open_channel(channel.clone());
        }
        if already {
            return;
        }
        let _ = self.cmd_tx.try_send(Command::JoinVoice {
            account,
            guild_id: channel.guild_id,
            channel_id: channel.id,
        });
    }

    pub(crate) fn leave_voice(&self) {
        let Some(account) = self.model.borrow().active else {
            return;
        };
        let _ = self.cmd_tx.try_send(Command::LeaveVoice { account });
    }

    /// Redraws the voice view when a voice channel is open.
    pub(crate) fn refresh_voice_view(self: &Rc<Self>) {
        let open = self
            .with_session(|session| {
                let channel_id = session.selected_channel?;
                session.store.read(|state| {
                    state
                        .channels
                        .get(&channel_id)
                        .filter(|channel| is_voice_only(channel))
                        .map(|channel| channel.id)
                })
            })
            .flatten();
        if let Some(channel_id) = open {
            self.render_voice_view(channel_id);
        }
    }

    /// Shows the people connected to a voice channel as tiles.
    pub(crate) fn render_voice_view(self: &Rc<Self>, channel_id: Snowflake) {
        clear_box(&self.voice_view);
        self.voice_tiles.borrow_mut().clear();
        self.message_stack.set_visible_child_name("voice");
        let Some(account) = self.model.borrow().active else {
            return;
        };

        let Some((people, joined, watching)) = self
            .with_session(|session| {
                let here = session
                    .voice
                    .as_ref()
                    .filter(|voice| voice.channel_id == channel_id);
                let joined = here.is_some();
                let watching = here.and_then(|voice| voice.watching);
                let me = session.user.as_ref().map(|user| user.id);
                session.store.read(|state| {
                    let guild = state.channels.get(&channel_id)?.guild_id;
                    let mut people: Vec<Participant> = state
                        .voice_states
                        .values()
                        .filter(|voice| voice.channel_id == Some(channel_id))
                        .filter_map(|voice| {
                            let user = state.users.get(&voice.user_id)?;
                            let avatar = guild
                                .and_then(|guild| {
                                    let hash = state.member(guild, user.id)?.avatar.clone()?;
                                    Some(cdn::member_avatar(guild, user.id, &hash, 160))
                                })
                                .unwrap_or_else(|| {
                                    cdn::user_avatar(user.id, user.avatar.as_deref(), 160)
                                });
                            Some(Participant {
                                id: user.id,
                                name: state.display_name(guild, user),
                                avatar,
                                muted: voice.mute || voice.self_mute,
                                deafened: voice.deaf || voice.self_deaf,
                                video: voice.self_video,
                                streaming: voice.self_stream,
                                is_self: Some(user.id) == me,
                            })
                        })
                        .collect();
                    people.sort_by_key(|person| person.name.to_lowercase());
                    Some((people, joined, watching))
                })
            })
            .flatten()
        else {
            return;
        };

        // A watched stream fills the view; otherwise the tiles sit centred.
        self.voice_view.set_valign(if watching.is_some() {
            gtk::Align::Fill
        } else {
            gtk::Align::Center
        });
        if people.is_empty() {
            self.voice_view.append(&empty_state(
                "audio-volume-high-symbolic",
                "No One Is Here",
                "Double-click the channel in the sidebar to join.",
            ));
            return;
        }

        if let Some(owner) = watching {
            let name = people
                .iter()
                .find(|person| person.id == owner)
                .map(|person| person.name.as_str())
                .unwrap_or("Someone");
            self.voice_view
                .append(&self.stream_stage(account, owner, name));
        }

        let tiles = gtk::FlowBox::new();
        tiles.set_selection_mode(gtk::SelectionMode::None);
        tiles.set_homogeneous(true);
        // A flow box asks for room for its maximum per line; size that to the
        // people present so a lone tile is centred, not placed at the start.
        tiles.set_max_children_per_line(people.len().clamp(1, 4) as u32);
        tiles.set_column_spacing(12);
        tiles.set_row_spacing(12);
        tiles.set_halign(gtk::Align::Center);
        let speaking = self.speaking.borrow().clone();
        for person in &people {
            let tile = self.voice_tile(
                person,
                speaking.contains(&person.id),
                account,
                joined,
                watching,
            );
            tiles.insert(&tile, -1);
            self.voice_tiles
                .borrow_mut()
                .insert(person.id, tile.upcast());
        }
        self.voice_view.append(&tiles);

        if !joined {
            let hint = gtk::Label::new(Some("Double-click the channel in the sidebar to join."));
            hint.add_css_class("dim-label");
            hint.set_wrap(true);
            hint.set_justify(gtk::Justification::Center);
            hint.set_max_width_chars(60);
            hint.set_halign(gtk::Align::Center);
            self.voice_view.append(&hint);
        }
    }

    /// Development aid (`KYLECORD_VIDEO_DEMO`): shows a locally generated
    /// test video in the stream view, to check decoding and display
    /// without a call.
    pub(crate) fn show_video_demo(self: &Rc<Self>) {
        let Some(account) = self.model.borrow().active else {
            return;
        };
        let owner = Snowflake::new(1);
        let feed = std::sync::Arc::new(kc_media::decode::VideoFeed::default());
        let source = feed.clone();
        std::thread::spawn(move || {
            let Ok(frames) = kc_media::decode::test_pattern(90, 30) else {
                return;
            };
            for frame in frames.iter().cycle().take(900) {
                source.push(frame.clone(), kc_voice::rtp::is_keyframe(frame));
                std::thread::sleep(Duration::from_millis(33));
            }
        });
        if let Some(video) = crate::live::LiveVideo::start(feed) {
            self.videos
                .borrow_mut()
                .insert((account, owner, true), video);
        }
        clear_box(&self.voice_view);
        self.message_stack.set_visible_child_name("voice");
        self.voice_view.set_valign(gtk::Align::Fill);
        self.voice_view
            .append(&self.stream_stage(account, owner, "Test pattern"));
    }

    /// The stream being watched, large, with a way to stop watching.
    fn stream_stage(self: &Rc<Self>, account: AccountId, owner: Snowflake, name: &str) -> gtk::Box {
        let stage = gtk::Box::new(gtk::Orientation::Vertical, 8);
        stage.add_css_class("kc-stage");
        stage.set_vexpand(true);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let live = gtk::Label::new(Some("LIVE"));
        live.add_css_class("kc-badge");
        live.set_valign(gtk::Align::Center);
        header.append(&live);
        let title = label(&format!("{name}'s stream"), &["heading"]);
        title.set_hexpand(true);
        title.set_xalign(0.0);
        header.append(&title);
        let stop = gtk::Button::with_label("Stop Watching");
        let app = self.clone();
        stop.connect_clicked(move |_| {
            let _ = app.cmd_tx.try_send(Command::StopWatching { account });
        });
        header.append(&stop);
        stage.append(&header);

        let video = self
            .videos
            .borrow()
            .get(&(account, owner, true))
            .map(|video| video.paintable.clone());
        match video {
            Some(paintable) => {
                let picture = gtk::Picture::for_paintable(&paintable);
                picture.add_css_class("kc-video");
                picture.set_content_fit(gtk::ContentFit::Contain);
                picture.set_hexpand(true);
                picture.set_vexpand(true);
                picture.set_size_request(-1, 240);
                stage.append(&picture);
            }
            None => {
                // Asked to watch; the first picture has not arrived yet.
                let waiting = gtk::Box::new(gtk::Orientation::Vertical, 12);
                waiting.add_css_class("kc-video");
                waiting.set_vexpand(true);
                waiting.set_valign(gtk::Align::Fill);
                let spinner = gtk::Spinner::new();
                spinner.set_spinning(true);
                spinner.set_size_request(32, 32);
                spinner.set_vexpand(true);
                spinner.set_valign(gtk::Align::End);
                waiting.append(&spinner);
                let text = label("Connecting to stream…", &["dim-label"]);
                text.set_vexpand(true);
                text.set_valign(gtk::Align::Start);
                waiting.append(&text);
                waiting.set_size_request(-1, 240);
                stage.append(&waiting);
            }
        }
        stage
    }

    fn voice_tile(
        self: &Rc<Self>,
        person: &Participant,
        speaking: bool,
        account: AccountId,
        joined: bool,
        watching: Option<Snowflake>,
    ) -> gtk::Box {
        let tile = gtk::Box::new(gtk::Orientation::Vertical, 8);
        tile.add_css_class("kc-voice-tile");
        if speaking {
            tile.add_css_class("kc-speaking");
        }

        // Their camera when it is being received, their avatar otherwise.
        let camera = self
            .videos
            .borrow()
            .get(&(account, person.id, false))
            .map(|video| video.paintable.clone());
        let face: gtk::Widget = match camera {
            Some(paintable) => {
                let picture = gtk::Picture::for_paintable(&paintable);
                picture.add_css_class("kc-video");
                picture.set_content_fit(gtk::ContentFit::Cover);
                picture.set_size_request(TILE_VIDEO.0, TILE_VIDEO.1);
                picture.upcast()
            }
            None => {
                let avatar = self
                    .images
                    .avatar(TILE_AVATAR, &person.name, Some(&person.avatar));
                avatar.set_vexpand(true);
                avatar.set_valign(gtk::Align::Center);
                avatar.upcast()
            }
        };
        face.set_halign(gtk::Align::Center);
        let click = gtk::GestureClick::new();
        let app = self.clone();
        let user_id = person.id;
        click.connect_released(move |gesture, _, _, _| {
            if let Some(widget) = gesture.widget() {
                app.show_profile(user_id, &widget);
            }
        });
        face.add_controller(click);
        face.set_cursor_from_name(Some("pointer"));
        tile.append(&face);

        let name = label(&person.name, &["heading"]);
        name.set_halign(gtk::Align::Center);
        name.set_max_width_chars(16);
        tile.append(&name);

        let badges = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        badges.set_halign(gtk::Align::Center);
        // Keeps tiles the same height whether or not they show badges.
        badges.set_height_request(18);
        if person.streaming {
            let live = gtk::Label::new(Some("LIVE"));
            live.add_css_class("kc-badge");
            live.set_valign(gtk::Align::Center);
            badges.append(&live);
            // Anyone in the call can tune into someone else's stream.
            if joined && !person.is_self && watching != Some(person.id) {
                let watch = gtk::Button::with_label("Watch Stream");
                watch.add_css_class("kc-watch");
                watch.add_css_class("suggested-action");
                let app = self.clone();
                let user_id = person.id;
                watch.connect_clicked(move |_| {
                    let _ = app
                        .cmd_tx
                        .try_send(Command::WatchStream { account, user_id });
                });
                badges.append(&watch);
            }
        }
        for (shown, icon, tooltip) in [
            (person.video, "camera-video-symbolic", "Camera on"),
            (
                person.muted,
                "microphone-sensitivity-muted-symbolic",
                "Muted",
            ),
            (person.deafened, "audio-volume-muted-symbolic", "Deafened"),
        ] {
            if shown {
                let image = gtk::Image::from_icon_name(icon);
                image.set_pixel_size(14);
                image.add_css_class("dim-label");
                image.set_tooltip_text(Some(tooltip));
                badges.append(&image);
            }
        }
        tile.append(&badges);
        tile
    }

    /// Lights or clears the speaking ring on a person's tile and sidebar
    /// row, without redrawing either.
    pub(crate) fn show_speaking(&self, user_id: Snowflake, speaking: bool) {
        let tiles = self.voice_tiles.borrow();
        let rows = self.voice_rows.borrow();
        for widget in [tiles.get(&user_id), rows.get(&user_id)]
            .into_iter()
            .flatten()
        {
            if speaking {
                widget.add_css_class("kc-speaking");
            } else {
                widget.remove_css_class("kc-speaking");
            }
        }
    }
}
