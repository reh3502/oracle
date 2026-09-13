use dandys_world_core::refresh_policy::{
    FetchFailure as F, RefreshLimits, RetryDecision as D, retry_decision,
};

#[test]
fn defaults_and_overrides_are_bounded() {
    let limits = RefreshLimits::default();
    assert!(limits.validate().is_ok());
    for changed in [
        RefreshLimits {
            transient_retries: 3,
            ..limits.clone()
        },
        RefreshLimits {
            max_pages: 10_001,
            ..limits.clone()
        },
        RefreshLimits {
            crawl_timeout_seconds: 901,
            ..limits.clone()
        },
        RefreshLimits {
            interval_seconds: 0,
            ..limits.clone()
        },
        RefreshLimits {
            max_corpus_bytes: usize::MAX,
            ..limits.clone()
        },
        RefreshLimits {
            max_candidate_bytes: 1,
            ..limits.clone()
        },
    ] {
        assert!(changed.validate().is_err());
    }
    let mut wire = serde_json::to_value(limits).unwrap();
    wire["url"] = "https://unapproved.example/".into();
    assert!(serde_json::from_value::<RefreshLimits>(wire).is_err());
}
#[test]
fn denial_and_bad_data_never_trigger_retries() {
    let limits = RefreshLimits::default();
    for failure in [F::Http(401), F::Http(403), F::Challenge] {
        assert_eq!(
            retry_decision(&limits, failure, 0, 100, 100_000, None),
            D::StopDenied
        );
    }
    for failure in [
        F::Http(400),
        F::Http(404),
        F::Http(301),
        F::InvalidResponse,
        F::LimitExceeded,
    ] {
        assert_eq!(
            retry_decision(&limits, failure, 0, 100, 100_000, None),
            D::Reject
        );
    }
}
#[test]
fn transient_retry_budget_and_deadline_are_exact() {
    let limits = RefreshLimits::default();
    for failure in [F::Timeout, F::Network, F::Http(429), F::Http(503)] {
        assert_eq!(
            retry_decision(&limits, failure, 0, 100, 100_000, None),
            D::RetryAt(1100)
        );
        assert_eq!(
            retry_decision(&limits, failure, 1, 1100, 100_000, None),
            D::RetryAt(3100)
        );
        assert_eq!(
            retry_decision(&limits, failure, 2, 3100, 100_000, None),
            D::Exhausted
        );
        assert_eq!(
            retry_decision(&limits, failure, 0, 99_000, 100_000, None),
            D::Exhausted
        );
    }
}
#[test]
fn retry_after_is_a_floor_never_clipped_to_local_budget() {
    let limits = RefreshLimits::default();
    assert_eq!(
        retry_decision(&limits, F::Http(429), 0, 100, 100_000, Some(50_000)),
        D::RetryAt(50_000)
    );
    assert_eq!(
        retry_decision(&limits, F::Http(429), 0, 100, 100_000, Some(90_000)),
        D::Exhausted
    );
    assert_eq!(
        retry_decision(&limits, F::Http(503), 0, 100, 100_000, Some(0)),
        D::RetryAt(1100)
    );
    assert_eq!(
        retry_decision(&limits, F::Http(503), 0, u64::MAX - 5, u64::MAX, None),
        D::Exhausted
    );
}
#[test]
fn regular_schedule_has_bounded_jitter() {
    let limits = RefreshLimits::default();
    let interval = 21_600_000;
    for entropy in [0, 1, u64::MAX / 2, u64::MAX] {
        let next = limits.next_regular_check_ms(1000, entropy);
        assert!((1000 + interval * 9 / 10..=1000 + interval * 11 / 10).contains(&next));
    }
    assert_eq!(limits.next_regular_check_ms(u64::MAX, 0), u64::MAX);
}
