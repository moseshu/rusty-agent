// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Read-only Windows ownership and broad-user mutation checks for the configuration namespace.
use anyhow::{Context, Result, anyhow};
use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
use std::{
    ffi::{OsStr, c_void},
    fs::OpenOptions,
    path::Path,
};
use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, GetLastError, HLOCAL, LocalFree},
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        Authorization::{ConvertStringSidToSidW, GetNamedSecurityInfoW, GetSecurityInfo},
        CopySid, DACL_SECURITY_INFORMATION, EqualSid, GENERIC_MAPPING, GetAce, GetAclInformation,
        GetLengthSid, IsValidAcl, LookupAccountNameW, MapGenericMask, OWNER_SECURITY_INFORMATION,
        SID_NAME_USE,
    },
    Storage::FileSystem::{
        DELETE, FILE_ALL_ACCESS, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA,
        READ_CONTROL, WRITE_DAC, WRITE_OWNER,
    },
};
const OBJECT_INHERIT_ACE_FLAG: u8 = 0x01;
const INHERIT_ONLY_ACE: u8 = 0x08;
const INHERITED_ACE: u8 = 0x10;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const TRUSTED_INSTALLER_SID: &str =
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
const STANDARD_USER_MUTATION_MASK: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | FILE_DELETE_CHILD
    | WRITE_DAC
    | WRITE_OWNER;
const SID_ADMINISTRATORS: &str = "S-1-5-32-544";
const SID_USERS: &str = "S-1-5-32-545";
const SID_AUTHENTICATED_USERS: &str = "S-1-5-11";
const SID_EVERYONE: &str = "S-1-1-0";
const SID_SYSTEM: &str = "S-1-5-18";

fn to_wide<S: AsRef<OsStr>>(s: S) -> Vec<u16> {
    let mut v: Vec<u16> = s.as_ref().encode_wide().collect();
    v.push(0);
    v
}

#[allow(
    unsafe_code,
    reason = "Windows SID conversion preserves the source copy-and-free lifetime."
)]
fn resolve_sid(name: &str) -> Result<Vec<u8>> {
    if let Some(sid_str) = well_known_sid_str(name) {
        return sid_bytes_from_string(sid_str);
    }
    let name_w = to_wide(OsStr::new(name));
    let mut sid_buffer = vec![0u8; 68];
    let mut sid_len: u32 = 68;
    let mut domain: Vec<u16> = Vec::new();
    let mut domain_len: u32 = 0;
    let mut use_type: SID_NAME_USE = 0;
    loop {
        let ok = unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                name_w.as_ptr(),
                sid_buffer.as_mut_ptr().cast::<std::ffi::c_void>(),
                &raw mut sid_len,
                domain.as_mut_ptr(),
                &raw mut domain_len,
                &raw mut use_type,
            )
        };
        if ok != 0 {
            sid_buffer.truncate(sid_len as usize);
            return Ok(sid_buffer);
        }
        let err = unsafe { GetLastError() };
        if err == ERROR_INSUFFICIENT_BUFFER {
            sid_buffer.resize(sid_len as usize, 0);
            domain.resize(domain_len as usize, 0);
            continue;
        }
        return Err(anyhow::anyhow!(
            "LookupAccountNameW failed for {name}: {err}"
        ));
    }
}

fn well_known_sid_str(name: &str) -> Option<&'static str> {
    match name {
        "Administrators" => Some(SID_ADMINISTRATORS),
        "Users" => Some(SID_USERS),
        "Authenticated Users" => Some(SID_AUTHENTICATED_USERS),
        "Everyone" => Some(SID_EVERYONE),
        "SYSTEM" => Some(SID_SYSTEM),
        _ => None,
    }
}

#[allow(
    unsafe_code,
    reason = "Windows SID conversion preserves the source copy-and-free lifetime."
)]
fn sid_bytes_from_string(sid_str: &str) -> Result<Vec<u8>> {
    let sid_w = to_wide(OsStr::new(sid_str));
    let mut psid: *mut std::ffi::c_void = std::ptr::null_mut();
    if unsafe { ConvertStringSidToSidW(sid_w.as_ptr(), &raw mut psid) } == 0 {
        return Err(anyhow::anyhow!(
            "ConvertStringSidToSidW failed for {sid_str}: {}",
            unsafe { GetLastError() }
        ));
    }
    let sid_len = unsafe { GetLengthSid(psid) };
    if sid_len == 0 {
        unsafe {
            LocalFree(psid.cast());
        }
        return Err(anyhow::anyhow!("GetLengthSid failed for {sid_str}"));
    }
    let mut out = vec![0u8; sid_len as usize];
    let ok = unsafe { CopySid(sid_len, out.as_mut_ptr().cast::<std::ffi::c_void>(), psid) };
    unsafe {
        LocalFree(psid.cast());
    }
    if ok == 0 {
        return Err(anyhow::anyhow!("CopySid failed for {sid_str}"));
    }
    Ok(out)
}
#[allow(
    unsafe_code,
    reason = "Read-only Windows security-descriptor inspection retains source allocation ownership."
)]
unsafe fn fetch_dacl_handle(path: &Path) -> Result<(*mut ACL, *mut c_void)> {
    // SAFETY: Callers retain the queried descriptor and pass its live ACL and SID pointers.
    unsafe {
        // Rust's file opening supports extended-length paths without changing the
        // caller's spelling or resolving links before the existing reparse checks.
        let handle = OpenOptions::new()
            .access_mode(READ_CONTROL)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .with_context(|| format!("open ACL target {}", path.display()))?;
        let mut p_sd: *mut c_void = std::ptr::null_mut();
        let mut p_dacl: *mut ACL = std::ptr::null_mut();
        let code = GetSecurityInfo(
            handle.as_raw_handle().cast(),
            1, // SE_FILE_OBJECT
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut p_dacl,
            std::ptr::null_mut(),
            &raw mut p_sd,
        );
        if code != ERROR_SUCCESS {
            return Err(anyhow!(
                "GetSecurityInfo failed for {}: {}",
                path.display(),
                code
            ));
        }
        Ok((p_dacl, p_sd))
    }
}

#[derive(Clone, Copy)]
enum AceScope {
    Effective,
    #[allow(
        dead_code,
        reason = "Retain the source explicit-ACE evaluator for sandbox consumers."
    )]
    Explicit,
    EffectiveOrChildFile,
}

#[allow(
    unsafe_code,
    reason = "Read-only Windows security-descriptor inspection retains source allocation ownership."
)]
unsafe fn dacl_mask_allows_with_scope(
    p_dacl: *mut ACL,
    psids: &[*mut c_void],
    desired_mask: u32,
    require_all_bits: bool,
    scope: AceScope,
) -> bool {
    // SAFETY: Callers retain the queried descriptor and pass its live ACL and SID pointers.
    unsafe {
        if p_dacl.is_null() {
            return false;
        }
        let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
        let Ok(info_size) = u32::try_from(std::mem::size_of::<ACL_SIZE_INFORMATION>()) else {
            return false;
        };
        let ok = GetAclInformation(
            p_dacl.cast_const(),
            (&raw mut info).cast::<c_void>(),
            info_size,
            AclSizeInformation,
        );
        if ok == 0 {
            return false;
        }
        let mapping = GENERIC_MAPPING {
            GenericRead: FILE_GENERIC_READ,
            GenericWrite: FILE_GENERIC_WRITE,
            GenericExecute: FILE_GENERIC_EXECUTE,
            GenericAll: FILE_ALL_ACCESS,
        };
        for i in 0..info.AceCount {
            let mut p_ace: *mut c_void = std::ptr::null_mut();
            if GetAce(p_dacl.cast_const(), i, &raw mut p_ace) == 0 {
                continue;
            }
            let hdr = &*(p_ace as *const ACE_HEADER);
            if hdr.AceType != ACCESS_ALLOWED_ACE_TYPE {
                continue; // not ACCESS_ALLOWED
            }
            if (hdr.AceFlags & INHERIT_ONLY_ACE) != 0
                && (!matches!(scope, AceScope::EffectiveOrChildFile)
                    || (hdr.AceFlags & OBJECT_INHERIT_ACE_FLAG) == 0)
            {
                continue;
            }
            // SET_ACCESS cannot replace an ACE inherited from an ancestor, so it cannot make
            // an explicit-only repair converge when that inherited ACE contains stale rights.
            if matches!(scope, AceScope::Explicit) && (hdr.AceFlags & INHERITED_ACE) != 0 {
                continue;
            }
            let base = p_ace as usize;
            let sid_ptr = (base + std::mem::size_of::<ACE_HEADER>() + std::mem::size_of::<u32>())
                as *mut c_void;
            let mut matched = false;
            for sid in psids {
                if EqualSid(sid_ptr, *sid) != 0 {
                    matched = true;
                    break;
                }
            }
            if !matched {
                continue;
            }
            let ace = &*(p_ace as *const ACCESS_ALLOWED_ACE);
            let mut mask = ace.Mask;
            MapGenericMask(&raw mut mask, &raw const mapping);
            if (require_all_bits && (mask & desired_mask) == desired_mask)
                || (!require_all_bits && (mask & desired_mask) != 0)
            {
                return true;
            }
        }
        false
    }
}

/// Whether a broad standard-user principal has an allow ACE that can mutate
/// the path. This is intentionally conservative and does not evaluate deny
/// ACEs against a concrete token.
pub fn path_has_standard_user_mutation_allow(path: &Path) -> Result<bool> {
    path_has_standard_user_mutation_allow_with_scope(path, AceScope::Effective)
}

/// Whether a broad standard-user principal has an allow ACE that can mutate
/// the path or a directly inherited child file.
pub fn path_or_child_file_has_standard_user_mutation_allow(path: &Path) -> Result<bool> {
    path_has_standard_user_mutation_allow_with_scope(path, AceScope::EffectiveOrChildFile)
}

#[allow(
    unsafe_code,
    reason = "Read-only Windows security-descriptor inspection retains source allocation ownership."
)]
fn path_has_standard_user_mutation_allow_with_scope(path: &Path, scope: AceScope) -> Result<bool> {
    // These canonical labels map to fixed SIDs before any localized account lookup.
    let mut users = resolve_sid("Users")?;
    let mut authenticated_users = resolve_sid("Authenticated Users")?;
    let mut everyone = resolve_sid("Everyone")?;
    let sids = [
        users.as_mut_ptr().cast(),
        authenticated_users.as_mut_ptr().cast(),
        everyone.as_mut_ptr().cast(),
    ];
    unsafe {
        let (dacl, descriptor) = fetch_dacl_handle(path)?;
        let result = if dacl.is_null() {
            Ok(true)
        } else if IsValidAcl(dacl) == 0 {
            Err(anyhow!("invalid DACL for {}", path.display()))
        } else {
            Ok(dacl_mask_allows_with_scope(
                dacl,
                &sids,
                STANDARD_USER_MUTATION_MASK,
                /*require_all_bits*/ false,
                scope,
            ))
        };
        if !descriptor.is_null() {
            LocalFree(descriptor as HLOCAL);
        }
        result
    }
}

/// Whether the path owner is one of the conservative administrator-controlled
/// principals used by the Windows system-config shadow check.
pub fn path_has_trusted_system_owner(path: &Path) -> Result<bool> {
    // These canonical labels map to fixed SIDs before any localized account lookup.
    let mut administrators = resolve_sid("Administrators")?;
    let mut system = resolve_sid("SYSTEM")?;
    let mut trusted_installer = sid_bytes_from_string(TRUSTED_INSTALLER_SID)?;
    let trusted_sids = [
        administrators.as_mut_ptr().cast(),
        system.as_mut_ptr().cast(),
        trusted_installer.as_mut_ptr().cast(),
    ];
    path_owner_matches(path, &trusted_sids)
}

#[allow(
    unsafe_code,
    reason = "Read-only Windows security-descriptor inspection retains source allocation ownership."
)]
fn path_owner_matches(path: &Path, trusted_sids: &[*mut c_void]) -> Result<bool> {
    let mut owner = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let code = unsafe {
        GetNamedSecurityInfoW(
            to_wide(path).as_ptr(),
            1, // SE_FILE_OBJECT
            OWNER_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if code != ERROR_SUCCESS {
        if !descriptor.is_null() {
            unsafe { LocalFree(descriptor as HLOCAL) };
        }
        return Err(anyhow!(
            "GetNamedSecurityInfoW failed for {}: {code}",
            path.display()
        ));
    }
    if owner.is_null() {
        if !descriptor.is_null() {
            unsafe { LocalFree(descriptor as HLOCAL) };
        }
        return Err(anyhow!("missing owner for {}", path.display()));
    }
    let matches = trusted_sids
        .iter()
        .any(|trusted_sid| unsafe { EqualSid(owner, *trusted_sid) } != 0);
    if !descriptor.is_null() {
        unsafe { LocalFree(descriptor as HLOCAL) };
    }
    Ok(matches)
}
