//! Validated immutable catalogs and crash-safe local publication.
use crate::model::{CatalogData, Citation, EvidenceState, SCHEMA_VERSION, SOURCE_ORIGIN};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

pub const MAX_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid catalog: {0}")]
    Invalid(String),
    #[error("snapshot storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("snapshot JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("another catalog publication is running")]
    Busy,
}
pub type Result<T> = std::result::Result<T, Error>;
fn require(ok: bool, reason: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(reason.into()))
    }
}
fn nonempty(s: &str) -> bool {
    !s.trim().is_empty()
}
fn hash_shape(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn citation_valid(citations: &[Citation], sources: &HashSet<&str>) -> Result<()> {
    require(!citations.is_empty(), "missing citations")?;
    for c in citations {
        require(
            sources.contains(c.source_id.as_str()) && nonempty(&c.quote),
            "orphan or empty citation",
        )?;
    }
    Ok(())
}
/// Validate before constructing any serving snapshot. Unknown data remains explicit.
pub fn validate(data: &CatalogData) -> Result<()> {
    require(
        data.schema_version == SCHEMA_VERSION,
        "unsupported schema version",
    )?;
    require(
        data.source_origin == SOURCE_ORIGIN,
        "unexpected source origin",
    )?;
    require(
        nonempty(&data.adapter_version)
            && nonempty(&data.crawl_started_at)
            && nonempty(&data.crawl_completed_at),
        "missing catalog metadata",
    )?;
    require(
        !data.sources.is_empty()
            && data.sources.len() <= 10_000
            && !data.entities.is_empty()
            && data.entities.len() <= 20_000,
        "source/entity bounds",
    )?;
    let mut sources = HashSet::new();
    let mut pages = HashSet::new();
    for s in &data.sources {
        require(
            nonempty(&s.id)
                && sources.insert(s.id.as_str())
                && s.page_id > 0
                && pages.insert(s.page_id),
            "duplicate or invalid source identity",
        )?;
        // A slash-delimited suffix fixes the complete authority, rejecting userinfo,
        // alternate ports, host suffixes, and scheme spoofing without URL rewriting.
        let suffix = s.url.strip_prefix(SOURCE_ORIGIN).unwrap_or("");
        require(
            suffix.starts_with("/wiki/")
                && suffix.len() > 6
                && !suffix
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace() || c == '\\'),
            "invalid canonical source URL",
        )?;
        require(
            nonempty(&s.title)
                && s.revision_id > 0
                && nonempty(&s.revision_timestamp)
                && s.validated_at_ms > 0
                && hash_shape(&s.content_sha256)
                && nonempty(&s.license)
                && s.license_url.starts_with("https://"),
            "invalid source provenance",
        )?;
    }
    let mut ids = HashSet::new();
    for e in &data.entities {
        require(
            nonempty(&e.id) && ids.insert(e.id.as_str()) && nonempty(&e.name),
            "duplicate or empty entity identity",
        )?;
    }
    let mut fact_ids = HashSet::new();
    let mut counts = BTreeMap::new();
    let mut total_facts = 0usize;
    for e in &data.entities {
        let kind = serde_json::to_value(e.kind)?.as_str().unwrap().to_owned();
        *counts.entry(kind).or_insert(0usize) += 1;
        require(
            e.aliases.len() <= 1000
                && e.aliases.iter().all(|s| nonempty(s))
                && e.facts.len() <= 10_000
                && e.relationships.len() <= 10_000,
            "entity field bounds",
        )?;
        total_facts += e.facts.len();
        require(total_facts <= 500_000, "catalog fact bounds")?;
        for f in &e.facts {
            require(
                nonempty(&f.id)
                    && fact_ids.insert(f.id.as_str())
                    && nonempty(&f.key)
                    && nonempty(&f.text),
                "duplicate or empty fact identity",
            )?;
            require(
                f.conditions.iter().all(|s| nonempty(s))
                    && f.unit.as_ref().is_none_or(|s| nonempty(s)),
                "empty condition/unit",
            )?;
            if f.state == EvidenceState::Supported {
                require(!f.value.is_null(), "supported fact has absent value")?;
            }
            if let Some(n) = f.value.as_number() {
                require(
                    n.as_f64().is_some_and(f64::is_finite),
                    "nonfinite numeric value",
                )?;
            }
            citation_valid(&f.citations, &sources)?;
        }
        for r in &e.relationships {
            require(
                nonempty(&r.relation) && ids.contains(r.target_id.as_str()),
                "orphan relationship",
            )?;
            citation_valid(&r.citations, &sources)?;
        }
    }
    let c = &data.coverage;
    require(
        c.imported_pages == c.discovered_pages && c.imported_pages == data.sources.len(),
        "incomplete discovery/import",
    )?;
    let namespace_total = c
        .namespace_counts
        .values()
        .try_fold(0usize, |n, v| n.checked_add(*v));
    require(
        namespace_total == Some(c.imported_pages) && c.entities_by_kind == counts,
        "inconsistent coverage counts",
    )?;
    require(
        c.nonredirect_articles
            .checked_add(c.redirects)
            .is_some_and(|n| c.namespace_counts.get("articles") == Some(&n)),
        "invalid article/redirect coverage",
    )?;
    for entry in c.excluded.iter().chain(&c.unresolved_redirects) {
        require(
            pages.contains(&entry.page_id) && nonempty(&entry.title) && nonempty(&entry.reason),
            "invalid coverage exception",
        )?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub id: String,
    pub data: Arc<CatalogData>,
}
impl Snapshot {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        require(
            bytes.len() <= MAX_SNAPSHOT_BYTES,
            "snapshot exceeds byte limit",
        )?;
        let data: CatalogData = serde_json::from_slice(bytes)?;
        validate(&data)?;
        Ok(Self {
            id: format!("{:x}", Sha256::digest(bytes)),
            data: Arc::new(data),
        })
    }
}

#[derive(Debug)]
pub struct Store {
    root: PathBuf,
}
fn safe_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .mode(0o600);
    }
    options
}
fn read_regular(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = safe_options().read(true).open(path)?;
    require(
        file.metadata()?.is_file(),
        "store entry is not a regular file",
    )?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    require(bytes.len() <= limit, "store entry exceeds byte limit")?;
    Ok(bytes)
}
fn check_root(root: &Path) -> Result<()> {
    let mut path = PathBuf::new();
    for component in root.components() {
        require(
            !matches!(component, Component::ParentDir),
            "parent traversal in store root",
        )?;
        path.push(component);
        let metadata = fs::symlink_metadata(&path)?;
        require(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "store root contains symlink/non-directory",
        )?;
    }
    Ok(())
}
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
impl Store {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = if root.as_ref().is_absolute() {
            root.as_ref().to_owned()
        } else {
            std::env::current_dir()?.join(root)
        };
        // Creation is allowed; every component is then required to be a real directory.
        fs::create_dir_all(&root)?;
        check_root(&root)?;
        Ok(Self { root })
    }
    pub fn load(&self) -> Result<Snapshot> {
        check_root(&self.root)?;
        let pointer = read_regular(&self.root.join("active"), 64)?;
        let id = std::str::from_utf8(&pointer)
            .map_err(|_| Error::Invalid("invalid active pointer".into()))?;
        require(hash_shape(id), "invalid active pointer")?;
        let snapshot = Snapshot::from_bytes(&read_regular(
            &self.root.join(format!("{id}.json")),
            MAX_SNAPSHOT_BYTES,
        )?)?;
        require(snapshot.id == id, "snapshot checksum mismatch")?;
        Ok(snapshot)
    }
    pub fn publish_bytes(&self, bytes: &[u8]) -> Result<Snapshot> {
        let snapshot = Snapshot::from_bytes(bytes)?;
        check_root(&self.root)?;
        let lock = safe_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("writer.lock"))?;
        require(
            lock.metadata()?.is_file(),
            "writer lock is not a regular file",
        )?;
        lock.try_lock().map_err(|_| Error::Busy)?;
        let destination = self.root.join(format!("{}.json", snapshot.id));
        match read_regular(&destination, MAX_SNAPSHOT_BYTES) {
            Ok(existing) => require(existing == bytes, "existing immutable snapshot is corrupt")?,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                let temporary = self.write_temp(bytes)?;
                // Linking is no-clobber, and only a fully flushed file becomes visible.
                fs::hard_link(&temporary.0, &destination)?;
                File::open(&self.root)?.sync_all()?;
            }
            Err(e) => return Err(e),
        }
        // Refuse a suspicious pointer even though rename itself never follows it.
        if let Ok(metadata) = fs::symlink_metadata(self.root.join("active")) {
            require(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "active pointer is not a regular file",
            )?;
        }
        let temporary = self.write_temp(snapshot.id.as_bytes())?;
        fs::rename(&temporary.0, self.root.join("active"))?;
        File::open(&self.root)?.sync_all()?;
        Ok(snapshot)
    }
    fn write_temp(&self, bytes: &[u8]) -> Result<TempFile> {
        for _ in 0..100 {
            let path = self.root.join(format!(
                ".candidate-{}-{}",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            match safe_options().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let guard = TempFile(path);
                    file.write_all(bytes)?;
                    file.sync_all()?;
                    return Ok(guard);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::Invalid("temporary filename space exhausted".into()))
    }
}
