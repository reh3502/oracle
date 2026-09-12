//! Operator-owned native runtime paths and revision-link policy. These settings
//! never come from module manifests or invocation input.
use oracle_core::{Error, ErrorCode, ModuleId, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::ErrorKind,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModuleRuntimeSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citation_prefix: Option<String>,
}

fn invalid() -> Error {
    Error::new(ErrorCode::InvalidInput)
}
fn io(_: std::io::Error) -> Error {
    Error::new(ErrorCode::Io)
}
fn overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
fn normalized(path: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };
    path.is_absolute()
        && text.len() <= 4096
        && !text.chars().any(char::is_control)
        && path.components().count() <= 64
        && path.components().all(|c|matches!(c, Component::RootDir | Component::Normal(_)))
        // Components erase interior `.` and duplicate separators; reject them too.
        && text.split('/').skip(1).all(|part| !part.is_empty() && part != "." && part != "..")
}

/// A fixed HTTPS prefix accepts only a host and an unescaped, ordinary path,
/// followed by one numeric revision parameter. No module-supplied URL is used.
pub fn validate_citation_prefix(prefix: &str) -> Result<()> {
    if prefix.len() > 2048 || !prefix.is_ascii() {
        return Err(invalid());
    }
    let authority_path = prefix
        .strip_prefix("https://")
        .and_then(|p| p.strip_suffix("?oldid="))
        .ok_or_else(invalid)?;
    let (host, path) = authority_path.split_once('/').ok_or_else(invalid)?;
    if host.len() > 253
        || host.is_empty()
        || path.is_empty()
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._~-".contains(&b))
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(invalid());
    }
    Ok(())
}

/// Validate every setting before creating anything, then prepare private data
/// directories. `denied_paths` names host credentials, configuration, databases,
/// control files or other reserved roots; files need not exist yet. The caller
/// supplies the trusted host-state owner rather than trusting module metadata.
///
/// Ancestors must be owned by this user or root and cannot be group/world writable
/// except root-owned sticky directories (such as /tmp). The host and its native
/// modules run as an operator-trusted user; this is not a sandbox against that user.
pub fn prepare_runtime_settings(
    settings: &BTreeMap<ModuleId, ModuleRuntimeSettings>,
    installed_package_root: &Path,
    denied_paths: &[PathBuf],
    expected_uid: u32,
) -> Result<BTreeMap<ModuleId, ModuleRuntimeSettings>> {
    if settings.len() > 128 || denied_paths.len() > 256 {
        return Err(invalid());
    }
    if !settings
        .values()
        .any(|setting| setting.data_directory.is_some())
    {
        for setting in settings.values() {
            if let Some(prefix) = &setting.citation_prefix {
                validate_citation_prefix(prefix)?;
            }
        }
        return Ok(settings.clone());
    }
    let mut denied = Vec::with_capacity(denied_paths.len() + 1);
    for path in
        std::iter::once(installed_package_root).chain(denied_paths.iter().map(PathBuf::as_path))
    {
        if !normalized(path) {
            return Err(invalid());
        }
        inspect_components(path, expected_uid, false, false)?;
        denied.push(path);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut directories = Vec::new();
    for setting in settings.values() {
        if let Some(prefix) = &setting.citation_prefix {
            validate_citation_prefix(prefix)?;
        }
        if let Some(path) = &setting.data_directory {
            if !normalized(path)
                || path.components().count() < 3
                || home.as_ref().is_some_and(|h| path == h)
                || denied.iter().any(|denied| overlap(path, denied))
                || directories
                    .iter()
                    .any(|other: &PathBuf| overlap(path, other))
            {
                return Err(invalid());
            }
            inspect_components(path, expected_uid, true, true)?;
            directories.push(path.clone());
        }
    }
    let mut created = Vec::new();
    let result = (|| {
        for path in &directories {
            // Recheck before each mutation, including shared parents another
            // directory preparation may just have created.
            inspect_components(path, expected_uid, true, true)?;
            let mut cursor = PathBuf::new();
            for component in path.components() {
                cursor.push(component.as_os_str());
                match fs::symlink_metadata(&cursor) {
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {
                        let mut builder = fs::DirBuilder::new();
                        builder.mode(0o700);
                        match builder.create(&cursor) {
                            Ok(()) => created.push(cursor.clone()),
                            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                            Err(e) => return Err(io(e)),
                        }
                    }
                    Err(e) => return Err(io(e)),
                }
                inspect_components(&cursor, expected_uid, false, true)?;
            }
            inspect_components(path, expected_uid, true, true)?;
            if fs::canonicalize(path).map_err(io)? != *path {
                return Err(invalid());
            }
        }
        // A failure never returns partially validated settings. Existing entries
        // are not chmodded or otherwise changed by preparation.
        for path in &directories {
            inspect_components(path, expected_uid, true, true)?;
            if fs::canonicalize(path).map_err(io)? != *path {
                return Err(invalid());
            }
        }
        Ok(settings.clone())
    })();
    if result.is_err() {
        for path in created.iter().rev() {
            let _ = fs::remove_dir(path);
        }
    }
    result
}

fn inspect_components(
    path: &Path,
    expected_uid: u32,
    dedicated: bool,
    trusted_ancestors: bool,
) -> Result<()> {
    let mut cursor = PathBuf::new();
    let mut missing = 0;
    for component in path.components() {
        cursor.push(component.as_os_str());
        match fs::symlink_metadata(&cursor) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(invalid());
                }
                if (cursor != path || dedicated)
                    && (!meta.is_dir()
                        || (trusted_ancestors
                            && ((meta.uid() != expected_uid && meta.uid() != 0)
                                || (meta.mode() & 0o022 != 0
                                    && !(meta.uid() == 0 && meta.mode() & 0o1000 != 0)))))
                {
                    return Err(invalid());
                }
                if cursor == path
                    && dedicated
                    && (meta.uid() != expected_uid || meta.mode() & 0o7777 != 0o700)
                {
                    return Err(invalid());
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                missing += 1;
                if missing > 8 {
                    return Err(invalid());
                }
            }
            Err(e) => return Err(io(e)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("oracle-runtime-{}", uuid::Uuid::new_v4()));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        fn uid(&self) -> u32 {
            fs::metadata(&self.0).unwrap().uid()
        }
        fn settings(&self, path: PathBuf) -> BTreeMap<ModuleId, ModuleRuntimeSettings> {
            BTreeMap::from([(
                serde_json::from_str("\"game.test\"").unwrap(),
                ModuleRuntimeSettings {
                    data_directory: Some(path),
                    citation_prefix: Some("https://wiki.example.test/w/index.php?oldid=".into()),
                },
            )])
        }
        fn prepare(
            &self,
            settings: &BTreeMap<ModuleId, ModuleRuntimeSettings>,
        ) -> Result<BTreeMap<ModuleId, ModuleRuntimeSettings>> {
            prepare_runtime_settings(
                settings,
                &self.0.join("packages"),
                &[self.0.join("config.toml"), self.0.join("database")],
                self.uid(),
            )
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn empty_settings_preserve_legacy_startup_without_path_preparation() {
        let settings = BTreeMap::new();
        assert_eq!(
            prepare_runtime_settings(&settings, Path::new("unused-relative"), &[], 0).unwrap(),
            settings
        );
    }
    #[test]
    fn prepares_private_nested_directory_and_reuses_it_without_changing_settings() {
        let temp = Temp::new();
        let path = temp.0.join("data/game");
        let settings = temp.settings(path.clone());
        assert_eq!(temp.prepare(&settings).unwrap(), settings);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o700);
        assert_eq!(temp.prepare(&settings).unwrap(), settings);
    }
    #[test]
    fn validates_entire_map_before_any_creation() {
        let temp = Temp::new();
        let new = temp.0.join("new/game");
        let mut settings = temp.settings(new.clone());
        settings.insert(
            serde_json::from_str("\"other.game\"").unwrap(),
            ModuleRuntimeSettings {
                data_directory: Some(temp.0.join("packages/game")),
                citation_prefix: None,
            },
        );
        assert!(temp.prepare(&settings).is_err());
        assert!(!new.parent().unwrap().exists());
    }
    #[test]
    fn rejects_path_aliases_symlinks_reserved_paths_and_cross_module_overlap() {
        let temp = Temp::new();
        for path in [
            PathBuf::from("relative"),
            PathBuf::from("/"),
            temp.0.join("data/../game"),
            temp.0.join("./game"),
            temp.0.join("database"),
            temp.0.join("config.toml/data"),
            temp.0.clone(),
        ] {
            assert!(
                temp.prepare(&temp.settings(path.clone())).is_err(),
                "accepted {}",
                path.display()
            );
        }
        fs::create_dir(temp.0.join("real")).unwrap();
        symlink(temp.0.join("real"), temp.0.join("link")).unwrap();
        assert!(
            temp.prepare(&temp.settings(temp.0.join("link/game")))
                .is_err()
        );
        let mut settings = temp.settings(temp.0.join("data/game"));
        settings.insert(
            serde_json::from_str("\"other.game\"").unwrap(),
            ModuleRuntimeSettings {
                data_directory: Some(temp.0.join("data")),
                citation_prefix: None,
            },
        );
        assert!(temp.prepare(&settings).is_err());
        assert!(!temp.0.join("data").exists());
    }
    #[test]
    fn refuses_existing_public_or_wrong_owner_directory_without_chmod() {
        let temp = Temp::new();
        let path = temp.0.join("data");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(temp.prepare(&temp.settings(path.clone())).is_err());
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o755);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            prepare_runtime_settings(
                &temp.settings(path),
                &temp.0.join("packages"),
                &[],
                temp.uid().wrapping_add(1)
            )
            .is_err()
        );
    }
    #[test]
    fn refuses_writable_data_ancestors_and_excessive_missing_depth() {
        let temp = Temp::new();
        let parent = temp.0.join("shared");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(temp.prepare(&temp.settings(parent.join("game"))).is_err());
        assert!(!parent.join("game").exists());
        let deep = temp.0.join("a/b/c/d/e/f/g/h/i");
        assert!(temp.prepare(&temp.settings(deep)).is_err());
        assert!(!temp.0.join("a").exists());
    }
    #[test]
    fn private_data_can_be_separate_from_writable_reserved_path_ancestors() {
        let temp = Temp::new();
        let other = temp.0.join("other");
        fs::create_dir(&other).unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o775)).unwrap();
        let settings = temp.settings(temp.0.join("data"));
        assert_eq!(
            prepare_runtime_settings(
                &settings,
                &other.join("packages"),
                &[other.join("config.toml")],
                temp.uid()
            )
            .unwrap(),
            settings
        );
    }
    #[test]
    fn citation_prefix_is_a_single_operator_owned_https_revision_link() {
        validate_citation_prefix("https://wiki.example.test/w/index.php?oldid=").unwrap();
        for prefix in [
            "http://wiki.example.test/index.php?oldid=",
            "https://u:p@wiki.example.test/index.php?oldid=",
            "https://wiki.example.test/index.php?a=1&oldid=",
            "https://wiki.example.test/index.php?oldid=#",
            "https://wiki.example.test/%2findex.php?oldid=",
            "https://wiki.example.test/../index.php?oldid=",
            "https://wiki.example.test/index.php\n?oldid=",
            "https://wiki.example.test/index.php?other=",
        ] {
            assert!(
                validate_citation_prefix(prefix).is_err(),
                "accepted {prefix:?}"
            );
        }
        assert!(
            serde_json::from_str::<ModuleRuntimeSettings>(
                r#"{"data_directory":"/tmp/data","destination":"channel"}"#
            )
            .is_err()
        );
    }
}
