use dandys_world_core::{
    refresh_review::{
        ReasonCode, ReviewApproval, ReviewStatus, publication_allowed, review_candidate,
    },
    snapshot::Snapshot,
};
use serde_json::{Value, json};
const BASE: u64 = 1_789_171_200_000; // 2026-09-12T00:00:00Z, independently fixed fixture time.
const NOW: u64 = BASE + 7_200_000;
fn catalog() -> Value {
    let source = json!({"id":"page:1","page_id":1,"title":"Fixture Rock","url":"https://dandys-world-robloxhorror.fandom.com/wiki/Fixture_Rock","revision_id":2,"revision_timestamp":"2026-09-11T00:00:00Z","validated_at_ms":BASE+30_000,"content_sha256":"a".repeat(64),"license":"CC-BY-SA-3.0","license_url":"https://creativecommons.org/licenses/by-sa/3.0/"});
    let fact = json!({"id":"fixture.health","key":"health","text":"Two fixture hearts","value":2,"unit":"hearts","conditions":["base"],"state":"supported","citations":[{"source_id":"page:1","section":"Stats","quote":"Two fixture hearts"}]});
    json!({"schema_version":1,"adapter_version":"synthetic-review-fixture","source_origin":"https://dandys-world-robloxhorror.fandom.com","crawl_started_at":"2026-09-12T00:00:00Z","crawl_completed_at":"2026-09-12T00:01:00Z","sources":[source],"entities":[{"id":"toon:fixture","kind":"toon","name":"Fixture Rock","aliases":["Rock"],"availability":"supported","warnings":[],"facts":[fact],"relationships":[]}],"coverage":{"discovered_pages":1,"imported_pages":1,"namespace_counts":{"articles":1},"nonredirect_articles":1,"redirects":0,"entities_by_kind":{"toon":1},"excluded":[],"unresolved_redirects":[],"warnings":[]}})
}
fn bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}
fn snapshot(value: &Value) -> Snapshot {
    Snapshot::from_bytes(&bytes(value)).unwrap()
}
fn refreshed() -> Value {
    let mut c = catalog();
    c["crawl_started_at"] = json!("2026-09-12T01:00:00.123456+00:00");
    c["crawl_completed_at"] = json!("2026-09-12T01:01:00Z");
    c["sources"][0]["validated_at_ms"] = json!(BASE + 3_630_000);
    c
}
fn assert_reason(old: &Value, new: &Value, status: ReviewStatus, code: ReasonCode) {
    let review = review_candidate(&snapshot(old), &bytes(new), NOW);
    assert_eq!(review.status, status, "{:?}", review.reasons);
    assert!(
        review.reasons.iter().any(|r| r.code == code),
        "{:?}",
        review.reasons
    );
}
#[test]
fn unchanged_and_verified_unchanged_timestamps_are_eligible() {
    let active = snapshot(&catalog());
    assert_eq!(
        review_candidate(&active, &bytes(&catalog()), NOW).status,
        ReviewStatus::Eligible
    );
    assert_eq!(
        review_candidate(&active, &bytes(&refreshed()), NOW).status,
        ReviewStatus::Eligible
    );
    assert!(publication_allowed(
        &active,
        &bytes(&refreshed()),
        NOW,
        None
    ));
}
#[test]
fn timestamp_only_refresh_cannot_invent_future_pre_revision_or_outside_crawl_times() {
    for value in [NOW + 1, BASE - 86_400_001, BASE + 3_700_000] {
        let mut candidate = refreshed();
        candidate["sources"][0]["validated_at_ms"] = json!(value);
        assert_eq!(
            review_candidate(&snapshot(&catalog()), &bytes(&candidate), NOW).status,
            ReviewStatus::Rejected
        );
    }
    let mut candidate = refreshed();
    candidate["sources"][0]["validated_at_ms"] = json!(BASE + 1);
    assert_reason(
        &catalog(),
        &candidate,
        ReviewStatus::Rejected,
        ReasonCode::ValidationRegressed,
    );
}
#[test]
fn malformed_calendar_and_crawl_regression_are_rejected() {
    for timestamp in [
        "2026-02-30T00:00:00Z",
        "2026-09-12T25:00:00Z",
        "2026-09-12T01:00:00.1234567890Z",
        "2026-09-12T01:00:00+01:00",
        "nonsense",
    ] {
        let mut candidate = refreshed();
        candidate["crawl_started_at"] = json!(timestamp);
        assert_reason(
            &catalog(),
            &candidate,
            ReviewStatus::Rejected,
            ReasonCode::InvalidTimestamp,
        );
    }
    let mut candidate = refreshed();
    candidate["crawl_completed_at"] = json!("2026-09-12T00:00:01Z");
    assert_eq!(
        review_candidate(&snapshot(&catalog()), &bytes(&candidate), NOW).status,
        ReviewStatus::Rejected
    );
}
#[test]
fn immutable_revision_identity_and_hash_cannot_be_rewritten() {
    for (field, value, code) in [
        (
            "content_sha256",
            json!("b".repeat(64)),
            ReasonCode::RevisionContentMismatch,
        ),
        (
            "revision_timestamp",
            json!("2026-09-11T00:00:01Z"),
            ReasonCode::RevisionContentMismatch,
        ),
        ("revision_id", json!(1), ReasonCode::RevisionRegressed),
        ("page_id", json!(2), ReasonCode::SourceIdentityMismatch),
        (
            "url",
            json!("https://dandys-world-robloxhorror.fandom.com/wiki/Someone_Else"),
            ReasonCode::SourceIdentityMismatch,
        ),
    ] {
        let mut candidate = refreshed();
        candidate["sources"][0][field] = value;
        assert_reason(&catalog(), &candidate, ReviewStatus::Rejected, code);
    }
}
#[test]
fn new_source_revision_requires_explicit_exact_byte_pair_approval() {
    let active = snapshot(&catalog());
    let mut candidate = refreshed();
    candidate["sources"][0]["revision_id"] = json!(3);
    candidate["sources"][0]["revision_timestamp"] = json!("2026-09-12T00:30:00Z");
    candidate["sources"][0]["content_sha256"] = json!("b".repeat(64));
    let candidate_bytes = bytes(&candidate);
    let review = review_candidate(&active, &candidate_bytes, NOW);
    assert_eq!(review.status, ReviewStatus::ReviewRequired);
    assert!(!publication_allowed(&active, &candidate_bytes, NOW, None));
    let approval = ReviewApproval {
        active_digest: review.active_digest,
        candidate_digest: review.candidate_digest,
    };
    assert!(publication_allowed(
        &active,
        &candidate_bytes,
        NOW,
        Some(&approval)
    ));
    let pretty = serde_json::to_vec_pretty(&candidate).unwrap();
    assert!(
        !publication_allowed(&active, &pretty, NOW, Some(&approval)),
        "approval is bound to exact bytes"
    );
    let newer_active = snapshot(&refreshed());
    assert!(
        !publication_allowed(&newer_active, &candidate_bytes, NOW, Some(&approval)),
        "approval cannot be replayed after active changed"
    );
}
#[test]
fn all_gameplay_fact_changes_and_parser_version_changes_require_review() {
    for (pointer, value) in [
        ("/entities/0/facts/0/value", json!(3)),
        ("/entities/0/facts/0/conditions", json!(["even floors"])),
        ("/entities/0/facts/0/text", json!("Changed mechanic")),
        ("/adapter_version", json!("changed-parser")),
        ("/entities/0/aliases", json!(["Different alias"])),
    ] {
        let mut candidate = refreshed();
        *candidate.pointer_mut(pointer).unwrap() = value;
        assert_eq!(
            review_candidate(&snapshot(&catalog()), &bytes(&candidate), NOW).status,
            ReviewStatus::ReviewRequired
        );
    }
}
#[test]
fn parser_quality_loss_is_not_approvable() {
    for candidate in [
        {
            let mut c = refreshed();
            c["entities"][0]["facts"][0]["state"] = json!("unverified");
            c
        },
        {
            let mut c = refreshed();
            c["entities"][0]["facts"] = json!([]);
            c
        },
        {
            let mut c = refreshed();
            c["coverage"]["warnings"] = json!(["parser failed"]);
            c
        },
        {
            let mut c = refreshed();
            c["coverage"]["unresolved_redirects"] =
                json!([{"page_id":1,"title":"Fixture Rock","reason":"unresolved"}]);
            c
        },
    ] {
        let active = snapshot(&catalog());
        let data = bytes(&candidate);
        let review = review_candidate(&active, &data, NOW);
        assert_eq!(review.status, ReviewStatus::Rejected);
        let approval = ReviewApproval {
            active_digest: review.active_digest,
            candidate_digest: review.candidate_digest,
        };
        assert!(!publication_allowed(&active, &data, NOW, Some(&approval)));
    }
}
#[test]
fn explicit_conflicting_fact_is_quarantined_and_requires_review() {
    let mut candidate = refreshed();
    candidate["entities"][0]["facts"][0]["state"] = json!("conflicting");
    assert_reason(
        &catalog(),
        &candidate,
        ReviewStatus::ReviewRequired,
        ReasonCode::FactsChanged,
    );
}
#[test]
fn incomplete_discovery_is_rejected_before_approval() {
    let active = snapshot(&catalog());
    let mut candidate = refreshed();
    candidate["coverage"]["discovered_pages"] = json!(2);
    let data = bytes(&candidate);
    let review = review_candidate(&active, &data, NOW);
    assert_eq!(review.status, ReviewStatus::Rejected);
    let approval = ReviewApproval {
        active_digest: review.active_digest,
        candidate_digest: review.candidate_digest,
    };
    assert!(!publication_allowed(&active, &data, NOW, Some(&approval)));
    assert_eq!(
        review_candidate(&active, b"not-json", NOW).status,
        ReviewStatus::Rejected
    );
}
fn two_entities() -> Value {
    let mut data = catalog();
    let mut entity = data["entities"][0].clone();
    entity["id"] = json!("toon:second");
    entity["name"] = json!("Second");
    entity["facts"][0]["id"] = json!("second.health");
    data["entities"].as_array_mut().unwrap().push(entity);
    data["coverage"]["entities_by_kind"]["toon"] = json!(2);
    data
}
#[test]
fn additions_and_deletions_require_review_but_category_loss_is_rejected() {
    assert_reason(
        &catalog(),
        &two_entities(),
        ReviewStatus::ReviewRequired,
        ReasonCode::EntityAdded,
    );
    let old = many_entities();
    let mut deleted = old.clone();
    deleted["entities"].as_array_mut().unwrap().pop();
    deleted["coverage"]["entities_by_kind"]["toon"] = json!(9);
    assert_reason(
        &old,
        &deleted,
        ReviewStatus::ReviewRequired,
        ReasonCode::EntityDeleted,
    );
    let mut candidate = catalog();
    candidate["entities"][0]["id"] = json!("twisted:fixture");
    candidate["entities"][0]["kind"] = json!("twisted");
    candidate["entities"][0]["facts"][0]["id"] = json!("twisted.health");
    candidate["coverage"]["entities_by_kind"] = json!({"twisted":1});
    assert_reason(
        &catalog(),
        &candidate,
        ReviewStatus::Rejected,
        ReasonCode::CategoryLoss,
    );
}
#[test]
fn source_or_fact_identity_reassignment_is_rejected() {
    let mut candidate = two_entities();
    candidate["entities"][1]["facts"][0]["id"] = json!("fixture.health");
    candidate["entities"].as_array_mut().unwrap().remove(0);
    candidate["coverage"]["entities_by_kind"]["toon"] = json!(1);
    assert_reason(
        &catalog(),
        &candidate,
        ReviewStatus::Rejected,
        ReasonCode::EntityIdentityMismatch,
    );
}
#[test]
fn collection_order_is_semantically_irrelevant_but_approval_digests_are_exact() {
    let old = two_entities();
    let mut candidate = old.clone();
    candidate["entities"].as_array_mut().unwrap().reverse();
    let active = snapshot(&old);
    let result = review_candidate(&active, &bytes(&candidate), NOW);
    assert_eq!(result.status, ReviewStatus::Eligible);
    assert_ne!(result.active_digest, result.candidate_digest);
}
#[test]
fn reports_are_bounded_and_do_not_copy_game_text() {
    let old = catalog();
    let mut candidate = two_entities();
    candidate["entities"][1]["name"] = json!("PRIVATE WIKI BODY".repeat(1000));
    let report =
        serde_json::to_string(&review_candidate(&snapshot(&old), &bytes(&candidate), NOW)).unwrap();
    assert!(report.len() < 30_000);
    assert!(!report.contains("PRIVATE WIKI BODY"));
}

fn many_sources() -> Value {
    let mut data = catalog();
    for id in 2..=10 {
        let mut source = data["sources"][0].clone();
        source["id"] = json!(format!("page:{id}"));
        source["page_id"] = json!(id);
        source["title"] = json!(format!("Supporting {id}"));
        source["url"] = json!(format!(
            "https://dandys-world-robloxhorror.fandom.com/wiki/Supporting_{id}"
        ));
        data["sources"].as_array_mut().unwrap().push(source);
    }
    data["coverage"]["discovered_pages"] = json!(10);
    data["coverage"]["imported_pages"] = json!(10);
    data["coverage"]["nonredirect_articles"] = json!(10);
    data["coverage"]["namespace_counts"]["articles"] = json!(10);
    data
}
#[test]
fn complete_discovery_deletions_require_review_until_major_coverage_loss() {
    let old = many_sources();
    for (count, status, reason) in [
        (9, ReviewStatus::ReviewRequired, ReasonCode::SourceDeleted),
        (8, ReviewStatus::ReviewRequired, ReasonCode::SourceDeleted),
        (7, ReviewStatus::Rejected, ReasonCode::DiscoveryLoss),
    ] {
        let mut candidate = old.clone();
        candidate["sources"].as_array_mut().unwrap().truncate(count);
        candidate["coverage"]["discovered_pages"] = json!(count);
        candidate["coverage"]["imported_pages"] = json!(count);
        candidate["coverage"]["nonredirect_articles"] = json!(count);
        candidate["coverage"]["namespace_counts"]["articles"] = json!(count);
        assert_reason(&old, &candidate, status, reason);
    }
}
#[test]
fn legitimate_page_rename_is_reviewable_without_changing_page_identity() {
    let mut candidate = refreshed();
    candidate["sources"][0]["title"] = json!("Renamed Rock");
    candidate["sources"][0]["url"] =
        json!("https://dandys-world-robloxhorror.fandom.com/wiki/Renamed_Rock");
    candidate["sources"][0]["revision_id"] = json!(3);
    candidate["sources"][0]["revision_timestamp"] = json!("2026-09-12T00:30:00Z");
    assert_reason(
        &catalog(),
        &candidate,
        ReviewStatus::ReviewRequired,
        ReasonCode::SourceChanged,
    );
}
#[test]
fn per_page_validation_does_not_freshen_untouched_sources() {
    let old = many_sources();
    let mut candidate = old.clone();
    candidate["crawl_started_at"] = json!("2026-09-12T01:00:00Z");
    candidate["crawl_completed_at"] = json!("2026-09-12T01:01:00Z");
    candidate["sources"][0]["validated_at_ms"] = json!(BASE + 3_630_000);
    assert_eq!(
        review_candidate(&snapshot(&old), &bytes(&candidate), NOW).status,
        ReviewStatus::Eligible
    );
    assert_eq!(
        candidate["sources"][1]["validated_at_ms"],
        old["sources"][1]["validated_at_ms"]
    );
}

fn many_entities() -> Value {
    let mut data = catalog();
    for id in 2..=10 {
        let mut entity = data["entities"][0].clone();
        entity["id"] = json!(format!("toon:fixture{id}"));
        entity["facts"][0]["id"] = json!(format!("fixture{id}.health"));
        data["entities"].as_array_mut().unwrap().push(entity);
    }
    data["coverage"]["entities_by_kind"]["toon"] = json!(10);
    data
}
#[test]
fn major_entity_discovery_loss_cannot_hide_behind_unchanged_source_coverage() {
    let old = many_entities();
    let mut candidate = old.clone();
    candidate["entities"].as_array_mut().unwrap().truncate(7);
    candidate["coverage"]["entities_by_kind"]["toon"] = json!(7);
    assert_reason(
        &old,
        &candidate,
        ReviewStatus::Rejected,
        ReasonCode::CategoryLoss,
    );
}
