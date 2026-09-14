//! Host-only authority for explicitly enabled member mutations.
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

pub use crate::member_read::MemberContext;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemberMutationPolicy {
    /// Empty means all channels where Discord authorizes this interaction.
    #[serde(default)]
    pub channels: BTreeSet<String>,
    /// Optional module permission labels mapped to host-observed roles.
    /// Hosting is available to all admitted members regardless of these roles.
    #[serde(default)]
    pub permission_roles: BTreeMap<String, BTreeSet<String>>,
    pub per_user_per_minute: u32,
    pub per_guild_per_minute: u32,
}
impl MemberMutationPolicy {
    fn validate(&self) -> Result<()> {
        if self.channels.len() > 250
            || self.permission_roles.len() > 32
            || !(1..=60).contains(&self.per_user_per_minute)
            || !(1..=600).contains(&self.per_guild_per_minute)
            || self.per_user_per_minute > self.per_guild_per_minute
            || self.permission_roles.keys().any(|label| {
                label.is_empty()
                    || label.len() > 64
                    || !label
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
            || self
                .permission_roles
                .values()
                .any(|roles| roles.len() > 250)
            || self
                .channels
                .iter()
                .chain(self.permission_roles.values().flatten())
                .any(|id| UserId::new(id).is_err())
        {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        Ok(())
    }
}
struct Entry {
    policy: MemberMutationPolicy,
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
pub struct MemberMutationGate {
    state: Arc<Mutex<State>>,
}
#[derive(Clone)]
pub struct MemberMutationPermit {
    gate: MemberMutationGate,
    key: (GuildId, ModuleId),
    revision: u64,
    expires: Instant,
    cancel: CancellationToken,
    actor: Option<crate::AuthenticatedMember>,
}
impl MemberMutationPermit {
    pub fn actor(&self) -> Option<&crate::AuthenticatedMember> {
        self.actor.as_ref()
    }
    pub fn expires_at(&self) -> Instant {
        self.expires
    }
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
impl MemberMutationGate {
    /// Check catalog visibility without consuming invocation quota.
    pub fn check_access(
        &self,
        context: &MemberContext,
        guild: &GuildId,
        module: &ModuleId,
    ) -> Result<()> {
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
        let state = self.state.lock().unwrap();
        let entry = state
            .entries
            .get(&(guild.clone(), module.clone()))
            .ok_or_else(|| Error::new(ErrorCode::ForbiddenPermission))?;
        if entry
            .invalidated_at
            .is_some_and(|at| context.observed_at <= at)
            || (!entry.policy.channels.is_empty()
                && !entry.policy.channels.contains(&context.channel))
        {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        Ok(())
    }
    /// Host configuration only. Installing/enabling a module alone grants no member access.
    pub fn configure(
        &self,
        guild: GuildId,
        module: ModuleId,
        policy: Option<MemberMutationPolicy>,
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
    /// Loss of Gateway coverage revokes every pending read. New requests must
    /// obtain identity after this observation gap; existing quotas are retained.
    pub fn invalidate_all(&self) {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        for entry in state.entries.values_mut() {
            entry.cancellation.cancel();
            entry.cancellation = CancellationToken::new();
            entry.invalidated_at = Some(now);
        }
    }
    /// Fresh, bounded worker authority from the current configured registry entry.
    pub fn worker(&self, guild: &GuildId, module: &ModuleId) -> Result<MemberMutationPermit> {
        let state = self.state.lock().unwrap();
        let key = (guild.clone(), module.clone());
        let entry = state
            .entries
            .get(&key)
            .ok_or_else(|| Error::new(ErrorCode::ForbiddenPermission))?;
        Ok(MemberMutationPermit {
            gate: self.clone(),
            key,
            revision: entry.revision,
            expires: Instant::now() + Duration::from_secs(30),
            cancel: entry.cancellation.child_token(),
            actor: None,
        })
    }
    pub fn admit(
        &self,
        context: &MemberContext,
        guild: &GuildId,
        module: &ModuleId,
        interaction_id: &str,
        permissions: &[String],
    ) -> Result<MemberMutationPermit> {
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
        if !entry.policy.channels.is_empty() && !entry.policy.channels.contains(&context.channel) {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        if now.duration_since(entry.window) >= WINDOW {
            entry.window = now;
            entry.guild_used = 0;
            entry.users.clear();
        }
        // Snowflake age prevents unseen deliveries from recreating expired receipts.
        let snowflake = interaction_id
            .parse::<u64>()
            .map_err(|_| Error::new(ErrorCode::InvalidInput))?;
        if snowflake.to_string() != interaction_id {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        let created_ms = (snowflake >> 22) + 1_420_070_400_000;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::new(ErrorCode::ForbiddenPermission))?
            .as_millis();
        if u128::from(created_ms) > now_ms || now_ms - u128::from(created_ms) > 900_000 {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        let actor = crate::AuthenticatedMember {
            user_id: context.user.clone(),
            channel_id: context.channel.clone(),
            interaction_id: interaction_id.into(),
            permissions: entry
                .policy
                .permission_roles
                .iter()
                .filter(|(name, roles)| {
                    permissions.contains(name) && !roles.is_disjoint(&context.roles)
                })
                .map(|(name, _)| name.clone())
                .collect(),
        };
        let user_used = entry.users.get(&context.user).copied().unwrap_or_default();
        if user_used >= entry.policy.per_user_per_minute
            || entry.guild_used >= entry.policy.per_guild_per_minute
        {
            return Err(Error::new(ErrorCode::QuotaExceeded));
        }
        entry.guild_used += 1;
        entry.users.insert(context.user.clone(), user_used + 1);
        Ok(MemberMutationPermit {
            actor: Some(actor),
            gate: self.clone(),
            key,
            revision: entry.revision,
            expires: context.observed_at + IDENTITY_TTL,
            cancel: entry.cancellation.child_token(),
        })
    }
}

impl Default for MemberMutationPolicy {
    fn default() -> Self {
        Self {
            channels: BTreeSet::new(),
            permission_roles: BTreeMap::new(),
            per_user_per_minute: 20,
            per_guild_per_minute: 200,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snowflake(age_ms: u64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        ((now - age_ms - 1_420_070_400_000) << 22).to_string()
    }
    fn context() -> MemberContext {
        MemberContext {
            guild: "123".parse().unwrap(),
            user: "456".parse().unwrap(),
            channel: "789".into(),
            roles: BTreeSet::from(["222".into()]),
            observed_at: Instant::now(),
        }
    }
    #[test]
    fn default_deny_identity_permissions_quotas_and_revocation() {
        let gate = MemberMutationGate::default();
        let mut c = context();
        let module: ModuleId = "test.mutation".parse().unwrap();
        let interaction = snowflake(1000);
        let permissions = vec!["manage_all_runs".into()];
        assert!(
            gate.admit(&c, &c.guild, &module, &interaction, &permissions)
                .is_err()
        );
        gate.configure(
            c.guild.clone(),
            module.clone(),
            Some(MemberMutationPolicy {
                channels: BTreeSet::from(["789".into()]),
                permission_roles: BTreeMap::from([
                    ("manage_all_runs".into(), c.roles.clone()),
                    ("undeclared".into(), c.roles.clone()),
                ]),
                per_user_per_minute: 2,
                per_guild_per_minute: 3,
            }),
        )
        .unwrap();
        let permit = gate
            .admit(&c, &c.guild, &module, &interaction, &permissions)
            .unwrap();
        let actor = permit.actor().unwrap();
        assert_eq!(actor.user_id, c.user);
        assert_eq!(actor.channel_id, c.channel);
        assert_eq!(actor.interaction_id, interaction);
        assert_eq!(
            actor.permissions,
            BTreeSet::from(["manage_all_runs".into()])
        );
        assert!(
            gate.admit(
                &c,
                &"999".parse().unwrap(),
                &module,
                &interaction,
                &permissions
            )
            .is_err()
        );
        assert!(
            gate.admit(&c, &c.guild, &module, &snowflake(901_000), &permissions)
                .is_err()
        );
        let mut stale = c.clone();
        stale.observed_at -= Duration::from_secs(10);
        assert!(
            gate.admit(&stale, &c.guild, &module, &interaction, &permissions)
                .is_err()
        );
        gate.admit(&c, &c.guild, &module, &interaction, &permissions)
            .unwrap();
        assert_eq!(
            gate.admit(&c, &c.guild, &module, &interaction, &permissions)
                .err()
                .unwrap()
                .code,
            ErrorCode::QuotaExceeded
        );
        let worker = gate.worker(&c.guild, &module).unwrap();
        assert!(worker.actor().is_none());
        gate.invalidate_guild(&c.guild);
        assert!(permit.check().is_err());
        assert!(worker.check().is_err());
        assert!(
            gate.admit(&c, &c.guild, &module, &interaction, &permissions)
                .is_err()
        );
        c.observed_at = Instant::now();
        c.roles.clear();
        c.user = "555".parse().unwrap();
        let fresh = gate
            .admit(&c, &c.guild, &module, &interaction, &permissions)
            .unwrap();
        assert!(fresh.actor().unwrap().permissions.is_empty()); // No Run Host role requirement.
        gate.configure(c.guild.clone(), module.clone(), None)
            .unwrap();
        assert!(fresh.check().is_err());
        assert!(gate.worker(&c.guild, &module).is_err());
    }
}
