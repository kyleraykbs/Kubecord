//! Message search: a results panel beside the chat, with the web client's
//! filters, tabs and orders.
//!
//! The search box takes words and filters typed the web client's way
//! (`from:ada has:image before:2026-01-01 cats`); the Filters dialog edits
//! the same filters with fields, and the two stay in step: the dialog writes
//! the box's text, and one parser reads it. In a server the whole server is
//! searched, as on the web; `in:` narrows it to channels.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::Snowflake;
use kc_http::search::{AuthorType, Has, Scope, SearchParams, Sort, Tab};
use kc_state::{ChannelType, Message};

use crate::shell::AppState;
use crate::widgets::{clear_box, empty_state, first_icon, icon_button, label};

/// The filters the box understands, by the name typed before the colon.
const KEYS: [(&str, Key); 17] = [
    ("from", Key::From),
    ("mentions", Key::Mentions),
    ("mention", Key::Mentions),
    ("has", Key::Has),
    ("in", Key::In),
    ("pinned", Key::Pinned),
    ("before", Key::Before),
    ("after", Key::After),
    ("during", Key::During),
    ("on", Key::During),
    ("authortype", Key::AuthorType),
    ("author", Key::AuthorType),
    ("linkfrom", Key::LinkFrom),
    ("link", Key::LinkFrom),
    ("filetype", Key::FileType),
    ("filename", Key::FileName),
    ("file", Key::FileName),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    From,
    Mentions,
    Has,
    In,
    Pinned,
    Before,
    After,
    During,
    AuthorType,
    LinkFrom,
    FileType,
    FileName,
}

impl Key {
    /// The name written back into the box.
    const fn name(self) -> &'static str {
        match self {
            Key::From => "from",
            Key::Mentions => "mentions",
            Key::Has => "has",
            Key::In => "in",
            Key::Pinned => "pinned",
            Key::Before => "before",
            Key::After => "after",
            Key::During => "during",
            Key::AuthorType => "authorType",
            Key::LinkFrom => "linkFrom",
            Key::FileType => "fileType",
            Key::FileName => "fileName",
        }
    }
}

/// Splits what was typed into its words and its `key:value` filters. Values
/// (and words) may be quoted to hold spaces. An unknown `key:` is a word.
pub(crate) fn parse_query(text: &str) -> (String, Vec<(Key, String)>) {
    let mut words: Vec<String> = Vec::new();
    let mut filters = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        // One token: quoted parts keep their spaces.
        let mut token = String::new();
        let mut quoted_value = false;
        let mut in_quotes = false;
        while let Some(&c) = chars.peek() {
            if c == '"' {
                in_quotes = !in_quotes;
                chars.next();
                if token.ends_with(':') || token.is_empty() {
                    quoted_value = true;
                }
                continue;
            }
            if c.is_whitespace() && !in_quotes {
                break;
            }
            token.push(c);
            chars.next();
        }
        let filter = token.split_once(':').and_then(|(key, value)| {
            let key = key.to_ascii_lowercase().replace('_', "");
            let key = KEYS.iter().find(|(name, _)| *name == key)?.1;
            (!value.trim().is_empty()).then(|| (key, value.trim().to_owned()))
        });
        match filter {
            Some(filter) => filters.push(filter),
            None if quoted_value && token.contains(' ') => words.push(format!("\"{token}\"")),
            None => words.push(token),
        }
    }
    (words.join(" "), filters)
}

/// A value written into the box, quoted when it holds a space.
fn quote(value: &str) -> String {
    if value.contains(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_owned()
    }
}

/// A day typed as `2026-01-31` (or with slashes), `today` or `yesterday`.
pub(crate) fn parse_day(text: &str) -> Option<glib::DateTime> {
    let text = text.trim().to_ascii_lowercase();
    let today = glib::DateTime::now_local().ok()?;
    let midnight = |date: &glib::DateTime| {
        glib::DateTime::from_local(date.year(), date.month(), date.day_of_month(), 0, 0, 0.0).ok()
    };
    match text.as_str() {
        "today" => return midnight(&today),
        "yesterday" => return midnight(&today.add_days(-1).ok()?),
        _ => {}
    }
    let parts: Vec<&str> = text.split(['-', '/', '.']).collect();
    let [year, month, day] = parts.as_slice() else {
        return None;
    };
    glib::DateTime::from_local(year.parse().ok()?, month.parse().ok()?, day.parse().ok()?, 0, 0, 0.0).ok()
}

/// The snowflake for a moment (the lowest id a message then could have).
fn snowflake_at(time: &glib::DateTime) -> Snowflake {
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let ms = time.to_unix() * 1000;
    Snowflake(((ms - DISCORD_EPOCH_MS).max(0) as u64) << 22)
}

/// The search as chosen: what was typed, resolved (names to ids), with the
/// names kept for the chips.
#[derive(Clone, Debug, Default)]
pub(crate) struct Filters {
    pub content: String,
    /// Each filter as typed (for the box and the chips) and as sent.
    pub items: Vec<(Key, String)>,
    pub params: SearchParams,
}

impl Filters {
    /// The box's text for these filters.
    pub(crate) fn text(&self) -> String {
        let mut parts: Vec<String> = self
            .items
            .iter()
            .map(|(key, value)| format!("{}:{}", key.name(), quote(value)))
            .collect();
        if !self.content.trim().is_empty() {
            parts.push(self.content.trim().to_owned());
        }
        parts.join(" ")
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.params.is_empty()
    }
}

/// Where a search runs, and the open search panel's state.
struct Run {
    scope: Scope,
    guild: Option<Snowflake>,
    filters: Filters,
    tab: Tab,
    sort: Sort,
    /// Results loaded so far and the total Discord reported.
    offset: u32,
    total: u64,
    loading: bool,
    /// Bumped by each new search, so a late page of an old one is dropped.
    generation: u64,
    /// Retries while Discord is still indexing.
    retries: u32,
}

/// The results panel, built when first needed.
pub(crate) struct SearchPanel {
    root: gtk::Box,
    title: gtk::Label,
    tabs: Vec<(Tab, gtk::ToggleButton)>,
    sort: gtk::DropDown,
    chips: gtk::FlowBox,
    list: gtk::Box,
    scroll: gtk::ScrolledWindow,
    footer: gtk::Label,
    run: RefCell<Run>,
    /// Set while the tab and sort widgets are being set from code.
    quiet: Cell<bool>,
}

impl AppState {
    /// Whether the search panel is showing.
    pub(crate) fn search_open(&self) -> bool {
        self.search
            .borrow()
            .as_ref()
            .is_some_and(|_| self.sidebar_stack.visible_child_name().as_deref() == Some("search"))
    }

    /// Shows or hides the member list, unless search results hold the side.
    pub(crate) fn show_member_side(&self, show: bool) {
        if self.search_open() {
            self.split.set_show_sidebar(true);
        } else {
            self.split.set_show_sidebar(show);
        }
    }

    /// The scope a search from the open channel covers: its server, or the
    /// DM itself.
    fn search_scope(&self) -> Option<(Scope, Option<Snowflake>)> {
        self.with_session(|session| {
            let channel_id = session.selected_channel?;
            let guild = session.store.read(|state| state.channels.get(&channel_id).and_then(|c| c.guild_id));
            Some(match guild {
                Some(guild) => (Scope::Guild(guild), Some(guild)),
                None => (Scope::Channel(channel_id), None),
            })
        })
        .flatten()
    }

    /// Reads the search box's text into filters, naming what could not be
    /// understood.
    pub(crate) fn resolve_search(&self, text: &str, guild: Option<Snowflake>) -> (Filters, Vec<String>) {
        let (content, typed) = parse_query(text);
        let mut filters = Filters {
            content: content.clone(),
            params: SearchParams {
                content,
                ..SearchParams::default()
            },
            ..Filters::default()
        };
        let mut problems = Vec::new();
        let channel = self.with_session(|session| session.selected_channel).flatten();
        self.with_session(|session| {
            session.store.read(|state| {
                let find_user = |name: &str| -> Option<Snowflake> {
                    let name = name.trim().trim_start_matches('@');
                    if let Ok(id) = name.parse::<u64>()
                        && id > 1 << 32
                    {
                        return Some(Snowflake(id));
                    }
                    let lower = name.to_lowercase();
                    let mut candidates: Vec<kc_state::User> = match guild {
                        Some(guild) => state.guild_member_users(guild),
                        None => Vec::new(),
                    };
                    if let Some(channel) = channel.and_then(|id| state.channels.get(&id)) {
                        candidates.extend(channel.recipients.iter().flatten().cloned());
                    }
                    candidates.extend(state.me.clone());
                    candidates.extend(state.users.values().cloned());
                    candidates
                        .iter()
                        .find(|user| {
                            user.username.to_lowercase() == lower
                                || user.global_name.as_deref().is_some_and(|n| n.to_lowercase() == lower)
                                || state.display_name(guild, user).to_lowercase() == lower
                        })
                        .map(|user| user.id)
                };
                for (key, value) in typed {
                    let mut ok = true;
                    match key {
                        Key::From | Key::Mentions => match find_user(&value) {
                            Some(id) if key == Key::From => filters.params.authors.push(id),
                            Some(id) => filters.params.mentions.push(id),
                            None => {
                                problems.push(format!("No one called “{value}” here"));
                                ok = false;
                            }
                        },
                        Key::Has => match Has::named(&value) {
                            Some(has) => filters.params.has.push(has),
                            None => {
                                problems.push(format!("“has:{value}” isn't a kind of content"));
                                ok = false;
                            }
                        },
                        Key::AuthorType => match AuthorType::named(&value) {
                            Some(kind) => filters.params.author_types.push(kind),
                            None => {
                                problems.push(format!("“{value}” isn't user, bot or webhook"));
                                ok = false;
                            }
                        },
                        Key::Pinned => match value.to_ascii_lowercase().as_str() {
                            "true" | "yes" | "1" => filters.params.pinned = Some(true),
                            "false" | "no" | "0" => filters.params.pinned = Some(false),
                            _ => {
                                problems.push(format!("“pinned:{value}” is true or false"));
                                ok = false;
                            }
                        },
                        Key::In => {
                            let name = value.trim_start_matches('#').to_lowercase();
                            let found = guild.and_then(|guild| {
                                state
                                    .channels
                                    .values()
                                    .filter(|channel| channel.guild_id == Some(guild))
                                    .find(|channel| {
                                        channel.id.to_string() == name
                                            || channel.name.as_deref().is_some_and(|n| n.to_lowercase() == name)
                                    })
                                    .map(|channel| channel.id)
                            });
                            match found {
                                Some(id) => filters.params.channels.push(id),
                                None => {
                                    problems.push(format!("No channel called “{value}” here"));
                                    ok = false;
                                }
                            }
                        }
                        Key::Before | Key::After | Key::During => match parse_day(&value) {
                            Some(day) => {
                                let next = day.add_days(1).ok();
                                match key {
                                    Key::Before => filters.params.max_id = Some(snowflake_at(&day)),
                                    Key::After => filters.params.min_id = next.as_ref().map(snowflake_at),
                                    _ => {
                                        filters.params.min_id = Some(snowflake_at(&day));
                                        filters.params.max_id = next.as_ref().map(snowflake_at);
                                    }
                                }
                            }
                            None => {
                                problems.push(format!("“{value}” isn't a date like 2026-01-31"));
                                ok = false;
                            }
                        },
                        Key::LinkFrom => filters.params.link_hostnames.push(value.trim().to_lowercase()),
                        Key::FileType => filters
                            .params
                            .extensions
                            .push(value.trim().trim_start_matches('.').to_lowercase()),
                        Key::FileName => filters.params.filenames.push(value.trim().to_owned()),
                    }
                    if ok {
                        filters.items.push((key, value));
                    }
                }
            })
        });
        (filters, problems)
    }

    /// Runs what is in the search box.
    pub(crate) fn search_from_entry(self: &Rc<Self>) {
        let text = self.search_entry.text().to_string();
        let Some((scope, guild)) = self.search_scope() else {
            return;
        };
        let (filters, problems) = self.resolve_search(&text, guild);
        if !problems.is_empty() {
            self.set_status(&problems.join("; "));
        }
        if filters.is_empty() {
            return;
        }
        self.start_search(scope, guild, filters);
    }

    /// Opens the panel on a new search.
    fn start_search(self: &Rc<Self>, scope: Scope, guild: Option<Snowflake>, filters: Filters) {
        let panel = self.search_panel();
        {
            let mut run = panel.run.borrow_mut();
            run.scope = scope;
            run.guild = guild;
            run.filters = filters;
            run.tab = Tab::Messages;
        }
        panel.quiet.set(true);
        for (tab, button) in &panel.tabs {
            button.set_active(*tab == Tab::Messages);
        }
        panel.quiet.set(false);
        self.sidebar_stack.set_visible_child_name("search");
        self.split.set_min_sidebar_width(340.0);
        self.split.set_max_sidebar_width(460.0);
        self.split.set_sidebar_width_fraction(0.32);
        self.split.set_show_sidebar(true);
        self.rerun_search();
    }

    /// The demo: `text` searched, answered with `messages` (no network).
    pub(crate) fn demo_search(self: &Rc<Self>, text: &str, messages: Vec<Message>) {
        let Some((scope, guild)) = self.search_scope() else {
            return;
        };
        let (filters, problems) = self.resolve_search(text, guild);
        eprintln!("demo: search {:?} -> {} problems {problems:?}", filters.text(), filters.params.path(scope, Tab::Messages, Sort::Newest, 0, false));
        self.search_entry.set_text(&filters.text());
        let panel = self.search_panel();
        {
            let mut run = panel.run.borrow_mut();
            run.scope = scope;
            run.guild = guild;
            run.filters = filters;
            run.total = messages.len() as u64;
            run.offset = messages.len() as u32;
        }
        self.sidebar_stack.set_visible_child_name("search");
        self.split.set_min_sidebar_width(340.0);
        self.split.set_max_sidebar_width(460.0);
        self.split.set_sidebar_width_fraction(0.32);
        self.split.set_show_sidebar(true);
        self.render_chips(&panel);
        panel.title.set_label(&format!("{} Results", messages.len()));
        self.append_results(&panel, messages);
        panel.footer.set_label("That's everything.");
    }

    /// Closes the panel; the member list comes back.
    pub(crate) fn close_search(self: &Rc<Self>) {
        if let Some(panel) = self.search.borrow().as_ref() {
            panel.run.borrow_mut().generation += 1;
        }
        self.sidebar_stack.set_visible_child_name("members");
        self.split.set_min_sidebar_width(220.0);
        self.split.set_max_sidebar_width(260.0);
        self.split.set_sidebar_width_fraction(0.25);
        self.render_members();
    }

    /// Searches again from the first page (new filters, tab or order).
    fn rerun_search(self: &Rc<Self>) {
        let panel = self.search_panel();
        {
            let mut run = panel.run.borrow_mut();
            run.offset = 0;
            run.total = 0;
            run.retries = 0;
            run.generation += 1;
        }
        clear_box(&panel.list);
        panel.scroll.vadjustment().set_value(0.0);
        self.render_chips(&panel);
        if panel.run.borrow().filters.is_empty() {
            panel.title.set_label("Search");
            panel.footer.set_label("");
            panel.list.append(&empty_state("edit-find-symbolic", "Nothing to Search For", "Type in the search box, or add filters."));
            return;
        }
        panel.title.set_label("Searching…");
        self.fetch_search_page(&panel);
    }

    /// Fetches the page at the current offset.
    fn fetch_search_page(self: &Rc<Self>, panel: &Rc<SearchPanel>) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let (scope, params, tab, sort, offset, generation) = {
            let mut run = panel.run.borrow_mut();
            run.loading = true;
            (run.scope, run.filters.params.clone(), run.tab, run.sort, run.offset, run.generation)
        };
        panel.footer.set_label(if offset == 0 { "" } else { "Loading more…" });
        let panel = panel.clone();
        crate::video_setup::spawn_local(
            self,
            async move { http.search(scope, &params, tab, sort, offset).await },
            move |app, result| {
                if panel.run.borrow().generation != generation {
                    return;
                }
                panel.run.borrow_mut().loading = false;
                match result {
                    Ok(page) if page.indexing => {
                        // Discord indexes a channel on its first search; ask
                        // again shortly.
                        let retries = {
                            let mut run = panel.run.borrow_mut();
                            run.retries += 1;
                            run.retries
                        };
                        if retries > 6 {
                            panel.title.set_label("Still Indexing");
                            panel.footer.set_label("Discord is still indexing these messages. Try again in a minute.");
                            return;
                        }
                        panel.title.set_label("Indexing…");
                        panel.footer.set_label("Discord is indexing these messages for the first time.");
                        let app = app.clone();
                        glib::timeout_add_local_once(Duration::from_secs(3), move || {
                            if panel.run.borrow().generation == generation {
                                app.fetch_search_page(&panel);
                            }
                        });
                    }
                    Ok(page) => {
                        let (offset, total) = {
                            let mut run = panel.run.borrow_mut();
                            run.total = page.total;
                            run.offset += page.messages.len() as u32;
                            (run.offset, run.total)
                        };
                        panel.title.set_label(&match total {
                            0 => "No Results".to_owned(),
                            1 => "1 Result".to_owned(),
                            total => format!("{total} Results"),
                        });
                        if total == 0 {
                            panel.list.append(&empty_state(
                                "edit-find-symbolic",
                                "No Results",
                                "Nothing matches. Try fewer filters or other words.",
                            ));
                        }
                        app.append_results(&panel, page.messages);
                        let more = u64::from(offset) < total && offset > 0;
                        panel.footer.set_label(if more { "" } else if total > 0 { "That's everything." } else { "" });
                    }
                    Err(error) => {
                        panel.title.set_label("Search Failed");
                        panel.footer.set_label(&error.to_string());
                    }
                }
            },
        );
    }

    /// Loads the next page when the results are scrolled to their end.
    fn search_load_more(self: &Rc<Self>) {
        let Some(panel) = self.search.borrow().clone() else {
            return;
        };
        let more = {
            let run = panel.run.borrow();
            !run.loading && run.offset > 0 && u64::from(run.offset) < run.total
        };
        if more {
            self.fetch_search_page(&panel);
        }
    }

    /// A card for each result: where it was said, the message, and a click
    /// jumps to it.
    fn append_results(self: &Rc<Self>, panel: &Rc<SearchPanel>, messages: Vec<Message>) {
        let Some((_, _, store)) = self.active() else {
            return;
        };
        let show_channel = matches!(panel.run.borrow().scope, Scope::Guild(_));
        store.read(|state| {
            for message in &messages {
                let guild = state.channels.get(&message.channel_id).and_then(|channel| channel.guild_id);
                let card = gtk::Box::new(gtk::Orientation::Vertical, 2);
                card.add_css_class("kc-search-result");
                if show_channel {
                    let name = state
                        .channels
                        .get(&message.channel_id)
                        .map(|channel| match channel.kind {
                            kind if kind.is_thread() => format!("↳ {}", channel.display_name()),
                            ChannelType::GuildVoice | ChannelType::GuildStageVoice => {
                                format!("🔊 {}", channel.display_name())
                            }
                            _ => format!("# {}", channel.display_name()),
                        })
                        .unwrap_or_else(|| "# unknown channel".to_owned());
                    card.append(&label(&name, &["caption", "dim-label", "kc-search-channel"]));
                }
                card.append(&self.message_body(message, true, guild, state));
                let click = gtk::GestureClick::new();
                let app = self.clone();
                let (channel, id) = (message.channel_id, message.id);
                click.connect_released(move |gesture, _, _, _| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    app.jump_to_result(channel, id);
                });
                card.add_controller(click);
                card.set_cursor_from_name(Some("pointer"));
                panel.list.append(&card);
            }
        });
    }

    /// Opens a result's channel (when another) and jumps to it.
    fn jump_to_result(self: &Rc<Self>, channel_id: Snowflake, message_id: Snowflake) {
        let open = self.with_session(|session| session.selected_channel).flatten();
        if open == Some(channel_id) {
            self.jump_or_fetch(channel_id, message_id);
            return;
        }
        let Some((_, _, store)) = self.active() else {
            return;
        };
        let Some(channel) = store.read(|state| state.channels.get(&channel_id).cloned()) else {
            self.set_status("That channel isn't available.");
            return;
        };
        self.open_channel(channel);
        self.jump_when_ready(channel_id, message_id);
    }

    /// The chips for the filters in force; each removes its filter.
    fn render_chips(self: &Rc<Self>, panel: &Rc<SearchPanel>) {
        while let Some(child) = panel.chips.first_child() {
            panel.chips.remove(&child);
        }
        let filters = panel.run.borrow().filters.clone();
        let mut chips: Vec<(String, Option<usize>)> = filters
            .items
            .iter()
            .enumerate()
            .map(|(index, (key, value))| (format!("{}: {value}", key.name()), Some(index)))
            .collect();
        if !filters.content.trim().is_empty() {
            chips.push((format!("“{}”", filters.content.trim()), None));
        }
        for (text, index) in chips {
            let chip = gtk::Box::new(gtk::Orientation::Horizontal, 2);
            chip.add_css_class("kc-search-chip");
            let text_label = label(&text, &["caption"]);
            text_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            text_label.set_max_width_chars(28);
            chip.append(&text_label);
            let remove = icon_button("window-close-symbolic", "Remove");
            remove.add_css_class("flat");
            remove.add_css_class("circular");
            let app = self.clone();
            remove.connect_clicked(move |_| app.remove_search_filter(index));
            chip.append(&remove);
            panel.chips.append(&chip);
        }
        panel.chips.set_visible(panel.chips.first_child().is_some());
    }

    /// Removes one filter (`None`: the words) and searches again.
    fn remove_search_filter(self: &Rc<Self>, index: Option<usize>) {
        let Some(panel) = self.search.borrow().clone() else {
            return;
        };
        let (mut filters, guild) = {
            let run = panel.run.borrow();
            (run.filters.clone(), run.guild)
        };
        match index {
            Some(index) if index < filters.items.len() => {
                filters.items.remove(index);
            }
            Some(_) => return,
            None => filters.content.clear(),
        }
        self.apply_search_text(&filters.text(), guild);
    }

    /// Writes `text` into the box and searches it (from the chips and the
    /// Filters dialog, so the box always shows the search).
    fn apply_search_text(self: &Rc<Self>, text: &str, guild: Option<Snowflake>) -> Vec<String> {
        let (filters, problems) = self.resolve_search(text, guild);
        if !problems.is_empty() {
            return problems;
        }
        self.search_entry.set_text(&filters.text());
        let panel = self.search_panel();
        panel.run.borrow_mut().filters = filters;
        self.rerun_search();
        Vec::new()
    }

    /// The panel, built on first use.
    fn search_panel(self: &Rc<Self>) -> Rc<SearchPanel> {
        if let Some(panel) = self.search.borrow().as_ref() {
            return panel.clone();
        }
        let panel = Rc::new(build_panel());
        self.sidebar_stack.add_named(&panel.root, Some("search"));
        self.connect_panel(&panel);
        *self.search.borrow_mut() = Some(panel.clone());
        panel
    }

    fn connect_panel(self: &Rc<Self>, panel: &Rc<SearchPanel>) {
        for (tab, button) in &panel.tabs {
            let app = self.clone();
            let weak = Rc::downgrade(panel);
            let tab = *tab;
            button.connect_toggled(move |button| {
                let Some(panel) = weak.upgrade() else { return };
                if panel.quiet.get() || !button.is_active() {
                    return;
                }
                panel.run.borrow_mut().tab = tab;
                app.rerun_search();
            });
        }
        let app = self.clone();
        let weak = Rc::downgrade(panel);
        panel.sort.connect_selected_notify(move |dropdown| {
            let Some(panel) = weak.upgrade() else { return };
            if panel.quiet.get() {
                return;
            }
            panel.run.borrow_mut().sort = Sort::ALL[(dropdown.selected() as usize).min(Sort::ALL.len() - 1)];
            app.rerun_search();
        });
        let app = self.clone();
        panel.scroll.connect_edge_reached(move |_, edge| {
            if edge == gtk::PositionType::Bottom {
                app.search_load_more();
            }
        });
        // The header's buttons.
        if let Some(header) = panel.root.first_child().and_downcast::<gtk::Box>() {
            let mut child = header.first_child();
            while let Some(widget) = child {
                if let Some(button) = widget.downcast_ref::<gtk::Button>() {
                    let app = self.clone();
                    match button.widget_name().as_str() {
                        "close" => {
                            button.connect_clicked(move |_| app.close_search());
                        }
                        "filters" => {
                            button.connect_clicked(move |_| app.show_search_filters());
                        }
                        _ => {}
                    }
                }
                child = widget.next_sibling();
            }
        }
    }

    /// Opens the Filters dialog (from the panel or the search box's menu).
    pub(crate) fn show_search_filters(self: &Rc<Self>) {
        let Some((scope, guild)) = self.search_scope() else {
            return;
        };
        let current = {
            let text = self.search_entry.text().to_string();
            self.resolve_search(&text, guild).0
        };
        let values = |wanted: Key| -> String {
            current
                .items
                .iter()
                .filter(|(key, _)| *key == wanted)
                .map(|(_, value)| value.clone())
                .collect::<Vec<_>>()
                .join(", ")
        };

        let dialog = adw::Dialog::new();
        dialog.set_title("Filters");
        dialog.set_content_width(440);
        dialog.set_content_height(640);
        let page = adw::PreferencesPage::new();

        let entry_group = |title: &str, description: &str, row_title: &str, value: String| {
            let group = adw::PreferencesGroup::new();
            group.set_title(title);
            group.set_description(Some(description));
            let row = adw::EntryRow::new();
            row.set_title(row_title);
            row.set_text(&value);
            group.add(&row);
            (group, row)
        };

        let (from_group, from) =
            entry_group("From", "Sent by any of these people.", "Names, separated by commas", values(Key::From));
        page.add(&from_group);

        let has_group = adw::PreferencesGroup::new();
        has_group.set_title("Has");
        has_group.set_description(Some("Includes any of these kinds of content."));
        let has_flow = gtk::FlowBox::new();
        has_flow.set_selection_mode(gtk::SelectionMode::None);
        has_flow.set_max_children_per_line(5);
        has_flow.set_column_spacing(6);
        has_flow.set_row_spacing(6);
        let mut has_buttons = Vec::new();
        for has in Has::ALL {
            let button = gtk::ToggleButton::with_label(has.label());
            button.set_active(current.params.has.contains(&has));
            has_flow.append(&button);
            has_buttons.push((has, button));
        }
        has_group.add(&has_flow);
        page.add(&has_group);

        let (mentions_group, mentions) = entry_group(
            "Mentions",
            "Mentions any of these people.",
            "Names, separated by commas",
            values(Key::Mentions),
        );
        page.add(&mentions_group);

        let date_group = adw::PreferencesGroup::new();
        date_group.set_title("Date");
        date_group.set_description(Some("When the message was sent: 2026-01-31, today or yesterday."));
        let date_row = |title: &str, key: Key| {
            let row = adw::EntryRow::new();
            row.set_title(title);
            row.set_text(&values(key));
            date_group.add(&row);
            row
        };
        let before = date_row("Before", Key::Before);
        let during = date_row("On", Key::During);
        let after = date_row("After", Key::After);
        page.add(&date_group);

        let author_group = adw::PreferencesGroup::new();
        author_group.set_title("Author Type");
        author_group.set_description(Some("Sent by any of these kinds of author."));
        let author_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let mut author_buttons = Vec::new();
        for kind in AuthorType::ALL {
            let button = gtk::ToggleButton::with_label(kind.label());
            button.set_active(current.params.author_types.contains(&kind));
            author_box.append(&button);
            author_buttons.push((kind, button));
        }
        author_group.add(&author_box);
        page.add(&author_group);

        let pinned_group = adw::PreferencesGroup::new();
        pinned_group.set_title("Pinned");
        let pinned = adw::ComboRow::new();
        pinned.set_title("Pinned or not");
        pinned.set_model(Some(&gtk::StringList::new(&["Either", "Pinned", "Not pinned"])));
        pinned.set_selected(match current.params.pinned {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        });
        pinned_group.add(&pinned);
        page.add(&pinned_group);

        let channels = matches!(scope, Scope::Guild(_)).then(|| {
            let (group, row) = entry_group(
                "In",
                "In any of these channels (the whole server when empty).",
                "Channel names, separated by commas",
                values(Key::In),
            );
            page.add(&group);
            row
        });

        let more = adw::PreferencesGroup::new();
        more.set_title("Links and Files");
        let more_row = |title: &str, key: Key| {
            let row = adw::EntryRow::new();
            row.set_title(title);
            row.set_text(&values(key));
            more.add(&row);
            row
        };
        let link_from = more_row("Links to (example.com)", Key::LinkFrom);
        let file_type = more_row("File type (pdf, png)", Key::FileType);
        let file_name = more_row("File name contains", Key::FileName);
        page.add(&more);

        let problems = gtk::Label::new(None);
        problems.add_css_class("error");
        problems.set_wrap(true);
        problems.set_xalign(0.0);
        problems.set_visible(false);
        problems.set_margin_start(12);
        problems.set_margin_end(12);

        let clear = gtk::Button::with_label("Clear Filters");
        clear.add_css_class("flat");
        let cancel = gtk::Button::with_label("Cancel");
        let apply = gtk::Button::with_label("Apply Filters");
        apply.add_css_class("suggested-action");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_margin_top(8);
        buttons.set_margin_bottom(12);
        buttons.set_margin_start(12);
        buttons.set_margin_end(12);
        clear.set_hexpand(true);
        clear.set_halign(gtk::Align::Start);
        buttons.append(&clear);
        buttons.append(&cancel);
        buttons.append(&apply);
        let bottom = gtk::Box::new(gtk::Orientation::Vertical, 4);
        bottom.append(&problems);
        bottom.append(&buttons);

        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&page));
        view.add_bottom_bar(&bottom);
        dialog.set_child(Some(&view));

        let target = dialog.clone();
        cancel.connect_clicked(move |_| {
            target.close();
        });
        {
            let rows: Vec<adw::EntryRow> = [&from, &mentions, &before, &during, &after, &link_from, &file_type, &file_name]
                .into_iter()
                .cloned()
                .chain(channels.clone())
                .collect();
            let has_buttons = has_buttons.clone();
            let author_buttons = author_buttons.clone();
            let pinned = pinned.clone();
            clear.connect_clicked(move |_| {
                for row in &rows {
                    row.set_text("");
                }
                for (_, button) in &has_buttons {
                    button.set_active(false);
                }
                for (_, button) in &author_buttons {
                    button.set_active(false);
                }
                pinned.set_selected(0);
            });
        }
        let app = self.clone();
        let target = dialog.clone();
        let content = current.content.clone();
        apply.connect_clicked(move |_| {
            let mut parts: Vec<String> = Vec::new();
            let mut add = |key: Key, row: &adw::EntryRow| {
                for value in row.text().split(',').map(str::trim).filter(|value| !value.is_empty()) {
                    parts.push(format!("{}:{}", key.name(), quote(value)));
                }
            };
            add(Key::From, &from);
            add(Key::Mentions, &mentions);
            add(Key::Before, &before);
            add(Key::During, &during);
            add(Key::After, &after);
            if let Some(row) = &channels {
                add(Key::In, row);
            }
            add(Key::LinkFrom, &link_from);
            add(Key::FileType, &file_type);
            add(Key::FileName, &file_name);
            for (has, button) in &has_buttons {
                if button.is_active() {
                    parts.push(format!("has:{}", has.label().to_lowercase()));
                }
            }
            for (kind, button) in &author_buttons {
                if button.is_active() {
                    parts.push(format!("authorType:{}", kind.value()));
                }
            }
            match pinned.selected() {
                1 => parts.push("pinned:true".to_owned()),
                2 => parts.push("pinned:false".to_owned()),
                _ => {}
            }
            if !content.trim().is_empty() {
                parts.push(content.trim().to_owned());
            }
            let text = parts.join(" ");
            let (filters, found) = app.resolve_search(&text, guild);
            if !found.is_empty() {
                problems.set_label(&found.join("\n"));
                problems.set_visible(true);
                return;
            }
            target.close();
            app.search_entry.set_text(&filters.text());
            if filters.is_empty() {
                app.close_search();
                return;
            }
            app.start_search(scope, guild, filters);
        });
        dialog.present(Some(&self.window));
    }
}

fn build_panel() -> SearchPanel {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    root.add_css_class("kc-search-panel");
    // The side pane's own background (as the member list has).
    root.add_css_class("kc-pane-members");

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    header.set_margin_top(8);
    header.set_margin_start(12);
    header.set_margin_end(6);
    let title = gtk::Label::new(Some("Search"));
    title.add_css_class("heading");
    title.set_xalign(0.0);
    title.set_hexpand(true);
    header.append(&title);
    let labels: Vec<&str> = Sort::ALL.iter().map(|sort| sort.label()).collect();
    let sort = gtk::DropDown::from_strings(&labels);
    sort.set_tooltip_text(Some("Order"));
    sort.set_valign(gtk::Align::Center);
    sort.add_css_class("flat");
    header.append(&sort);
    let filters = icon_button(
        first_icon(&["funnel-symbolic", "filter-symbolic", "view-more-horizontal-symbolic", "view-more-symbolic"]),
        "Filters",
    );
    filters.set_widget_name("filters");
    filters.add_css_class("flat");
    header.append(&filters);
    let close = icon_button("window-close-symbolic", "Close Search");
    close.set_widget_name("close");
    close.add_css_class("flat");
    header.append(&close);
    root.append(&header);

    let chips = gtk::FlowBox::new();
    chips.set_selection_mode(gtk::SelectionMode::None);
    chips.set_max_children_per_line(6);
    chips.set_column_spacing(4);
    chips.set_row_spacing(4);
    chips.set_margin_start(10);
    chips.set_margin_end(10);
    chips.set_visible(false);
    root.append(&chips);

    let tab_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tab_box.add_css_class("linked");
    tab_box.set_margin_start(10);
    tab_box.set_margin_end(10);
    let mut tabs = Vec::new();
    let mut first: Option<gtk::ToggleButton> = None;
    for tab in Tab::ALL {
        let button = gtk::ToggleButton::with_label(tab.label());
        button.add_css_class("caption");
        button.set_hexpand(true);
        if let Some(text) = button.child().and_downcast::<gtk::Label>() {
            text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        }
        if let Some(first) = &first {
            button.set_group(Some(first));
        } else {
            first = Some(button.clone());
        }
        tab_box.append(&button);
        tabs.push((tab, button));
    }
    if let Some(first) = &first {
        first.set_active(true);
    }
    root.append(&tab_box);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 6);
    list.set_margin_start(8);
    list.set_margin_end(8);
    list.set_margin_bottom(8);
    let footer = gtk::Label::new(None);
    footer.add_css_class("caption");
    footer.add_css_class("dim-label");
    footer.set_wrap(true);
    footer.set_margin_bottom(12);
    let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
    column.append(&list);
    column.append(&footer);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);
    scroll.set_child(Some(&column));
    root.append(&scroll);

    SearchPanel {
        root,
        title,
        tabs,
        sort,
        chips,
        list,
        scroll,
        footer,
        run: RefCell::new(Run {
            scope: Scope::Channel(Snowflake(0)),
            guild: None,
            filters: Filters::default(),
            tab: Tab::Messages,
            sort: Sort::Newest,
            offset: 0,
            total: 0,
            loading: false,
            generation: 0,
            retries: 0,
        }),
        quiet: Cell::new(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_box_reads_words_and_filters() {
        let (content, filters) = parse_query("from:ada has:image cats in:\"general chat\" pinned:true dogs");
        assert_eq!(content, "cats dogs");
        assert_eq!(
            filters,
            vec![
                (Key::From, "ada".to_owned()),
                (Key::Has, "image".to_owned()),
                (Key::In, "general chat".to_owned()),
                (Key::Pinned, "true".to_owned()),
            ]
        );
        // Unknown keys and empty values are words.
        let (content, filters) = parse_query("time:12 from: ok");
        assert_eq!(content, "time:12 from: ok");
        assert!(filters.is_empty());
        // Names are matched however they are cased.
        let (_, filters) = parse_query("AuthorType:bot fileName:report LinkFrom:x.com");
        assert_eq!(
            filters,
            vec![
                (Key::AuthorType, "bot".to_owned()),
                (Key::FileName, "report".to_owned()),
                (Key::LinkFrom, "x.com".to_owned()),
            ]
        );
    }

    #[test]
    fn filters_write_back_what_reads_the_same() {
        let filters = Filters {
            content: "hello there".into(),
            items: vec![(Key::From, "ada lovelace".into()), (Key::Has, "image".into())],
            ..Filters::default()
        };
        let text = filters.text();
        assert_eq!(text, "from:\"ada lovelace\" has:image hello there");
        let (content, items) = parse_query(&text);
        assert_eq!(content, "hello there");
        assert_eq!(items, filters.items);
    }

    #[test]
    fn days_become_the_snowflakes_around_them() {
        let day = parse_day("2026-01-31").unwrap();
        assert_eq!((day.year(), day.month(), day.day_of_month(), day.hour()), (2026, 1, 31, 0));
        assert!(parse_day("2026/1/31").is_some());
        assert!(parse_day("today").is_some() && parse_day("yesterday").is_some());
        assert!(parse_day("someday").is_none() && parse_day("2026-13-40").is_none());
        let next = day.add_days(1).unwrap();
        assert!(snowflake_at(&next).0 - snowflake_at(&day).0 == 86_400_000 << 22);
    }
}
