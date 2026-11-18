//! Permission bits and the effective-permission computation.

use kc_core::Snowflake;

use crate::channel::{Channel, Overwrite};
use crate::guild::{Guild, Member};

pub const ADMINISTRATOR: u64 = 1 << 3;
pub const MANAGE_CHANNELS: u64 = 1 << 4;
pub const ADD_REACTIONS: u64 = 1 << 6;
pub const VIEW_CHANNEL: u64 = 1 << 10;
pub const SEND_MESSAGES: u64 = 1 << 11;
pub const MANAGE_MESSAGES: u64 = 1 << 13;
pub const EMBED_LINKS: u64 = 1 << 14;
pub const ATTACH_FILES: u64 = 1 << 15;
pub const READ_MESSAGE_HISTORY: u64 = 1 << 16;
pub const MENTION_EVERYONE: u64 = 1 << 17;
pub const USE_EXTERNAL_EMOJIS: u64 = 1 << 18;
pub const CONNECT: u64 = 1 << 20;
pub const SPEAK: u64 = 1 << 21;
pub const CHANGE_NICKNAME: u64 = 1 << 26;
pub const MANAGE_NICKNAMES: u64 = 1 << 27;
pub const SEND_MESSAGES_IN_THREADS: u64 = 1 << 38;
pub const PIN_MESSAGES: u64 = 1 << 51;
pub const ALL: u64 = u64::MAX;

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
