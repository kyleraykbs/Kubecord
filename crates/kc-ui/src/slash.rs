//! Slash commands in the composer: the apps' commands (and Discord's own
//! built-ins like `/shrug`), suggested as they are typed, then run.
//!
//! A command line reads `/name [group] [subcommand] option:value ...`. A
//! value given without its `name:` goes to the first required option (or
//! the first option) still unset, so `/play never gonna` works.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use kc_http::commands::option;
use kc_http::{Command, CommandIndex, CommandOption, CommandScope, cdn};
use serde_json::{Value, json};

use crate::composer::Completion;
use crate::interactions::{Choices, Outcome, Waiting};
use crate::session::AccountId;
use crate::shell::AppState;

/// A command list is looked up again after this long.
const INDEX_MAX_AGE: Duration = Duration::from_secs(5 * 60);
/// Typing pauses this long before the app is asked for suggestions.
const AUTOCOMPLETE_PAUSE: Duration = Duration::from_millis(250);
/// Suggestions shown at once.
const SHOWN: usize = 10;

/// One of Discord's own commands, done by the client.
struct Builtin {
    name: &'static str,
    description: &'static str,
    /// Whether it needs text after it.
    needs_text: bool,
    apply: fn(&str) -> String,
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "shrug",
        description: "Appends ¯\\_(ツ)_/¯ to your message.",
        needs_text: false,
        apply: |text| format!("{text} ¯\\_(ツ)_/¯").trim_start().to_owned(),
    },
    Builtin {
        name: "tableflip",
        description: "Appends (╯°□°)╯︵ ┻━┻ to your message.",
        needs_text: false,
        apply: |text| format!("{text} (╯°□°)╯︵ ┻━┻").trim_start().to_owned(),
    },
    Builtin {
        name: "unflip",
        description: "Appends ┬─┬ノ( º _ ºノ) to your message.",
        needs_text: false,
        apply: |text| format!("{text} ┬─┬ノ( º _ ºノ)").trim_start().to_owned(),
    },
    Builtin {
        name: "me",
        description: "Displays text with emphasis.",
        needs_text: true,
        apply: |text| format!("_{text}_"),
    },
    Builtin {
        name: "spoiler",
        description: "Marks your message as a spoiler.",
        needs_text: true,
        apply: |text| format!("||{text}||"),
    },
];

struct Cached {
    index: Rc<CommandIndex>,
    fetched: Instant,
}

thread_local! {
    /// Command lists by account and place (each merged with the user's own
    /// installed apps).
    static INDEXES: RefCell<HashMap<(AccountId, CommandScope), Cached>> = RefCell::new(HashMap::new());
    /// Lookups under way.
    static FETCHING: RefCell<Vec<(AccountId, CommandScope)>> = const { RefCell::new(Vec::new()) };
    /// The app's last suggestions: for which (command, option, typed text),
    /// and the (name, value) pairs.
    static SUGGESTED: RefCell<Option<(String, Choices)>> = const { RefCell::new(None) };
    /// The typed text a suggestion request is waiting to send for.
    static ASKING: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Suggestions picked: (option, shown name) -> the value to send.
    static CHOSEN: RefCell<Vec<(String, String, Value)>> = const { RefCell::new(Vec::new()) };
}

/// A command line taken apart up to some point.
pub(crate) struct Line<'a> {
    pub command: &'a Command,
    /// Subcommands (and groups) chosen.
    pub path: Vec<&'a CommandOption>,
    /// What can follow the path: subcommands while `needs_subcommand`,
    /// options otherwise.
    pub leaf: &'a [CommandOption],
    pub needs_subcommand: bool,
    /// Options given by name: the option, its value as typed, and where the
    /// value starts (in characters).
    pub given: Vec<(&'a CommandOption, String, usize)>,
    /// Text given without an option name, and where it starts.
    pub positional: Option<(String, usize)>,
}

/// The command `text` starts with, if `/name` is complete (followed by a
/// space or the end of the text when `complete`).
fn find_command<'a>(text: &str, commands: &'a [Command]) -> Option<(&'a Command, usize)> {
    let body = text.strip_prefix('/')?;
    let name_len = body.find(char::is_whitespace).unwrap_or(body.len());
    let name = &body[..name_len];
    let command = commands
        .iter()
        .find(|command| command.kind == Command::CHAT_INPUT && command.name == name)?;
    Some((command, 1 + name.chars().count()))
}

/// Takes `text` apart against `command` (named by its first `name_chars`
/// characters).
pub(crate) fn split_line<'a>(text: &str, command: &'a Command, name_chars: usize) -> Line<'a> {
    let chars: Vec<char> = text.chars().collect();
    let mut at = name_chars;
    let mut path = Vec::new();
    let mut leaf: &[CommandOption] = &command.options;
    let mut needs_subcommand = leaf.iter().any(CommandOption::is_subcommand);
    while needs_subcommand {
        while at < chars.len() && chars[at].is_whitespace() {
            at += 1;
        }
        let start = at;
        while at < chars.len() && !chars[at].is_whitespace() {
            at += 1;
        }
        let word: String = chars[start..at].iter().collect();
        // Only a finished word picks a subcommand.
        let finished = at < chars.len();
        match leaf
            .iter()
            .find(|option| option.is_subcommand() && option.name == word)
            .filter(|_| finished)
        {
            Some(sub) => {
                path.push(sub);
                leaf = &sub.options;
                needs_subcommand = leaf.iter().any(CommandOption::is_subcommand);
            }
            None => {
                at = start;
                break;
            }
        }
    }
    let options_start = at;
    let mut given = Vec::new();
    let mut positional = None;
    if !needs_subcommand {
        // Where each `name:` starts (at a word start), longest names first so
        // `user_id:` is not read as `user:`.
        let mut names: Vec<&CommandOption> = leaf.iter().collect();
        names.sort_by_key(|option| std::cmp::Reverse(option.name.len()));
        let mut markers: Vec<(usize, usize, &CommandOption)> = Vec::new();
        let mut i = options_start;
        while i < chars.len() {
            let word_start = i == options_start || chars[i - 1].is_whitespace();
            if word_start {
                let rest: String = chars[i..].iter().collect();
                if let Some(option) = names
                    .iter()
                    .find(|option| rest.starts_with(&format!("{}:", option.name)))
                {
                    let value_start = i + option.name.chars().count() + 1;
                    markers.push((i, value_start, option));
                    i = value_start;
                    continue;
                }
            }
            i += 1;
        }
        let first = markers.first().map_or(chars.len(), |(start, ..)| *start);
        let before: String = chars[options_start..first].iter().collect();
        if !before.trim().is_empty() {
            let skipped = before.chars().take_while(|c| c.is_whitespace()).count();
            positional = Some((before.trim().to_owned(), options_start + skipped));
        }
        for (index, (_, value_start, option)) in markers.iter().enumerate() {
            let end = markers
                .get(index + 1)
                .map_or(chars.len(), |(next, ..)| *next);
            let value: String = chars[*value_start..end].iter().collect();
            given.push((*option, value, *value_start));
        }
    }
    Line {
        command,
        path,
        leaf,
        needs_subcommand,
        given,
        positional,
    }
}

/// The id in a mention (`<@1>`, `<@!1>`, `<@&1>`, `<#1>`) or a bare id.
fn mention_id(raw: &str, prefixes: &[&str]) -> Option<String> {
    let inner = prefixes
        .iter()
        .find_map(|prefix| raw.strip_prefix(prefix).and_then(|rest| rest.strip_suffix('>')))
        .unwrap_or(raw);
    (!inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit())).then(|| inner.to_owned())
}

/// The value to send for `raw` typed for `option`. `chosen` knows values of
/// suggestions picked from the app.
fn convert(
    option: &CommandOption,
    raw: &str,
    chosen: &dyn Fn(&str, &str) -> Option<Value>,
) -> Result<Value, String> {
    let raw = raw.trim();
    let name = &option.name;
    if let Some(value) = chosen(name, raw) {
        return Ok(value);
    }
    if !option.choices.is_empty() {
        return option
            .choices
            .iter()
            .find(|choice| {
                choice.name.eq_ignore_ascii_case(raw)
                    || match &choice.value {
                        Value::String(value) => value == raw,
                        // A number choice typed as its number.
                        other => raw.parse::<Value>().ok().as_ref() == Some(other),
                    }
            })
            .map(|choice| choice.value.clone())
            .ok_or_else(|| format!("“{raw}” is not one of the choices for {name}"));
    }
    let bad = |what: &str| format!("{name} needs {what}");
    match option.kind {
        option::STRING => Ok(json!(raw)),
        option::INTEGER => raw.parse::<i64>().map(|n| json!(n)).map_err(|_| bad("a whole number")),
        option::NUMBER => raw.parse::<f64>().map(|n| json!(n)).map_err(|_| bad("a number")),
        option::BOOLEAN => match raw.to_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(json!(true)),
            "false" | "no" | "off" | "0" => Ok(json!(false)),
            _ => Err(bad("True or False")),
        },
        option::USER => mention_id(raw, &["<@!", "<@"]).map(|id| json!(id)).ok_or_else(|| bad("a user")),
        option::ROLE => mention_id(raw, &["<@&"]).map(|id| json!(id)).ok_or_else(|| bad("a role")),
        option::CHANNEL => mention_id(raw, &["<#"]).map(|id| json!(id)).ok_or_else(|| bad("a channel")),
        option::MENTIONABLE => mention_id(raw, &["<@&", "<@!", "<@"])
            .map(|id| json!(id))
            .ok_or_else(|| bad("a user or role")),
        option::ATTACHMENT => Err(format!("{name} needs a file, which Kubecord cannot add to commands yet")),
        _ => Ok(json!(raw)),
    }
}

/// The options to send for a finished command line, nested under its
/// subcommands. With `focused`, the option (and its partial value) being
/// typed is marked for autocomplete, and nothing is checked.
pub(crate) fn build_options(
    line: &Line,
    chosen: &dyn Fn(&str, &str) -> Option<Value>,
    focused: Option<(&str, &str)>,
) -> Result<Vec<Value>, String> {
    if line.needs_subcommand {
        let names: Vec<&str> = line.leaf.iter().map(|option| option.name.as_str()).collect();
        return Err(format!("Choose one of: {}", names.join(", ")));
    }
    let mut values: Vec<(&CommandOption, String)> = line
        .given
        .iter()
        .map(|(option, value, _)| (*option, value.clone()))
        .collect();
    if let Some((text, _)) = &line.positional {
        let unset = |option: &&CommandOption| !values.iter().any(|(given, _)| given.name == option.name);
        let target = line
            .leaf
            .iter()
            .filter(unset)
            .find(|option| option.required)
            .or_else(|| line.leaf.iter().find(unset));
        match target {
            Some(option) => values.push((option, text.clone())),
            None if focused.is_some() => {}
            None => return Err(format!("{} takes no more text", line.command.name)),
        }
    }
    let mut out = Vec::new();
    for (option, raw) in &values {
        if let Some((name, partial)) = focused
            && name == option.name
        {
            out.push(json!({"type": option.kind, "name": option.name, "value": partial, "focused": true}));
            continue;
        }
        match convert(option, raw, chosen) {
            Ok(value) => out.push(json!({"type": option.kind, "name": option.name, "value": value})),
            Err(_) if focused.is_some() => {}
            Err(error) => return Err(error),
        }
    }
    if focused.is_none() {
        let missing: Vec<&str> = line
            .leaf
            .iter()
            .filter(|option| option.required && !values.iter().any(|(given, _)| given.name == option.name))
            .map(|option| option.name.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(format!("Missing: {}", missing.join(", ")));
        }
    }
    for sub in line.path.iter().rev() {
        out = vec![json!({"type": sub.kind, "name": sub.name, "options": out})];
    }
    Ok(out)
}

/// What sending a line starting with `/` did.
pub(crate) enum Sent {
    /// Not a known command: send it as text.
    NotCommand,
    /// A built-in turned it into this text, to send.
    Text(String),
    /// Run (or refused, with the reason shown): nothing more to send.
    Handled,
}

fn chosen_value(option: &str, shown: &str) -> Option<Value> {
    CHOSEN.with_borrow(|chosen| {
        chosen
            .iter()
            .find(|(name, display, _)| name == option && display == shown)
            .map(|(.., value)| value.clone())
    })
}

impl AppState {
    /// Puts `index` in place as the commands here (for the demo, which has
    /// no network).
    pub(crate) fn seed_commands(&self, index: CommandIndex) {
        if let Some(key) = self.command_scope() {
            INDEXES.with_borrow_mut(|indexes| {
                indexes.insert(
                    key,
                    Cached {
                        index: Rc::new(index),
                        fetched: Instant::now(),
                    },
                );
            });
        }
    }

    fn command_scope(&self) -> Option<(AccountId, CommandScope)> {
        let (account, ..) = self.active()?;
        let scope = self
            .with_session(|session| {
                match session.selected_guild.filter(|_| !session.dm_mode) {
                    Some(guild) => Some(CommandScope::Guild(guild)),
                    None => session.selected_channel.map(CommandScope::Channel),
                }
            })
            .flatten()?;
        Some((account, scope))
    }

    /// The commands usable here, if known; looks them up (again) when not,
    /// or when old, and refreshes the suggestions once they come.
    pub(crate) fn commands_here(self: &Rc<Self>) -> Option<Rc<CommandIndex>> {
        let (account, scope) = self.command_scope()?;
        let key = (account, scope);
        let (known, stale) = INDEXES.with_borrow(|indexes| match indexes.get(&key) {
            Some(cached) => (Some(cached.index.clone()), cached.fetched.elapsed() > INDEX_MAX_AGE),
            None => (None, true),
        });
        let fetching = FETCHING.with_borrow(|fetching| fetching.contains(&key));
        if stale
            && !fetching
            && let Some((_, http, _)) = self.active()
        {
            FETCHING.with_borrow_mut(|fetching| fetching.push(key));
            crate::video_setup::spawn_local(
                self,
                async move {
                    let mut index = http.command_index(scope).await?;
                    // The user's own installed apps, where they allow it.
                    if let Ok(mine) = http.command_index(CommandScope::User).await {
                        index.merge(mine);
                    }
                    Ok::<_, kc_http::Error>(index)
                },
                move |app, result| {
                    FETCHING.with_borrow_mut(|fetching| fetching.retain(|known| *known != key));
                    match result {
                        Ok(index) => {
                            INDEXES.with_borrow_mut(|indexes| {
                                indexes.insert(
                                    key,
                                    Cached {
                                        index: Rc::new(index),
                                        fetched: Instant::now(),
                                    },
                                );
                            });
                            if app.composer_text().starts_with('/') {
                                app.refresh_completion();
                            }
                        }
                        Err(error) => tracing::warn!(%error, "could not get the commands here"),
                    }
                },
            );
        }
        known
    }

    /// Suggestions for a command line being typed (`before` is the text up
    /// to the cursor, one character per buffer position), or `None` when it
    /// is not one. The flag says the list only hints (Enter still sends).
    pub(crate) fn slash_completions(self: &Rc<Self>, before: &str) -> Option<(Vec<Completion>, bool)> {
        let body = before.strip_prefix('/')?;
        let index = self.commands_here();
        let commands: &[Command] = index.as_ref().map_or(&[], |index| &index.commands);

        // Still typing the command's name.
        if !body.contains(char::is_whitespace) {
            let typed = body.to_lowercase();
            let mut found: Vec<Completion> = BUILTINS
                .iter()
                .filter(|builtin| builtin.name.starts_with(&typed))
                .map(|builtin| Completion {
                    title: format!("/{}", builtin.name),
                    detail: builtin.description.to_owned(),
                    icon: None,
                    glyph: None,
                    display: format!("/{}", builtin.name),
                    raw: format!("/{}", builtin.name),
                    span: Some((0, before.chars().count() as i32)),
                    trail: true,
                })
                .collect();
            let mut apps: Vec<&Command> = commands
                .iter()
                .filter(|command| command.kind == Command::CHAT_INPUT && command.name.starts_with(&typed))
                .collect();
            apps.sort_by(|a, b| a.name.cmp(&b.name));
            for command in apps {
                let app = index.as_ref().and_then(|index| index.app(command.application_id));
                found.push(Completion {
                    title: format!("/{}", command.name),
                    detail: match app {
                        Some(app) => format!("{} · {}", command.description, app.name),
                        None => command.description.clone(),
                    },
                    icon: app.and_then(|app| {
                        app.icon.as_deref().map(|icon| cdn::app_icon(app.id, icon, 48))
                    }),
                    glyph: None,
                    display: format!("/{}", command.name),
                    raw: format!("/{}", command.name),
                    span: Some((0, before.chars().count() as i32)),
                    trail: true,
                });
            }
            found.truncate(SHOWN * 2);
            return Some((found, false));
        }

        let (command, name_chars) = find_command(before, commands)?;
        let line = split_line(before, command, name_chars);
        let length = before.chars().count();
        let word_start = before
            .chars()
            .collect::<Vec<_>>()
            .iter()
            .rposition(|c| c.is_whitespace())
            .map_or(0, |at| at + 1);
        let word: String = before.chars().skip(word_start).collect();

        if line.needs_subcommand {
            let found = line
                .leaf
                .iter()
                .filter(|option| option.is_subcommand() && option.name.starts_with(&word))
                .take(SHOWN)
                .map(|option| Completion {
                    title: option.name.clone(),
                    detail: option.description.clone(),
                    icon: None,
                    glyph: None,
                    display: option.name.clone(),
                    raw: option.name.clone(),
                    span: Some((word_start as i32, length as i32)),
                    trail: true,
                })
                .collect();
            return Some((found, false));
        }

        let unset = |option: &&CommandOption| {
            !line.given.iter().any(|(given, ..)| given.name == option.name)
        };
        let option_names = |prefix: &str| -> Vec<Completion> {
            line.leaf
                .iter()
                .filter(unset)
                .filter(|option| option.name.starts_with(prefix))
                .take(SHOWN)
                .map(|option| Completion {
                    title: format!("{}:", option.name),
                    detail: if option.required {
                        format!("{} (required)", option.description)
                    } else {
                        option.description.clone()
                    },
                    icon: None,
                    glyph: None,
                    display: format!("{}:", option.name),
                    raw: format!("{}:", option.name),
                    span: Some((word_start as i32, length as i32)),
                    trail: false,
                })
                .collect()
        };

        // A partly typed option name.
        if !word.is_empty() && !word.contains(':') {
            let names = option_names(&word);
            if !names.is_empty() {
                return Some((names, false));
            }
        }
        // A value being typed: the last option named, or the bare text.
        let current = line
            .given
            .last()
            .map(|(option, value, start)| (*option, value.clone(), *start))
            .or_else(|| {
                let (text, start) = line.positional.clone()?;
                let unset_required = line.leaf.iter().filter(unset).find(|option| option.required);
                let option = unset_required.or_else(|| line.leaf.iter().find(unset))?;
                Some((option, text, start))
            });
        if let Some((option, value, start)) = current {
            // A value ended by a space is done: offer the options left.
            if !value.ends_with(char::is_whitespace) {
                let found = self.value_suggestions(&line, option, &value, start, length);
                if !found.is_empty() {
                    return Some((found, false));
                }
            }
        }
        if before.ends_with(char::is_whitespace) {
            return Some((option_names(""), true));
        }
        Some((Vec::new(), false))
    }

    /// Suggestions for the value of `option` typed so far (`typed`, from
    /// character `start` to `end`).
    fn value_suggestions(
        self: &Rc<Self>,
        line: &Line,
        option: &CommandOption,
        typed: &str,
        start: usize,
        end: usize,
    ) -> Vec<Completion> {
        let typed_trim = typed.trim_start();
        let start = start + (typed.chars().count() - typed_trim.chars().count());
        let lower = typed_trim.to_lowercase();
        let span = Some((start as i32, end as i32));
        let simple = |title: String, display: String, raw: String, icon: Option<String>| Completion {
            title,
            detail: String::new(),
            icon,
            glyph: None,
            display,
            raw,
            span,
            trail: true,
        };
        if !option.choices.is_empty() {
            return option
                .choices
                .iter()
                .filter(|choice| choice.name.to_lowercase().contains(&lower))
                .take(SHOWN)
                .map(|choice| simple(choice.name.clone(), choice.name.clone(), choice.name.clone(), None))
                .collect();
        }
        if option.autocomplete {
            return self.app_suggestions(line, option, typed_trim, span);
        }
        let (guild, channel) = self
            .with_session(|session| {
                (session.selected_guild.filter(|_| !session.dm_mode), session.selected_channel)
            })
            .unwrap_or((None, None));
        match option.kind {
            option::BOOLEAN => ["True", "False"]
                .into_iter()
                .filter(|name| name.to_lowercase().starts_with(&lower))
                .map(|name| simple(name.to_owned(), name.to_owned(), name.to_owned(), None))
                .collect(),
            option::USER | option::ROLE | option::MENTIONABLE | option::CHANNEL => {
                let query = lower.trim_start_matches(['@', '#']).to_owned();
                self.with_session(|session| {
                    session.store.read(|state| {
                        let mut found = Vec::new();
                        if matches!(option.kind, option::USER | option::MENTIONABLE) {
                            let users: Vec<&kc_state::User> = match guild {
                                Some(guild) => state
                                    .guild_members
                                    .get(&guild)
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|id| state.users.get(id))
                                    .collect(),
                                None => channel
                                    .and_then(|id| state.channels.get(&id))
                                    .and_then(|channel| channel.recipients.as_ref())
                                    .into_iter()
                                    .flatten()
                                    .collect(),
                            };
                            for user in users {
                                let name = state.display_name(guild, user);
                                if name.to_lowercase().contains(&query)
                                    || user.username.to_lowercase().contains(&query)
                                {
                                    found.push(simple(
                                        format!("@{name}"),
                                        format!("@{name}"),
                                        format!("<@{}>", user.id),
                                        Some(cdn::user_avatar(user.id, user.avatar.as_deref(), 48)),
                                    ));
                                }
                            }
                        }
                        if matches!(option.kind, option::ROLE | option::MENTIONABLE)
                            && let Some(guild) = guild.and_then(|id| state.guilds.get(&id))
                        {
                            for role in &guild.roles {
                                if role.name.to_lowercase().contains(&query) {
                                    found.push(simple(
                                        format!("@{}", role.name),
                                        format!("@{}", role.name),
                                        format!("<@&{}>", role.id),
                                        None,
                                    ));
                                }
                            }
                        }
                        if option.kind == option::CHANNEL {
                            for section in guild.map(|guild| state.channel_tree(guild)).unwrap_or_default() {
                                for channel in section.channels {
                                    if channel.display_name().to_lowercase().contains(&query) {
                                        found.push(simple(
                                            format!("#{}", channel.display_name()),
                                            format!("#{}", channel.display_name()),
                                            format!("<#{}>", channel.id),
                                            None,
                                        ));
                                    }
                                }
                            }
                        }
                        found.truncate(SHOWN);
                        found
                    })
                })
                .unwrap_or_default()
            }
            _ => Vec::new(),
        }
    }

    /// The app's own suggestions for the option being typed: the last ones
    /// if they are for this text, else asked for after a short pause.
    fn app_suggestions(
        self: &Rc<Self>,
        line: &Line,
        option: &CommandOption,
        typed: &str,
        span: Option<(i32, i32)>,
    ) -> Vec<Completion> {
        let key = format!("{}\u{0}{}\u{0}{}", line.command.id, option.name, typed);
        let known = SUGGESTED.with_borrow(|suggested| {
            suggested
                .as_ref()
                .filter(|(for_key, _)| *for_key == key)
                .map(|(_, choices)| choices.clone())
        });
        if let Some(choices) = known {
            let option_name = option.name.clone();
            return choices
                .into_iter()
                .take(SHOWN)
                .map(|(name, value)| {
                    CHOSEN.with_borrow_mut(|chosen| {
                        chosen.retain(|(known, shown, _)| !(known == &option_name && shown == &name));
                        chosen.push((option_name.clone(), name.clone(), value));
                    });
                    Completion {
                        title: name.clone(),
                        detail: String::new(),
                        icon: None,
                        glyph: None,
                        display: name.clone(),
                        raw: name,
                        span,
                        trail: true,
                    }
                })
                .collect();
        }
        let Ok(options) = build_options(line, &chosen_value, Some((&option.name, typed))) else {
            return Vec::new();
        };
        let Some((http, place)) = self.interaction_place(line.command.application_id) else {
            return Vec::new();
        };
        let body_command = line.command.clone();
        ASKING.with_borrow_mut(|asking| *asking = Some(key.clone()));
        let app = self.clone();
        glib::timeout_add_local_once(AUTOCOMPLETE_PAUSE, move || {
            // Typing went on: a newer request replaces this one.
            if ASKING.with_borrow(|asking| asking.as_deref() != Some(key.as_str())) {
                return;
            }
            let nonce = kc_http::HttpClient::new_nonce();
            let body = place.command(&body_command, options, true, &nonce);
            let answered = app.clone();
            app.send_interaction(
                http,
                body,
                nonce,
                Waiting::Suggestions(Box::new(move |choices| {
                    SUGGESTED.with_borrow_mut(|suggested| *suggested = Some((key, choices)));
                    answered.refresh_completion();
                })),
            );
        });
        Vec::new()
    }

    /// Runs the command line `text` (mentions already in their raw form).
    pub(crate) fn send_slash(self: &Rc<Self>, text: &str) -> Sent {
        let body = text.trim_start_matches('/');
        let name_len = body.find(char::is_whitespace).unwrap_or(body.len());
        let (name, rest) = body.split_at(name_len);
        if let Some(builtin) = BUILTINS.iter().find(|builtin| builtin.name == name) {
            let rest = rest.trim();
            if builtin.needs_text && rest.is_empty() {
                self.set_status(&format!("/{} needs some text", builtin.name));
                return Sent::Handled;
            }
            return Sent::Text((builtin.apply)(rest));
        }
        let Some(index) = self.commands_here() else {
            return Sent::NotCommand;
        };
        let Some((command, name_chars)) = find_command(text, &index.commands) else {
            return Sent::NotCommand;
        };
        let line = split_line(text, command, name_chars);
        let options = match build_options(&line, &chosen_value, None) {
            Ok(options) => options,
            Err(error) => {
                self.set_status(&format!("/{}: {error}", command.name));
                return Sent::Handled;
            }
        };
        let Some((http, place)) = self.interaction_place(command.application_id) else {
            self.set_status("Not connected yet: try again in a moment");
            return Sent::Handled;
        };
        let nonce = kc_http::HttpClient::new_nonce();
        let request = place.command(command, options, false, &nonce);
        let typed = text.to_owned();
        let app = self.clone();
        self.send_interaction(
            http,
            request,
            nonce,
            Waiting::Outcome(Box::new(move |outcome| {
                // Put a refused command back, to fix and try again.
                if let Outcome::Failed(_) = outcome {
                    app.restore_command(&typed);
                }
            })),
        );
        CHOSEN.with_borrow_mut(Vec::clear);
        self.set_composer_text("");
        Sent::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command() -> Command {
        let raw = json!({
            "id": "1", "application_id": "9", "type": 1, "name": "music", "description": "",
            "options": [
                {"type": 1, "name": "play", "description": "", "options": [
                    {"type": 3, "name": "song", "description": "", "required": true},
                    {"type": 5, "name": "loop", "description": ""},
                    {"type": 6, "name": "for", "description": ""}
                ]},
                {"type": 1, "name": "stop", "description": ""}
            ]
        });
        let mut command: Command = serde_json::from_value(raw.clone()).unwrap();
        command.raw = raw;
        command
    }

    fn nothing(_: &str, _: &str) -> Option<Value> {
        None
    }

    #[test]
    fn a_line_with_a_subcommand_and_named_options() {
        let command = command();
        let text = "/music play never gonna loop:yes for:<@42>";
        let line = split_line(text, &command, 6);
        assert_eq!(line.path.len(), 1);
        assert!(!line.needs_subcommand);
        assert_eq!(line.positional.as_ref().map(|(text, _)| text.as_str()), Some("never gonna"));
        let options = build_options(&line, &nothing, None).unwrap();
        assert_eq!(
            options,
            vec![json!({"type": 1, "name": "play", "options": [
                {"type": 5, "name": "loop", "value": true},
                {"type": 6, "name": "for", "value": "42"},
                {"type": 3, "name": "song", "value": "never gonna"}
            ]})]
        );
    }

    #[test]
    fn missing_and_bad_values_are_explained() {
        let command = command();
        let line = split_line("/music play loop:yes", &command, 6);
        assert_eq!(build_options(&line, &nothing, None), Err("Missing: song".to_owned()));
        let line = split_line("/music play x loop:maybe", &command, 6);
        assert_eq!(build_options(&line, &nothing, None), Err("loop needs True or False".to_owned()));
        let line = split_line("/music ", &command, 6);
        assert!(line.needs_subcommand);
        assert_eq!(build_options(&line, &nothing, None), Err("Choose one of: play, stop".to_owned()));
    }

    #[test]
    fn an_unfinished_subcommand_word_is_not_chosen() {
        let command = command();
        let line = split_line("/music play", &command, 6);
        assert!(line.needs_subcommand);
        let line = split_line("/music stop", &command, 6);
        assert!(line.needs_subcommand);
        let line = split_line("/music stop ", &command, 6);
        assert!(!line.needs_subcommand);
        assert_eq!(build_options(&line, &nothing, None).unwrap(), vec![json!({"type": 1, "name": "stop", "options": []})]);
    }

    #[test]
    fn focused_options_are_marked_for_autocomplete() {
        let command = command();
        let line = split_line("/music play nev", &command, 6);
        let options = build_options(&line, &nothing, Some(("song", "nev"))).unwrap();
        assert_eq!(options[0]["options"][0], json!({"type": 3, "name": "song", "value": "nev", "focused": true}));
    }

    #[test]
    fn builtins_shape_the_text() {
        let shrug = BUILTINS.iter().find(|b| b.name == "shrug").unwrap();
        assert_eq!((shrug.apply)("ok"), "ok ¯\\_(ツ)_/¯");
        assert_eq!((shrug.apply)(""), "¯\\_(ツ)_/¯");
        let me = BUILTINS.iter().find(|b| b.name == "me").unwrap();
        assert_eq!((me.apply)("waves"), "_waves_");
    }
}
