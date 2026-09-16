//! Security descriptors name the current user explicitly; the token's default
//! owner can be a group and OWNER RIGHTS is not a substitute for the user's SID.
use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        GetTokenInformation, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

pub(super) struct Descriptor(pub(super) PSECURITY_DESCRIPTOR);
impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: both security APIs used here allocate descriptors with LocalAlloc.
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn current_user_sid() -> io::Result<String> {
    let mut raw = null_mut();
    // SAFETY: valid pseudo-process handle and writable output handle slot.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful OpenProcessToken transfers a fresh handle to us.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    // TOKEN_USER followed by the maximum Windows SID fits in this aligned buffer.
    let mut buffer = [0usize; 128];
    let mut length = 0;
    // SAFETY: aligned live writable buffer and its accurate byte size.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            std::mem::size_of_val(&buffer) as u32,
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful TokenUser query initializes this header and its in-buffer SID.
    let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    let mut sid_text = null_mut();
    // SAFETY: SID is alive and the conversion allocates a terminated UTF-16 result.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful conversion returns a valid NUL-terminated allocation.
    let result = unsafe {
        let mut len = 0;
        while *sid_text.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, len));
        LocalFree(sid_text.cast());
        text
    };
    Ok(result)
}

pub(super) fn private_descriptor(inheritance: &str, access: &str) -> io::Result<Descriptor> {
    let sid = current_user_sid()?;
    let sddl: Vec<u16> =
        format!("O:{sid}D:P(A;{inheritance};{access};;;{sid})(A;{inheritance};{access};;;SY)\0")
            .encode_utf16()
            .collect();
    let mut descriptor = null_mut();
    // SAFETY: SDDL is terminated and output points to a live descriptor slot.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Descriptor(descriptor))
}

/// Authenticate the server before sending any local control request. Named pipes
/// live in a machine-wide namespace, so an ACL alone cannot prevent squatting
/// before the legitimate server starts.
pub(super) fn owned_by_current_user(
    handle: std::os::windows::io::BorrowedHandle<'_>,
) -> io::Result<bool> {
    use windows_sys::Win32::Security::{
        Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT},
        EqualSid, GetSecurityDescriptorOwner, OWNER_SECURITY_INFORMATION,
    };
    let expected = private_descriptor("", "GA")?;
    let mut expected_owner = null_mut();
    let mut defaulted = 0;
    // SAFETY: expected owns a valid descriptor and output slots are writable.
    if unsafe { GetSecurityDescriptorOwner(expected.0, &mut expected_owner, &mut defaulted) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut owner = null_mut();
    let mut actual = null_mut();
    // SAFETY: borrowed handle is live; successful query allocates descriptor and
    // returns an owner pointer within it, both retained through the comparison.
    let code = unsafe {
        GetSecurityInfo(
            handle.as_raw_handle(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut actual,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    let _actual = Descriptor(actual);
    Ok(!owner.is_null() && unsafe { EqualSid(owner, expected_owner) } != 0)
}
