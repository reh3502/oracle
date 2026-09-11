//! Durable admission accounting. Unknown billing never refunds a reservation.
use crate::provider::{PreparedTurn, Usage};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_tokens: u64,
    pub verification_tokens: u64,
    pub max_cost_micros: u64,
    pub max_requests: u32,
    pub max_tool_calls: u32,
    pub max_no_progress_turns: u32,
    pub deadline_ms: u64,
}

/// Operator-supplied conservative rate covering input, output and reasoning tokens.
/// This is an admission estimate, never a claim about the provider's final invoice.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceTable {
    pub revision: String,
    pub micros_per_million_tokens: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub sequence: u32,
    pub tokens: u64,
    pub cost_micros: u64,
    pub price_revision: String,
    pub micros_per_million_tokens: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub charged_tokens: u64,
    pub estimated_cost_micros: u64,
    pub reported_tokens: u64,
    pub unknown_attempts: u32,
    pub requests: u32,
    pub tool_calls: u32,
    pub no_progress_turns: u32,
    pub pending: Option<Reservation>,
}

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum BudgetError {
    #[error("invalid budget or pricing")]
    Invalid,
    #[error("token or spending budget exhausted")]
    Exhausted,
    #[error("run deadline expired")]
    Deadline,
    #[error("request or tool limit reached")]
    Limit,
    #[error("run made no progress")]
    NoProgress,
    #[error("reservation must be reconciled before further admission")]
    Pending,
}

fn cost(tokens: u64, rate: u64) -> Result<u64, BudgetError> {
    let micros = (u128::from(tokens) * u128::from(rate)).div_ceil(1_000_000);
    u64::try_from(micros).map_err(|_| BudgetError::Exhausted)
}
impl Limits {
    pub fn validate(&self) -> Result<(), BudgetError> {
        if self.max_tokens == 0
            || self.verification_tokens >= self.max_tokens
            || self.max_cost_micros == 0
            || self.max_requests == 0
            || self.max_tool_calls == 0
            || self.max_no_progress_turns == 0
            || self.deadline_ms == 0
        {
            return Err(BudgetError::Invalid);
        }
        Ok(())
    }
}
impl Budget {
    pub fn check(&self, limits: &Limits, now_ms: u64) -> Result<(), BudgetError> {
        limits.validate()?;
        if self.pending.is_some() {
            return Err(BudgetError::Pending);
        }
        if now_ms >= limits.deadline_ms {
            return Err(BudgetError::Deadline);
        }
        if self.no_progress_turns >= limits.max_no_progress_turns {
            return Err(BudgetError::NoProgress);
        }
        if self.charged_tokens >= limits.max_tokens
            || self.estimated_cost_micros >= limits.max_cost_micros
        {
            return Err(BudgetError::Exhausted);
        }
        Ok(())
    }

    /// Persist this reservation before sending any provider request, including retries.
    /// Verification allowance is host-owned; model text cannot opt into it.
    pub fn reserve(
        &mut self,
        limits: &Limits,
        prices: &PriceTable,
        turn: &PreparedTurn,
        now_ms: u64,
        verification: bool,
    ) -> Result<Reservation, BudgetError> {
        self.check(limits, now_ms)?;
        if prices.revision.is_empty() || prices.micros_per_million_tokens == 0 {
            return Err(BudgetError::Invalid);
        }
        if self.requests >= limits.max_requests {
            return Err(BudgetError::Limit);
        }
        if turn.input_token_reservation == 0 || turn.output_token_reservation == 0 {
            return Err(BudgetError::Invalid);
        }
        let tokens = turn
            .input_token_reservation
            .checked_add(u64::from(turn.output_token_reservation))
            .ok_or(BudgetError::Exhausted)?;
        let charged_tokens = self
            .charged_tokens
            .checked_add(tokens)
            .ok_or(BudgetError::Exhausted)?;
        let ceiling = limits.max_tokens
            - if verification {
                0
            } else {
                limits.verification_tokens
            };
        let reservation_cost = cost(tokens, prices.micros_per_million_tokens)?;
        let total_cost = self
            .estimated_cost_micros
            .checked_add(reservation_cost)
            .ok_or(BudgetError::Exhausted)?;
        if charged_tokens > ceiling || total_cost > limits.max_cost_micros {
            return Err(BudgetError::Exhausted);
        }
        let reservation = Reservation {
            sequence: self.requests + 1,
            tokens,
            cost_micros: reservation_cost,
            price_revision: prices.revision.clone(),
            micros_per_million_tokens: prices.micros_per_million_tokens,
        };
        self.requests += 1;
        self.charged_tokens = charged_tokens;
        self.estimated_cost_micros = total_cost;
        self.pending = Some(reservation.clone());
        Ok(reservation)
    }

    /// A timeout, cancellation or interrupted host uses None, retaining the full charge.
    /// Settlement is exactly once and bound to the recorded attempt.
    pub fn settle(
        &mut self,
        reservation: &Reservation,
        usage: Option<&Usage>,
    ) -> Result<(), BudgetError> {
        if self.pending.as_ref() != Some(reservation) {
            return Err(BudgetError::Pending);
        }
        let reported = usage.and_then(|usage| {
            usage.total_tokens.filter(|total| {
                usage.input_tokens.is_none_or(|input| input <= *total)
                    && usage.output_tokens.is_none_or(|output| output <= *total)
                    && usage
                        .reasoning_tokens
                        .is_none_or(|reasoning| reasoning <= *total)
                    && usage.cached_tokens.is_none_or(|cached| cached <= *total)
            })
        });
        if let Some(tokens) = reported {
            // Record a provider overrun even when it exceeds the admission cap. No
            // subsequent request may erase already billed work or resume at zero.
            self.charged_tokens = self
                .charged_tokens
                .saturating_sub(reservation.tokens)
                .saturating_add(tokens);
            self.estimated_cost_micros = self
                .estimated_cost_micros
                .saturating_sub(reservation.cost_micros)
                .saturating_add(
                    cost(tokens, reservation.micros_per_million_tokens).unwrap_or(u64::MAX),
                );
            self.reported_tokens = self.reported_tokens.saturating_add(tokens);
        } else {
            self.unknown_attempts = self.unknown_attempts.saturating_add(1);
        }
        self.pending = None;
        Ok(())
    }

    /// Admit an entire proposed batch before executing its first tool.
    pub fn admit_tools(
        &mut self,
        limits: &Limits,
        count: u32,
        now_ms: u64,
    ) -> Result<(), BudgetError> {
        self.check(limits, now_ms)?;
        let total = self
            .tool_calls
            .checked_add(count)
            .ok_or(BudgetError::Limit)?;
        if total > limits.max_tool_calls {
            return Err(BudgetError::Limit);
        }
        self.tool_calls = total;
        Ok(())
    }

    /// Progress is determined from host receipts/fresh observations, never model prose.
    pub fn progress(&mut self, changed: bool) {
        self.no_progress_turns = if changed {
            0
        } else {
            self.no_progress_turns.saturating_add(1)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn limits() -> Limits {
        Limits {
            max_tokens: 1000,
            verification_tokens: 100,
            max_cost_micros: 1000,
            max_requests: 3,
            max_tool_calls: 4,
            max_no_progress_turns: 2,
            deadline_ms: 100,
        }
    }
    fn prices() -> PriceTable {
        PriceTable {
            revision: "test/v1".into(),
            micros_per_million_tokens: 1_000_000,
        }
    }
    fn turn(input: u64, output: u32) -> PreparedTurn {
        PreparedTurn {
            provider_metadata: None,
            body: String::new(),
            input_token_reservation: input,
            output_token_reservation: output,
            max_response_bytes: 1000,
            timeout_ms: 10,
        }
    }
    #[test]
    fn reserves_verification_and_rejects_without_mutating() {
        let mut budget = Budget::default();
        assert_eq!(
            budget.reserve(&limits(), &prices(), &turn(850, 100), 1, false),
            Err(BudgetError::Exhausted)
        );
        assert_eq!(budget.requests, 0);
        assert_eq!(budget.charged_tokens, 0);
        assert!(
            budget
                .reserve(&limits(), &prices(), &turn(850, 100), 1, true)
                .is_ok()
        );
    }
    #[test]
    fn unknown_billing_survives_restart_and_is_not_refunded() {
        let mut budget = Budget::default();
        let reservation = budget
            .reserve(&limits(), &prices(), &turn(200, 100), 1, false)
            .unwrap();
        let mut reopened: Budget =
            serde_json::from_str(&serde_json::to_string(&budget).unwrap()).unwrap();
        assert_eq!(reopened.check(&limits(), 2), Err(BudgetError::Pending));
        reopened
            .settle(&reservation, Some(&Usage::default()))
            .unwrap();
        assert_eq!(reopened.charged_tokens, 300);
        assert_eq!(reopened.unknown_attempts, 1);
        assert_eq!(
            reopened.settle(&reservation, None),
            Err(BudgetError::Pending)
        );
    }
    #[test]
    fn actual_usage_releases_reservation_and_uses_original_prices() {
        let mut budget = Budget::default();
        let reservation = budget
            .reserve(&limits(), &prices(), &turn(200, 100), 1, false)
            .unwrap();
        budget
            .settle(
                &reservation,
                Some(&Usage {
                    total_tokens: Some(51),
                    ..Default::default()
                }),
            )
            .unwrap();
        assert_eq!(budget.charged_tokens, 51);
        assert_eq!(budget.estimated_cost_micros, 51);
        assert_eq!(budget.reported_tokens, 51);
    }
    #[test]
    fn overrun_and_inconsistent_usage_cannot_create_new_budget() {
        let mut budget = Budget::default();
        let reservation = budget
            .reserve(&limits(), &prices(), &turn(200, 100), 1, false)
            .unwrap();
        budget
            .settle(
                &reservation,
                Some(&Usage {
                    total_tokens: Some(1),
                    input_tokens: Some(10),
                    ..Default::default()
                }),
            )
            .unwrap();
        assert_eq!(budget.charged_tokens, 300);
        let reservation = budget
            .reserve(&limits(), &prices(), &turn(200, 100), 1, false)
            .unwrap();
        budget
            .settle(
                &reservation,
                Some(&Usage {
                    total_tokens: Some(1001),
                    ..Default::default()
                }),
            )
            .unwrap();
        assert_eq!(budget.check(&limits(), 2), Err(BudgetError::Exhausted));
    }
    #[test]
    fn tool_batch_and_no_progress_limits_are_host_owned() {
        let mut budget = Budget::default();
        assert_eq!(budget.admit_tools(&limits(), 5, 1), Err(BudgetError::Limit));
        assert_eq!(budget.tool_calls, 0);
        budget.admit_tools(&limits(), 4, 1).unwrap();
        assert_eq!(budget.admit_tools(&limits(), 1, 1), Err(BudgetError::Limit));
        budget.progress(false);
        budget.progress(false);
        assert_eq!(budget.check(&limits(), 2), Err(BudgetError::NoProgress));
        budget.progress(true);
        assert!(budget.check(&limits(), 2).is_ok());
        assert_eq!(budget.check(&limits(), 100), Err(BudgetError::Deadline));
    }
}
