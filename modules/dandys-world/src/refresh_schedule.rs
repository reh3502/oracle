//! Pure, serializable refresh scheduling. The caller durably saves each transition
//! before starting work. No transition modifies catalog source timestamps.
use crate::refresh_policy::{FetchFailure, RefreshLimits};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptResult {
    Published,
    Unchanged,
    ReviewRequired,
    Rejected,
    Timeout,
    Network,
    RateLimited,
    ServerFailure,
    SourceDenied,
    InvalidResponse,
    LimitExceeded,
    Interrupted,
}
impl From<FetchFailure> for AttemptResult {
    fn from(failure: FetchFailure) -> Self {
        match failure {
            FetchFailure::Timeout => Self::Timeout,
            FetchFailure::Network => Self::Network,
            FetchFailure::Http(401 | 403) | FetchFailure::Challenge => Self::SourceDenied,
            FetchFailure::Http(429) => Self::RateLimited,
            FetchFailure::Http(500..=599) => Self::ServerFailure,
            FetchFailure::Http(_) | FetchFailure::InvalidResponse => Self::InvalidResponse,
            FetchFailure::LimitExceeded => Self::LimitExceeded,
        }
    }
}
impl AttemptResult {
    fn successful(self) -> bool {
        matches!(self, Self::Published | Self::Unchanged)
    }
    fn regular(self) -> bool {
        self.successful() || matches!(self, Self::ReviewRequired | Self::Rejected)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "ScheduleWire")]
pub struct RefreshSchedule {
    schema_version: u32,
    config_identity: String,
    next_due_ms: Option<u64>,
    last_attempt_ms: Option<u64>,
    last_success_ms: Option<u64>,
    last_completed_ms: Option<u64>,
    last_result: Option<AttemptResult>,
    running: bool,
    stopped_denied: bool,
    consecutive_failures: u8,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleWire {
    schema_version: u32,
    config_identity: String,
    #[serde(deserialize_with = "required_option")]
    next_due_ms: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    last_attempt_ms: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    last_success_ms: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    last_completed_ms: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    last_result: Option<AttemptResult>,
    running: bool,
    stopped_denied: bool,
    consecutive_failures: u8,
}
fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}
impl TryFrom<ScheduleWire> for RefreshSchedule {
    type Error = &'static str;
    fn try_from(w: ScheduleWire) -> Result<Self, Self::Error> {
        let state = Self {
            schema_version: w.schema_version,
            config_identity: w.config_identity,
            next_due_ms: w.next_due_ms,
            last_attempt_ms: w.last_attempt_ms,
            last_success_ms: w.last_success_ms,
            last_completed_ms: w.last_completed_ms,
            last_result: w.last_result,
            running: w.running,
            stopped_denied: w.stopped_denied,
            consecutive_failures: w.consecutive_failures,
        };
        state.validate()?;
        Ok(state)
    }
}
fn valid_identity(identity: &str) -> bool {
    identity.len() == 64
        && identity
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl RefreshSchedule {
    pub fn new(
        config_identity: String,
        now_ms: u64,
        limits: &RefreshLimits,
        _entropy: u64,
    ) -> Result<Self, &'static str> {
        limits.validate()?;
        if !valid_identity(&config_identity) {
            return Err("configuration identity must be a lowercase SHA-256 digest");
        }
        Ok(Self {
            schema_version: 1,
            config_identity,
            next_due_ms: Some(now_ms),
            last_attempt_ms: None,
            last_success_ms: None,
            last_completed_ms: None,
            last_result: None,
            running: false,
            stopped_denied: false,
            consecutive_failures: 0,
        })
    }
    fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1
            || !valid_identity(&self.config_identity)
            || self.consecutive_failures > 16
        {
            return Err("invalid refresh schedule version, identity, or failure count");
        }
        if self.next_due_ms.is_some() == (self.running || self.stopped_denied)
            || (self.running && self.stopped_denied)
            || (self.running && self.last_attempt_ms.is_none())
            || (self.stopped_denied && self.last_result != Some(AttemptResult::SourceDenied))
            || (self.last_result.is_some() != self.last_completed_ms.is_some())
            || (self.last_attempt_ms.is_none()
                && (self.last_result.is_some()
                    || self.last_success_ms.is_some()
                    || self.consecutive_failures != 0))
            || (self.last_success_ms.is_some() && self.last_completed_ms.is_none())
            || (!self.running && self.last_attempt_ms.is_some() && self.last_result.is_none())
            || self
                .next_due_ms
                .zip(self.last_completed_ms)
                .is_some_and(|(due, completed)| due < completed)
            || (self.last_result.is_some_and(AttemptResult::successful)
                && self.last_success_ms != self.last_completed_ms)
            || self
                .last_success_ms
                .zip(self.last_completed_ms)
                .is_some_and(|(s, c)| s > c)
            || self
                .last_attempt_ms
                .zip(self.last_completed_ms)
                .is_some_and(|(a, c)| if self.running { c > a } else { c < a })
        {
            return Err("inconsistent refresh schedule state");
        }
        Ok(())
    }
    /// Call exactly once after loading persisted state. An interrupted job becomes
    /// a failed attempt; a denied source remains stopped across restarts.
    pub fn resume(
        &mut self,
        identity: &str,
        now_ms: u64,
        limits: &RefreshLimits,
        entropy: u64,
    ) -> Result<(), &'static str> {
        limits.validate()?;
        if identity != self.config_identity {
            *self = Self::new(identity.to_owned(), now_ms, limits, entropy)?;
        } else if self.running {
            self.finish(AttemptResult::Interrupted, now_ms, limits, entropy)?;
        }
        Ok(())
    }
    pub fn due(&self, now_ms: u64) -> bool {
        !self.running && !self.stopped_denied && self.next_due_ms.is_some_and(|due| now_ms >= due)
    }
    pub fn start(&mut self, now_ms: u64) -> Result<(), &'static str> {
        if !self.due(now_ms) {
            return Err("refresh is not due or is stopped");
        }
        if self.last_completed_ms.is_some_and(|last| now_ms < last) {
            return Err("refresh clock moved backwards");
        }
        self.running = true;
        self.next_due_ms = None;
        self.last_attempt_ms = Some(now_ms);
        Ok(())
    }
    pub fn finish(
        &mut self,
        result: AttemptResult,
        now_ms: u64,
        limits: &RefreshLimits,
        entropy: u64,
    ) -> Result<(), &'static str> {
        self.finish_with_retry_after(result, now_ms, limits, entropy, None)
    }
    /// An absolute Retry-After floor can only delay the next scheduled job.
    pub fn finish_with_retry_after(
        &mut self,
        result: AttemptResult,
        now_ms: u64,
        limits: &RefreshLimits,
        entropy: u64,
        retry_not_before_ms: Option<u64>,
    ) -> Result<(), &'static str> {
        limits.validate()?;
        if !self.running {
            return Err("no refresh attempt is running");
        }
        if self.last_attempt_ms.is_some_and(|start| now_ms < start) {
            return Err("refresh clock moved backwards");
        }
        let failures = if result.regular() {
            0
        } else {
            self.consecutive_failures.saturating_add(1).min(16)
        };
        let next = if result == AttemptResult::SourceDenied {
            None
        } else {
            let mut timing = limits.clone();
            if !result.regular() {
                timing.interval_seconds = (60u64 << (failures - 1).min(6)).min(3600);
            }
            let delay = timing.next_regular_check_ms(0, entropy);
            Some(
                now_ms
                    .checked_add(delay)
                    .ok_or("refresh deadline overflow")?
                    .max(retry_not_before_ms.unwrap_or(0)),
            )
        };
        self.running = false;
        self.stopped_denied = result == AttemptResult::SourceDenied;
        self.consecutive_failures = failures;
        self.next_due_ms = next;
        self.last_completed_ms = Some(now_ms);
        self.last_result = Some(result);
        if result.successful() {
            self.last_success_ms = Some(now_ms);
        }
        Ok(())
    }
    /// Explicit operator reset clears the stopped state and makes a check due.
    /// Historical success is preserved; resetting never creates a success.
    pub fn reset(
        &mut self,
        now_ms: u64,
        limits: &RefreshLimits,
        _entropy: u64,
    ) -> Result<(), &'static str> {
        limits.validate()?;
        if self.running {
            return Err("cannot reset a running refresh");
        }
        if self.last_completed_ms.is_some_and(|last| now_ms < last) {
            return Err("refresh clock moved backwards");
        }
        self.stopped_denied = false;
        self.next_due_ms = Some(now_ms);
        self.consecutive_failures = 0;
        Ok(())
    }
    pub fn config_identity(&self) -> &str {
        &self.config_identity
    }
    pub fn next_due_ms(&self) -> Option<u64> {
        self.next_due_ms
    }
    pub fn last_attempt_ms(&self) -> Option<u64> {
        self.last_attempt_ms
    }
    pub fn last_success_ms(&self) -> Option<u64> {
        self.last_success_ms
    }
    pub fn last_completed_ms(&self) -> Option<u64> {
        self.last_completed_ms
    }
    pub fn last_result(&self) -> Option<AttemptResult> {
        self.last_result
    }
    pub fn running(&self) -> bool {
        self.running
    }
    pub fn stopped_denied(&self) -> bool {
        self.stopped_denied
    }
    pub fn consecutive_failures(&self) -> u8 {
        self.consecutive_failures
    }
}
