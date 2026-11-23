//! Forwarding a message: pick up to five DMs or channels, add a note if you
//! like, and send.
//!
//! A forward is a message with no content whose `message_reference` has
//! `type: 1` and names the original; Discord copies the original into its
//! `message_snapshots`. A note goes in a message of its own after it, as the
//! web client sends it.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::Snowflake;
use kc_state::{ChannelType, State};

use crate::shell::AppState;
use crate::widgets::label;

/// Discord forwards to at most this many places at once.
const MAX_DESTINATIONS: usize = 5;
/// Rows shown at once (a search narrows them).
const MAX_ROWS: usize = 60;

/// Somewhere a message can go.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Destination {
    channel_id: Snowflake,
    /// "ada" for a DM, "#general" for a channel.
    title: String,
    /// The server's name, for a channel.
    place: Option<String>,
    private: bool,
}

/// DMs (most recent first), then the channels of each server one may post
/// in, in sidebar order.
fn destinations(state: &State) -> Vec<Destination> {
    let mut all: Vec<Destination> = state
        .dm_channels()
        .into_iter()
        .map(|channel| Destination {
            channel_id: channel.id,
            title: state.channel_title(channel),
            place: None,
            private: true,
        })
        .collect();
    for guild in state.ordered_guilds() {
        for channel in state.guild_channels(guild.id) {
            if matches!(channel.kind, ChannelType::GuildText | ChannelType::GuildAnnouncement)
                && state.can_send(channel.id)
            {
                all.push(Destination {
                    channel_id: channel.id,
                    title: format!("#{}", channel.name.as_deref().unwrap_or("channel")),
                    place: Some(guild.name.clone()),
                    private: false,
                });
            }
        }
    }
    all
}

/// Whether a destination matches what was typed (every word, anywhere in
/// its title or server, ignoring case and a leading `#`).
fn matches(destination: &Destination, query: &str) -> bool {
    let haystack = format!(
        "{} {}",
        destination.title.to_lowercase(),
        destination.place.as_deref().unwrap_or("").to_lowercase()
    );
    query
        .split_whitespace()
        .map(|word| word.trim_start_matches('#').to_lowercase())
        .all(|word| haystack.contains(&word))
}

impl AppState {
    /// The Forward dialog for a message in `channel_id`.
    pub(crate) fn show_forward(self: &Rc<Self>, channel_id: Snowflake, message_id: Snowflake) {
        let Some((all, guild_id)) = self.with_session(|session| {
            session.store.read(|state| {
                let guild = state.channels.get(&channel_id).and_then(|channel| channel.guild_id);
                (destinations(state), guild)
            })
        }) else {
            return;
        };
        let all = Rc::new(all);
        let chosen: Rc<RefCell<BTreeSet<Snowflake>>> = Rc::default();

        let dialog = adw::Dialog::new();
        dialog.set_title("Forward To");
        dialog.set_content_width(420);
        dialog.set_content_height(600);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_margin_bottom(12);
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search DMs and channels"));
        content.append(&search);
        let counter = label("", &["caption", "dim-label"]);
        counter.set_xalign(0.0);
        content.append(&counter);

        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&list));
        content.append(&scroll);

        let note = gtk::Entry::new();
        note.set_placeholder_text(Some("Add an optional message…"));
        content.append(&note);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let send = gtk::Button::with_label("Send");
        send.add_css_class("suggested-action");
        send.set_sensitive(false);
        buttons.append(&cancel);
        buttons.append(&send);
        content.append(&buttons);

        // Lists what matches the search, chosen ones first, with a check box
        // each (disabled for the rest once five are chosen).
        let fill: Rc<dyn Fn()> = {
            let (list, search, all, chosen, counter, send) =
                (list.clone(), search.clone(), all.clone(), chosen.clone(), counter.clone(), send.clone());
            let refill: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::default();
            let me = refill.clone();
            let fill: Rc<dyn Fn()> = Rc::new(move || {
                while let Some(row) = list.first_child() {
                    list.remove(&row);
                }
                let query = search.text().to_string();
                let picked = chosen.borrow().clone();
                let full = picked.len() >= MAX_DESTINATIONS;
                let mut shown: Vec<&Destination> =
                    all.iter().filter(|dest| picked.contains(&dest.channel_id)).collect();
                shown.extend(
                    all.iter()
                        .filter(|dest| !picked.contains(&dest.channel_id) && matches(dest, &query))
                        .take(MAX_ROWS),
                );
                for dest in shown {
                    let row = adw::ActionRow::new();
                    row.set_title(&glib::markup_escape_text(&dest.title));
                    if let Some(place) = &dest.place {
                        row.set_subtitle(&glib::markup_escape_text(place));
                    }
                    if dest.private {
                        row.add_prefix(&gtk::Image::from_icon_name("avatar-default-symbolic"));
                    }
                    let check = gtk::CheckButton::new();
                    let on = picked.contains(&dest.channel_id);
                    check.set_active(on);
                    check.set_sensitive(on || !full);
                    check.set_valign(gtk::Align::Center);
                    row.add_suffix(&check);
                    row.set_activatable_widget(Some(&check));
                    let id = dest.channel_id;
                    let (chosen, me) = (chosen.clone(), me.clone());
                    check.connect_toggled(move |check| {
                        {
                            let mut chosen = chosen.borrow_mut();
                            if check.is_active() {
                                chosen.insert(id);
                            } else {
                                chosen.remove(&id);
                            }
                        }
                        // Redrawn after the toggle finishes (the row is in it).
                        let me = me.clone();
                        glib::idle_add_local_once(move || {
                            if let Some(fill) = me.borrow().as_ref() {
                                fill();
                            }
                        });
                    });
                    list.append(&row);
                }
                let count = picked.len();
                counter.set_text(&if count == 0 {
                    format!("Choose up to {MAX_DESTINATIONS}")
                } else {
                    format!("{count} of {MAX_DESTINATIONS} chosen")
                });
                send.set_sensitive(count > 0);
            });
            refill.replace(Some(fill.clone()));
            fill
        };
        fill();
        {
            let fill = fill.clone();
            search.connect_search_changed(move |_| fill());
        }

        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&content));
        dialog.set_child(Some(&view));

        let target = dialog.clone();
        cancel.connect_clicked(move |_| {
            target.close();
        });
        let app = self.clone();
        let target = dialog.clone();
        send.connect_clicked(move |_| {
            let to: Vec<Snowflake> = chosen.borrow().iter().copied().collect();
            let note = note.text().trim().to_owned();
            target.close();
            app.forward_message(guild_id, channel_id, message_id, to, note);
        });
        dialog.present(Some(&self.window));
        search.grab_focus();
    }

    fn forward_message(
        self: &Rc<Self>,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
        message_id: Snowflake,
        to: Vec<Snowflake>,
        note: String,
    ) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        self.spawn(
            async move {
                let mut failed = 0;
                for destination in &to {
                    let forward = kc_http::Outgoing {
                        forward: Some((guild_id, channel_id, message_id)),
                        ..kc_http::Outgoing::default()
                    };
                    if http.send(*destination, &forward).await.is_err() {
                        failed += 1;
                        continue;
                    }
                    if !note.is_empty() {
                        let text = kc_http::Outgoing {
                            content: note.clone(),
                            ..kc_http::Outgoing::default()
                        };
                        let _ = http.send(*destination, &text).await;
                    }
                }
                (to.len(), failed)
            },
            |app, (sent, failed)| {
                app.set_status(&match failed {
                    0 if sent == 1 => "Forwarded".to_owned(),
                    0 => format!("Forwarded to {sent} places"),
                    failed => format!("Could not forward to {failed} of {sent}"),
                });
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matches_every_word_in_title_or_server() {
        let dest = Destination {
            channel_id: Snowflake(1),
            title: "#general".into(),
            place: Some("Rust Lovers".into()),
            private: false,
        };
        assert!(matches(&dest, ""));
        assert!(matches(&dest, "#gen"));
        assert!(matches(&dest, "rust gen"));
        assert!(!matches(&dest, "rust random"));
    }
}
