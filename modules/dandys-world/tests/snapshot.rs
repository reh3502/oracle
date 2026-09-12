use dandys_world_core::{
    model::CatalogData,
    snapshot::{Snapshot, Store, validate},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "dw-snapshot-{}-{}",
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
fn candidate() -> Value {
    json!({
        "schema_version":1,"adapter_version":"test-1","source_origin":"https://dandys-world-robloxhorror.fandom.com","crawl_started_at":"2026-09-12T00:00:00Z","crawl_completed_at":"2026-09-12T00:01:00Z",
        "sources":[{"id":"page:1","page_id":1,"title":"Pebble","url":"https://dandys-world-robloxhorror.fandom.com/wiki/Pebble","revision_id":2,"revision_timestamp":"2026-09-12T00:00:00Z","validated_at_ms":1,"content_sha256":"a".repeat(64),"license":"CC BY-SA 3.0","license_url":"https://creativecommons.org/licenses/by-sa/3.0/"}],
        "entities":[{"id":"toon:pebble","kind":"toon","name":"Pebble","aliases":["Rock"],"availability":"supported","warnings":[],"facts":[{"id":"pebble.speed","key":"speed","text":"Base speed","value":20,"unit":"speed","conditions":["base"],"state":"supported","citations":[{"source_id":"page:1","section":"Stats","quote":"Speed 20"}]}],"relationships":[]}],
        "coverage":{"discovered_pages":1,"imported_pages":1,"namespace_counts":{"articles":1},"nonredirect_articles":1,"redirects":0,"entities_by_kind":{"toon":1},"excluded":[],"unresolved_redirects":[],"warnings":[]}
    })
}
fn bytes(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap()
}
#[test]
fn rejects_malformed_and_unsupported_schema() {
    assert!(Snapshot::from_bytes(b"not JSON").is_err());
    let mut value = candidate();
    value["schema_version"] = json!(2);
    assert!(Snapshot::from_bytes(&bytes(&value)).is_err());
    let mut value = candidate();
    value["surprise"] = json!(true);
    assert!(Snapshot::from_bytes(&bytes(&value)).is_err());
}
#[test]
fn rejects_duplicate_identities_or_bad_provenance() {
    for pointer in [
        "/sources/0/content_sha256",
        "/sources/0/id",
        "/sources/0/title",
    ] {
        let mut v = candidate();
        *v.pointer_mut(pointer).unwrap() = json!("");
        assert!(Snapshot::from_bytes(&bytes(&v)).is_err(), "{pointer}");
    }
    for key in ["sources", "entities"] {
        let mut v = candidate();
        let duplicate = v[key][0].clone();
        v[key].as_array_mut().unwrap().push(duplicate);
        assert!(Snapshot::from_bytes(&bytes(&v)).is_err());
    }
    let mut v = candidate();
    let duplicate = v["entities"][0]["facts"][0].clone();
    v["entities"][0]["facts"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    assert!(Snapshot::from_bytes(&bytes(&v)).is_err());
}
#[test]
fn rejects_spoofed_origins_and_uncited_claims() {
    for url in [
        "https://dandys-world-robloxhorror.fandom.com.evil/wiki/Pebble",
        "https://dandys-world-robloxhorror.fandom.com@evil/wiki/Pebble",
        "https://dandys-world-robloxhorror.fandom.com\\@evil/wiki/Pebble",
        "https://evil/wiki/Pebble",
    ] {
        let mut v = candidate();
        v["sources"][0]["url"] = json!(url);
        assert!(Snapshot::from_bytes(&bytes(&v)).is_err());
    }
    let mut v = candidate();
    v["entities"][0]["facts"][0]["citations"][0]["source_id"] = json!("missing");
    assert!(Snapshot::from_bytes(&bytes(&v)).is_err());
    let mut v = candidate();
    v["entities"][0]["facts"][0]["citations"] = json!([]);
    assert!(Snapshot::from_bytes(&bytes(&v)).is_err());
    let mut v = candidate();
    v["entities"][0]["relationships"] = json!([{"relation":"counterpart","target_id":"missing","citations":v["entities"][0]["facts"][0]["citations"]}]);
    assert!(Snapshot::from_bytes(&bytes(&v)).is_err());
}
#[test]
fn rejects_incomplete_coverage_and_absent_supported_value() {
    for pointer in [
        "/coverage/discovered_pages",
        "/coverage/imported_pages",
        "/coverage/namespace_counts/articles",
        "/coverage/entities_by_kind/toon",
        "/coverage/redirects",
    ] {
        let mut v = candidate();
        *v.pointer_mut(pointer).unwrap() = json!(5);
        assert!(Snapshot::from_bytes(&bytes(&v)).is_err(), "{pointer}");
    }
    let mut v = candidate();
    v["entities"][0]["facts"][0]["value"] = Value::Null;
    assert!(Snapshot::from_bytes(&bytes(&v)).is_err());
    v["entities"][0]["facts"][0]["state"] = json!("unknown");
    let data: CatalogData = serde_json::from_value(v).unwrap();
    validate(&data).unwrap();
}
#[test]
fn publish_restart_and_old_readers_pin_complete_snapshot() {
    let root = Temp::new();
    let store = Store::new(&root.0).unwrap();
    let first = store.publish_bytes(&bytes(&candidate())).unwrap();
    assert_eq!(Store::new(&root.0).unwrap().load().unwrap().id, first.id);
    let mut next = candidate();
    next["entities"][0]["name"] = json!("New name");
    let second = store.publish_bytes(&bytes(&next)).unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(first.data.entities[0].name, "Pebble");
    assert_eq!(store.load().unwrap().data.entities[0].name, "New name");
    assert!(root.0.join(format!("{}.json", first.id)).is_file());
    assert!(store.publish_bytes(b"{}").is_err());
    assert_eq!(store.load().unwrap().id, second.id);
    assert_eq!(store.publish_bytes(&bytes(&next)).unwrap().id, second.id);
}
#[test]
fn rejects_corruption_and_pointer_traversal() {
    let root = Temp::new();
    let store = Store::new(&root.0).unwrap();
    let first = store.publish_bytes(&bytes(&candidate())).unwrap();
    let mut changed = candidate();
    changed["entities"][0]["name"] = json!("Corrupted");
    fs::write(root.0.join(format!("{}.json", first.id)), bytes(&changed)).unwrap();
    assert!(store.load().is_err());
    assert!(store.publish_bytes(&bytes(&candidate())).is_err());
    fs::write(root.0.join("active"), "../private").unwrap();
    assert!(store.load().is_err());
}
#[test]
fn writer_lock_blocks_then_recovers_without_deleting_lock() {
    let root = Temp::new();
    let store = Store::new(&root.0).unwrap();
    let first = store.publish_bytes(&bytes(&candidate())).unwrap();
    let lock = fs::OpenOptions::new()
        .write(true)
        .open(root.0.join("writer.lock"))
        .unwrap();
    lock.lock().unwrap();
    assert!(store.publish_bytes(&bytes(&candidate())).is_err());
    assert_eq!(store.load().unwrap().id, first.id);
    drop(lock);
    store.publish_bytes(&bytes(&candidate())).unwrap();
}
#[cfg(unix)]
#[test]
fn refuses_symlink_store_entries_and_preserves_private_files() {
    use std::os::unix::fs::symlink;
    let root = Temp::new();
    let private = root.0.join("private");
    fs::write(&private, b"private contents").unwrap();
    let directory = root.0.join("store");
    let store = Store::new(&directory).unwrap();
    symlink(&private, directory.join("writer.lock")).unwrap();
    assert!(store.publish_bytes(&bytes(&candidate())).is_err());
    fs::remove_file(directory.join("writer.lock")).unwrap();
    let id = Snapshot::from_bytes(&bytes(&candidate())).unwrap().id;
    symlink(&private, directory.join(format!("{id}.json"))).unwrap();
    assert!(store.publish_bytes(&bytes(&candidate())).is_err());
    fs::remove_file(directory.join(format!("{id}.json"))).unwrap();
    symlink(&private, directory.join("active")).unwrap();
    assert!(store.publish_bytes(&bytes(&candidate())).is_err());
    assert!(store.load().is_err());
    symlink(&directory, root.0.join("linked")).unwrap();
    assert!(Store::new(root.0.join("linked")).is_err());
    assert_eq!(fs::read(&private).unwrap(), b"private contents");
}
#[test]
fn concurrent_readers_observe_only_complete_catalogs() {
    let root = Temp::new();
    let store = Store::new(&root.0).unwrap();
    store.publish_bytes(&bytes(&candidate())).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for n in 0..30 {
                let mut v = candidate();
                v["entities"][0]["name"] = json!(format!("Version {n}"));
                store.publish_bytes(&bytes(&v)).unwrap();
            }
        });
        for _ in 0..3 {
            scope.spawn(|| {
                for _ in 0..60 {
                    let snap = store.load().unwrap();
                    assert_eq!(snap.data.entities[0].facts[0].value, 20);
                }
            });
        }
    });
}
