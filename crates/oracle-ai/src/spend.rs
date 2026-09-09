//! Atomic per-guild UTC-day spending admission shared by all runs.
use crate::budget::Reservation;
use oracle_core::{
    Error, ErrorCode, GuildId, OperationId, Result, WorkflowKind, WorkflowRepository,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DailyReservation {
    pub day: u64,
    pub run: OperationId,
    pub request: Reservation,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Account {
    last_sequence: u32,
    charged_micros: u64,
    pending: Option<Reservation>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Day {
    total_micros: u64,
    runs: BTreeMap<String, Account>,
}

/// A reservation is persisted before network dispatch. The caller supplies a
/// host-authorized guild and limits; this facade is not an RPC or model tool.
pub struct SpendStore {
    repository: Arc<dyn WorkflowRepository>,
}
impl SpendStore {
    pub fn new(repository: Arc<dyn WorkflowRepository>) -> Self {
        Self { repository }
    }

    pub async fn reserve(
        &self,
        guild: &GuildId,
        run: &OperationId,
        request: &Reservation,
        now_ms: u64,
        daily_limit_micros: u64,
    ) -> Result<DailyReservation> {
        if daily_limit_micros == 0 || request.sequence == 0 || request.cost_micros == 0 {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        let day = now_ms / 86_400_000;
        let key = format!("day:{day}");
        for _ in 0..8 {
            let record = self
                .repository
                .workflow_get(guild, WorkflowKind::AgentSpend, &key)
                .await?;
            let revision = record.as_ref().map(|record| record.revision);
            let mut state: Day = record
                .map(|record| serde_json::from_value(record.value))
                .transpose()
                .map_err(|_| Error::new(ErrorCode::Integrity))?
                .unwrap_or_default();
            let total = state
                .total_micros
                .checked_add(request.cost_micros)
                .filter(|total| *total <= daily_limit_micros)
                .ok_or_else(|| Error::new(ErrorCode::QuotaExceeded))?;
            // Bound the record independently of the monetary cap. Exhaustion is explicit.
            if !state.runs.contains_key(run.as_str()) && state.runs.len() >= 128 {
                return Err(Error::new(ErrorCode::QuotaExceeded));
            }
            let account = state.runs.entry(run.to_string()).or_default();
            if account.pending.is_some() || request.sequence <= account.last_sequence {
                return Err(Error::new(ErrorCode::Conflict));
            }
            account.charged_micros = account
                .charged_micros
                .checked_add(request.cost_micros)
                .ok_or_else(|| Error::new(ErrorCode::QuotaExceeded))?;
            account.last_sequence = request.sequence;
            account.pending = Some(request.clone());
            state.total_micros = total;
            let value =
                serde_json::to_value(state).map_err(|_| Error::new(ErrorCode::Integrity))?;
            match self
                .repository
                .workflow_put(guild, WorkflowKind::AgentSpend, &key, revision, &value)
                .await
            {
                Ok(_) => {
                    return Ok(DailyReservation {
                        day,
                        run: run.clone(),
                        request: request.clone(),
                    });
                }
                Err(error) if error.code == ErrorCode::Conflict => continue,
                Err(error) => return Err(error),
            }
        }
        Err(Error::new(ErrorCode::Conflict))
    }

    /// Settle the original UTC day even after midnight. None retains the charge.
    /// Unknown/cancelled attempts cannot refund money that may already be billed.
    pub async fn settle(
        &self,
        guild: &GuildId,
        reservation: &DailyReservation,
        actual_cost_micros: Option<u64>,
    ) -> Result<()> {
        let key = format!("day:{}", reservation.day);
        for _ in 0..8 {
            let record = self
                .repository
                .workflow_get(guild, WorkflowKind::AgentSpend, &key)
                .await?
                .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
            let mut state: Day = serde_json::from_value(record.value)
                .map_err(|_| Error::new(ErrorCode::Integrity))?;
            let account = state
                .runs
                .get_mut(reservation.run.as_str())
                .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
            if account.pending.as_ref() != Some(&reservation.request) {
                return Err(Error::new(ErrorCode::Conflict));
            }
            if let Some(actual) = actual_cost_micros {
                account.charged_micros = account
                    .charged_micros
                    .saturating_sub(reservation.request.cost_micros)
                    .saturating_add(actual);
                state.total_micros = state
                    .total_micros
                    .saturating_sub(reservation.request.cost_micros)
                    .saturating_add(actual);
            }
            account.pending = None;
            let value =
                serde_json::to_value(state).map_err(|_| Error::new(ErrorCode::Integrity))?;
            match self
                .repository
                .workflow_put(
                    guild,
                    WorkflowKind::AgentSpend,
                    &key,
                    Some(record.revision),
                    &value,
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(error) if error.code == ErrorCode::Conflict => continue,
                Err(error) => return Err(error),
            }
        }
        Err(Error::new(ErrorCode::Conflict))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oracle_storage::{DatabaseConfig, Storage};
    fn request(sequence: u32, cost: u64) -> Reservation {
        Reservation {
            sequence,
            tokens: cost,
            cost_micros: cost,
            price_revision: "test/v1".into(),
            micros_per_million_tokens: 1_000_000,
        }
    }
    #[tokio::test]
    async fn simultaneous_runs_cannot_exceed_daily_cap_or_refund_unknown_requests() {
        let folder = tempfile::tempdir().unwrap();
        let storage = Arc::new(
            Storage::open(DatabaseConfig::Sqlite {
                path: folder.path().join("spend.sqlite"),
            })
            .await
            .unwrap(),
        );
        let guild = GuildId::new("100").unwrap();
        storage
            .initialize_guilds(std::slice::from_ref(&guild))
            .await
            .unwrap();
        let store = SpendStore::new(storage.clone());
        let a = OperationId::generate();
        let b = OperationId::generate();
        let c = OperationId::generate();
        let request = request(1, 60);
        let (first, second) = tokio::join!(
            store.reserve(&guild, &a, &request, 0, 100),
            store.reserve(&guild, &b, &request, 0, 100)
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let reservation = first.or(second).unwrap();
        store.settle(&guild, &reservation, None).await.unwrap();
        assert_eq!(
            store
                .reserve(&guild, &c, &request, 0, 100)
                .await
                .unwrap_err()
                .code,
            ErrorCode::QuotaExceeded
        );
        assert_eq!(
            store
                .settle(&guild, &reservation, Some(0))
                .await
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        // A new UTC day gets a new budget. Settling day zero cannot consume day one.
        let next = store
            .reserve(&guild, &c, &request, 86_400_000, 100)
            .await
            .unwrap();
        assert_eq!(next.day, 1);
        store.settle(&guild, &next, Some(10)).await.unwrap();
        assert!(
            store
                .reserve(&guild, &a, &request, 86_400_000, 100)
                .await
                .is_ok()
        );
        storage.close().await.unwrap();
    }
}
