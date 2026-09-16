//! Windows filesystem checks used by the native package boundary.
use crate::security::{Descriptor, private_descriptor};
use std::{
    io,
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle},
    },
    path::Path,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
    Security::Authorization::*,
    Security::*,
    Storage::FileSystem::*,
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};
fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains NUL",
        ));
    }
    value.push(0);
    Ok(value)
}

pub fn single_link(path: &Path) -> io::Result<bool> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file owns the handle and info is a valid output buffer.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.nNumberOfLinks == 1 && info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0)
}

pub fn private_directory(path: &Path) -> io::Result<bool> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(false);
    }
    private_acl(path)
}

/// A private regular file must have one link and no reparse redirection.
pub fn private_file(path: &Path) -> io::Result<bool> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file()
        || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !single_link(path)?
    {
        return Ok(false);
    }
    private_acl(path)
}

fn private_acl(path: &Path) -> io::Result<bool> {
    // SAFETY: API buffers remain alive; returned security descriptor owns the ACL/SID pointers.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = [0usize; 128];
        let mut length = 0;
        let result = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            std::mem::size_of_val(&buffer) as u32,
            &mut length,
        );
        let error = io::Error::last_os_error();
        CloseHandle(token);
        if result == 0 {
            return Err(error);
        }
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut owner = std::ptr::null_mut();
        let mut acl = std::ptr::null_mut();
        let mut sd = std::ptr::null_mut();
        let result = GetNamedSecurityInfoW(
            wide(path)?.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut acl,
            std::ptr::null_mut(),
            &mut sd,
        );
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        let _descriptor = Descriptor(sd);
        if owner.is_null() || EqualSid(owner, user.User.Sid) == 0 || acl.is_null() {
            return Ok(false);
        }
        let mut info = ACL_SIZE_INFORMATION::default();
        if GetAclInformation(
            acl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of_val(&info) as u32,
            AclSizeInformation,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut owner_access = false;
        for index in 0..info.AceCount {
            let mut entry = std::ptr::null_mut();
            if GetAce(acl, index, &mut entry) == 0 {
                return Err(io::Error::last_os_error());
            }
            let header = &*(entry as *const ACE_HEADER);
            // Inspect the common header before casting an ordinary allow ACE.
            if header.AceType != 0 {
                return Ok(false);
            }
            let ace = &*(entry as *const ACCESS_ALLOWED_ACE);
            let sid = (&ace.SidStart as *const u32).cast_mut().cast();
            let own = EqualSid(sid, user.User.Sid) != 0
                || IsWellKnownSid(sid, WinCreatorOwnerRightsSid) != 0;
            if !own && IsWellKnownSid(sid, WinLocalSystemSid) == 0 {
                return Ok(false);
            }
            if own && ace.Header.AceFlags as u32 & INHERIT_ONLY_ACE == 0 {
                owner_access = true;
            }
        }
        Ok(owner_access)
    }
}

/// Create a new dedicated directory atomically with a protected inheritable DACL.
/// Existing directories must already be private; never rewrite their ACLs.
pub fn create_private_directory(path: &Path) -> io::Result<()> {
    create_private_directory_impl(path, true)
}

/// Atomically reserve a new private directory, refusing an existing path.
pub fn create_private_directory_new(path: &Path) -> io::Result<()> {
    create_private_directory_impl(path, false)
}

fn create_private_directory_impl(path: &Path, allow_existing: bool) -> io::Result<()> {
    let descriptor = private_descriptor("OICI", "FA")?;
    // SAFETY: pointer arguments refer to live local buffers and the owned descriptor.
    unsafe {
        let attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        if CreateDirectoryW(wide(path)?.as_ptr(), &attrs) == 0 {
            let error = io::Error::last_os_error();
            if !allow_existing || error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
    }
    if private_directory(path)? {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory must be private to the current Windows user",
        ))
    }
}

/// Atomically create a new private regular file; never truncate an existing file.
pub fn create_private_file(path: &Path) -> io::Result<std::fs::File> {
    let descriptor = private_descriptor("", "FA")?;
    let attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let path = wide(path)?;
    // SAFETY: live terminated path and descriptor, CREATE_NEW prevents following
    // existing files/reparse points. Successful API result transfers one handle.
    let raw = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ,
            &attrs,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { std::fs::File::from_raw_handle(raw) })
}

/// Publish a previously flushed file without a missing-destination interval.
/// Both paths must be on the same volume; cross-volume copy is never enabled.
pub fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    move_path(
        source,
        destination,
        MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
    )
}

/// Publish a new directory on the same volume, refusing an existing destination.
pub fn rename_directory_new(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source must be a real directory",
        ));
    }
    move_path(source, destination, MOVEFILE_WRITE_THROUGH)
}

fn move_path(source: &Path, destination: &Path, flags: MOVE_FILE_FLAGS) -> io::Result<()> {
    let source = wide(source)?;
    let destination = wide(destination)?;
    // SAFETY: both NUL-terminated buffers remain alive throughout the call.
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Validate the destination directory after a Windows publication. Windows has
/// no supported directory-fsync equivalent: callers must sync file contents and
/// publish with atomic_replace/rename_directory_new (MOVEFILE_WRITE_THROUGH).
/// This check itself makes no additional power-loss durability guarantee.
pub fn durable_directory(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a real directory",
        ))
    }
}

/// Publish a flushed immutable file at a previously absent same-volume path.
pub fn atomic_publish_new(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source must be a real file",
        ));
    }
    move_path(source, destination, MOVEFILE_WRITE_THROUGH)
}

/// Stable same-volume file identity for quota accounting and hard-link deduplication.
/// Directories and reparse points are rejected without following the final component.
pub fn file_identity(path: &Path) -> io::Result<(u32, u64)> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file owns a live handle; info is a correctly sized output buffer.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a real file",
        ));
    }
    Ok((
        info.dwVolumeSerialNumber,
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    ))
}
