use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kc_core::Snowflake;
use parking_lot::RwLock;
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::warn;

use crate::channel::Channel;
use crate::guild::{Emoji, Guild, Member, Presence, Relationship, Role, Stream, VoiceState};
use crate::member_list::MemberList;
use crate::message::{Message, Reaction, SendState};
use crate::settings::{CustomStatus, GuildFolder, GuildSettings, ReadState};
use crate::user::User;

/// How many messages a channel keeps loaded when [`UNLOAD_DISTANT`] is on. The
/// window slides as you scroll: older pages load in, the distant ones at the
/// far end are dropped (and reloaded if you scroll back to them).
const MESSAGE_WINDOW: usize = 2000;

/// Whether to drop distant messages to cap memory (the default), or keep every
/// message scrolled into view. Set from the user's settings.
static UNLOAD_DISTANT: AtomicBool = AtomicBool::new(true);

/// Turns distant-message unloading on or off.
pub fn set_unload_distant(on: bool) {
    UNLOAD_DISTANT.store(on, Ordering::Relaxed);
}

fn unload_distant() -> bool {
    UNLOAD_DISTANT.load(Ordering::Relaxed)
}

/// A nonce as a plain string, whether Discord echoed it as a string or a
/// number, for matching an optimistic message to its confirmation.
fn nonce_str(nonce: &Option<Value>) -> Option<String> {
    match nonce.as_ref()? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// A per-channel, id-ordered window of the most recent messages.
#[derive(Debug)]
pub struct MessageBuffer {
    messages: Vec<Message>,
    limit: usize,
    /// The window was loaded around an older message and does not reach the
    /// channel's newest one; new messages are not added until it does, so the
    /// window never has a gap in it.
    detached: bool,
}

impl Default for MessageBuffer {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            limit: MESSAGE_WINDOW,
            detached: false,
        }
    }
}

impl MessageBuffer {
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Whether the window stops short of the channel's newest message.
    pub fn is_detached(&self) -> bool {
        self.detached
    }

    pub fn set_detached(&mut self, detached: bool) {
        self.detached = detached;
    }

    /// Replaces the window with a page loaded around an older message.
    /// `newest` is the channel's newest message id, when known.
    pub fn replace_around(&mut self, mut messages: Vec<Message>, newest: Option<Snowflake>) {
        messages.sort_by_key(|message| message.id);
        messages.dedup_by_key(|message| message.id);
        self.detached = match (messages.last(), newest) {
            (Some(last), Some(newest)) => last.id < newest,
            // Without a known newest id, a full page may have more after it.
            _ => false,
        };
        self.messages = messages;
    }

    /// Merges a page of messages newer than the window. A short page means
    /// the window has reached the present.
    pub fn extend_newer(&mut self, messages: Vec<Message>, full_page: bool) {
        self.detached = full_page;
        self.extend(messages);
        // Newer messages came in at the bottom; the distant ones to drop are
        // the oldest, at the top.
        self.cap_oldest();
    }

    /// Caps the window by dropping the oldest messages (the top). Used when
    /// the bottom grows — a new live message or a newer page.
    fn cap_oldest(&mut self) {
        if unload_distant() && self.messages.len() > self.limit {
            let excess = self.messages.len() - self.limit;
            self.messages.drain(..excess);
        }
    }

    /// Caps the window by dropping the newest messages (the bottom), which
    /// detaches it from the present. Used when scrolling far back: older pages
    /// grow the top, so the distant ones to drop are the newest.
    fn cap_newest(&mut self) {
        if unload_distant() && self.messages.len() > self.limit {
            self.messages.truncate(self.limit);
            self.detached = true;
        }
    }

    pub fn get(&self, id: Snowflake) -> Option<&Message> {
        self.messages
            .binary_search_by_key(&id, |message| message.id)
            .ok()
            .map(|index| &self.messages[index])
    }

    /// Inserts or replaces a message. A new live message arrives at the
    /// bottom, so the window is kept bounded by dropping the oldest.
    pub fn insert(&mut self, message: Message) {
        match self
            .messages
            .binary_search_by_key(&message.id, |existing| existing.id)
        {
            Ok(index) => self.messages[index] = message,
            Err(index) => self.messages.insert(index, message),
        }
        self.cap_oldest();
    }

    /// Adds a message shown before Discord confirms it. Its synthetic id
    /// sorts after every real message, so it appears at the bottom.
    pub fn insert_pending(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// Removes and returns the id of the optimistic message with this nonce,
    /// if one is still pending.
    pub fn take_pending(&mut self, nonce: &str) -> Option<Snowflake> {
        let index = self.messages.iter().position(|message| {
            message.send_state != SendState::Sent
                && nonce_str(&message.nonce).as_deref() == Some(nonce)
        })?;
        Some(self.messages.remove(index).id)
    }

    /// Sets the send progress of a pending message, found by nonce.
    pub fn set_send_state(&mut self, nonce: &str, state: SendState) -> Option<Snowflake> {
        let message = self.messages.iter_mut().find(|message| {
            message.send_state != SendState::Sent
                && nonce_str(&message.nonce).as_deref() == Some(nonce)
        })?;
        message.send_state = state;
        Some(message.id)
    }

    /// Merges a page of messages, deduplicating by id. Does not cap; the
    /// caller caps the correct end for its direction.
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
    /// Someone played a soundboard sound in a voice channel (we are in it).
    SoundboardPlayed {
        channel_id: Snowflake,
        user_id: Snowflake,
        sound_id: Snowflake,
        volume: f64,
    },
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
    GuildMembersUpdated(Snowflake),
    HistoryLoaded(Snowflake),
    /// Unread state or mention count of a channel changed.
    ReadStateUpdated(Snowflake),
    /// Guild folders, mutes, or the account's own status changed.
    SettingsUpdated,
    /// The lazily loaded member list of a guild changed.
    MemberListUpdated(Snowflake),
    /// Someone was banned from or unbanned in a guild (bans are not
    /// cached; a bans list should fetch again).
    BansUpdated(Snowflake),
}

#[derive(Default)]
pub struct State {
    pub me: Option<User>,
    /// The gateway session's ID from READY (the website sends it when
    /// joining a server).
    pub session_id: Option<String>,
    pub users: HashMap<Snowflake, User>,
    pub guilds: HashMap<Snowflake, Guild>,
    /// Guild ids in the order READY/GUILD_CREATE delivered them.
    pub guild_order: Vec<Snowflake>,
    pub channels: HashMap<Snowflake, Channel>,
    pub messages: HashMap<Snowflake, MessageBuffer>,
    pub read_states: HashMap<Snowflake, ReadState>,
    pub voice_states: HashMap<(Option<Snowflake>, Snowflake), VoiceState>,
    /// The latest presence seen for each user, from any guild or friendship.
    pub presences: HashMap<Snowflake, Presence>,
    pub relationships: HashMap<Snowflake, Relationship>,
    pub streams: HashMap<String, Stream>,
    /// Guild id -> member user ids, seeded by READY/GUILD_CREATE and refreshed
    /// by GUILD_MEMBER_LIST_UPDATE.
    pub guild_members: HashMap<Snowflake, HashSet<Snowflake>>,
    /// (guild id, user id) -> membership details (nickname, roles).
    pub members: HashMap<(Snowflake, Snowflake), Member>,
    /// Notification settings per guild; `None` holds the DM settings.
    pub guild_settings: HashMap<Option<Snowflake>, GuildSettings>,
    pub guild_folders: Vec<GuildFolder>,
    /// Guild id -> its member lists, one per audience (list id). A guild can
    /// have several when some channels restrict who can view them; keeping them
    /// separate stops one audience's update from wiping another's rows.
    pub member_lists: HashMap<Snowflake, HashMap<String, MemberList>>,
    /// The account's own status: `online`, `idle`, `dnd` or `invisible`.
    pub status: Option<String>,
    pub custom_status: Option<CustomStatus>,
}

impl State {
    pub fn apply(&mut self, name: &str, data: &Value) -> Vec<StoreEvent> {
        let mut events = Vec::new();
        match name {
            "READY" => self.ready(data, &mut events),
            "READY_SUPPLEMENTAL" => self.ready_supplemental(data, &mut events),
            "GUILD_CREATE" => self.guild_create(data, &mut events),
            "GUILD_UPDATE" => self.guild_update(data, &mut events),
            "GUILD_DELETE" => {
                if let Some(id) = snowflake(data, "id") {
                    self.guilds.remove(&id);
                    self.guild_order.retain(|guild| *guild != id);
                    self.channels
                        .retain(|_, channel| channel.guild_id != Some(id));
                    self.member_lists.remove(&id);
                    events.push(StoreEvent::GuildRemoved(id));
                }
            }
            "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "THREAD_CREATE" | "THREAD_UPDATE" => {
                self.channel_upsert(data, &mut events);
            }
            "CHANNEL_DELETE" | "THREAD_DELETE" => {
                if let Some(id) = snowflake(data, "id") {
                    self.channels.remove(&id);
                    self.messages.remove(&id);
                    events.push(StoreEvent::ChannelRemoved(id));
                }
            }
            "THREAD_LIST_SYNC" => {
                if let (Some(guild_id), Some(threads)) =
                    (snowflake(data, "guild_id"), data["threads"].as_array())
                {
                    for thread in threads {
                        if let Ok(mut channel) = serde_json::from_value::<Channel>(thread.clone()) {
                            channel.guild_id = Some(guild_id);
                            let id = channel.id;
                            // The sync omits `member`; keep what we know.
                            if channel.member.is_none() {
                                channel.member =
                                    self.channels.get(&id).and_then(|old| old.member.clone());
                            }
                            self.channels.insert(id, channel);
                        }
                    }
                    events.push(StoreEvent::GuildChannelsLoaded(guild_id));
                }
            }
            "CHANNEL_RECIPIENT_ADD" | "CHANNEL_RECIPIENT_REMOVE" => {
                if let (Some(channel_id), Ok(user)) = (
                    snowflake(data, "channel_id"),
                    serde_json::from_value::<User>(data["user"].clone()),
                ) {
                    let removed = name.ends_with("REMOVE");
                    if !removed {
                        self.upsert_user(user.clone());
                    }
                    if let Some(channel) = self.channels.get_mut(&channel_id) {
                        let recipients = channel.recipients.get_or_insert_with(Vec::new);
                        recipients.retain(|existing| existing.id != user.id);
                        if !removed {
                            recipients.push(user);
                        }
                        events.push(StoreEvent::ChannelUpdated(channel_id));
                    }
                }
            }
            "CHANNEL_UNREAD_UPDATE" => {
                if let Some(updates) = data["channel_unread_updates"].as_array() {
                    for update in updates {
                        if let (Some(id), Some(last)) = (
                            snowflake(update, "id"),
                            snowflake(update, "last_message_id"),
                        ) && let Some(channel) = self.channels.get_mut(&id)
                        {
                            channel.last_message_id = Some(last);
                            events.push(StoreEvent::ReadStateUpdated(id));
                        }
                    }
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
            "MESSAGE_ACK" => {
                if let Some(channel_id) = snowflake(data, "channel_id") {
                    let state = self.read_states.entry(channel_id).or_default();
                    if let Some(message_id) = snowflake(data, "message_id") {
                        state.last_message_id = Some(message_id);
                    }
                    // An omitted count means "keep"; a plain ack clears it.
                    state.mention_count = match data["mention_count"].as_u64() {
                        Some(count) => count as u32,
                        None if data["manual"].as_bool() == Some(true) => state.mention_count,
                        None => 0,
                    };
                    events.push(StoreEvent::ReadStateUpdated(channel_id));
                }
            }
            "MESSAGE_REACTION_ADD" | "MESSAGE_REACTION_REMOVE" => {
                self.reaction(data, name.ends_with("REMOVE"), &mut events)
            }
            // DEBOUNCE_MESSAGE_REACTIONS: several adds in one event, each
            // emoji with the users who added it.
            "MESSAGE_REACTION_ADD_MANY" => {
                for reaction in data["reactions"].as_array().into_iter().flatten() {
                    for user in reaction["users"].as_array().into_iter().flatten() {
                        let single = serde_json::json!({
                            "channel_id": data["channel_id"],
                            "message_id": data["message_id"],
                            "guild_id": data["guild_id"],
                            "user_id": user,
                            "emoji": reaction["emoji"],
                            "type": reaction.get("type").or_else(|| reaction.get("reactionType")).cloned().unwrap_or(Value::from(0)),
                        });
                        self.reaction(&single, false, &mut events);
                    }
                }
            }
            // PASSIVE_GUILD_UPDATE_V2: for servers without a subscription,
            // channel activity and voice changes arrive batched here instead
            // of as CHANNEL_UNREAD_UPDATE and VOICE_STATE_UPDATE.
            "PASSIVE_UPDATE_V2" => {
                let guild_id = data["guild_id"].clone();
                let updates = serde_json::json!({ "channel_unread_updates": data["updated_channels"] });
                events.extend(self.apply("CHANNEL_UNREAD_UPDATE", &updates));
                for member in data["updated_members"].as_array().into_iter().flatten() {
                    if let Some(guild) = snowflake(data, "guild_id") {
                        if let Some(user) = member.get("user") {
                            self.upsert_user_from_value(user);
                        }
                        self.upsert_member(guild, member);
                    }
                }
                for state in data["updated_voice_states"].as_array().into_iter().flatten() {
                    let mut state = state.clone();
                    state["guild_id"] = guild_id.clone();
                    events.extend(self.apply("VOICE_STATE_UPDATE", &state));
                }
                for user in data["removed_voice_states"].as_array().into_iter().flatten() {
                    let left = serde_json::json!({ "guild_id": guild_id, "user_id": user, "channel_id": null });
                    events.extend(self.apply("VOICE_STATE_UPDATE", &left));
                }
            }
            // A vote in a poll, by anyone (ours too, echoed back).
            "MESSAGE_POLL_VOTE_ADD" | "MESSAGE_POLL_VOTE_REMOVE" => {
                let added = name.ends_with("ADD");
                if let (Some(channel_id), Some(message_id), Some(user_id), Some(answer)) = (
                    snowflake(data, "channel_id"),
                    snowflake(data, "message_id"),
                    snowflake(data, "user_id"),
                    data["answer_id"].as_u64(),
                ) {
                    let mine = self.me.as_ref().map(|me| me.id) == Some(user_id);
                    if let Some(buffer) = self.messages.get_mut(&channel_id) {
                        let updated = buffer.update_with(message_id, |message| {
                            if let Some(poll) = &mut message.poll {
                                poll.apply_vote(answer as u32, added, mine);
                            }
                        });
                        if updated {
                            events.push(StoreEvent::MessageUpdated { channel_id, message_id });
                        }
                    }
                }
            }
            "MESSAGE_REACTION_REMOVE_ALL" | "MESSAGE_REACTION_REMOVE_EMOJI" => {
                self.reaction_clear(data, &mut events)
            }
            "TYPING_START" => {
                if let (Some(channel_id), Some(user_id)) =
                    (snowflake(data, "channel_id"), snowflake(data, "user_id"))
                {
                    if let (Some(guild_id), Some(member)) =
                        (snowflake(data, "guild_id"), data.get("member"))
                    {
                        self.upsert_member(guild_id, member);
                    }
                    events.push(StoreEvent::Typing {
                        channel_id,
                        user_id,
                    });
                }
            }
            "GUILD_MEMBER_ADD" | "GUILD_MEMBER_UPDATE" => {
                if let Some(guild_id) = snowflake(data, "guild_id")
                    && self.upsert_member(guild_id, data).is_some()
                {
                    events.push(StoreEvent::GuildMembersUpdated(guild_id));
                }
            }
            "GUILD_MEMBER_REMOVE" => {
                if let (Some(guild_id), Some(user_id)) = (
                    snowflake(data, "guild_id"),
                    data.pointer("/user/id")
                        .and_then(Value::as_str)
                        .and_then(|id| id.parse().ok())
                        .or_else(|| snowflake(data, "user_id")),
                ) {
                    if let Some(members) = self.guild_members.get_mut(&guild_id) {
                        members.remove(&user_id);
                    }
                    self.members.remove(&(guild_id, user_id));
                    events.push(StoreEvent::GuildMembersUpdated(guild_id));
                }
            }
            "GUILD_MEMBERS_CHUNK" => {
                if let Some(guild_id) = snowflake(data, "guild_id") {
                    for member in data["members"].as_array().into_iter().flatten() {
                        self.upsert_member(guild_id, member);
                    }
                    for presence in data["presences"].as_array().into_iter().flatten() {
                        self.upsert_presence(presence);
                    }
                    events.push(StoreEvent::GuildMembersUpdated(guild_id));
                }
            }
            "GUILD_MEMBER_LIST_UPDATE" => self.member_list_update(data, &mut events),
            "GUILD_BAN_ADD" | "GUILD_BAN_REMOVE" => {
                if let Some(guild_id) = snowflake(data, "guild_id") {
                    events.push(StoreEvent::BansUpdated(guild_id));
                }
            }
            "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" => {
                if let (Some(guild_id), Ok(role)) = (
                    snowflake(data, "guild_id"),
                    serde_json::from_value::<Role>(data["role"].clone()),
                ) && let Some(guild) = self.guilds.get_mut(&guild_id)
                {
                    guild.roles.retain(|existing| existing.id != role.id);
                    guild.roles.push(role);
                    events.push(StoreEvent::GuildUpdated(guild_id));
                }
            }
            "GUILD_ROLE_DELETE" => {
                if let (Some(guild_id), Some(role_id)) =
                    (snowflake(data, "guild_id"), snowflake(data, "role_id"))
                    && let Some(guild) = self.guilds.get_mut(&guild_id)
                {
                    guild.roles.retain(|role| role.id != role_id);
                    events.push(StoreEvent::GuildUpdated(guild_id));
                }
            }
            "GUILD_EMOJIS_UPDATE" => {
                if let (Some(guild_id), Ok(emojis)) = (
                    snowflake(data, "guild_id"),
                    serde_json::from_value::<Vec<Emoji>>(data["emojis"].clone()),
                ) && let Some(guild) = self.guilds.get_mut(&guild_id)
                {
                    guild.emojis = emojis;
                    events.push(StoreEvent::GuildUpdated(guild_id));
                }
            }
            // A server's soundboard: all of it, or one sound.
            "SOUNDBOARD_SOUNDS" | "GUILD_SOUNDBOARD_SOUNDS_UPDATE" => {
                if let (Some(guild_id), Ok(sounds)) = (
                    snowflake(data, "guild_id"),
                    serde_json::from_value::<Vec<crate::guild::SoundboardSound>>(data["soundboard_sounds"].clone()),
                ) && let Some(guild) = self.guilds.get_mut(&guild_id)
                {
                    guild.soundboard_sounds = sounds;
                    events.push(StoreEvent::GuildUpdated(guild_id));
                }
            }
            "GUILD_SOUNDBOARD_SOUND_CREATE" | "GUILD_SOUNDBOARD_SOUND_UPDATE" | "GUILD_SOUNDBOARD_SOUND_DELETE" => {
                if let (Some(guild_id), Some(sound_id)) = (snowflake(data, "guild_id"), snowflake(data, "sound_id"))
                    && let Some(guild) = self.guilds.get_mut(&guild_id)
                {
                    let at = guild.soundboard_sounds.iter().position(|sound| sound.sound_id == sound_id);
                    if name.ends_with("DELETE") {
                        if let Some(at) = at {
                            guild.soundboard_sounds.remove(at);
                        }
                    } else if let Ok(sound) = serde_json::from_value::<crate::guild::SoundboardSound>(data.clone()) {
                        match at {
                            Some(at) => guild.soundboard_sounds[at] = sound,
                            None => guild.soundboard_sounds.push(sound),
                        }
                    }
                    events.push(StoreEvent::GuildUpdated(guild_id));
                }
            }
            "VOICE_CHANNEL_EFFECT_SEND" => {
                if let (Some(channel_id), Some(user_id), Some(sound_id)) =
                    (snowflake(data, "channel_id"), snowflake(data, "user_id"), snowflake(data, "sound_id"))
                {
                    events.push(StoreEvent::SoundboardPlayed {
                        channel_id,
                        user_id,
                        sound_id,
                        volume: data["sound_volume"].as_f64().unwrap_or(1.0),
                    });
                }
            }
            // A stage went live, changed its topic, or ended.
            "STAGE_INSTANCE_CREATE" | "STAGE_INSTANCE_UPDATE" | "STAGE_INSTANCE_DELETE" => {
                if let Ok(stage) = serde_json::from_value::<crate::guild::StageInstance>(data.clone())
                    && let Some(guild_id) = stage.guild_id
                    && let Some(guild) = self.guilds.get_mut(&guild_id)
                {
                    let channel_id = stage.channel_id;
                    guild.stage_instances.retain(|live| live.channel_id != channel_id);
                    if name != "STAGE_INSTANCE_DELETE" {
                        guild.stage_instances.push(stage);
                    }
                    events.push(StoreEvent::GuildUpdated(guild_id));
                    events.push(StoreEvent::ChannelUpdated(channel_id));
                }
            }
            "GUILD_STICKERS_UPDATE" => {
                if let (Some(guild_id), Ok(stickers)) = (
                    snowflake(data, "guild_id"),
                    serde_json::from_value::<Vec<crate::guild::Sticker>>(data["stickers"].clone()),
                ) && let Some(guild) = self.guilds.get_mut(&guild_id)
                {
                    guild.stickers = stickers;
                    events.push(StoreEvent::GuildUpdated(guild_id));
                }
            }
            "PRESENCE_UPDATE" => {
                if let Some(user) = data.get("user")
                    && user.get("username").is_some()
                {
                    self.upsert_user_from_value(user);
                }
                if let Some((guild_id, user_id)) = self.upsert_presence(data) {
                    events.push(StoreEvent::PresenceUpdated { guild_id, user_id });
                }
            }
            "RELATIONSHIP_ADD" | "RELATIONSHIP_UPDATE" => {
                if let Some(user) = data.get("user") {
                    self.upsert_user_from_value(user);
                }
                if let Ok(relationship) = serde_json::from_value::<Relationship>(data.clone()) {
                    self.relationships.insert(relationship.id, relationship);
                    events.push(StoreEvent::RelationshipsUpdated);
                }
            }
            "RELATIONSHIP_REMOVE" => {
                if let Some(id) = snowflake(data, "id") {
                    self.relationships.remove(&id);
                    events.push(StoreEvent::RelationshipsUpdated);
                }
            }
            "USER_UPDATE" => {
                if let Ok(user) = serde_json::from_value::<User>(data.clone()) {
                    if self.me.as_ref().is_some_and(|me| me.id == user.id) {
                        self.me = Some(user.clone());
                    }
                    self.upsert_user(user);
                    events.push(StoreEvent::SettingsUpdated);
                }
            }
            "USER_SETTINGS_UPDATE" => {
                self.user_settings(data);
                events.push(StoreEvent::SettingsUpdated);
            }
            // With USER_SETTINGS_PROTO, settings changes come as protobuf;
            // type 1 is `PreloadedUserSettings`.
            "USER_SETTINGS_PROTO_UPDATE" => {
                if data.pointer("/settings/type").and_then(Value::as_u64) == Some(1)
                    && let Some(proto) = data.pointer("/settings/proto").and_then(Value::as_str)
                {
                    let partial = data["partial"].as_bool().unwrap_or(false);
                    self.user_settings_proto(proto, partial);
                    events.push(StoreEvent::SettingsUpdated);
                }
            }
            "USER_GUILD_SETTINGS_UPDATE" => {
                let (guild_id, settings) = GuildSettings::parse(data);
                self.guild_settings.insert(guild_id, settings);
                events.push(StoreEvent::SettingsUpdated);
            }
            "SESSIONS_REPLACE" => {
                if self.sessions(data) {
                    events.push(StoreEvent::SettingsUpdated);
                }
            }
            "VOICE_STATE_UPDATE" => {
                // Guild voice states carry a member object for users we have
                // not otherwise seen; use it so the channel occupant list can
                // render names instead of dropping the entry.
                if let Some(user) = data.pointer("/member/user") {
                    self.upsert_user_from_value(user);
                }
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
            // A DM call's participants: CALL_CREATE lists who is in it, and
            // CALL_DELETE ends it. Discord only sends voice state updates
            // to those in the call, so someone who left before the others
            // relies on CALL_DELETE to clear them.
            "CALL_CREATE" => {
                for value in data["voice_states"].as_array().into_iter().flatten() {
                    if let Ok(voice_state) = serde_json::from_value::<VoiceState>(value.clone()) {
                        events.push(StoreEvent::VoiceStateUpdated {
                            guild_id: voice_state.guild_id,
                            user_id: voice_state.user_id,
                        });
                        self.voice_states.insert(voice_state.key(), voice_state);
                    }
                }
            }
            "CALL_DELETE" => {
                if let Some(channel_id) = snowflake(data, "channel_id") {
                    let gone: Vec<(Option<Snowflake>, Snowflake)> = self
                        .voice_states
                        .iter()
                        .filter(|(_, voice)| {
                            voice.guild_id.is_none() && voice.channel_id == Some(channel_id)
                        })
                        .map(|(key, _)| *key)
                        .collect();
                    for key in gone {
                        self.voice_states.remove(&key);
                        events.push(StoreEvent::VoiceStateUpdated {
                            guild_id: None,
                            user_id: key.1,
                        });
                    }
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
        self.session_id = data["session_id"].as_str().map(str::to_owned);
        if let Ok(user) = serde_json::from_value::<User>(data["user"].clone()) {
            self.users.insert(user.id, user.clone());
            self.me = Some(user);
        }

        // With DEDUPE_USER_OBJECTS every other user object in READY is
        // replaced by an id that refers into this array.
        for user in data["users"].as_array().into_iter().flatten() {
            self.upsert_user_from_value(user);
        }

        if let Some(guilds) = data["guilds"].as_array() {
            for guild in guilds {
                let Some(id) = snowflake(guild, "id") else {
                    continue;
                };
                if !self.guild_order.contains(&id) {
                    self.guild_order.push(id);
                }

                if guild["unavailable"].as_bool().unwrap_or(false) {
                    self.guilds.entry(id).or_insert_with(|| Guild {
                        id,
                        name: String::new(),
                        unavailable: true,
                        ..Guild::default()
                    });
                    continue;
                }

                let Ok(mut parsed) = serde_json::from_value::<Guild>(with_properties(guild)) else {
                    self.guilds.entry(id).or_insert_with(|| Guild {
                        id,
                        name: String::new(),
                        ..Guild::default()
                    });
                    continue;
                };
                if let Some(channels) = parsed.channels.take() {
                    self.merge_guild_channels(id, channels);
                }
                for thread in guild["threads"].as_array().into_iter().flatten() {
                    if let Ok(thread) = serde_json::from_value::<Channel>(thread.clone()) {
                        self.merge_guild_channels(id, vec![thread]);
                    }
                }
                self.guilds.insert(id, parsed);
            }
        }

        self.merged_members(data);

        if let Some(channels) = data["private_channels"].as_array() {
            for channel in channels {
                if let Ok(mut channel) = serde_json::from_value::<Channel>(channel.clone()) {
                    self.resolve_recipients(&mut channel);
                    self.index_recipients(&channel);
                    self.channels.insert(channel.id, channel);
                }
            }
        }

        if let Some(relationships) = data["relationships"].as_array() {
            for relationship in relationships {
                if let Some(user) = relationship.get("user") {
                    self.upsert_user_from_value(user);
                }
                if let Ok(relationship) =
                    serde_json::from_value::<Relationship>(relationship.clone())
                {
                    self.relationships.insert(relationship.id, relationship);
                }
            }
        }

        // `read_state` is `{entries: [...]}` with VERSIONED_READ_STATES and a
        // plain array without it.
        let entries = data
            .pointer("/read_state/entries")
            .or_else(|| data.get("read_state"))
            .and_then(Value::as_array);
        for entry in entries.into_iter().flatten() {
            // Non-channel read states (events, onboarding, ...) carry a type.
            if entry["read_state_type"].as_u64().unwrap_or(0) != 0 {
                continue;
            }
            if let Some(channel_id) = snowflake(entry, "id") {
                self.read_states.insert(
                    channel_id,
                    ReadState {
                        last_message_id: snowflake(entry, "last_message_id")
                            .filter(|id| id.get() != 0),
                        mention_count: entry["mention_count"].as_u64().unwrap_or(0) as u32,
                    },
                );
            }
        }

        let settings = data
            .pointer("/user_guild_settings/entries")
            .or_else(|| data.get("user_guild_settings"))
            .and_then(Value::as_array);
        for entry in settings.into_iter().flatten() {
            let (guild_id, settings) = GuildSettings::parse(entry);
            self.guild_settings.insert(guild_id, settings);
        }

        if let Some(settings) = data.get("user_settings") {
            self.user_settings(settings);
        }
        if let Some(proto) = data["user_settings_proto"].as_str() {
            self.user_settings_proto(proto, false);
        }
        if let Some(sessions) = data.get("sessions") {
            self.sessions(sessions);
        }

        // Without DEDUPE_USER_OBJECTS presences arrive here; with it they are
        // in READY_SUPPLEMENTAL.
        for presence in data["presences"].as_array().into_iter().flatten() {
            self.upsert_presence(presence);
        }

        events.push(StoreEvent::Ready);
    }

    /// `merged_members[i]` lists the members of `guilds[i]` that READY wants
    /// the client to know (the account itself, friends, voice participants).
    fn merged_members(&mut self, data: &Value) {
        let Some(merged) = data["merged_members"].as_array() else {
            return;
        };
        for (index, members) in merged.iter().enumerate() {
            let Some(guild_id) = data["guilds"]
                .get(index)
                .and_then(|guild| snowflake(guild, "id"))
            else {
                continue;
            };
            for member in members.as_array().into_iter().flatten() {
                self.upsert_member(guild_id, member);
            }
        }
    }

    fn user_settings(&mut self, settings: &Value) {
        if let Some(folders) = settings.get("guild_folders")
            && let Ok(folders) = serde_json::from_value::<Vec<GuildFolder>>(folders.clone())
        {
            self.guild_folders = folders;
        } else if self.guild_folders.is_empty()
            && let Some(positions) = settings["guild_positions"].as_array()
        {
            self.guild_folders = positions
                .iter()
                .filter_map(|id| id.as_str()?.parse().ok())
                .map(|id| GuildFolder {
                    guild_ids: vec![id],
                    ..GuildFolder::default()
                })
                .collect();
        }
        if let Some(status) = settings["status"].as_str() {
            self.status = Some(status.to_owned());
        }
        if let Some(custom) = settings.get("custom_status") {
            self.custom_status = (!custom.is_null()).then(|| CustomStatus {
                text: custom["text"].as_str().map(str::to_owned),
                emoji_name: custom["emoji_name"].as_str().map(str::to_owned),
                emoji_id: snowflake(custom, "emoji_id"),
            });
        }
    }

    fn user_settings_proto(&mut self, proto: &str, partial: bool) {
        let Some(settings) =
            crate::settings_proto::decode(proto, partial, crate::time::unix_millis_now())
        else {
            warn!("could not decode the settings proto");
            return;
        };
        if let Some(folders) = settings.guild_folders {
            self.guild_folders = folders;
        }
        if let Some(status) = settings.status.filter(|status| !status.is_empty()) {
            self.status = Some(status);
        }
        if let Some(custom) = settings.custom_status {
            self.custom_status = custom;
        }
    }

    /// The account's effective status is the `all` session when present,
    /// otherwise the first listed session. Returns whether it changed.
    fn sessions(&mut self, sessions: &Value) -> bool {
        let Some(list) = sessions.as_array() else {
            return false;
        };
        let session = list
            .iter()
            .find(|session| session["session_id"] == "all")
            .or_else(|| list.first());
        let Some(status) = session.and_then(|session| session["status"].as_str()) else {
            return false;
        };
        let changed = self.status.as_deref() != Some(status);
        self.status = Some(status.to_owned());
        changed
    }

    fn guild_create(&mut self, data: &Value, events: &mut Vec<StoreEvent>) {
        let Ok(mut guild) = serde_json::from_value::<Guild>(with_properties(data)) else {
            warn!("could not parse GUILD_CREATE");
            return;
        };
        let id = guild.id;
        let channels = guild.channels.take().unwrap_or_default();

        let is_new = !self.guilds.contains_key(&id);
        if !self.guild_order.contains(&id) {
            self.guild_order.push(id);
        }
        if let Some(existing) = self.guilds.get(&id) {
            // A later GUILD_CREATE can be a lighter object than the READY one.
            if guild.roles.is_empty() {
                guild.roles = existing.roles.clone();
            }
            if guild.emojis.is_empty() {
                guild.emojis = existing.emojis.clone();
            }
            if guild.soundboard_sounds.is_empty() {
                guild.soundboard_sounds = existing.soundboard_sounds.clone();
            }
            if guild.features.is_empty() && data.get("features").is_none() && data.pointer("/properties/features").is_none() {
                guild.features = existing.features.clone();
            }
            if guild.stage_instances.is_empty() && data.get("stage_instances").is_none() {
                guild.stage_instances = existing.stage_instances.clone();
            }
            if guild.joined_at.is_none() {
                guild.joined_at = existing.joined_at.clone();
            }
        }
        self.guilds.insert(id, guild);

        self.merge_guild_channels(id, channels);
        for thread in data["threads"].as_array().into_iter().flatten() {
            if let Ok(thread) = serde_json::from_value::<Channel>(thread.clone()) {
                self.merge_guild_channels(id, vec![thread]);
            }
        }

        for member in data["members"].as_array().into_iter().flatten() {
            self.upsert_member(id, member);
        }
        for presence in data["presences"].as_array().into_iter().flatten() {
            if let Some(user) = presence.get("user")
                && user.get("username").is_some()
            {
                self.upsert_user_from_value(user);
            }
            self.upsert_presence(presence);
        }

        if let Some(voice_states) = data["voice_states"].as_array() {
            for voice_state in voice_states {
                if let Some(user) = voice_state.pointer("/member/user") {
                    self.upsert_user_from_value(user);
                }
                if let Ok(mut voice_state) =
                    serde_json::from_value::<VoiceState>(voice_state.clone())
                {
                    if voice_state.guild_id.is_none() {
                        voice_state.guild_id = Some(id);
                    }
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
        let data = &with_properties(data);
        let Some(id) = snowflake(data, "id") else {
            return;
        };
        let Some(guild) = self.guilds.get_mut(&id) else {
            return;
        };
        if let Some(name) = data["name"].as_str() {
            guild.name = name.to_owned();
        }
        if let Some(features) = data["features"].as_array() {
            guild.features = features.iter().filter_map(|feature| feature.as_str().map(str::to_owned)).collect();
        }
        if let Some(level) = data["verification_level"].as_u64() {
            guild.verification_level = Some(level as u8);
        }
        if let Some(level) = data["explicit_content_filter"].as_u64() {
            guild.explicit_content_filter = Some(level as u8);
        }
        if data.get("rules_channel_id").is_some() {
            guild.rules_channel_id = snowflake(data, "rules_channel_id");
        }
        if data.get("public_updates_channel_id").is_some() {
            guild.public_updates_channel_id = snowflake(data, "public_updates_channel_id");
        }
        if let Some(icon) = data.get("icon") {
            guild.icon = icon.as_str().map(str::to_owned);
        }
        if let Some(banner) = data.get("banner") {
            guild.banner = banner.as_str().map(str::to_owned);
        }
        if let Some(owner) = snowflake(data, "owner_id") {
            guild.owner_id = Some(owner);
        }
        if let Some(description) = data.get("description") {
            guild.description = description.as_str().map(str::to_owned);
        }
        if let Some(afk) = data.get("afk_channel_id") {
            guild.afk_channel_id = afk.as_str().and_then(|id| id.parse().ok());
        }
        if let Some(timeout) = data["afk_timeout"].as_u64() {
            guild.afk_timeout = Some(timeout as u32);
        }
        if let Some(system) = data.get("system_channel_id") {
            guild.system_channel_id = system.as_str().and_then(|id| id.parse().ok());
        }
        if let Some(level) = data["default_message_notifications"].as_u64() {
            guild.default_message_notifications = Some(level as u8);
        }
        if let Some(roles) = data.get("roles")
            && let Ok(roles) = serde_json::from_value::<Vec<Role>>(roles.clone())
        {
            guild.roles = roles;
        }
        if let Some(emojis) = data.get("emojis")
            && let Ok(emojis) = serde_json::from_value::<Vec<Emoji>>(emojis.clone())
        {
            guild.emojis = emojis;
        }
        if let Some(stickers) = data.get("stickers")
            && let Ok(stickers) = serde_json::from_value::<Vec<crate::guild::Sticker>>(stickers.clone())
        {
            guild.stickers = stickers;
        }
        events.push(StoreEvent::GuildUpdated(id));
    }

    fn channel_upsert(&mut self, data: &Value, events: &mut Vec<StoreEvent>) {
        let Ok(mut channel) = serde_json::from_value::<Channel>(data.clone()) else {
            return;
        };
        let id = channel.id;
        self.resolve_recipients(&mut channel);
        if let Some(existing) = self.channels.get(&id) {
            // Updates do not carry `last_message_id`, and thread updates do
            // not repeat the membership.
            if channel.last_message_id.is_none() {
                channel.last_message_id = existing.last_message_id;
            }
            if channel.member.is_none() {
                channel.member = existing.member.clone();
            }
            if channel.recipients.is_none() {
                channel.recipients = existing.recipients.clone();
            }
        }
        self.index_recipients(&channel);
        let is_new = self.channels.insert(id, channel).is_none();
        events.push(if is_new {
            StoreEvent::ChannelAdded(id)
        } else {
            StoreEvent::ChannelUpdated(id)
        });
    }

    fn message_create(&mut self, data: &Value, events: &mut Vec<StoreEvent>) {
        let Ok(message) = serde_json::from_value::<Message>(data.clone()) else {
            warn!("could not parse MESSAGE_CREATE");
            return;
        };
        let channel_id = message.channel_id;
        let id = message.id;
        self.upsert_user(message.author.clone());
        for user in &message.mentions {
            self.upsert_user(user.clone());
        }
        if let (Some(guild_id), Some(member)) = (message.guild_id, data.get("member")) {
            self.store_member(guild_id, message.author.id, member);
        }

        let my_id = self.me.as_ref().map(|me| me.id);
        if my_id == Some(message.author.id) {
            // Sending marks the channel read.
            self.read_states.insert(
                channel_id,
                ReadState {
                    last_message_id: Some(id),
                    mention_count: 0,
                },
            );
        } else if self.mentions_me(&message) {
            self.read_states
                .entry(channel_id)
                .or_default()
                .mention_count += 1;
        }

        let buffer = self.messages.entry(channel_id).or_default();
        // The real message replaces the optimistic one shown while it sent.
        let replaced = nonce_str(&message.nonce)
            .and_then(|nonce| buffer.take_pending(&nonce))
            .filter(|pending| *pending != id);
        if !buffer.is_detached() {
            buffer.insert(message);
        }
        if let Some(pending) = replaced {
            events.push(StoreEvent::MessageDeleted {
                channel_id,
                message_id: pending,
            });
        }
        if let Some(channel) = self.channels.get_mut(&channel_id) {
            channel.last_message_id = Some(id);
        }
        events.push(StoreEvent::MessageAdded {
            channel_id,
            message_id: id,
        });
        events.push(StoreEvent::ReadStateUpdated(channel_id));
    }

    /// Whether a new message should raise the channel's mention badge.
    fn mentions_me(&self, message: &Message) -> bool {
        let Some(me) = self.me.as_ref().map(|me| me.id) else {
            return false;
        };
        if self
            .relationships
            .get(&message.author.id)
            .is_some_and(|relationship| relationship.kind == Relationship::BLOCKED)
        {
            return false;
        }
        if message.mentions.iter().any(|user| user.id == me) {
            return true;
        }
        let Some(guild_id) = message.guild_id.or_else(|| {
            self.channels
                .get(&message.channel_id)
                .and_then(|channel| channel.guild_id)
        }) else {
            // Every message in an unmuted DM counts.
            return !self.is_muted(message.channel_id);
        };
        let settings = self.guild_settings.get(&Some(guild_id));
        if message.mention_everyone && !settings.is_some_and(|settings| settings.suppress_everyone)
        {
            return true;
        }
        if !message.mention_roles.is_empty()
            && !settings.is_some_and(|settings| settings.suppress_roles)
            && let Some(member) = self.members.get(&(guild_id, me))
        {
            return message
                .mention_roles
                .iter()
                .any(|role| member.roles.contains(role));
        }
        false
    }

    fn message_update(&mut self, data: &Value, events: &mut Vec<StoreEvent>) {
        let (Some(channel_id), Some(id)) = (snowflake(data, "channel_id"), snowflake(data, "id"))
        else {
            return;
        };

        // A full message (e.g. link-preview embed completion) replaces the row.
        if data.get("author").is_some() {
            if let Ok(mut message) = serde_json::from_value::<Message>(data.clone()) {
                self.upsert_user(message.author.clone());
                let buffer = self.messages.entry(channel_id).or_default();
                // Updates omit reactions; keep the ones already counted.
                if message.reactions.is_empty()
                    && let Some(existing) = buffer.get(id)
                {
                    message.reactions = existing.reactions.clone();
                }
                // Do not let an edit of an old message outside the window in.
                if buffer.get(id).is_some() {
                    buffer.insert(message);
                }
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
                if let Some(pinned) = data.get("pinned").and_then(Value::as_bool) {
                    message.pinned = pinned;
                }
                if let Some(flags) = data.get("flags").and_then(Value::as_u64) {
                    message.flags = flags;
                }
                if let Some(components) = data.get("components").and_then(Value::as_array) {
                    message.components = components.clone();
                }
                if let Some(flags) = data.get("flags").and_then(Value::as_u64) {
                    message.flags = flags;
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

    fn reaction(&mut self, data: &Value, removed: bool, events: &mut Vec<StoreEvent>) {
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
        let updated = buffer.update_with(message_id, |message| {
            let key = |reaction: &Reaction| (reaction.emoji.id, reaction.emoji.name.clone());
            // Custom emoji are identified by id alone: the name can change.
            let same = |reaction: &Reaction| match emoji.id {
                Some(id) => reaction.emoji.id == Some(id),
                None => key(reaction) == (None, emoji.name.clone()),
            };

            if let Some(reaction) = message.reactions.iter_mut().find(|reaction| same(reaction)) {
                if removed {
                    reaction.count = reaction.count.saturating_sub(1);
                    if me {
                        reaction.me = false;
                    }
                } else {
                    // The server echoes our own optimistic reaction.
                    if !(me && reaction.me) {
                        reaction.count += 1;
                    }
                    if me {
                        reaction.me = true;
                    }
                }
                message.reactions.retain(|reaction| reaction.count > 0);
            } else if !removed {
                message.reactions.push(Reaction {
                    count: 1,
                    me,
                    emoji: emoji.clone(),
                });
            }
        });
        if updated {
            events.push(StoreEvent::MessageUpdated {
                channel_id,
                message_id,
            });
        }
    }

    /// REMOVE_ALL clears every reaction; REMOVE_EMOJI clears one emoji.
    fn reaction_clear(&mut self, data: &Value, events: &mut Vec<StoreEvent>) {
        let (Some(channel_id), Some(message_id)) =
            (snowflake(data, "channel_id"), snowflake(data, "message_id"))
        else {
            return;
        };
        let emoji = serde_json::from_value::<crate::message::ReactionEmoji>(data["emoji"].clone())
            .ok()
            .filter(|_| data.get("emoji").is_some());
        if let Some(buffer) = self.messages.get_mut(&channel_id)
            && buffer.update_with(message_id, |message| match &emoji {
                Some(emoji) => message.reactions.retain(|reaction| {
                    (reaction.emoji.id, &reaction.emoji.name) != (emoji.id, &emoji.name)
                }),
                None => message.reactions.clear(),
            })
        {
            events.push(StoreEvent::MessageUpdated {
                channel_id,
                message_id,
            });
        }
    }

    pub fn load_history(&mut self, channel_id: Snowflake, messages: Vec<Message>) {
        self.note_authors(&messages);
        let buffer = self.messages.entry(channel_id).or_default();
        buffer.extend(messages);
        // A history page is older-or-equal (scroll-back or the first page), so
        // the distant end to drop is the newest. This never fires on the first
        // page or after a jump (the window is small then).
        buffer.cap_newest();
    }

    /// Replaces a channel's window with the page around an older message.
    pub fn load_around(&mut self, channel_id: Snowflake, messages: Vec<Message>) {
        self.note_authors(&messages);
        let newest = self
            .channels
            .get(&channel_id)
            .and_then(|channel| channel.last_message_id);
        self.messages
            .entry(channel_id)
            .or_default()
            .replace_around(messages, newest);
    }

    /// Adds a page of messages after the window; `full_page` says whether
    /// Discord may have more after it.
    pub fn load_newer(&mut self, channel_id: Snowflake, messages: Vec<Message>, full_page: bool) {
        self.note_authors(&messages);
        self.messages
            .entry(channel_id)
            .or_default()
            .extend_newer(messages, full_page);
    }

    fn note_authors(&mut self, messages: &[Message]) {
        for message in messages {
            self.upsert_user(message.author.clone());
            for user in &message.mentions {
                self.upsert_user(user.clone());
            }
        }
    }

    pub fn mark_read(&mut self, channel_id: Snowflake, message_id: Snowflake) {
        self.read_states.insert(
            channel_id,
            ReadState {
                last_message_id: Some(message_id),
                mention_count: 0,
            },
        );
    }

    /// Stores a member object (`{user, nick, roles, ...}` or the deduplicated
    /// `{user_id, ...}` form). Returns the user id.
    fn upsert_member(&mut self, guild_id: Snowflake, value: &Value) -> Option<Snowflake> {
        if let Some(user) = value.get("user")
            && user.get("username").is_some()
        {
            self.upsert_user_from_value(user);
        }
        let user_id = value
            .pointer("/user/id")
            .and_then(Value::as_str)
            .and_then(|id| id.parse().ok())
            .or_else(|| snowflake(value, "user_id"))?;
        self.store_member(guild_id, user_id, value);
        Some(user_id)
    }

    fn store_member(&mut self, guild_id: Snowflake, user_id: Snowflake, value: &Value) {
        self.guild_members
            .entry(guild_id)
            .or_default()
            .insert(user_id);
        if let Ok(member) = serde_json::from_value::<Member>(value.clone()) {
            self.members.insert((guild_id, user_id), member);
        }
    }

    fn upsert_presence(&mut self, value: &Value) -> Option<(Option<Snowflake>, Snowflake)> {
        let presence = Presence::from_value(value)?;
        let key = (presence.guild_id, presence.user_id);
        self.presences.insert(presence.user_id, presence);
        Some(key)
    }

    /// Applies SYNC / INSERT / UPDATE / DELETE / INVALIDATE to the guild's
    /// member list and records the members and presences the rows carry.
    fn member_list_update(&mut self, data: &Value, events: &mut Vec<StoreEvent>) {
        let Some(guild_id) = snowflake(data, "guild_id") else {
            return;
        };
        let list_id = data["id"].as_str().unwrap_or("everyone").to_owned();
        let mut items_seen: Vec<&Value> = Vec::new();
        // Each audience (list id) keeps its own rows, so an update for one does
        // not clobber another's.
        let list = self
            .member_lists
            .entry(guild_id)
            .or_default()
            .entry(list_id.clone())
            .or_insert_with(|| MemberList {
                id: list_id,
                ..MemberList::default()
            });
        list.member_count = data["member_count"].as_u64().unwrap_or(0) as u32;
        list.online_count = data["online_count"].as_u64().unwrap_or(0) as u32;
        if let Some(groups) = data["groups"].as_array() {
            list.group_counts = groups
                .iter()
                .filter_map(|group| {
                    Some((
                        group["id"].as_str()?.to_owned(),
                        group["count"].as_u64().unwrap_or(0) as u32,
                    ))
                })
                .collect();
        }

        for op in data["ops"].as_array().into_iter().flatten() {
            let index = op["index"].as_u64().unwrap_or(0) as usize;
            let range = |position: usize| op["range"][position].as_u64().unwrap_or(0) as usize;
            match op["op"].as_str().unwrap_or("") {
                "SYNC" => {
                    let items: Vec<&Value> = op["items"].as_array().into_iter().flatten().collect();
                    list.sync(
                        range(0),
                        items
                            .iter()
                            .map(|item| MemberList::parse_item(item))
                            .collect(),
                    );
                    items_seen.extend(items);
                }
                "INSERT" => {
                    list.insert(index, MemberList::parse_item(&op["item"]));
                    items_seen.push(&op["item"]);
                }
                "UPDATE" => {
                    list.update(index, MemberList::parse_item(&op["item"]));
                    items_seen.push(&op["item"]);
                }
                "DELETE" => list.delete(index),
                "INVALIDATE" => list.invalidate(range(0), range(1)),
                _ => {}
            }
        }

        for item in items_seen {
            let Some(member) = item.get("member") else {
                continue;
            };
            self.upsert_member(guild_id, member);
            if let Some(presence) = member.get("presence") {
                self.upsert_presence(presence);
            }
        }
        events.push(StoreEvent::GuildMembersUpdated(guild_id));
        events.push(StoreEvent::MemberListUpdated(guild_id));
    }

    /// The member list to show for `channel_id`: the audience whose id matches
    /// the channel's view-permission overwrites. Falls back to the audience with
    /// the most synced rows if none matches (e.g. the id hash disagrees), so the
    /// list still shows rather than coming up empty.
    pub fn member_list_for(
        &self,
        guild_id: Snowflake,
        channel_id: Snowflake,
    ) -> Option<&MemberList> {
        let lists = self.member_lists.get(&guild_id)?;
        if let Some(wanted) = self.channel_list_id(channel_id)
            && let Some(list) = lists.get(&wanted)
        {
            return Some(list);
        }
        lists.values().max_by_key(|list| list.rows().count())
    }

    /// The member list id Discord uses for a channel: a hash of its
    /// view-permission overwrites. Threads carry no overwrites of their own, so
    /// they inherit the parent channel's audience.
    fn channel_list_id(&self, channel_id: Snowflake) -> Option<String> {
        let channel = self.channels.get(&channel_id)?;
        let source = if channel.kind.is_thread() {
            channel
                .parent_id
                .and_then(|parent| self.channels.get(&parent))
                .unwrap_or(channel)
        } else {
            channel
        };
        let guild_id = source.guild_id?;
        Some(crate::member_list::compute_list_id(
            &source.permission_overwrites,
            guild_id,
        ))
    }

    /// Guild channel objects from `READY`/`GUILD_CREATE` are scoped by the
    /// enclosing guild, not by a `guild_id` on each channel. Abaddon does the
    /// same assignment in ProcessNewGuild; without it `guild_channels()` finds
    /// nothing and the sidebar says "No channels".
    fn merge_guild_channels(&mut self, guild_id: Snowflake, channels: Vec<Channel>) {
        for mut channel in channels {
            channel.guild_id = Some(guild_id);
            self.index_recipients(&channel);
            self.channels.insert(channel.id, channel);
        }
    }

    /// READY_SUPPLEMENTAL carries live voice states, the rest of the merged
    /// members and every presence that READY itself omits.
    fn ready_supplemental(&mut self, data: &Value, events: &mut Vec<StoreEvent>) {
        self.merged_members(data);

        for presence in data
            .pointer("/merged_presences/friends")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some((_, user_id)) = self.upsert_presence(presence) {
                events.push(StoreEvent::PresenceUpdated {
                    guild_id: None,
                    user_id,
                });
            }
        }
        for guild_presences in data
            .pointer("/merged_presences/guilds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for presence in guild_presences.as_array().into_iter().flatten() {
                self.upsert_presence(presence);
            }
        }

        for channel in data["lazy_private_channels"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Ok(mut channel) = serde_json::from_value::<Channel>(channel.clone()) {
                self.resolve_recipients(&mut channel);
                self.index_recipients(&channel);
                self.channels.insert(channel.id, channel);
            }
        }

        if let Some(guilds) = data["guilds"].as_array() {
            for guild in guilds {
                let guild_id = snowflake(guild, "id");
                let Some(states) = guild["voice_states"].as_array() else {
                    continue;
                };
                for state in states {
                    if let Some(user) = state.pointer("/member/user") {
                        self.upsert_user_from_value(user);
                    }
                    let Ok(mut voice_state) = serde_json::from_value::<VoiceState>(state.clone())
                    else {
                        continue;
                    };
                    if voice_state.guild_id.is_none() {
                        voice_state.guild_id = guild_id;
                    }
                    let event = StoreEvent::VoiceStateUpdated {
                        guild_id: voice_state.guild_id,
                        user_id: voice_state.user_id,
                    };
                    self.voice_states.insert(voice_state.key(), voice_state);
                    events.push(event);
                }
            }
        }
    }

    /// Fills `recipients` from `recipient_ids` using the known users.
    fn resolve_recipients(&self, channel: &mut Channel) {
        if channel.recipients.is_some() {
            return;
        }
        if let Some(ids) = &channel.recipient_ids {
            channel.recipients = Some(
                ids.iter()
                    .filter_map(|id| self.users.get(id).cloned())
                    .collect(),
            );
        }
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

    /// Shows a message optimistically, before Discord confirms it. Does
    /// nothing while the channel's window is on older messages, since the
    /// pending row would sit below a gap.
    pub fn add_pending(&self, channel_id: Snowflake, message: Message) {
        let id = message.id;
        {
            let mut state = self.inner.write();
            let buffer = state.messages.entry(channel_id).or_default();
            if buffer.is_detached() {
                return;
            }
            buffer.insert_pending(message);
        }
        let _ = self.tx.send(StoreEvent::MessageAdded {
            channel_id,
            message_id: id,
        });
    }

    /// Marks the optimistic message with this nonce as failed (or another
    /// send state). Emits an update so the row redraws.
    pub fn set_send_state(&self, channel_id: Snowflake, nonce: &str, state: SendState) {
        let id = self
            .inner
            .write()
            .messages
            .get_mut(&channel_id)
            .and_then(|buffer| buffer.set_send_state(nonce, state));
        if let Some(message_id) = id {
            let _ = self.tx.send(StoreEvent::MessageUpdated {
                channel_id,
                message_id,
            });
        }
    }

    /// Drops the optimistic message with this nonce (a failed send the user
    /// discards, or a retry about to re-add it).
    pub fn drop_pending(&self, channel_id: Snowflake, nonce: &str) {
        let id = self
            .inner
            .write()
            .messages
            .get_mut(&channel_id)
            .and_then(|buffer| buffer.take_pending(nonce));
        if let Some(message_id) = id {
            let _ = self.tx.send(StoreEvent::MessageDeleted {
                channel_id,
                message_id,
            });
        }
    }
}

/// A gateway guild as one object. With CLIENT_STATE_V2 (which the web client
/// sends) its own fields -- name, icon, owner, features -- are under
/// `properties`, apart from its channels, roles and other lists.
fn with_properties(guild: &Value) -> Value {
    let mut guild = guild.clone();
    if let Some(Value::Object(properties)) = guild.as_object_mut().and_then(|guild| guild.remove("properties"))
        && let Some(object) = guild.as_object_mut()
    {
        for (key, value) in properties {
            object.entry(key).or_insert(value);
        }
    }
    guild
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

    fn parsed(id: u64, channel: u64) -> Message {
        serde_json::from_value(message(id, channel, "old")).unwrap()
    }

    #[test]
    fn the_window_caps_the_correct_end_and_can_be_turned_off() {
        let over: Vec<Message> = (1..=(MESSAGE_WINDOW as u64 + 100))
            .map(|id| parsed(id, 10))
            .collect();

        set_unload_distant(true);
        // Scrolling back (older pages grow the top) drops the newest and
        // detaches from the present, keeping the oldest.
        let mut back = MessageBuffer::default();
        back.extend(over.clone());
        back.cap_newest();
        assert_eq!(back.messages().len(), MESSAGE_WINDOW);
        assert_eq!(back.messages().first().unwrap().id, Snowflake(1));
        assert!(back.is_detached());

        // Live/newer growth drops the oldest, keeping the newest.
        let mut live = MessageBuffer::default();
        live.extend(over.clone());
        live.cap_oldest();
        assert_eq!(live.messages().len(), MESSAGE_WINDOW);
        assert_eq!(
            live.messages().last().unwrap().id,
            Snowflake(MESSAGE_WINDOW as u64 + 100)
        );

        // Turned off, nothing is dropped.
        set_unload_distant(false);
        let mut keep = MessageBuffer::default();
        keep.extend(over);
        keep.cap_newest();
        keep.cap_oldest();
        assert_eq!(keep.messages().len(), MESSAGE_WINDOW + 100);
        set_unload_distant(true);
    }

    #[test]
    fn a_window_around_an_old_message_takes_no_new_messages_until_it_reaches_the_present() {
        let store = Store::new();
        store.apply(
            "CHANNEL_CREATE",
            &json!({"id": "10", "type": 0, "guild_id": "5", "name": "general", "position": 1}),
        );
        store.apply("MESSAGE_CREATE", &message(900, 10, "newest"));
        store.write(|state| state.load_around(Snowflake(10), vec![parsed(20, 10), parsed(10, 10)]));
        store.read(|state| {
            let buffer = &state.messages[&Snowflake(10)];
            assert!(buffer.is_detached());
            let ids: Vec<_> = buffer.messages().iter().map(|m| m.id.0).collect();
            assert_eq!(ids, [10, 20]);
        });

        // A live message would leave a gap after 20, so it waits.
        store.apply("MESSAGE_CREATE", &message(901, 10, "live"));
        store.read(|state| {
            assert_eq!(state.messages(Snowflake(10)).len(), 2);
            assert_eq!(
                state.channels[&Snowflake(10)].last_message_id,
                Some(Snowflake(901))
            );
        });

        // A full page after the window: still short of the present.
        store.write(|state| state.load_newer(Snowflake(10), vec![parsed(30, 10)], true));
        store.apply("MESSAGE_CREATE", &message(902, 10, "live"));
        assert!(store.read(|state| state.messages[&Snowflake(10)].is_detached()));

        // A short page reaches it; live messages are added again.
        store.write(|state| {
            state.load_newer(Snowflake(10), vec![parsed(901, 10), parsed(902, 10)], false)
        });
        store.apply("MESSAGE_CREATE", &message(903, 10, "live"));
        store.read(|state| {
            let ids: Vec<_> = state
                .messages(Snowflake(10))
                .iter()
                .map(|m| m.id.0)
                .collect();
            assert_eq!(ids, [10, 20, 30, 901, 902, 903]);
        });
    }

    #[test]
    fn a_window_that_includes_the_newest_message_is_not_detached() {
        let store = Store::new();
        store.apply(
            "CHANNEL_CREATE",
            &json!({"id": "10", "type": 0, "guild_id": "5", "name": "general", "position": 1}),
        );
        store.apply("MESSAGE_CREATE", &message(20, 10, "newest"));
        store.write(|state| state.load_around(Snowflake(10), vec![parsed(10, 10), parsed(20, 10)]));
        assert!(!store.read(|state| state.messages[&Snowflake(10)].is_detached()));
    }

    #[test]
    fn an_optimistic_message_is_replaced_by_its_confirmation() {
        let store = Store::new();
        store.apply(
            "CHANNEL_CREATE",
            &json!({"id": "10", "type": 0, "guild_id": "5", "name": "general", "position": 1}),
        );
        let mut pending = parsed(1_000_000, 10);
        pending.send_state = SendState::Sending;
        pending.nonce = Some(json!("abc"));
        store.add_pending(Snowflake(10), pending);
        store.read(|state| {
            let messages = state.messages(Snowflake(10));
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].send_state, SendState::Sending);
        });

        // The gateway echo carries the same nonce but the real id.
        let mut confirmed = message(42, 10, "hi");
        confirmed["nonce"] = json!("abc");
        store.apply("MESSAGE_CREATE", &confirmed);
        store.read(|state| {
            let messages = state.messages(Snowflake(10));
            assert_eq!(messages.len(), 1, "the pending row was replaced, not kept");
            assert_eq!(messages[0].id, Snowflake(42));
            assert_eq!(messages[0].send_state, SendState::Sent);
        });
    }

    #[test]
    fn a_failed_send_can_be_marked_and_dropped() {
        let store = Store::new();
        store.apply(
            "CHANNEL_CREATE",
            &json!({"id": "10", "type": 0, "guild_id": "5", "name": "general", "position": 1}),
        );
        let mut pending = parsed(1_000_000, 10);
        pending.send_state = SendState::Sending;
        pending.nonce = Some(json!("n"));
        store.add_pending(Snowflake(10), pending);
        store.set_send_state(Snowflake(10), "n", SendState::Failed);
        assert_eq!(
            store.read(|state| state.messages(Snowflake(10))[0].send_state),
            SendState::Failed
        );
        store.drop_pending(Snowflake(10), "n");
        assert!(store.read(|state| state.messages(Snowflake(10)).is_empty()));
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
    fn community_servers_are_known_from_their_features() {
        let store = Store::new();
        store.apply("READY", &json!({"session_id": "s", "user": {"id": "1", "username": "me"}, "users": [],
            "guilds": [
                {"id": "5", "properties": {"id": "5", "name": "Big", "features": ["COMMUNITY", "NEWS"]}},
                {"id": "6", "name": "Small", "features": []}
            ],
            "private_channels": [], "relationships": [], "read_state": {"entries": []}, "user_guild_settings": {"entries": []}}));
        store.read(|state| {
            assert!(state.guilds[&Snowflake(5)].is_community());
            assert!(!state.guilds[&Snowflake(6)].is_community());
        });
        store.apply("GUILD_UPDATE", &json!({"id": "6", "name": "Small", "features": ["COMMUNITY"]}));
        store.read(|state| assert!(state.guilds[&Snowflake(6)].is_community()));
    }

    #[test]
    fn soundboards_load_change_and_play() {
        let store = Store::new();
        store.apply("READY", &json!({"session_id": "s", "user": {"id": "1", "username": "me"}, "users": [],
            "guilds": [{"id": "5", "name": "G"}],
            "private_channels": [], "relationships": [], "read_state": {"entries": []}, "user_guild_settings": {"entries": []}}));
        store.apply("SOUNDBOARD_SOUNDS", &json!({"guild_id": "5", "soundboard_sounds": [
            {"sound_id": "50", "name": "bonk", "volume": 0.5, "emoji_name": "🔨", "guild_id": "5", "available": true}]}));
        store.apply("GUILD_SOUNDBOARD_SOUND_CREATE", &json!({"sound_id": "51", "name": "honk", "guild_id": "5"}));
        store.apply("GUILD_SOUNDBOARD_SOUND_UPDATE", &json!({"sound_id": "50", "name": "BONK", "volume": 1.0, "guild_id": "5"}));
        store.apply("GUILD_SOUNDBOARD_SOUND_DELETE", &json!({"sound_id": "51", "guild_id": "5"}));
        store.read(|state| {
            let sounds = &state.guilds[&Snowflake(5)].soundboard_sounds;
            assert_eq!(sounds.len(), 1);
            assert_eq!((sounds[0].name.as_str(), sounds[0].volume), ("BONK", 1.0));
        });
        let mut events = store.subscribe();
        store.apply("VOICE_CHANNEL_EFFECT_SEND", &json!({"channel_id": "9", "guild_id": "5", "user_id": "2",
            "sound_id": "50", "sound_volume": 0.5, "animation_id": 0}));
        let mut played = false;
        while let Ok(event) = events.try_recv() {
            played |= matches!(event,
                StoreEvent::SoundboardPlayed { sound_id, volume, .. } if sound_id == Snowflake(50) && volume == 0.5);
        }
        assert!(played);
    }

    #[test]
    fn stages_go_live_change_topic_and_end() {
        let store = Store::new();
        store.apply("READY", &json!({"session_id": "s", "user": {"id": "1", "username": "me"}, "users": [],
            "guilds": [{"id": "5", "name": "G", "channels": [{"id": "9", "type": 13, "name": "Stage"}],
                "stage_instances": [{"id": "90", "guild_id": "5", "channel_id": "9", "topic": "Hello"}]}],
            "private_channels": [], "relationships": [], "read_state": {"entries": []}, "user_guild_settings": {"entries": []}}));
        let topic = |store: &Store| store.read(|state| state.stage_instance(Snowflake(9)).map(|stage| stage.topic.clone()));
        assert_eq!(topic(&store).as_deref(), Some("Hello"));
        store.apply("STAGE_INSTANCE_UPDATE", &json!({"id": "90", "guild_id": "5", "channel_id": "9", "topic": "Q&A"}));
        assert_eq!(topic(&store).as_deref(), Some("Q&A"));
        store.apply("STAGE_INSTANCE_DELETE", &json!({"id": "90", "guild_id": "5", "channel_id": "9", "topic": "Q&A"}));
        assert_eq!(topic(&store), None);
        store.apply("VOICE_STATE_UPDATE", &json!({"user_id": "2", "guild_id": "5", "channel_id": "9", "session_id": "x",
            "suppress": true, "request_to_speak_timestamp": "2026-11-23T04:00:00Z"}));
        store.read(|state| {
            let voice = &state.voice_states[&(Some(Snowflake(5)), Snowflake(2))];
            assert!(voice.suppress && voice.request_to_speak_timestamp.is_some());
        });
    }

    #[test]
    fn message_requests_stay_out_of_the_dm_list_until_accepted() {
        let store = Store::new();
        store.apply("READY", &json!({"session_id": "s", "user": {"id": "1", "username": "me"}, "users": [], "guilds": [],
            "private_channels": [
                {"id": "70", "type": 1, "recipients": [{"id": "2", "username": "friend"}]},
                {"id": "71", "type": 1, "is_message_request": true, "recipients": [{"id": "3", "username": "stranger"}]},
                {"id": "72", "type": 1, "is_message_request": true, "is_spam": true, "recipients": [{"id": "4", "username": "spammer"}]}
            ], "relationships": [], "read_state": {"entries": []}, "user_guild_settings": {"entries": []}}));
        let ids = |channels: Vec<&Channel>| channels.iter().map(|channel| channel.id.0).collect::<Vec<_>>();
        store.read(|state| {
            assert_eq!(ids(state.dm_channels()), vec![70]);
            assert_eq!(ids(state.message_requests(false)), vec![71]);
            assert_eq!(ids(state.message_requests(true)), vec![72]);
        });
        // Accepted: Discord's CHANNEL_UPDATE clears the flag.
        store.apply("CHANNEL_UPDATE", &json!({"id": "71", "type": 1, "is_message_request": false,
            "recipients": [{"id": "3", "username": "stranger"}]}));
        store.read(|state| {
            assert_eq!(ids(state.dm_channels()).len(), 2);
            assert!(state.message_requests(false).is_empty());
        });
    }

    #[test]
    fn poll_votes_are_counted_and_ours_once() {
        let store = Store::new();
        store.apply("READY", &json!({"session_id": "s", "user": {"id": "1", "username": "me"}, "users": [], "guilds": [],
            "private_channels": [], "relationships": [], "read_state": {"entries": []}, "user_guild_settings": {"entries": []}}));
        let mut posted = message(100, 10, "");
        posted["poll"] = json!({"question": {"text": "Lunch?"}, "answers": [
            {"answer_id": 1, "poll_media": {"text": "Pizza"}},
            {"answer_id": 2, "poll_media": {"text": "Sushi", "emoji": {"name": "🍣"}}}
        ], "expiry": "2099-01-01T00:00:00+00:00", "allow_multiselect": false, "layout_type": 1});
        store.apply("MESSAGE_CREATE", &posted);
        let vote = |user: &str, answer: u32, added: bool| {
            store.apply(
                if added { "MESSAGE_POLL_VOTE_ADD" } else { "MESSAGE_POLL_VOTE_REMOVE" },
                &json!({"channel_id": "10", "message_id": "100", "user_id": user, "answer_id": answer}),
            );
        };
        vote("2", 1, true);
        vote("3", 1, true);
        vote("1", 2, true);
        vote("1", 2, true); // echoed twice: counted once
        store.read(|state| {
            let poll = state.messages(Snowflake(10))[0].poll.clone().unwrap();
            assert_eq!((poll.count(1), poll.count(2), poll.total()), (2, 1, 3));
            assert!(poll.voted(2) && !poll.voted(1) && poll.has_voted());
            assert_eq!(poll.answers[1].poll_media.emoji.as_ref().and_then(|e| e.name.clone()).as_deref(), Some("🍣"));
            assert!(!poll.is_closed(0));
        });
        vote("1", 2, false);
        vote("3", 1, false);
        store.read(|state| {
            let poll = state.messages(Snowflake(10))[0].poll.clone().unwrap();
            assert_eq!((poll.count(1), poll.count(2)), (1, 0));
            assert!(!poll.has_voted());
        });
    }

    #[test]
    fn batched_reactions_count_each_user() {
        let store = Store::new();
        store.apply("MESSAGE_CREATE", &message(100, 10, "hello"));
        store.apply(
            "MESSAGE_REACTION_ADD_MANY",
            &json!({"channel_id": "10", "message_id": "100", "reactions": [
                {"emoji": {"name": "👍"}, "users": ["1", "2", "3"]},
                {"emoji": {"id": "55", "name": "blob"}, "users": ["2"]}
            ]}),
        );
        store.read(|state| {
            let reactions = &state.messages(Snowflake(10))[0].reactions;
            assert_eq!(reactions.len(), 2);
            assert_eq!(reactions[0].count, 3);
            assert_eq!(reactions[1].count, 1);
        });
    }

    #[test]
    fn ready_reads_the_web_clients_shapes() {
        // CLIENT_STATE_V2 puts a guild's fields under `properties`, and
        // USER_SETTINGS_PROTO replaces `user_settings` with a protobuf.
        let store = Store::new();
        store.apply(
            "READY",
            &json!({
                "session_id": "s",
                "user": {"id": "1", "username": "me"},
                "users": [],
                "guilds": [{
                    "id": "10",
                    "properties": {"id": "10", "name": "Lambda Ignition", "icon": "abc", "owner_id": "1"},
                    "channels": [{"id": "20", "type": 0, "name": "general"}],
                    "roles": [],
                    "data_mode": "full",
                    "version": 1
                }],
                "merged_members": [[]],
                "private_channels": [],
                "relationships": [],
                "read_state": {"entries": []},
                "user_guild_settings": {"entries": []},
                "user_settings_proto": crate::settings_proto::tests::sample()
            }),
        );
        store.read(|state| {
            let guild = &state.guilds[&Snowflake(10)];
            assert_eq!(guild.name, "Lambda Ignition");
            assert_eq!(guild.icon.as_deref(), Some("abc"));
            assert!(state.channels.contains_key(&Snowflake(20)));
            assert_eq!(state.status.as_deref(), Some("dnd"));
            assert_eq!(state.custom_status.as_ref().and_then(|c| c.text.as_deref()), Some("coding"));
            assert_eq!(state.guild_folders.len(), 2);
            assert_eq!(state.guild_folders[0].name.as_deref(), Some("Games"));
        });
        // A later change to the status alone keeps the custom status.
        use base64::Engine;
        let mut status = Vec::new();
        let mut wrapper = Vec::new();
        crate::settings_proto::tests::message(&mut wrapper, 1, b"idle");
        crate::settings_proto::tests::message(&mut status, 1, &wrapper);
        let mut settings = Vec::new();
        crate::settings_proto::tests::message(&mut settings, 11, &status);
        store.apply(
            "USER_SETTINGS_PROTO_UPDATE",
            &json!({"settings": {"type": 1, "proto": base64::engine::general_purpose::STANDARD.encode(settings)}, "partial": true}),
        );
        store.read(|state| {
            assert_eq!(state.status.as_deref(), Some("idle"));
            assert!(state.custom_status.is_some());
            assert_eq!(state.guild_folders.len(), 2);
        });
    }

    #[test]
    fn passive_updates_move_unreads_and_voice() {
        let store = Store::new();
        store.apply(
            "GUILD_CREATE",
            &json!({"id": "5", "properties": {"name": "Quiet"}, "channels": [
                {"id": "50", "type": 0, "name": "general"},
                {"id": "51", "type": 2, "name": "Voice"}
            ]}),
        );
        store.apply(
            "PASSIVE_UPDATE_V2",
            &json!({
                "guild_id": "5",
                "updated_channels": [{"id": "50", "last_message_id": "900", "last_pin_timestamp": null}],
                "updated_voice_states": [{"user_id": "7", "channel_id": "51", "session_id": "x"}],
                "updated_members": [{"user": {"id": "7", "username": "seven"}, "roles": []}],
                "removed_voice_states": []
            }),
        );
        store.read(|state| {
            assert_eq!(state.guilds[&Snowflake(5)].name, "Quiet");
            assert_eq!(state.channels[&Snowflake(50)].last_message_id, Some(Snowflake(900)));
            let voice = &state.voice_states[&(Some(Snowflake(5)), Snowflake(7))];
            assert_eq!(voice.channel_id, Some(Snowflake(51)));
        });
        store.apply(
            "PASSIVE_UPDATE_V2",
            &json!({"guild_id": "5", "updated_channels": [], "updated_voice_states": [],
                    "updated_members": [], "removed_voice_states": ["7"]}),
        );
        store.read(|state| {
            let left = state
                .voice_states
                .get(&(Some(Snowflake(5)), Snowflake(7)))
                .and_then(|voice| voice.channel_id);
            assert_eq!(left, None);
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
        store.apply("CHANNEL_CREATE", &json!({"id": "10", "type": 1}));
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

    #[test]
    fn guild_create_channels_are_scoped_to_the_parent_guild() {
        let store = Store::new();
        // Embedded guild channels normally have no guild_id; Abaddon attaches
        // the enclosing guild id in ProcessNewGuild.
        store.apply(
            "GUILD_CREATE",
            &json!({
                "id": "5",
                "name": "Lambda Ignition",
                "channels": [
                    {"id": "10", "type": 4, "name": "Text Channels", "position": 0},
                    {"id": "11", "type": 0, "name": "general", "position": 1},
                    {"id": "12", "type": 2, "name": "General", "position": 2}
                ]
            }),
        );

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
    }

    #[test]
    fn ready_with_full_guilds_keeps_channels() {
        let store = Store::new();
        store.apply(
            "READY",
            &json!({
                "user": {"id": "1", "username": "lambda"},
                "guilds": [{
                    "id": "5",
                    "name": "Lambda Ignition",
                    "channels": [
                        {"id": "11", "type": 0, "name": "general", "position": 1}
                    ]
                }]
            }),
        );

        store.read(|state| {
            let channels = state.guild_channels(Snowflake(5));
            assert_eq!(channels.len(), 1);
            assert_eq!(channels[0].display_name(), "general");
            assert_eq!(channels[0].guild_id, Some(Snowflake(5)));
        });
    }

    #[test]
    fn guild_create_seeds_members_and_member_list_updates_adds() {
        let store = Store::new();
        store.apply(
            "GUILD_CREATE",
            &json!({
                "id": "5",
                "name": "Lambda Ignition",
                "channels": [],
                "members": [
                    {"user": {"id": "1", "username": "lambda"}},
                    {"user": {"id": "2", "username": "friend"}}
                ]
            }),
        );
        store.apply(
            "GUILD_MEMBER_LIST_UPDATE",
            &json!({
                "guild_id": "5",
                "id": "everyone",
                "ops": [
                    {"op": "SYNC", "range": [0, 99], "items": [
                        {"group": {"id": "online", "count": 2}},
                        {"member": {"user": {"id": "1", "username": "lambda"},
                                    "presence": {"user": {"id": "1"}, "status": "idle"}}}
                    ]},
                    {"op": "INSERT", "index": 2,
                     "item": {"member": {"user": {"id": "3", "username": "third"}}}}
                ]
            }),
        );
        store.read(|state| {
            let names: Vec<String> = state
                .guild_member_users(Snowflake(5))
                .into_iter()
                .map(|user| user.username)
                .collect();
            assert_eq!(names, vec!["friend", "lambda", "third"]);
            let list = &state.member_lists[&Snowflake(5)]["everyone"];
            assert_eq!(list.rows().count(), 3);
            assert_eq!(state.status_of(Snowflake(1)), "idle");
        });
    }

    #[test]
    fn different_audiences_do_not_clobber_each_other() {
        // A guild with a public channel ("everyone") and a restricted one
        // ("restricted-hash"): an update for one audience must not wipe the
        // other's rows -- the bug where a server's member list came up empty.
        let store = Store::new();
        let sync = |id: &str, user: &str| {
            json!({
                "guild_id": "5",
                "id": id,
                "member_count": 1,
                "online_count": 1,
                "ops": [{"op": "SYNC", "range": [0, 99], "items": [
                    {"member": {"user": {"id": user, "username": user}}}
                ]}]
            })
        };
        store.apply("GUILD_MEMBER_LIST_UPDATE", &sync("everyone", "1"));
        store.apply("GUILD_MEMBER_LIST_UPDATE", &sync("restricted-hash", "2"));
        store.read(|state| {
            let lists = &state.member_lists[&Snowflake(5)];
            assert_eq!(lists.len(), 2, "both audiences are kept");
            assert_eq!(lists["everyone"].rows().count(), 1);
            assert_eq!(lists["restricted-hash"].rows().count(), 1);
        });
    }

    #[test]
    fn voice_state_member_populates_occupant_name() {
        let store = Store::new();
        store.apply(
            "VOICE_STATE_UPDATE",
            &json!({
                "guild_id": "5",
                "channel_id": "12",
                "user_id": "2",
                "member": {"user": {"id": "2", "username": "friend"}}
            }),
        );
        store.read(|state| {
            let users = state.users_in_channel_voice(Snowflake(5), Snowflake(12));
            assert_eq!(users.len(), 1);
            assert_eq!(users[0].display_name(), "friend");
        });
    }

    #[test]
    fn dm_calls_are_seeded_by_call_create_and_cleared_by_call_delete() {
        let store = Store::new();
        store.apply(
            "CALL_CREATE",
            &json!({
                "channel_id": "40",
                "ringing": [],
                "voice_states": [
                    {"channel_id": "40", "user_id": "1"},
                    {"channel_id": "40", "user_id": "2"}
                ]
            }),
        );
        let in_call = |store: &Store| {
            store.read(|state| {
                state
                    .voice_states
                    .values()
                    .filter(|voice| voice.channel_id == Some(Snowflake(40)))
                    .count()
            })
        };
        assert_eq!(in_call(&store), 2);
        // A guild voice state in another channel is left alone.
        store.apply(
            "VOICE_STATE_UPDATE",
            &json!({"guild_id": "5", "channel_id": "12", "user_id": "3"}),
        );
        store.apply("CALL_DELETE", &json!({"channel_id": "40"}));
        assert_eq!(in_call(&store), 0);
        assert!(store.read(|state| {
            state
                .voice_states
                .contains_key(&(Some(Snowflake(5)), Snowflake(3)))
        }));
    }

    #[test]
    fn ready_supplemental_seeds_voice_states() {
        let store = Store::new();
        store.apply(
            "READY_SUPPLEMENTAL",
            &json!({
                "merged_presences": {"friends": []},
                "guilds": [{
                    "id": "5",
                    "voice_states": [{
                        "user_id": "1",
                        "channel_id": "12",
                        "session_id": "abc"
                    }]
                }]
            }),
        );

        store.read(|state| {
            let states = state.guild_voice_states(Snowflake(5));
            assert_eq!(states.len(), 1);
            assert_eq!(states[0].channel_id, Some(Snowflake(12)));
        });
    }

    fn ready_fixture() -> Value {
        json!({
            "user": {"id": "1", "username": "me"},
            "users": [
                {"id": "2", "username": "friend", "global_name": "Friend"},
                {"id": "3", "username": "blocked"}
            ],
            "guilds": [
                {
                    "id": "50", "name": "Alpha", "owner_id": "9", "joined_at": "2020-01-01T00:00:00+00:00",
                    "roles": [
                        {"id": "50", "name": "@everyone", "permissions": "3072", "position": 0},
                        {"id": "51", "name": "Mods", "permissions": "0", "position": 2, "color": 255},
                        {"id": "52", "name": "Low", "permissions": "0", "position": 1, "color": 65280}
                    ],
                    "emojis": [
                        {"id": "70", "name": "wave"},
                        {"id": "71", "name": "dance", "animated": true},
                        {"id": "72", "name": "mods_only", "roles": ["99"]}
                    ],
                    "channels": [
                        {"id": "60", "type": 4, "name": "Text", "position": 1},
                        {"id": "61", "type": 0, "name": "general", "position": 5, "parent_id": "60",
                         "last_message_id": "4000000000000000000"},
                        {"id": "62", "type": 2, "name": "Voice", "position": 0, "parent_id": "60"},
                        {"id": "63", "type": 0, "name": "secret", "position": 6, "parent_id": "60",
                         "permission_overwrites": [{"id": "50", "type": 0, "allow": "0", "deny": "1024"}]},
                        {"id": "64", "type": 4, "name": "Empty", "position": 2},
                        {"id": "65", "type": 0, "name": "rules", "position": 9}
                    ]
                },
                {"id": "80", "name": "Beta", "joined_at": "2021-01-01T00:00:00+00:00"},
                {"id": "90", "name": "Gamma", "joined_at": "2022-01-01T00:00:00+00:00"}
            ],
            "merged_members": [
                [{"user_id": "1", "roles": ["51", "52"], "nick": "Me Here"}], [], []
            ],
            "private_channels": [
                {"id": "100", "type": 1, "recipient_ids": ["2"], "last_message_id": "5"},
                {"id": "101", "type": 3, "recipient_ids": ["2", "3"], "last_message_id": "9"}
            ],
            "relationships": [
                {"id": "2", "user_id": "2", "type": 1, "nickname": "Bestie"},
                {"id": "3", "user_id": "3", "type": 2}
            ],
            "read_state": {"entries": [
                {"id": "61", "last_message_id": "4000000000000000000", "mention_count": 0},
                {"id": "100", "last_message_id": "5", "mention_count": 0},
                {"id": "7", "read_state_type": 3, "last_acked_id": "1"}
            ]},
            "user_guild_settings": {"entries": [
                {"guild_id": "80", "muted": true, "mute_config": null, "channel_overrides": []}
            ]},
            "user_settings": {
                "status": "dnd",
                "custom_status": {"text": "busy", "emoji_name": null, "emoji_id": null},
                "guild_folders": [
                    {"id": null, "guild_ids": ["50"]},
                    {"id": 7, "name": "Games", "color": 16711680, "guild_ids": ["80", "12345"]}
                ]
            },
            "sessions": [{"session_id": "abc", "status": "dnd"}]
        })
    }

    #[test]
    fn ready_resolves_deduplicated_users() {
        let store = Store::new();
        store.apply("READY", &ready_fixture());
        store.read(|state| {
            let dm = &state.channels[&Snowflake(100)];
            assert_eq!(state.channel_title(dm), "Bestie");
            assert_eq!(dm.dm_recipient().unwrap().id, Snowflake(2));
            assert_eq!(state.dm_with(Snowflake(2)).unwrap().id, Snowflake(100));
            // Group DM is newer, so it sorts first.
            let order: Vec<u64> = state.dm_channels().iter().map(|c| c.id.0).collect();
            assert_eq!(order, vec![101, 100]);
            assert_eq!(state.relationships_of(Relationship::FRIEND).len(), 1);
            assert!(state.is_blocked(Snowflake(3)));
            assert_eq!(state.status_of(Snowflake(1)), "dnd");
            assert_eq!(
                state.custom_status.as_ref().unwrap().text.as_deref(),
                Some("busy")
            );
            // Non-channel read states are skipped.
            assert!(!state.read_states.contains_key(&Snowflake(7)));
        });
    }

    #[test]
    fn guild_rail_follows_folders_and_puts_unlisted_guilds_first() {
        let store = Store::new();
        store.apply("READY", &ready_fixture());
        store.read(|state| {
            use crate::settings::GuildEntry;
            assert_eq!(
                state.guild_entries(),
                vec![
                    // Gamma is in no folder: newest join first.
                    GuildEntry::Guild(Snowflake(90)),
                    GuildEntry::Guild(Snowflake(50)),
                    // The folder drops the guild the account has left.
                    GuildEntry::Folder {
                        id: 7,
                        name: Some("Games".into()),
                        color: Some(16711680),
                        guild_ids: vec![Snowflake(80)],
                    },
                ]
            );
            let flat: Vec<u64> = state.ordered_guilds().iter().map(|g| g.id.0).collect();
            assert_eq!(flat, vec![90, 50, 80]);
        });
    }

    #[test]
    fn channel_tree_hides_what_the_account_cannot_see() {
        let store = Store::new();
        store.apply("READY", &ready_fixture());
        store.read(|state| {
            let tree = state.channel_tree(Snowflake(50));
            let shape: Vec<(Option<&str>, Vec<&str>)> = tree
                .iter()
                .map(|section| {
                    (
                        section.category.as_ref().and_then(|c| c.name.as_deref()),
                        section
                            .channels
                            .iter()
                            .filter_map(|c| c.name.as_deref())
                            .collect(),
                    )
                })
                .collect();
            // Orphans first; text before voice despite positions; the hidden
            // channel and the empty category are gone.
            assert_eq!(
                shape,
                vec![
                    (None, vec!["rules"]),
                    (Some("Text"), vec!["general", "Voice"])
                ]
            );
            assert!(state.can_send(Snowflake(61)));
            assert!(!state.can(Snowflake(63), crate::permissions::VIEW_CHANNEL));
            assert_eq!(state.member_color(Snowflake(50), Snowflake(1)), Some(255));
            let me = state.me.clone().unwrap();
            assert_eq!(state.display_name(Some(Snowflake(50)), &me), "Me Here");
        });
    }

    #[test]
    fn mentions_and_acks_drive_unread_state() {
        let store = Store::new();
        store.apply("READY", &ready_fixture());
        let read = |store: &Store, channel: u64| {
            store.read(|state| {
                (
                    state.unread(Snowflake(channel)),
                    state.mention_count(Snowflake(channel)),
                )
            })
        };
        assert_eq!(read(&store, 61), (false, 0));
        // Before the account joined: `rules` has no read state and no messages.
        assert_eq!(read(&store, 65), (false, 0));

        let mut plain = message(4_000_000_000_000_000_001, 61, "hello");
        plain["guild_id"] = json!("50");
        plain["author"] = json!({"id": "2", "username": "friend"});
        store.apply("MESSAGE_CREATE", &plain);
        assert_eq!(read(&store, 61), (true, 0));
        assert_eq!(
            store.read(|state| state.guild_unread(Snowflake(50))),
            (true, 0)
        );

        let mut role_ping = plain.clone();
        role_ping["id"] = json!("4000000000000000002");
        role_ping["mention_roles"] = json!(["52"]);
        store.apply("MESSAGE_CREATE", &role_ping);
        assert_eq!(read(&store, 61), (true, 1));

        // A blocked user's mention does not count.
        let mut blocked = plain.clone();
        blocked["id"] = json!("4000000000000000003");
        blocked["author"] = json!({"id": "3", "username": "blocked"});
        blocked["mentions"] = json!([{"id": "1", "username": "me"}]);
        store.apply("MESSAGE_CREATE", &blocked);
        assert_eq!(read(&store, 61), (true, 1));

        store.apply(
            "MESSAGE_ACK",
            &json!({"channel_id": "61", "message_id": "4000000000000000003"}),
        );
        assert_eq!(read(&store, 61), (false, 0));

        // Our own message marks the channel read.
        store.apply(
            "MESSAGE_CREATE",
            &plain_from(&plain, "4000000000000000004", "2"),
        );
        assert!(read(&store, 61).0);
        store.apply(
            "MESSAGE_CREATE",
            &plain_from(&plain, "4000000000000000005", "1"),
        );
        assert_eq!(read(&store, 61), (false, 0));

        // Every message in an unmuted DM is a mention.
        let mut dm = message(50, 100, "yo");
        dm["author"] = json!({"id": "2", "username": "friend"});
        store.apply("MESSAGE_CREATE", &dm);
        assert_eq!(read(&store, 100), (true, 1));
        // The group DM has no read state at all, so it is unread as well.
        let unread: Vec<(u64, u32)> = store.read(|state| {
            state
                .unread_dms()
                .iter()
                .map(|(channel, count)| (channel.id.0, *count))
                .collect()
        });
        assert_eq!(unread, vec![(100, 1), (101, 1)]);
    }

    fn plain_from(base: &Value, id: &str, author: &str) -> Value {
        let mut value = base.clone();
        value["id"] = json!(id);
        value["author"] = json!({"id": author, "username": "someone"});
        value
    }

    #[test]
    fn muted_guilds_only_report_mentions() {
        let store = Store::new();
        store.apply("READY", &ready_fixture());
        store.apply(
            "CHANNEL_CREATE",
            &json!({"id": "81", "type": 0, "guild_id": "80", "name": "chat"}),
        );
        let mut msg = message(4_000_000_000_000_000_010, 81, "hi");
        msg["guild_id"] = json!("80");
        msg["author"] = json!({"id": "2", "username": "friend"});
        store.apply("MESSAGE_CREATE", &msg);
        assert_eq!(
            store.read(|state| state.guild_unread(Snowflake(80))),
            (false, 0)
        );
        msg["id"] = json!("4000000000000000011");
        msg["mentions"] = json!([{"id": "1", "username": "me"}]);
        store.apply("MESSAGE_CREATE", &msg);
        assert_eq!(
            store.read(|state| state.guild_unread(Snowflake(80))),
            (true, 1)
        );
    }

    #[test]
    fn presence_updates_use_the_nested_user_id() {
        let store = Store::new();
        store.apply("READY", &ready_fixture());
        store.apply(
            "PRESENCE_UPDATE",
            &json!({
                "user": {"id": "2"}, "status": "online",
                "activities": [
                    {"type": 0, "name": "Factorio"},
                    {"type": 4, "name": "Custom Status", "state": "afk soon", "emoji": {"name": "🌙"}}
                ]
            }),
        );
        store.read(|state| {
            assert_eq!(state.status_of(Snowflake(2)), "online");
            let presence = state.presence(Snowflake(2)).unwrap();
            assert_eq!(presence.summary().as_deref(), Some("🌙 afk soon"));
            // The partial user object must not erase the known user.
            assert_eq!(state.users[&Snowflake(2)].username, "friend");
        });
    }

    #[test]
    fn sendable_emojis_respect_nitro_and_roles() {
        let store = Store::new();
        store.apply("READY", &ready_fixture());
        store.read(|state| {
            let names = |nitro: bool, guild: Option<u64>| -> Vec<String> {
                state
                    .sendable_emojis(guild.map(Snowflake), nitro)
                    .into_iter()
                    .flat_map(|(_, emojis)| emojis.into_iter().map(|emoji| emoji.name.clone()))
                    .collect()
            };
            assert_eq!(names(false, Some(50)), vec!["wave"]);
            assert_eq!(names(true, None), vec!["wave", "dance"]);
            assert!(names(false, None).is_empty());
            assert_eq!(
                state.emoji(Snowflake(71)).unwrap().1.mention(),
                "<a:dance:71>"
            );
        });
    }

    #[test]
    fn channel_updates_keep_the_last_message_id() {
        let store = Store::new();
        store.apply("READY", &ready_fixture());
        store.apply(
            "CHANNEL_UPDATE",
            &json!({"id": "61", "type": 0, "guild_id": "50", "name": "renamed", "parent_id": "60"}),
        );
        store.read(|state| {
            let channel = &state.channels[&Snowflake(61)];
            assert_eq!(channel.name.as_deref(), Some("renamed"));
            assert_eq!(
                channel.last_message_id,
                Some(Snowflake(4_000_000_000_000_000_000))
            );
        });
    }

    #[test]
    fn moderation_events_update_the_cache() {
        let mut state = State::default();
        state.apply(
            "GUILD_CREATE",
            &json!({"id": "1", "name": "g", "roles": [{"id": "1", "name": "@everyone", "permissions": "0"}],
                    "channels": [], "afk_timeout": 300}),
        );
        // A member's roles, nickname and timeout change.
        let events = state.apply(
            "GUILD_MEMBER_UPDATE",
            &json!({"guild_id": "1", "user": {"id": "9", "username": "x"}, "nick": "Nick",
                    "roles": ["5"], "communication_disabled_until": "2030-01-01T00:00:00+00:00"}),
        );
        assert!(matches!(events[..], [StoreEvent::GuildMembersUpdated(_)]));
        let member = state.member(Snowflake::new(1), Snowflake::new(9)).unwrap();
        assert_eq!(member.nick.as_deref(), Some("Nick"));
        assert_eq!(member.roles, [Snowflake::new(5)]);
        assert!(member.communication_disabled_until.is_some());

        // Roles are created, updated in place and deleted.
        state.apply(
            "GUILD_ROLE_CREATE",
            &json!({"guild_id": "1", "role": {"id": "5", "name": "Mods", "position": 1, "permissions": "2", "managed": false}}),
        );
        state.apply(
            "GUILD_ROLE_UPDATE",
            &json!({"guild_id": "1", "role": {"id": "5", "name": "Moderators", "position": 1, "permissions": "6", "color": 255}}),
        );
        let roles = &state.guilds[&Snowflake::new(1)].roles;
        assert_eq!(roles.len(), 2);
        let role = roles
            .iter()
            .find(|role| role.id == Snowflake::new(5))
            .unwrap();
        assert_eq!(
            (role.name.as_str(), role.permissions, role.color),
            ("Moderators", 6, 255)
        );
        state.apply(
            "GUILD_ROLE_DELETE",
            &json!({"guild_id": "1", "role_id": "5"}),
        );
        assert_eq!(state.guilds[&Snowflake::new(1)].roles.len(), 1);

        // Server settings arrive in GUILD_UPDATE; a null clears.
        state.apply(
            "GUILD_UPDATE",
            &json!({"id": "1", "name": "renamed", "description": "about", "afk_channel_id": "7",
                    "afk_timeout": 900, "system_channel_id": null, "default_message_notifications": 1}),
        );
        let guild = &state.guilds[&Snowflake::new(1)];
        assert_eq!(guild.name, "renamed");
        assert_eq!(guild.description.as_deref(), Some("about"));
        assert_eq!(guild.afk_channel_id, Some(Snowflake::new(7)));
        assert_eq!(guild.afk_timeout, Some(900));
        assert_eq!(guild.system_channel_id, None);
        assert_eq!(guild.default_message_notifications, Some(1));

        // Channel edits keep their voice settings.
        state.apply(
            "CHANNEL_UPDATE",
            &json!({"id": "7", "type": 2, "guild_id": "1", "name": "AFK", "bitrate": 96000, "user_limit": 5}),
        );
        let channel = &state.channels[&Snowflake::new(7)];
        assert_eq!(
            (channel.bitrate, channel.user_limit),
            (Some(96000), Some(5))
        );

        let events = state.apply(
            "GUILD_BAN_ADD",
            &json!({"guild_id": "1", "user": {"id": "9", "username": "x"}}),
        );
        assert!(matches!(events[..], [StoreEvent::BansUpdated(id)] if id == Snowflake::new(1)));
        state.apply(
            "GUILD_MEMBER_REMOVE",
            &json!({"guild_id": "1", "user": {"id": "9", "username": "x"}}),
        );
        assert!(state.member(Snowflake::new(1), Snowflake::new(9)).is_none());
    }
}
