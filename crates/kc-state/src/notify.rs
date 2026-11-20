//! Whether a new message should raise a desktop notification (and its
//! sound). The rules live in [`decide`], a pure function of the facts about
//! the message and the user's choices; [`State::message_facts`] gathers the
//! facts from the store: Discord's per-server and per-channel notification
//! settings and mutes, the server default, blocks, and mentions.

use kc_core::Snowflake;

use crate::message::Message;
use crate::settings::level;
use crate::state::State;

/// Why a message notifies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyReason {
    /// A message in a DM or group DM.
    DirectMessage,
    /// It mentions you by name, or replies to you with a ping.
    Mention,
    /// It mentions a role you have.
    RoleMention,
    /// `@everyone` or `@here`.
    Everyone,
    /// Any message, in a channel set to notify for all messages.
    Message,
}

impl NotifyReason {
    /// Whether this is a mention of some kind (for the mention sound).
    pub fn is_mention(self) -> bool {
        matches!(self, Self::Mention | Self::RoleMention | Self::Everyone)
    }
}

/// The user's own notification choices (kylecord's settings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotifyPrefs {
    /// Every message in a DM notifies; when off, only mentions there do.
    pub all_dm_messages: bool,
    /// Only mentions notify, whatever a server or channel is set to.
    pub mentions_only: bool,
    /// `@everyone` / `@here` notify (where the server allows it).
    pub everyone: bool,
    /// Mentions of your roles notify (where the server allows it).
    pub roles: bool,
}

impl Default for NotifyPrefs {
    fn default() -> Self {
        Self {
            all_dm_messages: true,
            mentions_only: false,
            everyone: true,
            roles: true,
        }
    }
}

/// What is known about one new message, for [`decide`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageFacts {
    /// You sent it.
    pub own: bool,
    /// Its author is someone you blocked.
    pub blocked: bool,
    /// Sent with `@silent` (the SUPPRESS_NOTIFICATIONS flag), or a kind
    /// that never notifies (a call's start).
    pub silent: bool,
    /// In a DM or group DM.
    pub direct: bool,
    /// Names you, or replies to you with a ping.
    pub mentions_me: bool,
    /// Names a role you have.
    pub mentions_my_role: bool,
    /// `@everyone` or `@here`.
    pub mentions_everyone: bool,
    /// The level in effect: the channel's, its category's, the server's
    /// own setting, or the server's default ([`level`]).
    pub level: u8,
    /// The channel, its category or the server is muted (or the DM).
    pub muted: bool,
    /// The server's settings drop `@everyone` / `@here`.
    pub suppress_everyone: bool,
    /// The server's settings drop role mentions.
    pub suppress_roles: bool,
    /// The channel is open in a window that has the focus.
    pub viewing: bool,
    /// Your status is Do Not Disturb.
    pub do_not_disturb: bool,
}

/// Whether a message notifies, and why. Muted places only notify for
/// mentions (as Discord: "unless you are mentioned"); "Nothing" never
/// does.
pub fn decide(facts: &MessageFacts, prefs: &NotifyPrefs) -> Option<NotifyReason> {
    if facts.own || facts.blocked || facts.silent || facts.viewing || facts.do_not_disturb {
        return None;
    }
    if facts.direct {
        if facts.muted {
            return None;
        }
        if facts.mentions_me {
            return Some(NotifyReason::Mention);
        }
        return prefs.all_dm_messages.then_some(NotifyReason::DirectMessage);
    }
    if facts.level == level::NO_MESSAGES {
        return None;
    }
    if facts.mentions_me {
        return Some(NotifyReason::Mention);
    }
    // A muted place notifies only for a direct @mention of you, not for a
    // role or @everyone/@here ping (as Discord).
    if facts.mentions_my_role && prefs.roles && !facts.suppress_roles && !facts.muted {
        return Some(NotifyReason::RoleMention);
    }
    if facts.mentions_everyone && prefs.everyone && !facts.suppress_everyone && !facts.muted {
        return Some(NotifyReason::Everyone);
    }
    let all = facts.level == level::ALL_MESSAGES;
    (all && !facts.muted && !prefs.mentions_only).then_some(NotifyReason::Message)
}

/// The SUPPRESS_NOTIFICATIONS message flag (`@silent`).
pub const SUPPRESS_NOTIFICATIONS: u64 = 1 << 12;
/// Message type of a call's start in a DM; the call itself rings.
const CALL_MESSAGE: u8 = 3;

impl State {
    /// The notification level in effect for a channel: its own setting,
    /// then its parent's (category, or a thread's channel and that one's
    /// category), then the server's, then the server's default.
    pub fn notification_level(&self, channel_id: Snowflake) -> u8 {
        let Some(channel) = self.channels.get(&channel_id) else {
            return level::ALL_MESSAGES;
        };
        let Some(guild_id) = channel.guild_id else {
            return level::ALL_MESSAGES;
        };
        let settings = self.guild_settings.get(&Some(guild_id));
        let mut current = Some(channel);
        // A thread, its channel, and that channel's category.
        for _ in 0..3 {
            let Some(here) = current else {
                break;
            };
            if let Some(own) = settings
                .and_then(|settings| settings.channel_overrides.get(&here.id))
                .map(|channel| channel.message_notifications)
                .filter(|level| *level != level::INHERIT)
            {
                return own;
            }
            current = here.parent_id.and_then(|parent| self.channels.get(&parent));
        }
        if let Some(own) = settings
            .map(|settings| settings.message_notifications)
            .filter(|level| *level != level::INHERIT)
        {
            return own;
        }
        self.guilds
            .get(&guild_id)
            .and_then(|guild| guild.default_message_notifications)
            .unwrap_or(level::ALL_MESSAGES)
    }

    /// Whether a channel is muted through itself, a parent, or its server
    /// (DMs: through the DM settings).
    fn muted_anywhere(&self, channel_id: Snowflake) -> bool {
        if self.is_muted(channel_id) {
            return true;
        }
        let Some(channel) = self.channels.get(&channel_id) else {
            return false;
        };
        // A thread's channel's category.
        if let Some(parent) = channel.parent_id
            && channel.kind.is_thread()
            && self.is_muted(parent)
        {
            return true;
        }
        channel
            .guild_id
            .is_some_and(|guild_id| self.guild_muted(guild_id))
    }

    /// The facts [`decide`] needs about a message in the store. `viewing`
    /// says whether its channel is open in a focused window.
    pub fn message_facts(&self, message: &Message, viewing: bool) -> MessageFacts {
        let me = self.my_id();
        let channel = self.channels.get(&message.channel_id);
        let guild_id = message
            .guild_id
            .or_else(|| channel.and_then(|channel| channel.guild_id));
        let settings = self.guild_settings.get(&guild_id);
        let mentions_me = me.is_some_and(|me| {
            message.mentions.iter().any(|user| user.id == me)
            // A reply pings its author by listing them in `mentions`;
            // without the ping they are not listed.
        });
        let mentions_my_role = match (guild_id, me) {
            (Some(guild_id), Some(me)) if !message.mention_roles.is_empty() => {
                self.members.get(&(guild_id, me)).is_some_and(|member| {
                    message
                        .mention_roles
                        .iter()
                        .any(|role| member.roles.contains(role))
                })
            }
            _ => false,
        };
        MessageFacts {
            own: me == Some(message.author.id),
            blocked: self.is_blocked(message.author.id),
            silent: message.flags & SUPPRESS_NOTIFICATIONS != 0 || message.kind == CALL_MESSAGE,
            direct: guild_id.is_none(),
            mentions_me,
            mentions_my_role,
            mentions_everyone: message.mention_everyone,
            level: if guild_id.is_some() {
                self.notification_level(message.channel_id)
            } else {
                level::ALL_MESSAGES
            },
            muted: self.muted_anywhere(message.channel_id),
            suppress_everyone: settings.is_some_and(|settings| settings.suppress_everyone),
            suppress_roles: settings.is_some_and(|settings| settings.suppress_roles),
            viewing,
            do_not_disturb: self.status.as_deref() == Some("dnd"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn guild_message() -> MessageFacts {
        MessageFacts {
            level: level::ALL_MESSAGES,
            ..MessageFacts::default()
        }
    }

    #[test]
    fn never_for_your_own_blocked_silent_viewed_or_while_dnd() {
        let prefs = NotifyPrefs::default();
        let base = MessageFacts {
            mentions_me: true,
            ..guild_message()
        };
        assert_eq!(decide(&base, &prefs), Some(NotifyReason::Mention));
        for facts in [
            MessageFacts { own: true, ..base },
            MessageFacts {
                blocked: true,
                ..base
            },
            MessageFacts {
                silent: true,
                ..base
            },
            MessageFacts {
                viewing: true,
                ..base
            },
            MessageFacts {
                do_not_disturb: true,
                ..base
            },
        ] {
            assert_eq!(decide(&facts, &prefs), None, "{facts:?}");
        }
    }

    #[test]
    fn direct_messages() {
        let mut prefs = NotifyPrefs::default();
        let dm = MessageFacts {
            direct: true,
            ..MessageFacts::default()
        };
        assert_eq!(decide(&dm, &prefs), Some(NotifyReason::DirectMessage));
        // A muted DM stays quiet, even for a ping.
        let muted = MessageFacts {
            muted: true,
            mentions_me: true,
            ..dm
        };
        assert_eq!(decide(&muted, &prefs), None);
        // With "all DM messages" off, only pings in DMs notify.
        prefs.all_dm_messages = false;
        assert_eq!(decide(&dm, &prefs), None);
        let ping = MessageFacts {
            mentions_me: true,
            ..dm
        };
        assert_eq!(decide(&ping, &prefs), Some(NotifyReason::Mention));
        // Server levels do not apply to DMs.
        let nothing = MessageFacts {
            level: level::NO_MESSAGES,
            ..dm
        };
        assert_eq!(
            decide(&nothing, &NotifyPrefs::default()),
            Some(NotifyReason::DirectMessage)
        );
    }

    #[test]
    fn server_levels() {
        let prefs = NotifyPrefs::default();
        let all = guild_message();
        assert_eq!(decide(&all, &prefs), Some(NotifyReason::Message));
        let only = MessageFacts {
            level: level::ONLY_MENTIONS,
            ..all
        };
        assert_eq!(decide(&only, &prefs), None);
        assert_eq!(
            decide(
                &MessageFacts {
                    mentions_me: true,
                    ..only
                },
                &prefs
            ),
            Some(NotifyReason::Mention)
        );
        // Nothing means nothing, pings included.
        let nothing = MessageFacts {
            level: level::NO_MESSAGES,
            mentions_me: true,
            mentions_everyone: true,
            ..all
        };
        assert_eq!(decide(&nothing, &prefs), None);
        // Mentions only, kylecord-wide, overrides "all messages".
        let mentions_only = NotifyPrefs {
            mentions_only: true,
            ..prefs
        };
        assert_eq!(decide(&all, &mentions_only), None);
        assert_eq!(
            decide(
                &MessageFacts {
                    mentions_me: true,
                    ..all
                },
                &mentions_only
            ),
            Some(NotifyReason::Mention)
        );
    }

    #[test]
    fn muted_places_notify_only_for_mentions() {
        let prefs = NotifyPrefs::default();
        let muted = MessageFacts {
            muted: true,
            ..guild_message()
        };
        assert_eq!(decide(&muted, &prefs), None);
        assert_eq!(
            decide(
                &MessageFacts {
                    mentions_me: true,
                    ..muted
                },
                &prefs
            ),
            Some(NotifyReason::Mention)
        );
        // A muted place does NOT notify for a role or @everyone ping, only a
        // direct @mention of you.
        assert_eq!(
            decide(
                &MessageFacts {
                    mentions_my_role: true,
                    ..muted
                },
                &prefs
            ),
            None
        );
        assert_eq!(
            decide(
                &MessageFacts {
                    mentions_everyone: true,
                    ..muted
                },
                &prefs
            ),
            None
        );
        // But an unmuted place still does.
        assert_eq!(
            decide(
                &MessageFacts {
                    mentions_everyone: true,
                    ..guild_message()
                },
                &prefs
            ),
            Some(NotifyReason::Everyone)
        );
    }

    #[test]
    fn everyone_and_roles_follow_server_and_app_settings() {
        let prefs = NotifyPrefs::default();
        let only = MessageFacts {
            level: level::ONLY_MENTIONS,
            ..MessageFacts::default()
        };
        let everyone = MessageFacts {
            mentions_everyone: true,
            ..only
        };
        let role = MessageFacts {
            mentions_my_role: true,
            ..only
        };
        assert_eq!(decide(&everyone, &prefs), Some(NotifyReason::Everyone));
        assert_eq!(decide(&role, &prefs), Some(NotifyReason::RoleMention));
        assert_eq!(
            decide(
                &MessageFacts {
                    suppress_everyone: true,
                    ..everyone
                },
                &prefs
            ),
            None
        );
        assert_eq!(
            decide(
                &MessageFacts {
                    suppress_roles: true,
                    ..role
                },
                &prefs
            ),
            None
        );
        let off = NotifyPrefs {
            everyone: false,
            roles: false,
            ..prefs
        };
        assert_eq!(decide(&everyone, &off), None);
        assert_eq!(decide(&role, &off), None);
        // A direct ping still notifies with both off.
        assert_eq!(
            decide(
                &MessageFacts {
                    mentions_me: true,
                    ..everyone
                },
                &off
            ),
            Some(NotifyReason::Mention)
        );
        assert!(NotifyReason::Everyone.is_mention() && !NotifyReason::Message.is_mention());
    }

    /// A store with you (1), a friend (2), someone blocked (3), a server
    /// (500) whose default is "only mentions", and a DM.
    fn store() -> State {
        let mut state = State::default();
        state.apply(
            "READY",
            &json!({
                "user": {"id": "1", "username": "me"},
                "users": [
                    {"id": "2", "username": "friend"},
                    {"id": "3", "username": "pest"}
                ],
                "guilds": [{
                    "id": "500", "name": "Server", "default_message_notifications": 1,
                    "roles": [{"id": "500", "name": "@everyone", "permissions": "0", "position": 0},
                              {"id": "77", "name": "Crew", "permissions": "0", "position": 1}],
                    "channels": [
                        {"id": "600", "type": 4, "name": "Talk"},
                        {"id": "601", "type": 0, "name": "general", "parent_id": "600"},
                        {"id": "602", "type": 0, "name": "loud", "parent_id": "600"},
                        {"id": "603", "type": 0, "name": "quiet"},
                        {"id": "604", "type": 0, "name": "news"}
                    ]
                }],
                "merged_members": [[{"user_id": "1", "roles": ["77"]}]],
                "private_channels": [{"id": "900", "type": 1, "recipient_ids": ["2"]}],
                "relationships": [
                    {"id": "2", "type": 1, "user_id": "2"},
                    {"id": "3", "type": 2, "user_id": "3"}
                ],
                "read_state": {"entries": []},
                "user_guild_settings": {"entries": [{
                    "guild_id": "500", "message_notifications": 3, "suppress_everyone": false,
                    "channel_overrides": [
                        {"channel_id": "600", "message_notifications": 0, "muted": false},
                        {"channel_id": "602", "message_notifications": 3, "muted": false},
                        {"channel_id": "603", "message_notifications": 2, "muted": false},
                        {"channel_id": "604", "message_notifications": 3, "muted": true, "mute_config": null}
                    ]
                }]},
                "user_settings": {"status": "online"}
            }),
        );
        state
    }

    fn message(channel: &str, author: &str, extra: serde_json::Value) -> Message {
        let mut value = json!({
            "id": "1000", "channel_id": channel, "author": {"id": author, "username": "x"},
            "content": "hello", "mentions": []
        });
        if let (Some(base), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            for (key, item) in extra {
                base.insert(key.clone(), item.clone());
            }
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn levels_come_from_channel_then_category_then_server_default() {
        let state = store();
        // The category says all messages; general and loud inherit it.
        assert_eq!(
            state.notification_level(Snowflake(601)),
            level::ALL_MESSAGES
        );
        assert_eq!(
            state.notification_level(Snowflake(602)),
            level::ALL_MESSAGES
        );
        assert_eq!(state.notification_level(Snowflake(603)), level::NO_MESSAGES);
        // No category, server entry inherits: the server's default.
        assert_eq!(
            state.notification_level(Snowflake(604)),
            level::ONLY_MENTIONS
        );
    }

    #[test]
    fn facts_from_the_store() {
        let mut state = store();
        let prefs = NotifyPrefs::default();
        let notify = |state: &State, message: &Message, viewing: bool| {
            decide(&state.message_facts(message, viewing), &prefs)
        };
        let plain = message("601", "2", json!({}));
        assert_eq!(notify(&state, &plain, false), Some(NotifyReason::Message));
        assert_eq!(notify(&state, &plain, true), None);
        // Your own message, and a blocked author's.
        assert_eq!(notify(&state, &message("601", "1", json!({})), false), None);
        assert_eq!(notify(&state, &message("601", "3", json!({})), false), None);
        // @silent.
        let silent = message("601", "2", json!({"flags": SUPPRESS_NOTIFICATIONS}));
        assert_eq!(notify(&state, &silent, false), None);
        // A muted channel only for a ping; a reply that pings lists you.
        assert_eq!(notify(&state, &message("604", "2", json!({})), false), None);
        let reply = message(
            "604",
            "2",
            json!({"type": 19, "mentions": [{"id": "1", "username": "me"}],
                   "referenced_message": {"id": "999", "channel_id": "604",
                       "author": {"id": "1", "username": "me"}, "content": "hi"}}),
        );
        assert_eq!(notify(&state, &reply, false), Some(NotifyReason::Mention));
        // Channel 604 is muted, so a role you have does NOT notify there —
        // only a direct @mention does (checked above). A RoleMention in a
        // non-muted channel is covered by
        // `everyone_and_roles_follow_server_and_app_settings`.
        let role = message("604", "2", json!({"mention_roles": ["77"]}));
        assert_eq!(notify(&state, &role, false), None);
        let other_role = message("604", "2", json!({"mention_roles": ["78"]}));
        assert_eq!(notify(&state, &other_role, false), None);
        // DMs, and a DM call's start message (the ring covers it).
        assert_eq!(
            notify(&state, &message("900", "2", json!({})), false),
            Some(NotifyReason::DirectMessage)
        );
        assert_eq!(
            notify(&state, &message("900", "2", json!({"type": 3})), false),
            None
        );
        // Do Not Disturb silences everything.
        state.status = Some("dnd".to_owned());
        assert_eq!(notify(&state, &reply, false), None);
    }
}
