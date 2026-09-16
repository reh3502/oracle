//! Durable review admission. Source adapters must establish validation evidence
//! before submitting bytes; members never select a file, source or approval.
use crate::{
    refresh_review::{
        CandidateReview, ReviewApproval, ReviewStatus, publication_allowed, review_candidate,
    },
    snapshot::{MAX_SNAPSHOT_BYTES, Snapshot, Store},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("a refresh or review is already running")]
    Busy,
    #[error("there is no pending candidate")]
    NoCandidate,
    #[error("candidate approval does not match the current snapshot and pending candidate")]
    ApprovalMismatch,
    #[error("refresh control data is invalid")]
    Invalid,
    #[error("refresh control storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("catalog storage: {0}")]
    Store(#[from] crate::snapshot::Error),
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Outcome {
    Unchanged { snapshot_id: String },
    Published { snapshot_id: String },
    ReviewRequired { review: CandidateReview },
    Rejected { review: CandidateReview },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    active_digest: String,
    candidate_digest: String,
    created_at_ms: u64,
}
pub struct RefreshControl {
    root: PathBuf,
    store: Store,
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn options() -> OpenOptions {
    crate::snapshot::safe_options()
}
fn read(path: &Path, bound: usize) -> Result<Vec<u8>> {
    let file = options().read(true).open(path)?;
    if !file.metadata()?.is_file() || crate::snapshot::is_reparse(&file.metadata()?) {
        return Err(Error::Invalid);
    }
    let mut bytes = vec![];
    file.take(bound as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > bound {
        return Err(Error::Invalid);
    }
    Ok(bytes)
}
impl RefreshControl {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = std::path::absolute(root)?;
        let store = Store::new(&root)?;
        Ok(Self { root, store })
    }
    fn lock(&self) -> Result<File> {
        // Recheck ancestor/symlink invariants before every operation.
        Store::new(&self.root)?;
        let lock = options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("refresh.lock"))?;
        if !lock.metadata()?.is_file() || crate::snapshot::is_reparse(&lock.metadata()?) {
            return Err(Error::Invalid);
        }
        lock.try_lock().map_err(|_| Error::Busy)?;
        Ok(lock)
    }
    fn candidate_path(&self, id: &str) -> Result<PathBuf> {
        if !digest(id) {
            return Err(Error::Invalid);
        }
        Ok(self.root.join(format!("refresh-candidate-{id}.json")))
    }
    fn atomic(&self, name: &Path, bytes: &[u8]) -> Result<()> {
        if name.parent() != Some(self.root.as_path()) {
            return Err(Error::Invalid);
        }
        let name = name
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(Error::Invalid)?;
        self.store.write_refresh_file(name, bytes)?;
        Ok(())
    }
    fn pending(&self) -> Result<Option<Pending>> {
        match read(&self.root.join("refresh-pending.json"), 1024) {
            Ok(bytes) => {
                let pending: Pending =
                    serde_json::from_slice(&bytes).map_err(|_| Error::Invalid)?;
                if !digest(&pending.active_digest) || !digest(&pending.candidate_digest) {
                    return Err(Error::Invalid);
                }
                Ok(Some(pending))
            }
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn clear_pending(&self, pending: &Pending) -> Result<()> {
        // The pointer is removed first: interruption can leave only a discardable
        // candidate, never a pointer to a candidate that has already been removed.
        fs::remove_file(self.root.join("refresh-pending.json"))?;
        crate::snapshot::sync_directory(&self.root)?;
        match fs::remove_file(self.candidate_path(&pending.candidate_digest)?) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
    /// Submission uses source-validated bytes. A failed/rejected submission leaves
    /// the active snapshot and any earlier pending review unchanged.
    pub fn submit(&self, bytes: &[u8], now_ms: u64) -> Result<Outcome> {
        let _lock = self.lock()?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(Error::Invalid);
        }
        let active = self.store.load()?;
        let review = review_candidate(&active, bytes, now_ms);
        if review.status == ReviewStatus::Rejected {
            return Ok(Outcome::Rejected { review });
        }
        if review.candidate_digest == active.id {
            return Ok(Outcome::Unchanged {
                snapshot_id: active.id,
            });
        }
        if review.status == ReviewStatus::Eligible {
            let snapshot = self.store.publish_if_current(bytes, &active.id)?;
            return Ok(Outcome::Published {
                snapshot_id: snapshot.id,
            });
        }
        let old = self.pending()?;
        let pending = Pending {
            active_digest: active.id,
            candidate_digest: review.candidate_digest.clone(),
            created_at_ms: now_ms,
        };
        let path = self.candidate_path(&pending.candidate_digest)?;
        self.atomic(&path, bytes)?;
        self.atomic(
            &self.root.join("refresh-pending.json"),
            &serde_json::to_vec(&pending).map_err(|_| Error::Invalid)?,
        )?;
        if let Some(old) = old
            && old.candidate_digest != pending.candidate_digest
        {
            let _ = fs::remove_file(self.candidate_path(&old.candidate_digest)?);
        }
        Ok(Outcome::ReviewRequired { review })
    }
    pub fn inspect_pending(&self, now_ms: u64) -> Result<Option<CandidateReview>> {
        let _lock = self.lock()?;
        let Some(pending) = self.pending()? else {
            return Ok(None);
        };
        let active = self.store.load()?;
        if active.id != pending.active_digest {
            return Err(Error::ApprovalMismatch);
        }
        let bytes = read(
            &self.candidate_path(&pending.candidate_digest)?,
            MAX_SNAPSHOT_BYTES,
        )?;
        let review = review_candidate(&active, &bytes, now_ms);
        if review.candidate_digest != pending.candidate_digest {
            return Err(Error::Invalid);
        }
        Ok(Some(review))
    }
    /// The caller must authenticate this operator action. Hashes identify a
    /// decision, not authority, and cannot turn a rejected candidate into valid data.
    pub fn approve(&self, approval: &ReviewApproval, now_ms: u64) -> Result<Snapshot> {
        let _lock = self.lock()?;
        let pending = self.pending()?.ok_or(Error::NoCandidate)?;
        if approval.active_digest != pending.active_digest
            || approval.candidate_digest != pending.candidate_digest
        {
            return Err(Error::ApprovalMismatch);
        }
        let active = self.store.load()?;
        if active.id != approval.active_digest {
            return Err(Error::ApprovalMismatch);
        }
        let bytes = read(
            &self.candidate_path(&pending.candidate_digest)?,
            MAX_SNAPSHOT_BYTES,
        )?;
        if !publication_allowed(&active, &bytes, now_ms, Some(approval)) {
            return Err(Error::Invalid);
        }
        let snapshot = self.store.publish_if_current(&bytes, &active.id)?;
        self.clear_pending(&pending)?;
        Ok(snapshot)
    }
    pub fn discard(&self, approval: &ReviewApproval) -> Result<()> {
        let _lock = self.lock()?;
        let pending = self.pending()?.ok_or(Error::NoCandidate)?;
        if pending.active_digest != approval.active_digest
            || pending.candidate_digest != approval.candidate_digest
        {
            return Err(Error::ApprovalMismatch);
        }
        self.clear_pending(&pending)
    }
}
