//! Same-user local control transport. No network listener is exposed.
use std::{io, path::Path};

#[cfg(unix)]
pub use tokio::net::UnixStream as Client;
#[cfg(windows)]
pub use tokio::net::windows::named_pipe::NamedPipeClient as Client;
#[cfg(unix)]
pub type Connection = tokio::net::UnixStream;
#[cfg(windows)]
pub type Connection = tokio::net::windows::named_pipe::NamedPipeServer;

pub async fn connect(path: &Path) -> io::Result<Client> {
    #[cfg(unix)]
    {
        Client::connect(path).await
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        for _ in 0..100 {
            match ClientOptions::new().open(pipe_name(path)?) {
                Err(e) if e.raw_os_error() == Some(231) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await
                }
                Ok(client) => {
                    use std::os::windows::io::AsHandle;
                    if !security::owned_by_current_user(client.as_handle())? {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "control pipe belongs to another Windows user",
                        ));
                    }
                    return Ok(client);
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "control pipe busy"))
    }
}

pub struct Listener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(unix)]
    path: std::path::PathBuf,
    #[cfg(windows)]
    inner: Connection,
    #[cfg(windows)]
    name: String,
}
impl Listener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The host holds deployment ownership before binding.
            if path.exists() {
                std::fs::remove_file(path)?;
            }
            let inner = tokio::net::UnixListener::bind(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self {
                inner,
                path: path.into(),
            })
        }
        #[cfg(windows)]
        {
            let name = pipe_name(path)?;
            Ok(Self {
                inner: windows::create(&name, true)?,
                name,
            })
        }
    }
    pub async fn accept(&mut self) -> io::Result<Connection> {
        #[cfg(unix)]
        {
            self.inner.accept().await.map(|(stream, _)| stream)
        }
        #[cfg(windows)]
        {
            self.inner.connect().await?;
            let next = windows::create(&self.name, false)?;
            Ok(std::mem::replace(&mut self.inner, next))
        }
    }
}
#[cfg(unix)]
impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(windows)]
fn pipe_name(path: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    // State exists before server bind; clients use the same canonical directory.
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let canonical = std::fs::canonicalize(parent)?;
    let filename = path
        .file_name()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let digest = Sha256::digest(
        canonical
            .join(filename)
            .to_string_lossy()
            .to_lowercase()
            .as_bytes(),
    );
    Ok(format!(r"\\.\pipe\oracle-{:x}", digest))
}

#[cfg(windows)]
mod windows {
    use super::*;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    pub(super) fn create(name: &str, first: bool) -> io::Result<Connection> {
        let descriptor = super::security::private_descriptor("", "GA")?;
        let mut attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        // SAFETY: attrs and its allocated descriptor remain valid throughout creation.
        unsafe {
            tokio::net::windows::named_pipe::ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    (&mut attrs as *mut SECURITY_ATTRIBUTES).cast(),
                )
        }
    }
}

#[cfg(windows)]
mod filesystem;
#[cfg(windows)]
mod security;
#[cfg(windows)]
pub use filesystem::{
    atomic_publish_new, atomic_replace, create_private_directory, create_private_directory_new,
    create_private_file, durable_directory, file_identity, private_directory, private_file,
    rename_directory_new, single_link,
};

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn local_control_roundtrip_and_rebind() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("control.sock");
        let mut server = Listener::bind(&path).unwrap();
        for value in [7, 19] {
            let client = async {
                let mut client = connect(&path).await.unwrap();
                client.write_u8(value).await.unwrap();
                assert_eq!(client.read_u8().await.unwrap(), value + 1);
            };
            let responder = async {
                let mut connection = server.accept().await.unwrap();
                let value = connection.read_u8().await.unwrap();
                connection.write_u8(value + 1).await.unwrap();
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(client, responder);
            })
            .await
            .unwrap();
        }
        drop(server);
        assert!(connect(&path).await.is_err());
        let _replacement = Listener::bind(&path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn private_directories_and_hard_links_are_checked() {
        let root = tempfile::tempdir().unwrap();
        let private = root.path().join("private");
        create_private_directory(&private).unwrap();
        assert!(private_directory(&private).unwrap());
        create_private_directory(&private.join("child")).unwrap();
        assert!(private_directory(&private.join("child")).unwrap());
        assert!(create_private_directory(&private).is_ok());
        assert!(!private_directory(root.path()).unwrap());
        let file = private.join("file");
        std::fs::write(&file, "content").unwrap();
        assert!(single_link(&file).unwrap());
        std::fs::hard_link(&file, private.join("alias")).unwrap();
        assert!(!single_link(&file).unwrap());
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn pipe_paths_are_distinct() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.sock");
        let second = root.path().join("second.sock");
        let _first_server = Listener::bind(&first).unwrap();
        let _second_server = Listener::bind(&second).unwrap();
        assert_ne!(pipe_name(&first).unwrap(), pipe_name(&second).unwrap());
    }

    // Run on native Windows. Wine 10 ignores FILE_FLAG_FIRST_PIPE_INSTANCE
    // in CreateNamedPipeW; Wine checks explicitly skip this OS-contract test.
    #[cfg(windows)]
    #[tokio::test]
    async fn active_listener_cannot_be_replaced() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("control.sock");
        let _server = Listener::bind(&path).unwrap();
        assert!(Listener::bind(&path).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn private_directory_rejects_embedded_nul_before_creation() {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap();
        let prefix = root.path().join("must-not-be-created");
        let mut wide: Vec<u16> = prefix.as_os_str().encode_wide().collect();
        wide.extend([0, 65]);
        let path = std::path::PathBuf::from(std::ffi::OsString::from_wide(&wide));
        assert_eq!(
            create_private_directory(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!prefix.exists());
    }
    #[cfg(windows)]
    #[test]
    fn private_files_and_atomic_publication_preserve_boundaries() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("private");
        create_private_directory_new(&directory).unwrap();
        assert_eq!(
            create_private_directory_new(&directory).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let destination = directory.join("published");
        let source = directory.join("pending");
        for (path, contents) in [(&destination, b"old"), (&source, b"new")] {
            let mut file = create_private_file(path).unwrap();
            file.write_all(contents).unwrap();
            file.sync_all().unwrap();
        }
        assert!(private_file(&source).unwrap());
        assert!(create_private_file(&source).is_err());
        atomic_replace(&source, &destination).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"new");
        assert!(!source.exists());
        assert!(private_file(&destination).unwrap());
        let alias = directory.join("alias");
        std::fs::hard_link(&destination, &alias).unwrap();
        assert_eq!(
            file_identity(&destination).unwrap(),
            file_identity(&alias).unwrap()
        );
        assert!(!private_file(&destination).unwrap());
        let immutable = directory.join("immutable");
        drop(create_private_file(&source).unwrap());
        assert_ne!(
            file_identity(&source).unwrap(),
            file_identity(&destination).unwrap()
        );
        atomic_publish_new(&source, &immutable).unwrap();
        assert!(!source.exists());
        drop(create_private_file(&source).unwrap());
        assert!(atomic_publish_new(&source, &immutable).is_err());
        assert!(source.is_file());
        let staging = directory.join("staging");
        let published = directory.join("directory");
        create_private_directory_new(&staging).unwrap();
        rename_directory_new(&staging, &published).unwrap();
        create_private_directory_new(&staging).unwrap();
        assert!(rename_directory_new(&staging, &published).is_err());
        assert!(staging.is_dir());
        assert!(published.is_dir());
        durable_directory(&directory).unwrap();
        assert!(durable_directory(&destination).is_err());
    }
}
