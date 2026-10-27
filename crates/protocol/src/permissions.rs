//! Role based permissions, modelled after Discord.
//!
//! Resolution order for a member in a channel:
//! 1. base permissions of the `@everyone` role
//! 2. OR of all roles the member has
//! 3. `ADMINISTRATOR` short-circuits to all permissions
//! 4. channel overwrite for `@everyone`
//! 5. channel overwrites for the member's roles (all denies, then all allows)
//! 6. channel overwrite for the member itself

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct Permissions: u64 {
        const VIEW_CHANNEL        = 1 << 0;
        const SEND_MESSAGES       = 1 << 1;
        const ATTACH_FILES        = 1 << 2;
        const CONNECT             = 1 << 3;
        const SPEAK               = 1 << 4;
        /// Share the screen and the webcam.
        const SHARE_SCREEN        = 1 << 5;
        /// Create own channels within the limits configured for the group.
        const CREATE_OWN_CHANNELS = 1 << 6;
        const MANAGE_CHANNELS     = 1 << 7;
        const MANAGE_MESSAGES     = 1 << 8;
        const MUTE_MEMBERS        = 1 << 9;
        const MOVE_MEMBERS        = 1 << 10;
        const KICK_MEMBERS        = 1 << 11;
        const BAN_MEMBERS         = 1 << 12;
        /// Manage roles ranked below the member's highest role.
        const MANAGE_ROLES        = 1 << 13;
        const MANAGE_GROUP        = 1 << 14;
        const ADMINISTRATOR       = 1 << 63;
    }
}

impl Permissions {
    /// Sensible defaults for the `@everyone` role of a new group.
    pub const DEFAULT_EVERYONE: Self = Self::VIEW_CHANNEL
        .union(Self::SEND_MESSAGES)
        .union(Self::ATTACH_FILES)
        .union(Self::CONNECT)
        .union(Self::SPEAK)
        .union(Self::SHARE_SCREEN);

    /// Permissions granted to the creator inside a user-created channel.
    pub const CHANNEL_OWNER: Self = Self::MANAGE_CHANNELS
        .union(Self::MANAGE_MESSAGES)
        .union(Self::MUTE_MEMBERS)
        .union(Self::MOVE_MEMBERS);
}

/// Who a channel overwrite applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum OverwriteTarget<RoleId, MemberId> {
    Role(RoleId),
    Member(MemberId),
}

/// A per-channel allow/deny overwrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overwrite<RoleId, MemberId> {
    pub target: OverwriteTarget<RoleId, MemberId>,
    pub allow: Permissions,
    pub deny: Permissions,
}

impl<R, M> Overwrite<R, M> {
    fn apply(&self, perms: Permissions) -> Permissions {
        (perms - self.deny) | self.allow
    }
}

/// Computes the effective permissions of a member in a channel.
///
/// `everyone_role` is the id of the group's `@everyone` role and
/// `role_perms` holds the base permissions of every role the member has
/// (excluding `@everyone`, whose base is passed as `everyone_perms`).
pub fn compute_channel_permissions<R: PartialEq, M: PartialEq>(
    everyone_role: &R,
    everyone_perms: Permissions,
    member: &M,
    member_roles: &[(R, Permissions)],
    overwrites: &[Overwrite<R, M>],
) -> Permissions {
    let base = member_roles
        .iter()
        .fold(everyone_perms, |acc, (_, p)| acc | *p);
    if base.contains(Permissions::ADMINISTRATOR) {
        return Permissions::all();
    }

    let mut perms = base;
    if let Some(ow) = overwrites
        .iter()
        .find(|o| matches!(&o.target, OverwriteTarget::Role(r) if r == everyone_role))
    {
        perms = ow.apply(perms);
    }

    let (mut allow, mut deny) = (Permissions::empty(), Permissions::empty());
    for ow in overwrites {
        if let OverwriteTarget::Role(r) = &ow.target
            && member_roles.iter().any(|(mr, _)| mr == r)
        {
            allow |= ow.allow;
            deny |= ow.deny;
        }
    }
    perms = (perms - deny) | allow;

    if let Some(ow) = overwrites
        .iter()
        .find(|o| matches!(&o.target, OverwriteTarget::Member(m) if m == member))
    {
        perms = ow.apply(perms);
    }
    perms
}

#[cfg(test)]
mod tests {
    use super::*;

    type Ow = Overwrite<u32, u32>;
    const EVERYONE: u32 = 0;
    const MOD: u32 = 1;
    const ALICE: u32 = 100;

    fn role_ow(role: u32, allow: Permissions, deny: Permissions) -> Ow {
        Ow {
            target: OverwriteTarget::Role(role),
            allow,
            deny,
        }
    }

    #[test]
    fn base_permissions_without_overwrites() {
        let p =
            compute_channel_permissions(&EVERYONE, Permissions::DEFAULT_EVERYONE, &ALICE, &[], &[]);
        assert_eq!(p, Permissions::DEFAULT_EVERYONE);
    }

    #[test]
    fn administrator_bypasses_overwrites() {
        let ows = [role_ow(EVERYONE, Permissions::empty(), Permissions::all())];
        let p = compute_channel_permissions(
            &EVERYONE,
            Permissions::empty(),
            &ALICE,
            &[(MOD, Permissions::ADMINISTRATOR)],
            &ows,
        );
        assert_eq!(p, Permissions::all());
    }

    #[test]
    fn role_allow_beats_everyone_deny() {
        // Private channel: hidden for everyone, visible for moderators.
        let ows = [
            role_ow(EVERYONE, Permissions::empty(), Permissions::VIEW_CHANNEL),
            role_ow(MOD, Permissions::VIEW_CHANNEL, Permissions::empty()),
        ];
        let roles = [(MOD, Permissions::empty())];
        let p = compute_channel_permissions(
            &EVERYONE,
            Permissions::DEFAULT_EVERYONE,
            &ALICE,
            &roles,
            &ows,
        );
        assert!(p.contains(Permissions::VIEW_CHANNEL));
        let p = compute_channel_permissions(
            &EVERYONE,
            Permissions::DEFAULT_EVERYONE,
            &ALICE,
            &[],
            &ows,
        );
        assert!(!p.contains(Permissions::VIEW_CHANNEL));
    }

    #[test]
    fn role_allow_wins_over_role_deny_on_same_level() {
        let ows = [
            role_ow(MOD, Permissions::empty(), Permissions::SHARE_SCREEN),
            role_ow(2, Permissions::SHARE_SCREEN, Permissions::empty()),
        ];
        let roles = [(MOD, Permissions::empty()), (2, Permissions::empty())];
        let p = compute_channel_permissions(
            &EVERYONE,
            Permissions::DEFAULT_EVERYONE,
            &ALICE,
            &roles,
            &ows,
        );
        assert!(p.contains(Permissions::SHARE_SCREEN));
    }

    #[test]
    fn member_overwrite_is_applied_last() {
        let ows = [
            role_ow(MOD, Permissions::SPEAK, Permissions::empty()),
            Ow {
                target: OverwriteTarget::Member(ALICE),
                allow: Permissions::empty(),
                deny: Permissions::SPEAK,
            },
        ];
        let roles = [(MOD, Permissions::empty())];
        let p = compute_channel_permissions(
            &EVERYONE,
            Permissions::DEFAULT_EVERYONE,
            &ALICE,
            &roles,
            &ows,
        );
        assert!(!p.contains(Permissions::SPEAK));
    }
}
