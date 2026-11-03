use super::*;
use std::io::Read;
use windows_sys::Win32::System::SystemServices::{SE_GROUP_ENABLED, SE_GROUP_USE_FOR_DENY_ONLY};

#[test]
fn filtering_keeps_the_user_and_logon_sids_but_removes_admin_groups() {
    for sid in [
        "S-1-5-32-544",
        "S-1-5-32-547",
        "S-1-5-32-551",
        "S-1-5-21-1-2-3-512",
        "S-1-5-21-1-2-3-519",
    ] {
        assert!(administrative_group(sid, 0));
    }
    for sid in [
        "S-1-5-21-1-2-3-500",
        "S-1-5-5-1234-5678",
        "S-1-5-32-545",
        "S-1-5-11",
    ] {
        assert!(!administrative_group(sid, 0));
    }
    assert!(administrative_group(
        "S-1-5-21-1-2-3-9999",
        SE_GROUP_OWNER as u32
    ));
}

#[test]
fn interactive_owner_accepts_administrator_but_not_service_accounts() {
    let mut owner = current_identity().unwrap();
    owner.sid = "S-1-5-21-1234-500".into();
    owner.elevated = true;
    owner.is_system = false;
    owner.session_id = 1;
    assert!(owner.is_interactive_user());
    for sid in ["S-1-5-18", "S-1-5-19", "S-1-5-20"] {
        owner.sid = sid.into();
        assert!(!owner.is_interactive_user());
    }
    owner.sid = "S-1-5-21-1234-500".into();
    owner.session_id = 0;
    assert!(!owner.is_interactive_user());
}

#[test]
fn reduced_identity_must_preserve_the_user_and_session_without_elevation() {
    let mut original = current_identity().unwrap();
    original.elevated = true;
    original.integrity = "High".into();
    original.is_system = false;
    let mut limited = original.clone();
    limited.elevated = false;
    limited.restricted_user = true;
    limited.integrity = "Medium".into();
    validate_user_token(&original, &limited).unwrap();
    for mutation in 0..5 {
        let mut changed = limited.clone();
        match mutation {
            0 => changed.sid.push_str("-1"),
            1 => changed.session_id += 1,
            2 => changed.restricted_user = false,
            3 => changed.integrity = "High".into(),
            _ => changed.is_system = true,
        }
        assert!(validate_user_token(&original, &changed).is_err());
    }
}

#[test]
fn restricted_token_removes_admin_rights_and_runs_a_medium_child() {
    let source = token(unsafe { GetCurrentProcess() }).unwrap();
    let original = identity(source.as_raw_handle()).unwrap();
    // Also exercise the no-linked-token reduction on ordinary CI accounts.
    // Elevated CI and the local elevated regression run verify real filtering.
    if original.is_system {
        return;
    }
    let limited = restricted_token(source.as_raw_handle()).unwrap();
    let reduced = identity(limited.as_raw_handle()).unwrap();
    validate_user_token(&original, &reduced).unwrap();
    assert_eq!(
        reduced.elevated, original.elevated,
        "raw Windows flag is preserved"
    );
    assert!(same_logon(source.as_raw_handle(), limited.as_raw_handle()).unwrap());
    let groups = token_data(limited.as_raw_handle(), TokenGroups).unwrap();
    let groups = unsafe { &*groups.as_ptr().cast::<TOKEN_GROUPS>() };
    let groups =
        unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) };
    for group in groups {
        if sid_text(group.Sid).unwrap() == "S-1-5-32-544" {
            assert_eq!(group.Attributes & SE_GROUP_ENABLED as u32, 0);
            assert_ne!(group.Attributes & SE_GROUP_USE_FOR_DENY_ONLY as u32, 0);
        }
    }
    let privileges = token_data(limited.as_raw_handle(), TokenPrivileges).unwrap();
    let privileges = unsafe { &*privileges.as_ptr().cast::<TOKEN_PRIVILEGES>() };
    assert!(
        privileges.PrivilegeCount <= 1,
        "only traversal privilege may remain"
    );

    // An elevated service-session token has no interactive desktop for the
    // reduced child. Production rejects this identity before relaunching.
    if original.elevated && !original.is_interactive_user() {
        return;
    }

    let probe = "[Console]::Out.Write([Security.Principal.WindowsIdentity]::GetCurrent().User.Value); [Console]::Error.Write('filtered'); exit 7";
    let mut child = spawn_with_token(
        crate::system_directory()
            .unwrap()
            .join("WindowsPowerShell/v1.0/powershell.exe")
            .to_str()
            .unwrap(),
        &["-NoProfile", "-NonInteractive", "-Command", probe],
        None,
        &BTreeMap::new(),
        Some(limited),
    )
    .unwrap();
    drop(child.stdin);
    let mut output = String::new();
    let mut errors = String::new();
    child.stdout.read_to_string(&mut output).unwrap();
    child.stderr.read_to_string(&mut errors).unwrap();
    assert_eq!(output, original.sid);
    assert_eq!(errors, "filtered");
    validate_user_token(&original, &reduced).unwrap();
    validate_user_token(&original, &child.child.identity).unwrap();
    for _ in 0..200 {
        if let Some(status) = child.child.try_wait().unwrap() {
            assert_eq!(status, 7);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("restricted child did not exit");
}

#[test]
fn elevated_token_is_reduced_without_changing_its_logon() {
    let source = token(unsafe { GetCurrentProcess() }).unwrap();
    if !identity(source.as_raw_handle()).unwrap().elevated {
        return;
    }
    let limited = user_token(source.as_raw_handle()).unwrap();
    validate_user_token(
        &identity(source.as_raw_handle()).unwrap(),
        &identity(limited.as_raw_handle()).unwrap(),
    )
    .unwrap();
    assert!(same_logon(source.as_raw_handle(), limited.as_raw_handle()).unwrap());
}

#[test]
#[ignore = "run as SYSTEM with TRUSTD_TEST_LOGIN_SESSION set to an explicit interactive admin session"]
fn system_recovers_admin_only_for_the_same_filtered_logon() {
    assert!(current_identity().unwrap().is_system);
    let session = std::env::var("TRUSTD_TEST_LOGIN_SESSION")
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert_ne!(session, 0);
    let mut login = null_mut();
    check(unsafe { WTSQueryUserToken(session, &mut login) }).unwrap();
    let login = own(login).unwrap();
    let login = if identity(login.as_raw_handle()).unwrap().elevated {
        login
    } else {
        linked_token(login.as_raw_handle()).unwrap()
    };
    let limited = user_token(login.as_raw_handle()).unwrap();
    let actual = identity(limited.as_raw_handle()).unwrap();
    assert!(actual.is_unprivileged());
    assert!(actual.restricted_user);
    assert_eq!(actual.session_id, session);
    let recovered = administrator_source(limited.as_raw_handle(), &actual).unwrap();
    let administrator = identity(recovered.as_raw_handle()).unwrap();
    assert!(administrator.elevated);
    assert!(!administrator.is_system);
    assert_eq!(administrator.sid, actual.sid);
    assert_eq!(administrator.session_id, actual.session_id);
    assert!(same_logon(limited.as_raw_handle(), recovered.as_raw_handle()).unwrap());

    let mut wrong = actual.clone();
    wrong.sid.push_str("-1");
    assert!(administrator_source(limited.as_raw_handle(), &wrong).is_err());
    wrong = actual;
    wrong.session_id = 0;
    assert!(administrator_source(limited.as_raw_handle(), &wrong).is_err());
}
