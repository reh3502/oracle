use dandys_world_core::refresh_policy::{FetchFailure, RefreshLimits};
use dandys_world_core::refresh_schedule::{AttemptResult, RefreshSchedule};

fn schedule() -> RefreshSchedule {
    RefreshSchedule::new("a".repeat(64), 1000, &RefreshLimits::default(), 0).unwrap()
}
fn roundtrip(state: &RefreshSchedule) -> RefreshSchedule {
    serde_json::from_slice(&serde_json::to_vec(state).unwrap()).unwrap()
}
#[test]
fn due_boundary_and_single_flight() {
    let mut state = schedule();
    assert!(!state.due(999));
    assert!(state.start(999).is_err());
    assert!(state.due(1000));
    state.start(1000).unwrap();
    let running = state.clone();
    assert!(!state.due(u64::MAX));
    assert!(state.start(1000).is_err());
    assert!(state.reset(1000, &RefreshLimits::default(), 0).is_err());
    assert_eq!(running, state);
    state
        .finish(AttemptResult::Published, 1100, &RefreshLimits::default(), 0)
        .unwrap();
    assert_eq!(state.last_success_ms(), Some(1100));
    assert_eq!(state.next_due_ms(), Some(1100 + 19_440_000));
    assert!(
        state
            .finish(AttemptResult::Published, 1200, &RefreshLimits::default(), 0)
            .is_err()
    );
    assert_eq!(roundtrip(&state), state);
}
#[test]
fn restart_interrupts_once_and_preserves_success() {
    let limits = RefreshLimits::default();
    let mut state = schedule();
    state.start(1000).unwrap();
    state
        .finish(AttemptResult::Published, 1100, &limits, 0)
        .unwrap();
    let due = state.next_due_ms().unwrap();
    state.start(due).unwrap();
    state = roundtrip(&state);
    state
        .resume(&"a".repeat(64), due + 100, &limits, 0)
        .unwrap();
    assert_eq!(state.last_result(), Some(AttemptResult::Interrupted));
    assert_eq!(state.last_success_ms(), Some(1100));
    assert_eq!(state.next_due_ms(), Some(due + 100 + 54_000));
    let resumed = state.clone();
    state
        .resume(&"a".repeat(64), due + 200, &limits, 10)
        .unwrap();
    assert_eq!(state, resumed);
}
#[test]
fn repeated_failures_back_off_with_bounded_jitter() {
    let limits = RefreshLimits::default();
    let mut state = schedule();
    for attempt in 0..24 {
        let now = state.next_due_ms().unwrap();
        state.start(now).unwrap();
        state
            .finish(AttemptResult::Network, now, &limits, 0)
            .unwrap();
        let seconds = (60u64 << attempt.min(6)).min(3600);
        assert_eq!(state.next_due_ms(), Some(now + seconds * 900));
        state = roundtrip(&state);
    }
    assert_eq!(state.consecutive_failures(), 16);
    let now = state.next_due_ms().unwrap();
    state.start(now).unwrap();
    state
        .finish(AttemptResult::Unchanged, now, &limits, 4_320_000)
        .unwrap();
    assert_eq!(state.consecutive_failures(), 0);
    assert_eq!(state.next_due_ms(), Some(now + 23_760_000));
}
#[test]
fn denied_sources_stay_stopped_until_explicit_reset_or_identity_change() {
    for failure in [
        FetchFailure::Http(401),
        FetchFailure::Http(403),
        FetchFailure::Challenge,
    ] {
        let limits = RefreshLimits::default();
        let mut state = schedule();
        state.start(1000).unwrap();
        state.finish(failure.into(), 1100, &limits, 0).unwrap();
        state = roundtrip(&state);
        state.resume(&"a".repeat(64), 999_999, &limits, 0).unwrap();
        assert!(state.stopped_denied());
        assert!(!state.due(u64::MAX));
        assert!(state.start(u64::MAX).is_err());
        let mut changed = state.clone();
        changed.resume(&"b".repeat(64), 2000, &limits, 0).unwrap();
        assert!(changed.due(2000));
        assert_eq!(changed.last_success_ms(), None);
        state.reset(2000, &limits, 0).unwrap();
        assert!(state.due(2000));
        assert_eq!(state.last_result(), Some(AttemptResult::SourceDenied));
        assert_eq!(state.last_attempt_ms(), Some(1000));
        assert_eq!(state.last_success_ms(), None);
        assert_eq!(roundtrip(&state), state);
    }
}
#[test]
fn reset_never_fabricates_success_or_rewrites_attempt_history() {
    let limits = RefreshLimits::default();
    let mut state = schedule();
    state.start(1000).unwrap();
    state
        .finish(AttemptResult::Published, 1100, &limits, 0)
        .unwrap();
    state.reset(1200, &limits, 0).unwrap();
    state.start(1200).unwrap();
    state
        .finish(AttemptResult::SourceDenied, 1300, &limits, 0)
        .unwrap();
    state.reset(1400, &limits, 0).unwrap();
    assert_eq!(state.last_success_ms(), Some(1100));
    assert_eq!(state.last_attempt_ms(), Some(1200));
    assert_eq!(state.last_completed_ms(), Some(1300));
    assert_eq!(state.last_result(), Some(AttemptResult::SourceDenied));
    assert_eq!(roundtrip(&state), state);
}
#[test]
fn review_and_rejection_are_not_successes() {
    for result in [AttemptResult::ReviewRequired, AttemptResult::Rejected] {
        let mut state = schedule();
        state.start(1000).unwrap();
        state
            .finish(result, 1100, &RefreshLimits::default(), 0)
            .unwrap();
        assert_eq!(state.last_success_ms(), None);
        assert_eq!(state.last_result(), Some(result));
        assert_eq!(state.next_due_ms(), Some(19_441_100));
    }
}
#[test]
fn retry_after_floor_survives_restart() {
    let limits = RefreshLimits::default();
    let mut state = schedule();
    state.start(1000).unwrap();
    state
        .finish_with_retry_after(AttemptResult::RateLimited, 1100, &limits, 0, Some(999_999))
        .unwrap();
    state = roundtrip(&state);
    state.resume(&"a".repeat(64), 2000, &limits, 0).unwrap();
    assert!(!state.due(999_998));
    assert!(state.due(999_999));
}
#[test]
fn invalid_transitions_leave_state_unchanged() {
    let limits = RefreshLimits::default();
    let mut state = schedule();
    state.start(1000).unwrap();
    let saved = state.clone();
    assert!(
        state
            .finish(AttemptResult::Network, 999, &limits, 0)
            .is_err()
    );
    assert_eq!(state, saved);
    assert!(
        state
            .finish(AttemptResult::Network, u64::MAX, &limits, 0)
            .is_err()
    );
    assert_eq!(state, saved);
    assert!(state.resume("bad identity", 2000, &limits, 0).is_err());
    assert_eq!(state, saved);
}
#[test]
fn serialized_state_rejects_unknown_missing_and_inconsistent_fields() {
    let state = serde_json::to_value(schedule()).unwrap();
    for (key, value) in [
        ("unknown", serde_json::json!(true)),
        ("schema_version", serde_json::json!(2)),
        (
            "config_identity",
            serde_json::json!("untrusted remote text"),
        ),
        ("consecutive_failures", serde_json::json!(17)),
        ("running", serde_json::json!(true)),
        ("stopped_denied", serde_json::json!(true)),
        ("last_result", serde_json::json!("remote error text")),
        ("last_success_ms", serde_json::json!(1000)),
        ("next_due_ms", serde_json::Value::Null),
    ] {
        let mut bad = state.clone();
        bad[key] = value;
        assert!(
            serde_json::from_value::<RefreshSchedule>(bad).is_err(),
            "{key}"
        );
    }
    for key in state.as_object().unwrap().keys() {
        let mut bad = state.clone();
        bad.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<RefreshSchedule>(bad).is_err(),
            "missing {key}"
        );
    }
}

#[test]
fn backwards_restart_and_maximum_retry_floor_never_schedule_early() {
    let limits = RefreshLimits::default();
    let mut state = schedule();
    state.start(1000).unwrap();
    let saved = state.clone();
    assert!(state.resume(&"a".repeat(64), 999, &limits, 0).is_err());
    assert_eq!(state, saved);
    state
        .finish_with_retry_after(AttemptResult::RateLimited, 1100, &limits, 0, Some(u64::MAX))
        .unwrap();
    state = roundtrip(&state);
    state.resume(&"a".repeat(64), 1, &limits, 0).unwrap();
    assert_eq!(state.next_due_ms(), Some(u64::MAX));
    assert!(!state.due(u64::MAX - 1));
    let saved = state.clone();
    assert!(state.reset(1099, &limits, 0).is_err());
    assert_eq!(state, saved);
    // Only an explicit reset or changed configuration can bypass a server floor.
    state.reset(1200, &limits, 0).unwrap();
    assert!(state.due(1200));
    assert_eq!(state.last_success_ms(), None);
    let fresh = RefreshSchedule::new("c".repeat(64), u64::MAX, &limits, 0).unwrap();
    assert!(!fresh.due(u64::MAX - 1));
    assert!(fresh.due(u64::MAX));
}

#[test]
fn fetch_failures_map_only_to_bounded_local_results() {
    for (failure, result) in [
        (FetchFailure::Timeout, AttemptResult::Timeout),
        (FetchFailure::Network, AttemptResult::Network),
        (FetchFailure::Http(429), AttemptResult::RateLimited),
        (FetchFailure::Http(503), AttemptResult::ServerFailure),
        (FetchFailure::Http(404), AttemptResult::InvalidResponse),
        (
            FetchFailure::InvalidResponse,
            AttemptResult::InvalidResponse,
        ),
        (FetchFailure::LimitExceeded, AttemptResult::LimitExceeded),
    ] {
        assert_eq!(AttemptResult::from(failure), result);
    }
}
