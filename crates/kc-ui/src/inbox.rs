//! The inbox: recent messages that mention you, filtered by server, by
//! whether that server notifies you, and by kind of mention.
//!
//! As the web client asks for them: `GET /users/@me/mentions` 25 at a time
//! (`before`, `guild_id`, `roles`, `everyone`); `DELETE
//! /users/@me/mentions/{id}` takes one out. Only fetched when the inbox is
//! opened or its filters change.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use kc_core::Snowflake;
use kc_state::{Message, State};

use crate::shell::AppState;
use crate::video_setup::spawn_local;
use crate::widgets::{clear_box, empty_state, label};

/// At most this many pages are fetched to fill one view when a filter
/// that Discord can't apply (notifications) hides most of a page.
const MAX_PAGES: usize = 4;
/// Enough mentions shown to stop fetching more pages.
const ENOUGH: usize = 10;

/// Which servers' mentions are shown, by whether they notify you.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Notified {
    #[default]
    Any,
    /// Servers that notify you: not muted, not set to Nothing.
    On,
    /// Muted servers, or ones set to Nothing.
    Off,
}

/// The inbox's filters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Filters {
    /// One server, or all of them (and DMs).
    pub guild: Option<Snowflake>,
    pub notified: Notified,
    pub roles: bool,
    pub everyone: bool,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            guild: None,
            notified: Notified::Any,
            roles: true,
            everyone: true,
        }
    }
}

thread_local! {
    /// The filters, kept between openings.
    static FILTERS: RefCell<Filters> = RefCell::new(Filters::default());
}

/// A section's heading in the filter popover.
fn heading(text: &str) -> gtk::Label {
    let heading = label(text, &["caption-heading", "dim-label"]);
    heading.set_xalign(0.0);
    heading
}

/// Whether a server notifies you: not muted, and not set to Nothing (its
/// own default when you haven't chosen).
pub(crate) fn guild_notifies(state: &State, guild_id: Snowflake) -> bool {
    if state.guild_muted(guild_id) {
        return false;
    }
    let level = state
        .guild_settings
        .get(&Some(guild_id))
        .map_or(3, |settings| settings.message_notifications);
    let level = if level == 3 {
        state
            .guilds
            .get(&guild_id)
            .and_then(|guild| guild.default_message_notifications)
            .unwrap_or(0)
    } else {
        level
    };
    level != 2
}

/// Whether a mention passes the filters Discord can't apply.
fn shown(state: &State, filters: &Filters, message: &Message) -> bool {
    let guild = message
        .guild_id
        .or_else(|| state.channels.get(&message.channel_id).and_then(|channel| channel.guild_id));
    match (filters.notified, guild) {
        (Notified::Any, _) => true,
        // DMs always notify.
        (Notified::On, None) => true,
        (Notified::Off, None) => false,
        (Notified::On, Some(guild)) => guild_notifies(state, guild),
        (Notified::Off, Some(guild)) => !guild_notifies(state, guild),
    }
}

/// The inbox while open.
struct Inbox {
    list: gtk::Box,
    more: gtk::Button,
    popover: gtk::Popover,
    /// What has been fetched for the filters in force, newest first.
    fetched: RefCell<Vec<Message>>,
    /// Discord has no older ones.
    done: RefCell<bool>,
    /// Bumped when the filters change, so a late answer is dropped.
    generation: RefCell<u64>,
}

impl AppState {
    /// Opens the inbox under its header button.
    pub(crate) fn show_inbox(self: &Rc<Self>) {
        let popover = gtk::Popover::new();
        popover.set_parent(&self.inbox_button);
        popover.set_position(gtk::PositionType::Bottom);
        popover.connect_closed(|popover| popover.unparent());
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_size_request(460, 560);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let title = label("Mentions", &["heading"]);
        title.set_xalign(0.0);
        title.set_hexpand(true);
        header.append(&title);
        let filter = gtk::MenuButton::new();
        filter.set_icon_name(crate::widgets::first_icon(&["funnel-symbolic", "filter-symbolic", "view-more-symbolic"]));
        filter.set_tooltip_text(Some("Filter"));
        filter.add_css_class("flat");
        header.append(&filter);
        content.append(&header);
        let summary = label("", &["caption", "dim-label"]);
        summary.set_xalign(0.0);
        content.append(&summary);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let more = gtk::Button::with_label("Load More");
        more.add_css_class("flat");
        more.set_visible(false);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
        column.append(&list);
        column.append(&more);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&column));
        content.append(&scroll);
        popover.set_child(Some(&content));

        let inbox = Rc::new(Inbox {
            list,
            more: more.clone(),
            popover: popover.clone(),
            fetched: RefCell::default(),
            done: RefCell::new(false),
            generation: RefCell::new(0),
        });
        let app = self.clone();
        let target = inbox.clone();
        more.connect_clicked(move |_| app.load_mentions(&target, false));

        // The filters, in a popover of their own.
        let refresh: Rc<dyn Fn()> = {
            let (app, inbox, summary) = (self.clone(), inbox.clone(), summary.clone());
            Rc::new(move || {
                summary.set_text(&app.inbox_summary());
                app.load_mentions(&inbox, true);
            })
        };
        filter.set_popover(Some(&self.inbox_filters(refresh.clone())));
        refresh();

        if std::env::var_os("KUBECORD_DEMO").is_some() {
            popover.set_autohide(false);
        }
        popover.popup();
    }

    /// "All servers · notified only · no @everyone".
    fn inbox_summary(&self) -> String {
        let filters = FILTERS.with_borrow(Clone::clone);
        let mut parts = vec![match filters.guild {
            None => "All servers".to_owned(),
            Some(guild) => self
                .with_session(|session| session.store.read(|state| state.guilds.get(&guild).map(|guild| guild.name.clone())))
                .flatten()
                .unwrap_or_else(|| "One server".to_owned()),
        }];
        match filters.notified {
            Notified::Any => {}
            Notified::On => parts.push("servers that notify you".to_owned()),
            Notified::Off => parts.push("muted or silenced servers".to_owned()),
        }
        if !filters.everyone {
            parts.push("no @everyone".to_owned());
        }
        if !filters.roles {
            parts.push("no role mentions".to_owned());
        }
        parts.join(" · ")
    }

    /// The filter popover; `changed` runs after each change.
    fn inbox_filters(self: &Rc<Self>, changed: Rc<dyn Fn()>) -> gtk::Popover {
        let filters = FILTERS.with_borrow(Clone::clone);
        let popover = gtk::Popover::new();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(6);
        content.set_margin_end(6);
        content.set_width_request(260);

        content.append(&heading("Server"));
        let guilds: Vec<(Snowflake, String)> = self
            .with_session(|session| {
                session
                    .store
                    .read(|state| state.ordered_guilds().into_iter().map(|guild| (guild.id, guild.name.clone())).collect())
            })
            .unwrap_or_default();
        let mut names = vec!["All Servers".to_owned()];
        names.extend(guilds.iter().map(|(_, name)| name.clone()));
        let open_guild = self
            .with_session(|session| {
                let channel = session.selected_channel?;
                session.store.read(|state| state.channels.get(&channel).and_then(|channel| channel.guild_id))
            })
            .flatten();
        let server = gtk::DropDown::from_strings(&names.iter().map(String::as_str).collect::<Vec<_>>());
        server.set_enable_search(true);
        let selected = filters
            .guild
            .and_then(|guild| guilds.iter().position(|(id, _)| *id == guild))
            .map_or(0, |index| index + 1);
        server.set_selected(selected as u32);
        content.append(&server);
        if let Some(open) = open_guild {
            let this = gtk::Button::with_label("This Server");
            this.add_css_class("flat");
            let (server, guilds) = (server.clone(), guilds.clone());
            this.connect_clicked(move |_| {
                if let Some(index) = guilds.iter().position(|(id, _)| *id == open) {
                    server.set_selected(index as u32 + 1);
                }
            });
            content.append(&this);
        }
        {
            let changed = changed.clone();
            server.connect_selected_notify(move |server| {
                let index = server.selected() as usize;
                let guild = index.checked_sub(1).and_then(|index| guilds.get(index)).map(|(id, _)| *id);
                FILTERS.with_borrow_mut(|filters| filters.guild = guild);
                changed();
            });
        }

        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&heading("Notifications"));
        let any = gtk::CheckButton::with_label("Every server");
        let on = gtk::CheckButton::with_label("Servers that notify me");
        let off = gtk::CheckButton::with_label("Muted or silenced servers");
        on.set_group(Some(&any));
        off.set_group(Some(&any));
        match filters.notified {
            Notified::Any => any.set_active(true),
            Notified::On => on.set_active(true),
            Notified::Off => off.set_active(true),
        }
        for (button, value) in [(&any, Notified::Any), (&on, Notified::On), (&off, Notified::Off)] {
            let changed = changed.clone();
            button.connect_toggled(move |button| {
                if button.is_active() {
                    FILTERS.with_borrow_mut(|filters| filters.notified = value);
                    changed();
                }
            });
            content.append(button);
        }

        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&heading("Include"));
        let everyone = gtk::CheckButton::with_label("@everyone and @here");
        everyone.set_active(filters.everyone);
        let roles = gtk::CheckButton::with_label("Role mentions");
        roles.set_active(filters.roles);
        {
            let changed = changed.clone();
            everyone.connect_toggled(move |button| {
                FILTERS.with_borrow_mut(|filters| filters.everyone = button.is_active());
                changed();
            });
        }
        roles.connect_toggled(move |button| {
            FILTERS.with_borrow_mut(|filters| filters.roles = button.is_active());
            changed();
        });
        content.append(&everyone);
        content.append(&roles);
        popover.set_child(Some(&content));
        popover
    }

    /// Fetches mentions: afresh (`reset`, the filters changed), or the page
    /// after those shown. Keeps fetching (a few pages at most) while the
    /// notification filter hides most of what comes.
    fn load_mentions(self: &Rc<Self>, inbox: &Rc<Inbox>, reset: bool) {
        if reset {
            *inbox.generation.borrow_mut() += 1;
            inbox.fetched.borrow_mut().clear();
            *inbox.done.borrow_mut() = false;
            clear_box(&inbox.list);
            let loading = label("Loading…", &["dim-label"]);
            loading.set_margin_top(24);
            inbox.list.append(&loading);
        }
        self.fetch_mention_pages(inbox, *inbox.generation.borrow(), MAX_PAGES);
    }

    fn fetch_mention_pages(self: &Rc<Self>, inbox: &Rc<Inbox>, generation: u64, pages_left: usize) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let filters = FILTERS.with_borrow(Clone::clone);
        let before = inbox.fetched.borrow().last().map(|message| message.id);
        if let Some(mentions) = crate::demo::fake_mentions() {
            *inbox.done.borrow_mut() = true;
            *inbox.fetched.borrow_mut() = mentions;
            self.render_inbox(inbox);
            return;
        }
        inbox.more.set_sensitive(false);
        let inbox = inbox.clone();
        spawn_local(
            self,
            async move { http.mentions(before, filters.guild, filters.roles, filters.everyone).await },
            move |app, result| {
                if *inbox.generation.borrow() != generation {
                    return;
                }
                inbox.more.set_sensitive(true);
                match result {
                    Ok(page) => {
                        *inbox.done.borrow_mut() = page.len() < 25;
                        inbox.fetched.borrow_mut().extend(page);
                        let visible = app.render_inbox(&inbox);
                        if visible < ENOUGH && !*inbox.done.borrow() && pages_left > 1 {
                            app.fetch_mention_pages(&inbox, generation, pages_left - 1);
                        }
                    }
                    Err(error) => {
                        clear_box(&inbox.list);
                        let failed = label(&format!("Could not load mentions: {error}"), &["dim-label"]);
                        failed.set_wrap(true);
                        inbox.list.append(&failed);
                    }
                }
            },
        );
    }

    /// Draws what was fetched that passes the filters; returns how many.
    fn render_inbox(self: &Rc<Self>, inbox: &Rc<Inbox>) -> usize {
        clear_box(&inbox.list);
        let filters = FILTERS.with_borrow(Clone::clone);
        let Some((_, _, store)) = self.active() else {
            return 0;
        };
        let mut count = 0;
        store.read(|state| {
            for message in inbox.fetched.borrow().iter().filter(|message| shown(state, &filters, message)) {
                count += 1;
                inbox.list.append(&self.mention_card(inbox, message, state));
            }
        });
        if count == 0 {
            inbox.list.append(&empty_state(
                "mail-inbox-symbolic",
                if *inbox.done.borrow() { "No Mentions" } else { "None Here Yet" },
                if *inbox.done.borrow() { "Nothing mentions you with these filters." } else { "Load more to look further back." },
            ));
        }
        inbox.more.set_visible(!*inbox.done.borrow());
        count
    }

    fn mention_card(self: &Rc<Self>, inbox: &Rc<Inbox>, message: &Message, state: &State) -> gtk::Widget {
        let guild = message
            .guild_id
            .or_else(|| state.channels.get(&message.channel_id).and_then(|channel| channel.guild_id));
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("card");
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        top.set_margin_top(6);
        top.set_margin_start(10);
        top.set_margin_end(6);
        let place = match (state.channels.get(&message.channel_id), guild.and_then(|guild| state.guilds.get(&guild))) {
            (Some(channel), Some(guild)) => format!("#{} · {}", state.channel_title(channel), guild.name),
            (Some(channel), None) => state.channel_title(channel),
            _ => "Unknown channel".to_owned(),
        };
        let heading = label(&place, &["caption-heading"]);
        heading.set_xalign(0.0);
        heading.set_hexpand(true);
        heading.set_ellipsize(gtk::pango::EllipsizeMode::End);
        top.append(&heading);
        let jump = gtk::Button::with_label("Jump");
        jump.add_css_class("flat");
        let (app, popover, channel, id) = (self.clone(), inbox.popover.clone(), message.channel_id, message.id);
        jump.connect_clicked(move |_| {
            popover.popdown();
            app.jump_to_result(channel, id);
        });
        top.append(&jump);
        let remove = gtk::Button::from_icon_name("window-close-symbolic");
        remove.add_css_class("flat");
        remove.set_tooltip_text(Some("Remove from Inbox"));
        let (app, target) = (self.clone(), inbox.clone());
        remove.connect_clicked(move |_| app.remove_mention(&target, id));
        top.append(&remove);
        card.append(&top);

        let body = self.message_body(message, true, guild, state);
        body.set_margin_top(4);
        body.set_margin_bottom(8);
        body.set_margin_start(8);
        body.set_margin_end(8);
        card.append(&body);
        card.upcast()
    }

    fn remove_mention(self: &Rc<Self>, inbox: &Rc<Inbox>, message_id: Snowflake) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        inbox.fetched.borrow_mut().retain(|message| message.id != message_id);
        self.render_inbox(inbox);
        spawn_local(self, async move { http.delete_mention(message_id).await }, |app, result| {
            if let Err(error) = result {
                app.set_status(&format!("Could not remove the mention: {error}"));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_notification_filter_follows_mutes_and_levels() {
        let store = kc_state::Store::new();
        store.apply("READY", &json!({"session_id": "s", "user": {"id": "1", "username": "me"}, "users": [],
            "guilds": [
                {"id": "10", "name": "Loud", "default_message_notifications": 0},
                {"id": "20", "name": "Muted"},
                {"id": "30", "name": "Nothing"},
                {"id": "40", "name": "Quiet by default", "default_message_notifications": 1}
            ],
            "private_channels": [], "relationships": [], "read_state": {"entries": []},
            "user_guild_settings": {"entries": [
                {"guild_id": "20", "muted": true, "message_notifications": 0},
                {"guild_id": "30", "muted": false, "message_notifications": 2}
            ]}}));
        store.read(|state| {
            assert!(guild_notifies(state, Snowflake(10)));
            assert!(!guild_notifies(state, Snowflake(20)));
            assert!(!guild_notifies(state, Snowflake(30)));
            // Only mentions still notifies of a mention.
            assert!(guild_notifies(state, Snowflake(40)));
            let mention = |guild: Option<&str>| -> Message {
                serde_json::from_value(json!({"id": "5", "channel_id": "6", "guild_id": guild,
                    "author": {"id": "2", "username": "a"}, "content": "hi"}))
                .unwrap()
            };
            let on = Filters { notified: Notified::On, ..Filters::default() };
            let off = Filters { notified: Notified::Off, ..Filters::default() };
            assert!(shown(state, &on, &mention(Some("10"))));
            assert!(!shown(state, &on, &mention(Some("20"))));
            assert!(shown(state, &off, &mention(Some("30"))));
            assert!(shown(state, &on, &mention(None)), "DMs notify");
            assert!(!shown(state, &off, &mention(None)));
            assert!(shown(state, &Filters::default(), &mention(Some("20"))));
        });
    }
}
