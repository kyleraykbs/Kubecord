//! Group DMs: the friend picker that starts a DM or a group DM (or adds
//! friends to a group), and renaming, leaving and removing members.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use serde_json::json;

use kc_core::Snowflake;
use kc_http::cdn;
use kc_state::{Channel, ChannelType, Relationship, User};

use crate::shell::AppState;
use crate::widgets::label;

/// Discord's cap on a group DM: ten people, the account included.
pub(crate) const MAX_GROUP_SIZE: usize = 10;

/// How many more friends may be picked when `members` people (the account
/// included) are in already and `picked` are chosen.
pub(crate) fn room_left(members: usize, picked: usize) -> usize {
    MAX_GROUP_SIZE.saturating_sub(members + picked)
}

/// Picks or unpicks a friend; refuses (returns false) once the group is full.
pub(crate) fn toggle_pick(
    picked: &mut Vec<Snowflake>,
    id: Snowflake,
    on: bool,
    members: usize,
) -> bool {
    if !on {
        picked.retain(|other| *other != id);
        return true;
    }
    if picked.contains(&id) {
        return true;
    }
    if room_left(members, picked.len()) == 0 {
        return false;
    }
    picked.push(id);
    true
}

/// What the picker's button does with the friends chosen.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Submit {
    Nothing,
    /// One friend from the DM list: their (possibly existing) one-to-one DM.
    OpenDm(Snowflake),
    CreateGroup(Vec<Snowflake>),
    /// Friends added one by one to an existing group.
    AddTo(Snowflake, Vec<Snowflake>),
}

/// `group` is the group being added to, or `None` when starting one.
pub(crate) fn submit_plan(group: Option<Snowflake>, picked: &[Snowflake]) -> Submit {
    match (group, picked) {
        (_, []) => Submit::Nothing,
        (None, [one]) => Submit::OpenDm(*one),
        (None, many) => Submit::CreateGroup(many.to_vec()),
        (Some(group), some) => Submit::AddTo(group, some.to_vec()),
    }
}

fn submit_label(group: Option<Snowflake>, picked: usize) -> &'static str {
    match (group, picked) {
        (Some(_), _) => "Add",
        (None, 0 | 1) => "Create DM",
        (None, _) => "Create Group DM",
    }
}

fn room_text(room: usize) -> String {
    match room {
        0 => "This group has a 10 member limit.".to_owned(),
        1 => "You can add 1 more friend.".to_owned(),
        room => format!("You can add {room} more friends."),
    }
}

/// One friend in the picker.
struct Friend {
    id: Snowflake,
    name: String,
    username: String,
    avatar: String,
}

impl AppState {
    /// The friend picker. `group` adds to that group DM; without it the
    /// choice starts a DM (one friend) or a group DM (several).
    pub(crate) fn group_dm_dialog(self: &Rc<Self>, group: Option<Snowflake>) {
        let Some((friends, members, title)) = self.with_session(|session| {
            session.store.read(|state| {
                let channel = group.and_then(|id| state.channels.get(&id));
                let inside: Vec<Snowflake> = channel
                    .and_then(|channel| channel.recipients.as_ref())
                    .map(|users| users.iter().map(|user| user.id).collect())
                    .unwrap_or_default();
                let friends: Vec<Friend> = state
                    .relationships_of(Relationship::FRIEND)
                    .into_iter()
                    .filter(|(_, user)| !inside.contains(&user.id))
                    .map(|(_, user)| Friend {
                        id: user.id,
                        name: state.display_name(None, user),
                        username: user.username.clone(),
                        avatar: cdn::user_avatar(user.id, user.avatar.as_deref(), 64),
                    })
                    .collect();
                let title = channel.map(|channel| state.channel_title(channel));
                (friends, inside.len() + 1, title)
            })
        }) else {
            return;
        };

        let dialog = adw::Dialog::new();
        dialog.set_title(match &title {
            Some(_) => "Add Friends to Group",
            None => "Select Friends",
        });
        dialog.set_content_width(420);
        dialog.set_content_height(560);

        let header = adw::HeaderBar::new();
        let heading = adw::WindowTitle::new(&dialog.title(), &room_text(room_left(members, 0)));
        header.set_title_widget(Some(&heading));

        let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        body.set_margin_top(6);
        body.set_margin_bottom(12);
        body.set_margin_start(12);
        body.set_margin_end(12);
        if let Some(title) = &title {
            body.append(&label(&format!("Adding to {title}"), &["dim-label"]));
        }
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Type the username of a friend"));
        body.append(&search);
        let chips = gtk::FlowBox::new();
        chips.set_selection_mode(gtk::SelectionMode::None);
        chips.set_max_children_per_line(4);
        chips.set_row_spacing(4);
        chips.set_column_spacing(4);
        chips.set_visible(false);
        body.append(&chips);

        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        list.set_valign(gtk::Align::Start);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&list));
        body.append(&scroll);

        let submit = gtk::Button::with_label(submit_label(group, 0));
        submit.add_css_class("suggested-action");
        submit.add_css_class("pill");
        submit.set_halign(gtk::Align::Center);
        submit.set_sensitive(false);
        body.append(&submit);

        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&body));
        dialog.set_child(Some(&view));

        if friends.is_empty() {
            let row = adw::ActionRow::new();
            row.set_title(if group.is_some() {
                "All of your friends are in this group"
            } else {
                "You have no friends to add yet"
            });
            list.append(&row);
        }

        let picked: Rc<RefCell<Vec<Snowflake>>> = Rc::default();
        let checks: Rc<RefCell<Vec<(Snowflake, gtk::CheckButton)>>> = Rc::default();
        let names: Rc<Vec<(Snowflake, String)>> = Rc::new(
            friends
                .iter()
                .map(|friend| (friend.id, friend.name.clone()))
                .collect(),
        );
        // Redraws what depends on the choice: chips, count, button, and
        // which unchecked rows can still be ticked.
        let refresh: Rc<dyn Fn()> = {
            let picked = picked.clone();
            let checks = checks.clone();
            let chips = chips.clone();
            let heading = heading.clone();
            let submit = submit.clone();
            let names = names.clone();
            Rc::new(move || {
                let chosen = picked.borrow().clone();
                let room = room_left(members, chosen.len());
                heading.set_subtitle(&room_text(room));
                submit.set_label(submit_label(group, chosen.len()));
                submit.set_sensitive(!chosen.is_empty());
                for (id, check) in checks.borrow().iter() {
                    let on = chosen.contains(id);
                    if check.is_active() != on {
                        check.set_active(on);
                    }
                    if let Some(row) = check
                        .parent()
                        .and_then(|parent| parent.ancestor(adw::ActionRow::static_type()))
                    {
                        row.set_sensitive(on || room > 0);
                    }
                }
                clear_box_flow(&chips);
                chips.set_visible(!chosen.is_empty());
                for id in &chosen {
                    let name = names
                        .iter()
                        .find(|(other, _)| other == id)
                        .map_or_else(String::new, |(_, name)| name.clone());
                    let chip = gtk::Button::new();
                    chip.add_css_class("pill");
                    chip.add_css_class("kc-chip");
                    chip.set_tooltip_text(Some("Remove"));
                    let content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
                    content.append(&label(&name, &[]));
                    content.append(&gtk::Image::from_icon_name("window-close-symbolic"));
                    chip.set_child(Some(&content));
                    let checks = checks.clone();
                    let id = *id;
                    chip.connect_clicked(move |_| {
                        if let Some((_, check)) =
                            checks.borrow().iter().find(|(other, _)| *other == id)
                        {
                            check.set_active(false);
                        }
                    });
                    chips.insert(&chip, -1);
                }
            })
        };

        for friend in &friends {
            let row = adw::ActionRow::new();
            row.set_title(&glib::markup_escape_text(&friend.name));
            row.set_subtitle(&glib::markup_escape_text(&friend.username));
            let avatar = self.images.avatar(32, &friend.name, Some(&friend.avatar));
            row.add_prefix(&avatar);
            let check = gtk::CheckButton::new();
            check.set_valign(gtk::Align::Center);
            row.add_suffix(&check);
            row.set_activatable_widget(Some(&check));
            let picked = picked.clone();
            let refresh = refresh.clone();
            let id = friend.id;
            check.connect_toggled(move |check| {
                let accepted =
                    toggle_pick(&mut picked.borrow_mut(), id, check.is_active(), members);
                if !accepted {
                    check.set_active(false);
                }
                refresh();
            });
            checks.borrow_mut().push((friend.id, check));
            // Searched by both names.
            row.set_widget_name(&format!(
                "{}\n{}",
                friend.name.to_lowercase(),
                friend.username.to_lowercase()
            ));
            list.append(&row);
        }

        let query = Rc::new(RefCell::new(String::new()));
        {
            let query = query.clone();
            list.set_filter_func(move |row| {
                let query = query.borrow();
                query.is_empty() || row.widget_name().contains(query.as_str())
            });
        }
        {
            let list = list.clone();
            search.connect_search_changed(move |entry| {
                *query.borrow_mut() = entry.text().trim().to_lowercase();
                list.invalidate_filter();
            });
        }

        let app = self.clone();
        let parent = dialog.clone();
        submit.connect_clicked(move |_| {
            let chosen = picked.borrow().clone();
            parent.close();
            app.submit_group_pick(submit_plan(group, &chosen));
        });

        dialog.present(Some(&self.window));
        search.grab_focus();
    }

    fn submit_group_pick(self: &Rc<Self>, plan: Submit) {
        let Some((_, http, store)) = self.active() else {
            return;
        };
        match plan {
            Submit::Nothing => {}
            Submit::OpenDm(user) => self.open_dm_with(user),
            Submit::CreateGroup(users) => self.spawn(
                async move { http.create_group_dm(&users).await },
                move |app, result| match result {
                    Ok(channel) => {
                        // CHANNEL_CREATE follows on the gateway; storing it
                        // now opens it at once, and the event then only
                        // updates the same entry.
                        store.apply(
                            "CHANNEL_CREATE",
                            &serde_json::to_value(&channel).unwrap_or_default(),
                        );
                        let stored = store.read(|state| state.channels.get(&channel.id).cloned());
                        app.open_channel(stored.unwrap_or(channel));
                    }
                    Err(error) => app.report_failure("Could not create the group", &error),
                },
            ),
            Submit::AddTo(group, users) => {
                let known: Vec<User> = store.read(|state| {
                    users
                        .iter()
                        .filter_map(|id| state.users.get(id).cloned())
                        .collect()
                });
                self.spawn(
                    async move {
                        // Add in order, stopping at the first failure, and
                        // report which ids were accepted so only those show
                        // locally (the rejected ones must not appear added).
                        let mut added = Vec::new();
                        let mut failed = None;
                        for user in &users {
                            match http.add_group_recipient(group, *user).await {
                                Ok(()) => added.push(*user),
                                Err(error) => {
                                    failed = Some(error);
                                    break;
                                }
                            }
                        }
                        (added, failed)
                    },
                    move |app, (added, failed): (Vec<Snowflake>, Option<kc_http::Error>)| {
                        // Only the accepted users are applied; the gateway's
                        // CHANNEL_RECIPIENT_ADD replaces, not duplicates.
                        for user in &known {
                            if added.contains(&user.id) {
                                store.apply(
                                    "CHANNEL_RECIPIENT_ADD",
                                    &json!({"channel_id": group.to_string(), "user": user}),
                                );
                            }
                        }
                        if let Some(error) = failed {
                            app.report_failure("Could not add to the group", &error);
                        }
                    },
                );
            }
        }
    }

    /// Asks for a new group name; an empty one goes back to the members'.
    pub(crate) fn rename_group_dialog(self: &Rc<Self>, channel_id: Snowflake) {
        let Some(current) = self.group_channel(channel_id) else {
            return;
        };
        let dialog = adw::AlertDialog::new(Some("Rename Group"), None);
        let entry = gtk::Entry::new();
        entry.set_text(current.name.as_deref().unwrap_or(""));
        entry.set_placeholder_text(Some(&current.display_name()));
        entry.set_activates_default(true);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        let app = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "save" {
                return;
            }
            let Some((_, http, store)) = app.active() else {
                return;
            };
            let name = entry.text().to_string();
            app.spawn(
                async move { http.rename_group_dm(channel_id, &name).await },
                move |app, result| match result {
                    Ok(channel) => store.apply(
                        "CHANNEL_UPDATE",
                        &serde_json::to_value(&channel).unwrap_or_default(),
                    ),
                    Err(error) => app.report_failure("Could not rename the group", &error),
                },
            );
        });
        dialog.present(Some(&self.window));
    }

    /// Asks before leaving a group DM.
    pub(crate) fn confirm_leave_group(self: &Rc<Self>, channel_id: Snowflake) {
        let title = self
            .with_session(|session| {
                session.store.read(|state| {
                    state
                        .channels
                        .get(&channel_id)
                        .map(|channel| state.channel_title(channel))
                })
            })
            .flatten()
            .unwrap_or_default();
        let dialog = adw::AlertDialog::new(
            Some(&format!("Leave “{title}”?")),
            Some("You won't be able to rejoin this group unless you are re-invited."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("leave", "Leave Group")]);
        dialog.set_response_appearance("leave", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let app = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "leave" {
                app.close_private_channel(channel_id);
            }
        });
        dialog.present(Some(&self.window));
    }

    /// Closes a DM or leaves a group DM, and leaves its page if it is open.
    pub(crate) fn close_private_channel(self: &Rc<Self>, channel_id: Snowflake) {
        let Some((_, http, store)) = self.active() else {
            return;
        };
        self.spawn(
            async move { http.close_dm(channel_id).await },
            move |app, result| match result {
                Ok(()) => {
                    let open = app
                        .with_session(|session| session.selected_channel == Some(channel_id))
                        .unwrap_or(false);
                    // The gateway's CHANNEL_DELETE does the same again.
                    store.apply("CHANNEL_DELETE", &json!({"id": channel_id.to_string()}));
                    if open {
                        app.open_home();
                    }
                }
                Err(error) => app.report_failure("Could not close the conversation", &error),
            },
        );
    }

    /// Removes someone from a group DM the account owns.
    pub(crate) fn remove_from_group(self: &Rc<Self>, channel_id: Snowflake, user_id: Snowflake) {
        let Some((_, http, store)) = self.active() else {
            return;
        };
        let user = store.read(|state| state.users.get(&user_id).cloned());
        self.spawn(
            async move { http.remove_group_recipient(channel_id, user_id).await },
            move |app, result| match result {
                Ok(()) => {
                    if let Some(user) = user {
                        store.apply(
                            "CHANNEL_RECIPIENT_REMOVE",
                            &json!({"channel_id": channel_id.to_string(), "user": user}),
                        );
                    }
                }
                Err(error) => app.report_failure("Could not remove from the group", &error),
            },
        );
    }

    /// The group DM the account owns that is open, for the member menu's
    /// Remove From Group, when `user_id` is one of its other members.
    pub(crate) fn owned_group_with(&self, user_id: Snowflake) -> Option<Snowflake> {
        self.with_session(|session| {
            let channel_id = session.selected_channel?;
            session.store.read(|state| {
                let channel = state.channels.get(&channel_id)?;
                let mine = channel.owner_id.is_some() && channel.owner_id == state.my_id();
                let member = channel
                    .recipients
                    .iter()
                    .flatten()
                    .any(|user| user.id == user_id);
                (channel.kind == ChannelType::GroupDm && mine && member).then_some(channel_id)
            })
        })
        .flatten()
    }

    fn group_channel(&self, channel_id: Snowflake) -> Option<Channel> {
        self.with_session(|session| {
            session
                .store
                .read(|state| state.channels.get(&channel_id).cloned())
        })
        .flatten()
        .filter(|channel| channel.kind == ChannelType::GroupDm)
    }

    /// The right-click menu items of a group DM row.
    pub(crate) fn add_group_items(
        self: &Rc<Self>,
        menu: &gtk::Box,
        popover: &gtk::Popover,
        channel_id: Snowflake,
    ) {
        let items: [(&str, bool, crate::menu::MenuAction); 3] = [
            (
                "Add Friends to Group",
                false,
                Box::new(move |app| app.group_dm_dialog(Some(channel_id))),
            ),
            (
                "Rename Group",
                false,
                Box::new(move |app| app.rename_group_dialog(channel_id)),
            ),
            (
                "Leave Group",
                true,
                Box::new(move |app| app.confirm_leave_group(channel_id)),
            ),
        ];
        for (text, danger, action) in items {
            let item = crate::menu::menu_item(text, danger);
            let popover = popover.clone();
            let app = self.clone();
            item.connect_clicked(move |_| {
                popover.popdown();
                action(&app);
            });
            menu.append(&item);
        }
    }
}

fn clear_box_flow(flow: &gtk::FlowBox) {
    while let Some(child) = flow.first_child() {
        flow.remove(&child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(range: std::ops::Range<u64>) -> Vec<Snowflake> {
        range.map(Snowflake::new).collect()
    }

    #[test]
    fn one_friend_opens_a_dm_and_more_make_a_group() {
        assert_eq!(submit_plan(None, &[]), Submit::Nothing);
        assert_eq!(
            submit_plan(None, &ids(5..6)),
            Submit::OpenDm(Snowflake::new(5))
        );
        assert_eq!(
            submit_plan(None, &ids(5..7)),
            Submit::CreateGroup(ids(5..7))
        );
        // Adding to a group never turns into a DM.
        assert_eq!(
            submit_plan(Some(Snowflake::new(90)), &ids(5..6)),
            Submit::AddTo(Snowflake::new(90), ids(5..6))
        );
        assert_eq!(submit_label(None, 1), "Create DM");
        assert_eq!(submit_label(None, 2), "Create Group DM");
    }

    #[test]
    fn picks_stop_at_ten_people() {
        // Starting a group: you plus nine friends.
        let mut picked = Vec::new();
        for id in ids(1..10) {
            assert!(toggle_pick(&mut picked, id, true, 1));
        }
        assert_eq!(room_left(1, picked.len()), 0);
        assert!(!toggle_pick(&mut picked, Snowflake::new(50), true, 1));
        assert_eq!(picked.len(), 9);
        // Picking someone twice changes nothing; unpicking frees a place.
        assert!(toggle_pick(&mut picked, Snowflake::new(3), true, 1));
        assert!(toggle_pick(&mut picked, Snowflake::new(3), false, 1));
        assert_eq!(picked.len(), 8);
        assert!(toggle_pick(&mut picked, Snowflake::new(50), true, 1));

        // A group of four (you and three others) has room for six.
        assert_eq!(room_left(4, 0), 6);
        let mut picked = ids(1..7);
        assert!(!toggle_pick(&mut picked, Snowflake::new(60), true, 4));
        assert_eq!(room_left(12, 0), 0);
        assert_eq!(room_text(0), "This group has a 10 member limit.");
        assert_eq!(room_text(1), "You can add 1 more friend.");
    }
}
