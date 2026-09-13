//! One module-owned refresh loop. No guild callbacks or public file/URL inputs.
use dandys_world_core::{
    refresh_control::{Outcome, RefreshControl},
    refresh_job::{RefreshJobFailure, RefreshJobOutcome, RefreshJobSettings, run},
    refresh_policy::RefreshLimits,
    refresh_schedule::{AttemptResult, RefreshSchedule},
    snapshot::Store,
};
use oracle_module_sdk::{RpcError, TaskScope};
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub type Diagnostics = Arc<RwLock<String>>;
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
fn read(path: &Path) -> Result<Option<Vec<u8>>, ()> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if !file.metadata().map_err(|_| ())?.is_file() {
        return Err(());
    }
    let mut bytes = Vec::new();
    file.take(16_385).read_to_end(&mut bytes).map_err(|_| ())?;
    if bytes.len() > 16_384 {
        return Err(());
    }
    Ok(Some(bytes))
}
fn save(root: &Path, schedule: &RefreshSchedule) -> Result<(), ()> {
    let bytes = serde_json::to_vec(schedule).map_err(|_| ())?;
    Store::new(root)
        .and_then(|store| store.write_refresh_file("refresh-schedule.json", &bytes))
        .map_err(|_| ())
}
fn report(diagnostics: &Diagnostics, message: impl Into<String>) {
    *diagnostics.write().unwrap() = message.into();
}
pub fn start(root: PathBuf, global: &TaskScope, diagnostics: Diagnostics) -> Result<(), RpcError> {
    let bytes = match read(&root.join("refresh-settings.json")) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            report(&diagnostics, "Disabled: no refresh configuration");
            return Ok(());
        }
        Err(()) => {
            report(&diagnostics, "Disabled: invalid refresh configuration");
            return Ok(());
        }
    };
    let settings: RefreshJobSettings = match serde_json::from_slice(&bytes) {
        Ok(settings) => settings,
        Err(_) => {
            report(&diagnostics, "Disabled: invalid refresh configuration");
            return Ok(());
        }
    };
    if !settings.enabled || !settings.source_access_qualified {
        report(
            &diagnostics,
            "Disabled: source access must be qualified and enabled by the operator",
        );
        return Ok(());
    }
    let identity = format!("{:x}", Sha256::digest(&bytes));
    let limits = RefreshLimits::default();
    let mut schedule = match read(&root.join("refresh-schedule.json")) {
        Ok(Some(bytes)) => match serde_json::from_slice::<RefreshSchedule>(&bytes) {
            Ok(schedule) => schedule,
            Err(_) => {
                report(&diagnostics, "Stopped: invalid saved refresh schedule");
                return Ok(());
            }
        },
        Ok(None) => RefreshSchedule::new(identity.clone(), now(), &limits, now())
            .map_err(|_| RpcError::Remote("Cannot create refresh schedule".into()))?,
        Err(()) => {
            report(&diagnostics, "Stopped: cannot read refresh schedule");
            return Ok(());
        }
    };
    if schedule.resume(&identity, now(), &limits, now()).is_err() || save(&root, &schedule).is_err()
    {
        report(&diagnostics, "Stopped: cannot resume refresh schedule");
        return Ok(());
    }
    let cancel = global.cancellation();
    global.spawn("dw-wiki-refresh", async move {
        loop {
            if cancel.is_cancelled() { break; }
            if !schedule.due(now()) {
                let label = if schedule.stopped_denied() { "Stopped" } else { "Scheduled" };
                let duration = schedule.last_completed_ms().zip(schedule.last_attempt_ms()).map(|(end, start)| end.saturating_sub(start));
                report(&diagnostics, format!("{}: {}; last duration ms: {}", label, serde_json::to_string(&schedule).unwrap_or_else(|_| "unavailable".into()), duration.map(|ms| ms.to_string()).unwrap_or_else(|| "unavailable".into())));
                tokio::select! { biased; _ = cancel.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
                continue;
            }
            if schedule.start(now()).is_err() || save(&root, &schedule).is_err() {
                report(&diagnostics, "Stopped: cannot record refresh attempt"); break;
            }
            report(&diagnostics, "Wiki refresh running; queries use the current snapshot");
            let mut retry_floor = None;
            let result = match run(&settings, &root, cancel.clone()).await {
                RefreshJobOutcome::Candidate(bytes) => {
                    if cancel.is_cancelled() { break; }
                    let directory = root.clone();
                    // This join is retained through cancellation. Publication is
                    // finished or refused before module quiescence completes.
                    match tokio::task::spawn_blocking(move || RefreshControl::new(directory)?.submit(&bytes, now())).await {
                        Ok(Ok(Outcome::Published { .. })) => AttemptResult::Published,
                        Ok(Ok(Outcome::Unchanged { .. })) => AttemptResult::Unchanged,
                        Ok(Ok(Outcome::ReviewRequired { .. })) => AttemptResult::ReviewRequired,
                        Ok(Ok(Outcome::Rejected { .. })) => AttemptResult::Rejected,
                        _ => AttemptResult::Interrupted,
                    }
                }
                RefreshJobOutcome::Denied => AttemptResult::SourceDenied,
                RefreshJobOutcome::Cancelled => break,
                RefreshJobOutcome::Disabled => { report(&diagnostics, "Disabled"); break; }
                RefreshJobOutcome::RetryAt { not_before_ms } => {
                    retry_floor = Some(not_before_ms);
                    AttemptResult::RateLimited
                }
                RefreshJobOutcome::Failed(failure) => match failure {
                    RefreshJobFailure::Timeout => AttemptResult::Timeout,
                    RefreshJobFailure::Quota => AttemptResult::LimitExceeded,
                    RefreshJobFailure::InvalidOutput | RefreshJobFailure::InvalidSettings => AttemptResult::InvalidResponse,
                    _ => AttemptResult::Interrupted,
                },
            };
            if schedule.finish_with_retry_after(result, now(), &limits, now(), retry_floor).is_err() || save(&root, &schedule).is_err() {
                report(&diagnostics, "Stopped: cannot record refresh result"); break;
            }
        }
        Ok(())
    }).map_err(|_| RpcError::Remote("Cannot start Dandy's World refresh task".into()))?;
    Ok(())
}
