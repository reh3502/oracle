//! Minimal command publication with durable ownership and uncertainty records.
mod definition;
pub use definition::canonical_definition;
use definition::command_key;

use crate::executor::{DispatchFence, SendGuard};
use async_trait::async_trait;
use oracle_core::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;
fn error(code: ErrorCode) -> Error {
    Error::new(code)
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRoute {
    pub session: String,
    pub generation: u64,
    pub epoch: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DesiredCommand {
    pub owner: ModuleId,
    pub definition: Value,
    pub route: Option<CommandRoute>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublishedCommand {
    pub id: String,
    pub definition: Value,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingCommand {
    Create,
    Edit,
    Delete,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandBinding {
    pub owner: ModuleId,
    pub id: Option<String>,
    pub definition: Value,
    pub route: Option<CommandRoute>,
    pub pending: Option<PendingCommand>,
    pub target: Option<Value>,
    pub deleted: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CommandReport {
    pub created: u32,
    pub edited: u32,
    pub deleted: u32,
    pub unchanged: u32,
}
/// Implementations dispatch every write through the guard after their rate-limit waits.
#[async_trait]
pub trait CommandBackend: Send + Sync {
    async fn list(&self, guild: &GuildId) -> Result<Vec<PublishedCommand>>;
    async fn create(
        &self,
        guild: &GuildId,
        definition: &Value,
        guard: &SendGuard,
    ) -> Result<PublishedCommand>;
    async fn edit(
        &self,
        guild: &GuildId,
        id: &str,
        definition: &Value,
        guard: &SendGuard,
    ) -> Result<PublishedCommand>;
    async fn delete(&self, guild: &GuildId, id: &str, guard: &SendGuard) -> Result<()>;
}
/// A durable fence for all changed commands of each affected module. Sharded
/// previous bindings retain complete definitions without a 64 KiB group document.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationGroup {
    version: u32,
    id: String,
    active: bool,
    prepared: bool,
    expires_at: u64,
    previous_count: usize,
    affected: BTreeSet<ModuleId>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousCommand {
    publication: String,
    binding: CommandBinding,
}
pub struct CommandReconciler {
    repository: Arc<dyn WorkflowRepository>,
    backend: Arc<dyn CommandBackend>,
    locks: Mutex<BTreeMap<GuildId, Arc<tokio::sync::Mutex<()>>>>,
}
impl CommandReconciler {
    pub fn new(repository: Arc<dyn WorkflowRepository>, backend: Arc<dyn CommandBackend>) -> Self {
        Self {
            repository,
            backend,
            locks: Mutex::new(BTreeMap::new()),
        }
    }
    // Release the map mutex before awaiting the per-guild lock. Adoption and
    // reconciliation must serialize through the same lock for a guild.
    fn guild_lock(&self, guild: &GuildId) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(guild.clone())
            .or_default()
            .clone()
    }

    /// Adopt only an exact supported bootstrap definition already present remotely.
    /// This never mutates Discord and never replaces an existing ownership or recovery record.
    pub async fn adopt_known(
        &self,
        guild: &GuildId,
        owner: &ModuleId,
        known_definitions: &[Value],
    ) -> Result<bool> {
        if known_definitions.is_empty() || known_definitions.len() > 8 {
            return Err(error(ErrorCode::InvalidInput));
        }
        let known = known_definitions
            .iter()
            .map(canonical_definition)
            .collect::<Result<Vec<_>>>()?;
        if known
            .iter()
            .any(|definition| command_key(definition).ok().as_deref() != Some("1:oracle"))
        {
            return Err(error(ErrorCode::InvalidInput));
        }
        let lock = self.guild_lock(guild);
        let _lock = lock.lock().await;
        if let Some((_, binding)) = self.records(guild).await?.get("1:oracle") {
            return if &binding.owner == owner {
                Ok(false)
            } else {
                Err(error(ErrorCode::Conflict))
            };
        }
        let observed = self.observed(guild).await?;
        let Some(actual) = observed.get("1:oracle") else {
            return Ok(false);
        };
        if !known.contains(&actual.definition) {
            return Err(error(ErrorCode::Conflict));
        }
        self.save(
            guild,
            "1:oracle",
            None,
            &CommandBinding {
                owner: owner.clone(),
                id: Some(actual.id.clone()),
                definition: actual.definition.clone(),
                route: None,
                pending: None,
                target: None,
                deleted: false,
            },
        )
        .await?;
        Ok(true)
    }
    async fn records(&self, guild: &GuildId) -> Result<BTreeMap<String, (u64, CommandBinding)>> {
        let mut result = BTreeMap::new();
        let mut after = None;
        loop {
            let page = self
                .repository
                .workflow_list(guild, WorkflowKind::CommandBinding, after.as_deref(), 100)
                .await?;
            if page.is_empty() {
                break;
            }
            after = page.last().map(|r| r.key.clone());
            for row in page {
                result.insert(
                    row.key,
                    (
                        row.revision,
                        serde_json::from_value(row.value)
                            .map_err(|_| error(ErrorCode::Integrity))?,
                    ),
                );
            }
        }
        Ok(result)
    }
    pub async fn bindings(&self, guild: &GuildId) -> Result<Vec<CommandBinding>> {
        // A stable publication revision prevents a reader from mixing a group
        // marker before publication with command records midway through it.
        for _ in 0..3 {
            let before = self.publication(guild).await?;
            let records = self.records(guild).await?;
            let after = self.publication(guild).await?;
            if before.as_ref().map(|(r, _)| r) != after.as_ref().map(|(r, _)| r) {
                continue;
            }
            return Ok(records
                .into_values()
                .map(|(_, b)| b)
                .filter(|b| {
                    !b.deleted
                        && b.pending.is_none()
                        && !after
                            .as_ref()
                            .is_some_and(|(_, g)| g.active && g.affected.contains(&b.owner))
                })
                .collect());
        }
        Err(error(ErrorCode::Conflict))
    }
    async fn save(
        &self,
        guild: &GuildId,
        key: &str,
        revision: Option<u64>,
        binding: &CommandBinding,
    ) -> Result<u64> {
        Ok(self
            .repository
            .workflow_put(
                guild,
                WorkflowKind::CommandBinding,
                key,
                revision,
                &serde_json::to_value(binding).map_err(|_| error(ErrorCode::Integrity))?,
            )
            .await?
            .revision)
    }
    async fn observed(&self, guild: &GuildId) -> Result<BTreeMap<String, PublishedCommand>> {
        let mut result = BTreeMap::new();
        let mut ids = BTreeSet::new();
        for mut command in self.backend.list(guild).await? {
            command.definition = canonical_definition(&command.definition)?;
            if command.id.is_empty()
                || !ids.insert(command.id.clone())
                || result
                    .insert(command_key(&command.definition)?, command)
                    .is_some()
            {
                return Err(error(ErrorCode::Conflict));
            }
        }
        Ok(result)
    }
    /// Durable publication recovery item for operator status surfaces. It never
    /// exposes desired commands as live invocation bindings.
    pub async fn publication_status(&self, guild: &GuildId) -> Result<Option<Value>> {
        self.publication(guild)
            .await?
            .map(|(_, group)| serde_json::to_value(group).map_err(|_| error(ErrorCode::Integrity)))
            .transpose()
    }
    async fn publication(&self, guild: &GuildId) -> Result<Option<(u64, PublicationGroup)>> {
        self.repository
            .workflow_get(guild, WorkflowKind::CommandGroup, "publication")
            .await?
            .map(|record| {
                let group: PublicationGroup = serde_json::from_value(record.value)
                    .map_err(|_| error(ErrorCode::Integrity))?;
                if group.version != 1 || group.previous_count > 100 || group.affected.len() > 100 {
                    return Err(error(ErrorCode::Integrity));
                }
                Ok((record.revision, group))
            })
            .transpose()
    }
    async fn save_publication(
        &self,
        guild: &GuildId,
        revision: Option<u64>,
        group: &PublicationGroup,
    ) -> Result<u64> {
        Ok(self
            .repository
            .workflow_put(
                guild,
                WorkflowKind::CommandGroup,
                "publication",
                revision,
                &serde_json::to_value(group).map_err(|_| error(ErrorCode::Integrity))?,
            )
            .await?
            .revision)
    }
    async fn previous_plan(
        &self,
        guild: &GuildId,
        group: &PublicationGroup,
    ) -> Result<Vec<CommandBinding>> {
        let mut previous = Vec::new();
        for index in 0..group.previous_count {
            let row = self
                .repository
                .workflow_get(
                    guild,
                    WorkflowKind::CommandGroup,
                    &format!("previous:{index:03}"),
                )
                .await?
                .ok_or_else(|| error(ErrorCode::Integrity))?;
            let slot: PreviousCommand =
                serde_json::from_value(row.value).map_err(|_| error(ErrorCode::Integrity))?;
            if slot.publication != group.id
                || slot.binding.deleted
                || slot.binding.pending.is_some()
                || slot.binding.id.is_none()
            {
                return Err(error(ErrorCode::Integrity));
            }
            previous.push(slot.binding);
        }
        Ok(previous)
    }
    /// Full remote ownership preflight before the first write. The per-command
    /// checks still run after this because Discord has no multi-command transaction.
    async fn preflight(
        &self,
        guild: &GuildId,
        wanted: &BTreeMap<String, DesiredCommand>,
    ) -> Result<()> {
        let records = self.records(guild).await?;
        let observed = self.observed(guild).await?;
        for key in records.keys().chain(wanted.keys()).collect::<BTreeSet<_>>() {
            let current = records.get(key).map(|(_, binding)| binding);
            let actual = observed.get(key);
            if current.is_some_and(|b| b.pending.is_some()) {
                return Err(error(ErrorCode::RecoveryRequired));
            }
            if let (Some(current), Some(desired)) = (current, wanted.get(key))
                && current.owner != desired.owner
            {
                return Err(error(ErrorCode::Conflict));
            }
            match current {
                Some(binding) if !binding.deleted => {
                    if !actual.is_some_and(|remote| {
                        Some(&remote.id) == binding.id.as_ref()
                            && remote.definition == binding.definition
                    }) {
                        return Err(error(ErrorCode::Conflict));
                    }
                }
                _ if actual.is_some() => return Err(error(ErrorCode::Conflict)),
                _ => {}
            }
        }
        Ok(())
    }
    async fn begin_publication(
        &self,
        guild: &GuildId,
        wanted: &BTreeMap<String, DesiredCommand>,
        expires_at: u64,
    ) -> Result<(u64, PublicationGroup)> {
        let records = self.records(guild).await?;
        let previous = records
            .values()
            .map(|(_, b)| b)
            .filter(|b| !b.deleted)
            .cloned()
            .collect::<Vec<_>>();
        if previous.len() > 100 || wanted.len() > 100 {
            return Err(error(ErrorCode::QuotaExceeded));
        }
        let mut affected = BTreeSet::new();
        for (key, (_, binding)) in &records {
            if binding.deleted {
                continue;
            }
            if wanted.get(key).is_none_or(|d| {
                d.definition != binding.definition
                    || d.route != binding.route
                    || d.owner != binding.owner
            }) {
                affected.insert(binding.owner.clone());
            }
        }
        for (key, desired) in wanted {
            if records.get(key).is_none_or(|(_, b)| {
                b.deleted
                    || b.definition != desired.definition
                    || b.route != desired.route
                    || b.owner != desired.owner
            }) {
                affected.insert(desired.owner.clone());
            }
        }
        let mut group = PublicationGroup {
            version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            active: true,
            prepared: false,
            expires_at,
            previous_count: previous.len(),
            affected,
        };
        let prior = self.publication(guild).await?;
        if prior.as_ref().is_some_and(|(_, group)| group.active) {
            return Err(error(ErrorCode::RecoveryRequired));
        }
        // Claim the journal before staging fixed slots. CAS also serializes two
        // host processes: another publisher cannot overwrite an active snapshot.
        let revision = self
            .save_publication(guild, prior.map(|(r, _)| r), &group)
            .await?;
        // A crash while unprepared is safe to unwind: no Discord write has begun.
        for (index, binding) in previous.into_iter().enumerate() {
            let key = format!("previous:{index:03}");
            let previous = self
                .repository
                .workflow_get(guild, WorkflowKind::CommandGroup, &key)
                .await?;
            let value = serde_json::to_value(PreviousCommand {
                publication: group.id.clone(),
                binding,
            })
            .map_err(|_| error(ErrorCode::Integrity))?;
            self.repository
                .workflow_put(
                    guild,
                    WorkflowKind::CommandGroup,
                    &key,
                    previous.map(|r| r.revision),
                    &value,
                )
                .await?;
        }
        group.prepared = true;
        let revision = self.save_publication(guild, Some(revision), &group).await?;
        Ok((revision, group))
    }
    async fn rollback_publication(
        &self,
        guild: &GuildId,
        revision: u64,
        mut group: PublicationGroup,
        guard: &SendGuard,
    ) -> Result<()> {
        if !group.prepared {
            group.active = false;
            group.affected.clear();
            self.save_publication(guild, Some(revision), &group).await?;
            return Ok(());
        }
        let previous = self.previous_plan(guild, &group).await?;
        let current = self.records(guild).await?;
        // A lost create acknowledgement never proves ownership. Do not adopt a
        // matching name, retry it, or release any affected owner's group.
        if current.values().any(|(_, b)| b.pending.is_some()) {
            return Err(error(ErrorCode::RecoveryRequired));
        }
        for old in &previous {
            let key = command_key(&old.definition)?;
            if current
                .get(&key)
                .is_none_or(|(_, now)| now.deleted || now.id != old.id || now.owner != old.owner)
            {
                // Discord cannot recreate a deleted command with its former ID.
                return Err(error(ErrorCode::RecoveryRequired));
            }
        }
        let wanted = previous
            .iter()
            .map(|b| {
                Ok((
                    command_key(&b.definition)?,
                    DesiredCommand {
                        owner: b.owner.clone(),
                        definition: b.definition.clone(),
                        route: b.route.clone(),
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        self.apply_plan(guild, &wanted, guard).await?;
        let restored = self.records(guild).await?;
        for old in &previous {
            if restored
                .get(&command_key(&old.definition)?)
                .is_none_or(|(_, now)| now != old)
            {
                return Err(error(ErrorCode::RecoveryRequired));
            }
        }
        group.active = false;
        group.affected.clear();
        self.save_publication(guild, Some(revision), &group).await?;
        Ok(())
    }

    /// Cancel this run whenever its desired revision is superseded. A cancelled run never
    /// dispatches further changes; already-sent changes retain their durable pending record.
    pub async fn reconcile(
        &self,
        guild: &GuildId,
        desired: &[DesiredCommand],
        cancel: &CancellationToken,
        expires_at: u64,
    ) -> Result<CommandReport> {
        self.reconcile_inner(guild, desired, cancel, expires_at, None)
            .await
    }
    /// Bind actual writes to a live registry revision, including writes after rate waits.
    pub async fn reconcile_fenced(
        &self,
        guild: &GuildId,
        desired: &[DesiredCommand],
        cancel: &CancellationToken,
        expires_at: u64,
        fence: Arc<dyn DispatchFence>,
    ) -> Result<CommandReport> {
        self.reconcile_inner(guild, desired, cancel, expires_at, Some(fence))
            .await
    }
    async fn reconcile_inner(
        &self,
        guild: &GuildId,
        desired: &[DesiredCommand],
        cancel: &CancellationToken,
        expires_at: u64,
        fence: Option<Arc<dyn DispatchFence>>,
    ) -> Result<CommandReport> {
        let lock = self.guild_lock(guild);
        let _lock = tokio::select! {biased;_=cancel.cancelled()=>return Err(error(ErrorCode::Cancelled)),lock=lock.lock()=>lock};
        let guard = match fence {
            Some(fence) => SendGuard::with_fence(cancel.clone(), expires_at, fence),
            None => SendGuard::new(cancel.clone(), expires_at),
        };
        let mut wanted = BTreeMap::new();
        for d in desired {
            let mut d = d.clone();
            d.definition = canonical_definition(&d.definition)?;
            if wanted.insert(command_key(&d.definition)?, d).is_some() {
                return Err(error(ErrorCode::Conflict));
            }
        }
        if wanted.len() > 100 {
            return Err(error(ErrorCode::QuotaExceeded));
        }
        if let Some((revision, mut group)) = self.publication(guild).await?
            && group.active
        {
            // Another host instance may still own this publication. Its send
            // guard expires at the same durable lease boundary.
            if crate::executor::now() < group.expires_at {
                return Err(error(ErrorCode::RecoveryRequired));
            }
            group.expires_at = expires_at;
            let revision = self.save_publication(guild, Some(revision), &group).await?;
            self.rollback_publication(guild, revision, group, &guard)
                .await?;
            // Recovery never accepts a new group in the same attempt.
            return Err(error(ErrorCode::RecoveryRequired));
        }
        self.preflight(guild, &wanted).await?;
        let (revision, mut group) = self.begin_publication(guild, &wanted, expires_at).await?;
        let result = match self.apply_plan(guild, &wanted, &guard).await {
            Ok(report) => self
                .preflight(guild, &wanted)
                .await
                .and_then(|_| guard.dispatch(|| Ok(report))),
            Err(error) => Err(error),
        };
        match result {
            Ok(report) => {
                group.active = false;
                group.affected.clear();
                self.save_publication(guild, Some(revision), &group).await?;
                Ok(report)
            }
            Err(original) => {
                // Cancellation and uncertain outcomes keep the durable fence.
                // A known late collision can compensate using confirmed IDs.
                if guard.dispatch(|| Ok(())).is_ok() {
                    let _ = self
                        .rollback_publication(guild, revision, group, &guard)
                        .await;
                }
                Err(original)
            }
        }
    }
    async fn apply_plan(
        &self,
        guild: &GuildId,
        wanted: &BTreeMap<String, DesiredCommand>,
        guard: &SendGuard,
    ) -> Result<CommandReport> {
        let mut records = self.records(guild).await?;
        let keys: BTreeSet<_> = records.keys().chain(wanted.keys()).cloned().collect();
        let mut keys = keys.into_iter().collect::<Vec<_>>();
        // Confirm additions and edits before removing old routes where possible.
        keys.sort_by_key(|key| (wanted.get(key).is_none(), key.clone()));
        let mut report = CommandReport::default();
        for key in keys {
            guard.dispatch(|| Ok(()))?;
            let observed = self.observed(guild).await?;
            guard.dispatch(|| Ok(()))?;
            let actual = observed.get(&key);
            let wanted = wanted.get(&key);
            let previous = records.remove(&key);
            let (revision, mut binding) = match previous {
                Some((revision, binding)) => (Some(revision), binding),
                None => {
                    let desired = wanted.ok_or_else(|| error(ErrorCode::Integrity))?;
                    if actual.is_some() {
                        return Err(error(ErrorCode::Conflict));
                    }
                    (
                        None,
                        CommandBinding {
                            owner: desired.owner.clone(),
                            id: None,
                            definition: desired.definition.clone(),
                            route: desired.route.clone(),
                            pending: None,
                            target: None,
                            deleted: true,
                        },
                    )
                }
            };
            if let Some(pending) = &binding.pending {
                // A matching name after an uncertain create is insufficient evidence of ownership.
                let complete = match pending {
                    PendingCommand::Create => false,
                    PendingCommand::Edit => actual.is_some_and(|a| {
                        Some(&a.id) == binding.id.as_ref()
                            && Some(&a.definition) == binding.target.as_ref()
                    }),
                    PendingCommand::Delete => !observed
                        .values()
                        .any(|a| Some(&a.id) == binding.id.as_ref()),
                };
                if !complete {
                    return Err(error(ErrorCode::RecoveryRequired));
                }
                if matches!(pending, PendingCommand::Delete) {
                    binding.deleted = true;
                    binding.id = None;
                    binding.route = None;
                } else {
                    binding.definition = binding
                        .target
                        .take()
                        .ok_or_else(|| error(ErrorCode::Integrity))?;
                    binding.route = None;
                }
                binding.pending = None;
                binding.target = None;
                self.save(guild, &key, revision, &binding).await?;
                // A fresh reconcile must reconsider desired routing after recovering a write.
                return Err(error(ErrorCode::RecoveryRequired));
            }
            if let Some(wanted) = wanted
                && wanted.owner != binding.owner
            {
                return Err(error(ErrorCode::Conflict));
            }
            if !binding.deleted
                && !actual.is_some_and(|a| {
                    Some(&a.id) == binding.id.as_ref() && a.definition == binding.definition
                })
            {
                return Err(error(ErrorCode::Conflict));
            }
            if binding.deleted && actual.is_some() {
                return Err(error(ErrorCode::Conflict));
            }
            let action = match wanted {
                None if binding.deleted => continue,
                None => PendingCommand::Delete,
                Some(_) if binding.deleted => PendingCommand::Create,
                Some(w) if w.definition == binding.definition => {
                    if binding.route != w.route {
                        binding.route = w.route.clone();
                        self.save(guild, &key, revision, &binding).await?;
                    }
                    report.unchanged += 1;
                    continue;
                }
                Some(_) => PendingCommand::Edit,
            };
            binding.pending = Some(action.clone());
            binding.target = wanted.map(|w| w.definition.clone());
            binding.route = None;
            let revision = self.save(guild, &key, revision, &binding).await?;
            guard.dispatch(|| Ok(()))?;
            let sent = match action {
                PendingCommand::Create => Some(
                    self.backend
                        .create(guild, binding.target.as_ref().unwrap(), guard)
                        .await?,
                ),
                PendingCommand::Edit => Some(
                    self.backend
                        .edit(
                            guild,
                            binding.id.as_deref().unwrap(),
                            binding.target.as_ref().unwrap(),
                            guard,
                        )
                        .await?,
                ),
                PendingCommand::Delete => {
                    self.backend
                        .delete(guild, binding.id.as_deref().unwrap(), guard)
                        .await?;
                    None
                }
            };
            guard.dispatch(|| Ok(()))?;
            let after = self.observed(guild).await?;
            guard.dispatch(|| Ok(()))?;
            if let Some(sent) = sent {
                let observed = after.get(&key).ok_or_else(|| error(ErrorCode::Integrity))?;
                if observed.id != sent.id
                    || canonical_definition(&sent.definition)? != *binding.target.as_ref().unwrap()
                    || observed.definition != *binding.target.as_ref().unwrap()
                    || (!matches!(action, PendingCommand::Create)
                        && Some(&sent.id) != binding.id.as_ref())
                {
                    return Err(error(ErrorCode::Integrity));
                }
                binding.id = Some(sent.id);
                binding.definition = observed.definition.clone();
                binding.deleted = false;
                binding.route = wanted.and_then(|w| w.route.clone());
            } else {
                if after.contains_key(&key)
                    || after.values().any(|a| Some(&a.id) == binding.id.as_ref())
                {
                    return Err(error(ErrorCode::Integrity));
                }
                binding.deleted = true;
                binding.id = None;
            }
            binding.pending = None;
            binding.target = None;
            self.save(guild, &key, Some(revision), &binding).await?;
            match action {
                PendingCommand::Create => report.created += 1,
                PendingCommand::Edit => report.edited += 1,
                PendingCommand::Delete => report.deleted += 1,
            }
        }
        Ok(report)
    }
}
