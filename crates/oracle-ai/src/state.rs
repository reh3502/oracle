//! Durable semantic run facts. Native provider reasoning is deliberately not stored here.
use crate::{
    budget::{Budget, Limits, PriceTable},
    provider::ModelProfile,
};
use oracle_core::{
    CoreService, Error, ErrorCode, GuildId, OperationId, PolicyContext, Result, WorkflowKind,
    WorkflowRepository,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Inspecting,
    Planning,
    Executing,
    Verifying,
    WaitingInput,
    WaitingApproval,
    Recovering,
    Paused,
    Succeeded,
    Partial,
    Cancelled,
}
impl RunStatus {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Partial | Self::Cancelled)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub id: OperationId,
    pub guild: GuildId,
    pub owner: String,
    pub goal: String,
    pub profile: ModelProfile,
    pub limits: Limits,
    pub prices: PriceTable,
    pub budget: Budget,
    pub status: RunStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub catalog_revision: Option<u64>,
    pub problem: Option<String>,
    /// Host-issued references created by this run; never populated from model claims.
    pub references: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SavedRun {
    pub revision: u64,
    pub run: Run,
}

/// Host-only persistence facade. Public ingress must supply authenticated context.
pub struct RunStore {
    core: Arc<CoreService>,
    repository: Arc<dyn WorkflowRepository>,
}
fn owner(context: &PolicyContext) -> String {
    match context {
        PolicyContext::LocalOperator => "local_operator".into(),
        PolicyContext::Discord { user, .. } => format!("discord:{user}"),
    }
}
fn invalid() -> Error {
    Error::new(ErrorCode::InvalidInput)
}
impl RunStore {
    pub fn new(core: Arc<CoreService>, repository: Arc<dyn WorkflowRepository>) -> Self {
        Self { core, repository }
    }

    pub async fn create(&self, context: &PolicyContext, run: Run) -> Result<SavedRun> {
        self.core.authorize_module(context, &run.guild).await?;
        run.limits.validate().map_err(|_| invalid())?;
        if run.owner != owner(context)
            || run.goal.trim().is_empty()
            || run.goal.len() > 4096
            || run.status != RunStatus::Inspecting
            || !run.references.is_empty()
            || run.created_at_ms >= run.limits.deadline_ms
            || run.updated_at_ms != run.created_at_ms
            || run.budget.requests != 0
            || run.budget.pending.is_some()
            || run.prices.revision.is_empty()
            || run.prices.micros_per_million_tokens == 0
        {
            return Err(invalid());
        }
        let value = serde_json::to_value(&run).map_err(|_| invalid())?;
        let record = self
            .repository
            .workflow_put(
                &run.guild,
                WorkflowKind::AgentRun,
                run.id.as_str(),
                None,
                &value,
            )
            .await?;
        Ok(SavedRun {
            revision: record.revision,
            run,
        })
    }

    pub async fn inspect(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        id: &OperationId,
    ) -> Result<SavedRun> {
        self.core.status(context, Some(guild)).await?;
        let record = self
            .repository
            .workflow_get(guild, WorkflowKind::AgentRun, id.as_str())
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        let run: Run =
            serde_json::from_value(record.value).map_err(|_| Error::new(ErrorCode::Integrity))?;
        if run.guild != *guild || run.id != *id {
            return Err(Error::new(ErrorCode::Integrity));
        }
        if run.owner != owner(context) {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        Ok(SavedRun {
            revision: record.revision,
            run,
        })
    }

    /// Authorization is rechecked on resume; old permission flags are never persisted.
    pub async fn authorize(&self, context: &PolicyContext, saved: &SavedRun) -> Result<()> {
        if saved.run.owner != owner(context) {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        self.core.authorize_module(context, &saved.run.guild).await
    }

    pub(crate) async fn save(&self, saved: &mut SavedRun, now_ms: u64) -> Result<()> {
        saved.run.updated_at_ms = now_ms;
        let value = serde_json::to_value(&saved.run).map_err(|_| invalid())?;
        let record = self
            .repository
            .workflow_put(
                &saved.run.guild,
                WorkflowKind::AgentRun,
                saved.run.id.as_str(),
                Some(saved.revision),
                &value,
            )
            .await?;
        saved.revision = record.revision;
        Ok(())
    }

    /// A crashed provider attempt may already be billed. Reconcile that reservation,
    /// then pause for explicit resume and fresh receipt inspection; never replay tools.
    pub async fn recover(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        id: &OperationId,
        now_ms: u64,
    ) -> Result<SavedRun> {
        let mut saved = self.inspect(context, guild, id).await?;
        if saved.run.status.terminal() {
            return Ok(saved);
        }
        if let Some(reservation) = saved.run.budget.pending.clone() {
            saved
                .run
                .budget
                .settle(&reservation, None)
                .map_err(|_| Error::new(ErrorCode::Integrity))?;
        }
        for mut call in self.calls(&saved).await? {
            if call.call.state == CallState::Admitted {
                self.finish_call(
                    &saved,
                    &mut call,
                    serde_json::json!({"error":"interrupted_call_requires_receipt_reconciliation"}),
                    true,
                    true,
                )
                .await?;
            }
        }
        saved.run.status = RunStatus::Recovering;
        saved.run.problem = Some("interrupted_run_requires_receipt_reconciliation".into());
        self.save(&mut saved, now_ms).await?;
        Ok(saved)
    }
}

#[derive(Clone, Debug)]
pub struct SavedCall {
    /// Only the winner of the insertion may dispatch; existing admissions are recovery work.
    pub newly_admitted: bool,
    pub revision: u64,
    pub call: CallRecord,
}
fn call_key(run: &OperationId, call_id: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{}:{:x}", run, Sha256::digest(call_id.as_bytes()))
}
impl RunStore {
    /// Record admission before dispatch. Repeated IDs return the original outcome;
    /// they never allocate fresh authority or a second invocation.
    pub async fn admit_call(&self, saved: &SavedRun, call: CallRecord) -> Result<SavedCall> {
        if call.run != saved.run.id
            || call.call_id.is_empty()
            || call.call_id.len() > 256
            || call.name.is_empty()
            || call.name.len() > 64
            || call.binding.len() > 512
            || call.state != CallState::Admitted
            || call.result.is_some()
            || call.is_error
            || serde_json::to_vec(&call.arguments)
                .map_err(|_| invalid())?
                .len()
                > 8192
        {
            return Err(invalid());
        }
        let key = call_key(&call.run, &call.call_id);
        let value = serde_json::to_value(&call).map_err(|_| invalid())?;
        match self
            .repository
            .workflow_put(
                &saved.run.guild,
                WorkflowKind::AgentCall,
                &key,
                None,
                &value,
            )
            .await
        {
            Ok(record) => Ok(SavedCall {
                newly_admitted: true,
                revision: record.revision,
                call,
            }),
            Err(error) if error.code == ErrorCode::Conflict => {
                let record = self
                    .repository
                    .workflow_get(&saved.run.guild, WorkflowKind::AgentCall, &key)
                    .await?
                    .ok_or_else(|| Error::new(ErrorCode::Integrity))?;
                let existing: CallRecord = serde_json::from_value(record.value)
                    .map_err(|_| Error::new(ErrorCode::Integrity))?;
                if existing.run != call.run
                    || existing.call_id != call.call_id
                    || existing.name != call.name
                    || existing.binding != call.binding
                    || existing.arguments != call.arguments
                {
                    return Err(Error::new(ErrorCode::Conflict));
                }
                Ok(SavedCall {
                    newly_admitted: false,
                    revision: record.revision,
                    call: existing,
                })
            }
            Err(error) => Err(error),
        }
    }

    pub async fn finish_call(
        &self,
        run: &SavedRun,
        saved: &mut SavedCall,
        result: Value,
        is_error: bool,
        unknown: bool,
    ) -> Result<()> {
        if saved.call.run != run.run.id
            || saved.call.state != CallState::Admitted
            || serde_json::to_vec(&result).map_err(|_| invalid())?.len() > 16 * 1024
        {
            return Err(Error::new(ErrorCode::Conflict));
        }
        let mut call = saved.call.clone();
        call.state = if unknown {
            CallState::Unknown
        } else {
            CallState::Finished
        };
        call.result = Some(result);
        call.is_error = is_error;
        let value = serde_json::to_value(&call).map_err(|_| invalid())?;
        let record = self
            .repository
            .workflow_put(
                &run.run.guild,
                WorkflowKind::AgentCall,
                &call_key(&call.run, &call.call_id),
                Some(saved.revision),
                &value,
            )
            .await?;
        saved.revision = record.revision;
        saved.newly_admitted = false;
        saved.call = call;
        Ok(())
    }

    pub async fn calls(&self, run: &SavedRun) -> Result<Vec<SavedCall>> {
        let prefix = format!("{}:", run.run.id);
        let mut cursor = prefix.clone();
        let mut calls = Vec::new();
        loop {
            let records = self
                .repository
                .workflow_list(&run.run.guild, WorkflowKind::AgentCall, Some(&cursor), 100)
                .await?;
            if records.is_empty() {
                return Ok(calls);
            }
            for record in records {
                if !record.key.starts_with(&prefix) {
                    return Ok(calls);
                }
                cursor = record.key;
                let call: CallRecord = serde_json::from_value(record.value)
                    .map_err(|_| Error::new(ErrorCode::Integrity))?;
                if call.run != run.run.id || cursor != call_key(&call.run, &call.call_id) {
                    return Err(Error::new(ErrorCode::Integrity));
                }
                calls.push(SavedCall {
                    newly_admitted: false,
                    revision: record.revision,
                    call,
                });
                if calls.len() > run.run.limits.max_tool_calls as usize {
                    return Err(Error::new(ErrorCode::Integrity));
                }
            }
        }
    }
}

impl Run {
    pub fn new(
        context: &PolicyContext,
        guild: GuildId,
        goal: String,
        profile: ModelProfile,
        limits: Limits,
        prices: PriceTable,
        now_ms: u64,
    ) -> Self {
        Self {
            id: OperationId::generate(),
            guild,
            owner: owner(context),
            goal,
            profile,
            limits,
            prices,
            budget: Budget::default(),
            status: RunStatus::Inspecting,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            catalog_revision: None,
            problem: None,
            references: Vec::new(),
        }
    }
}

/// One durable tool call per record keeps run summaries small and makes replay explicit.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallRecord {
    pub run: OperationId,
    pub call_id: String,
    pub name: String,
    pub binding: String,
    pub arguments: Value,
    pub state: CallState,
    pub result: Option<Value>,
    pub is_error: bool,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    Admitted,
    Finished,
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::PreparedTurn;
    use oracle_core::{GuildPolicy, Repository, UserId};
    use oracle_storage::{DatabaseConfig, Storage};
    async fn setup() -> (tempfile::TempDir, Arc<Storage>, RunStore, GuildId) {
        let folder = tempfile::tempdir().unwrap();
        let storage = Arc::new(
            Storage::open(DatabaseConfig::Sqlite {
                path: folder.path().join("test.sqlite"),
            })
            .await
            .unwrap(),
        );
        let guild = GuildId::new("100").unwrap();
        storage
            .initialize_guilds(&[guild.clone(), GuildId::new("200").unwrap()])
            .await
            .unwrap();
        let core = Arc::new(CoreService::new(
            storage.clone(),
            vec![GuildPolicy {
                guild: guild.clone(),
                operators: vec![UserId::new("101").unwrap(), UserId::new("102").unwrap()],
            }],
        ));
        let store = RunStore::new(core, storage.clone());
        (folder, storage, store, guild)
    }
    fn actor(guild: &GuildId, user: &str) -> PolicyContext {
        PolicyContext::Discord {
            guild: guild.clone(),
            user: UserId::new(user).unwrap(),
            manage_guild: true,
        }
    }
    fn draft(context: &PolicyContext, guild: GuildId) -> Run {
        Run::new(
            context,
            guild,
            "Set up Minecraft".into(),
            ModelProfile {
                id: "beta-3.8".into(),
                model: "gemini-3.8-flash".into(),
                api_version: "v1beta".into(),
                max_context_tokens: 10000,
                max_output_tokens: 1000,
            },
            Limits {
                max_tokens: 10000,
                verification_tokens: 1000,
                max_cost_micros: 10000,
                max_requests: 10,
                max_tool_calls: 30,
                max_no_progress_turns: 2,
                deadline_ms: 1000,
            },
            PriceTable {
                revision: "test/v1".into(),
                micros_per_million_tokens: 1000000,
            },
            1,
        )
    }
    #[tokio::test]
    async fn run_records_bind_principal_and_guild_and_recheck_pause() {
        let (_folder, storage, store, guild) = setup().await;
        let context = actor(&guild, "101");
        let saved = store
            .create(&context, draft(&context, guild.clone()))
            .await
            .unwrap();
        assert!(store.inspect(&context, &guild, &saved.run.id).await.is_ok());
        assert_eq!(
            store
                .inspect(&actor(&guild, "102"), &guild, &saved.run.id)
                .await
                .unwrap_err()
                .code,
            ErrorCode::ForbiddenPermission
        );
        assert_eq!(
            store
                .inspect(&context, &GuildId::new("200").unwrap(), &saved.run.id)
                .await
                .unwrap_err()
                .code,
            ErrorCode::ForbiddenScope
        );
        let rev = storage.status(Some(&guild)).await.unwrap().guilds[0].revision;
        storage
            .set_paused(
                &guild,
                true,
                rev,
                "local_operator",
                &OperationId::generate(),
            )
            .await
            .unwrap();
        assert!(store.inspect(&context, &guild, &saved.run.id).await.is_ok());
        assert_eq!(
            store.authorize(&context, &saved).await.unwrap_err().code,
            ErrorCode::ForbiddenPermission
        );
        storage.close().await.unwrap();
    }
    #[tokio::test]
    async fn interrupted_request_preserves_charge_and_cas_blocks_stale_writer() {
        let (_folder, storage, store, guild) = setup().await;
        let context = actor(&guild, "101");
        let mut saved = store
            .create(&context, draft(&context, guild.clone()))
            .await
            .unwrap();
        let mut stale = saved.clone();
        let prepared = PreparedTurn {
            provider_metadata: None,
            body: String::new(),
            input_token_reservation: 100,
            output_token_reservation: 100,
            max_response_bytes: 1000,
            timeout_ms: 10,
        };
        saved
            .run
            .budget
            .reserve(&saved.run.limits, &saved.run.prices, &prepared, 2, false)
            .unwrap();
        store.save(&mut saved, 2).await.unwrap();
        assert_eq!(
            store.save(&mut stale, 3).await.unwrap_err().code,
            ErrorCode::Conflict
        );
        let recovered = store
            .recover(&context, &guild, &saved.run.id, 4)
            .await
            .unwrap();
        assert_eq!(recovered.run.status, RunStatus::Recovering);
        assert_eq!(recovered.run.budget.charged_tokens, 200);
        assert_eq!(recovered.run.budget.unknown_attempts, 1);
        assert!(recovered.run.budget.pending.is_none());
        let serialized = serde_json::to_string(&recovered.run).unwrap();
        assert!(!serialized.contains("opaque"));
        storage.close().await.unwrap();
    }
    #[tokio::test]
    async fn call_ids_preserve_one_outcome_and_restart_never_replays_admission() {
        let (_folder, storage, store, guild) = setup().await;
        let context = actor(&guild, "101");
        let saved = store
            .create(&context, draft(&context, guild.clone()))
            .await
            .unwrap();
        let call = CallRecord {
            run: saved.run.id.clone(),
            call_id: "call-1".into(),
            name: "core_discord_apply_v1".into(),
            binding: "plan:owned".into(),
            arguments: serde_json::json!({"plan_ref":"owned"}),
            state: CallState::Admitted,
            result: None,
            is_error: false,
        };
        let mut admitted = store.admit_call(&saved, call.clone()).await.unwrap();
        assert!(admitted.newly_admitted);
        assert!(
            !store
                .admit_call(&saved, call.clone())
                .await
                .unwrap()
                .newly_admitted
        );
        assert_eq!(
            store
                .admit_call(&saved, call.clone())
                .await
                .unwrap()
                .revision,
            admitted.revision
        );
        store
            .finish_call(
                &saved,
                &mut admitted,
                serde_json::json!({"receipt":"verified-1"}),
                false,
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .admit_call(&saved, call.clone())
                .await
                .unwrap()
                .call
                .state,
            CallState::Finished
        );
        let mut changed = call.clone();
        changed.arguments = serde_json::json!({"plan_ref":"foreign"});
        assert_eq!(
            store.admit_call(&saved, changed).await.unwrap_err().code,
            ErrorCode::Conflict
        );
        let mut pending = call;
        pending.call_id = "call-2".into();
        store.admit_call(&saved, pending).await.unwrap();
        let recovered = store
            .recover(&context, &guild, &saved.run.id, 2)
            .await
            .unwrap();
        let calls = store.calls(&recovered).await.unwrap();
        assert_eq!(calls.len(), 2);
        assert!(
            calls
                .iter()
                .any(|call| call.call.state == CallState::Finished)
        );
        assert!(
            calls
                .iter()
                .any(|call| call.call.state == CallState::Unknown)
        );
        assert!(
            calls
                .iter()
                .all(|call| call.call.state != CallState::Admitted)
        );
        storage.close().await.unwrap();
    }
}
