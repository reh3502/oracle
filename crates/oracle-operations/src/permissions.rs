//! Discord permission calculations over a complete, freshly fetched guild snapshot.
//! Missing facts fail closed. The adapter must still enforce guild identity and freshness.
use oracle_core::{Error, ErrorCode, Result};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::BTreeSet};

pub const ADMINISTRATOR: u64 = 1 << 3;
pub const MANAGE_CHANNELS: u64 = 1 << 4;
pub const VIEW_CHANNEL: u64 = 1 << 10;
pub const SEND_MESSAGES: u64 = 1 << 11;
pub const READ_MESSAGE_HISTORY: u64 = 1 << 16;
pub const CONNECT: u64 = 1 << 20;
pub const MANAGE_ROLES: u64 = 1 << 28;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    pub id: String,
    pub position: i64,
    pub permissions: u64,
    pub managed: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub id: String,
    pub roles: Vec<String>,
    pub timed_out: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverwriteKind {
    Role,
    Member,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overwrite {
    pub id: String,
    pub kind: OverwriteKind,
    pub allow: u64,
    pub deny: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDelta {
    Unchanged,
    Reduction,
    Expansion,
}
/// Mixed additions and removals still require expansion approval.
pub fn permission_delta(before: u64, after: u64) -> PermissionDelta {
    if after & !before != 0 {
        PermissionDelta::Expansion
    } else if before != after {
        PermissionDelta::Reduction
    } else {
        PermissionDelta::Unchanged
    }
}
fn denied() -> Error {
    Error::new(ErrorCode::ForbiddenPermission)
}
fn snowflake(id: &str) -> Result<u64> {
    if id.starts_with('0') || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(denied());
    }
    id.parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(denied)
}
fn validate(guild: &str, owner: &str, roles: &[Role], member: &Member) -> Result<()> {
    snowflake(guild)?;
    snowflake(owner)?;
    snowflake(&member.id)?;
    if roles.is_empty() || roles.len() > 250 || member.roles.len() > 250 {
        return Err(denied());
    }
    let mut ids = BTreeSet::new();
    for role in roles {
        snowflake(&role.id)?;
        if role.position < 0 || !ids.insert(role.id.as_str()) {
            return Err(denied());
        }
    }
    if !ids.contains(guild) {
        return Err(denied());
    }
    let mut assigned = BTreeSet::new();
    for id in &member.roles {
        if !ids.contains(id.as_str()) || !assigned.insert(id) {
            return Err(denied());
        }
    }
    Ok(())
}
fn base(guild: &str, owner: &str, roles: &[Role], member: &Member) -> Result<u64> {
    validate(guild, owner, roles, member)?;
    let bits = roles
        .iter()
        .filter(|r| r.id == guild || member.roles.contains(&r.id))
        .fold(0, |bits, role| bits | role.permissions);
    Ok(if member.id == owner || bits & ADMINISTRATOR != 0 {
        u64::MAX
    } else {
        bits
    })
}
fn timeout(bits: u64, member: &Member) -> u64 {
    if member.timed_out && bits & ADMINISTRATOR == 0 {
        bits & (VIEW_CHANNEL | READ_MESSAGE_HISTORY)
    } else {
        bits
    }
}
pub fn guild_permissions(guild: &str, owner: &str, roles: &[Role], member: &Member) -> Result<u64> {
    Ok(timeout(base(guild, owner, roles, member)?, member))
}
/// Returns the overwrite bitset, then applies timeout restrictions. Callers must also
/// check VIEW_CHANNEL and action-specific implicit restrictions (e.g. CONNECT for voice).
pub fn channel_permissions(
    guild: &str,
    owner: &str,
    roles: &[Role],
    member: &Member,
    overwrites: &[Overwrite],
) -> Result<u64> {
    let mut bits = base(guild, owner, roles, member)?;
    if overwrites.len() > 1000 {
        return Err(denied());
    }
    let mut seen = BTreeSet::new();
    for overwrite in overwrites {
        snowflake(&overwrite.id)?;
        if !seen.insert((overwrite.kind, &overwrite.id))
            || (overwrite.kind == OverwriteKind::Role
                && !roles.iter().any(|role| role.id == overwrite.id))
            || overwrite.allow & overwrite.deny != 0
        {
            return Err(denied());
        }
    }
    if bits & ADMINISTRATOR != 0 {
        return Ok(bits);
    }
    if let Some(everyone) = overwrites
        .iter()
        .find(|o| o.kind == OverwriteKind::Role && o.id == guild)
    {
        bits = (bits & !everyone.deny) | everyone.allow;
    }
    let (allow, deny) = overwrites
        .iter()
        .filter(|o| o.kind == OverwriteKind::Role && o.id != guild && member.roles.contains(&o.id))
        .fold((0, 0), |(allow, deny), o| (allow | o.allow, deny | o.deny));
    bits = (bits & !deny) | allow;
    if let Some(personal) = overwrites
        .iter()
        .find(|o| o.kind == OverwriteKind::Member && o.id == member.id)
    {
        bits = (bits & !personal.deny) | personal.allow;
    }
    // ADMINISTRATOR cannot be conferred by a channel overwrite.
    bits &= !ADMINISTRATOR;
    Ok(timeout(bits, member))
}
pub fn require_visible_to_both(
    guild: &str,
    owner: &str,
    roles: &[Role],
    actor: &Member,
    bot: &Member,
    overwrites: &[Overwrite],
) -> Result<()> {
    for member in [actor, bot] {
        if channel_permissions(guild, owner, roles, member, overwrites)? & VIEW_CHANNEL == 0 {
            return Err(denied());
        }
    }
    Ok(())
}
/// Discord resolves equal role positions by the older (smaller) snowflake first.
/// Everyone is always the lowest role, independent of a supplied position.
pub fn compare_roles(guild: &str, left: &Role, right: &Role) -> Result<Ordering> {
    let left_id = snowflake(&left.id)?;
    let right_id = snowflake(&right.id)?;
    if left.id == right.id {
        return Ok(Ordering::Equal);
    }
    if left.id == guild {
        return Ok(Ordering::Less);
    }
    if right.id == guild {
        return Ok(Ordering::Greater);
    }
    Ok(left
        .position
        .cmp(&right.position)
        .then_with(|| right_id.cmp(&left_id)))
}
fn outranks(guild: &str, roles: &[Role], member: &Member, target: &Role) -> Result<bool> {
    for role in roles
        .iter()
        .filter(|r| r.id == guild || member.roles.contains(&r.id))
    {
        if compare_roles(guild, role, target)? == Ordering::Greater {
            return Ok(true);
        }
    }
    Ok(false)
}
/// Oracle requires both requester and bot to have authority. Administrator bypasses
/// overwrites, never hierarchy. Managed roles and @everyone are outside this edit API.
pub fn require_manage_role(
    guild: &str,
    owner: &str,
    roles: &[Role],
    actor: &Member,
    bot: &Member,
    target_role: &str,
    new_permissions: u64,
) -> Result<()> {
    let target = roles
        .iter()
        .find(|r| r.id == target_role)
        .ok_or_else(denied)?;
    if target.managed || target.id == guild {
        return Err(denied());
    }
    for member in [actor, bot] {
        let permissions = guild_permissions(guild, owner, roles, member)?;
        if permissions & MANAGE_ROLES == 0
            || new_permissions & !permissions != 0
            || (member.id != owner && !outranks(guild, roles, member, target)?)
        {
            return Err(denied());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn role(id: &str, position: i64, permissions: u64) -> Role {
        Role {
            id: id.into(),
            position,
            permissions,
            managed: false,
        }
    }
    fn member(id: &str, roles: &[&str]) -> Member {
        Member {
            id: id.into(),
            roles: roles.iter().map(|s| (*s).into()).collect(),
            timed_out: false,
        }
    }
    fn overwrite(id: &str, kind: OverwriteKind, allow: u64, deny: u64) -> Overwrite {
        Overwrite {
            id: id.into(),
            kind,
            allow,
            deny,
        }
    }
    #[test]
    fn everyone_roles_aggregate_before_member_override() {
        let roles = [
            role("1", 0, VIEW_CHANNEL | SEND_MESSAGES),
            role("10", 1, 0),
            role("20", 2, 0),
        ];
        let actor = member("100", &["10", "20"]);
        let mut overwrites = vec![
            overwrite("1", OverwriteKind::Role, 0, VIEW_CHANNEL),
            overwrite("10", OverwriteKind::Role, VIEW_CHANNEL, SEND_MESSAGES),
            overwrite("20", OverwriteKind::Role, SEND_MESSAGES, VIEW_CHANNEL),
        ];
        assert_eq!(
            channel_permissions("1", "999", &roles, &actor, &overwrites).unwrap(),
            VIEW_CHANNEL | SEND_MESSAGES
        );
        overwrites.push(overwrite("100", OverwriteKind::Member, 0, VIEW_CHANNEL));
        assert_eq!(
            channel_permissions("1", "999", &roles, &actor, &overwrites).unwrap(),
            SEND_MESSAGES
        );
        assert!(
            require_visible_to_both("1", "999", &roles, &actor, &member("200", &[]), &overwrites)
                .is_err()
        );
    }
    #[test]
    fn visibility_is_actor_and_bot_intersection() {
        let roles = [role("1", 0, VIEW_CHANNEL)];
        let actor = member("100", &[]);
        let bot = member("200", &[]);
        assert!(require_visible_to_both("1", "999", &roles, &actor, &bot, &[]).is_ok());
        for hidden in ["100", "200"] {
            assert!(
                require_visible_to_both(
                    "1",
                    "999",
                    &roles,
                    &actor,
                    &bot,
                    &[overwrite(hidden, OverwriteKind::Member, 0, VIEW_CHANNEL)]
                )
                .is_err()
            );
        }
    }
    #[test]
    fn admin_and_owner_bypass_overwrites_but_timeout_applies_to_ordinary_members() {
        let roles = [
            role(
                "1",
                0,
                VIEW_CHANNEL | READ_MESSAGE_HISTORY | SEND_MESSAGES | MANAGE_ROLES,
            ),
            role("10", 1, ADMINISTRATOR),
        ];
        let deny = [overwrite("1", OverwriteKind::Role, 0, VIEW_CHANNEL)];
        for mut actor in [member("999", &[]), member("100", &["10"])] {
            actor.timed_out = true;
            assert_eq!(
                channel_permissions("1", "999", &roles, &actor, &deny).unwrap(),
                u64::MAX
            );
        }
        let mut actor = member("100", &[]);
        actor.timed_out = true;
        assert_eq!(
            guild_permissions("1", "999", &roles, &actor).unwrap(),
            VIEW_CHANNEL | READ_MESSAGE_HISTORY
        );
        let allow = [overwrite(
            "100",
            OverwriteKind::Member,
            SEND_MESSAGES | MANAGE_ROLES,
            0,
        )];
        assert_eq!(
            channel_permissions("1", "999", &roles, &actor, &allow).unwrap(),
            VIEW_CHANNEL | READ_MESSAGE_HISTORY
        );
    }
    #[test]
    fn missing_or_removed_roles_and_duplicate_facts_fail_closed() {
        let roles = [role("1", 0, VIEW_CHANNEL), role("10", 1, ADMINISTRATOR)];
        assert!(guild_permissions("1", "999", &roles, &member("100", &["20"])).is_err());
        assert!(guild_permissions("1", "999", &roles[1..], &member("100", &["10"])).is_err());
        assert!(
            guild_permissions(
                "1",
                "999",
                &[roles[0].clone(), roles[0].clone()],
                &member("100", &[])
            )
            .is_err()
        );
        assert!(
            channel_permissions(
                "1",
                "999",
                &roles,
                &member("100", &["10"]),
                &[overwrite("20", OverwriteKind::Role, VIEW_CHANNEL, 0)]
            )
            .is_err()
        );
    }
    #[test]
    fn strict_hierarchy_uses_snowflakes_for_ties_even_for_administrators() {
        let roles = [
            role("1", 0, 0),
            role("10", 3, ADMINISTRATOR),
            role("20", 3, ADMINISTRATOR),
            role("30", 2, 0),
        ];
        assert_eq!(
            compare_roles("1", &roles[1], &roles[2]).unwrap(),
            Ordering::Greater
        );
        let actor = member("100", &["10"]);
        let bot = member("200", &["20"]);
        assert!(require_manage_role("1", "999", &roles, &actor, &bot, "30", VIEW_CHANNEL).is_ok());
        assert!(require_manage_role("1", "999", &roles, &actor, &bot, "20", 0).is_err());
        assert!(require_manage_role("1", "999", &roles, &actor, &bot, "10", 0).is_err());
        assert!(require_manage_role("1", "999", &roles, &actor, &bot, "1", 0).is_err());
    }
    #[test]
    fn managed_roles_and_unheld_permissions_cannot_be_granted() {
        let mut roles = vec![
            role("1", 0, 0),
            role("10", 3, MANAGE_ROLES),
            role("20", 1, 0),
        ];
        let actor = member("100", &["10"]);
        let bot = member("200", &["10"]);
        assert!(require_manage_role("1", "999", &roles, &actor, &bot, "20", 0).is_ok());
        assert!(
            require_manage_role("1", "999", &roles, &actor, &bot, "20", ADMINISTRATOR).is_err()
        );
        roles[2].managed = true;
        assert!(require_manage_role("1", "999", &roles, &actor, &bot, "20", 0).is_err());
    }
    #[test]
    fn mixed_delta_requires_expansion_approval() {
        assert_eq!(
            permission_delta(VIEW_CHANNEL, VIEW_CHANNEL),
            PermissionDelta::Unchanged
        );
        assert_eq!(
            permission_delta(VIEW_CHANNEL | SEND_MESSAGES, VIEW_CHANNEL),
            PermissionDelta::Reduction
        );
        assert_eq!(
            permission_delta(VIEW_CHANNEL, SEND_MESSAGES),
            PermissionDelta::Expansion
        );
    }
}
