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
use crate::tile_grid::{self, TileGrid};
use crate::widgets::{clear_box, empty_state, label};

const TILE_AVATAR: i32 = 80;
/// Size of a tile in the row under an enlarged one (16:9).
const MINI_TILE: (i32, i32) = (176, 99);

/// A camera or stream: account, person, and whether it is their stream.
pub(crate) type VideoKey = (AccountId, Snowflake, bool);

/// One person connected to a voice channel.
#[derive(Clone)]
pub(crate) struct Participant {
    id: Snowflake,
    name: String,
    avatar: String,
    muted: bool,
    deafened: bool,
    /// Muted or deafened by a moderator (shown red).
    server_muted: bool,
    server_deafened: bool,
    streaming: bool,
    is_self: bool,
}

/// A made-up call for `KYLECORD_VIDEO_DEMO`.
pub(crate) struct DemoCall {
    people: Vec<Participant>,
    watching: Option<Snowflake>,
}

/// A video shown in a window of its own.
pub(crate) struct Popout {
    window: gtk::Window,
    picture: gtk::Picture,
}

impl Popout {
    pub(crate) fn close(self) {
        self.window.close();
    }
}

/// What the tiles of one drawing of the call have in common.
struct CallContext {
    account: AccountId,
    /// We are in this call ourselves.
    joined: bool,
    /// Whose stream we are tuned into.
    watching: Option<Snowflake>,
    speaking: std::collections::HashSet<Snowflake>,
}

/// What a tile shows.
enum Face {
    /// A camera or stream picture.
    Video(gtk::gdk::Paintable),
    /// No camera: the person's avatar.
    Avatar,
    /// A stream we asked to watch whose first picture has not arrived.
    Connecting,
    /// Someone's stream we are not watching.
    NotWatching,
    /// Our own stream, with its preview turned off.
    OwnStream,
    /// The picture is in a window of its own.
    PoppedOut,
}

/// Where a tile sits, which decides its size and controls.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// One of the grid, sized by it.
    Grid,
    /// Small, in the row under the enlarged tile.
    Mini,
    /// Enlarged; `true` when there are other tiles to hide or show.
    Stage(bool),
}

/// Whether a channel is voice-only (no text history of its own).
pub(crate) fn is_voice_only(channel: &Channel) -> bool {
    channel.kind.is_voice() && !channel.kind.is_text()
}

/// What the chat column shows for a channel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CallLayout {
    /// No call: the chat alone.
    Hidden,
    /// A DM call above its chat.
    WithChat,
    /// A voice channel: the call alone.
    Only,
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

    /// The DM header call button: hang up if we are in this DM's call, else
    /// start/join it.
    pub(crate) fn toggle_dm_call(self: &Rc<Self>) {
        let channel = self
            .with_session(|session| {
                let channel_id = session.selected_channel?;
                session
                    .store
                    .read(|state| state.channels.get(&channel_id).cloned())
            })
            .flatten();
        let Some(channel) = channel else {
            return;
        };
        let in_call = self
            .with_session(|session| {
                session
                    .voice
                    .as_ref()
                    .is_some_and(|voice| voice.channel_id == channel.id)
            })
            .unwrap_or(false);
        if in_call {
            self.leave_voice();
        } else {
            self.join_call(&channel);
        }
    }

    /// Redraws the voice view when a voice channel — or a DM with a live
    /// call — is open.
    /// Redraws the call for the open channel, or takes it away when no one
    /// is in one any more.
    pub(crate) fn refresh_voice_view(self: &Rc<Self>) {
        let demo = self
            .voice_demo
            .borrow()
            .as_ref()
            .map(|demo| (demo.people.clone(), demo.watching));
        if let Some((people, watching)) = demo {
            let account = self.model.borrow().active.unwrap_or(0);
            self.build_call_view(account, &people, true, watching, None);
            return;
        }
        let Some(channel_id) = self
            .with_session(|session| session.selected_channel)
            .flatten()
        else {
            return;
        };
        match self.call_layout(channel_id) {
            CallLayout::Hidden => self.set_call_layout(CallLayout::Hidden),
            _ => self.render_voice_view(channel_id),
        }
    }

    /// Voice channels are the call alone; a DM shows its call above the chat
    /// while anyone, you or not, is in it.
    pub(crate) fn call_layout(&self, channel_id: Snowflake) -> CallLayout {
        self.with_session(|session| {
            let joined = session
                .voice
                .as_ref()
                .is_some_and(|voice| voice.channel_id == channel_id);
            session.store.read(|state| {
                if state.channels.get(&channel_id).is_some_and(is_voice_only) {
                    return CallLayout::Only;
                }
                let active = state
                    .voice_states
                    .values()
                    .any(|voice| voice.guild_id.is_none() && voice.channel_id == Some(channel_id));
                if joined || active {
                    CallLayout::WithChat
                } else {
                    CallLayout::Hidden
                }
            })
        })
        .unwrap_or(CallLayout::Hidden)
    }

    pub(crate) fn set_call_layout(&self, layout: CallLayout) {
        self.call_area.set_visible(layout != CallLayout::Hidden);
        self.chat_box.set_visible(layout != CallLayout::Only);
    }

    /// Starts (or joins) a one-to-one voice call with a user: opens the DM,
    /// connects to its call, and rings them.
    pub(crate) fn start_call_with(self: &Rc<Self>, user_id: Snowflake) {
        let Some((_, http, store)) = self.active() else {
            return;
        };
        if let Some(channel) = store.read(|state| state.dm_with(user_id).cloned()) {
            self.open_channel(channel.clone());
            self.join_call(&channel);
            return;
        }
        self.spawn(
            async move { http.create_dm(user_id).await },
            move |app, result| match result {
                Ok(channel) => {
                    store.apply(
                        "CHANNEL_CREATE",
                        &serde_json::to_value(&channel).unwrap_or_default(),
                    );
                    let channel = store
                        .read(|state| state.channels.get(&channel.id).cloned())
                        .unwrap_or(channel);
                    app.open_channel(channel.clone());
                    app.join_call(&channel);
                }
                Err(error) => app.set_status(&format!("Could not start call: {error}")),
            },
        );
    }

    /// Joins the call in a DM or group DM and rings its other members. In a
    /// guild voice channel, prefer [`join_voice_channel`].
    pub(crate) fn join_call(self: &Rc<Self>, channel: &Channel) {
        let Some((account, http, _)) = self.active() else {
            return;
        };
        let channel_id = channel.id;
        let already = self
            .with_session(|session| {
                session
                    .voice
                    .as_ref()
                    .is_some_and(|voice| voice.channel_id == channel_id)
            })
            .unwrap_or(false);
        if !already {
            let _ = self.cmd_tx.try_send(Command::JoinVoice {
                account,
                guild_id: None,
                channel_id,
            });
            // Starting a call rings the others; answering or joining one
            // already going does not.
            let others_in_call = self
                .with_session(|session| {
                    let me = session.user.as_ref().map(|user| user.id);
                    session.store.read(|state| {
                        state.voice_states.values().any(|voice| {
                            voice.guild_id.is_none()
                                && voice.channel_id == Some(channel_id)
                                && Some(voice.user_id) != me
                        })
                    })
                })
                .unwrap_or(false);
            if !others_in_call {
                self.spawn(async move { http.ring_call(channel_id).await }, |_, _| {});
            }
        }
        self.render_voice_view(channel_id);
    }

    /// Shows the call in a channel: a tile for each person and each stream.
    pub(crate) fn render_voice_view(self: &Rc<Self>, channel_id: Snowflake) {
        let layout = match self.call_layout(channel_id) {
            CallLayout::Only => CallLayout::Only,
            _ => CallLayout::WithChat,
        };
        self.set_call_layout(layout);
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
                            let (self_mute, self_deaf) = here
                                .filter(|_| Some(user.id) == me)
                                .and_then(|own| own.own_flags(channel_id))
                                .unwrap_or((voice.self_mute, voice.self_deaf));
                            Some(Participant {
                                id: user.id,
                                name: state.display_name(guild, user),
                                avatar,
                                muted: voice.mute || self_mute,
                                deafened: voice.deaf || self_deaf,
                                server_muted: voice.mute,
                                server_deafened: voice.deaf,
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
            clear_box(&self.voice_view);
            self.voice_tiles.borrow_mut().clear();
            return;
        };
        self.build_call_view(account, &people, joined, watching, Some(channel_id));
    }

    /// Lays the call out. With nothing enlarged, every tile sits in a grid
    /// that fills the room; with one enlarged, it takes the room and the
    /// rest line up small beneath it.
    fn build_call_view(
        self: &Rc<Self>,
        account: AccountId,
        people: &[Participant],
        joined: bool,
        watching: Option<Snowflake>,
        channel_id: Option<Snowflake>,
    ) {
        clear_box(&self.voice_view);
        self.voice_tiles.borrow_mut().clear();
        // The channel to offer joining, when we are not in it.
        let join_target = channel_id
            .filter(|_| !joined)
            .and_then(|channel_id| {
                self.with_session(|session| {
                    session
                        .store
                        .read(|state| state.channels.get(&channel_id).cloned())
                })
            })
            .flatten();
        if people.is_empty() {
            self.voice_view.set_valign(gtk::Align::Center);
            let page = empty_state(
                "audio-volume-high-symbolic",
                "No One Is Here",
                "Nobody is in this voice channel yet.",
            );
            if let Some(channel) = join_target {
                page.set_child(Some(&self.join_button(channel)));
            }
            self.voice_view.append(&page);
            return;
        }
        self.voice_view.set_valign(gtk::Align::Fill);

        // Streams first, then people, as Discord orders them. A stream is
        // always a tile of its own, next to its owner's.
        let tiles: Vec<(&Participant, bool)> = people
            .iter()
            .filter(|person| person.streaming)
            .map(|person| (person, true))
            .chain(people.iter().map(|person| (person, false)))
            .collect();

        let call = CallContext {
            account,
            joined,
            watching,
            speaking: self.speaking.borrow().clone(),
        };
        // The enlarged tile, if it still shows a picture (or is about to).
        let focused = self.focused_video.get().filter(|(user, is_stream)| {
            tiles
                .iter()
                .find(|(person, stream)| person.id == *user && stream == is_stream)
                .is_some_and(|(person, stream)| {
                    matches!(
                        self.tile_face(&call, person, *stream),
                        Face::Video(_) | Face::Connecting
                    )
                })
        });

        match focused {
            Some((user, is_stream)) => {
                let others: Vec<&(&Participant, bool)> = tiles
                    .iter()
                    .filter(|(person, stream)| !(person.id == user && *stream == is_stream))
                    .collect();
                if let Some((person, stream)) = tiles
                    .iter()
                    .find(|(person, stream)| person.id == user && *stream == is_stream)
                {
                    let stage =
                        self.call_tile(&call, person, *stream, Role::Stage(!others.is_empty()));
                    self.voice_view.append(&stage);
                }
                if !others.is_empty() && !self.members_hidden.get() {
                    let strip = gtk::Box::new(gtk::Orientation::Horizontal, tile_grid::SPACING);
                    strip.set_halign(gtk::Align::Center);
                    for (person, stream) in others {
                        strip.append(&self.call_tile(&call, person, *stream, Role::Mini));
                    }
                    let scroll = gtk::ScrolledWindow::new();
                    scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
                    scroll.set_child(Some(&strip));
                    self.voice_view.append(&scroll);
                }
            }
            None => {
                let grid = TileGrid::new();
                grid.set_hexpand(true);
                grid.set_vexpand(true);
                for (person, stream) in &tiles {
                    grid.append(&self.call_tile(&call, person, *stream, Role::Grid));
                }
                self.voice_view.append(&grid);
            }
        }

        if let Some(channel) = join_target {
            self.voice_view.append(&self.join_button(channel));
        }
    }

    /// The button that joins a call we are not in: a guild voice channel
    /// or a DM call.
    fn join_button(self: &Rc<Self>, channel: Channel) -> gtk::Button {
        let guild = channel.guild_id.is_some();
        let join = gtk::Button::with_label(if guild { "Join Voice" } else { "Join Call" });
        join.add_css_class("suggested-action");
        join.add_css_class("pill");
        join.set_halign(gtk::Align::Center);
        let app = self.clone();
        join.connect_clicked(move |button| {
            // Once is enough; the page is redrawn when the call starts.
            button.set_sensitive(false);
            if guild {
                app.join_voice_channel(&channel);
            } else {
                app.join_call(&channel);
            }
        });
        join
    }

    /// What a tile has to show for a person (or, with `is_stream`, for
    /// their stream).
    fn tile_face(&self, call: &CallContext, person: &Participant, is_stream: bool) -> Face {
        let key = (call.account, person.id, is_stream);
        if self.popouts.borrow().contains_key(&key) {
            return Face::PoppedOut;
        }
        let video = self
            .videos
            .borrow()
            .get(&key)
            .map(|video| video.paintable.clone());
        if !is_stream {
            return video.map_or(Face::Avatar, Face::Video);
        }
        // Your own stream needs no watching: it is previewed locally.
        let tuned_in = person.is_self || call.watching == Some(person.id);
        match video {
            Some(paintable) if tuned_in => Face::Video(paintable),
            _ if person.is_self => Face::OwnStream,
            _ if call.watching == Some(person.id) => Face::Connecting,
            _ => Face::NotWatching,
        }
    }

    /// One tile of the call: a person (their camera, or their avatar) or a
    /// stream, at the size its place in the layout gives it.
    fn call_tile(
        self: &Rc<Self>,
        call: &CallContext,
        person: &Participant,
        is_stream: bool,
        role: Role,
    ) -> gtk::Widget {
        let account = call.account;
        let user_id = person.id;
        let key = (account, user_id, is_stream);
        let face = self.tile_face(call, person, is_stream);
        let stage = matches!(role, Role::Stage(_));
        let title = if is_stream {
            format!("{}'s stream", person.name)
        } else {
            format!("{}'s camera", person.name)
        };

        let tile = gtk::Overlay::new();
        tile.add_css_class("kc-tile");
        tile.set_overflow(gtk::Overflow::Hidden);
        if is_stream || matches!(face, Face::Video(_)) {
            tile.add_css_class("kc-tile-dark");
        }
        if !is_stream && call.speaking.contains(&user_id) {
            tile.add_css_class("kc-speaking");
        }
        match role {
            Role::Grid => {}
            Role::Mini => tile.set_size_request(MINI_TILE.0, MINI_TILE.1),
            Role::Stage(_) => {
                tile.set_hexpand(true);
                tile.set_vexpand(true);
                tile.set_size_request(-1, 180);
            }
        }

        // -- what fills the tile -----------------------------------------
        let avatar_size = match role {
            Role::Mini => 44,
            _ => TILE_AVATAR,
        };
        let centred = |children: &[gtk::Widget]| {
            let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
            column.set_halign(gtk::Align::Center);
            column.set_valign(gtk::Align::Center);
            for child in children {
                child.set_halign(gtk::Align::Center);
                column.append(child);
            }
            column
        };
        let caption = |text: &str| {
            let caption = label(text, &["kc-tile-caption"]);
            caption.set_ellipsize(gtk::pango::EllipsizeMode::End);
            caption.upcast::<gtk::Widget>()
        };
        let content: gtk::Widget = match &face {
            Face::Video(paintable) => {
                let picture = gtk::Picture::for_paintable(paintable);
                // A camera fills its tile; a screen is shown whole.
                picture.set_content_fit(if is_stream || stage {
                    gtk::ContentFit::Contain
                } else {
                    gtk::ContentFit::Cover
                });
                picture.upcast()
            }
            Face::Avatar => {
                let avatar = self
                    .images
                    .avatar(avatar_size, &person.name, Some(&person.avatar));
                centred(&[avatar.upcast()]).upcast()
            }
            Face::Connecting => {
                let spinner = gtk::Spinner::new();
                spinner.set_spinning(true);
                spinner.set_size_request(28, 28);
                let mut children = vec![spinner.upcast::<gtk::Widget>()];
                if role != Role::Mini {
                    children.push(caption("Connecting to stream…"));
                }
                centred(&children).upcast()
            }
            Face::NotWatching => {
                if call.joined {
                    let watch = gtk::Button::with_label(if role == Role::Mini {
                        "Watch"
                    } else {
                        "Watch Stream"
                    });
                    watch.add_css_class("kc-watch");
                    watch.add_css_class("suggested-action");
                    let app = self.clone();
                    watch.connect_clicked(move |_| {
                        let _ = app
                            .cmd_tx
                            .try_send(Command::WatchStream { account, user_id });
                    });
                    centred(&[watch.upcast()]).upcast()
                } else {
                    centred(&[caption("Join to watch")]).upcast()
                }
            }
            Face::OwnStream => centred(&[caption("You're live")]).upcast(),
            Face::PoppedOut => {
                let back = gtk::Button::with_label("Pop In");
                back.add_css_class("kc-watch");
                let app = self.clone();
                back.connect_clicked(move |_| app.pop_in_video(key));
                let mut children = Vec::new();
                if role != Role::Mini {
                    children.push(caption("In its own window"));
                }
                children.push(back.upcast());
                centred(&children).upcast()
            }
        };
        tile.set_child(Some(&content));

        // -- name, bottom left -------------------------------------------
        let name = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        name.add_css_class("kc-tile-name");
        name.set_halign(gtk::Align::Start);
        name.set_valign(gtk::Align::End);
        name.set_margin_start(6);
        name.set_margin_bottom(6);
        name.set_margin_end(6);
        let small_icon = |icon: &str, tooltip: &str| {
            let image = gtk::Image::from_icon_name(icon);
            image.set_pixel_size(12);
            image.set_tooltip_text(Some(tooltip));
            image
        };
        if is_stream {
            name.append(&small_icon("video-display-symbolic", "Stream"));
        }
        let text = gtk::Label::new(Some(&person.name));
        text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.set_max_width_chars(if role == Role::Mini { 12 } else { 28 });
        name.append(&text);
        if !is_stream {
            if person.muted {
                let icon = small_icon(
                    "microphone-sensitivity-muted-symbolic",
                    if person.server_muted {
                        "Server Muted"
                    } else {
                        "Muted"
                    },
                );
                if person.server_muted {
                    icon.add_css_class("kc-server-muted");
                }
                name.append(&icon);
            }
            if person.deafened {
                let icon = small_icon(
                    "audio-volume-muted-symbolic",
                    if person.server_deafened {
                        "Server Deafened"
                    } else {
                        "Deafened"
                    },
                );
                if person.server_deafened {
                    icon.add_css_class("kc-server-muted");
                }
                name.append(&icon);
            }
        }
        tile.add_overlay(&name);

        if is_stream && !stage {
            let live = gtk::Label::new(Some("LIVE"));
            live.add_css_class("kc-badge");
            live.set_halign(gtk::Align::End);
            live.set_valign(gtk::Align::Start);
            live.set_margin_end(6);
            live.set_margin_top(6);
            tile.add_overlay(&live);
        }

        // -- controls: pop out, and on the enlarged tile the rest ---------
        let has_video = matches!(face, Face::Video(_));
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        controls.add_css_class("kc-tile-controls");
        controls.set_valign(gtk::Align::Start);
        controls.set_margin_top(6);
        if stage {
            controls.add_css_class("kc-shown");
            controls.set_halign(gtk::Align::End);
            controls.set_margin_end(6);
        } else {
            controls.set_halign(gtk::Align::Start);
            controls.set_margin_start(6);
        }
        let control = |icon: &str, tooltip: &str| {
            let button = gtk::Button::from_icon_name(icon);
            button.set_tooltip_text(Some(tooltip));
            button
        };
        if has_video && role != Role::Mini {
            let info = control("dialog-information-symbolic", "Stream information");
            let app = self.clone();
            info.connect_clicked(move |_| {
                app.video_info.borrow_mut().toggle(key);
                app.refresh_voice_view();
            });
            controls.append(&info);
        }
        if has_video {
            let pop = control("window-new-symbolic", "Pop out into its own window");
            let app = self.clone();
            let title = title.clone();
            pop.connect_clicked(move |_| app.pop_out_video(key, &title));
            controls.append(&pop);
        }
        if stage {
            let minimise = control("view-restore-symbolic", "Back to the grid");
            let app = self.clone();
            minimise.connect_clicked(move |_| app.set_focused_video(None));
            controls.append(&minimise);
            if is_stream && !person.is_self {
                let stop = gtk::Button::with_label("Stop Watching");
                let app = self.clone();
                stop.connect_clicked(move |_| {
                    let _ = app.cmd_tx.try_send(Command::StopWatching { account });
                });
                controls.append(&stop);
            }
        }
        if controls.first_child().is_some() {
            tile.add_overlay(&controls);
        }

        // -- information about the picture, when asked for ----------------
        if has_video && role != Role::Mini && self.video_info.borrow().shown(key) {
            let panel = self.video_info_label(key);
            panel.set_halign(gtk::Align::Start);
            panel.set_valign(gtk::Align::Start);
            panel.set_margin_start(6);
            panel.set_margin_top(6);
            // On a tile the controls share this corner while the pointer
            // is over it, and the panel moves down out of their way.
            if !stage {
                panel.add_css_class("kc-video-info-yields");
            }
            tile.add_overlay(&panel);
        }

        // -- hide or show the others, under the enlarged tile -------------
        if role == Role::Stage(true) {
            let hidden = self.members_hidden.get();
            let toggle = gtk::Button::new();
            toggle.add_css_class("kc-members-toggle");
            let icons = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            icons.append(&gtk::Image::from_icon_name(if hidden {
                "pan-up-symbolic"
            } else {
                "pan-down-symbolic"
            }));
            icons.append(&gtk::Image::from_icon_name("system-users-symbolic"));
            toggle.set_child(Some(&icons));
            toggle.set_tooltip_text(Some(if hidden {
                "Show Members"
            } else {
                "Hide Members"
            }));
            toggle.set_halign(gtk::Align::Center);
            toggle.set_valign(gtk::Align::End);
            toggle.set_margin_bottom(6);
            let app = self.clone();
            toggle.connect_clicked(move |_| {
                app.members_hidden.set(!app.members_hidden.get());
                app.refresh_voice_view();
            });
            tile.add_overlay(&toggle);
        }

        // Clicking a picture enlarges it (or, enlarged, puts it back); an
        // avatar opens the profile. Right-click always opens the user menu.
        let focusable = matches!(face, Face::Video(_) | Face::Connecting);
        let click = gtk::GestureClick::new();
        let app = self.clone();
        click.connect_released(move |gesture, _, _, _| {
            if focusable {
                app.set_focused_video((!stage).then_some((user_id, is_stream)));
            } else if !is_stream && let Some(widget) = gesture.widget() {
                app.show_profile(user_id, &widget);
            }
        });
        tile.add_controller(click);
        if focusable || !is_stream {
            tile.set_cursor_from_name(Some("pointer"));
        }
        self.attach_user_menu(&tile, user_id);

        if !is_stream {
            self.voice_tiles
                .borrow_mut()
                .insert(user_id, tile.clone().upcast());
        }
        tile.upcast()
    }

    /// Sets (or clears) the enlarged video and redraws the voice view.
    pub(crate) fn set_focused_video(self: &Rc<Self>, value: Option<(Snowflake, bool)>) {
        self.focused_video.set(value);
        self.refresh_voice_view();
    }

    // -- videos in their own windows -------------------------------------

    /// Moves a camera or stream into a window of its own. Closing that
    /// window puts it back in the call.
    pub(crate) fn pop_out_video(self: &Rc<Self>, key: VideoKey, title: &str) {
        if let Some(popout) = self.popouts.borrow().get(&key) {
            popout.window.present();
            return;
        }
        let Some(paintable) = self
            .videos
            .borrow()
            .get(&key)
            .map(|video| video.paintable.clone())
        else {
            return;
        };
        let picture = gtk::Picture::for_paintable(&paintable);
        picture.set_content_fit(gtk::ContentFit::Contain);
        // The information button shows while the pointer is over the
        // window; its panel stays until it is clicked again.
        let view = gtk::Overlay::new();
        view.add_css_class("kc-popout");
        view.set_child(Some(&picture));
        let panel = self.video_info_label(key);
        panel.set_halign(gtk::Align::Start);
        panel.set_valign(gtk::Align::Start);
        panel.set_margin_start(6);
        panel.set_margin_top(6);
        panel.add_css_class("kc-video-info-yields");
        panel.set_visible(self.video_info.borrow().shown(key));
        view.add_overlay(&panel);
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        controls.add_css_class("kc-tile-controls");
        controls.set_halign(gtk::Align::Start);
        controls.set_valign(gtk::Align::Start);
        controls.set_margin_start(6);
        controls.set_margin_top(6);
        let info = gtk::Button::from_icon_name("dialog-information-symbolic");
        info.set_tooltip_text(Some("Stream information"));
        let app = self.clone();
        let shown = panel.clone();
        info.connect_clicked(move |_| {
            let on = app.video_info.borrow_mut().toggle(key);
            shown.set_visible(on);
        });
        controls.append(&info);
        view.add_overlay(&controls);
        let window = gtk::Window::builder()
            .title(title)
            .default_width(960)
            .default_height(540)
            .child(&view)
            .build();
        window.set_application(self.window.application().as_ref());

        // Double-click (or F / F11) fills the screen; Escape leaves it.
        let double = gtk::GestureClick::new();
        let target = window.clone();
        double.connect_released(move |_, presses, _, _| {
            if presses == 2 {
                target.set_fullscreened(!target.is_fullscreen());
            }
        });
        picture.add_controller(double);
        let keys = gtk::EventControllerKey::new();
        let target = window.clone();
        keys.connect_key_pressed(move |_, key, _, _| match key {
            gtk::gdk::Key::f | gtk::gdk::Key::F | gtk::gdk::Key::F11 => {
                target.set_fullscreened(!target.is_fullscreen());
                gtk::glib::Propagation::Stop
            }
            gtk::gdk::Key::Escape if target.is_fullscreen() => {
                target.set_fullscreened(false);
                gtk::glib::Propagation::Stop
            }
            _ => gtk::glib::Propagation::Proceed,
        });
        window.add_controller(keys);

        let app = self.clone();
        window.connect_close_request(move |_| {
            // Closed by hand: back into the call. (Closed because the
            // video ended, it has been forgotten already.)
            if app.popouts.borrow_mut().remove(&key).is_some() {
                app.refresh_voice_view();
            }
            gtk::glib::Propagation::Proceed
        });
        self.popouts.borrow_mut().insert(
            key,
            Popout {
                window: window.clone(),
                picture,
            },
        );
        if self.focused_video.get() == Some((key.1, key.2)) {
            self.focused_video.set(None);
        }
        window.present();
        self.refresh_voice_view();
    }

    /// Closes a video's own window, which puts it back in the call.
    pub(crate) fn pop_in_video(self: &Rc<Self>, key: VideoKey) {
        let popout = self.popouts.borrow_mut().remove(&key);
        if let Some(popout) = popout {
            popout.window.close();
            self.refresh_voice_view();
        }
    }

    /// A video restarted: its window, if it has one, shows the new picture.
    pub(crate) fn update_popout(&self, key: VideoKey) {
        if let (Some(popout), Some(video)) = (
            self.popouts.borrow().get(&key),
            self.videos.borrow().get(&key),
        ) {
            popout.picture.set_paintable(Some(&video.paintable));
        }
    }

    /// Closes every video window (the call ended, or kylecord is closing).
    pub(crate) fn close_popouts(&self) {
        let popouts: Vec<Popout> = self.popouts.borrow_mut().drain().map(|(_, p)| p).collect();
        for popout in popouts {
            popout.close();
        }
    }

    /// Development aid (`KYLECORD_VIDEO_DEMO`): a made-up call with
    /// locally generated video, to check the layout without a real one.
    /// The value picks the state: `grid`, `focus`, `hidden` (focused, with
    /// the others hidden), `popout`, `info` and `info-grid` (information
    /// panels showing), or `sequence` (watch a stream, enlarge a camera,
    /// then put it back).
    pub(crate) fn show_video_demo(self: &Rc<Self>, mode: &str) {
        let account = self.model.borrow().active.unwrap_or(0);
        let person = |id: u64, name: &str, streaming: bool| Participant {
            id: Snowflake::new(id),
            name: name.to_owned(),
            avatar: String::new(),
            muted: id.is_multiple_of(3),
            deafened: id == 6,
            server_muted: id == 3,
            server_deafened: false,
            streaming,
            is_self: id == 2,
        };
        let people = vec![
            person(1, "kyleraykbs", true),
            person(2, "Lambda Ignition", true),
            person(3, "lain", false),
            person(4, "Gamma", false),
            person(5, "someone with a rather long name", false),
            person(6, "Delta", false),
        ];
        // Streams from the first two, cameras from the first three.
        let keys = [(1, true), (2, true), (1, false), (2, false), (3, false)];
        let mut feeds = Vec::new();
        for (id, stream) in keys {
            let feed = std::sync::Arc::new(kc_media::decode::VideoFeed::default());
            if let Some(video) = crate::live::LiveVideo::start(feed.clone()) {
                self.videos
                    .borrow_mut()
                    .insert((account, Snowflake::new(id), stream), video);
            }
            feeds.push(feed);
        }
        std::thread::spawn(move || {
            let Ok(frames) = kc_media::decode::test_pattern(90, 30) else {
                return;
            };
            for frame in frames.iter().cycle().take(1800) {
                for feed in &feeds {
                    feed.push(frame.clone(), kc_voice::rtp::is_keyframe(frame));
                }
                std::thread::sleep(Duration::from_millis(33));
            }
        });
        *self.voice_demo.borrow_mut() = Some(DemoCall {
            people,
            watching: Some(Snowflake::new(1)),
        });
        self.stack.set_visible_child_name("chat");
        self.set_call_layout(CallLayout::Only);
        // The voice panel too, with a made-up ping (`KYLECORD_DEMO_PING`,
        // milliseconds) to show the signal bars.
        self.voice_panel.set_visible(true);
        let ping = std::env::var("KYLECORD_DEMO_PING")
            .ok()
            .and_then(|millis| millis.parse().ok())
            .unwrap_or(40);
        self.note_ping(Some(ping));
        let stream = (Snowflake::new(1), true);
        let camera = (Snowflake::new(3), false);
        match mode {
            "focus" => self.focused_video.set(Some(stream)),
            // Information panels: on the enlarged stream, or on grid tiles.
            "info" => {
                self.focused_video.set(Some(stream));
                self.video_info
                    .borrow_mut()
                    .toggle((account, stream.0, true));
            }
            "info-grid" => {
                let mut info = self.video_info.borrow_mut();
                info.toggle((account, stream.0, true));
                info.toggle((account, camera.0, false));
            }
            "hidden" => {
                self.focused_video.set(Some(stream));
                self.members_hidden.set(true);
            }
            "popout" => {
                self.refresh_voice_view();
                self.pop_out_video((account, stream.0, true), "kyleraykbs's stream");
            }
            "sequence" => {
                self.focused_video.set(Some(stream));
                let app = self.clone();
                gtk::glib::timeout_add_local_once(Duration::from_millis(700), move || {
                    app.set_focused_video(Some(camera));
                });
                let app = self.clone();
                gtk::glib::timeout_add_local_once(Duration::from_millis(1400), move || {
                    app.set_focused_video(None);
                });
            }
            _ => {}
        }
        self.refresh_voice_view();
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

impl AppState {
    /// The ringing card for a call to us, centred over the window as in
    /// Discord: who is calling, Decline and Accept.
    pub(crate) fn show_incoming_call(self: &Rc<Self>, channel_id: Snowflake) {
        use libadwaita as adw;
        use libadwaita::prelude::*;

        if self
            .incoming_call
            .borrow()
            .as_ref()
            .is_some_and(|(ringing, _)| *ringing == channel_id)
        {
            return;
        }
        if let Some((_, old)) = self.incoming_call.borrow_mut().take() {
            old.close();
        }
        let Some(caller) = self
            .with_session(|session| {
                session.store.read(|state| {
                    let channel = state.channels.get(&channel_id)?;
                    // A DM shows the other person; a group DM shows itself.
                    let solo = channel
                        .recipients
                        .as_ref()
                        .filter(|recipients| recipients.len() == 1)
                        .and_then(|recipients| recipients.first());
                    Some(match solo {
                        Some(user) => (
                            state.display_name(None, user),
                            Some(cdn::user_avatar(user.id, user.avatar.as_deref(), 160)),
                        ),
                        None => (
                            channel.display_name(),
                            channel
                                .icon
                                .as_deref()
                                .map(|hash| cdn::channel_icon(channel_id, hash, 160)),
                        ),
                    })
                })
            })
            .flatten()
        else {
            return;
        };
        let (name, avatar_url) = caller;

        let dialog = adw::Dialog::new();
        dialog.set_title("Incoming Call");
        dialog.set_content_width(300);
        let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
        card.set_margin_top(28);
        card.set_margin_bottom(24);
        card.set_margin_start(24);
        card.set_margin_end(24);
        let avatar = self.images.avatar(96, &name, avatar_url.as_deref());
        avatar.add_css_class("kc-ringing");
        avatar.set_halign(gtk::Align::Center);
        avatar.set_margin_bottom(10);
        card.append(&avatar);
        let title = gtk::Label::new(Some(&name));
        title.add_css_class("title-3");
        title.set_wrap(true);
        title.set_justify(gtk::Justification::Center);
        card.append(&title);
        let subtitle = gtk::Label::new(Some("Incoming Call…"));
        subtitle.add_css_class("dim-label");
        card.append(&subtitle);

        let decline = gtk::Button::from_icon_name("call-stop-symbolic");
        decline.add_css_class("kc-call-decline");
        decline.set_tooltip_text(Some("Decline"));
        let accept = gtk::Button::from_icon_name("call-start-symbolic");
        accept.add_css_class("kc-call-accept");
        accept.set_tooltip_text(Some("Accept"));
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        buttons.set_halign(gtk::Align::Center);
        buttons.set_margin_top(18);
        buttons.append(&decline);
        buttons.append(&accept);
        card.append(&buttons);
        dialog.set_child(Some(&card));
        dialog.set_focus(Some(&accept));

        let app = self.clone();
        decline.connect_clicked(move |_| {
            app.dismiss_incoming_call(channel_id);
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let me = app
                .with_session(|session| session.user.as_ref().map(|user| user.id))
                .flatten();
            app.spawn(
                async move {
                    let me: Vec<Snowflake> = me.into_iter().collect();
                    http.stop_ringing(channel_id, Some(&me)).await
                },
                |app, result| {
                    if let Err(error) = result {
                        app.set_status(&format!("Could not decline: {error}"));
                    }
                },
            );
        });
        let app = self.clone();
        accept.connect_clicked(move |_| {
            app.dismiss_incoming_call(channel_id);
            let channel = app
                .with_session(|session| {
                    session
                        .store
                        .read(|state| state.channels.get(&channel_id).cloned())
                })
                .flatten();
            if let Some(channel) = channel {
                app.open_channel(channel.clone());
                app.join_call(&channel);
            }
        });
        // Closed any other way (Escape, a click outside), the card just goes.
        let app = Rc::downgrade(self);
        dialog.connect_closed(move |closed| {
            if let Some(app) = app.upgrade() {
                let mut current = app.incoming_call.borrow_mut();
                if current.as_ref().is_some_and(|(_, dialog)| dialog == closed) {
                    current.take();
                }
            }
        });
        dialog.present(Some(&self.window));
        *self.incoming_call.borrow_mut() = Some((channel_id, dialog));
    }

    /// Takes the ringing card for `channel_id` away, if it shows.
    pub(crate) fn dismiss_incoming_call(&self, channel_id: Snowflake) {
        use libadwaita::prelude::*;

        let dialog = {
            let mut current = self.incoming_call.borrow_mut();
            if current
                .as_ref()
                .is_some_and(|(ringing, _)| *ringing == channel_id)
            {
                current.take().map(|(_, dialog)| dialog)
            } else {
                None
            }
        };
        if let Some(dialog) = dialog {
            dialog.close();
        }
    }
}
