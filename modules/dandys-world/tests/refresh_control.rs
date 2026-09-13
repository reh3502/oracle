use dandys_world_core::{
    refresh_control::{Error, Outcome, RefreshControl},
    refresh_review::ReviewApproval,
    snapshot::Store,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
const NOW: u64 = 1_789_171_260_000;
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "dw-review-control-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn data() -> Value {
    json!({
        "schema_version":1,"adapter_version":"test-1","source_origin":"https://dandys-world-robloxhorror.fandom.com","crawl_started_at":"2026-09-12T00:00:00Z","crawl_completed_at":"2026-09-12T00:01:00Z",
        "sources":[{"id":"page:1","page_id":1,"title":"Fixture","url":"https://dandys-world-robloxhorror.fandom.com/wiki/Fixture","revision_id":2,"revision_timestamp":"2026-09-12T00:00:00Z","validated_at_ms":1789171230000u64,"content_sha256":"a".repeat(64),"license":"CC BY-SA 3.0","license_url":"https://creativecommons.org/licenses/by-sa/3.0/"}],
        "entities":[{"id":"toon:fixture","kind":"toon","name":"Fixture","aliases":[],"availability":"supported","warnings":[],"facts":[{"id":"fixture.speed","key":"speed","text":"Fixture speed","value":20,"unit":"speed","conditions":["base"],"state":"supported","citations":[{"source_id":"page:1","section":"Stats","quote":"Fixture speed 20"}]}],"relationships":[]}],
        "coverage":{"discovered_pages":1,"imported_pages":1,"namespace_counts":{"articles":1},"nonredirect_articles":1,"redirects":0,"entities_by_kind":{"toon":1},"excluded":[],"unresolved_redirects":[],"warnings":[]}
    })
}
fn bytes(data: &Value) -> Vec<u8> {
    serde_json::to_vec(data).unwrap()
}
fn changed() -> Value {
    let mut value = data();
    value["entities"][0]["facts"][0]["value"] = json!(25);
    value["entities"][0]["facts"][0]["text"] = json!("Revised fixture speed");
    value["sources"][0]["revision_id"] = json!(3);
    value["sources"][0]["content_sha256"] = json!("b".repeat(64));
    value
}
#[test]
fn review_survives_restart_and_requires_exact_pair() {
    let temp = Temp::new();
    let store = Store::new(&temp.0).unwrap();
    let original = store.publish_bytes(&bytes(&data())).unwrap();
    let control = RefreshControl::new(&temp.0).unwrap();
    let Outcome::ReviewRequired { review } = control.submit(&bytes(&changed()), NOW).unwrap()
    else {
        panic!("expected review")
    };
    assert_eq!(store.load().unwrap().id, original.id);
    drop(control);
    let control = RefreshControl::new(&temp.0).unwrap();
    assert_eq!(
        control
            .inspect_pending(NOW)
            .unwrap()
            .unwrap()
            .candidate_digest,
        review.candidate_digest
    );
    let mut approval = ReviewApproval {
        active_digest: review.active_digest,
        candidate_digest: review.candidate_digest,
    };
    approval.active_digest = "0".repeat(64);
    assert!(matches!(
        control.approve(&approval, NOW),
        Err(Error::ApprovalMismatch)
    ));
    approval.active_digest = original.id;
    let published = control.approve(&approval, NOW).unwrap();
    assert_eq!(published.id, approval.candidate_digest);
    assert!(control.inspect_pending(NOW).unwrap().is_none());
    assert_eq!(store.load().unwrap().id, published.id);
}
#[test]
fn modified_pending_bytes_and_stale_active_cannot_be_approved() {
    for tamper in [true, false] {
        let temp = Temp::new();
        let store = Store::new(&temp.0).unwrap();
        store.publish_bytes(&bytes(&data())).unwrap();
        let control = RefreshControl::new(&temp.0).unwrap();
        let Outcome::ReviewRequired { review } = control.submit(&bytes(&changed()), NOW).unwrap()
        else {
            panic!("expected review")
        };
        let approval = ReviewApproval {
            active_digest: review.active_digest,
            candidate_digest: review.candidate_digest,
        };
        if tamper {
            fs::write(
                temp.0.join(format!(
                    "refresh-candidate-{}.json",
                    approval.candidate_digest
                )),
                b"{}",
            )
            .unwrap();
        } else {
            let mut other = data();
            other["sources"][0]["validated_at_ms"] = json!(1789171240000u64);
            store.publish_bytes(&bytes(&other)).unwrap();
        }
        let before = store.load().unwrap().id;
        assert!(control.approve(&approval, NOW).is_err());
        assert_eq!(store.load().unwrap().id, before);
    }
}
#[test]
fn invalid_refresh_does_not_destroy_valid_pending_review() {
    let temp = Temp::new();
    let store = Store::new(&temp.0).unwrap();
    store.publish_bytes(&bytes(&data())).unwrap();
    let control = RefreshControl::new(&temp.0).unwrap();
    let Outcome::ReviewRequired { review } = control.submit(&bytes(&changed()), NOW).unwrap()
    else {
        panic!("expected review")
    };
    assert!(matches!(
        control.submit(b"invalid", NOW).unwrap(),
        Outcome::Rejected { .. }
    ));
    assert_eq!(
        control
            .inspect_pending(NOW)
            .unwrap()
            .unwrap()
            .candidate_digest,
        review.candidate_digest
    );
}
#[test]
fn same_snapshot_is_noop_and_unchanged_content_can_freshen_from_evidence() {
    let temp = Temp::new();
    let store = Store::new(&temp.0).unwrap();
    store.publish_bytes(&bytes(&data())).unwrap();
    let control = RefreshControl::new(&temp.0).unwrap();
    assert!(matches!(
        control.submit(&bytes(&data()), NOW).unwrap(),
        Outcome::Unchanged { .. }
    ));
    let mut refreshed = data();
    refreshed["sources"][0]["validated_at_ms"] = json!(1789171240000u64);
    assert!(matches!(
        control.submit(&bytes(&refreshed), NOW).unwrap(),
        Outcome::Published { .. }
    ));
    assert_eq!(
        store.load().unwrap().data.sources[0].validated_at_ms,
        1789171240000
    );
}
#[test]
fn writer_contention_and_symlinks_fail_without_publication() {
    use std::os::unix::fs::symlink;
    let temp = Temp::new();
    let store = Store::new(&temp.0).unwrap();
    let first = store.publish_bytes(&bytes(&data())).unwrap();
    let control = RefreshControl::new(&temp.0).unwrap();
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(temp.0.join("refresh.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    assert!(matches!(
        control.submit(&bytes(&changed()), NOW),
        Err(Error::Busy)
    ));
    drop(lock);
    symlink("active", temp.0.join("refresh-pending.json")).unwrap();
    assert!(control.submit(&bytes(&changed()), NOW).is_err());
    assert_eq!(store.load().unwrap().id, first.id);
}
