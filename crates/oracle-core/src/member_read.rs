//! Host-only authority for explicitly enabled public module reads.
//! No transport JSON deserialization and no conversion to operator authority.
use crate::{Error, ErrorCode, GuildId, ModuleId, Result, UserId};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

const IDENTITY_TTL: Duration = Duration::from_secs(10);
const WINDOW: Duration = Duration::from_secs(60);

/// Constructed by authenticated ingress, never from operation input.
#[derive(Clone, Debug)]
pub struct MemberContext {
    pub guild: GuildId,
    pub user: UserId,
    pub channel: String,
    pub roles: BTreeSet<String>,
    pub observed_at: Instant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberReadPolicy {
    /// Empty means all channels where Discord authorizes this interaction.
    #[serde(default)]
    pub channels: BTreeSet<String>,
    /// Empty means all authenticated members; otherwise at least one role is required.
    #[serde(default)]
    pub roles: BTreeSet<String>,
    pub per_user_per_minute: u32,
    pub per_guild_per_minute: u32,
}
impl MemberReadPolicy {
    fn validate(&self) -> Result<()> {
        if self.channels.len() > 250
            || self.roles.len() > 250
            || !(1..=60).contains(&self.per_user_per_minute)
            || !(1..=600).contains(&self.per_guild_per_minute)
            || self.per_user_per_minute > self.per_guild_per_minute
            || self
                .channels
                .iter()
                .chain(&self.roles)
                .any(|id| UserId::new(id).is_err())
        {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        Ok(())
    }
}
struct Entry {
    policy: MemberReadPolicy,
    revision: u64,
    cancellation: CancellationToken,
    invalidated_at: Option<Instant>,
    window: Instant,
    guild_used: u32,
    users: BTreeMap<UserId, u32>,
}
#[derive(Default)]
struct State {
    revision: u64,
    entries: BTreeMap<(GuildId, ModuleId), Entry>,
}
#[derive(Clone, Default)]
pub struct MemberReadGate {
    state: Arc<Mutex<State>>,
}
#[derive(Clone)]
pub struct MemberReadPermit {
    gate: MemberReadGate,
    key: (GuildId, ModuleId),
    revision: u64,
    expires: Instant,
    cancel: CancellationToken,
}
impl MemberReadPermit {
    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.child_token()
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// Serialize final nonblocking dispatch admission with policy revocation.
    pub fn dispatch<T>(&self, send: impl FnOnce() -> T) -> Result<T> {
        let state = self.gate.state.lock().unwrap();
        let valid = state
            .entries
            .get(&self.key)
            .is_some_and(|entry| entry.revision == self.revision);
        if !valid || self.cancel.is_cancelled() || Instant::now() >= self.expires {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        Ok(send())
    }
    pub fn check(&self) -> Result<()> {
        self.dispatch(|| ())
    }
}
impl MemberReadGate {
    /// Host configuration only. Installing/enabling a module alone grants no member access.
    pub fn configure(
        &self,
        guild: GuildId,
        module: ModuleId,
        policy: Option<MemberReadPolicy>,
    ) -> Result<()> {
        if let Some(policy) = &policy {
            policy.validate()?;
        }
        let mut state = self.state.lock().unwrap();
        let key = (guild, module);
        if !state.entries.contains_key(&key) && policy.is_some() && state.entries.len() >= 1024 {
            return Err(Error::new(ErrorCode::QuotaExceeded));
        }
        let revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| Error::new(ErrorCode::QuotaExceeded))?;
        state.revision = revision;
        if let Some(old) = state.entries.remove(&key) {
            old.cancellation.cancel();
        }
        if let Some(policy) = policy {
            state.entries.insert(
                key,
                Entry {
                    policy,
                    revision,
                    cancellation: CancellationToken::new(),
                    invalidated_at: None,
                    window: Instant::now(),
                    guild_used: 0,
                    users: BTreeMap::new(),
                },
            );
        }
        Ok(())
    }
    /// Role/channel/membership changes invalidate already admitted reads. Fresh
    /// ingress must supply current identity again; quota usage is retained.
    pub fn invalidate_guild(&self, guild: &GuildId) {
        let mut state = self.state.lock().unwrap();
        for ((entry_guild, _), entry) in &mut state.entries {
            if entry_guild == guild {
                entry.cancellation.cancel();
                entry.cancellation = CancellationToken::new();
                entry.invalidated_at = Some(Instant::now());
            }
        }
    }
    pub fn admit(
        &self,
        context: &MemberContext,
        guild: &GuildId,
        module: &ModuleId,
    ) -> Result<MemberReadPermit> {
        if &context.guild != guild {
            return Err(Error::new(ErrorCode::ForbiddenScope));
        }
        let now = Instant::now();
        if context.observed_at > now
            || now.duration_since(context.observed_at) >= IDENTITY_TTL
            || context.roles.len() > 250
            || UserId::new(&context.channel).is_err()
            || context.roles.iter().any(|id| UserId::new(id).is_err())
        {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        let mut state = self.state.lock().unwrap();
        let key = (guild.clone(), module.clone());
        let entry = state
            .entries
            .get_mut(&key)
            .ok_or_else(|| Error::new(ErrorCode::ForbiddenPermission))?;
        if entry
            .invalidated_at
            .is_some_and(|at| context.observed_at <= at)
        {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        if (!entry.policy.channels.is_empty() && !entry.policy.channels.contains(&context.channel))
            || (!entry.policy.roles.is_empty() && entry.policy.roles.is_disjoint(&context.roles))
        {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        if now.duration_since(entry.window) >= WINDOW {
            entry.window = now;
            entry.guild_used = 0;
            entry.users.clear();
        }
        let user_used = entry.users.get(&context.user).copied().unwrap_or_default();
        if user_used >= entry.policy.per_user_per_minute
            || entry.guild_used >= entry.policy.per_guild_per_minute
        {
            return Err(Error::new(ErrorCode::QuotaExceeded));
        }
        entry.guild_used += 1;
        entry.users.insert(context.user.clone(), user_used + 1);
        Ok(MemberReadPermit {
            gate: self.clone(),
            key,
            revision: entry.revision,
            expires: context.observed_at + IDENTITY_TTL,
            cancel: entry.cancellation.child_token(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> MemberContext {
        MemberContext {
            guild: GuildId::new("123").unwrap(),
            user: UserId::new("456").unwrap(),
            channel: "789".into(),
            roles: BTreeSet::from(["222".into()]),
            observed_at: Instant::now(),
        }
    }
    fn policy() -> MemberReadPolicy {
        MemberReadPolicy {
            channels: BTreeSet::from(["789".into()]),
            roles: BTreeSet::from(["222".into()]),
            per_user_per_minute: 2,
            per_guild_per_minute: 3,
        }
    }
    #[test]
    fn default_deny_scope_policy_and_freshness() {
        let gate = MemberReadGate::default();
        let c = context();
        let module = ModuleId::new("test.reader").unwrap();
        assert!(gate.admit(&c, &c.guild, &module).is_err());
        gate.configure(c.guild.clone(), module.clone(), Some(policy()))
            .unwrap();
        assert!(
            gate.admit(&c, &GuildId::new("999").unwrap(), &module)
                .is_err()
        );
        let mut wrong = c.clone();
        wrong.channel = "999".into();
        assert!(gate.admit(&wrong, &c.guild, &module).is_err());
        wrong = c.clone();
        wrong.roles.clear();
        assert!(gate.admit(&wrong, &c.guild, &module).is_err());
        wrong = c.clone();
        wrong.observed_at -= IDENTITY_TTL;
        assert!(gate.admit(&wrong, &c.guild, &module).is_err());
        assert!(gate.admit(&c, &c.guild, &module).is_ok());
    }
    #[test]
    fn quotas_and_revocation_are_shared_and_bounded() {
        let gate = MemberReadGate::default();
        let c = context();
        let module = ModuleId::new("test.reader").unwrap();
        gate.configure(c.guild.clone(), module.clone(), Some(policy()))
            .unwrap();
        let permit = gate.admit(&c, &c.guild, &module).unwrap();
        assert_eq!(permit.dispatch(|| 7).unwrap(), 7);
        gate.clone().admit(&c, &c.guild, &module).unwrap();
        assert_eq!(
            gate.admit(&c, &c.guild, &module).err().unwrap().code,
            ErrorCode::QuotaExceeded
        );
        let mut other = c.clone();
        other.user = UserId::new("555").unwrap();
        gate.admit(&other, &c.guild, &module).unwrap();
        other.user = UserId::new("666").unwrap();
        assert_eq!(
            gate.admit(&other, &c.guild, &module).err().unwrap().code,
            ErrorCode::QuotaExceeded
        );
        gate.invalidate_guild(&c.guild);
        assert!(permit.cancellation().is_cancelled());
        assert!(permit.check().is_err());
        assert_eq!(
            gate.admit(&other, &c.guild, &module).err().unwrap().code,
            ErrorCode::ForbiddenPermission
        );
        // Revoking identity does not reset the quota.
        other.observed_at = Instant::now();
        assert_eq!(
            gate.admit(&other, &c.guild, &module).err().unwrap().code,
            ErrorCode::QuotaExceeded
        );
        gate.configure(c.guild.clone(), module.clone(), None)
            .unwrap();
        assert!(gate.admit(&c, &c.guild, &module).is_err());
    }
}
