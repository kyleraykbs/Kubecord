//! Application commands and interactions: the slash commands apps offer in
//! a place, and using apps -- running a command, pressing a button or
//! choosing from a menu on an app's message, submitting an app's form.
//!
//! Interactions are posted to `/interactions`; Discord answers on the
//! gateway (`INTERACTION_SUCCESS`, `INTERACTION_FAILURE`,
//! `INTERACTION_MODAL_CREATE`, `APPLICATION_COMMAND_AUTOCOMPLETE_RESPONSE`),
//! matched by the nonce sent with the request.

use kc_core::Snowflake;
use serde::Deserialize;
use serde_json::{Value, json};

/// Option types (`CommandOption::kind`).
pub mod option {
    pub const SUB_COMMAND: u8 = 1;
    pub const SUB_COMMAND_GROUP: u8 = 2;
    pub const STRING: u8 = 3;
    pub const INTEGER: u8 = 4;
    pub const BOOLEAN: u8 = 5;
    pub const USER: u8 = 6;
    pub const CHANNEL: u8 = 7;
    pub const ROLE: u8 = 8;
    pub const MENTIONABLE: u8 = 9;
    pub const NUMBER: u8 = 10;
    pub const ATTACHMENT: u8 = 11;
}

/// An app that offers commands.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CommandApp {
    pub id: Snowflake,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub bot_id: Option<Snowflake>,
}

/// One command an app offers.
#[derive(Clone, Debug, Deserialize)]
pub struct Command {
    pub id: Snowflake,
    pub application_id: Snowflake,
    #[serde(default)]
    pub version: Option<Snowflake>,
    /// 1 = a slash command; 2 and 3 are user and message context menus.
    #[serde(rename = "type", default = "chat_input")]
    pub kind: u8,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub options: Vec<CommandOption>,
    /// The command as Discord sent it, sent back when it is used.
    #[serde(skip)]
    pub raw: Value,
}

fn chat_input() -> u8 {
    1
}

impl Command {
    pub const CHAT_INPUT: u8 = 1;
    /// Listed when right-clicking a user.
    pub const USER: u8 = 2;
    /// Listed in a message's menu.
    pub const MESSAGE: u8 = 3;
}

/// An option (or subcommand) of a command.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CommandOption {
    #[serde(rename = "type")]
    pub kind: u8,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub choices: Vec<CommandChoice>,
    /// A subcommand's (or group's) own options.
    #[serde(default)]
    pub options: Vec<CommandOption>,
    /// The app suggests values as they are typed.
    #[serde(default)]
    pub autocomplete: bool,
}

impl CommandOption {
    pub fn is_subcommand(&self) -> bool {
        matches!(self.kind, option::SUB_COMMAND | option::SUB_COMMAND_GROUP)
    }
}

/// A fixed value an option can take.
#[derive(Clone, Debug, Deserialize)]
pub struct CommandChoice {
    pub name: String,
    pub value: Value,
}

/// The commands available in one place, and the apps offering them.
#[derive(Clone, Debug, Default)]
pub struct CommandIndex {
    pub applications: Vec<CommandApp>,
    pub commands: Vec<Command>,
}

impl CommandIndex {
    /// Reads an `application-command-index` answer. Commands that do not
    /// parse are skipped, not fatal.
    pub fn from_value(value: &Value) -> Self {
        let applications = value["applications"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|app| serde_json::from_value(app.clone()).ok())
            .collect();
        let commands = value["application_commands"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|raw| {
                let mut command: Command = serde_json::from_value(raw.clone()).ok()?;
                command.raw = raw.clone();
                Some(command)
            })
            .collect();
        Self {
            applications,
            commands,
        }
    }

    /// Adds `other`'s apps and commands that are not here already.
    pub fn merge(&mut self, other: CommandIndex) {
        for app in other.applications {
            if !self.applications.iter().any(|known| known.id == app.id) {
                self.applications.push(app);
            }
        }
        for command in other.commands {
            if !self.commands.iter().any(|known| known.id == command.id) {
                self.commands.push(command);
            }
        }
    }

    pub fn app(&self, id: Snowflake) -> Option<&CommandApp> {
        self.applications.iter().find(|app| app.id == id)
    }
}

/// Where commands are looked up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommandScope {
    /// A server's commands.
    Guild(Snowflake),
    /// A DM's or group DM's commands.
    Channel(Snowflake),
    /// Apps the user installed for themself (usable anywhere they allow).
    User,
}

impl CommandScope {
    pub(crate) fn path(self) -> String {
        match self {
            Self::Guild(id) => format!("/guilds/{id}/application-command-index"),
            Self::Channel(id) => format!("/channels/{id}/application-command-index"),
            Self::User => "/users/@me/application-command-index".to_owned(),
        }
    }
}

/// Where an interaction happens: the app, the place, and this client's
/// gateway session (Discord answers on it).
#[derive(Clone, Debug)]
pub struct InteractionPlace {
    pub application_id: Snowflake,
    pub guild_id: Option<Snowflake>,
    pub channel_id: Snowflake,
    pub session_id: String,
}

impl InteractionPlace {
    fn body(&self, kind: u8, nonce: &str, data: Value) -> Value {
        let mut body = json!({
            "type": kind,
            "application_id": self.application_id,
            "channel_id": self.channel_id,
            "session_id": self.session_id,
            "nonce": nonce,
            "data": data,
        });
        if let Some(guild) = self.guild_id {
            body["guild_id"] = json!(guild);
        }
        body
    }

    /// Runs `command` with `options` (already nested under any
    /// subcommands). With `autocomplete`, asks for suggestions for the
    /// option marked `"focused": true` instead.
    pub fn command(
        &self,
        command: &Command,
        options: Vec<Value>,
        autocomplete: bool,
        nonce: &str,
    ) -> Value {
        let mut body = self.body(
            if autocomplete { 4 } else { 2 },
            nonce,
            json!({
                "version": command.version,
                "id": command.id,
                "name": command.name,
                "type": command.kind,
                "options": options,
                "application_command": command.raw,
                "attachments": [],
            }),
        );
        if !autocomplete {
            body["analytics_location"] = json!("slash_ui");
        }
        body
    }

    /// Runs a user or message command on `target_id`.
    pub fn context_command(&self, command: &Command, target_id: Snowflake, nonce: &str) -> Value {
        let mut body = self.body(
            2,
            nonce,
            json!({
                "version": command.version,
                "id": command.id,
                "name": command.name,
                "type": command.kind,
                "target_id": target_id,
                "options": [],
                "application_command": command.raw,
                "attachments": [],
            }),
        );
        body["analytics_location"] = json!("context_menu");
        body
    }

    /// Presses a button (no `values`) or chooses from a select menu.
    pub fn component(
        &self,
        message_id: Snowflake,
        message_flags: u64,
        component_type: u8,
        custom_id: &str,
        values: Option<Vec<String>>,
        nonce: &str,
    ) -> Value {
        let mut data = json!({ "component_type": component_type, "custom_id": custom_id });
        if let Some(values) = values {
            data["values"] = json!(values);
        }
        let mut body = self.body(3, nonce, data);
        body["message_id"] = json!(message_id);
        body["message_flags"] = json!(message_flags);
        body
    }

    /// Submits an app's form: `components` hold the filled-in fields.
    pub fn modal(&self, modal_id: Snowflake, custom_id: &str, components: Vec<Value>, nonce: &str) -> Value {
        self.body(
            5,
            nonce,
            json!({ "id": modal_id, "custom_id": custom_id, "components": components }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_parses_and_keeps_the_raw_command() {
        let value = json!({
            "applications": [{"id": "10", "name": "Music", "icon": null, "bot_id": "10"}],
            "application_commands": [
                {"id": "1", "application_id": "10", "version": "5", "type": 1, "name": "play",
                 "description": "Play a song",
                 "options": [{"type": 3, "name": "song", "description": "What", "required": true, "autocomplete": true}]},
                {"broken": true}
            ]
        });
        let index = CommandIndex::from_value(&value);
        assert_eq!(index.applications[0].name, "Music");
        assert_eq!(index.commands.len(), 1);
        let play = &index.commands[0];
        assert_eq!(play.options[0].kind, option::STRING);
        assert!(play.options[0].required && play.options[0].autocomplete);
        assert_eq!(play.raw["name"], "play");
    }

    #[test]
    fn interaction_bodies_carry_the_place_and_nonce() {
        let place = InteractionPlace {
            application_id: Snowflake::new(10),
            guild_id: None,
            channel_id: Snowflake::new(20),
            session_id: "abc".to_owned(),
        };
        let button = place.component(Snowflake::new(30), 0, 2, "next", None, "99");
        assert_eq!(button["type"], 3);
        assert_eq!(button["data"]["custom_id"], "next");
        assert!(button.get("guild_id").is_none());
        assert_eq!(button["nonce"], "99");
        let raw = json!({"id": "1", "application_id": "10", "type": 3, "name": "Translate"});
        let mut translate: Command = serde_json::from_value(raw.clone()).unwrap();
        translate.raw = raw;
        let run = place.context_command(&translate, Snowflake::new(55), "7");
        assert_eq!(run["data"]["type"], 3);
        assert_eq!(run["data"]["target_id"], "55");
        let form = place.modal(Snowflake::new(40), "form", vec![], "100");
        assert_eq!(form["type"], 5);
        assert_eq!(form["data"]["id"], "40");
    }
}
