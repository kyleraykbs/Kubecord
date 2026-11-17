use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kc_core::Snowflake;
use parking_lot::RwLock;
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::warn;

use crate::channel::Channel;
use crate::guild::{Guild, Presence, Relationship, Stream, VoiceState};
use crate::message::{Message, Reaction};
use crate::user::User;

const MESSAGE_WINDOW: usize = 300;

/// A per-channel, id-ordered window of the most recent messages.
#[derive(Debug)]
pub struct MessageBuffer {
    messages: Vec<Message>,
    limit: usize,
}

impl Default for MessageBuffer {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            limit: MESSAGE_WINDOW,
        }
    }
}

impl MessageBuffer {
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn get(&self, id: Snowflake) -> Option<&Message> {
        self.messages
            .binary_search_by_key(&id, |message| message.id)
            .ok()
            .map(|index| &self.messages[index])
    }

    /// Inserts or replaces a message, keeping the window bounded.
    pub fn insert(&mut self, message: Message) {
        match self
            .messages
            .binary_search_by_key(&message.id, |existing| existing.id)
        {
            Ok(index) => self.messages[index] = message,
            Err(index) => self.messages.insert(index, message),
        }
        if self.messages.len() > self.limit {
            let excess = self.messages.len() - self.limit;
            self.messages.drain(..excess);
        }
    }

    /// Merges a page of history, deduplicating by id.
    pub fn extend(&mut self, messages: impl IntoIterator<Item = Message>) {
        for message in messages {
            match self
                .messages
                .binary_search_by_key(&message.id, |existing| existing.id)
            {
                Ok(index) => self.messages[index] = message,
                Err(index) => self.messages.insert(index, message),
            }
        }
        if self.messages.len() > self.limit {
            let excess = self.messages.len() - self.limit;
            self.messages.drain(..excess);
        }
    }

    pub fn remove(&mut self, id: Snowflake) -> Option<Message> {
        self.messages
            .binary_search_by_key(&id, |message| message.id)
            .ok()
            .map(|index| self.messages.remove(index))
    }

    /// Applies an in-place edit. Returns false when the message is outside the
    /// window (e.g. an edit for a message older than the retained history).
    pub fn update_with(&mut self, id: Snowflake, f: impl FnOnce(&mut Message)) -> bool {
        match self
            .messages
            .binary_search_by_key(&id, |message| message.id)
        {
            Ok(index) => {
                f(&mut self.messages[index]);
                true
            }
            Err(_) => false,
        }
    }
}

/// Changes worth telling the UI about.
#[derive(Clone, Debug)]
pub enum StoreEvent {
    Ready,
    GuildAdded(Snowflake),
    GuildUpdated(Snowflake),
    GuildRemoved(Snowflake),
    ChannelAdded(Snowflake),
    ChannelUpdated(Snowflake),
    ChannelRemoved(Snowflake),
    MessageAdded {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    MessageUpdated {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    MessageDeleted {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    Typing {
        channel_id: Snowflake,
        user_id: Snowflake,
    },
    PresenceUpdated {
        guild_id: Option<Snowflake>,
        user_id: Snowflake,
    },
    VoiceStateUpdated {
        guild_id: Option<Snowflake>,
        user_id: Snowflake,
    },
    VoiceServerUpdated {
        guild_id: Snowflake,
        endpoint: Option<String>,
    },
    StreamUpdated(String),
    RelationshipsUpdated,
    GuildChannelsLoaded(Snowflake),
    HistoryLoaded(Snowflake),
}

#[derive(Default)]
pub struct State {
    pub me: Option<User>,
    pub users: HashMap<Snowflake, User>,
    pub guilds: HashMap<Snowflake, Guild>,
    pub guild_order: Vec<Snowflake>,
    pub channels: HashMap<Snowflake, Channel>,
    pub messages: HashMap<Snowflake, MessageBuffer>,
    pub read_states: HashMap<Snowflake, Snowflake>,
    pub voice_states: HashMap<(Option<Snowflake>, Snowflake), VoiceState>,
    pub presences: HashMap<(Option<Snowflake>, Snowflake), Presence>,
    pub relationships: HashMap<Snowflake, Relationship>,
    pub streams: HashMap<String, Stream>,
}

impl State {
    pub fn apply(&mut self, name: &str, data: &Value) -> Vec<StoreEvent> {
        let mut events = Vec::new();
        match name {
            "READY" => self.ready(data, &mut events),
            "GUILD_CREATE" => self.guild_create(data, &mut events),
            "GUILD_UPDATE" => self.guild_update(data, &mut events),
            "GUILD_DELETE" => {
                if let Some(id) = snowflake(data, "id") {
                    self.guilds.remove(&id);
                    self.guild_order.retain(|guild| *guild != id);
                    events.push(StoreEvent::GuildRemoved(id));
                }
            }
            "CHANNEL_CREATE" | "CHANNEL_UPDATE" => {
                if let Ok(channel) = serde_json::from_value::<Channel>(data.clone()) {
                    let id = channel.id;
                    let is_new = self.channels.insert(id, channel.clone()).is_none();
                    self.index_recipients(&channel);
                    events.push(if is_new {
                        StoreEvent::ChannelAdded(id)
                    } else {
                        StoreEvent::ChannelUpdated(id)
                    });
                }
            }
            "CHANNEL_DELETE" => {
                if let Some(id) = snowflake(data, "id") {
                    self.channels.remove(&id);
                    self.messages.remove(&id);
                    events.push(StoreEvent::ChannelRemoved(id));
                }
            }
            "MESSAGE_CREATE" => self.message_create(data, &mut events),
            "MESSAGE_UPDATE" => self.message_update(data, &mut events),
            "MESSAGE_DELETE" => {
                if let (Some(channel_id), Some(id)) =
                    (snowflake(data, "channel_id"), snowflake(data, "id"))
                {
                    if let Some(buffer) = self.messages.get_mut(&channel_id) {
                        buffer.remove(id);
                    }
                    events.push(StoreEvent::MessageDeleted {
                        channel_id,
                        message_id: id,
                    });
                }
            }
            "MESSAGE_DELETE_BULK" => {
                if let Some(channel_id) = snowflake(data, "channel_id")
                    && let Some(ids) = data["ids"].as_array()
                {
                    for value in ids {
                        if let Some(id) = value.as_str().and_then(|s| s.parse().ok()) {
                            if let Some(buffer) = self.messages.get_mut(&channel_id) {
                                buffer.remove(id);
                            }
                            events.push(StoreEvent::MessageDeleted {
                                channel_id,
                                message_id: id,
                            });
                        }
                    }
                }
            }
            "MESSAGE_REACTION_ADD" | "MESSAGE_REACTION_REMOVE" => {
                self.reaction(data, name.ends_with("REMOVE"))
            }
            "MESSAGE_REACTION_REMOVE_ALL" => self.reaction_remove_all(data),
            "TYPING_START" => {
                if let (Some(channel_id), Some(user_id)) =
                    (snowflake(data, "channel_id"), snowflake(data, "user_id"))
                {
                    events.push(StoreEvent::Typing {
                        channel_id,
                        user_id,
                    });
                }
            }
            "PRESENCE_UPDATE" => {
                if let Ok(presence) = serde_json::from_value::<Presence>(data.clone())
                    && let Some(guild_id) = presence.guild_id
                {
                    if let Some(user) = data.pointer("/user") {
                        self.upsert_user_from_value(user);
                    }
                    let key = (Some(guild_id), presence.user_id);
                    let event = StoreEvent::PresenceUpdated {
                        guild_id: Some(guild_id),
                        user_id: presence.user_id,
                    };
                    self.presences.insert(key, presence);
                    events.push(event);
                }
            }
            "RELATIONSHIP_ADD" | "RELATIONSHIP_REMOVE" => {
                if let Ok(relationship) = serde_json::from_value::<Relationship>(data.clone()) {
                    if name.ends_with("REMOVE") {
                        self.relationships.remove(&relationship.id);
                    } else {
                        self.relationships.insert(relationship.id, relationship);
                    }
                    events.push(StoreEvent::RelationshipsUpdated);
                }
            }
            "VOICE_STATE_UPDATE" => {
                if let Ok(voice_state) = serde_json::from_value::<VoiceState>(data.clone()) {
                    let key = voice_state.key();
                    let event = StoreEvent::VoiceStateUpdated {
                        guild_id: voice_state.guild_id,
                        user_id: voice_state.user_id,
                    };
                    self.voice_states.insert(key, voice_state);
                    events.push(event);
                }
            }
            "VOICE_SERVER_UPDATE" => {
                if let Some(guild_id) = snowflake(data, "guild_id") {
                    events.push(StoreEvent::VoiceServerUpdated {
                        guild_id,
                        endpoint: data["endpoint"].as_str().map(str::to_owned),
                    });
                }
            }
            "STREAM_CREATE" | "STREAM_UPDATE" => {
                if let Ok(stream) = serde_json::from_value::<Stream>(data.clone()) {
                    let key = stream.stream_key.clone();
                    self.streams.insert(key.clone(), stream);
                    events.push(StoreEvent::StreamUpdated(key));
                }
            }
            "STREAM_DELETE" => {
                if let Some(key) = data["stream_key"].as_str() {
                    self.streams.remove(key);
                    events.push(StoreEvent::StreamUpdated(key.to_owned()));
                }
            }
            other => {
                tracing::trace!(event = other, "unhandled dispatch");
            }
        }
        events
    }

    fn ready(&mut self, data: &Value, events: &mut Vec<StoreEvent>) {
        if let Ok(user) = serde_json::from_value::<User>(data["user"].clone()) {
            self.users.insert(user.id, user.clone());
            self.me = Some(user);
        }

        if let Some(guilds) = data["guilds"].as_array() {
            for guild in guilds {
                if let Some(id) = snowflake(guild, "id") {
                    if !self.guild_order.contains(&id) {
                        self.guild_order.push(id);
                    }
                    self.guilds.entry(id).or_insert_with(|| Guild {
                        id,
                        name: String::new(),
                        ..Guild::default()
                    });
                }
            }
        }

        if let Some(channels) = data["private_channels"].as_array() {
            for channel in channels {
                if let Ok(channel) = serde_json::from_value::<Channel>(channel.clone()) {
                    self.index_recipients(&channel);
                    self.channels.insert(channel.id, channel);
                }
            }
        }

        if let Some(relationships) = data["relationships"].as_array() {
            for relationship in relationships {
                if let Ok(relationship) =
                    serde_json::from_value::<Relationship>(relationship.clone())
                {
                    self.relationships.insert(relationship.id, relationship);
                }
            }
        }

        if let Some(entries) = data
            .pointer("/read_state/entries")
            .and_then(Value::as_array)
        {
            for entry in entries {
                if let (Some(channel_id), Some(last)) = (
                    snowflake(entry, "id"),
                    entry["last_message_id"]
                        .as_str()
                        .and_then(|s| s.parse().ok()),
                ) {
                    self.read_states.insert(channel_id, last);
                }
            }
        }

        events.push(StoreEvent::Ready);
    }

    fn guild_create(&mut self, data: &Value, events: &mut Vec<StoreEvent>) {
        let Ok(mut guild) = serde_json::from_value::<Guild>(data.clone()) else {
            warn!("could not parse GUILD_CREATE");
            return;
        };
        let id = guild.id;
        let channels = guild.channels.take().unwrap_or_default();

        let is_new = !self.guilds.contains_key(&id);
        if is_new {
            self.guild_order.push(id);
        }
        self.guilds
            .entry(id)
            .and_modify(|existing| {
                existing.name = guild.name.clone();
                existing.icon = guild.icon.clone();
                existing.owner_id = guild.owner_id;
                existing.unavailable = guild.unavailable;
            })
            .or_insert(guild);

        for channel in channels {
            self.index_recipients(&channel);
            self.channels.insert(channel.id, channel);
        }

        if let Some(members) = data["members"].as_array() {
            for member in members {
                if let Some(user) = member.get("user") {
                    self.upsert_user_from_value(user);
                }
            }
        }

        if let Some(presences) = data["presences"].as_array() {
            for presence in presences {
                if let Some(user_id) = snowflake(presence, "user_id") {
                    if let Some(user) = presence.get("user") {
                        self.upsert_user_from_value(user);
                    }
                    if let Ok(presence) = serde_json::from_value::<Presence>(presence.clone()) {
                        self.presences.insert((Some(id), user_id), presence);
                    }
                }
            }
        }

        if let Some(voice_states) = data["voice_states"].as_array() {
            for voice_state in voice_states {
                if let Ok(voice_state) = serde_json::from_value::<VoiceState>(voice_state.clone()) {
                    self.voice_states.insert(voice_state.key(), voice_state);
                }
            }
        }

        events.push(if is_new {
            StoreEvent::GuildAdded(id)
        } else {
            StoreEvent::GuildUpdated(id)
        });
    }

    fn guild_update(&mut self, data: &Value, events: &mut Vec<StoreEvent>) {
        if let Some(id) = snowflake(data, "id")
            && let Some(guild) = self.guilds.get_mut(&id)
        {
            if let Some(name) = data["name"].as_str() {
                guild.name = name.to_owned();
            }
            if let Some(icon) = data.get("icon") {
                guild.icon = icon.as_str().map(str::to_owned);
            }
            events.push(StoreEvent::GuildUpdated(id));
        }
    }

    fn message_create(&mut self, data: &Value, events: &mut Vec<StoreEvent>) {
        let Ok(message) = serde_json::from_value::<Message>(data.clone()) else {
            warn!("could not parse MESSAGE_CREATE");
            return;
        };
        let channel_id = message.channel_id;
        let id = message.id;
        self.upsert_user(message.author.clone());
        self.messages.entry(channel_id).or_default().insert(message);
        if let Some(channel) = self.channels.get_mut(&channel_id) {
            channel.last_message_id = Some(id);
        }
        events.push(StoreEvent::MessageAdded {
            channel_id,
            message_id: id,
        });
    }

    fn message_update(&mut self, data: &Value, events: &mut Vec<StoreEvent>) {
        let (Some(channel_id), Some(id)) = (snowflake(data, "channel_id"), snowflake(data, "id"))
        else {
            return;
        };

        // A full message (e.g. link-preview embed completion) replaces the row.
        if data.get("author").is_some() {
            if let Ok(message) = serde_json::from_value::<Message>(data.clone()) {
                self.upsert_user(message.author.clone());
                self.messages.entry(channel_id).or_default().insert(message);
            }
        } else {
            let Some(buffer) = self.messages.get_mut(&channel_id) else {
                return;
            };
            let updated = buffer.update_with(id, |message| {
                if let Some(content) = data.get("content").and_then(Value::as_str) {
                    message.content = content.to_owned();
                }
                if let Some(edited) = data.get("edited_timestamp") {
                    message.edited_timestamp = edited.as_str().map(str::to_owned);
                }
                if let Some(embeds) = data.get("embeds")
                    && let Ok(embeds) = serde_json::from_value(embeds.clone())
                {
                    message.embeds = embeds;
                }
                if let Some(attachments) = data.get("attachments")
                    && let Ok(attachments) = serde_json::from_value(attachments.clone())
                {
                    message.attachments = attachments;
                }
            });
            if !updated {
                return;
            }
        }

        events.push(StoreEvent::MessageUpdated {
            channel_id,
            message_id: id,
        });
    }

    fn reaction(&mut self, data: &Value, removed: bool) {
        let (Some(channel_id), Some(message_id), Some(user_id)) = (
            snowflake(data, "channel_id"),
            snowflake(data, "message_id"),
            snowflake(data, "user_id"),
        ) else {
            return;
        };
        let Ok(emoji) =
            serde_json::from_value::<crate::message::ReactionEmoji>(data["emoji"].clone())
        else {
            return;
        };
        let me = self.me.as_ref().map(|me| me.id) == Some(user_id);

        let Some(buffer) = self.messages.get_mut(&channel_id) else {
            return;
        };
        buffer.update_with(message_id, |message| {
            let key = |reaction: &Reaction| (reaction.emoji.id, reaction.emoji.name.clone());
            let target = (emoji.id, emoji.name.clone());

            if let Some(reaction) = message
                .reactions
                .iter_mut()
                .find(|reaction| key(reaction) == target)
            {
                if removed {
                    reaction.count = reaction.count.saturating_sub(1);
                    if me {
                        reaction.me = false;
                    }
                } else {
                    reaction.count += 1;
                    if me {
                        reaction.me = true;
                    }
                }
                if reaction.count == 0 {
                    message.reactions.retain(|reaction| key(reaction) != target);
                }
            } else if !removed {
                message.reactions.push(Reaction {
                    count: 1,
                    me,
                    emoji: emoji.clone(),
                });
            }
        });
    }

    fn reaction_remove_all(&mut self, data: &Value) {
        let (Some(channel_id), Some(message_id)) =
            (snowflake(data, "channel_id"), snowflake(data, "message_id"))
        else {
            return;
        };
        if let Some(buffer) = self.messages.get_mut(&channel_id) {
            buffer.update_with(message_id, |message| message.reactions.clear());
        }
    }

    pub fn load_history(&mut self, channel_id: Snowflake, messages: Vec<Message>) {
        for message in &messages {
            self.upsert_user(message.author.clone());
        }
        self.messages
            .entry(channel_id)
            .or_default()
            .extend(messages);
    }

    pub fn mark_read(&mut self, channel_id: Snowflake, message_id: Snowflake) {
        self.read_states.insert(channel_id, message_id);
    }

    fn index_recipients(&mut self, channel: &Channel) {
        if let Some(recipients) = &channel.recipients {
            for user in recipients {
                self.upsert_user(user.clone());
            }
        }
    }

    fn upsert_user(&mut self, user: User) {
        self.users.insert(user.id, user);
    }

    fn upsert_user_from_value(&mut self, value: &Value) {
        if let Ok(user) = serde_json::from_value::<User>(value.clone()) {
            self.upsert_user(user);
        }
    }

    // -- queries ---------------------------------------------------------

    pub fn guild(&self, id: Snowflake) -> Option<&Guild> {
        self.guilds.get(&id)
    }

    pub fn ordered_guilds(&self) -> Vec<&Guild> {
        self.guild_order
            .iter()
            .filter_map(|id| self.guilds.get(id))
            .collect()
    }

    /// Channels of a guild, categories and children interleaved by position.
    pub fn guild_channels(&self, guild_id: Snowflake) -> Vec<&Channel> {
        let mut channels: Vec<&Channel> = self
            .channels
            .values()
            .filter(|channel| channel.guild_id == Some(guild_id))
            .collect();
        channels.sort_by_key(|channel| channel.sort_key());
        channels
    }

    /// DM and group-DM channels, most recent conversation first.
    pub fn dm_channels(&self) -> Vec<&Channel> {
        let mut channels: Vec<&Channel> = self
            .channels
            .values()
            .filter(|channel| {
                matches!(
                    channel.kind,
                    crate::channel::ChannelType::Dm | crate::channel::ChannelType::GroupDm
                )
            })
            .collect();
        channels
            .sort_by_key(|channel| std::cmp::Reverse(channel.last_message_id.map(|id| id.get())));
        channels
    }

    pub fn messages(&self, channel_id: Snowflake) -> Vec<Message> {
        self.messages
            .get(&channel_id)
            .map(|buffer| buffer.messages().to_vec())
            .unwrap_or_default()
    }

    pub fn last_message_id(&self, channel_id: Snowflake) -> Option<Snowflake> {
        self.messages
            .get(&channel_id)
            .and_then(|buffer| buffer.messages().last().map(|message| message.id))
            .or_else(|| {
                self.channels
                    .get(&channel_id)
                    .and_then(|channel| channel.last_message_id)
            })
    }

    pub fn unread(&self, channel_id: Snowflake) -> bool {
        match (
            self.last_message_id(channel_id),
            self.read_states.get(&channel_id).copied(),
        ) {
            (Some(last), Some(read)) => last > read,
            (Some(_), None) => true,
            _ => false,
        }
    }

    pub fn users_in_channel_voice(&self, guild_id: Snowflake, channel_id: Snowflake) -> Vec<User> {
        self.voice_states
            .values()
            .filter(|state| {
                state.guild_id == Some(guild_id) && state.channel_id == Some(channel_id)
            })
            .filter_map(|state| self.users.get(&state.user_id).cloned())
            .collect()
    }

    pub fn guild_voice_states(&self, guild_id: Snowflake) -> Vec<&VoiceState> {
        self.voice_states
            .values()
            .filter(|state| state.guild_id == Some(guild_id))
            .collect()
    }

    pub fn all_user_ids(&self) -> HashSet<Snowflake> {
        self.users.keys().copied().collect()
    }
}

/// Thread-safe handle to the store, with a broadcast of change events.
#[derive(Clone)]
pub struct Store {
    inner: Arc<RwLock<State>>,
    tx: broadcast::Sender<StoreEvent>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(512);
        Self {
            inner: Arc::new(RwLock::new(State::default())),
            tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StoreEvent> {
        self.tx.subscribe()
    }

    pub fn read<R>(&self, f: impl FnOnce(&State) -> R) -> R {
        f(&self.inner.read())
    }

    pub fn write<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.inner.write())
    }

    /// Applies one dispatch payload and publishes the resulting changes.
    pub fn apply(&self, name: &str, data: &Value) {
        let events = {
            let mut state = self.inner.write();
            state.apply(name, data)
        };
        for event in events {
            let _ = self.tx.send(event);
        }
    }

    /// Merges the channel list returned by `GET /guilds/{id}/channels` into the
    /// gateway cache. Gateway READY guilds can be partial and omit channels.
    pub fn load_guild_channels(&self, guild_id: Snowflake, channels: Vec<Channel>) {
        let events = {
            let mut state = self.inner.write();
            let mut events = Vec::with_capacity(channels.len());
            for mut channel in channels {
                if channel.guild_id.is_none() {
                    channel.guild_id = Some(guild_id);
                }
                let id = channel.id;
                let is_new = state.channels.insert(id, channel.clone()).is_none();
                if let Some(recipients) = &channel.recipients {
                    for user in recipients {
                        state.users.insert(user.id, user.clone());
                    }
                }
                events.push(if is_new {
                    StoreEvent::ChannelAdded(id)
                } else {
                    StoreEvent::ChannelUpdated(id)
                });
            }
            events
        };
        for event in events {
            let _ = self.tx.send(event);
        }
        let _ = self.tx.send(StoreEvent::GuildChannelsLoaded(guild_id));
    }
    pub fn load_history(&self, channel_id: Snowflake, messages: Vec<Message>) {
        {
            let mut state = self.inner.write();
            state.load_history(channel_id, messages);
        }
        let _ = self.tx.send(StoreEvent::HistoryLoaded(channel_id));
    }

    pub fn mark_read(&self, channel_id: Snowflake, message_id: Snowflake) {
        self.inner.write().mark_read(channel_id, message_id);
    }
}

fn snowflake(value: &Value, key: &str) -> Option<Snowflake> {
    match value.get(key)? {
        Value::String(text) => text.parse().ok(),
        Value::Number(number) => number.as_u64().map(Snowflake),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(id: u64, channel: u64, content: &str) -> Value {
        json!({
            "id": id.to_string(),
            "channel_id": channel.to_string(),
            "author": {"id": "1", "username": "kyle"},
            "content": content,
            "timestamp": "2026-11-12T00:00:00.000000+00:00",
            "type": 0
        })
    }

    #[test]
    fn message_create_updates_window_and_channel() {
        let store = Store::new();
        store.apply(
            "CHANNEL_CREATE",
            &json!({"id": "10", "type": 0, "guild_id": "5", "name": "general", "position": 1}),
        );
        store.apply("MESSAGE_CREATE", &message(100, 10, "hello"));
        store.apply("MESSAGE_CREATE", &message(101, 10, "world"));

        store.read(|state| {
            let messages = state.messages(Snowflake(10));
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[1].content, "world");
            assert_eq!(
                state.channels[&Snowflake(10)].last_message_id,
                Some(Snowflake(101))
            );
        });
    }

    #[test]
    fn message_update_merges_partial_payload() {
        let store = Store::new();
        store.apply("MESSAGE_CREATE", &message(100, 10, "original"));
        store.apply(
            "MESSAGE_UPDATE",
            &json!({"id": "100", "channel_id": "10", "content": "edited", "edited_timestamp": "2026-11-12T00:00:01.000000+00:00"}),
        );

        store.read(|state| {
            let messages = state.messages(Snowflake(10));
            assert_eq!(messages[0].content, "edited");
            assert!(messages[0].edited_timestamp.is_some());
        });
    }

    #[test]
    fn reactions_increment_and_decrement() {
        let store = Store::new();
        store.apply("MESSAGE_CREATE", &message(100, 10, "hello"));
        store.apply(
            "MESSAGE_REACTION_ADD",
            &json!({"channel_id": "10", "message_id": "100", "user_id": "1", "emoji": {"name": "👍"}}),
        );
        store.read(|state| {
            let messages = state.messages(Snowflake(10));
            assert_eq!(messages[0].reactions.len(), 1);
            assert_eq!(messages[0].reactions[0].count, 1);
        });
        store.apply(
            "MESSAGE_REACTION_REMOVE",
            &json!({"channel_id": "10", "message_id": "100", "user_id": "1", "emoji": {"name": "👍"}}),
        );
        store.read(|state| {
            let messages = state.messages(Snowflake(10));
            assert!(messages[0].reactions.is_empty());
        });
    }

    #[test]
    fn history_merges_in_id_order_without_duplicates() {
        let mut state = State::default();
        state.load_history(
            Snowflake(10),
            vec![
                serde_json::from_value(message(102, 10, "c")).unwrap(),
                serde_json::from_value(message(101, 10, "b")).unwrap(),
            ],
        );
        state.load_history(
            Snowflake(10),
            vec![
                serde_json::from_value(message(101, 10, "b")).unwrap(),
                serde_json::from_value(message(100, 10, "a")).unwrap(),
            ],
        );
        let messages = state.messages(Snowflake(10));
        assert_eq!(
            messages.iter().map(|m| m.id.0).collect::<Vec<_>>(),
            vec![100, 101, 102]
        );
    }

    #[test]
    fn unread_tracks_read_state() {
        let store = Store::new();
        store.apply("MESSAGE_CREATE", &message(100, 10, "hi"));
        assert!(store.read(|state| state.unread(Snowflake(10))));
        store.mark_read(Snowflake(10), Snowflake(100));
        assert!(!store.read(|state| state.unread(Snowflake(10))));
    }

    #[test]
    fn rest_channel_refresh_populates_partial_guild_and_updates_existing() {
        let store = Store::new();
        store.apply(
            "READY",
            &json!({
                "user": {"id": "1", "username": "kyle"},
                "guilds": [{"id": "5", "unavailable": false}]
            }),
        );
        let channels = vec![
            serde_json::from_value(
                json!({"id":"10", "type":4, "name":"Text Channels", "position":0}),
            )
            .unwrap(),
            serde_json::from_value(json!({"id":"11", "type":0, "name":"general", "position":1}))
                .unwrap(),
            serde_json::from_value(json!({"id":"12", "type":2, "name":"General", "position":2}))
                .unwrap(),
        ];
        store.load_guild_channels(Snowflake(5), channels);
        store.read(|state| {
            let channels = state.guild_channels(Snowflake(5));
            assert_eq!(channels.len(), 3);
            assert_eq!(channels[1].display_name(), "general");
            assert_eq!(channels[2].display_name(), "General");
            assert!(
                channels
                    .iter()
                    .all(|channel| channel.guild_id == Some(Snowflake(5)))
            );
        });

        store.load_guild_channels(
            Snowflake(5),
            vec![
                serde_json::from_value(json!({
                    "id":"11", "type":0, "name":"general-renamed", "position":1
                }))
                .unwrap(),
            ],
        );
        store.read(|state| {
            assert_eq!(state.guild_channels(Snowflake(5)).len(), 3);
            assert_eq!(
                state.channels[&Snowflake(11)].display_name(),
                "general-renamed"
            );
        });
    }
}
