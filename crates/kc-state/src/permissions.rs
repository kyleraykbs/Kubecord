//! Permission bits and the effective-permission computation.

use kc_core::Snowflake;

use crate::channel::{Channel, Overwrite};
use crate::guild::{Guild, Member};

pub const CREATE_INSTANT_INVITE: u64 = 1 << 0;
pub const KICK_MEMBERS: u64 = 1 << 1;
pub const BAN_MEMBERS: u64 = 1 << 2;
pub const ADMINISTRATOR: u64 = 1 << 3;
pub const MANAGE_CHANNELS: u64 = 1 << 4;
pub const MANAGE_GUILD: u64 = 1 << 5;
pub const ADD_REACTIONS: u64 = 1 << 6;
pub const VIEW_AUDIT_LOG: u64 = 1 << 7;
pub const PRIORITY_SPEAKER: u64 = 1 << 8;
pub const STREAM: u64 = 1 << 9;
pub const VIEW_CHANNEL: u64 = 1 << 10;
pub const SEND_MESSAGES: u64 = 1 << 11;
pub const SEND_TTS_MESSAGES: u64 = 1 << 12;
pub const MANAGE_MESSAGES: u64 = 1 << 13;
pub const EMBED_LINKS: u64 = 1 << 14;
pub const ATTACH_FILES: u64 = 1 << 15;
pub const READ_MESSAGE_HISTORY: u64 = 1 << 16;
pub const MENTION_EVERYONE: u64 = 1 << 17;
pub const USE_EXTERNAL_EMOJIS: u64 = 1 << 18;
pub const VIEW_GUILD_INSIGHTS: u64 = 1 << 19;
pub const CONNECT: u64 = 1 << 20;
pub const SPEAK: u64 = 1 << 21;
pub const MUTE_MEMBERS: u64 = 1 << 22;
pub const DEAFEN_MEMBERS: u64 = 1 << 23;
pub const MOVE_MEMBERS: u64 = 1 << 24;
pub const USE_VAD: u64 = 1 << 25;
pub const CHANGE_NICKNAME: u64 = 1 << 26;
pub const MANAGE_NICKNAMES: u64 = 1 << 27;
pub const MANAGE_ROLES: u64 = 1 << 28;
pub const MANAGE_WEBHOOKS: u64 = 1 << 29;
pub const MANAGE_GUILD_EXPRESSIONS: u64 = 1 << 30;
pub const USE_APPLICATION_COMMANDS: u64 = 1 << 31;
pub const REQUEST_TO_SPEAK: u64 = 1 << 32;
pub const MANAGE_EVENTS: u64 = 1 << 33;
pub const MANAGE_THREADS: u64 = 1 << 34;
pub const CREATE_PUBLIC_THREADS: u64 = 1 << 35;
pub const CREATE_PRIVATE_THREADS: u64 = 1 << 36;
pub const USE_EXTERNAL_STICKERS: u64 = 1 << 37;
pub const SEND_MESSAGES_IN_THREADS: u64 = 1 << 38;
pub const USE_EMBEDDED_ACTIVITIES: u64 = 1 << 39;
pub const MODERATE_MEMBERS: u64 = 1 << 40;
pub const USE_SOUNDBOARD: u64 = 1 << 42;
pub const CREATE_GUILD_EXPRESSIONS: u64 = 1 << 43;
pub const CREATE_EVENTS: u64 = 1 << 44;
pub const USE_EXTERNAL_SOUNDS: u64 = 1 << 45;
pub const SEND_VOICE_MESSAGES: u64 = 1 << 46;
pub const SEND_POLLS: u64 = 1 << 49;
pub const USE_EXTERNAL_APPS: u64 = 1 << 50;
pub const PIN_MESSAGES: u64 = 1 << 51;
pub const ALL: u64 = u64::MAX;

/// One permission as the role and channel editors show it.
#[derive(Clone, Copy, Debug)]
pub struct PermissionInfo {
    pub bit: u64,
    pub name: &'static str,
    pub description: &'static str,
    /// Meaningful in a channel's permission overwrites (not server-only).
    pub in_channels: bool,
}

const fn info(
    bit: u64,
    name: &'static str,
    description: &'static str,
    in_channels: bool,
) -> PermissionInfo {
    PermissionInfo {
        bit,
        name,
        description,
        in_channels,
    }
}

/// The permissions the editors offer, grouped as Discord's own editor
/// groups them.
pub const GROUPS: &[(&str, &[PermissionInfo])] = &[
    (
        "General Server Permissions",
        &[
            info(
                VIEW_CHANNEL,
                "View Channels",
                "Members can see channels by default (excluding private channels).",
                true,
            ),
            info(
                MANAGE_CHANNELS,
                "Manage Channels",
                "Create, edit or delete channels.",
                true,
            ),
            info(
                MANAGE_ROLES,
                "Manage Roles",
                "Create new roles and edit or delete roles lower than their highest role. In a channel: manage its permissions.",
                true,
            ),
            info(
                CREATE_GUILD_EXPRESSIONS,
                "Create Expressions",
                "Add custom emoji, stickers and sounds.",
                false,
            ),
            info(
                MANAGE_GUILD_EXPRESSIONS,
                "Manage Expressions",
                "Edit or remove custom emoji, stickers and sounds.",
                false,
            ),
            info(
                VIEW_AUDIT_LOG,
                "View Audit Log",
                "See a record of who made which changes in this server.",
                false,
            ),
            info(
                VIEW_GUILD_INSIGHTS,
                "View Server Insights",
                "See the server's analytics.",
                false,
            ),
            info(
                MANAGE_WEBHOOKS,
                "Manage Webhooks",
                "Create, edit or delete webhooks.",
                true,
            ),
            info(
                MANAGE_GUILD,
                "Manage Server",
                "Change the server's name, region and other settings, and see all invites.",
                false,
            ),
        ],
    ),
    (
        "Membership Permissions",
        &[
            info(
                CREATE_INSTANT_INVITE,
                "Create Invite",
                "Invite new people to this server.",
                true,
            ),
            info(
                CHANGE_NICKNAME,
                "Change Nickname",
                "Change their own nickname in this server.",
                false,
            ),
            info(
                MANAGE_NICKNAMES,
                "Manage Nicknames",
                "Change the nicknames of other members.",
                false,
            ),
            info(
                KICK_MEMBERS,
                "Kick Members",
                "Remove other members from this server.",
                false,
            ),
            info(
                BAN_MEMBERS,
                "Ban Members",
                "Permanently remove other members and delete their recent messages.",
                false,
            ),
            info(
                MODERATE_MEMBERS,
                "Time Out Members",
                "Stop members from chatting, reacting or speaking for a while.",
                false,
            ),
        ],
    ),
    (
        "Text Channel Permissions",
        &[
            info(
                SEND_MESSAGES,
                "Send Messages and Create Posts",
                "Send messages in text channels and create forum posts.",
                true,
            ),
            info(
                SEND_MESSAGES_IN_THREADS,
                "Send Messages in Threads and Posts",
                "Send messages in threads and forum posts.",
                true,
            ),
            info(
                CREATE_PUBLIC_THREADS,
                "Create Public Threads",
                "Start threads everyone in the channel can see.",
                true,
            ),
            info(
                CREATE_PRIVATE_THREADS,
                "Create Private Threads",
                "Start invite-only threads.",
                true,
            ),
            info(
                EMBED_LINKS,
                "Embed Links",
                "Links they share show embedded content.",
                true,
            ),
            info(
                ATTACH_FILES,
                "Attach Files",
                "Upload files and media.",
                true,
            ),
            info(
                ADD_REACTIONS,
                "Add Reactions",
                "Add new emoji reactions to messages.",
                true,
            ),
            info(
                USE_EXTERNAL_EMOJIS,
                "Use External Emoji",
                "Use emoji from other servers.",
                true,
            ),
            info(
                USE_EXTERNAL_STICKERS,
                "Use External Stickers",
                "Use stickers from other servers.",
                true,
            ),
            info(
                MENTION_EVERYONE,
                "Mention @everyone, @here and All Roles",
                "Notify everyone, everyone online, or any role.",
                true,
            ),
            info(
                MANAGE_MESSAGES,
                "Manage Messages",
                "Delete messages by other members.",
                true,
            ),
            info(
                PIN_MESSAGES,
                "Pin Messages",
                "Pin or unpin any message.",
                true,
            ),
            info(
                MANAGE_THREADS,
                "Manage Threads and Posts",
                "Rename, delete, close and set slowmode for threads and posts.",
                true,
            ),
            info(
                READ_MESSAGE_HISTORY,
                "Read Message History",
                "Read messages sent before they arrived.",
                true,
            ),
            info(
                SEND_TTS_MESSAGES,
                "Send Text-to-Speech Messages",
                "Send messages read aloud with /tts.",
                true,
            ),
            info(
                SEND_VOICE_MESSAGES,
                "Send Voice Messages",
                "Send voice messages.",
                true,
            ),
            info(SEND_POLLS, "Create Polls", "Create polls.", true),
        ],
    ),
    (
        "Voice Channel Permissions",
        &[
            info(
                CONNECT,
                "Connect",
                "Join voice channels and hear others.",
                true,
            ),
            info(SPEAK, "Speak", "Talk in voice channels.", true),
            info(
                STREAM,
                "Video",
                "Share their camera, screen or a game.",
                true,
            ),
            info(
                USE_SOUNDBOARD,
                "Use Soundboard",
                "Play sounds from the server soundboard.",
                true,
            ),
            info(
                USE_EXTERNAL_SOUNDS,
                "Use External Sounds",
                "Play sounds from other servers.",
                true,
            ),
            info(
                USE_VAD,
                "Use Voice Activity",
                "Talk by simply speaking; without it they must use Push to Talk.",
                true,
            ),
            info(
                PRIORITY_SPEAKER,
                "Priority Speaker",
                "Be heard more easily: others are quieter while they talk.",
                true,
            ),
            info(
                MUTE_MEMBERS,
                "Mute Members",
                "Mute other members in voice for everyone.",
                true,
            ),
            info(
                DEAFEN_MEMBERS,
                "Deafen Members",
                "Deafen other members in voice.",
                true,
            ),
            info(
                MOVE_MEMBERS,
                "Move Members",
                "Disconnect or move other members between voice channels.",
                true,
            ),
        ],
    ),
    (
        "Apps and Events",
        &[
            info(
                USE_APPLICATION_COMMANDS,
                "Use Application Commands",
                "Use commands from apps and bots.",
                true,
            ),
            info(
                USE_EMBEDDED_ACTIVITIES,
                "Use Activities",
                "Use activities in voice and text.",
                true,
            ),
            info(
                USE_EXTERNAL_APPS,
                "Use External Apps",
                "Apps they added themselves may post here.",
                true,
            ),
            info(
                CREATE_EVENTS,
                "Create Events",
                "Create server events.",
                true,
            ),
            info(
                MANAGE_EVENTS,
                "Manage Events",
                "Edit and cancel events.",
                true,
            ),
            info(
                REQUEST_TO_SPEAK,
                "Request to Speak",
                "Ask to speak in stage channels.",
                true,
            ),
        ],
    ),
    (
        "Advanced Permissions",
        &[info(
            ADMINISTRATOR,
            "Administrator",
            "Every permission, bypassing every channel's overwrites. Dangerous to grant.",
            false,
        )],
    ),
];

/// Guild-wide permissions of a member: owner and administrators get
/// everything; otherwise `@everyone` (the role whose id is the guild id) OR-ed
/// with each of the member's roles.
pub fn base(guild: &Guild, user_id: Snowflake, member_roles: &[Snowflake]) -> u64 {
    if guild.owner_id == Some(user_id) {
        return ALL;
    }
    let mut permissions = 0;
    for role in &guild.roles {
        if role.id == guild.id || member_roles.contains(&role.id) {
            permissions |= role.permissions;
        }
    }
    if permissions & ADMINISTRATOR != 0 {
        return ALL;
    }
    permissions
}

/// Applies channel overwrites in Discord's order: `@everyone`, then all of
/// the member's role overwrites together, then the member's own overwrite.
pub fn apply_overwrites(
    mut permissions: u64,
    guild_id: Snowflake,
    user_id: Snowflake,
    member_roles: &[Snowflake],
    overwrites: &[Overwrite],
) -> u64 {
    if permissions == ALL {
        return ALL;
    }
    if let Some(everyone) = overwrites.iter().find(|overwrite| overwrite.id == guild_id) {
        permissions &= !everyone.deny;
        permissions |= everyone.allow;
    }
    let (mut allow, mut deny) = (0, 0);
    for overwrite in overwrites {
        if overwrite.kind == Overwrite::ROLE
            && overwrite.id != guild_id
            && member_roles.contains(&overwrite.id)
        {
            allow |= overwrite.allow;
            deny |= overwrite.deny;
        }
    }
    permissions &= !deny;
    permissions |= allow;
    if let Some(own) = overwrites
        .iter()
        .find(|overwrite| overwrite.kind == Overwrite::MEMBER && overwrite.id == user_id)
    {
        permissions &= !own.deny;
        permissions |= own.allow;
    }
    permissions
}

/// Effective permissions of a member in a guild channel. `overwrite_source`
/// is the channel itself, or the parent channel for a thread (threads carry no
/// overwrites of their own).
pub fn in_channel(
    guild: &Guild,
    overwrite_source: &Channel,
    user_id: Snowflake,
    member: Option<&Member>,
) -> u64 {
    let roles: &[Snowflake] = member.map(|member| member.roles.as_slice()).unwrap_or(&[]);
    apply_overwrites(
        base(guild, user_id, roles),
        guild.id,
        user_id,
        roles,
        &overwrite_source.permission_overwrites,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guild::Role;

    fn guild() -> Guild {
        Guild {
            id: Snowflake(1),
            name: "g".into(),
            owner_id: Some(Snowflake(99)),
            roles: vec![
                Role {
                    id: Snowflake(1),
                    permissions: VIEW_CHANNEL | SEND_MESSAGES,
                    ..Role::default()
                },
                Role {
                    id: Snowflake(2),
                    permissions: MANAGE_MESSAGES,
                    ..Role::default()
                },
                Role {
                    id: Snowflake(3),
                    permissions: ADMINISTRATOR,
                    ..Role::default()
                },
            ],
            ..Guild::default()
        }
    }

    #[test]
    fn the_catalogue_lists_each_bit_once() {
        let mut seen = 0u64;
        for (_, permissions) in GROUPS {
            for permission in *permissions {
                assert_eq!(permission.bit.count_ones(), 1, "{}", permission.name);
                assert_eq!(seen & permission.bit, 0, "{} twice", permission.name);
                seen |= permission.bit;
            }
        }
        assert_ne!(seen & ADMINISTRATOR, 0);
    }

    #[test]
    fn owner_and_admin_get_everything() {
        let guild = guild();
        assert_eq!(base(&guild, Snowflake(99), &[]), ALL);
        assert_eq!(base(&guild, Snowflake(5), &[Snowflake(3)]), ALL);
    }

    #[test]
    fn roles_are_ored_onto_everyone() {
        let guild = guild();
        let permissions = base(&guild, Snowflake(5), &[Snowflake(2)]);
        assert_eq!(permissions, VIEW_CHANNEL | SEND_MESSAGES | MANAGE_MESSAGES);
    }

    #[test]
    fn overwrites_apply_everyone_then_roles_then_member() {
        let base = VIEW_CHANNEL | SEND_MESSAGES;
        let overwrites = vec![
            // @everyone cannot see the channel...
            Overwrite {
                id: Snowflake(1),
                kind: Overwrite::ROLE,
                allow: 0,
                deny: VIEW_CHANNEL,
            },
            // ...the member's role can...
            Overwrite {
                id: Snowflake(2),
                kind: Overwrite::ROLE,
                allow: VIEW_CHANNEL,
                deny: 0,
            },
            // ...but this member specifically cannot send.
            Overwrite {
                id: Snowflake(5),
                kind: Overwrite::MEMBER,
                allow: 0,
                deny: SEND_MESSAGES,
            },
        ];
        let with_role = apply_overwrites(
            base,
            Snowflake(1),
            Snowflake(5),
            &[Snowflake(2)],
            &overwrites,
        );
        assert_eq!(with_role, VIEW_CHANNEL);
        let without_role = apply_overwrites(base, Snowflake(1), Snowflake(6), &[], &overwrites);
        assert_eq!(without_role, SEND_MESSAGES);
    }
}
