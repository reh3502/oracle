//! Host startup bookkeeping. No principal is reconstructed and no provider,
//! module invocation or external effect can run through this entry point.
use crate::{
    spend::SpendStore,
    state::{CallState, Run, RunStatus, RunStore, SavedRun},
};
use oracle_core::{Error, ErrorCode, GuildId, Result, WorkflowKind, WorkflowRepository};
use serde::Serialize;

#[derive(Default, Debug, Serialize)]
pub struct RecoverySummary {
    pub inspected_runs: u64,
    pub paused_runs: u64,
    pub settled_attempts: u64,
    pub interrupted_calls: u64,
}

/// Call under exclusive host startup ownership, before accepting ingress. Pages
/// and call lists are bounded; the composition root owns the total deadline and
/// must not publish readiness if the scan fails or is interrupted. Repeating an
/// interrupted scan is safe, including the daily-spend/run-record commit gap.
pub async fn recover_guild(
    runs: &RunStore,
    spend: &SpendStore,
    repository: &dyn WorkflowRepository,
    guild: &GuildId,
    now_ms: u64,
) -> Result<RecoverySummary> {
    let mut summary = RecoverySummary::default();
    let mut cursor = None;
    loop {
        let records = repository
            .workflow_list(guild, WorkflowKind::AgentRun, cursor.as_deref(), 100)
            .await?;
        if records.is_empty() {
            return Ok(summary);
        }
        for record in records {
            if cursor.as_ref().is_some_and(|cursor| record.key <= *cursor) {
                return Err(Error::new(ErrorCode::Integrity));
            }
            cursor = Some(record.key.clone());
            let run: Run = serde_json::from_value(record.value)
                .map_err(|_| Error::new(ErrorCode::Integrity))?;
            if run.guild != *guild
                || run.id.as_str() != record.key
                || run.limits.max_tool_calls > 30
            {
                return Err(Error::new(ErrorCode::Integrity));
            }
            let mut saved = SavedRun {
                revision: record.revision,
                run,
            };
            summary.inspected_runs += 1;
            let mut changed = false;
            if let Some(pending) = saved.run.pending_spend.clone() {
                if pending.run != saved.run.id {
                    return Err(Error::new(ErrorCode::Integrity));
                }
                spend.recover_reservation(guild, &pending).await?;
                saved.run.pending_spend = None;
                changed = true;
            }
            if let Some(reservation) = saved.run.budget.pending.clone() {
                saved
                    .run
                    .budget
                    .settle(&reservation, None)
                    .map_err(|_| Error::new(ErrorCode::Integrity))?;
                summary.settled_attempts += 1;
                changed = true;
            }
            let mut interrupted = false;
            for mut call in runs.calls(&saved).await? {
                if call.call.state == CallState::Admitted {
                    runs.finish_call(&saved, &mut call, serde_json::json!({"error":"interrupted_call_requires_receipt_reconciliation"}), true, true).await?;
                    summary.interrupted_calls += 1;
                    interrupted = true;
                    changed = true;
                }
            }
            let active = matches!(
                saved.run.status,
                RunStatus::Inspecting
                    | RunStatus::Planning
                    | RunStatus::Executing
                    | RunStatus::Verifying
                    | RunStatus::Recovering
            );
            if active || (interrupted && !saved.run.status.terminal()) {
                saved.run.status = RunStatus::Paused;
                saved.run.problem =
                    Some("host_restart_requires_authenticated_receipt_reconciliation".into());
                summary.paused_runs += 1;
                changed = true;
            } else if interrupted && saved.run.status == RunStatus::Succeeded {
                saved.run.status = RunStatus::Partial;
                saved.run.problem = Some("interrupted_call_requires_receipt_reconciliation".into());
            }
            if changed {
                runs.save(&mut saved, now_ms).await?;
            }
        }
    }
}
