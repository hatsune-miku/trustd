//! Keep the login daemon unprivileged even when Windows has no split UAC token.
use super::*;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::System::RemoteDesktop::WTSQueryUserToken;
use windows_sys::Win32::System::SystemServices::{
    SE_GROUP_INTEGRITY, SE_GROUP_OWNER, SE_GROUP_USE_FOR_DENY_ONLY, SECURITY_MANDATORY_MEDIUM_RID,
};

fn linked_token(handle: HANDLE) -> io::Result<OwnedHandle> {
    let data = token_data(handle, TokenLinkedToken)?;
    // SAFETY: the returned token handle is owned by the caller.
    own(unsafe { (*data.as_ptr().cast::<TOKEN_LINKED_TOKEN>()).LinkedToken })
}

fn adjustable_token_handle(handle: HANDLE) -> io::Result<OwnedHandle> {
    let mut duplicate = null_mut();
    // SAFETY: both process pseudo-handles and the source token remain valid.
    // Duplicate the handle, not the token object or its parent-token identity.
    check(unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            GetCurrentProcess(),
            &mut duplicate,
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
            0,
            0,
        )
    })?;
    own(duplicate)
}

fn restricted_token(handle: HANDLE) -> io::Result<OwnedHandle> {
    // Filter the original token, not a duplicate: process creation without
    // SeAssignPrimaryTokenPrivilege requires a restriction of the caller's
    // own primary token. Duplicating first breaks that relationship for a
    // built-in Administrator without a split UAC token.
    let groups = token_data(handle, TokenGroups)?;
    // SAFETY: the variable-sized TOKEN_GROUPS stays allocated through filtering.
    let groups = unsafe { &*groups.as_ptr().cast::<TOKEN_GROUPS>() };
    let groups =
        unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) };
    let mut disabled = Vec::new();
    for group in groups {
        if administrative_group(&sid_text(group.Sid)?, group.Attributes) {
            disabled.push(*group);
        }
    }
    let mut restricted = null_mut();
    // Filter groups explicitly: LUA_TOKEN also disables the built-in
    // Administrator's user SID, preventing it from accessing its own objects.
    // Remove privileges except traversal; never use SANDBOX_INERT.
    check(unsafe {
        CreateRestrictedToken(
            handle,
            DISABLE_MAX_PRIVILEGE,
            disabled.len() as u32,
            disabled.as_ptr(),
            0,
            null(),
            0,
            null(),
            &mut restricted,
        )
    })?;
    let restricted = own(restricted)?;
    // Filtering inherits the source handle's access mask. Reopen that SAME
    // token object with rights to adjust its defaults; do not clone it again.
    let restricted = adjustable_token_handle(restricted.as_raw_handle())?;
    user_default_security(restricted.as_raw_handle())?;
    let mut sid = SID {
        Revision: 1,
        SubAuthorityCount: 1,
        IdentifierAuthority: SID_IDENTIFIER_AUTHORITY {
            Value: [0, 0, 0, 0, 0, 16],
        },
        SubAuthority: [SECURITY_MANDATORY_MEDIUM_RID as u32],
    };
    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: (&mut sid as *mut SID).cast(),
            Attributes: SE_GROUP_INTEGRITY as u32,
        },
    };
    // SAFETY: the one-subauthority medium SID and label live through this call.
    check(unsafe {
        SetTokenInformation(
            restricted.as_raw_handle(),
            TokenIntegrityLevel,
            (&label as *const TOKEN_MANDATORY_LABEL).cast(),
            (size_of_val(&label) + size_of_val(&sid)) as u32,
        )
    })?;
    Ok(restricted)
}

fn administrative_group(sid: &str, attributes: u32) -> bool {
    attributes & SE_GROUP_OWNER as u32 != 0
        || matches!(
            sid,
            "S-1-5-32-544"
                | "S-1-5-32-547"
                | "S-1-5-32-548"
                | "S-1-5-32-549"
                | "S-1-5-32-550"
                | "S-1-5-32-551"
                | "S-1-5-32-552"
        )
        || (sid.starts_with("S-1-5-21-")
            && matches!(sid.rsplit('-').next(), Some("512" | "518" | "519")))
}

/// TokenIsElevated is inherited metadata, not proof that a filtered token can
/// still exercise admin rights. Verify the OS group/privilege lists instead of
/// trusting a lowered integrity label or a caller-provided boolean alone.
pub(super) fn has_only_user_rights(handle: HANDLE) -> io::Result<bool> {
    let restrictions = token_data(handle, TokenHasRestrictions)?;
    if unsafe { *restrictions.as_ptr().cast::<u32>() } == 0 {
        return Ok(false);
    }
    let user = token_data(handle, TokenUser)?;
    let user = unsafe { &*user.as_ptr().cast::<TOKEN_USER>() };
    if user.User.Attributes & SE_GROUP_USE_FOR_DENY_ONLY as u32 != 0 {
        return Ok(false);
    }
    let groups = token_data(handle, TokenGroups)?;
    let groups = unsafe { &*groups.as_ptr().cast::<TOKEN_GROUPS>() };
    let groups =
        unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) };
    for group in groups {
        if administrative_group(&sid_text(group.Sid)?, group.Attributes)
            && group.Attributes & SE_GROUP_USE_FOR_DENY_ONLY as u32 == 0
        {
            return Ok(false);
        }
    }
    let privileges = token_data(handle, TokenPrivileges)?;
    let privileges = unsafe { &*privileges.as_ptr().cast::<TOKEN_PRIVILEGES>() };
    if privileges.PrivilegeCount > 1 {
        return Ok(false);
    }
    if privileges.PrivilegeCount == 1 {
        let mut traversal = LUID::default();
        check(unsafe { LookupPrivilegeValueW(null(), SE_CHANGE_NOTIFY_NAME, &mut traversal) })?;
        let privilege = privileges.Privileges[0].Luid;
        if privilege.HighPart != traversal.HighPart || privilege.LowPart != traversal.LowPart {
            return Ok(false);
        }
    }
    let ui_access = token_data(handle, TokenUIAccess)?;
    Ok(unsafe { *ui_access.as_ptr().cast::<u32>() } == 0)
}

fn user_default_security(handle: HANDLE) -> io::Result<()> {
    let user = token_data(handle, TokenUser)?;
    // SAFETY: aligned storage contains TOKEN_USER and its SID for both calls.
    let user = unsafe { &*user.as_ptr().cast::<TOKEN_USER>() };
    let owner = TOKEN_OWNER {
        Owner: user.User.Sid,
    };
    check(unsafe {
        SetTokenInformation(
            handle,
            TokenOwner,
            (&owner as *const TOKEN_OWNER).cast(),
            size_of_val(&owner) as u32,
        )
    })?;
    // An elevated token's default DACL can grant only Administrators/SYSTEM.
    // After filtering that would deny the new process access to its own objects
    // and fail DLL initialization. This DACL belongs only to the new token.
    let descriptor = wide(format!(
        "D:(A;;GA;;;SY)(A;;GA;;;{})",
        sid_text(user.User.Sid)?
    ))?;
    let mut raw = null_mut();
    check(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            1,
            &mut raw,
            null_mut(),
        )
    })?;
    let result = (|| {
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = null_mut();
        check(unsafe { GetSecurityDescriptorDacl(raw, &mut present, &mut acl, &mut defaulted) })?;
        if present == 0 || acl.is_null() {
            return Err(error("missing user token DACL"));
        }
        let dacl = TOKEN_DEFAULT_DACL { DefaultDacl: acl };
        check(unsafe {
            SetTokenInformation(
                handle,
                TokenDefaultDacl,
                (&dacl as *const TOKEN_DEFAULT_DACL).cast(),
                size_of_val(&dacl) as u32,
            )
        })
    })();
    // SAFETY: conversion allocated this descriptor with LocalAlloc.
    unsafe {
        LocalFree(raw);
    }
    result
}

fn user_token(handle: HANDLE) -> io::Result<OwnedHandle> {
    let original = identity(handle)?;
    // A linked token can be identification-only outside SYSTEM, and built-in
    // Administrator may have none. Filter the caller's own primary token.
    let limited = restricted_token(handle)?;
    validate_user_token(&original, &identity(limited.as_raw_handle())?)?;
    Ok(limited)
}

fn validate_user_token(original: &Identity, limited: &Identity) -> io::Result<()> {
    if original.sid != limited.sid
        || original.session_id != limited.session_id
        || !limited.is_unprivileged()
        || !limited.restricted_user
        || limited.integrity != "Medium"
    {
        return Err(error(
            "could not obtain the same user's non-elevated medium-integrity token",
        ));
    }
    Ok(())
}

fn same_logon(first: HANDLE, second: HANDLE) -> io::Result<bool> {
    let first = token_data(first, TokenStatistics)?;
    let second = token_data(second, TokenStatistics)?;
    // SAFETY: each allocation contains the requested TOKEN_STATISTICS structure.
    let first = unsafe { (*first.as_ptr().cast::<TOKEN_STATISTICS>()).AuthenticationId };
    let second = unsafe { (*second.as_ptr().cast::<TOKEN_STATISTICS>()).AuthenticationId };
    Ok(first.LowPart == second.LowPart && first.HighPart == second.HighPart)
}

pub(super) fn administrator_source(handle: HANDLE, actual: &Identity) -> io::Result<OwnedHandle> {
    if let Ok(linked) = linked_token(handle) {
        let candidate = identity(linked.as_raw_handle())?;
        if candidate.sid == actual.sid
            && candidate.session_id == actual.session_id
            && candidate.has_elevated_rights()
            && !candidate.is_system
        {
            return Ok(linked);
        }
    }
    // A normal standard account must never be upgraded through a different login.
    if !actual.is_interactive_user() || !actual.restricted_user {
        return Err(error(
            "the paired current user has no linked administrator token; no identity fallback is permitted",
        ));
    }
    let mut login = null_mut();
    // Only the SYSTEM broker can retrieve this token. No active-console/session
    // guessing: it must match the paired process's SID, session AND logon LUID.
    check(unsafe { WTSQueryUserToken(actual.session_id, &mut login) })?;
    let login = own(login)?;
    let login = if identity(login.as_raw_handle())?.has_elevated_rights() {
        login
    } else {
        linked_token(login.as_raw_handle())?
    };
    let candidate = identity(login.as_raw_handle())?;
    if candidate.sid != actual.sid
        || candidate.session_id != actual.session_id
        || !candidate.has_elevated_rights()
        || candidate.is_system
        || !same_logon(handle, login.as_raw_handle())?
    {
        return Err(error(
            "session token is not the paired user's administrator logon",
        ));
    }
    Ok(login)
}

/// The elevated process only supervises a job-contained, non-elevated child.
/// It never opens an endpoint or serves commands. Waiting preserves Task
/// Scheduler's running state; closing the supervisor also terminates its child.
pub fn relaunch_as_user_if_elevated() -> io::Result<Option<i32>> {
    let original = token(unsafe { GetCurrentProcess() })?;
    let current = identity(original.as_raw_handle())?;
    if current.is_unprivileged() {
        return Ok(None);
    }
    if !current.is_interactive_user() {
        return Err(error(
            "user daemon requires a non-elevated interactive user; refusing service accounts/Session 0",
        ));
    }
    let limited = user_token(original.as_raw_handle())?;
    let executable = std::env::current_exe()?;
    let executable = executable
        .to_str()
        .ok_or_else(|| error("invalid Unicode executable path"))?;
    let arguments = std::env::args_os()
        .skip(1)
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| error("invalid Unicode user daemon argument"))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    let Spawned {
        child,
        stdin,
        mut stdout,
        mut stderr,
    } = spawn_with_token(
        executable,
        &arguments,
        None,
        &BTreeMap::new(),
        Some(limited),
    )?;
    drop(stdin);
    let output = std::thread::spawn(move || io::copy(&mut stdout, &mut io::stdout().lock()));
    let errors = std::thread::spawn(move || io::copy(&mut stderr, &mut io::stderr().lock()));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    // Close descendant handles before joining the output drainers.
    drop(child);
    let _ = output.join();
    let _ = errors.join();
    Ok(Some(status))
}

#[cfg(test)]
mod tests;
