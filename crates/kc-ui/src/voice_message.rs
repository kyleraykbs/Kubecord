//! Voice messages: a player in the message (play, pause, click the waveform
//! to jump), and recording one from the message box's menu.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::prelude::*;

use kc_media::voice_message::{Player, Recorder};
use kc_state::{Attachment, permissions};

use crate::shell::AppState;
use crate::widgets::{icon_button, label};

/// The longest voice message Discord takes.
const LONGEST_SECS: f64 = 20.0 * 60.0;
const PLAY_ICON: &str = "media-playback-start-symbolic";
const PAUSE_ICON: &str = "media-playback-pause-symbolic";

/// "0:07", "12:34".
fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Draws `levels` (0-255) as rounded bars across `area`, those before
/// `played` (0.0-1.0) in the accent colour.
fn draw_waveform(area: &gtk::DrawingArea, cr: &gtk::cairo::Context, width: i32, height: i32, levels: &[u8], played: f64) {
    let foreground = area.color();
    let accent = gtk::gdk::RGBA::new(0.35, 0.55, 1.0, 1.0);
    let bars = ((width as f64) / 4.0).floor().max(1.0) as usize;
    let (width, height) = (f64::from(width), f64::from(height));
    for bar in 0..bars {
        let level = if levels.is_empty() {
            0.0
        } else {
            let index = bar * levels.len() / bars;
            f64::from(levels[index.min(levels.len() - 1)]) / 255.0
        };
        let tall = (level * (height - 2.0)).max(2.0);
        let x = bar as f64 * width / bars as f64;
        let colour = if (bar as f64 + 0.5) / bars as f64 <= played { accent } else { foreground };
        cr.set_source_rgba(
            f64::from(colour.red()),
            f64::from(colour.green()),
            f64::from(colour.blue()),
            if (bar as f64 + 0.5) / bars as f64 <= played { 1.0 } else { 0.45 },
        );
        cr.rectangle(x, (height - tall) / 2.0, 2.0, tall);
        let _ = cr.fill();
    }
}

/// One voice message's player, shared by its widgets.
struct Playback {
    url: String,
    duration: f64,
    player: RefCell<Option<Player>>,
    /// The share played, for drawing.
    played: Cell<f64>,
    loading: Cell<bool>,
    button: gtk::Button,
    area: gtk::DrawingArea,
    time: gtk::Label,
}

thread_local! {
    /// The one playing now (starting another pauses it).
    static PLAYING: RefCell<Weak<Playback>> = const { RefCell::new(Weak::new()) };
}

impl Playback {
    fn show(&self) {
        let playing = self.player.borrow().as_ref().is_some_and(Player::is_playing);
        self.button.set_icon_name(if playing { PAUSE_ICON } else { PLAY_ICON });
        self.button.set_tooltip_text(Some(if playing { "Pause" } else { "Play" }));
        let position = self.player.borrow().as_ref().map_or(0.0, Player::position);
        self.time.set_text(&if playing || position > 0.0 {
            clock(position)
        } else {
            clock(self.duration)
        });
        self.area.queue_draw();
    }

    fn pause(&self) {
        if let Some(player) = self.player.borrow().as_ref() {
            player.pause();
        }
        self.show();
    }

    /// Follows the playback until it stops.
    fn follow(self: &Rc<Self>) {
        let me = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let Some(me) = me.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let (playing, position, duration) = {
                let mut player = me.player.borrow_mut();
                let Some(player) = player.as_mut() else {
                    return glib::ControlFlow::Break;
                };
                let _ = player.poll();
                (player.is_playing(), player.position(), player.duration().unwrap_or(me.duration))
            };
            if duration > 0.0 {
                me.played.set((position / duration).clamp(0.0, 1.0));
            }
            me.show();
            if playing { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
        });
    }

    fn start(self: &Rc<Self>) {
        let previous = PLAYING.with_borrow(Weak::upgrade);
        if let Some(previous) = previous.filter(|previous| !Rc::ptr_eq(previous, self)) {
            previous.pause();
        }
        PLAYING.with_borrow_mut(|playing| *playing = Rc::downgrade(self));
        if let Some(player) = self.player.borrow_mut().as_mut() {
            player.play();
        }
        self.show();
        self.follow();
    }
}

impl AppState {
    /// A voice message's player card.
    pub(crate) fn voice_message_widget(self: &Rc<Self>, attachment: &Attachment) -> gtk::Widget {
        let card = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        card.add_css_class("kc-voice-message");
        card.set_halign(gtk::Align::Start);
        card.set_margin_top(2);

        let button = icon_button(PLAY_ICON, "Play");
        button.add_css_class("circular");
        button.remove_css_class("flat");
        button.add_css_class("suggested-action");
        button.set_valign(gtk::Align::Center);
        card.append(&button);

        let area = gtk::DrawingArea::new();
        area.set_content_width(220);
        area.set_content_height(28);
        area.set_valign(gtk::Align::Center);
        area.set_cursor_from_name(Some("pointer"));
        area.set_tooltip_text(Some("Click to jump"));
        card.append(&area);

        let duration = attachment.duration_secs.unwrap_or(0.0);
        let time = label(&clock(duration), &["caption", "numeric"]);
        time.set_width_chars(5);
        card.append(&time);

        let url = if attachment.url.is_empty() { attachment.proxy_url.clone() } else { attachment.url.clone() };
        let playback = Rc::new(Playback {
            url,
            duration,
            player: RefCell::new(None),
            played: Cell::new(0.0),
            loading: Cell::new(false),
            button: button.clone(),
            area: area.clone(),
            time,
        });

        let levels = attachment.waveform_levels();
        let weak = Rc::downgrade(&playback);
        area.set_draw_func(move |area, cr, width, height| {
            let played = weak.upgrade().map_or(0.0, |playback| playback.played.get());
            draw_waveform(area, cr, width, height, &levels, played);
        });

        // Play downloads the file first (once), then plays it.
        let app = self.clone();
        let target = playback.clone();
        button.connect_clicked(move |_| {
            let playing = target.player.borrow().as_ref().is_some_and(Player::is_playing);
            if playing {
                target.pause();
            } else if target.player.borrow().is_some() {
                target.start();
            } else {
                app.load_voice_message(&target, None);
            }
        });

        let click = gtk::GestureClick::new();
        let app = self.clone();
        let target = playback.clone();
        click.connect_pressed(move |gesture, _, x, _| {
            let width = f64::from(gesture.widget().map_or(1, |widget| widget.width()).max(1));
            let fraction = (x / width).clamp(0.0, 1.0);
            target.played.set(fraction);
            let loaded = target.player.borrow().is_some();
            if loaded {
                if let Some(player) = target.player.borrow_mut().as_mut() {
                    player.seek(fraction);
                }
                target.start();
            } else {
                app.load_voice_message(&target, Some(fraction));
            }
        });
        area.add_controller(click);

        // Keeps the player as long as its card is shown.
        unsafe {
            card.set_data("kc-playback", playback);
        }
        card.upcast()
    }

    fn load_voice_message(self: &Rc<Self>, playback: &Rc<Playback>, at: Option<f64>) {
        if playback.loading.replace(true) {
            return;
        }
        playback.button.set_sensitive(false);
        let app = self.clone();
        let target = playback.clone();
        self.images.file(&playback.url, "ogg", move |path| {
            target.loading.set(false);
            target.button.set_sensitive(true);
            let opened = path.ok_or_else(|| "download failed".to_owned()).and_then(|path| {
                Player::open(&path).map_err(|error| error.to_string())
            });
            match opened {
                Ok(player) => {
                    target.player.replace(Some(player));
                    // The file must be ready (paused) before seeking works.
                    let target = target.clone();
                    glib::timeout_add_local_once(Duration::from_millis(80), move || {
                        if let (Some(fraction), Some(player)) = (at, target.player.borrow_mut().as_mut()) {
                            player.seek(fraction);
                        }
                        target.start();
                    });
                }
                Err(error) => app.set_status(&format!("Could not play the voice message: {error}")),
            }
        });
    }

    /// Whether voice messages may be sent in the open channel.
    pub(crate) fn can_send_voice_messages(&self) -> bool {
        self.with_session(|session| {
            let channel = session.selected_channel?;
            Some(session.store.read(|state| {
                let private = state.channels.get(&channel).is_some_and(|channel| channel.guild_id.is_none());
                state.can_send(channel)
                    && (private
                        || state.permissions_in(channel) & permissions::SEND_VOICE_MESSAGES != 0)
            }))
        })
        .flatten()
        .unwrap_or(false)
    }

    /// Records a voice message: the message box gives way to a bar with
    /// the time, a live waveform, Cancel and Send.
    pub(crate) fn record_voice_message(self: &Rc<Self>) {
        let Some((channel_id, reply_to)) =
            self.with_session(|session| session.selected_channel.map(|channel| (channel, session.reply_to))).flatten()
        else {
            return;
        };
        let recorder = match Recorder::start() {
            Ok(recorder) => Rc::new(RefCell::new(Some(recorder))),
            Err(error) => {
                self.set_status(&format!("Could not record: {error}"));
                return;
            }
        };
        let Some(parent) = self.composer_box.parent().and_downcast::<gtk::Box>() else {
            return;
        };

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        bar.add_css_class("kc-composer");
        bar.add_css_class("kc-recording");
        bar.set_height_request(self.composer_box.height().max(44));
        let cancel = icon_button("user-trash-symbolic", "Discard");
        cancel.add_css_class("circular");
        cancel.set_valign(gtk::Align::Center);
        cancel.set_margin_start(6);
        bar.append(&cancel);
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.add_css_class("kc-recording-dot");
        dot.set_valign(gtk::Align::Center);
        bar.append(&dot);
        let time = label("0:00", &["numeric"]);
        bar.append(&time);
        let area = gtk::DrawingArea::new();
        area.set_hexpand(true);
        area.set_content_height(28);
        area.set_valign(gtk::Align::Center);
        bar.append(&area);
        let send = icon_button(
            crate::widgets::first_icon(&["paper-plane-symbolic", "document-send-symbolic", "mail-send-symbolic"]),
            "Send Voice Message",
        );
        send.add_css_class("circular");
        send.remove_css_class("flat");
        send.add_css_class("suggested-action");
        send.set_valign(gtk::Align::Center);
        send.set_margin_end(6);
        bar.append(&send);

        parent.insert_child_after(&bar, Some(&self.composer_box));
        self.composer_box.set_visible(false);

        // The live waveform: the latest stretch, newest at the right.
        let live = recorder.clone();
        area.set_draw_func(move |area, cr, width, height| {
            let peaks = live.borrow().as_ref().map(Recorder::peaks).unwrap_or_default();
            let fit = (width / 4).max(1) as usize;
            let recent = &peaks[peaks.len().saturating_sub(fit)..];
            let levels: Vec<u8> = recent.iter().map(|peak| (peak.sqrt() * 255.0).min(255.0) as u8).collect();
            // Bars fill from the left until the recording is wider than the bar.
            let mut padded = levels;
            padded.resize(fit, 0);
            draw_waveform(area, cr, width, height, &padded, 0.0);
        });

        let done: Rc<dyn Fn(bool)> = {
            let (app, recorder, bar) = (self.clone(), recorder.clone(), bar.clone());
            Rc::new(move |keep: bool| {
                let Some(recorder) = recorder.borrow_mut().take() else {
                    return;
                };
                parent.remove(&bar);
                app.composer_box.set_visible(true);
                app.composer.grab_focus();
                if !keep {
                    return;
                }
                let clip = match recorder.finish() {
                    Ok(clip) if clip.duration_secs >= 0.5 => clip,
                    Ok(_) => {
                        app.set_status("Too short to send");
                        return;
                    }
                    Err(error) => {
                        app.set_status(&format!("Could not record: {error}"));
                        return;
                    }
                };
                let Some((_, http, _)) = app.active() else {
                    return;
                };
                if reply_to.is_some() {
                    app.cancel_context();
                }
                app.set_status("Sending voice message…");
                app.spawn(
                    async move {
                        http.send_voice_message(channel_id, clip.ogg, clip.duration_secs, &clip.waveform, reply_to)
                            .await
                    },
                    |app, result| match result {
                        Ok(_) => app.set_status(""),
                        Err(error) => app.set_status(&format!("Could not send the voice message: {error}")),
                    },
                );
            })
        };

        {
            let done = done.clone();
            cancel.connect_clicked(move |_| done(false));
        }
        {
            let done = done.clone();
            send.connect_clicked(move |_| done(true));
        }
        let keys = gtk::EventControllerKey::new();
        {
            let done = done.clone();
            keys.connect_key_pressed(move |_, key, _, _| match key {
                gtk::gdk::Key::Escape => {
                    done(false);
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter => {
                    done(true);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            });
        }
        bar.add_controller(keys);
        send.grab_focus();

        let app = self.clone();
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let (elapsed, failed) = match recorder.borrow().as_ref() {
                Some(recorder) => (recorder.elapsed(), recorder.failed()),
                None => return glib::ControlFlow::Break,
            };
            if let Some(error) = failed {
                app.set_status(&format!("Could not record: {error}"));
                done(false);
                return glib::ControlFlow::Break;
            }
            time.set_text(&clock(elapsed));
            area.queue_draw();
            if elapsed >= LONGEST_SECS {
                done(true);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_reads_minutes_and_seconds() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(7.4), "0:07");
        assert_eq!(clock(754.0), "12:34");
    }
}
