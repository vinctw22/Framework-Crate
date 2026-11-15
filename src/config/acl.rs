// Config file ACL hardening (Windows).
//
// Split from config.rs: broad-group write ACE detection and removal
// live here. The config-save path is best-effort and fail-open; the
// `has_broad_modify_ace` variant is fail-closed because it gates registration
// of a SYSTEM scheduled task.

/// Write-like rights: writing/appending data, taking ownership, or changing
/// the DACL all let Everyone rewrite or re-permission the file.
const WORLD_WRITE_MASK: u32 = {
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_APPEND_DATA, FILE_WRITE_DATA, WRITE_DAC, WRITE_OWNER,
    };
    FILE_WRITE_DATA | FILE_APPEND_DATA | GENERIC_WRITE | WRITE_DAC | WRITE_OWNER
};

/// Rights that let a non-administrator replace or re-permission an object.
///
/// `DELETE` and `FILE_DELETE_CHILD` matter here: replacing a directory (or an
/// exe inside it) is enough to hijack a SYSTEM task even when no data-write
/// ACE is present.
const BROAD_MODIFY_MASK: u32 = {
    use windows_sys::Win32::Foundation::{GENERIC_ALL, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA,
        FILE_WRITE_EA, WRITE_DAC, WRITE_OWNER,
    };
    FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | DELETE
        | FILE_DELETE_CHILD
        | WRITE_DAC
        | WRITE_OWNER
        | GENERIC_WRITE
        | GENERIC_ALL
};

/// Builds the byte images of the given well-known SIDs. SIDs that cannot be
/// created are skipped; callers that need fail-closed behavior check for an
/// empty result.
fn well_known_sids(
    types: &[windows_sys::Win32::Security::WELL_KNOWN_SID_TYPE],
) -> Vec<(Vec<u8>, usize)> {
    use windows_sys::Win32::Security::{CreateWellKnownSid, PSID, SECURITY_MAX_SID_SIZE};
    let mut out = Vec::with_capacity(types.len());
    for &well_known in types {
        let mut buf = [0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut len = buf.len() as u32;
        // SAFETY: buffer is SECURITY_MAX_SID_SIZE bytes, the documented maximum.
        let ok = unsafe {
            CreateWellKnownSid(
                well_known,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as PSID,
                &mut len,
            )
        };
        if ok != 0 {
            out.push((buf.to_vec(), len as usize));
        }
    }
    out
}

/// Scans a DACL for an allow-ACE granting any bit in `mask` to `watched`.
///
/// `Ok(false)` means no matching ACE, `Ok(true)` means one was found. `Err`
/// means the DACL could not be read or is malformed; callers that guard
/// privileged execution must treat that as unsafe. A NULL DACL grants full
/// access to everyone and is reported as `true` when
/// `null_dacl_is_dangerous`.
fn acl_grants(
    path: &std::path::Path,
    mask: u32,
    watched: &[(Vec<u8>, usize)],
    null_dacl_is_dangerous: bool,
) -> Result<bool, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, INHERIT_ONLY_ACE,
    };
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
    use windows_sys::core::PCWSTR;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: null-terminated path; DACL and SD outputs are freed below.
    let err = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr() as PCWSTR,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if err != 0 || sd.is_null() {
        if !sd.is_null() {
            // SAFETY: SD was allocated by the call above.
            unsafe { LocalFree(sd) };
        }
        return Err(format!(
            "cannot read DACL for {}: {}",
            path.display(),
            std::io::Error::from_raw_os_error(err as i32)
        ));
    }
    if dacl.is_null() {
        // SAFETY: SD was allocated by GetNamedSecurityInfoW above.
        unsafe { LocalFree(sd) };
        return Ok(null_dacl_is_dangerous);
    }
    // Walk ACEs manually: the first ACE follows the 8-byte ACL header and each
    // ACE carries its own size. Bounds-checked against AclSize throughout.
    // SAFETY: dacl points to a valid ACL; sd is freed at the end.
    let result = unsafe {
        let acl_size = (*dacl).AclSize as usize;
        let ace_count = (*dacl).AceCount as usize;
        let mut offset = std::mem::size_of::<ACL>();
        let mut malformed = false;
        let mut hit = false;
        for _ in 0..ace_count {
            if offset + std::mem::size_of::<ACE_HEADER>() > acl_size {
                malformed = true;
                break;
            }
            let header = &*((dacl as *const u8).add(offset) as *const ACE_HEADER);
            let size = header.AceSize as usize;
            // Header + mask + a minimal SID: anything smaller cannot be a
            // valid allow/deny ACE and is treated as a malformed DACL.
            if size < 16 || offset + size > acl_size {
                malformed = true;
                break;
            }
            if header.AceType as u32 == ACCESS_ALLOWED_ACE_TYPE
                && (header.AceFlags as u32) & INHERIT_ONLY_ACE == 0
            {
                let ace = &*((dacl as *const u8).add(offset) as *const ACCESS_ALLOWED_ACE);
                let sid = &ace.SidStart as *const u32 as *const u8;
                // SID length is 8 header bytes plus 4 per sub-authority.
                let sid_len = 8 + 4 * (*sid.add(1) as usize);
                if sid_len > size - 8 {
                    malformed = true;
                    break;
                }
                let watched_hit = watched.iter().any(|(buf, len)| {
                    sid_len == *len && core::slice::from_raw_parts(sid, sid_len) == &buf[..*len]
                });
                if watched_hit && ace.Mask & mask != 0 {
                    hit = true;
                    break;
                }
            }
            offset += size;
        }
        if malformed {
            Err(format!("malformed DACL for {}", path.display()))
        } else {
            Ok(hit)
        }
    };
    // SAFETY: SD was allocated by GetNamedSecurityInfoW above.
    unsafe { LocalFree(sd) };
    result
}

/// Checks the file DACL for an allow-ACE granting write-like rights to a
/// broad SID (Everyone, Users, Authenticated Users). Returns false on any
/// query failure (fail-open: inspection errors must not block saves).
fn file_has_world_write_ace(path: &std::path::Path) -> bool {
    use windows_sys::Win32::Security::{WinAuthenticatedUserSid, WinBuiltinUsersSid, WinWorldSid};
    let watched = well_known_sids(&[WinWorldSid, WinBuiltinUsersSid, WinAuthenticatedUserSid]);
    if watched.is_empty() {
        return false;
    }
    acl_grants(path, WORLD_WRITE_MASK, &watched, false).unwrap_or(false)
}

/// Fail-closed check for paths that a SYSTEM task will execute.
///
/// Unlike [`file_has_world_write_ace`], a query failure or a malformed DACL is
/// an error, and a NULL DACL counts as broad access. Watched SIDs include
/// Interactive and Remote Desktop Users in addition to Everyone, Users and
/// Authenticated Users, and the mask includes delete rights because replacing
/// any directory in the chain is enough to replace the executable.
pub(crate) fn has_broad_modify_ace(path: &std::path::Path) -> Result<bool, String> {
    use windows_sys::Win32::Security::{
        WinAuthenticatedUserSid, WinBuiltinRemoteDesktopUsersSid, WinBuiltinUsersSid,
        WinInteractiveSid, WinWorldSid,
    };
    const WATCHED: [windows_sys::Win32::Security::WELL_KNOWN_SID_TYPE; 5] = [
        WinWorldSid,
        WinBuiltinUsersSid,
        WinAuthenticatedUserSid,
        WinInteractiveSid,
        WinBuiltinRemoteDesktopUsersSid,
    ];
    let watched = well_known_sids(&WATCHED);
    if watched.len() != WATCHED.len() {
        // Partial resolution would silently skip a group; fail closed.
        return Err(format!(
            "cannot build all well-known SIDs for ACL check ({} of {})",
            watched.len(),
            WATCHED.len()
        ));
    }
    acl_grants(path, BROAD_MODIFY_MASK, &watched, true)
}

/// Warns if the config file at `path` may be world-writable.
/// On Windows, %APPDATA% is per-user (user+SYSTEM+Admins only) so inherited ACL
/// is already restrictive; we warn if the DACL holds an explicit allow-ACE
/// granting write-like rights to Everyone. Fail-open: inspection errors are
/// silently ignored and never block a save.
pub(crate) fn warn_if_world_writable(path: &std::path::Path) {
    {
        if !path.exists() {
            return;
        }
        if file_has_world_write_ace(path) {
            tracing::warn!(
                "Config file {} grants write access to a broad group — consider removing the permissive ACE",
                path.display()
            );
        } else {
            tracing::debug!("Config file {} has no world-writable ACE", path.display());
        }
    }
}

/// Best-effort hardening: removes explicit Everyone write ACEs from the
/// config file, leaving inherited ACLs (e.g. from %APPDATA%) untouched.
/// Failures are warn-only and do not abort the save.
pub(crate) fn harden_file_acl(path: &std::path::Path) {
    if !file_has_world_write_ace(path) {
        tracing::debug!(
            "Config file {} saved with inherited ACL from %APPDATA% (per-user)",
            path.display()
        );
        return;
    }
    match revoke_everyone_write(path) {
        Ok(true) => tracing::warn!(
            "Removed permissive broad-group write ACEs from {}",
            path.display()
        ),
        Ok(false) => tracing::warn!(
            "Config file {} has an inherited broad-group write ACE, leaving it (parent dir out of scope)",
            path.display()
        ),
        Err(e) => tracing::warn!(
            "Failed to remove permissive ACE from {}: {}",
            path.display(),
            e
        ),
    }
}

/// Revokes broad-group write-like rights on `path` via explicit REVOKE
/// entries, then re-verifies. Only explicit ACEs are touched: inherited ones
/// belong to the parent directory and are reported, not modified.
/// Returns Ok(true) when the file verifies clean afterwards.
fn revoke_everyone_write(path: &std::path::Path) -> Result<bool, &'static str> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS,
        SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
        TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CreateWellKnownSid, DACL_SECURITY_INFORMATION,
        INHERITED_ACE, NO_INHERITANCE, PSID, SECURITY_MAX_SID_SIZE, WinAuthenticatedUserSid,
        WinBuiltinUsersSid, WinWorldSid,
    };
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
    use windows_sys::core::{PCWSTR, PWSTR};

    // Scrub the same broad SIDs the detector watches. Skipping e.g. Users
    // here would leave the file writable while detection keeps firing.
    let mut sid_bufs: Vec<Vec<u8>> = Vec::new();
    let mut sid_lens: Vec<usize> = Vec::new();
    for well_known in [WinWorldSid, WinBuiltinUsersSid, WinAuthenticatedUserSid] {
        let mut buf = [0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut len = buf.len() as u32;
        // SAFETY: buffer is SECURITY_MAX_SID_SIZE bytes, the documented maximum.
        let ok = unsafe {
            CreateWellKnownSid(
                well_known,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as PSID,
                &mut len,
            )
        };
        if ok != 0 {
            sid_lens.push(len as usize);
            sid_bufs.push(buf.to_vec());
        }
    }
    if sid_bufs.is_empty() {
        return Err("could not build well-known SIDs");
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: null-terminated path; DACL and SD outputs freed below.
    let err = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr() as PCWSTR,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if err != 0 || dacl.is_null() {
        if !sd.is_null() {
            // SAFETY: SD was allocated by the call above.
            unsafe { LocalFree(sd) };
        }
        return Err("could not read DACL");
    }
    // Refuse when any offending ACE is inherited: those belong to the parent
    // directory and must not be rewritten here.
    // SAFETY: dacl points to a valid ACL; bounds-checked against AclSize.
    let explicit_only = unsafe {
        let acl_size = (*dacl).AclSize as usize;
        let mut offset = std::mem::size_of::<ACL>();
        let mut all_explicit = true;
        while offset + std::mem::size_of::<ACE_HEADER>() <= acl_size {
            let header = &*((dacl as *const u8).add(offset) as *const ACE_HEADER);
            let size = header.AceSize as usize;
            if size < std::mem::size_of::<ACE_HEADER>() || offset + size > acl_size {
                break;
            }
            if header.AceType as u32 == ACCESS_ALLOWED_ACE_TYPE {
                let ace = &*((dacl as *const u8).add(offset) as *const ACCESS_ALLOWED_ACE);
                let sid = &ace.SidStart as *const u32 as *const u8;
                let sid_len = 8 + 4 * (*sid.add(1) as usize);
                let watched = sid_bufs.iter().zip(sid_lens.iter()).any(|(buf, len)| {
                    sid_len == *len && core::slice::from_raw_parts(sid, sid_len) == &buf[..*len]
                });
                if watched
                    && ace.Mask & WORLD_WRITE_MASK != 0
                    && (header.AceFlags as u32) & INHERITED_ACE != 0
                {
                    all_explicit = false;
                    break;
                }
            }
            offset += size;
        }
        all_explicit
    };
    if !explicit_only {
        // SAFETY: SD was allocated above.
        unsafe { LocalFree(sd) };
        return Ok(false);
    }
    let mut trustees: Vec<TRUSTEE_W> = Vec::with_capacity(sid_bufs.len());
    for buf in sid_bufs.iter_mut() {
        trustees.push(TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: buf.as_mut_ptr() as PWSTR,
        });
    }
    let entries: Vec<EXPLICIT_ACCESS_W> = trustees
        .iter()
        .map(|trustee| EXPLICIT_ACCESS_W {
            grfAccessPermissions: WORLD_WRITE_MASK,
            grfAccessMode: REVOKE_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: *trustee,
        })
        .collect();
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: explicit entries and old DACL are valid; new DACL freed below.
    // sid_bufs outlives the call, so trustee pointers stay valid.
    let err =
        unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), dacl, &mut new_dacl) };
    // SAFETY: SD was allocated above.
    unsafe { LocalFree(sd) };
    if err != 0 || new_dacl.is_null() {
        return Err("could not rebuild DACL");
    }
    // SAFETY: null-terminated path; new DACL built above, freed below.
    let err = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr() as PCWSTR,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: allocated by SetEntriesInAclW above.
    unsafe { LocalFree(new_dacl as *mut core::ffi::c_void) };
    if err != 0 {
        return Err("could not write DACL");
    }
    // Verify the removal actually landed.
    if file_has_world_write_ace(path) {
        return Err("permissive ACE still present after revoke");
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harden_removes_explicit_everyone_write_ace() {
        // End-to-end through the real DACL: plant an Everyone write ACE the
        // way a misconfigured tool would, then verify detection sees it and
        // hardening removes it. Skips quietly when icacls is unavailable.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acl-test.toml");
        std::fs::write(&path, "x = 1").unwrap();
        let grant = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/grant", "Everyone:W"])
            .output();
        let Ok(out) = grant else { return };
        if !out.status.success() {
            return;
        }
        assert!(
            file_has_world_write_ace(&path),
            "planted ACE must be detected"
        );
        harden_file_acl(&path);
        assert!(
            !file_has_world_write_ace(&path),
            "permissive ACE must be revoked"
        );
    }

    #[test]
    fn harden_removes_explicit_users_write_ace() {
        // Same end-to-end as above but with the Users group: detection
        // already watched it, and revoke must now clear it too.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acl-users.toml");
        std::fs::write(&path, "x = 1").unwrap();
        let grant = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/grant", "Users:W"])
            .output();
        let Ok(out) = grant else { return };
        if !out.status.success() {
            return;
        }
        assert!(
            file_has_world_write_ace(&path),
            "planted Users ACE must be detected"
        );
        harden_file_acl(&path);
        assert!(
            !file_has_world_write_ace(&path),
            "Users write ACE must be revoked"
        );
    }

    #[test]
    fn world_write_detects_builtin_users_ace() {
        // Same end-to-end as above but with the Users group ACE.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acl-users.toml");
        std::fs::write(&path, "x = 1").unwrap();
        let grant = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/grant", "Users:W"])
            .output();
        let Ok(out) = grant else { return };
        if !out.status.success() {
            return;
        }
        assert!(
            file_has_world_write_ace(&path),
            "Users write ACE must be detected"
        );
    }

    #[test]
    fn broad_modify_watched_sids_all_resolve() {
        use windows_sys::Win32::Security::{
            WinAuthenticatedUserSid, WinBuiltinRemoteDesktopUsersSid, WinBuiltinUsersSid,
            WinInteractiveSid, WinWorldSid,
        };
        let types = [
            WinWorldSid,
            WinBuiltinUsersSid,
            WinAuthenticatedUserSid,
            WinInteractiveSid,
            WinBuiltinRemoteDesktopUsersSid,
        ];
        assert_eq!(well_known_sids(&types).len(), types.len());
    }

    #[test]
    fn broad_modify_check_fails_closed_for_missing_path() {
        // A path the caller cannot inspect must never be reported as safe:
        // this check guards SYSTEM task registration.
        let dir = tempfile::tempdir().unwrap();
        assert!(has_broad_modify_ace(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn broad_modify_detects_everyone_full_control() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broad.toml");
        std::fs::write(&path, "x = 1").unwrap();
        let grant = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/grant", "Everyone:F"])
            .output();
        let Ok(out) = grant else { return };
        if !out.status.success() {
            return;
        }
        assert!(
            has_broad_modify_ace(&path).expect("ACL query"),
            "Everyone:F must be rejected before a SYSTEM task uses the path"
        );
    }
}
