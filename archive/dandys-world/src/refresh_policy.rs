//! Bounded refresh timing. Remote retry guidance never authorizes faster retries.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RefreshLimits {
    pub interval_seconds: u64,
    pub request_timeout_seconds: u64,
    pub transient_retries: u8,
    pub crawl_timeout_seconds: u64,
    pub max_pages: usize,
    pub max_page_bytes: usize,
    pub max_candidate_bytes: usize,
    pub max_corpus_bytes: usize,
}
impl Default for RefreshLimits {
    fn default() -> Self {
        Self {
            interval_seconds: 6 * 60 * 60,
            request_timeout_seconds: 15,
            transient_retries: 2,
            crawl_timeout_seconds: 15 * 60,
            max_pages: 10_000,
            max_page_bytes: 4 * 1024 * 1024,
            max_candidate_bytes: 128 * 1024 * 1024,
            max_corpus_bytes: 512 * 1024 * 1024,
        }
    }
}
impl RefreshLimits {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(60..=7 * 86_400).contains(&self.interval_seconds)
            || !(1..=60).contains(&self.request_timeout_seconds)
            || self.transient_retries > 2
            || !(self.request_timeout_seconds..=900).contains(&self.crawl_timeout_seconds)
            || !(1..=10_000).contains(&self.max_pages)
            || !(1..=4 * 1024 * 1024).contains(&self.max_page_bytes)
            || !(self.max_page_bytes..=128 * 1024 * 1024).contains(&self.max_candidate_bytes)
            || !(self.max_candidate_bytes..=512 * 1024 * 1024).contains(&self.max_corpus_bytes)
        {
            return Err("refresh limits exceed the supported bounds");
        }
        Ok(())
    }
    /// +/-10% jitter, deterministic when entropy is supplied by a test. Does not
    /// alter source validation times. Saturation means never schedule early.
    pub fn next_regular_check_ms(&self, now_ms: u64, entropy: u64) -> u64 {
        let interval_ms = self.interval_seconds.saturating_mul(1000);
        let spread = interval_ms / 10;
        let jitter = entropy % spread.saturating_mul(2).saturating_add(1);
        now_ms.saturating_add(interval_ms.saturating_sub(spread).saturating_add(jitter))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchFailure {
    Timeout,
    Network,
    Http(u16),
    Challenge,
    InvalidResponse,
    LimitExceeded,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryDecision {
    RetryAt(u64),
    Exhausted,
    StopDenied,
    Reject,
}
/// A failed attempt is counted from zero. `retry_after_ms` is an already parsed
/// absolute deadline; a server delay beyond this crawl's budget aborts the crawl.
/// Successful requests and eligible retries are serialized by the caller.
pub fn retry_decision(
    limits: &RefreshLimits,
    failure: FetchFailure,
    failed_attempt: u8,
    now_ms: u64,
    crawl_deadline_ms: u64,
    retry_after_ms: Option<u64>,
) -> RetryDecision {
    use FetchFailure::*;
    match failure {
        Http(401 | 403) | Challenge => return RetryDecision::StopDenied,
        InvalidResponse | LimitExceeded => return RetryDecision::Reject,
        Http(code) if code != 429 && !(500..=599).contains(&code) => {
            return RetryDecision::Reject;
        }
        _ => {}
    }
    if failed_attempt >= limits.transient_retries || now_ms >= crawl_deadline_ms {
        return RetryDecision::Exhausted;
    }
    let delay = 1000u64.saturating_mul(1u64 << failed_attempt.min(10));
    let retry_at = now_ms
        .saturating_add(delay)
        .max(retry_after_ms.unwrap_or(0));
    if retry_at
        .checked_add(limits.request_timeout_seconds.saturating_mul(1000))
        .is_none_or(|end| end > crawl_deadline_ms)
    {
        RetryDecision::Exhausted
    } else {
        RetryDecision::RetryAt(retry_at)
    }
}
