//! Stickers: drawing them (Discord's own are Lottie animations, drawn with
//! rlottie), the sticker picker, sending one, and "Wave to say hi" on a
//! join message.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;

use kc_core::Snowflake;
use kc_http::cdn;
use kc_media::lottie::Animation;
use kc_state::{Message, Sticker, StickerItem, StickerPack};

use crate::shell::AppState;
use crate::video_setup::spawn_local;
use crate::widgets::label;

/// Discord's waving Wumpus, the sticker its "Wave to say hi" sends.
pub(crate) const WAVE_STICKER: Snowflake = Snowflake(749_054_660_769_218_631);
const LOTTIE: u8 = 3;
/// Shown in a message, and in the picker.
const MESSAGE_SIZE: i32 = 160;
const PICKER_SIZE: i32 = 80;

thread_local! {
    /// Discord's sticker packs, fetched the first time the picker opens.
    static PACKS: RefCell<Option<Rc<Vec<StickerPack>>>> = const { RefCell::new(None) };
}

/// Demos: Discord's packs from a saved `/sticker-packs` answer.
pub(crate) fn demo_packs(json: &str) {
    if let Ok(body) = serde_json::from_str::<serde_json::Value>(json)
        && let Ok(packs) = serde_json::from_value::<Vec<StickerPack>>(body["sticker_packs"].clone())
    {
        PACKS.with_borrow_mut(|cached| *cached = Some(Rc::new(packs)));
    }
}

/// A Lottie sticker's animation file.
fn lottie_url(id: Snowflake) -> String {
    format!("https://cdn.discordapp.com/stickers/{id}.json")
}

/// An rlottie frame (premultiplied ARGB words) as a texture.
fn frame_texture(pixels: Vec<u32>, size: usize) -> gtk::gdk::MemoryTexture {
    let bytes: Vec<u8> = pixels.into_iter().flat_map(u32::to_ne_bytes).collect();
    let format = if cfg!(target_endian = "little") {
        gtk::gdk::MemoryFormat::B8g8r8a8Premultiplied
    } else {
        gtk::gdk::MemoryFormat::A8r8g8b8Premultiplied
    };
    gtk::gdk::MemoryTexture::new(size as i32, size as i32, format, &glib::Bytes::from_owned(bytes), size * 4)
}

impl AppState {
    /// A sticker in a message.
    pub(crate) fn sticker_widget(self: &Rc<Self>, sticker: &StickerItem) -> gtk::Widget {
        let widget = self.sticker_picture(sticker.id, sticker.format_type, MESSAGE_SIZE, true);
        widget.set_tooltip_text(Some(&sticker.name));
        widget.set_halign(gtk::Align::Start);
        widget
    }

    /// A sticker's picture at `size` pixels. A Lottie one plays on its own
    /// when `always` is set, else while the pointer is over it.
    pub(crate) fn sticker_picture(self: &Rc<Self>, id: Snowflake, format_type: u8, size: i32, always: bool) -> gtk::Widget {
        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        picture.set_size_request(size, size);
        picture.set_content_fit(gtk::ContentFit::Contain);
        if format_type == LOTTIE {
            self.play_lottie(&picture, &lottie_url(id), size, always);
        } else if let Some(url) = cdn::sticker(id, format_type) {
            self.images.set_picture(&picture, &url);
        }
        picture.upcast()
    }

    /// Draws a Lottie animation into `picture`, a frame at a time.
    fn play_lottie(self: &Rc<Self>, picture: &gtk::Picture, url: &str, size: i32, always: bool) {
        let key = url.to_owned();
        let target = picture.downgrade();
        self.images.file(url, "json", move |path| {
            let Some(picture) = target.upgrade() else {
                return;
            };
            let animation = path
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|json| Animation::from_json(&json, &key));
            let Some(animation) = animation else {
                return;
            };
            let frames = animation.frames().max(1);
            let rate = animation.framerate().max(1.0);
            let animation = RefCell::new(animation);
            // Drawn at the screen's scale, so it stays sharp.
            let pixels = (size * picture.scale_factor().max(1)) as usize;
            let draw = move |picture: &gtk::Picture, frame: usize| {
                let pixels_now = animation.borrow_mut().render(frame, pixels, pixels);
                picture.set_paintable(Some(&frame_texture(pixels_now, pixels)));
            };
            // At rest it shows its middle (many start on an empty frame).
            let resting = frames / 2;
            draw(&picture, resting);

            let hovered = Rc::new(Cell::new(false));
            if !always {
                let motion = gtk::EventControllerMotion::new();
                let (enter, leave) = (hovered.clone(), hovered.clone());
                motion.connect_enter(move |_, _, _| enter.set(true));
                motion.connect_leave(move |_| leave.set(false));
                picture.add_controller(motion);
            }
            let start: Cell<Option<i64>> = Cell::new(None);
            let shown = Cell::new(resting);
            picture.add_tick_callback(move |picture, clock| {
                let playing = (always || hovered.get()) && picture.is_mapped();
                if !playing {
                    // Back to rest, and from the start next time.
                    start.set(None);
                    if shown.replace(resting) != resting {
                        draw(picture, resting);
                    }
                    return glib::ControlFlow::Continue;
                }
                let now = clock.frame_time();
                let began = *start.get().get_or_insert(now);
                start.set(Some(began));
                let frame = ((now - began) as f64 / 1e6 * rate) as usize % frames;
                if shown.replace(frame) != frame {
                    draw(picture, frame);
                }
                glib::ControlFlow::Continue
            });
        });
    }

    /// Sends a sticker to the open channel (replying, if a reply is set).
    fn send_sticker(self: &Rc<Self>, sticker: Snowflake) {
        let Some((channel_id, reply_to, mention)) = self
            .with_session(|session| session.selected_channel.map(|channel| (channel, session.reply_to, self.context_ping.is_active())))
            .flatten()
        else {
            return;
        };
        let Some((_, http, _)) = self.active() else {
            return;
        };
        if reply_to.is_some() {
            self.cancel_context();
        }
        let message = kc_http::Outgoing {
            sticker_ids: vec![sticker],
            reply_to,
            mention_reply: mention,
            ..kc_http::Outgoing::default()
        };
        self.spawn(async move { http.send(channel_id, &message).await }, |app, result| {
            if let Err(error) = result {
                app.set_status(&format!("Could not send the sticker: {error}"));
            }
        });
    }

    /// "Wave to say hi": greets someone who just joined with the waving
    /// sticker, as a reply to their join message.
    fn wave_to(self: &Rc<Self>, message: &Message, button: &gtk::Button) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let (channel_id, message_id, guild_id) = (message.channel_id, message.id, message.guild_id);
        button.set_sensitive(false);
        let button = button.clone();
        spawn_local(
            self,
            async move { http.greet(channel_id, WAVE_STICKER, Some((guild_id, message_id))).await },
            move |app, result| {
                if let Err(error) = result {
                    button.set_sensitive(true);
                    app.set_status(&format!("Could not wave: {error}"));
                }
            },
        );
    }

    /// The "Wave to say hi!" button under a join message.
    pub(crate) fn wave_button(self: &Rc<Self>, message: &Message) -> gtk::Button {
        let button = gtk::Button::new();
        button.add_css_class("kc-wave");
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.append(&self.sticker_picture(WAVE_STICKER, LOTTIE, 28, true));
        content.append(&label("Wave to say hi!", &[]));
        button.set_child(Some(&content));
        button.set_halign(gtk::Align::Start);
        let app = self.clone();
        let message = message.clone();
        button.connect_clicked(move |button| app.wave_to(&message, button));
        button
    }

    /// The sticker picker, pointing at `anchor`.
    pub(crate) fn show_sticker_picker(self: &Rc<Self>, anchor: &gtk::Widget) -> Option<gtk::Popover> {
        let Some(sections) = self
            .with_session(|session| {
                let channel = session.selected_channel?;
                Some(session.store.read(|state| {
                    let guild_id = state.channels.get(&channel).and_then(|channel| channel.guild_id);
                    let nitro = state.me.as_ref().is_some_and(kc_state::User::has_nitro);
                    // This server's own first; others' only with Nitro.
                    let mut sections: Vec<(String, Vec<Sticker>)> = Vec::new();
                    if let Some(guild) = guild_id.and_then(|id| state.guilds.get(&id)) {
                        sections.push((guild.name.clone(), guild.stickers.clone()));
                    }
                    if nitro {
                        for guild in state.ordered_guilds() {
                            if Some(guild.id) != guild_id && !guild.stickers.is_empty() {
                                sections.push((guild.name.clone(), guild.stickers.clone()));
                            }
                        }
                    }
                    sections
                }))
            })
            .flatten()
        else {
            return None;
        };

        let popover = gtk::Popover::new();
        popover.set_parent(anchor);
        popover.set_position(gtk::PositionType::Top);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_size_request(400, 440);
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Find stickers"));
        content.append(&search);
        let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&list));
        content.append(&scroll);
        popover.set_child(Some(&content));
        popover.connect_closed(|popover| popover.unparent());

        let sections = Rc::new(RefCell::new(sections));
        let fill: Rc<dyn Fn()> = {
            let (app, list, search, sections, popover) =
                (self.clone(), list.clone(), search.clone(), sections.clone(), popover.clone());
            Rc::new(move || {
                while let Some(child) = list.first_child() {
                    list.remove(&child);
                }
                let query = search.text().to_lowercase();
                let wanted = |sticker: &Sticker| {
                    sticker.available
                        && (query.is_empty()
                            || sticker.name.to_lowercase().contains(&query)
                            || sticker.tags.to_lowercase().contains(&query))
                };
                let packs = PACKS.with_borrow(Clone::clone);
                let mut all: Vec<(String, Vec<Sticker>)> = sections.borrow().clone();
                if let Some(packs) = &packs {
                    all.extend(packs.iter().map(|pack| (pack.name.clone(), pack.stickers.clone())));
                }
                let mut any = false;
                for (name, stickers) in all {
                    let shown: Vec<&Sticker> = stickers.iter().filter(|sticker| wanted(sticker)).collect();
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
                    grid.set_max_children_per_line(4);
                    grid.set_min_children_per_line(4);
                    for sticker in shown {
                        let button = gtk::Button::new();
                        button.add_css_class("flat");
                        button.add_css_class("kc-sticker-choice");
                        button.set_tooltip_text(Some(&sticker.name));
                        button.set_child(Some(&app.sticker_picture(sticker.id, sticker.format_type, PICKER_SIZE, false)));
                        let (app, popover, id) = (app.clone(), popover.clone(), sticker.id);
                        button.connect_clicked(move |_| {
                            popover.popdown();
                            app.send_sticker(id);
                        });
                        grid.append(&button);
                    }
                    list.append(&grid);
                }
                if !any {
                    let empty = label(
                        if packs.is_none() { "Loading stickers…" } else { "No stickers found" },
                        &["dim-label"],
                    );
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
        if PACKS.with_borrow(Option::is_none)
            && let Some((_, http, _)) = self.active()
        {
            let fill = fill.clone();
            spawn_local(self, async move { http.sticker_packs().await }, move |app, result| match result {
                Ok(packs) => {
                    PACKS.with_borrow_mut(|cached| *cached = Some(Rc::new(packs)));
                    fill();
                }
                Err(error) => app.set_status(&format!("Could not load Discord's stickers: {error}")),
            });
        }
        // Demos open it without a click, which a compositor won't grant a
        // grab for.
        if std::env::var_os("KUBECORD_DEMO").is_some() {
            popover.set_autohide(false);
        }
        popover.popup();
        search.grab_focus();
        Some(popover)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_becomes_a_texture_of_its_size() {
        let texture = frame_texture(vec![0xff00_ff00; 4], 2);
        assert_eq!((texture.width(), texture.height()), (2, 2));
    }
}
