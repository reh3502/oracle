//! Validated immutable catalogs and crash-safe local publication.
use crate::model::{CatalogData, Citation, EvidenceState, SCHEMA_VERSION, SOURCE_ORIGIN};
use serde::{Deserialize, Serialize};
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
    #[error("another snapshot operation is running")]
    Busy,
    #[error("active snapshot changed")]
    Conflict,
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
/// Only canonical original raster URLs from this wiki's CDN are accepted.
pub fn image_url_valid(url: &str) -> bool {
    let Some(tail) =
        url.strip_prefix("https://static.wikia.nocookie.net/dandys-world-robloxhorror/images/")
    else {
        return false;
    };
    if url.len() > 2048
        || !url.is_ascii()
        || url
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace() || matches!(b, b'\\' | b'#'))
    {
        return false;
    }
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    if !query.is_empty()
        && !query.strip_prefix("cb=").is_some_and(|v| {
            !v.is_empty() && v.len() <= 20 && v.bytes().all(|b| b.is_ascii_digit())
        })
    {
        return false;
    }
    let parts: Vec<_> = path.split('/').collect();
    if !matches!(parts.len(), 5 | 7)
        || parts[0].len() != 1
        || parts[1].len() != 2
        || !parts[..2]
            .iter()
            .all(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
        || parts[3] != "revision"
        || parts[4] != "latest"
    {
        return false;
    }
    if parts.len() == 7
        && (parts[5] != "scale-to-width-down"
            || !parts[6]
                .parse::<u32>()
                .is_ok_and(|n| (1..=2048).contains(&n)))
    {
        return false;
    }
    let filename = parts[2];
    let lower = filename.to_ascii_lowercase();
    if filename.is_empty()
        || filename.contains("..")
        || ["%2e", "%2f", "%5c", "%25", "%00", "%0a", "%0d"]
            .iter()
            .any(|v| lower.contains(v))
        || ![".png", ".jpg", ".jpeg", ".webp", ".gif"]
            .iter()
            .any(|ext| lower.ends_with(ext))
    {
        return false;
    }
    let bytes = filename.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() || !bytes[i + 1..i + 3].iter().all(|b| b.is_ascii_hexdigit()) {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
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
    require(
        data.images.len() <= data.entities.len(),
        "image count exceeds entities",
    )?;
    for (id, image) in &data.images {
        let source = data
            .sources
            .iter()
            .find(|s| s.id == *id && id == &format!("page:{}", s.page_id));
        require(
            ids.contains(id.as_str())
                && source.is_some_and(|s| s.revision_id == image.article_revision),
            "image lacks canonical entity article provenance",
        )?;
        require(
            image_url_valid(&image.url)
                && image.file_title.starts_with("File:")
                && image.file_title.len() <= 300
                && image.file_title.len() > 5
                && !image.file_title.chars().any(char::is_control)
                && (1..=9_007_199_254_740_991).contains(&image.file_page_id)
                && (1..=9_007_199_254_740_991).contains(&image.revision)
                && image.width > 0
                && image.height > 0
                && image.width <= 100_000
                && image.height <= 100_000
                && image.validated_at_ms > 0
                && image.sha1.len() == 40
                && image
                    .sha1
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                && matches!(
                    image.mime.as_str(),
                    "image/png" | "image/jpeg" | "image/webp" | "image/gif"
                ),
            "invalid image metadata",
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

/// Every regular file under the store, including source jobs, shares this disk budget.
pub const MAX_STORE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_POINTER_BYTES: usize = 512;
const MAX_ENTRIES: usize = 10_000;

#[derive(Clone, Debug)]
pub struct LoadOutcome {
    pub snapshot: Snapshot,
    /// True only when a validated previous catalog replaced an unusable current catalog.
    pub recovered: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct RetentionReport {
    pub retained_snapshots: usize,
    pub removed_files: usize,
    /// All regular-file bytes recursively under the store, including source jobs.
    /// Hardlinked files are counted once on Unix; unrelated files are never pruned.
    pub total_bytes: u64,
}
/// Durable publication milestones. Observers cannot change the transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishStep {
    CatalogDurable,
    PreviousDurable,
    ActiveCommitted,
    CleanupComplete,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Heads {
    store_format_version: u32,
    current: String,
    previous: Option<String>,
}
impl Heads {
    fn new(current: String, previous: Option<String>) -> Self {
        Self {
            store_format_version: 1,
            current,
            previous,
        }
    }
    fn validate(&self) -> Result<()> {
        require(self.store_format_version == 1, "unsupported store format")?;
        require(
            hash_shape(&self.current),
            "invalid current snapshot pointer",
        )?;
        require(
            self.previous
                .as_ref()
                .is_none_or(|p| hash_shape(p) && p != &self.current),
            "invalid previous snapshot pointer",
        )
    }
    fn ids(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.current.as_str()).chain(self.previous.as_deref())
    }
}
#[derive(Debug)]
pub struct Store {
    root: PathBuf,
}
pub(crate) fn safe_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open the reparse point itself, never its target. Metadata checks below
        // reject all reparse points (including links and junctions).
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    options
}
pub(crate) fn is_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}
fn read_regular(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = safe_options().read(true).open(path)?;
    require(
        file.metadata()?.is_file() && !is_reparse(&file.metadata()?),
        "store entry is not a regular file",
    )?;
    require(
        file.metadata()?.len() <= limit as u64,
        "store entry exceeds byte limit",
    )?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    require(bytes.len() <= limit, "store entry exceeds byte limit")?;
    Ok(bytes)
}
fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    require(
        path.components().count() <= 64
            && !path.components().any(|c| matches!(c, Component::ParentDir)),
        "invalid store path",
    )?;
    Ok(path)
}
fn check_root(root: &Path) -> Result<()> {
    let mut path = PathBuf::new();
    for component in root.components() {
        require(
            !matches!(component, Component::ParentDir),
            "parent traversal in store root",
        )?;
        path.push(component);
        // A drive prefix such as C: is not the absolute root until RootDir.
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        require(
            metadata.is_dir() && !is_reparse(&metadata),
            "store root contains symlink/non-directory",
        )?;
    }
    Ok(())
}
fn create_root(root: &Path) -> Result<()> {
    let mut path = PathBuf::new();
    for component in root.components() {
        path.push(component);
        // A drive prefix such as C: is not the absolute root until RootDir.
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(meta) => require(
                meta.is_dir() && !is_reparse(&meta),
                "store root contains symlink/non-directory",
            )?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    fs::DirBuilder::new().mode(0o700).create(&path)?;
                }
                #[cfg(windows)]
                oracle_local_ipc::create_private_directory(&path)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    check_root(root)
}
fn regular_or_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => require(meta.is_file() && !is_reparse(&meta), "unsafe store entry"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
fn is_missing(error: &Error) -> bool {
    matches!(error, Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound)
}
pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all().map_err(Error::from)
    }
    #[cfg(windows)]
    {
        oracle_local_ipc::durable_directory(path).map_err(Error::from)
    }
}
fn candidate_name(name: &str) -> bool {
    name.strip_prefix(".candidate-")
        .and_then(|s| s.split_once('-'))
        .is_some_and(|(pid, id)| {
            !pid.is_empty()
                && !id.is_empty()
                && pid.len() <= 20
                && id.len() <= 20
                && pid.bytes().chain(id.bytes()).all(|b| b.is_ascii_digit())
        })
}
fn catalog_name(name: &str) -> Option<&str> {
    name.strip_suffix(".json").filter(|s| hash_shape(s))
}
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
struct TempDirectory(PathBuf);
impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Store {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = absolute(root.as_ref())?;
        create_root(&root)?;
        Ok(Self { root })
    }
    fn lock_file(&self, name: &str) -> Result<File> {
        check_root(&self.root)?;
        let file = safe_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join(name))?;
        require(
            file.metadata()?.is_file() && !is_reparse(&file.metadata()?),
            "store lock is not a regular file",
        )?;
        Ok(file)
    }
    fn writer(&self) -> Result<(File, File)> {
        let writer = self.lock_file("writer.lock")?;
        writer.try_lock().map_err(|_| Error::Busy)?;
        // Readers never contend for writer.lock; publication stays single-flight
        // without failing merely because a strict reader is finishing its load.
        let readers = self.lock_file("readers.lock")?;
        readers.lock()?;
        regular_or_absent(&self.root.join("active"))?;
        regular_or_absent(&self.root.join("previous"))?;
        Ok((writer, readers))
    }
    fn heads(&self) -> Result<Heads> {
        let bytes = read_regular(&self.root.join("active"), MAX_POINTER_BYTES)?;
        // Existing Stage 1/2 stores upgrade only when a mutation is committed.
        let heads = if bytes.len() == 64 && std::str::from_utf8(&bytes).is_ok_and(hash_shape) {
            Heads::new(String::from_utf8(bytes).expect("checked UTF-8"), None)
        } else {
            serde_json::from_slice::<Heads>(&bytes)?
        };
        heads.validate()?;
        Ok(heads)
    }
    fn catalog_bytes(&self, id: &str) -> Result<(Snapshot, Vec<u8>)> {
        require(hash_shape(id), "invalid snapshot ID")?;
        let bytes = read_regular(&self.root.join(format!("{id}.json")), MAX_SNAPSHOT_BYTES)?;
        let snapshot = Snapshot::from_bytes(&bytes)?;
        require(snapshot.id == id, "snapshot checksum mismatch")?;
        Ok((snapshot, bytes))
    }
    /// Strict load never repairs or selects another snapshot. Legacy plain64 pointers
    /// remain readable; format 1 atomically stores both current and previous IDs.
    pub fn load(&self) -> Result<Snapshot> {
        let lock = self.lock_file("readers.lock")?;
        lock.lock_shared()?;
        self.catalog_bytes(&self.heads()?.current).map(|(s, _)| s)
    }
    /// Avoid rebuilding an unchanged query index. The pointer and any new catalog
    /// are read under one shared lock so pruning cannot race the read.
    pub fn load_if_changed(&self, current: &str) -> Result<Option<Snapshot>> {
        let lock = self.lock_file("readers.lock")?;
        lock.lock_shared()?;
        let head = self.heads()?.current;
        if head == current {
            return Ok(None);
        }
        self.catalog_bytes(&head)
            .map(|(snapshot, _)| Some(snapshot))
    }
    fn mirror(&self) -> Result<String> {
        let bytes = read_regular(&self.root.join("previous"), 64)?;
        let id = std::str::from_utf8(&bytes)
            .map_err(|_| Error::Invalid("invalid recovery pointer".into()))?;
        require(hash_shape(id), "invalid recovery pointer")?;
        Ok(id.to_owned())
    }
    /// Recover only from recorded history, never from an uncommitted candidate.
    /// Successful fallback clears the rollback target and preserves source times.
    pub fn load_recovering(&self) -> Result<LoadOutcome> {
        let _locks = self.writer()?;
        let heads = self.heads();
        if let Ok(heads) = &heads
            && let Ok((snapshot, _)) = self.catalog_bytes(&heads.current)
        {
            let mut heads = heads.clone();
            let invalid_previous = heads
                .previous
                .as_ref()
                .is_some_and(|id| self.catalog_bytes(id).is_err());
            if invalid_previous {
                heads.previous = None;
            }
            // Discard abandoned writes before reserving bytes for repaired metadata.
            self.cleanup_locked(&heads)?;
            if invalid_previous {
                self.write_heads(&heads)?;
            }
            self.finish_heads(&heads)?;
            return Ok(LoadOutcome {
                snapshot,
                recovered: false,
            });
        }
        let recorded = heads.as_ref().ok().and_then(|h| h.previous.clone());
        let mirror = self.mirror().ok();
        for id in recorded.into_iter().chain(mirror) {
            if let Ok((snapshot, _)) = self.catalog_bytes(&id) {
                let heads = Heads::new(id, None);
                // The old current is already unusable. Keep the selected recorded
                // fallback while freeing incomplete/orphan files before repair.
                self.cleanup_locked(&heads)?;
                self.write_heads(&heads)?;
                self.finish_heads(&heads)?;
                self.cleanup_locked(&heads)?;
                return Ok(LoadOutcome {
                    snapshot,
                    recovered: true,
                });
            }
        }
        Err(Error::Invalid(
            "no complete current or recorded previous snapshot".into(),
        ))
    }
    pub fn publish_bytes(&self, bytes: &[u8]) -> Result<Snapshot> {
        self.publish_bytes_observed(bytes, |_| {})
    }
    /// Observe durable phases without changing publication semantics. An observer
    /// must be nonblocking; tests can terminate a child at an exact crash boundary.
    pub fn publish_bytes_observed(
        &self,
        bytes: &[u8],
        observer: impl FnMut(PublishStep),
    ) -> Result<Snapshot> {
        self.publish(bytes, None, observer)
    }
    /// Compare the reviewed base digest under the same lock as publication.
    pub fn publish_if_current(&self, bytes: &[u8], expected_active: &str) -> Result<Snapshot> {
        require(
            hash_shape(expected_active),
            "invalid expected active snapshot",
        )?;
        self.publish(bytes, Some(expected_active), |_| {})
    }
    fn publish(
        &self,
        bytes: &[u8],
        expected_active: Option<&str>,
        mut observer: impl FnMut(PublishStep),
    ) -> Result<Snapshot> {
        let snapshot = Snapshot::from_bytes(bytes)?;
        let _locks = self.writer()?;
        let old = match self.heads() {
            Ok(heads) => {
                self.validate_heads(&heads)?;
                Some(heads)
            }
            Err(error) if is_missing(&error) => {
                require(
                    !self.root.join("previous").try_exists()?,
                    "recover the missing active pointer before publication",
                )?;
                None
            }
            Err(error) => return Err(error),
        };
        if expected_active
            .is_some_and(|expected| old.as_ref().is_none_or(|h| h.current != expected))
        {
            return Err(Error::Conflict);
        }
        if let Some(old) = &old {
            self.cleanup_locked(old)?;
        } else {
            self.cleanup_orphans(&HashSet::new())?;
        }
        self.ensure_catalog(&snapshot.id, bytes)?;
        observer(PublishStep::CatalogDurable);
        let heads = if let Some(old) = &old
            && old.current == snapshot.id
        {
            old.clone()
        } else {
            Heads::new(snapshot.id.clone(), old.as_ref().map(|h| h.current.clone()))
        };
        if let Some(old) = &old {
            self.write_pointer("previous", old.current.as_bytes())?;
        }
        observer(PublishStep::PreviousDurable);
        self.write_heads(&heads)?;
        observer(PublishStep::ActiveCommitted);
        self.finish_heads(&heads)?;
        self.cleanup_locked(&heads)?;
        observer(PublishStep::CleanupComplete);
        Ok(snapshot)
    }
    /// Swap the two validated catalog IDs; never rewrite data or validation times.
    pub fn rollback(&self) -> Result<Snapshot> {
        let _locks = self.writer()?;
        let old = self.heads()?;
        self.validate_heads(&old)?;
        let previous = old
            .previous
            .clone()
            .ok_or_else(|| Error::Invalid("no previous snapshot to roll back to".into()))?;
        let snapshot = self.catalog_bytes(&previous)?.0;
        self.cleanup_locked(&old)?;
        self.write_pointer("previous", old.current.as_bytes())?;
        let heads = Heads::new(previous, Some(old.current));
        self.write_heads(&heads)?;
        self.finish_heads(&heads)?;
        self.cleanup_locked(&heads)?;
        Ok(snapshot)
    }
    fn validate_heads(&self, heads: &Heads) -> Result<()> {
        heads.validate()?;
        for id in heads.ids() {
            self.catalog_bytes(id)?;
        }
        Ok(())
    }
    fn write_heads(&self, heads: &Heads) -> Result<()> {
        heads.validate()?;
        self.write_pointer("active", &serde_json::to_vec(heads)?)
    }
    fn finish_heads(&self, heads: &Heads) -> Result<()> {
        self.write_pointer(
            "previous",
            heads.previous.as_ref().unwrap_or(&heads.current).as_bytes(),
        )
    }
    fn write_pointer(&self, name: &str, bytes: &[u8]) -> Result<()> {
        regular_or_absent(&self.root.join(name))?;
        let temporary = self.write_temp(bytes)?;
        #[cfg(unix)]
        fs::rename(&temporary.0, self.root.join(name))?;
        #[cfg(windows)]
        oracle_local_ipc::atomic_replace(&temporary.0, &self.root.join(name))?;
        sync_directory(&self.root)
    }
    fn ensure_catalog(&self, id: &str, bytes: &[u8]) -> Result<()> {
        let destination = self.root.join(format!("{id}.json"));
        match read_regular(&destination, MAX_SNAPSHOT_BYTES) {
            Ok(existing) => require(existing == bytes, "existing immutable snapshot is corrupt")?,
            Err(error) if is_missing(&error) => {
                require(
                    self.disk_bytes()?
                        .checked_add(bytes.len() as u64)
                        .is_some_and(|n| n <= MAX_STORE_BYTES),
                    "store disk budget exceeded",
                )?;
                let temporary = self.write_temp(bytes)?;
                #[cfg(unix)]
                fs::hard_link(&temporary.0, &destination)?;
                #[cfg(windows)]
                oracle_local_ipc::atomic_publish_new(&temporary.0, &destination)?;
                sync_directory(&self.root)?;
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }
    fn managed_entries(&self) -> Result<Vec<(PathBuf, String, u64)>> {
        let mut entries = Vec::new();
        for (index, entry) in fs::read_dir(&self.root)?.enumerate() {
            require(index < MAX_ENTRIES, "store directory entry limit exceeded")?;
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if catalog_name(&name).is_some()
                || candidate_name(&name)
                || matches!(name.as_str(), "active" | "previous")
            {
                let metadata = fs::symlink_metadata(entry.path())?;
                require(
                    metadata.is_file() && !is_reparse(&metadata),
                    "unsafe managed store entry",
                )?;
                entries.push((entry.path(), name, metadata.len()));
            }
        }
        Ok(entries)
    }
    /// Last-observed disk use for diagnostics. Concurrent worker staging may
    /// change during this read; admission still uses the writer-locked quota.
    pub fn disk_usage(&self) -> Result<u64> {
        self.disk_bytes()
    }
    fn disk_bytes(&self) -> Result<u64> {
        let mut directories = vec![(self.root.clone(), 0usize)];
        let mut entries = 0usize;
        let mut bytes = 0u64;
        let mut inodes = HashSet::new();
        while let Some((directory, depth)) = directories.pop() {
            require(depth <= 32, "store directory depth limit exceeded")?;
            check_root(&directory)?;
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                entries += 1;
                require(entries <= 50_000, "store tree entry limit exceeded")?;
                let metadata = fs::symlink_metadata(entry.path())?;
                require(!is_reparse(&metadata), "symlink in store quota tree")?;
                if metadata.is_dir() {
                    directories.push((entry.path(), depth + 1));
                } else {
                    require(metadata.is_file(), "nonregular file in store quota tree")?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        if !inodes.insert((metadata.dev(), metadata.ino())) {
                            continue;
                        }
                    }
                    #[cfg(windows)]
                    if !inodes.insert(oracle_local_ipc::file_identity(&entry.path())?) {
                        continue;
                    }
                    bytes = bytes
                        .checked_add(metadata.len())
                        .ok_or_else(|| Error::Invalid("store size overflow".into()))?;
                }
            }
        }
        Ok(bytes)
    }
    fn cleanup_orphans(&self, retained: &HashSet<&str>) -> Result<usize> {
        let entries = self.managed_entries()?;
        let mut removed = 0;
        for (path, name, _) in entries {
            if candidate_name(&name) || catalog_name(&name).is_some_and(|id| !retained.contains(id))
            {
                fs::remove_file(path)?;
                removed += 1;
            }
        }
        if removed > 0 {
            sync_directory(&self.root)?;
        }
        Ok(removed)
    }
    fn cleanup_locked(&self, heads: &Heads) -> Result<RetentionReport> {
        let retained = heads.ids().collect::<HashSet<_>>();
        let removed_files = self.cleanup_orphans(&retained)?;
        let total_bytes = self.disk_bytes()?;
        require(total_bytes <= MAX_STORE_BYTES, "store disk budget exceeded")?;
        Ok(RetentionReport {
            retained_snapshots: retained.len(),
            removed_files,
            total_bytes,
        })
    }
    /// Remove only recognized orphan catalogs and abandoned candidate files.
    /// Existing Arc< CatalogData > readers keep their immutable in-memory catalog.
    pub fn cleanup(&self) -> Result<RetentionReport> {
        let _locks = self.writer()?;
        let heads = self.heads()?;
        self.validate_heads(&heads)?;
        let report = self.cleanup_locked(&heads)?;
        self.finish_heads(&heads)?;
        Ok(RetentionReport {
            total_bytes: self.disk_bytes()?,
            ..report
        })
    }
    fn write_temp(&self, bytes: &[u8]) -> Result<TempFile> {
        require(
            self.disk_bytes()?
                .checked_add(bytes.len() as u64)
                .is_some_and(|n| n <= MAX_STORE_BYTES),
            "store disk budget exceeded",
        )?;
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
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(Error::Invalid("temporary filename space exhausted".into()))
    }
    /// Atomically replace bounded refresh-controller metadata or candidate bytes.
    /// The namespace cannot address catalog heads, locks, paths or subdirectories.
    pub fn write_refresh_file(&self, name: &str, bytes: &[u8]) -> Result<()> {
        require(
            name.starts_with("refresh-")
                && name.len() > "refresh-".len()
                && name.len() <= 128
                && !name.contains("..")
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "invalid refresh filename",
        )?;
        require(
            bytes.len() <= MAX_SNAPSHOT_BYTES,
            "refresh file exceeds byte limit",
        )?;
        let _locks = self.writer()?;
        self.write_pointer(name, bytes)
    }
    /// Write a self-contained, checksummed active+previous backup into a new
    /// directory. A final no-replace rename exposes only a complete backup.
    pub fn backup_to(&self, destination: impl AsRef<Path>) -> Result<()> {
        let _locks = self.writer()?;
        let heads = self.heads()?;
        let catalogs = heads
            .ids()
            .map(|id| {
                self.catalog_bytes(id)
                    .map(|(_, bytes)| (id.to_owned(), bytes))
            })
            .collect::<Result<Vec<_>>>()?;
        let destination = absolute(destination.as_ref())?;
        self.separate_path(&destination)?;
        let parent = destination
            .parent()
            .ok_or_else(|| Error::Invalid("backup needs a parent directory".into()))?;
        check_root(parent)?;
        require(
            matches!(fs::symlink_metadata(&destination), Err(ref e) if e.kind() == std::io::ErrorKind::NotFound),
            "backup destination already exists or is unsafe",
        )?;
        let stage = new_backup_directory(parent)?;
        for (id, bytes) in catalogs {
            write_complete(&stage.0.join(format!("{id}.json")), &bytes)?;
        }
        write_complete(&stage.0.join("manifest.json"), &serde_json::to_vec(&heads)?)?;
        sync_directory(&stage.0)?;
        rename_directory_new(&stage.0, &destination)?;
        sync_directory(parent)
    }
    /// Validate the entire external backup before changing the destination head.
    /// An existing store must be healthy; restore a damaged deployment into a new
    /// isolated store instead of overwriting corrupt immutable catalog files.
    pub fn restore_from(&self, backup: impl AsRef<Path>) -> Result<Snapshot> {
        let backup = absolute(backup.as_ref())?;
        self.separate_path(&backup)?;
        check_root(&backup)?;
        let heads: Heads = serde_json::from_slice(&read_regular(
            &backup.join("manifest.json"),
            MAX_POINTER_BYTES,
        )?)?;
        heads.validate()?;
        let expected = std::iter::once("manifest.json".to_owned())
            .chain(heads.ids().map(|id| format!("{id}.json")))
            .collect::<HashSet<_>>();
        let mut seen = HashSet::new();
        for entry in fs::read_dir(&backup)? {
            let entry = entry?;
            require(seen.len() < 3, "backup entry limit exceeded")?;
            let name = entry
                .file_name()
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::Invalid("invalid backup filename".into()))?;
            require(
                expected.contains(&name) && seen.insert(name),
                "unexpected backup entry",
            )?;
            let meta = fs::symlink_metadata(entry.path())?;
            require(meta.is_file() && !is_reparse(&meta), "unsafe backup entry")?;
        }
        require(seen == expected, "incomplete backup")?;
        let mut catalogs = Vec::new();
        for id in heads.ids() {
            let bytes = read_regular(&backup.join(format!("{id}.json")), MAX_SNAPSHOT_BYTES)?;
            let snapshot = Snapshot::from_bytes(&bytes)?;
            require(snapshot.id == id, "backup checksum mismatch")?;
            catalogs.push((snapshot, bytes));
        }
        let snapshot = catalogs[0].0.clone();
        let _locks = self.writer()?;
        let old = match self.heads() {
            Ok(old) => {
                self.validate_heads(&old)?;
                Some(old)
            }
            Err(error) if is_missing(&error) => {
                require(
                    !self.root.join("previous").try_exists()?,
                    "recover the missing active pointer before restore",
                )?;
                None
            }
            Err(error) => return Err(error),
        };
        if let Some(old) = &old {
            self.cleanup_locked(old)?;
        } else {
            self.cleanup_orphans(&HashSet::new())?;
        }
        for (catalog, bytes) in catalogs {
            self.ensure_catalog(&catalog.id, &bytes)?;
        }
        if let Some(old) = &old {
            self.write_pointer("previous", old.current.as_bytes())?;
        }
        self.write_heads(&heads)?;
        self.finish_heads(&heads)?;
        self.cleanup_locked(&heads)?;
        Ok(snapshot)
    }
    fn separate_path(&self, path: &Path) -> Result<()> {
        #[cfg(unix)]
        let root = self.root.clone();
        #[cfg(windows)]
        let (root, path) = {
            // Windows path comparisons must not permit overlap through case or
            // the extended-length drive spelling returned by canonicalize.
            let key = |p: &Path| {
                let text = p.to_string_lossy().replace('/', "\\");
                PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text).to_lowercase())
            };
            (key(&self.root), key(path))
        };
        require(
            !path.starts_with(&root) && !root.starts_with(path),
            "backup and store paths must not overlap",
        )
    }
}
fn write_complete(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = safe_options().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn new_backup_directory(parent: &Path) -> Result<TempDirectory> {
    for _ in 0..100 {
        let path = parent.join(format!(
            ".dw-backup-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        #[cfg(unix)]
        let creation = {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(&path)
        };
        #[cfg(windows)]
        let creation = oracle_local_ipc::create_private_directory_new(&path);
        match creation {
            Ok(()) => return Ok(TempDirectory(path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::Invalid(
        "backup staging filename space exhausted".into(),
    ))
}
fn rename_directory_new(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let source = CString::new(source.as_os_str().as_bytes())
            .map_err(|_| Error::Invalid("invalid backup path".into()))?;
        let destination = CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| Error::Invalid("invalid backup path".into()))?;
        // SAFETY: Both pointers are live NUL-terminated strings. AT_FDCWD resolves
        // absolute paths and RENAME_NOREPLACE prevents replacing a raced target.
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        oracle_local_ipc::rename_directory_new(source, destination).map_err(Error::from)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (source, destination);
        Err(Error::Invalid(
            "atomic no-replace directory backup requires Linux".into(),
        ))
    }
}
