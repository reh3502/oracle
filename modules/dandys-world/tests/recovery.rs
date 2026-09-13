//! Real filesystem and process-crash qualification of snapshot history and recovery.
use dandys_world_core::snapshot::{
    Error, MAX_SNAPSHOT_BYTES, MAX_STORE_BYTES, PublishStep, Snapshot, Store,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
// Forking a crash helper can briefly inherit another test thread's flock until
// exec closes CLOEXEC descriptors. Serialize these disk drills, while each drill
// still exercises its explicit competing writer and reader scenarios.
static DISK_DRILLS: std::sync::Mutex<()> = std::sync::Mutex::new(());
struct Temp {
    path: PathBuf,
    _guard: std::sync::MutexGuard<'static, ()>,
}
impl Temp {
    fn new() -> Self {
        let guard = DISK_DRILLS.lock().unwrap_or_else(|p| p.into_inner());
        let path = std::env::temp_dir().join(format!(
            "dw-recovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self {
            path,
            _guard: guard,
        }
    }
    fn store(&self) -> Store {
        Store::new(self.path.join("store")).unwrap()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
fn candidate(name: &str, checked: u64) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema_version":1,"adapter_version":"synthetic-recovery-fixture","source_origin":"https://dandys-world-robloxhorror.fandom.com","crawl_started_at":"2026-09-12T00:00:00Z","crawl_completed_at":"2026-09-12T00:01:00Z",
        "sources":[{"id":"page:1","page_id":1,"title":"Fixture Toon","url":"https://dandys-world-robloxhorror.fandom.com/wiki/Fixture_Toon","revision_id":2,"revision_timestamp":"2026-09-12T00:00:00Z","validated_at_ms":checked,"content_sha256":"a".repeat(64),"license":"CC BY-SA 3.0","license_url":"https://creativecommons.org/licenses/by-sa/3.0/"}],
        "entities":[{"id":"toon:fixture","kind":"toon","name":name,"aliases":[],"availability":"supported","warnings":[],"facts":[{"id":"fixture.speed","key":"speed","text":"Synthetic base speed","value":20,"unit":"speed","conditions":["base"],"state":"supported","citations":[{"source_id":"page:1","section":"Synthetic stats","quote":"Synthetic speed 20"}]}],"relationships":[]}],
        "coverage":{"discovered_pages":1,"imported_pages":1,"namespace_counts":{"articles":1},"nonredirect_articles":1,"redirects":0,"entities_by_kind":{"toon":1},"excluded":[],"unresolved_redirects":[],"warnings":[]}
    })).unwrap()
}
fn catalog_count(path: &std::path::Path) -> usize {
    fs::read_dir(path)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|s| s.len() == 69 && s.ends_with(".json"))
        })
        .count()
}
#[test]
fn legacy_pointer_upgrades_without_changing_catalog_or_source_times() {
    let root = Temp::new();
    let store = root.store();
    let bytes = candidate("Legacy", 100);
    let old = Snapshot::from_bytes(&bytes).unwrap();
    fs::write(
        root.path.join("store").join(format!("{}.json", old.id)),
        &bytes,
    )
    .unwrap();
    fs::write(root.path.join("store/active"), &old.id).unwrap();
    assert_eq!(store.load().unwrap().id, old.id);
    assert!(!store.load_recovering().unwrap().recovered);
    let new = store.publish_bytes(&candidate("New", 200)).unwrap();
    let pointer: Value =
        serde_json::from_slice(&fs::read(root.path.join("store/active")).unwrap()).unwrap();
    assert_eq!(pointer["store_format_version"], 1);
    assert_eq!(pointer["current"], new.id);
    assert_eq!(pointer["previous"], old.id);
    let rolled = store.rollback().unwrap();
    assert_eq!(rolled.id, old.id);
    assert_eq!(rolled.data.sources[0].validated_at_ms, 100);
}
#[test]
fn retention_keeps_two_snapshots_and_arc_readers_outlive_pruning() {
    let root = Temp::new();
    let store = root.store();
    let first = store.publish_bytes(&candidate("First", 100)).unwrap();
    store.publish_bytes(&candidate("Second", 200)).unwrap();
    let third = store.publish_bytes(&candidate("Third", 300)).unwrap();
    assert!(
        !root
            .path
            .join("store")
            .join(format!("{}.json", first.id))
            .exists()
    );
    assert_eq!(first.data.entities[0].name, "First");
    assert_eq!(first.data.sources[0].validated_at_ms, 100);
    assert_eq!(store.load().unwrap().id, third.id);
    fs::write(
        root.path.join("store/operator-note.txt"),
        "Keep this unrelated file",
    )
    .unwrap();
    fs::write(
        root.path.join("store/.candidate-999-1"),
        "abandoned partial bytes",
    )
    .unwrap();
    let report = store.cleanup().unwrap();
    assert_eq!(report.retained_snapshots, 2);
    assert_eq!(report.removed_files, 1);
    assert!(report.total_bytes < MAX_STORE_BYTES);
    assert_eq!(catalog_count(&root.path.join("store")), 2);
    assert!(root.path.join("store/operator-note.txt").exists());
}
#[test]
fn corruption_recovers_recorded_previous_and_never_freshens_it() {
    let root = Temp::new();
    let store = root.store();
    let old = store.publish_bytes(&candidate("Old", 100)).unwrap();
    let current = store.publish_bytes(&candidate("Current", 200)).unwrap();
    fs::write(
        root.path.join("store").join(format!("{}.json", current.id)),
        b"broken JSON",
    )
    .unwrap();
    assert!(store.load().is_err());
    let loaded = store.load_recovering().unwrap();
    assert!(loaded.recovered);
    assert_eq!(loaded.snapshot.id, old.id);
    assert_eq!(loaded.snapshot.data.sources[0].validated_at_ms, 100);
    assert!(!store.load_recovering().unwrap().recovered);
    assert!(store.rollback().is_err());
    assert_eq!(catalog_count(&root.path.join("store")), 1);
}
#[test]
fn damaged_pointer_uses_mirror_but_two_bad_catalogs_remain_unavailable() {
    let root = Temp::new();
    let store = root.store();
    let old = store.publish_bytes(&candidate("Old", 100)).unwrap();
    store.publish_bytes(&candidate("Current", 200)).unwrap();
    fs::write(root.path.join("store/active"), "truncated head").unwrap();
    let loaded = store.load_recovering().unwrap();
    assert!(loaded.recovered);
    assert_eq!(loaded.snapshot.id, old.id);
    let current = store.publish_bytes(&candidate("Current", 200)).unwrap();
    for id in [&old.id, &current.id] {
        fs::write(
            root.path.join("store").join(format!("{id}.json")),
            "corrupt",
        )
        .unwrap();
    }
    let pointer = fs::read(root.path.join("store/active")).unwrap();
    assert!(store.load_recovering().is_err());
    assert_eq!(fs::read(root.path.join("store/active")).unwrap(), pointer);
}
#[test]
fn healthy_current_drops_corrupt_previous_without_claiming_fallback() {
    let root = Temp::new();
    let store = root.store();
    let old = store.publish_bytes(&candidate("Old", 100)).unwrap();
    let current = store.publish_bytes(&candidate("Current", 200)).unwrap();
    fs::write(
        root.path.join("store").join(format!("{}.json", old.id)),
        "bad",
    )
    .unwrap();
    assert_eq!(store.load().unwrap().id, current.id);
    let loaded = store.load_recovering().unwrap();
    assert!(!loaded.recovered);
    assert_eq!(loaded.snapshot.id, current.id);
    assert!(store.rollback().is_err());
}
#[test]
fn rollback_swaps_history_and_compare_publish_rejects_review_races() {
    let root = Temp::new();
    let store = root.store();
    let first = store.publish_bytes(&candidate("First", 100)).unwrap();
    let second = store
        .publish_if_current(&candidate("Second", 200), &first.id)
        .unwrap();
    let stale = store.publish_if_current(&candidate("Third", 300), &first.id);
    assert!(
        matches!(stale, Err(Error::Conflict)),
        "stale publication: {stale:?}"
    );
    assert_eq!(store.load().unwrap().id, second.id);
    assert_eq!(store.rollback().unwrap().id, first.id);
    assert_eq!(store.rollback().unwrap().id, second.id);
    let third = Snapshot::from_bytes(&candidate("Third", 300)).unwrap();
    assert!(
        !root
            .path
            .join("store")
            .join(format!("{}.json", third.id))
            .exists()
    );
    let other = Store::new(root.path.join("store")).unwrap();
    let fourth = store
        .publish_bytes_observed(&candidate("Fourth", 400), |phase| {
            if phase == PublishStep::CatalogDurable {
                assert!(matches!(
                    other.publish_if_current(&candidate("Third", 300), &second.id),
                    Err(Error::Busy)
                ));
            }
        })
        .unwrap();
    assert!(matches!(
        other.publish_if_current(&candidate("Third", 300), &second.id),
        Err(Error::Conflict)
    ));
    assert_eq!(store.load().unwrap().id, fourth.id);
}
#[test]
fn backup_restore_is_complete_checked_and_preserves_both_catalogs() {
    let root = Temp::new();
    let store = root.store();
    let old = store.publish_bytes(&candidate("Old", 100)).unwrap();
    let current = store.publish_bytes(&candidate("Current", 200)).unwrap();
    let backup = root.path.join("backup");
    store.backup_to(&backup).unwrap();
    assert_eq!(fs::read_dir(&backup).unwrap().count(), 3);
    assert!(store.backup_to(&backup).is_err());
    let restored = Store::new(root.path.join("restored")).unwrap();
    let value = restored.restore_from(&backup).unwrap();
    assert_eq!(value.id, current.id);
    assert_eq!(value.data.sources[0].validated_at_ms, 200);
    let previous = restored.rollback().unwrap();
    assert_eq!(previous.id, old.id);
    assert_eq!(previous.data.sources[0].validated_at_ms, 100);
    // A healthy existing destination can atomically adopt both saved heads too.
    restored.publish_bytes(&candidate("Other", 300)).unwrap();
    assert_eq!(restored.restore_from(&backup).unwrap().id, current.id);
    assert_eq!(restored.rollback().unwrap().id, old.id);
}
#[test]
fn invalid_backups_never_change_active_or_partially_restore() {
    let root = Temp::new();
    let source = root.store();
    let old = source.publish_bytes(&candidate("Old", 100)).unwrap();
    let current = source.publish_bytes(&candidate("Current", 200)).unwrap();
    let backup = root.path.join("backup");
    source.backup_to(&backup).unwrap();
    let target = Store::new(root.path.join("target")).unwrap();
    let before = target
        .publish_bytes(&candidate("Destination", 300))
        .unwrap();
    let original = fs::read(backup.join(format!("{}.json", old.id))).unwrap();
    fs::write(backup.join(format!("{}.json", old.id)), "corrupt previous").unwrap();
    assert!(target.restore_from(&backup).is_err());
    assert_eq!(target.load().unwrap().id, before.id);
    assert!(
        !root
            .path
            .join("target")
            .join(format!("{}.json", current.id))
            .exists()
    );
    fs::write(backup.join(format!("{}.json", old.id)), original).unwrap();
    let original_manifest = fs::read(backup.join("manifest.json")).unwrap();
    for manifest in [
        json!({"store_format_version":1,"current":"../private","previous":null}),
        json!({"store_format_version":2,"current":current.id,"previous":old.id}),
        json!({"store_format_version":1,"current":current.id,"previous":old.id,"extra":true}),
    ] {
        fs::write(
            backup.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(target.restore_from(&backup).is_err());
        assert_eq!(target.load().unwrap().id, before.id);
    }
    fs::write(backup.join("manifest.json"), original_manifest).unwrap();
    fs::remove_file(backup.join(format!("{}.json", old.id))).unwrap();
    assert!(target.restore_from(&backup).is_err());
    assert_eq!(target.load().unwrap().id, before.id);
}
#[cfg(unix)]
#[test]
fn symlinks_traversal_and_overlapping_backups_are_rejected_without_side_effects() {
    use std::os::unix::fs::symlink;
    let root = Temp::new();
    let store = root.store();
    let current = store.publish_bytes(&candidate("Current", 200)).unwrap();
    assert!(store.backup_to(root.path.join("store/backup")).is_err());
    assert!(store.backup_to(&root.path).is_err());
    assert!(store.backup_to(root.path.join("store/../backup")).is_err());
    let external = root.path.join("external");
    fs::create_dir(&external).unwrap();
    symlink(&external, root.path.join("alias")).unwrap();
    assert!(Store::new(root.path.join("alias/new-store")).is_err());
    assert!(!external.join("new-store").exists());
    let backup = root.path.join("backup");
    store.backup_to(&backup).unwrap();
    let file = backup.join(format!("{}.json", current.id));
    let external_file = external.join("valid.json");
    fs::rename(&file, &external_file).unwrap();
    symlink(&external_file, &file).unwrap();
    let target = Store::new(root.path.join("target")).unwrap();
    assert!(target.restore_from(&backup).is_err());
    assert!(!root.path.join("target/active").exists());
    assert!(external_file.is_file());
    fs::remove_file(root.path.join("store/active")).unwrap();
    symlink(&external_file, root.path.join("store/active")).unwrap();
    assert!(store.load_recovering().is_err());
    assert_eq!(fs::read(&external_file).unwrap(), candidate("Current", 200));
}
#[test]
fn every_mutation_uses_the_same_writer_lock() {
    let root = Temp::new();
    let store = root.store();
    let first = store.publish_bytes(&candidate("First", 100)).unwrap();
    store.publish_bytes(&candidate("Second", 200)).unwrap();
    let backup = root.path.join("backup");
    store.backup_to(&backup).unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path.join("store/writer.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    assert!(matches!(
        store.publish_bytes(&candidate("Third", 300)),
        Err(Error::Busy)
    ));
    assert!(matches!(
        store.publish_if_current(&candidate("Third", 300), &first.id),
        Err(Error::Busy)
    ));
    assert!(matches!(store.rollback(), Err(Error::Busy)));
    assert!(matches!(store.load_recovering(), Err(Error::Busy)));
    assert!(matches!(store.cleanup(), Err(Error::Busy)));
    assert!(matches!(
        store.backup_to(root.path.join("other-backup")),
        Err(Error::Busy)
    ));
    assert!(matches!(store.restore_from(&backup), Err(Error::Busy)));
    assert!(store.load().is_ok());
    drop(lock);
    assert!(store.load_recovering().is_ok());
}
#[test]
fn raw_staging_counts_toward_quota_and_cleanup_preserves_unrelated_files() {
    let root = Temp::new();
    let store = root.store();
    let current = store.publish_bytes(&candidate("Current", 200)).unwrap();
    let raw = root.path.join("store/raw-staging");
    fs::create_dir(&raw).unwrap();
    let file = fs::File::create(raw.join("source-export.bin")).unwrap();
    file.set_len(MAX_STORE_BYTES).unwrap();
    assert!(store.publish_bytes(&candidate("Next", 300)).is_err());
    assert_eq!(store.load().unwrap().id, current.id);
    assert_eq!(file.metadata().unwrap().len(), MAX_STORE_BYTES);
    file.set_len(200 * 1024 * 1024).unwrap();
    fs::hard_link(raw.join("source-export.bin"), raw.join("second-name.bin")).unwrap();
    let report = store.cleanup().unwrap();
    assert!(report.total_bytes >= 200 * 1024 * 1024 && report.total_bytes < 201 * 1024 * 1024);
    assert!(raw.join("second-name.bin").exists());
    let orphan = fs::File::create(root.path.join("store/.candidate-1000-1")).unwrap();
    orphan.set_len(MAX_STORE_BYTES).unwrap();
    let report = store.cleanup().unwrap();
    assert_eq!(report.removed_files, 1);
    assert!(report.total_bytes < MAX_STORE_BYTES);
}
#[test]
fn oversized_catalog_and_backup_are_rejected_before_reading_entire_file() {
    let root = Temp::new();
    let store = root.store();
    let old = store.publish_bytes(&candidate("Old", 100)).unwrap();
    let current = store.publish_bytes(&candidate("Current", 200)).unwrap();
    let backup = root.path.join("backup");
    store.backup_to(&backup).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(backup.join(format!("{}.json", current.id)))
        .unwrap()
        .set_len(MAX_SNAPSHOT_BYTES as u64 + 1)
        .unwrap();
    let restored = Store::new(root.path.join("restored")).unwrap();
    assert!(restored.restore_from(&backup).is_err());
    assert!(!root.path.join("restored/active").exists());
    fs::OpenOptions::new()
        .write(true)
        .open(root.path.join("store").join(format!("{}.json", current.id)))
        .unwrap()
        .set_len(MAX_SNAPSHOT_BYTES as u64 + 1)
        .unwrap();
    let recovered = store.load_recovering().unwrap();
    assert!(recovered.recovered);
    assert_eq!(recovered.snapshot.id, old.id);
}
#[test]
#[ignore = "child helper; process-crash tests invoke this explicitly"]
fn crash_child() {
    let root = PathBuf::from(std::env::var_os("DW_CRASH_STORE").unwrap());
    let wanted = std::env::var("DW_CRASH_PHASE").unwrap();
    let store = Store::new(root).unwrap();
    store
        .publish_bytes_observed(&candidate("Crash candidate", 300), |phase| {
            if format!("{phase:?}") == wanted {
                std::process::exit(73);
            }
        })
        .unwrap();
    panic!("crash boundary was not reached");
}
fn crash(store: &std::path::Path, phase: PublishStep) {
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_child", "--ignored"])
        .env("DW_CRASH_STORE", store)
        .env("DW_CRASH_PHASE", format!("{phase:?}"))
        .output()
        .unwrap();
    assert_eq!(
        status.status.code(),
        Some(73),
        "child failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}
#[test]
fn process_crashes_at_every_publication_boundary_select_only_complete_committed_heads() {
    for phase in [
        PublishStep::CatalogDurable,
        PublishStep::PreviousDurable,
        PublishStep::ActiveCommitted,
        PublishStep::CleanupComplete,
    ] {
        let root = Temp::new();
        let store = root.store();
        let first = store.publish_bytes(&candidate("First", 100)).unwrap();
        let second = store.publish_bytes(&candidate("Second", 200)).unwrap();
        let new = Snapshot::from_bytes(&candidate("Crash candidate", 300)).unwrap();
        crash(&root.path.join("store"), phase);
        let committed = matches!(
            phase,
            PublishStep::ActiveCommitted | PublishStep::CleanupComplete
        );
        let expected = if committed { &new.id } else { &second.id };
        assert_eq!(&store.load().unwrap().id, expected, "{phase:?}");
        let restarted = store.load_recovering().unwrap();
        assert!(!restarted.recovered);
        assert_eq!(&restarted.snapshot.id, expected);
        assert_eq!(store.cleanup().unwrap().retained_snapshots, 2);
        assert_eq!(catalog_count(&root.path.join("store")), 2);
        assert_eq!(
            store.rollback().unwrap().id,
            if committed { second.id } else { first.id }
        );
    }
}
#[test]
fn interrupted_first_candidate_is_never_adopted_as_active() {
    for phase in [PublishStep::CatalogDurable, PublishStep::PreviousDurable] {
        let root = Temp::new();
        let store = root.store();
        crash(&root.path.join("store"), phase);
        assert!(store.load().is_err());
        assert!(store.load_recovering().is_err());
        assert!(!root.path.join("store/active").exists());
        let approved = store.publish_bytes(&candidate("Approved", 400)).unwrap();
        assert_eq!(store.load().unwrap().id, approved.id);
        assert_eq!(catalog_count(&root.path.join("store")), 1);
    }
}
#[test]
fn self_consistent_backup_with_incompatible_catalog_schema_is_rejected() {
    let root = Temp::new();
    let backup = root.path.join("backup");
    fs::create_dir(&backup).unwrap();
    let mut data: Value = serde_json::from_slice(&candidate("Future", 100)).unwrap();
    data["schema_version"] = json!(999);
    let bytes = serde_json::to_vec(&data).unwrap();
    let id = format!("{:x}", Sha256::digest(&bytes));
    fs::write(backup.join(format!("{id}.json")), bytes).unwrap();
    fs::write(
        backup.join("manifest.json"),
        serde_json::to_vec(&json!({"store_format_version":1,"current":id,"previous":null}))
            .unwrap(),
    )
    .unwrap();
    let store = root.store();
    assert!(store.restore_from(&backup).is_err());
    assert!(!root.path.join("store/active").exists());
}
#[test]
fn refresh_files_are_atomic_bounded_and_cannot_address_store_authority() {
    let root = Temp::new();
    let store = root.store();
    let current = store.publish_bytes(&candidate("Current", 200)).unwrap();
    for name in [
        "active",
        "previous",
        "writer.lock",
        "../refresh-pending.json",
        "refresh-../active",
        "refresh-/active",
        "refresh-",
        "refresh-a\\b",
        "refresh-a\nb",
    ] {
        assert!(store.write_refresh_file(name, b"forged").is_err(), "{name}");
        assert_eq!(store.load().unwrap().id, current.id);
    }
    store
        .write_refresh_file("refresh-pending.json", b"first")
        .unwrap();
    store
        .write_refresh_file("refresh-pending.json", b"second")
        .unwrap();
    assert_eq!(
        fs::read(root.path.join("store/refresh-pending.json")).unwrap(),
        b"second"
    );
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path.join("store/writer.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    assert!(matches!(
        store.write_refresh_file("refresh-pending.json", b"third"),
        Err(Error::Busy)
    ));
    drop(lock);
    let raw = fs::File::create(root.path.join("store/raw-export")).unwrap();
    raw.set_len(MAX_STORE_BYTES).unwrap();
    assert!(
        store
            .write_refresh_file("refresh-pending.json", b"over budget")
            .is_err()
    );
    assert_eq!(
        fs::read(root.path.join("store/refresh-pending.json")).unwrap(),
        b"second"
    );
}
#[cfg(unix)]
#[test]
fn refresh_file_symlink_is_never_followed_or_replaced() {
    let root = Temp::new();
    let store = root.store();
    let private = root.path.join("private");
    fs::write(&private, "unchanged").unwrap();
    std::os::unix::fs::symlink(&private, root.path.join("store/refresh-candidate.json")).unwrap();
    assert!(
        store
            .write_refresh_file("refresh-candidate.json", b"replaced")
            .is_err()
    );
    assert_eq!(fs::read_to_string(private).unwrap(), "unchanged");
}
#[test]
fn recovery_discards_abandoned_writes_before_metadata_quota_reservation() {
    let root = Temp::new();
    let store = root.store();
    let previous = store.publish_bytes(&candidate("Previous", 100)).unwrap();
    let active = store.publish_bytes(&candidate("Active", 200)).unwrap();
    let orphan = root.path.join("store/.candidate-12345-1");
    fs::File::create(&orphan)
        .unwrap()
        .set_len(MAX_STORE_BYTES)
        .unwrap();
    let loaded = store.load_recovering().unwrap();
    assert!(!loaded.recovered);
    assert_eq!(loaded.snapshot.id, active.id);
    assert!(!orphan.exists());
    fs::write(
        root.path.join("store").join(format!("{}.json", active.id)),
        "corrupt",
    )
    .unwrap();
    fs::File::create(&orphan)
        .unwrap()
        .set_len(MAX_STORE_BYTES)
        .unwrap();
    let loaded = store.load_recovering().unwrap();
    assert!(loaded.recovered);
    assert_eq!(loaded.snapshot.id, previous.id);
    assert!(!orphan.exists());
}
