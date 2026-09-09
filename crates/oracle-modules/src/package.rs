//! Static, local installation of operator-trusted native ELF packages. Never executes code.
use oracle_core::{
    Error, ErrorCode, InstalledModule, ModuleManifest, ModuleOperation, ModulePackage, Result,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

pub const HOST_TARGET: &str = env!("ORACLE_TARGET");
const MAX_PACKAGE: u64 = 512 * 1024;
const MAX_FILE: u64 = 128 * 1024 * 1024;
const MAX_TOTAL: u64 = 256 * 1024 * 1024;
fn err(code: ErrorCode) -> Error {
    Error::new(code)
}
fn io(_: std::io::Error) -> Error {
    err(ErrorCode::Io)
}
fn changed(_: std::io::Error) -> Error {
    err(ErrorCode::ArtifactChanged)
}
fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && !value.contains("..")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/@".contains(&b))
}
fn unique<'a>(mut values: impl Iterator<Item = &'a str>) -> bool {
    let mut set = BTreeSet::new();
    values.all(|value| set.insert(value))
}
fn allowed_capability(value: &str) -> bool {
    matches!(
        value,
        "storage.own" | "contracts.invoke" | "host.echo" | "config.own"
    )
}

/// Reject every external reference before passing a schema to the offline-only validator.
pub fn schema_validator(schema: &Value) -> Result<jsonschema::Validator> {
    fn local(value: &Value, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        match value {
            Value::Object(map) => map.iter().all(|(key, value)| {
                if matches!(
                    key.as_str(),
                    "$ref" | "$dynamicRef" | "$recursiveRef" | "$id"
                ) {
                    return value.as_str().is_some_and(|s| s.starts_with('#'));
                }
                if key == "$schema" {
                    return value.as_str().is_some_and(|s| {
                        matches!(
                            s,
                            "https://json-schema.org/draft/2020-12/schema"
                                | "https://json-schema.org/draft/2019-09/schema"
                                | "http://json-schema.org/draft-07/schema#"
                                | "http://json-schema.org/draft-06/schema#"
                                | "http://json-schema.org/draft-04/schema#"
                        )
                    });
                }
                local(value, depth + 1)
            }),
            Value::Array(values) => values.iter().all(|v| local(v, depth + 1)),
            _ => true,
        }
    }
    if serde_json::to_vec(schema)
        .map_err(|_| err(ErrorCode::SchemaInvalid))?
        .len()
        > MAX_PACKAGE as usize
        || !local(schema, 0)
    {
        return Err(err(ErrorCode::SchemaInvalid));
    }
    jsonschema::options()
        .should_validate_formats(true)
        .build(schema)
        .map_err(|_| err(ErrorCode::SchemaInvalid))
}
pub fn validate_input(operation: &ModuleOperation, input: &Value) -> Result<()> {
    validate_schema(&operation.input_schema, input)
}
pub fn validate_output(operation: &ModuleOperation, output: &Value) -> Result<()> {
    validate_schema(&operation.output_schema, output)
}
fn validate_schema(schema: &Value, value: &Value) -> Result<()> {
    if serde_json::to_vec(value)
        .map_err(|_| err(ErrorCode::InvalidInput))?
        .len()
        > 512 * 1024
    {
        return Err(err(ErrorCode::QuotaExceeded));
    }
    schema_validator(schema)?
        .validate(value)
        .map_err(|_| err(ErrorCode::SchemaInvalid))
}
pub fn validate_manifest(manifest: &ModuleManifest) -> Result<()> {
    if manifest.manifest_version != 1
        || manifest.protocol_major != 1
        || manifest.protocol_minor_min != 0
        || manifest.target != HOST_TARGET
    {
        return Err(err(ErrorCode::Compatibility));
    }
    semver::Version::parse(&manifest.version).map_err(|_| err(ErrorCode::Compatibility))?;
    let host =
        semver::VersionReq::parse(&manifest.host_api).map_err(|_| err(ErrorCode::Compatibility))?;
    if !host.matches(&semver::Version::new(1, 0, 0)) {
        return Err(err(ErrorCode::Compatibility));
    }
    if manifest.capabilities.len() > 16
        || !unique(manifest.capabilities.iter().map(String::as_str))
        || manifest.capabilities.iter().any(|c| !allowed_capability(c))
        || manifest.required_intents.iter().any(|v| v != "guilds")
        || !unique(manifest.required_intents.iter().map(String::as_str))
    {
        return Err(err(ErrorCode::Compatibility));
    }
    if manifest.data_version > 1024
        || manifest.readable_data_versions.len() > 1025
        || !manifest
            .readable_data_versions
            .contains(&manifest.data_version)
        || manifest
            .readable_data_versions
            .iter()
            .any(|v| *v > manifest.data_version)
        || manifest
            .readable_data_versions
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != manifest.readable_data_versions.len()
    {
        return Err(err(ErrorCode::DataVersionMismatch));
    }
    if manifest.operations.len() > 64
        || manifest.collections.len() > 32
        || manifest.provides.len() > 64
        || manifest.consumes.len() > 64
        || manifest.migrations.len() > 1024
    {
        return Err(err(ErrorCode::QuotaExceeded));
    }
    if !unique(manifest.operations.iter().map(|v| v.name.as_str()))
        || !unique(manifest.collections.iter().map(|v| v.name.as_str()))
        || !unique(manifest.provides.iter().map(|v| v.name.as_str()))
        || !unique(manifest.consumes.iter().map(|v| v.name.as_str()))
    {
        return Err(err(ErrorCode::InvalidInput));
    }
    if let Some(configuration) = &manifest.configuration {
        if configuration.schema_version == 0
            || configuration.presets.len() > 32
            || !manifest.capabilities.iter().any(|c| c == "config.own")
        {
            return Err(err(ErrorCode::InvalidInput));
        }
        schema_validator(&configuration.schema)?;
        for (preset, values) in &configuration.presets {
            if !name(preset)
                || !values.is_object()
                || serde_json::to_vec(values)
                    .map_err(|_| err(ErrorCode::InvalidInput))?
                    .len()
                    > 65536
            {
                return Err(err(ErrorCode::InvalidInput));
            }
        }
    }
    for operation in &manifest.operations {
        if !name(&operation.name)
            || operation.description.len() > 2048
            || operation.timeout_ms == 0
            || operation.timeout_ms > 300_000
            || operation
                .capabilities
                .iter()
                .any(|c| !manifest.capabilities.contains(c))
            || !unique(operation.capabilities.iter().map(String::as_str))
        {
            return Err(err(ErrorCode::InvalidInput));
        }
        schema_validator(&operation.input_schema)?;
        schema_validator(&operation.output_schema)?;
    }
    for collection in &manifest.collections {
        if !name(&collection.name) {
            return Err(err(ErrorCode::InvalidInput));
        }
        schema_validator(&collection.schema)?;
    }
    for provided in &manifest.provides {
        if !name(&provided.name)
            || !manifest
                .operations
                .iter()
                .any(|v| v.name == provided.operation)
        {
            return Err(err(ErrorCode::InvalidInput));
        }
        semver::Version::parse(&provided.version).map_err(|_| err(ErrorCode::Compatibility))?;
    }
    for consumed in &manifest.consumes {
        if !name(&consumed.name) {
            return Err(err(ErrorCode::InvalidInput));
        }
        semver::VersionReq::parse(&consumed.version).map_err(|_| err(ErrorCode::Compatibility))?;
    }
    let mut from = BTreeSet::new();
    for migration in &manifest.migrations {
        if migration.from.checked_add(1) != Some(migration.to)
            || migration.to > manifest.data_version
            || !from.insert(migration.from)
            || !manifest
                .operations
                .iter()
                .any(|v| v.name == migration.operation)
        {
            return Err(err(ErrorCode::DataVersionMismatch));
        }
    }
    Ok(())
}
fn relative(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 512
        || value.contains('\\')
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path.components().count() > 16
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(err(ErrorCode::InvalidInput));
    }
    Ok(path.to_owned())
}
fn digest_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn canonical(package: &ModulePackage) -> Result<Vec<u8>> {
    serde_json::to_vec(package).map_err(|_| err(ErrorCode::InvalidInput))
}
fn package_digest(package: &ModulePackage) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(canonical(package)?)))
}
fn validate_package(package: &ModulePackage) -> Result<()> {
    if canonical(package)?.len() > MAX_PACKAGE as usize {
        return Err(err(ErrorCode::QuotaExceeded));
    }
    validate_manifest(&package.manifest)?;
    if package.files.is_empty() || package.files.len() > 64 {
        return Err(err(ErrorCode::QuotaExceeded));
    }
    relative(&package.entrypoint)?;
    if !package.files.contains_key(&package.entrypoint) {
        return Err(err(ErrorCode::InvalidInput));
    }
    for (path, digest) in &package.files {
        relative(path)?;
        if path == "package.json" || !digest_valid(digest) {
            return Err(err(ErrorCode::InvalidInput));
        }
    }
    for value in [
        &package.source_revision,
        &package.toolchain,
        &package.license,
    ] {
        if value.is_empty() || value.len() > 1024 {
            return Err(err(ErrorCode::InvalidInput));
        }
    }
    Ok(())
}
fn regular(path: &Path, limit: u64) -> Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path).map_err(changed)?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(err(ErrorCode::ArtifactChanged));
    }
    if meta.len() > limit {
        return Err(err(ErrorCode::QuotaExceeded));
    }
    Ok(meta)
}
fn read_package(root: &Path) -> Result<ModulePackage> {
    regular(&root.join("package.json"), MAX_PACKAGE)?;
    let mut bytes = Vec::new();
    fs::File::open(root.join("package.json"))
        .map_err(changed)?
        .take(MAX_PACKAGE + 1)
        .read_to_end(&mut bytes)
        .map_err(changed)?;
    if bytes.len() > MAX_PACKAGE as usize {
        return Err(err(ErrorCode::QuotaExceeded));
    }
    serde_json::from_slice(&bytes).map_err(|_| err(ErrorCode::InvalidInput))
}
fn inventory(root: &Path, package: &ModulePackage, readonly: bool) -> Result<()> {
    let mut pending = vec![(root.to_owned(), PathBuf::new())];
    let mut found = BTreeSet::new();
    let mut total = 0u64;
    let mut count = 0;
    while let Some((directory, relative_dir)) = pending.pop() {
        let metadata = fs::symlink_metadata(&directory).map_err(changed)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || (readonly && metadata.mode() & 0o222 != 0)
        {
            return Err(err(ErrorCode::ArtifactChanged));
        }
        for entry in fs::read_dir(&directory).map_err(changed)? {
            count += 1;
            if count > 256 {
                return Err(err(ErrorCode::QuotaExceeded));
            }
            let entry = entry.map_err(changed)?;
            let rel = relative_dir.join(entry.file_name());
            let text = rel.to_str().ok_or_else(|| err(ErrorCode::InvalidInput))?;
            relative(text)?;
            let meta = fs::symlink_metadata(entry.path()).map_err(changed)?;
            if meta.file_type().is_symlink() {
                return Err(err(ErrorCode::ArtifactChanged));
            }
            if meta.is_dir() {
                if !package
                    .files
                    .keys()
                    .any(|name| name.starts_with(&format!("{text}/")))
                {
                    return Err(err(ErrorCode::ArtifactChanged));
                }
                pending.push((entry.path(), rel));
                continue;
            }
            let limit = if text == "package.json" {
                MAX_PACKAGE
            } else {
                MAX_FILE
            };
            regular(&entry.path(), limit)?;
            total = total
                .checked_add(meta.len())
                .ok_or_else(|| err(ErrorCode::QuotaExceeded))?;
            if total > MAX_TOTAL {
                return Err(err(ErrorCode::QuotaExceeded));
            }
            if readonly
                && (meta.mode() & 0o7777
                    != if text == package.entrypoint {
                        0o555
                    } else {
                        0o444
                    })
            {
                return Err(err(ErrorCode::ArtifactChanged));
            }
            if text != "package.json" {
                if !package.files.contains_key(text) {
                    return Err(err(ErrorCode::ArtifactChanged));
                }
                found.insert(text.to_owned());
            }
        }
    }
    if found.len() != package.files.len() {
        return Err(err(ErrorCode::ArtifactChanged));
    }
    Ok(())
}
fn native_elf(header: &[u8]) -> bool {
    let machine = if HOST_TARGET.starts_with("x86_64-") {
        62u16
    } else if HOST_TARGET.starts_with("aarch64-") {
        183u16
    } else {
        return false;
    };
    header.len() >= 64
        && &header[..4] == b"\x7fELF"
        && header[4] == 2
        && header[5] == 1
        && header[6] == 1
        && matches!(u16::from_le_bytes([header[16], header[17]]), 2 | 3)
        && u16::from_le_bytes([header[18], header[19]]) == machine
        && u32::from_le_bytes([header[20], header[21], header[22], header[23]]) == 1
}
fn checked_copy(source: &Path, target: Option<&Path>, expected: &str, entry: bool) -> Result<()> {
    regular(source, MAX_FILE)?;
    let mut source = fs::File::open(source).map_err(changed)?;
    let mut target = target
        .map(|p| fs::OpenOptions::new().write(true).create_new(true).open(p))
        .transpose()
        .map_err(io)?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut prefix = Vec::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = source.read(&mut buffer).map_err(changed)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_FILE {
            return Err(err(ErrorCode::QuotaExceeded));
        }
        if prefix.len() < 64 {
            prefix.extend_from_slice(&buffer[..n.min(64 - prefix.len())]);
        }
        hash.update(&buffer[..n]);
        if let Some(target) = &mut target {
            target.write_all(&buffer[..n]).map_err(io)?;
        }
    }
    if format!("{:x}", hash.finalize()) != expected {
        return Err(err(ErrorCode::ArtifactChanged));
    }
    if entry && !native_elf(&prefix) {
        return Err(err(ErrorCode::Compatibility));
    }
    if let Some(target) = target {
        target.sync_all().map_err(io)?;
    }
    Ok(())
}
fn permissions(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(io)
}
fn remove_tree(path: &Path) -> Result<()> {
    // Never follow links during cleanup, including a tampered staging directory.
    let meta = fs::symlink_metadata(path).map_err(io)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return fs::remove_file(path).map_err(io);
    }
    permissions(path, 0o700)?;
    for entry in fs::read_dir(path).map_err(io)? {
        remove_tree(&entry.map_err(io)?.path())?;
    }
    fs::remove_dir(path).map_err(io)
}
struct Stage(PathBuf);
impl Drop for Stage {
    fn drop(&mut self) {
        if self.0.exists() {
            let _ = remove_tree(&self.0);
        }
    }
}
pub struct ArtifactStore {
    root: PathBuf,
}
impl ArtifactStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root).map_err(io)?;
        if fs::symlink_metadata(&root)
            .map_err(io)?
            .file_type()
            .is_symlink()
        {
            return Err(err(ErrorCode::InvalidInput));
        }
        Ok(Self {
            root: fs::canonicalize(root).map_err(io)?,
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn install(&self, source_dir: &Path, trusted: bool) -> Result<InstalledModule> {
        if !trusted {
            return Err(err(ErrorCode::TrustedCodeRequired));
        }
        let package = read_package(source_dir)?;
        validate_package(&package)?;
        inventory(source_dir, &package, false)?;
        let installed = InstalledModule {
            digest: package_digest(&package)?,
            package,
        };
        let destination = self.root.join(&installed.digest);
        if fs::symlink_metadata(&destination).is_ok() {
            self.verify(&installed)?;
            return Ok(installed);
        }
        let stage = Stage(self.root.join(format!(".install-{}", uuid::Uuid::new_v4())));
        fs::create_dir(&stage.0).map_err(io)?;
        let mut directories = BTreeSet::from([stage.0.clone()]);
        for (name, digest) in &installed.package.files {
            let target = stage.0.join(name);
            let parent = target
                .parent()
                .ok_or_else(|| err(ErrorCode::InvalidInput))?;
            fs::create_dir_all(parent).map_err(io)?;
            let mut dir = parent;
            while dir.starts_with(&stage.0) {
                directories.insert(dir.to_owned());
                let Some(parent) = dir.parent() else { break };
                dir = parent;
            }
            checked_copy(
                &source_dir.join(name),
                Some(&target),
                digest,
                name == &installed.package.entrypoint,
            )?;
            permissions(
                &target,
                if name == &installed.package.entrypoint {
                    0o555
                } else {
                    0o444
                },
            )?;
        }
        let metadata = stage.0.join("package.json");
        fs::write(&metadata, canonical(&installed.package)?).map_err(io)?;
        fs::File::open(&metadata)
            .map_err(io)?
            .sync_all()
            .map_err(io)?;
        permissions(&metadata, 0o444)?;
        for directory in directories.iter().rev() {
            permissions(directory, 0o555)?;
        }
        inventory(&stage.0, &installed.package, true)?;
        match fs::rename(&stage.0, &destination) {
            Ok(()) => {}
            Err(_) if destination.exists() => {
                self.verify(&installed)?;
            }
            Err(error) => return Err(io(error)),
        }
        fs::File::open(&self.root)
            .map_err(io)?
            .sync_all()
            .map_err(io)?;
        self.verify(&installed)?;
        Ok(installed)
    }
    pub fn verify(&self, installed: &InstalledModule) -> Result<PathBuf> {
        validate_package(&installed.package)?;
        if !digest_valid(&installed.digest)
            || package_digest(&installed.package)? != installed.digest
        {
            return Err(err(ErrorCode::ArtifactChanged));
        }
        let root = self.root.join(&installed.digest);
        inventory(&root, &installed.package, true)?;
        if read_package(&root)? != installed.package {
            return Err(err(ErrorCode::ArtifactChanged));
        }
        for (name, digest) in &installed.package.files {
            checked_copy(
                &root.join(name),
                None,
                digest,
                name == &installed.package.entrypoint,
            )?;
        }
        Ok(root.join(&installed.package.entrypoint))
    }
    pub fn remove(&self, installed: &InstalledModule) -> Result<()> {
        self.verify(installed)?;
        let removed = self.root.join(format!(".remove-{}", uuid::Uuid::new_v4()));
        fs::rename(self.root.join(&installed.digest), &removed).map_err(io)?;
        remove_tree(&removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("oracle-package-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = remove_tree(&self.0);
        }
    }
    fn fixture() -> (Temp, ModulePackage) {
        let temp = Temp::new();
        fs::create_dir(temp.0.join("source")).unwrap();
        let bytes = fs::read("/bin/true").unwrap();
        fs::write(temp.0.join("source/module"), &bytes).unwrap();
        let package:ModulePackage=serde_json::from_value(json!({
            "manifest":{"manifest_version":1,"id":"test.module","version":"1.0.0","target":HOST_TARGET,
                "protocol_major":1,"protocol_minor_min":0,"host_api":"^1.0.0","data_version":1,
                "readable_data_versions":[1],"capabilities":["storage.own"],"required_intents":["guilds"],
                "operations":[{"name":"echo@1","description":"Echo","input_schema":{"type":"object","properties":{"id":{"type":"string"}},"additionalProperties":false},"output_schema":{"type":"object"},"timeout_ms":1000}]},
            "entrypoint":"module","files":{"module":format!("{:x}",Sha256::digest(&bytes))},
            "source_revision":"fixture","toolchain":"system fixture","license":"fixture"
        })).unwrap();
        write(&temp, &package);
        (temp, package)
    }
    fn write(temp: &Temp, package: &ModulePackage) {
        fs::write(
            temp.0.join("source/package.json"),
            serde_json::to_vec(package).unwrap(),
        )
        .unwrap();
    }
    #[test]
    fn static_install_reuses_digest_verifies_permissions_and_removes() {
        let (temp, _) = fixture();
        let store = ArtifactStore::new(temp.0.join("store")).unwrap();
        let first = store.install(&temp.0.join("source"), true).unwrap();
        let entry = store.verify(&first).unwrap();
        assert_eq!(fs::metadata(&entry).unwrap().mode() & 0o7777, 0o555);
        assert_eq!(store.install(&temp.0.join("source"), true).unwrap(), first);
        store.remove(&first).unwrap();
        assert!(!entry.exists());
    }
    #[test]
    fn explicit_trust_and_compatibility_are_required() {
        let (temp, mut package) = fixture();
        let store = ArtifactStore::new(temp.0.join("store")).unwrap();
        assert_eq!(
            store
                .install(&temp.0.join("source"), false)
                .unwrap_err()
                .code,
            ErrorCode::TrustedCodeRequired
        );
        package.manifest.protocol_major = 2;
        write(&temp, &package);
        assert_eq!(
            store
                .install(&temp.0.join("source"), true)
                .unwrap_err()
                .code,
            ErrorCode::Compatibility
        );
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 0);
    }
    #[test]
    fn inventory_tampering_traversal_links_and_unlisted_files_are_rejected() {
        let (temp, mut package) = fixture();
        let store = ArtifactStore::new(temp.0.join("store")).unwrap();
        let source = temp.0.join("source");
        package.entrypoint = "../escape".into();
        write(&temp, &package);
        assert!(store.install(&source, true).is_err());
        package.entrypoint = "module".into();
        write(&temp, &package);
        fs::write(source.join("extra"), b"extra").unwrap();
        assert!(store.install(&source, true).is_err());
        fs::remove_file(source.join("extra")).unwrap();
        fs::rename(source.join("module"), temp.0.join("real-module")).unwrap();
        std::os::unix::fs::symlink(temp.0.join("real-module"), source.join("module")).unwrap();
        assert!(store.install(&source, true).is_err());
        fs::remove_file(source.join("module")).unwrap();
        fs::hard_link(temp.0.join("real-module"), source.join("module")).unwrap();
        assert!(store.install(&source, true).is_err());
        fs::remove_file(source.join("module")).unwrap();
        fs::rename(temp.0.join("real-module"), source.join("module")).unwrap();
        let installed = store.install(&source, true).unwrap();
        let entry = store.verify(&installed).unwrap();
        permissions(&entry, 0o755).unwrap();
        fs::write(&entry, b"tamper").unwrap();
        permissions(&entry, 0o555).unwrap();
        assert_eq!(
            store.verify(&installed).unwrap_err().code,
            ErrorCode::ArtifactChanged
        );
    }
    #[test]
    fn scripts_are_rejected_without_running_install_hooks() {
        let (temp, mut package) = fixture();
        let marker = temp.0.join("executed");
        let script = format!("#!/bin/sh\ntouch '{}'\n", marker.display());
        fs::write(temp.0.join("source/module"), &script).unwrap();
        permissions(&temp.0.join("source/module"), 0o755).unwrap();
        package.files.insert(
            "module".into(),
            format!("{:x}", Sha256::digest(script.as_bytes())),
        );
        write(&temp, &package);
        let store = ArtifactStore::new(temp.0.join("store")).unwrap();
        assert_eq!(
            store
                .install(&temp.0.join("source"), true)
                .unwrap_err()
                .code,
            ErrorCode::Compatibility
        );
        assert!(!marker.exists());
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 0);
    }
    #[test]
    fn schemas_are_offline_validated_and_enforced() {
        assert!(schema_validator(&json!({"$ref":"https://example.invalid/schema"})).is_err());
        assert!(schema_validator(&json!({"$ref":"file:///etc/passwd"})).is_err());
        assert!(schema_validator(&json!({"type":"nonsense"})).is_err());
        let local = json!({"$defs":{"x":{"type":"integer"}},"$ref":"#/$defs/x"});
        assert!(schema_validator(&local).unwrap().is_valid(&json!(1)));
        assert!(!schema_validator(&local).unwrap().is_valid(&json!("1")));
        let (_temp, package) = fixture();
        let operation = &package.manifest.operations[0];
        assert!(validate_input(operation, &json!({"id":"123"})).is_ok());
        assert!(validate_input(operation, &json!({"id":123})).is_err());
    }
}
