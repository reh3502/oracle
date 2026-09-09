//! Minimal command publication with durable ownership and uncertainty records.
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
/// Server-owned identity and empty localization maps do not change a command definition.
pub fn canonical_definition(value: &Value) -> Result<Value> {
    let mut value = value.clone();
    let object = value
        .as_object_mut()
        .ok_or_else(|| error(ErrorCode::InvalidInput))?;
    for field in ["id", "application_id", "guild_id", "version"] {
        object.remove(field);
    }
    if !object.contains_key("type") {
        object.insert("type".into(), Value::from(1));
    }
    fn normalize(value: &mut Value) {
        match value {
            Value::Object(map) => {
                // Discord's localized display fields are derived from the localization maps.
                map.remove("name_localized");
                map.remove("description_localized");
                for field in [
                    "min_value",
                    "max_value",
                    "min_length",
                    "max_length",
                    "handler",
                    "dm_permission",
                ] {
                    if map.get(field).is_some_and(Value::is_null) {
                        map.remove(field);
                    }
                }
                for field in ["required", "autocomplete"] {
                    if map.get(field).and_then(Value::as_bool) == Some(false) {
                        map.remove(field);
                    }
                }
                for field in [
                    "options",
                    "choices",
                    "channel_types",
                    "file_types",
                    "integration_types",
                ] {
                    if map
                        .get(field)
                        .and_then(Value::as_array)
                        .is_some_and(Vec::is_empty)
                    {
                        map.remove(field);
                    }
                }
                for field in ["name_localizations", "description_localizations"] {
                    if map
                        .get(field)
                        .is_some_and(|v| v.is_null() || v.as_object().is_some_and(|m| m.is_empty()))
                    {
                        map.remove(field);
                    }
                }
                for v in map.values_mut() {
                    normalize(v);
                }
            }
            Value::Array(values) => {
                for v in values {
                    normalize(v);
                }
            }
            _ => {}
        }
    }
    normalize(&mut value);
    // Discord materializes these optional defaults in its readback objects.
    let object = value.as_object_mut().unwrap();
    for field in [
        "default_member_permissions",
        "contexts",
        "integration_types",
    ] {
        if object.get(field).is_some_and(Value::is_null) {
            object.remove(field);
        }
    }
    for (field, default) in [
        ("dm_permission", true),
        ("default_permission", true),
        ("nsfw", false),
    ] {
        if object.get(field).and_then(Value::as_bool) == Some(default) {
            object.remove(field);
        }
    }
    if object
        .get("options")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        object.remove("options");
    }
    command_key(&value)?;
    Ok(value)
}
fn command_key(value: &Value) -> Result<String> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| error(ErrorCode::InvalidInput))?;
    let kind = match value.get("type") {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| error(ErrorCode::InvalidInput))?,
        None => 1,
    };
    if !(1..=3).contains(&kind)
        || name.is_empty()
        || name.chars().count() > 32
        || name.chars().any(char::is_control)
        || (kind == 1
            && name
                .chars()
                .any(|c| !(c.is_lowercase() || c.is_numeric() || c == '-' || c == '_')))
    {
        return Err(error(ErrorCode::InvalidInput));
    }
    let key = format!("{kind}:{name}");
    if key.len() > 128 {
        return Err(error(ErrorCode::InvalidInput));
    }
    Ok(key)
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
        let lock = self
            .locks
            .lock()
            .unwrap()
            .entry(guild.clone())
            .or_default()
            .clone();
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
        Ok(self
            .records(guild)
            .await?
            .into_values()
            .map(|(_, b)| b)
            .filter(|b| !b.deleted && b.pending.is_none())
            .collect())
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
        let lock = self
            .locks
            .lock()
            .unwrap()
            .entry(guild.clone())
            .or_default()
            .clone();
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
        let mut records = self.records(guild).await?;
        let keys: BTreeSet<_> = records.keys().chain(wanted.keys()).cloned().collect();
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
                        .create(guild, binding.target.as_ref().unwrap(), &guard)
                        .await?,
                ),
                PendingCommand::Edit => Some(
                    self.backend
                        .edit(
                            guild,
                            binding.id.as_deref().unwrap(),
                            binding.target.as_ref().unwrap(),
                            &guard,
                        )
                        .await?,
                ),
                PendingCommand::Delete => {
                    self.backend
                        .delete(guild, binding.id.as_deref().unwrap(), &guard)
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
