//! What the account may do to other members and to roles: Discord's role
//! hierarchy on top of the computed permissions.
//!
//! Rules (as Discord enforces them):
//! - the owner may act on anyone but themselves, and on every role;
//! - nobody may act on the owner;
//! - otherwise the actor's highest role must be above the target's highest
//!   role (for members) or above the role itself (for roles). Administrator
//!   grants every permission but does not lift the hierarchy;
//! - members with Administrator cannot be timed out;
//! - roles owned by an integration (`managed`) and `@everyone` cannot be
//!   assigned or removed.
//!
//! Voice moderation (move, disconnect, server mute and deafen) is checked
//! against the permissions in the voice channel, without the hierarchy.

use std::cmp::Reverse;

use kc_core::Snowflake;

use crate::guild::{Guild, Role};
use crate::permissions::{self, ALL};
use crate::state::State;

/// Where a role (or a member's highest role) sits: higher compares greater.
/// Equal positions are broken by the older (smaller) id ranking higher.
pub type Rank = (i32, Reverse<Snowflake>);

/// The rank of a role.
pub fn role_rank(role: &Role) -> Rank {
    (role.position, Reverse(role.id))
}

/// The rank of a member's highest role; `@everyone` when they have none.
pub fn member_rank(guild: &Guild, roles: &[Snowflake]) -> Rank {
    guild
        .roles
        .iter()
        .filter(|role| roles.contains(&role.id))
        .map(role_rank)
        .max()
        .unwrap_or((0, Reverse(guild.id)))
}

/// Whether `actor` outranks `target` in the hierarchy (owner rules
/// included). Nobody outranks themselves.
pub fn outranks(
    guild: &Guild,
    actor: Snowflake,
    actor_roles: &[Snowflake],
    target: Snowflake,
    target_roles: &[Snowflake],
) -> bool {
    if actor == target || guild.owner_id == Some(target) {
        return false;
    }
    if guild.owner_id == Some(actor) {
        return true;
    }
    member_rank(guild, actor_roles) > member_rank(guild, target_roles)
}

/// Whether `actor` may edit, delete, assign or remove `role`.
pub fn can_manage_role(
    guild: &Guild,
    actor: Snowflake,
    actor_roles: &[Snowflake],
    role: &Role,
) -> bool {
    if guild.owner_id == Some(actor) {
        return true;
    }
    let granted = permissions::base(guild, actor, actor_roles);
    granted & permissions::MANAGE_ROLES != 0 && member_rank(guild, actor_roles) > role_rank(role)
}

/// Whether `role` can be put on or taken off a member at all.
pub fn is_assignable(guild: &Guild, role: &Role) -> bool {
    role.id != guild.id && !role.managed
}

/// What the account may do to one member. Every field is already gated on
/// permissions and the hierarchy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemberActions {
    pub kick: bool,
    pub ban: bool,
    pub timeout: bool,
    /// The member is timed out now (so "Remove Timeout" applies).
    pub timed_out: bool,
    pub nickname: bool,
    /// At least one role exists that the account may assign or remove.
    pub roles: bool,
    /// Voice: the member is in a voice channel of this guild.
    pub in_voice: bool,
    pub move_member: bool,
    pub disconnect: bool,
    pub server_mute: bool,
    pub server_deafen: bool,
    /// Their current server mute and deafen state.
    pub muted: bool,
    pub deafened: bool,
}

impl MemberActions {
    /// Any moderation item to show.
    pub fn any(&self) -> bool {
        self.kick
            || self.ban
            || self.timeout
            || self.nickname
            || self.roles
            || self.move_member
            || self.disconnect
            || self.server_mute
            || self.server_deafen
    }
}

impl State {
    /// The account's guild-wide permissions (no channel overwrites).
    pub fn guild_permissions(&self, guild_id: Snowflake) -> u64 {
        let (Some(guild), Some(me)) = (self.guilds.get(&guild_id), self.my_id()) else {
            return 0;
        };
        if guild.owner_id == Some(me) {
            return ALL;
        }
        let roles = self.member_roles(guild_id, me);
        permissions::base(guild, me, &roles)
    }

    /// Whether the account has `permission` across the guild.
    pub fn can_in_guild(&self, guild_id: Snowflake, permission: u64) -> bool {
        self.guild_permissions(guild_id) & permission != 0
    }

    fn member_roles(&self, guild_id: Snowflake, user_id: Snowflake) -> Vec<Snowflake> {
        self.member(guild_id, user_id)
            .map(|member| member.roles.clone())
            .unwrap_or_default()
    }

    /// The account's rank in a guild (`None` if the guild is unknown).
    pub fn my_rank(&self, guild_id: Snowflake) -> Option<Rank> {
        let guild = self.guilds.get(&guild_id)?;
        let me = self.my_id()?;
        Some(member_rank(guild, &self.member_roles(guild_id, me)))
    }

    /// Whether the account may edit, delete, assign or remove a role.
    pub fn can_manage_role(&self, guild_id: Snowflake, role: &Role) -> bool {
        let (Some(guild), Some(me)) = (self.guilds.get(&guild_id), self.my_id()) else {
            return false;
        };
        can_manage_role(guild, me, &self.member_roles(guild_id, me), role)
    }

    /// The guild's roles from the top of the hierarchy down, `@everyone`
    /// last.
    pub fn roles_by_rank(&self, guild_id: Snowflake) -> Vec<Role> {
        let mut roles = self
            .guilds
            .get(&guild_id)
            .map(|guild| guild.roles.clone())
            .unwrap_or_default();
        roles.sort_by_key(|role| Reverse(role_rank(role)));
        roles
    }

    /// Roles the account may put on or take off members, highest first.
    pub fn assignable_roles(&self, guild_id: Snowflake) -> Vec<Role> {
        let Some(guild) = self.guilds.get(&guild_id) else {
            return Vec::new();
        };
        self.roles_by_rank(guild_id)
            .into_iter()
            .filter(|role| is_assignable(guild, role) && self.can_manage_role(guild_id, role))
            .collect()
    }

    /// What the account may do to `target` in a guild, at `now_millis`.
    pub fn member_actions(
        &self,
        guild_id: Snowflake,
        target: Snowflake,
        now_millis: u64,
    ) -> MemberActions {
        let (Some(guild), Some(me)) = (self.guilds.get(&guild_id), self.my_id()) else {
            return MemberActions::default();
        };
        let my_roles = self.member_roles(guild_id, me);
        let target_roles = self.member_roles(guild_id, target);
        let granted = self.guild_permissions(guild_id);
        let has = |permission: u64| granted & permission != 0;
        let above = outranks(guild, me, &my_roles, target, &target_roles);
        let target_admin = guild.owner_id == Some(target)
            || permissions::base(guild, target, &target_roles) & permissions::ADMINISTRATOR != 0;
        let member = self.member(guild_id, target);

        let mut actions = MemberActions {
            kick: has(permissions::KICK_MEMBERS) && above,
            ban: has(permissions::BAN_MEMBERS) && above,
            timeout: has(permissions::MODERATE_MEMBERS) && above && !target_admin,
            timed_out: member.is_some_and(|member| member.timed_out_at(now_millis)),
            nickname: if target == me {
                has(permissions::CHANGE_NICKNAME) || has(permissions::MANAGE_NICKNAMES)
            } else {
                has(permissions::MANAGE_NICKNAMES) && above
            },
            // Your own roles follow the same rule: below your highest role.
            roles: (above || target == me) && !self.assignable_roles(guild_id).is_empty(),
            ..MemberActions::default()
        };

        if let Some(voice) = self.voice_states.get(&(Some(guild_id), target))
            && let Some(channel_id) = voice.channel_id
        {
            let here = self.permissions_in(channel_id);
            actions.in_voice = true;
            actions.muted = voice.mute;
            actions.deafened = voice.deaf;
            actions.move_member = here & permissions::MOVE_MEMBERS != 0;
            actions.disconnect = actions.move_member;
            actions.server_mute = here & permissions::MUTE_MEMBERS != 0;
            actions.server_deafen = here & permissions::DEAFEN_MEMBERS != 0;
        }
        actions
    }

    /// Voice channels of a guild someone may be moved to by the account:
    /// it needs Move Members there, and the channel must be visible and
    /// joinable for the account.
    pub fn move_targets(&self, guild_id: Snowflake) -> Vec<(Snowflake, String)> {
        let mut channels: Vec<_> = self
            .channels
            .values()
            .filter(|channel| {
                channel.guild_id == Some(guild_id)
                    && matches!(
                        channel.kind,
                        crate::ChannelType::GuildVoice | crate::ChannelType::GuildStageVoice
                    )
            })
            .filter(|channel| {
                let here = self.permissions_in(channel.id);
                here & permissions::VIEW_CHANNEL != 0
                    && here & (permissions::CONNECT | permissions::MOVE_MEMBERS)
                        == permissions::CONNECT | permissions::MOVE_MEMBERS
            })
            .map(|channel| {
                let parent = channel
                    .parent_id
                    .and_then(|id| self.channels.get(&id))
                    .map(|parent| parent.sort_key());
                (
                    parent,
                    channel.sort_key(),
                    channel.id,
                    channel.display_name(),
                )
            })
            .collect();
        channels.sort();
        channels
            .into_iter()
            .map(|(_, _, id, name)| (id, name))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::permissions::*;

    const GUILD: u64 = 10;
    const OWNER: u64 = 1;
    const MOD: u64 = 2;
    const MEMBER: u64 = 3;
    const ADMIN: u64 = 4;
    const PEER: u64 = 5;

    /// Roles: Admin (pos 4, administrator), Mod (pos 3, moderation),
    /// Bot (pos 2, managed), Member (pos 1), @everyone.
    fn state() -> State {
        let mut state = State::default();
        let all_mod = KICK_MEMBERS
            | BAN_MEMBERS
            | MODERATE_MEMBERS
            | MANAGE_NICKNAMES
            | MANAGE_ROLES
            | MOVE_MEMBERS
            | MUTE_MEMBERS;
        state.apply(
            "READY",
            &json!({
                "user": {"id": MOD.to_string(), "username": "mod"},
                "guilds": [{
                    "id": GUILD.to_string(), "name": "g", "owner_id": OWNER.to_string(),
                    "roles": [
                        {"id": GUILD.to_string(), "name": "@everyone", "position": 0,
                         "permissions": (VIEW_CHANNEL | CONNECT | CHANGE_NICKNAME).to_string()},
                        {"id": "100", "name": "Admin", "position": 4, "permissions": ADMINISTRATOR.to_string()},
                        {"id": "101", "name": "Mod", "position": 3, "permissions": all_mod.to_string()},
                        {"id": "102", "name": "Bot", "position": 2, "permissions": "0", "managed": true},
                        {"id": "103", "name": "Member", "position": 1, "permissions": "0"}
                    ],
                    "channels": [
                        {"id": "20", "type": 2, "name": "Voice A", "position": 0},
                        {"id": "21", "type": 2, "name": "Locked", "position": 1,
                         "permission_overwrites": [{"id": GUILD.to_string(), "type": 0, "allow": "0", "deny": CONNECT.to_string()}]}
                    ]
                }],
                "merged_members": [[
                    {"user_id": MOD.to_string(), "roles": ["101"]},
                    {"user_id": MEMBER.to_string(), "roles": ["103"]},
                    {"user_id": ADMIN.to_string(), "roles": ["100"]},
                    {"user_id": PEER.to_string(), "roles": ["101"]},
                    {"user_id": OWNER.to_string(), "roles": []}
                ]]
            }),
        );
        state
    }

    fn id(value: u64) -> Snowflake {
        Snowflake::new(value)
    }

    #[test]
    fn slowmode_applies_unless_manage_messages_or_channel_exempts() {
        let mut state = state();
        let text = |id: &str, slowmode: u32, overwrites: serde_json::Value| {
            json!({"id": id, "type": 0, "name": "t", "guild_id": GUILD.to_string(),
                   "rate_limit_per_user": slowmode, "permission_overwrites": overwrites})
        };
        state.apply("CHANNEL_CREATE", &text("30", 15, json!([])));
        state.apply("CHANNEL_CREATE", &text("31", 0, json!([])));
        // The Mod role gets Manage Messages here, which exempts it.
        state.apply(
            "CHANNEL_CREATE",
            &text(
                "32",
                15,
                json!([{"id": "101", "type": 0, "allow": MANAGE_MESSAGES.to_string(), "deny": "0"}]),
            ),
        );
        assert_eq!(state.slowmode(id(30)), Some(15));
        assert_eq!(state.slowmode(id(31)), None, "no slowmode set");
        assert_eq!(state.slowmode(id(32)), None, "Manage Messages exempts");
    }

    #[test]
    fn hierarchy_decides_who_may_act_on_whom() {
        let state = state();
        let guild = id(GUILD);
        let member = state.member_actions(guild, id(MEMBER), 0);
        assert!(member.kick && member.ban && member.timeout && member.nickname && member.roles);
        // Same top role: nothing.
        let peer = state.member_actions(guild, id(PEER), 0);
        assert!(!peer.kick && !peer.ban && !peer.timeout && !peer.nickname && !peer.roles);
        // Higher role and the owner: nothing.
        assert!(!state.member_actions(guild, id(ADMIN), 0).any());
        assert!(!state.member_actions(guild, id(OWNER), 0).any());
        // Yourself: nickname (you have Change Nickname via @everyone) and
        // roles below your own, never kick or ban.
        let me = state.member_actions(guild, id(MOD), 0);
        assert!(me.nickname && me.roles && !me.kick && !me.ban && !me.timeout);
    }

    #[test]
    fn the_owner_bypasses_the_hierarchy_but_admins_cannot_be_timed_out() {
        let mut state = state();
        state.me.as_mut().unwrap().id = id(OWNER);
        let guild = id(GUILD);
        let admin = state.member_actions(guild, id(ADMIN), 0);
        assert!(admin.kick && admin.ban && admin.nickname && admin.roles);
        assert!(!admin.timeout, "administrators cannot be timed out");
        assert!(!state.member_actions(guild, id(OWNER), 0).kick);
        // Every role but @everyone and the managed one is assignable.
        let names: Vec<String> = state
            .assignable_roles(guild)
            .into_iter()
            .map(|role| role.name)
            .collect();
        assert_eq!(names, ["Admin", "Mod", "Member"]);
    }

    #[test]
    fn assignable_roles_are_below_your_own_and_not_managed() {
        let state = state();
        let names: Vec<String> = state
            .assignable_roles(id(GUILD))
            .into_iter()
            .map(|role| role.name)
            .collect();
        assert_eq!(names, ["Member"]);
    }

    #[test]
    fn an_administrator_without_ownership_still_obeys_the_hierarchy() {
        let mut state = state();
        state.me.as_mut().unwrap().id = id(ADMIN);
        let guild = id(GUILD);
        assert!(state.member_actions(guild, id(MOD), 0).kick);
        assert!(!state.member_actions(guild, id(OWNER), 0).kick);
        let roles: Vec<String> = state
            .assignable_roles(guild)
            .into_iter()
            .map(|role| role.name)
            .collect();
        assert_eq!(roles, ["Mod", "Member"]);
    }

    #[test]
    fn equal_positions_rank_the_older_role_higher() {
        let guild = Guild {
            id: id(1),
            roles: vec![
                Role {
                    id: id(7),
                    position: 2,
                    ..Role::default()
                },
                Role {
                    id: id(8),
                    position: 2,
                    ..Role::default()
                },
            ],
            ..Guild::default()
        };
        assert!(outranks(&guild, id(50), &[id(7)], id(51), &[id(8)]));
        assert!(!outranks(&guild, id(51), &[id(8)], id(50), &[id(7)]));
    }

    #[test]
    fn timeouts_are_read_from_the_member() {
        let mut state = state();
        state.apply(
            "GUILD_MEMBER_UPDATE",
            &json!({"guild_id": GUILD.to_string(), "user": {"id": MEMBER.to_string(), "username": "m"},
                    "roles": ["103"], "communication_disabled_until": "2030-01-01T00:00:00+00:00"}),
        );
        let now = crate::time::iso_to_unix_millis("2029-12-31T23:00:00+00:00").unwrap();
        assert!(state.member_actions(id(GUILD), id(MEMBER), now).timed_out);
        let later = crate::time::iso_to_unix_millis("2030-01-01T00:00:01+00:00").unwrap();
        assert!(!state.member_actions(id(GUILD), id(MEMBER), later).timed_out);
    }

    #[test]
    fn voice_moderation_follows_channel_permissions() {
        let mut state = state();
        let guild = id(GUILD);
        assert!(!state.member_actions(guild, id(MEMBER), 0).in_voice);
        state.apply(
            "VOICE_STATE_UPDATE",
            &json!({"guild_id": GUILD.to_string(), "channel_id": "20", "user_id": ADMIN.to_string(),
                    "session_id": "s", "mute": true}),
        );
        // Voice moderation does not look at the hierarchy.
        let admin = state.member_actions(guild, id(ADMIN), 0);
        assert!(admin.in_voice && admin.muted && !admin.deafened);
        assert!(admin.move_member && admin.disconnect && admin.server_mute);
        assert!(!admin.server_deafen, "no Deafen Members");
        // The locked channel denies Connect, so it is no target.
        let targets: Vec<String> = state
            .move_targets(guild)
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        assert_eq!(targets, ["Voice A"]);
    }
}
