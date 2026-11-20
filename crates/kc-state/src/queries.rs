//! Read-only views over the state, shaped for the interface.

use std::collections::{HashMap, HashSet};

use kc_core::Snowflake;

use crate::channel::{Channel, ChannelType};
use crate::guild::{Emoji, Guild, Member, Presence, Relationship, Role, VoiceState};
use crate::message::Message;
use crate::permissions;
use crate::settings::GuildEntry;
use crate::state::State;
use crate::user::User;

/// A category and the channels under it, in sidebar order. The first section
/// of a guild has no category and holds the channels outside any category.
#[derive(Clone, Debug)]
pub struct ChannelSection {
    pub category: Option<Channel>,
    /// Channels in order; a joined thread follows its parent channel.
    pub channels: Vec<Channel>,
}

impl State {
    pub fn my_id(&self) -> Option<Snowflake> {
        self.me.as_ref().map(|me| me.id)
    }

    pub fn guild(&self, id: Snowflake) -> Option<&Guild> {
        self.guilds.get(&id)
    }

    /// Guilds in sidebar order, folders flattened.
    pub fn ordered_guilds(&self) -> Vec<&Guild> {
        self.guild_entries()
            .into_iter()
            .flat_map(|entry| match entry {
                GuildEntry::Guild(id) => vec![id],
                GuildEntry::Folder { guild_ids, .. } => guild_ids,
            })
            .filter_map(|id| self.guilds.get(&id))
            .collect()
    }

    /// The server rail: folders and loose guilds in the user's order.
    ///
    /// `guild_folders` can name guilds the account has left and can miss
    /// guilds joined since the list was last saved. Unlisted guilds go first,
    /// most recently joined at the top, which is where the official client
    /// puts a newly joined guild.
    pub fn guild_entries(&self) -> Vec<GuildEntry> {
        let mut listed: HashSet<Snowflake> = HashSet::new();
        let mut entries = Vec::new();
        for folder in &self.guild_folders {
            let guild_ids: Vec<Snowflake> = folder
                .guild_ids
                .iter()
                .copied()
                .filter(|id| self.guilds.contains_key(id) && listed.insert(*id))
                .collect();
            if guild_ids.is_empty() {
                continue;
            }
            if folder.is_folder() {
                entries.push(GuildEntry::Folder {
                    id: folder.id.unwrap_or(0),
                    name: folder.name.clone().filter(|name| !name.is_empty()),
                    color: folder.color,
                    guild_ids,
                });
            } else {
                entries.extend(guild_ids.into_iter().map(GuildEntry::Guild));
            }
        }

        let mut unlisted: Vec<&Guild> = self
            .guild_order
            .iter()
            .filter(|id| !listed.contains(id))
            .filter_map(|id| self.guilds.get(id))
            .collect();
        unlisted.sort_by(|a, b| b.joined_at.cmp(&a.joined_at).then(b.id.cmp(&a.id)));
        let mut all: Vec<GuildEntry> = unlisted
            .into_iter()
            .map(|guild| GuildEntry::Guild(guild.id))
            .collect();
        all.extend(entries);
        all
    }

    /// Every channel of a guild sorted by position, without permission
    /// filtering or grouping.
    pub fn guild_channels(&self, guild_id: Snowflake) -> Vec<&Channel> {
        let mut channels: Vec<&Channel> = self
            .channels
            .values()
            .filter(|channel| channel.guild_id == Some(guild_id))
            .collect();
        channels.sort_by_key(|channel| channel.sort_key());
        channels
    }

    /// The channel sidebar of a guild: channels the account can see, grouped
    /// under their categories. Within a group text channels precede voice
    /// channels (the two have independent position counters), empty
    /// categories are dropped, and joined threads follow their parent.
    pub fn channel_tree(&self, guild_id: Snowflake) -> Vec<ChannelSection> {
        let all: Vec<&Channel> = self
            .channels
            .values()
            .filter(|channel| channel.guild_id == Some(guild_id))
            .collect();

        let mut threads: HashMap<Snowflake, Vec<&Channel>> = HashMap::new();
        for channel in &all {
            if channel.kind.is_thread()
                && channel.member.is_some()
                && !channel
                    .thread_metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.archived)
                && let Some(parent) = channel.parent_id
            {
                threads.entry(parent).or_default().push(channel);
            }
        }
        for list in threads.values_mut() {
            list.sort_by_key(|thread| std::cmp::Reverse(thread.id));
        }

        let voice_last = |channel: &Channel| {
            matches!(
                channel.kind,
                ChannelType::GuildVoice | ChannelType::GuildStageVoice
            )
        };
        let order = |channel: &&Channel| {
            (
                voice_last(channel),
                channel.position.unwrap_or(0),
                channel.id.get(),
            )
        };
        let visible =
            |channel: &Channel| self.permissions_in(channel.id) & permissions::VIEW_CHANNEL != 0;

        let children_of = |parent: Option<Snowflake>| -> Vec<Channel> {
            let mut children: Vec<&Channel> = all
                .iter()
                .copied()
                .filter(|channel| {
                    !channel.kind.is_category()
                        && !channel.kind.is_thread()
                        && channel.parent_id == parent
                        && visible(channel)
                })
                .collect();
            children.sort_by_key(order);
            let mut out = Vec::new();
            for channel in children {
                out.push(channel.clone());
                for thread in threads.get(&channel.id).into_iter().flatten() {
                    out.push((*thread).clone());
                }
            }
            out
        };

        let mut sections = Vec::new();
        let orphans = children_of(None);
        if !orphans.is_empty() {
            sections.push(ChannelSection {
                category: None,
                channels: orphans,
            });
        }
        let mut categories: Vec<&Channel> = all
            .iter()
            .copied()
            .filter(|channel| channel.kind.is_category())
            .collect();
        categories.sort_by_key(|category| (category.position.unwrap_or(0), category.id.get()));
        for category in categories {
            let channels = children_of(Some(category.id));
            if !channels.is_empty() {
                sections.push(ChannelSection {
                    category: Some(category.clone()),
                    channels,
                });
            }
        }
        sections
    }

    /// DM and group-DM channels, most recent conversation first.
    pub fn dm_channels(&self) -> Vec<&Channel> {
        let mut channels: Vec<&Channel> = self
            .channels
            .values()
            .filter(|channel| channel.is_private())
            .collect();
        channels.sort_by_key(|channel| {
            std::cmp::Reverse(self.last_message_id(channel.id).unwrap_or(channel.id))
        });
        channels
    }

    /// The existing one-to-one DM with a user, if any.
    pub fn dm_with(&self, user_id: Snowflake) -> Option<&Channel> {
        self.channels.values().find(|channel| {
            channel
                .dm_recipient()
                .is_some_and(|recipient| recipient.id == user_id)
        })
    }

    pub fn messages(&self, channel_id: Snowflake) -> Vec<Message> {
        self.messages
            .get(&channel_id)
            .map(|buffer| buffer.messages().to_vec())
            .unwrap_or_default()
    }

    pub fn last_message_id(&self, channel_id: Snowflake) -> Option<Snowflake> {
        let newest_loaded = self
            .messages
            .get(&channel_id)
            .and_then(|buffer| buffer.messages().last().map(|message| message.id));
        let reported = self
            .channels
            .get(&channel_id)
            .and_then(|channel| channel.last_message_id);
        newest_loaded.max(reported)
    }

    // -- permissions -----------------------------------------------------

    pub fn member(&self, guild_id: Snowflake, user_id: Snowflake) -> Option<&Member> {
        self.members.get(&(guild_id, user_id))
    }

    /// The account's effective permissions in a channel. Private channels and
    /// channels of unknown guilds allow everything, so a missing cache entry
    /// never hides a channel.
    pub fn permissions_in(&self, channel_id: Snowflake) -> u64 {
        let Some(channel) = self.channels.get(&channel_id) else {
            return permissions::ALL;
        };
        let (Some(guild_id), Some(me)) = (channel.guild_id, self.my_id()) else {
            return permissions::ALL;
        };
        let Some(guild) = self.guilds.get(&guild_id) else {
            return permissions::ALL;
        };
        if guild.roles.is_empty() {
            return permissions::ALL;
        }
        // Threads inherit the overwrites of their parent channel.
        let source = if channel.kind.is_thread() {
            channel
                .parent_id
                .and_then(|parent| self.channels.get(&parent))
                .unwrap_or(channel)
        } else {
            channel
        };
        permissions::in_channel(guild, source, me, self.member(guild_id, me))
    }

    pub fn can(&self, channel_id: Snowflake, permission: u64) -> bool {
        self.permissions_in(channel_id) & permission != 0
    }

    /// Whether the account may post in a channel.
    pub fn can_send(&self, channel_id: Snowflake) -> bool {
        let is_thread = self
            .channels
            .get(&channel_id)
            .is_some_and(|channel| channel.kind.is_thread());
        let needed = if is_thread {
            permissions::SEND_MESSAGES_IN_THREADS
        } else {
            permissions::SEND_MESSAGES
        };
        let granted = self.permissions_in(channel_id);
        granted & permissions::VIEW_CHANNEL != 0 && granted & needed != 0
    }

    // -- names and colours -----------------------------------------------

    pub fn role(&self, guild_id: Snowflake, role_id: Snowflake) -> Option<&Role> {
        self.guilds
            .get(&guild_id)?
            .roles
            .iter()
            .find(|role| role.id == role_id)
    }

    /// The name to show for a user: guild nickname, then friend nickname,
    /// then the global display name.
    pub fn display_name(&self, guild_id: Option<Snowflake>, user: &User) -> String {
        if let Some(nick) = guild_id
            .and_then(|guild_id| self.member(guild_id, user.id))
            .and_then(|member| member.nick.as_deref())
            .filter(|nick| !nick.is_empty())
        {
            return nick.to_owned();
        }
        if guild_id.is_none()
            && let Some(nick) = self
                .relationships
                .get(&user.id)
                .and_then(|relationship| relationship.nickname.as_deref())
                .filter(|nick| !nick.is_empty())
        {
            return nick.to_owned();
        }
        user.display_name().to_owned()
    }

    /// The colour of the member's highest role that has one.
    pub fn member_color(&self, guild_id: Snowflake, user_id: Snowflake) -> Option<u32> {
        let member = self.member(guild_id, user_id)?;
        let guild = self.guilds.get(&guild_id)?;
        guild
            .roles
            .iter()
            .filter(|role| role.color != 0 && member.roles.contains(&role.id))
            .max_by_key(|role| (role.position, std::cmp::Reverse(role.id)))
            .map(|role| role.color)
    }

    /// The title of a channel: its name, or the recipients of a DM.
    pub fn channel_title(&self, channel: &Channel) -> String {
        if let Some(name) = channel.name.as_deref().filter(|name| !name.is_empty()) {
            return name.to_owned();
        }
        match channel.recipients.as_deref() {
            Some([]) | None if channel.is_private() => "Unnamed".to_owned(),
            Some(recipients) => recipients
                .iter()
                .map(|user| self.display_name(None, user))
                .collect::<Vec<_>>()
                .join(", "),
            None => channel.id.to_string(),
        }
    }

    // -- unread ----------------------------------------------------------

    fn mute_active(&self, guild_id: Option<Snowflake>, channel_id: Snowflake, now: &str) -> bool {
        self.guild_settings
            .get(&guild_id)
            .and_then(|settings| settings.channel_overrides.get(&channel_id))
            .is_some_and(|channel| channel.mute.active(now))
    }

    /// Whether a channel is muted, directly or through its category.
    pub fn is_muted(&self, channel_id: Snowflake) -> bool {
        let now = crate::time::now_iso();
        let Some(channel) = self.channels.get(&channel_id) else {
            return false;
        };
        if self.mute_active(channel.guild_id, channel_id, &now) {
            return true;
        }
        channel
            .parent_id
            .is_some_and(|parent| self.mute_active(channel.guild_id, parent, &now))
    }

    pub fn guild_muted(&self, guild_id: Snowflake) -> bool {
        self.guild_settings
            .get(&Some(guild_id))
            .is_some_and(|settings| settings.mute.active(&crate::time::now_iso()))
    }

    pub fn mention_count(&self, channel_id: Snowflake) -> u32 {
        self.read_states
            .get(&channel_id)
            .map_or(0, |state| state.mention_count)
    }

    /// Whether a channel has messages the account has not read, ignoring
    /// mutes. Voice-only and container channels never count; a channel with
    /// no read state counts only for messages sent after the account joined.
    fn has_unread(&self, channel: &Channel) -> bool {
        if !channel.kind.is_text()
            || matches!(
                channel.kind,
                ChannelType::GuildForum | ChannelType::GuildMedia
            )
        {
            return false;
        }
        if channel.kind.is_thread() && channel.member.is_none() {
            return false;
        }
        let Some(last) = self.last_message_id(channel.id) else {
            return false;
        };
        match self
            .read_states
            .get(&channel.id)
            .and_then(|state| state.last_message_id)
        {
            Some(read) => last > read,
            None => {
                let joined = channel
                    .guild_id
                    .and_then(|guild_id| self.guilds.get(&guild_id))
                    .and_then(|guild| guild.joined_at.as_deref())
                    .and_then(crate::time::iso_to_snowflake);
                joined.is_none_or(|joined| last > joined)
            }
        }
    }

    /// Whether the sidebar should mark a channel unread: unread messages in a
    /// visible channel that is not muted, or any mention.
    pub fn unread(&self, channel_id: Snowflake) -> bool {
        let Some(channel) = self.channels.get(&channel_id) else {
            return false;
        };
        if !self.can(channel_id, permissions::VIEW_CHANNEL) {
            return false;
        }
        if self.mention_count(channel_id) > 0 {
            return true;
        }
        !self.is_muted(channel_id) && self.has_unread(channel)
    }

    /// `(has unread, mention count)` for a guild's rail icon. A muted guild
    /// only reports mentions.
    pub fn guild_unread(&self, guild_id: Snowflake) -> (bool, u32) {
        let muted = self.guild_muted(guild_id);
        let mut unread = false;
        let mut mentions = 0;
        for channel in self.channels.values() {
            if channel.guild_id != Some(guild_id)
                || !self.can(channel.id, permissions::VIEW_CHANNEL)
            {
                continue;
            }
            mentions += self.mention_count(channel.id);
            if !unread && !muted && !self.is_muted(channel.id) && self.has_unread(channel) {
                unread = true;
            }
        }
        (unread || mentions > 0, mentions)
    }

    /// Unread private channels with their badge counts, newest first.
    pub fn unread_dms(&self) -> Vec<(&Channel, u32)> {
        self.dm_channels()
            .into_iter()
            .filter(|channel| self.unread(channel.id))
            .map(|channel| (channel, self.mention_count(channel.id).max(1)))
            .collect()
    }

    // -- people ----------------------------------------------------------

    pub fn presence(&self, user_id: Snowflake) -> Option<&Presence> {
        self.presences.get(&user_id)
    }

    /// `online`, `idle`, `dnd` or `offline`. The account's own status comes
    /// from its sessions; the gateway never sends it a presence for itself.
    pub fn status_of(&self, user_id: Snowflake) -> &str {
        if self.my_id() == Some(user_id) {
            return match self.status.as_deref() {
                Some("invisible") | None => "offline",
                Some(status) => status,
            };
        }
        self.presences
            .get(&user_id)
            .map_or("offline", Presence::status)
    }

    /// Relationships of one kind with their users, sorted by name.
    pub fn relationships_of(&self, kind: u8) -> Vec<(&Relationship, &User)> {
        let mut list: Vec<(&Relationship, &User)> = self
            .relationships
            .values()
            .filter(|relationship| relationship.kind == kind)
            .filter_map(|relationship| Some((relationship, self.users.get(&relationship.id)?)))
            .collect();
        list.sort_by_key(|(_, user)| self.display_name(None, user).to_lowercase());
        list
    }

    pub fn is_blocked(&self, user_id: Snowflake) -> bool {
        self.relationships
            .get(&user_id)
            .is_some_and(|relationship| relationship.kind == Relationship::BLOCKED)
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

    pub fn guild_member_users(&self, guild_id: Snowflake) -> Vec<User> {
        let mut users: Vec<User> = self
            .guild_members
            .get(&guild_id)
            .into_iter()
            .flatten()
            .filter_map(|user_id| self.users.get(user_id).cloned())
            .collect();
        users.sort_by_key(|user| user.display_name().to_lowercase());
        users
    }

    pub fn all_user_ids(&self) -> HashSet<Snowflake> {
        self.users.keys().copied().collect()
    }

    // -- emoji -----------------------------------------------------------

    /// Finds a custom emoji by id in any guild.
    pub fn emoji(&self, emoji_id: Snowflake) -> Option<(&Guild, &Emoji)> {
        self.guilds.values().find_map(|guild| {
            guild
                .emojis
                .iter()
                .find(|emoji| emoji.id == emoji_id)
                .map(|emoji| (guild, emoji))
        })
    }

    /// Custom emoji the account can send in a context, grouped by guild with
    /// the current guild first. Without Nitro only static, unrestricted emoji
    /// of the current guild are sendable; with it, every available emoji.
    pub fn sendable_emojis(
        &self,
        guild_id: Option<Snowflake>,
        nitro: bool,
    ) -> Vec<(&Guild, Vec<&Emoji>)> {
        let me = self.my_id();
        let mut groups: Vec<(&Guild, Vec<&Emoji>)> = self
            .ordered_guilds()
            .into_iter()
            .filter(|guild| nitro || Some(guild.id) == guild_id)
            .map(|guild| {
                let my_roles = me
                    .and_then(|me| self.member(guild.id, me))
                    .map(|member| member.roles.as_slice())
                    .unwrap_or(&[]);
                let emojis: Vec<&Emoji> = guild
                    .emojis
                    .iter()
                    .filter(|emoji| {
                        emoji.available
                            && (nitro || !emoji.animated)
                            && (emoji.roles.is_empty()
                                || emoji.roles.iter().any(|role| my_roles.contains(role)))
                    })
                    .collect();
                (guild, emojis)
            })
            .filter(|(_, emojis)| !emojis.is_empty())
            .collect();
        groups.sort_by_key(|(guild, _)| Some(guild.id) != guild_id);
        groups
    }
}
