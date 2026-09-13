//! Local operator CLI for publishing a validated catalog and running offline queries.
use dandys_world_core::{
    query::{QueryEngine, QueryRequest},
    refresh_control::RefreshControl,
    refresh_review::ReviewApproval,
    snapshot::{MAX_SNAPSHOT_BYTES, Store},
};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err("file exceeds size limit".into());
    }
    Ok(bytes)
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or("usage: dw-query <publish|query|stage|review|approve|discard|backup|restore|rollback|recover> --store DIR [options]")?;
    let mut options = BTreeMap::new();
    while let Some(key) = args.next() {
        let value = args.next().ok_or("option value missing")?;
        if options.insert(key, value).is_some() {
            return Err("duplicate option".into());
        }
    }
    let store_path = options.remove("--store").ok_or("--store is required")?;
    match command.as_str() {
        "stage" => {
            let path = options.remove("--catalog").ok_or("--catalog is required")?;
            if !options.is_empty() {
                return Err("unknown stage option".into());
            }
            let bytes = read_bounded(Path::new(&path), MAX_SNAPSHOT_BYTES)?;
            let outcome = RefreshControl::new(store_path)?.submit(&bytes, now_ms()?)?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
        }
        "review" => {
            if !options.is_empty() {
                return Err("unknown review option".into());
            }
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &RefreshControl::new(store_path)?.inspect_pending(now_ms()?)?
                )?
            );
        }
        "approve" | "discard" => {
            let approval = ReviewApproval {
                active_digest: options
                    .remove("--active")
                    .ok_or("--active digest is required")?,
                candidate_digest: options
                    .remove("--candidate")
                    .ok_or("--candidate digest is required")?,
            };
            if !options.is_empty() {
                return Err("unknown review decision option".into());
            }
            let control = RefreshControl::new(store_path)?;
            if command == "approve" {
                let snapshot = control.approve(&approval, now_ms()?)?;
                println!("{}", serde_json::json!({"published":snapshot.id}));
            } else {
                control.discard(&approval)?;
                println!(
                    "{}",
                    serde_json::json!({"discarded":approval.candidate_digest})
                );
            }
        }
        "backup" => {
            let output = options
                .remove("--output")
                .ok_or("--output directory is required")?;
            if !options.is_empty() {
                return Err("unknown backup option".into());
            }
            Store::new(store_path)?.backup_to(output)?;
            println!("{}", serde_json::json!({"backed_up":true}));
        }
        "restore" => {
            let backup = options
                .remove("--backup")
                .ok_or("--backup directory is required")?;
            if !options.is_empty() {
                return Err("unknown restore option".into());
            }
            let snapshot = Store::new(store_path)?.restore_from(backup)?;
            println!("{}", serde_json::json!({"restored":snapshot.id}));
        }
        "rollback" | "recover" => {
            if !options.is_empty() {
                return Err("unknown recovery option".into());
            }
            let store = Store::new(store_path)?;
            if command == "rollback" {
                let snapshot = store.rollback()?;
                println!("{}", serde_json::json!({"rolled_back":snapshot.id}));
            } else {
                let loaded = store.load_recovering()?;
                println!(
                    "{}",
                    serde_json::json!({"snapshot_id":loaded.snapshot.id,"recovered":loaded.recovered})
                );
            }
        }
        "publish" => {
            let path = options.remove("--catalog").ok_or("--catalog is required")?;
            if !options.is_empty() {
                return Err("unknown publish option".into());
            }
            let bytes = read_bounded(Path::new(&path), MAX_SNAPSHOT_BYTES)?;
            let snapshot = Store::new(store_path)?.publish_bytes(&bytes)?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"snapshot_id":snapshot.id,"entities":snapshot.data.entities.len(),"coverage":snapshot.data.coverage})
                )?
            );
        }
        "query" => {
            let input = options
                .remove("--input")
                .ok_or("--input JSON is required")?;
            if input.len() > 32 * 1024 {
                return Err("query envelope too large".into());
            }
            let now_ms = match options.remove("--now-ms") {
                Some(value) => value.parse::<u64>()?,
                None => u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?,
            };
            if !options.is_empty() {
                return Err("unknown query option".into());
            }
            let request: QueryRequest = serde_json::from_str(&input)?;
            let snapshot = Store::new(store_path)?.load()?;
            let engine = QueryEngine::new(snapshot.id, snapshot.data);
            println!(
                "{}",
                serde_json::to_string_pretty(&engine.execute(request, now_ms)?)?
            );
        }
        _ => return Err("unknown dw-query command".into()),
    }
    Ok(())
}
fn now_ms() -> Result<u64, Box<dyn std::error::Error>> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}
fn main() {
    if let Err(error) = run() {
        eprintln!("dw-query: {error}");
        std::process::exit(2);
    }
}
