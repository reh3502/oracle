//! Local operator CLI for publishing a validated catalog and running offline queries.
use dandys_world_core::{
    query::{QueryEngine, QueryRequest},
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
    let command = args.next().ok_or("usage: dw-query publish --catalog FILE --store DIR | query --store DIR --input JSON [--now-ms NUMBER]")?;
    let mut options = BTreeMap::new();
    while let Some(key) = args.next() {
        let value = args.next().ok_or("option value missing")?;
        if options.insert(key, value).is_some() {
            return Err("duplicate option".into());
        }
    }
    let store_path = options.remove("--store").ok_or("--store is required")?;
    match command.as_str() {
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
        _ => return Err("unknown command; expected publish or query".into()),
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("dw-query: {error}");
        std::process::exit(2);
    }
}
