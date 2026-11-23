//! Polls: shown in their message (vote, take a vote back, see the results,
//! end one of your own), and made with the Create Poll dialog.
//!
//! Requests as Discord documents them: a vote is `PUT
//! /channels/{channel}/polls/{message}/answers/@me` with the chosen answer
//! ids (they replace any earlier vote; none takes it back), ending is `POST
//! .../expire`, and a poll is a message with a `poll` field. Votes arrive as
//! MESSAGE_POLL_VOTE_ADD/REMOVE, ours included.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::Snowflake;
use kc_http::NewPoll;
use kc_state::{Message, Poll, State, permissions};

use crate::shell::AppState;
use crate::widgets::label;

/// Durations offered when making a poll, in hours.
const DURATIONS: [(&str, u32); 7] = [
    ("1 hour", 1),
    ("4 hours", 4),
    ("8 hours", 8),
    ("24 hours", 24),
    ("3 days", 72),
    ("1 week", 168),
    ("2 weeks", 336),
];
/// Discord's limits.
const MAX_ANSWERS: usize = 10;
const MAX_QUESTION: usize = 300;
const MAX_ANSWER: usize = 55;

/// What the reader has picked but not sent yet, and whether they asked to
/// see the results, by message (kept across redraws).
#[derive(Default)]
struct Local {
    picked: HashSet<u32>,
    results: bool,
}

thread_local! {
    static LOCAL: RefCell<HashMap<Snowflake, Local>> = RefCell::new(HashMap::new());
}

fn now_ms() -> u64 {
    kc_state::time::unix_millis_now()
}

/// "3 days left", "5 hours left", "12 minutes left".
fn time_left(poll: &Poll) -> Option<String> {
    let end = poll.expiry.as_deref().and_then(kc_state::time::iso_to_unix_millis)?;
    let minutes = end.saturating_sub(now_ms()) / 60_000;
    let (amount, unit) = match minutes {
        0 => return Some("Ending now".to_owned()),
        minutes if minutes < 60 => (minutes, "minute"),
        minutes if minutes < 48 * 60 => (minutes / 60, "hour"),
        minutes => (minutes / (24 * 60), "day"),
    };
    Some(format!("{amount} {unit}{} left", if amount == 1 { "" } else { "s" }))
}

fn votes(count: u32) -> String {
    format!("{count} vote{}", if count == 1 { "" } else { "s" })
}

impl AppState {
    /// A poll's card in its message.
    pub(crate) fn poll_widget(self: &Rc<Self>, message: &Message, poll: &Poll, state: &State) -> gtk::Widget {
        let (channel_id, message_id) = (message.channel_id, message.id);
        let closed = poll.is_closed(now_ms());
        let mine = state.my_id() == Some(message.author.id);
        let (picked, asked) = LOCAL.with_borrow(|local| {
            local
                .get(&message_id)
                .map(|local| (local.picked.clone(), local.results))
                .unwrap_or_default()
        });
        // Results show once voted, when it is over, or when asked for.
        let results = poll.has_voted() || closed || asked;
        let total = poll.total();
        let leader = poll.answers.iter().map(|answer| poll.count(answer.answer_id)).max().unwrap_or(0);

        let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
        card.add_css_class("kc-poll");
        card.set_margin_top(4);
        let question = label(poll.question.text.as_deref().unwrap_or("Poll"), &["heading"]);
        question.set_xalign(0.0);
        question.set_wrap(true);
        card.append(&question);
        let hint = if closed {
            "Voting has ended"
        } else if poll.allow_multiselect {
            "Select one or more answers"
        } else {
            "Select one answer"
        };
        card.append(&label(hint, &["caption", "dim-label"]));

        for answer in &poll.answers {
            let id = answer.answer_id;
            let count = poll.count(id);
            let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            if let Some(emoji) = &answer.poll_media.emoji {
                let shown = match (emoji.id, emoji.name.as_deref()) {
                    (Some(id), _) => {
                        let image = gtk::Picture::new();
                        image.set_size_request(20, 20);
                        image.set_can_shrink(true);
                        self.images.set_picture(&image, &kc_http::cdn::emoji(id, emoji.animated, 48));
                        image.upcast::<gtk::Widget>()
                    }
                    (None, Some(name)) => label(name, &[]).upcast(),
                    _ => gtk::Box::new(gtk::Orientation::Horizontal, 0).upcast(),
                };
                line.append(&shown);
            }
            let text = label(answer.poll_media.text.as_deref().unwrap_or(""), &[]);
            text.set_xalign(0.0);
            text.set_wrap(true);
            text.set_hexpand(true);
            line.append(&text);
            if results {
                if poll.voted(id) {
                    let check = gtk::Image::from_icon_name("object-select-symbolic");
                    check.set_tooltip_text(Some("Your vote"));
                    line.append(&check);
                }
                let percent = if total == 0 { 0 } else { (count * 100 + total / 2) / total };
                let figure = label(&format!("{percent}%"), &["numeric"]);
                figure.set_tooltip_text(Some(&votes(count)));
                line.append(&figure);
            }
            row.append(&line);
            if results {
                let bar = gtk::ProgressBar::new();
                bar.set_fraction(if total == 0 { 0.0 } else { f64::from(count) / f64::from(total) });
                if closed && count == leader && leader > 0 {
                    bar.add_css_class("kc-poll-winner");
                }
                row.append(&bar);
            }

            let button = gtk::ToggleButton::new();
            button.add_css_class("kc-poll-answer");
            button.set_child(Some(&row));
            if results {
                button.set_active(poll.voted(id));
                button.set_sensitive(false);
            } else {
                button.set_active(picked.contains(&id));
                let app = self.clone();
                let multi = poll.allow_multiselect;
                button.connect_toggled(move |button| {
                    LOCAL.with_borrow_mut(|local| {
                        let local = local.entry(message_id).or_default();
                        if button.is_active() {
                            if !multi {
                                local.picked.clear();
                            }
                            local.picked.insert(id);
                        } else {
                            local.picked.remove(&id);
                        }
                    });
                    app.refresh_message(channel_id, message_id);
                });
            }
            card.append(&button);
        }

        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let mut facts = vec![votes(total)];
        if closed {
            facts.push("Poll closed".to_owned());
        } else if let Some(left) = time_left(poll) {
            facts.push(left);
        }
        let facts = label(&facts.join(" · "), &["caption", "dim-label"]);
        facts.set_xalign(0.0);
        facts.set_hexpand(true);
        footer.append(&facts);

        if !closed {
            if !poll.has_voted() {
                let show = gtk::Button::with_label(if asked { "Hide Results" } else { "Show Results" });
                show.add_css_class("flat");
                let app = self.clone();
                show.connect_clicked(move |_| {
                    LOCAL.with_borrow_mut(|local| {
                        let local = local.entry(message_id).or_default();
                        local.results = !local.results;
                    });
                    app.refresh_message(channel_id, message_id);
                });
                footer.append(&show);
            }
            if poll.has_voted() {
                let remove = gtk::Button::with_label("Remove Vote");
                remove.add_css_class("flat");
                let app = self.clone();
                remove.connect_clicked(move |_| app.send_vote(channel_id, message_id, Vec::new()));
                footer.append(&remove);
            } else if !asked {
                let vote = gtk::Button::with_label("Vote");
                vote.add_css_class("suggested-action");
                vote.set_sensitive(!picked.is_empty());
                let app = self.clone();
                vote.connect_clicked(move |_| {
                    let mut chosen: Vec<u32> = picked.iter().copied().collect();
                    chosen.sort_unstable();
                    app.send_vote(channel_id, message_id, chosen);
                });
                footer.append(&vote);
            }
            if mine {
                let end = gtk::Button::with_label("End Poll");
                end.add_css_class("flat");
                let app = self.clone();
                end.connect_clicked(move |_| app.confirm_end_poll(channel_id, message_id));
                footer.append(&end);
            }
        }
        card.append(&footer);
        // A card of a steady, readable width at the message's start (not
        // stretched across a wide chat, nor shrunk to a short question).
        card.set_halign(gtk::Align::Start);
        card.set_size_request(420, -1);
        card.upcast()
    }

    /// Redraws one message (a poll's picks changed).
    fn refresh_message(self: &Rc<Self>, channel_id: Snowflake, message_id: Snowflake) {
        self.message_updated(channel_id, message_id);
    }

    fn send_vote(self: &Rc<Self>, channel_id: Snowflake, message_id: Snowflake, answers: Vec<u32>) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        LOCAL.with_borrow_mut(|local| {
            local.remove(&message_id);
        });
        self.spawn(
            async move { http.vote_poll(channel_id, message_id, &answers).await },
            move |app, result| {
                if let Err(error) = result {
                    app.set_status(&format!("Could not vote: {error}"));
                }
                app.refresh_message(channel_id, message_id);
            },
        );
    }

    fn confirm_end_poll(self: &Rc<Self>, channel_id: Snowflake, message_id: Snowflake) {
        let dialog = adw::AlertDialog::new(
            Some("End the Poll Now?"),
            Some("Voting stops and everyone sees the final results."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("end", "End Poll")]);
        dialog.set_response_appearance("end", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let app = self.clone();
        dialog.connect_response(Some("end"), move |_, _| {
            let Some((_, http, _)) = app.active() else {
                return;
            };
            app.spawn(
                async move { http.end_poll(channel_id, message_id).await },
                |app, result| {
                    if let Err(error) = result {
                        app.set_status(&format!("Could not end the poll: {error}"));
                    }
                },
            );
        });
        dialog.present(Some(&self.window));
    }

    /// Whether polls may be made in the open channel.
    pub(crate) fn can_make_polls(&self) -> bool {
        self.with_session(|session| {
            let channel = session.selected_channel?;
            Some(session.store.read(|state| {
                let private = state.channels.get(&channel).is_some_and(|channel| channel.guild_id.is_none());
                state.can_send(channel) && (private || state.permissions_in(channel) & permissions::SEND_POLLS != 0)
            }))
        })
        .flatten()
        .unwrap_or(false)
    }

    /// The Create Poll dialog, for the open channel.
    pub(crate) fn show_create_poll(self: &Rc<Self>) {
        let Some(channel_id) = self.with_session(|session| session.selected_channel).flatten() else {
            return;
        };
        let dialog = adw::Dialog::new();
        dialog.set_title("Create a Poll");
        dialog.set_content_width(440);
        dialog.set_content_height(620);
        let page = adw::PreferencesPage::new();

        let question_group = adw::PreferencesGroup::new();
        question_group.set_title("Question");
        let question = adw::EntryRow::new();
        question.set_title("What do you want to ask?");
        question.set_max_length(MAX_QUESTION as i32);
        question_group.add(&question);
        page.add(&question_group);

        let answers_group = adw::PreferencesGroup::new();
        answers_group.set_title("Answers");
        let add = gtk::Button::from_icon_name("list-add-symbolic");
        add.add_css_class("flat");
        add.set_tooltip_text(Some("Add Another Answer"));
        answers_group.set_header_suffix(Some(&add));
        let answers: Rc<RefCell<Vec<(adw::EntryRow, gtk::Entry)>>> = Rc::new(RefCell::new(Vec::new()));
        let add_answer = {
            let answers = answers.clone();
            let group = answers_group.clone();
            let add = add.clone();
            move || {
                let count = answers.borrow().len();
                if count >= MAX_ANSWERS {
                    return;
                }
                let row = adw::EntryRow::new();
                row.set_title(&format!("Answer {}", count + 1));
                row.set_max_length(MAX_ANSWER as i32);
                let emoji = gtk::Entry::new();
                emoji.set_placeholder_text(Some("🙂"));
                emoji.set_max_width_chars(3);
                emoji.set_width_chars(3);
                emoji.set_valign(gtk::Align::Center);
                emoji.set_tooltip_text(Some("An emoji for this answer (optional)"));
                row.add_suffix(&emoji);
                group.add(&row);
                answers.borrow_mut().push((row, emoji));
                add.set_sensitive(answers.borrow().len() < MAX_ANSWERS);
            }
        };
        add_answer();
        add_answer();
        {
            let add_answer = add_answer.clone();
            add.connect_clicked(move |_| add_answer());
        }
        page.add(&answers_group);

        let options = adw::PreferencesGroup::new();
        let duration = adw::ComboRow::new();
        duration.set_title("Duration");
        let names: Vec<&str> = DURATIONS.iter().map(|(name, _)| *name).collect();
        duration.set_model(Some(&gtk::StringList::new(&names)));
        duration.set_selected(3);
        options.add(&duration);
        let multi = adw::SwitchRow::new();
        multi.set_title("Allow Multiple Answers");
        options.add(&multi);
        page.add(&options);

        let cancel = gtk::Button::with_label("Cancel");
        let post = gtk::Button::with_label("Post");
        post.add_css_class("suggested-action");
        post.set_sensitive(false);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.set_margin_top(8);
        buttons.set_margin_bottom(12);
        buttons.set_margin_end(12);
        buttons.append(&cancel);
        buttons.append(&post);

        // A question and at least two answers.
        let ready = {
            let question = question.clone();
            let answers = answers.clone();
            let post = post.clone();
            move || {
                let filled = answers.borrow().iter().filter(|(row, _)| !row.text().trim().is_empty()).count();
                post.set_sensitive(!question.text().trim().is_empty() && filled >= 2);
            }
        };
        {
            let ready = ready.clone();
            question.connect_changed(move |_| ready());
        }
        {
            // Rows added later connect when the button adds them; checking
            // on every key of the dialog covers both.
            let keys = gtk::EventControllerKey::new();
            let ready = ready.clone();
            keys.connect_key_released(move |_, _, _, _| ready());
            dialog.add_controller(keys);
        }

        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&page));
        view.add_bottom_bar(&buttons);
        dialog.set_child(Some(&view));

        let target = dialog.clone();
        cancel.connect_clicked(move |_| {
            target.close();
        });
        let app = self.clone();
        let target = dialog.clone();
        let first = question.clone();
        post.connect_clicked(move |_| {
            let poll = NewPoll {
                question: question.text().to_string(),
                answers: answers
                    .borrow()
                    .iter()
                    .map(|(row, emoji)| {
                        let emoji = emoji.text().trim().to_owned();
                        (row.text().to_string(), (!emoji.is_empty()).then_some(emoji))
                    })
                    .collect(),
                hours: DURATIONS[(duration.selected() as usize).min(DURATIONS.len() - 1)].1,
                allow_multiselect: multi.is_active(),
            };
            target.close();
            app.post_poll(channel_id, poll);
        });
        dialog.present(Some(&self.window));
        first.grab_focus();
    }

    fn post_poll(self: &Rc<Self>, channel_id: Snowflake, poll: NewPoll) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let message = kc_http::Outgoing {
            poll: Some(poll.body()),
            ..kc_http::Outgoing::default()
        };
        self.spawn(
            async move { http.send(channel_id, &message).await },
            |app, result| {
                if let Err(error) = result {
                    app.set_status(&format!("Could not post the poll: {error}"));
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_polls_body_is_what_discord_takes() {
        let poll = NewPoll {
            question: " Lunch? ".into(),
            answers: vec![("Pizza".into(), Some("🍕".into())), ("  ".into(), None), ("Sushi".into(), None)],
            hours: 24,
            allow_multiselect: true,
        };
        assert_eq!(
            poll.body(),
            serde_json::json!({
                "question": {"text": "Lunch?"},
                "answers": [
                    {"poll_media": {"text": "Pizza", "emoji": {"name": "🍕"}}},
                    {"poll_media": {"text": "Sushi"}}
                ],
                "allow_multiselect": true,
                "duration": 24,
                "layout_type": 1
            })
        );
    }

    #[test]
    fn time_left_reads_naturally() {
        let in_ms = |ms: u64| {
            let poll = Poll {
                expiry: Some(kc_state::time::millis_to_iso(now_ms() + ms)),
                ..Poll::default()
            };
            time_left(&poll).unwrap()
        };
        assert_eq!(in_ms(90 * 60_000 + 30_000), "1 hour left");
        assert_eq!(in_ms(5 * 60_000 + 30_000), "5 minutes left");
        assert_eq!(in_ms(3 * 24 * 3_600_000 + 60_000), "3 days left");
    }
}
