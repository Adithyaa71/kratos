//! The Windows-specific parts: who the current user is, a security descriptor
//! that admits only them, single-instance guarding, and sealing the API key at
//! rest.
//!
//! This is the only place in the workspace that calls into Win32 for security,
//! so "can anything else on this machine reach the pipe" is a question with
//! one place to look.

use std::ffi::c_void;
use std::io;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, OpenProcessToken,
};

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn from_wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
}

/// The SID of the account this process runs as, in string form
/// (`S-1-5-21-...`).
///
/// Used twice: in the pipe's name, so another account's broker is a different
/// object entirely, and in its DACL, so the name alone is not what protects it.
pub fn current_user_sid() -> io::Result<String> {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(last_error());
        }
        // Scoped so the token handle is closed on every path out, including
        // the error ones below.
        let result = (|| {
            let mut needed: u32 = 0;
            // First call is expected to fail with ERROR_INSUFFICIENT_BUFFER;
            // all we want from it is the size.
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            if needed == 0 {
                return Err(last_error());
            }
            let mut buf = vec![0u8; needed as usize];
            if GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr() as *mut c_void,
                needed,
                &mut needed,
            ) == 0
            {
                return Err(last_error());
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut sid_str: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid_str) == 0 {
                return Err(last_error());
            }
            let s = from_wide(sid_str);
            LocalFree(sid_str as *mut c_void);
            Ok(s)
        })();
        CloseHandle(token);
        result
    }
}

/// A `SECURITY_ATTRIBUTES` admitting exactly one SID and nothing else.
///
/// Owns the security descriptor it points at, so it must outlive every pipe
/// instance created from it -- hence the struct rather than a bare pointer.
pub struct OwnerOnlySecurity {
    attrs: SECURITY_ATTRIBUTES,
    descriptor: *mut c_void,
}

// The descriptor is a plain allocation that nothing else aliases; it is only
// read by CreateNamedPipe while this value is borrowed.
unsafe impl Send for OwnerOnlySecurity {}
unsafe impl Sync for OwnerOnlySecurity {}

impl OwnerOnlySecurity {
    /// `D:P(A;;FA;;;<sid>)` -- a protected DACL (`P`: no inherited ACEs can be
    /// added) with a single ACE granting full access to one SID.
    ///
    /// No ACE for SYSTEM and none for Administrators: an administrator can
    /// take ownership of anything on the machine regardless, so adding them
    /// buys no security and would widen the set of processes that can connect
    /// without being asked.
    pub fn for_sid(sid: &str) -> io::Result<Self> {
        let sddl = format!("D:P(A;;FA;;;{sid})");
        let mut descriptor: *mut c_void = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide(&sddl).as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(OwnerOnlySecurity {
            attrs: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                // A detached broker must not hand this pipe to children.
                bInheritHandle: 0,
            },
            descriptor,
        })
    }

    pub fn for_current_user() -> io::Result<Self> {
        Self::for_sid(&current_user_sid()?)
    }

    /// Pointer for `ServerOptions::create_with_security_attributes_raw`.
    pub fn as_ptr(&self) -> *mut c_void {
        &self.attrs as *const SECURITY_ATTRIBUTES as *mut c_void
    }
}

impl Drop for OwnerOnlySecurity {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            unsafe { LocalFree(self.descriptor) };
            self.descriptor = std::ptr::null_mut();
        }
    }
}

/// A process-wide, session-local named mutex. Held for the lifetime of the
/// handle; dropping it releases the name.
///
/// `Local\` rather than `Global\` deliberately: creating an object in the
/// global namespace needs SeCreateGlobalPrivilege, which a standard user does
/// not have, and the broker and the shims that spawn it always share one
/// session anyway.
pub struct SingleInstance {
    handle: HANDLE,
    pub already_running: bool,
}

unsafe impl Send for SingleInstance {}
unsafe impl Sync for SingleInstance {}

impl SingleInstance {
    pub fn acquire(name: &str) -> io::Result<Self> {
        let full = format!("Local\\{name}");
        let handle = unsafe { CreateMutexW(std::ptr::null(), 1, wide(&full).as_ptr()) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        Ok(SingleInstance { handle, already_running })
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

/// Seal bytes with DPAPI, scoped to the current user.
///
/// The result is useless on another machine or under another account, so a
/// copied `config.json` does not leak the API key -- which is the whole reason
/// the key is not stored as text.
pub fn protect(plain: &[u8]) -> io::Result<Vec<u8>> {
    crypt(plain, true)
}

pub fn unprotect(sealed: &[u8]) -> io::Result<Vec<u8>> {
    crypt(sealed, false)
}

fn crypt(input: &[u8], seal: bool) -> io::Result<Vec<u8>> {
    // A zero-length input is a legitimate "no key set"; DPAPI would reject it.
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let inblob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    let ok = unsafe {
        if seal {
            CryptProtectData(
                &inblob,
                wide("kratos decider key").as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                // Never prompt: the broker may be running detached with no
                // desktop to show a dialog on.
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &inblob,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
    };
    if ok == 0 {
        return Err(last_error());
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    unsafe { LocalFree(out.pbData as *mut c_void) };
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_sid_looks_like_a_sid() {
        let sid = current_user_sid().expect("token query");
        assert!(sid.starts_with("S-1-"), "{sid}");
        assert!(sid.len() > 8, "{sid}");
    }

    #[test]
    fn security_descriptor_builds_for_the_current_user() {
        let sec = OwnerOnlySecurity::for_current_user().expect("sddl");
        assert!(!sec.as_ptr().is_null());
    }

    #[test]
    fn a_malformed_sid_is_rejected_rather_than_widening_access() {
        // The failure mode that matters: if a bad SID silently produced a
        // null descriptor, the pipe would be created with the *default* ACL,
        // which is wider than owner-only.
        assert!(OwnerOnlySecurity::for_sid("not-a-sid").is_err());
    }

    #[test]
    fn dpapi_round_trips_and_ciphertext_differs() {
        let secret = b"apikey_example_value";
        let sealed = protect(secret).expect("protect");
        assert_ne!(sealed.as_slice(), secret.as_slice());
        assert!(sealed.len() > secret.len());
        assert_eq!(unprotect(&sealed).expect("unprotect"), secret);
    }

    #[test]
    fn empty_input_is_not_an_error() {
        assert!(protect(b"").unwrap().is_empty());
        assert!(unprotect(b"").unwrap().is_empty());
    }

    #[test]
    fn garbage_does_not_unprotect() {
        assert!(unprotect(b"this is not a dpapi blob at all").is_err());
    }

    #[test]
    fn second_acquire_of_a_name_reports_already_running() {
        let name = format!("kratos-test-mutex-{}", std::process::id());
        let first = SingleInstance::acquire(&name).unwrap();
        assert!(!first.already_running);
        let second = SingleInstance::acquire(&name).unwrap();
        assert!(second.already_running);
        drop(second);
        drop(first);
        // Released: a fresh acquire is first again.
        let third = SingleInstance::acquire(&name).unwrap();
        assert!(!third.already_running);
    }
}
