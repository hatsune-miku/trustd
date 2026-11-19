use super::*;
use crate::{auth::AuthManager, config::Config};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use localshelld_sync_protocol::owner::testing::{Authenticator, owner_for};

/// Roster device IDs of the two other devices the fixture's owner approved.
struct Devices {
    alice: String,
    bob: String,
    passkey: Authenticator,
    owner: owner::Owner,
    statements: Vec<owner::SignedStatement>,
}

async fn fixture() -> (tempfile::TempDir, AppState, Devices) {
    let directory = tempfile::Builder::new()
        .prefix("trustd-remote [jobs] '")
        .tempdir()
        .unwrap();
    let config = Config {
        password_hash: "unused".into(),
        ..Config::default()
    };
    let mut state = AppState::new(config, AuthManager::new("unused".into(), None))
        // Installed Windows paths are canonicalized and carry the \\?\ prefix.
        .with_storage(directory.path().canonicalize().unwrap().join("config.toml"))
        .unwrap();
    state.mode = DaemonMode::User;
    let passkey = Authenticator::new(b"owner");
    let owner = owner_for(&passkey, "https://backend.example");
    let approve = |name: &str| {
        let identity = Identity::generate().unwrap();
        let statement = owner::Statement::DeviceApprove {
            device_id: identity.id().unwrap(),
            public_key: identity.public_key.clone(),
            name: name.into(),
            issued_at: 1,
        };
        (identity.id().unwrap(), passkey.sign(&owner, &statement))
    };
    let (alice, alice_approval) = approve("alice");
    let (bob, bob_approval) = approve("bob");
    let statements = vec![alice_approval, bob_approval];
    state
        .remote
        .join(owner.clone(), statements.clone())
        .await
        .unwrap();
    (
        directory,
        state,
        Devices {
            alice,
            bob,
            passkey,
            owner,
            statements,
        },
    )
}

fn execution(command: &str) -> Value {
    if cfg!(windows) {
        json!({"command":command,"privilege_level":"current-user"})
    } else {
        json!({"command":command,"sudo":false})
    }
}

#[tokio::test]
async fn jobs_survive_session_close_and_persist_without_replaying() {
    let (directory, state, devices) = fixture().await;
    let session = state
        .remote_open(devices.alice.as_str(), &json!({}))
        .await
        .unwrap();
    let command = if cfg!(windows) {
        "Start-Sleep -Seconds 1; Write-Output 'durable-result'"
    } else {
        "sleep 1; printf durable-result"
    };
    let mut args = execution(command);
    args["session_ref"] = session["session_ref"].clone();
    args["idempotency_key"] = json!("once");
    args["_agent_id"] = json!("agent-a");
    let job = state
        .remote_job_start(devices.alice.as_str(), &args)
        .await
        .unwrap();
    assert_eq!(
        state
            .remote_job_start(devices.alice.as_str(), &args)
            .await
            .unwrap()["id"],
        job["id"]
    );
    let mut changed = args.clone();
    changed["command"] = json!("echo twice");
    assert!(
        state
            .remote_job_start(devices.alice.as_str(), &changed)
            .await
            .is_err()
    );
    state
        .remote_dispatch(
            devices.alice.as_str(),
            json!({"operation":"session_close","arguments":{"session_ref":session["session_ref"]}}),
        )
        .await
        .unwrap();
    let query = json!({"job_id":job["id"]});
    let rebound =
        json!({"job_id":job["id"],"session_ref":"new-delegated-session","_agent_id":"agent-a"});
    assert!(
        state
            .remote_job_action(devices.alice.as_str(), "job_get", &rebound)
            .await
            .is_ok()
    );
    assert!(
        state
            .remote_job_action(
                devices.alice.as_str(),
                "job_get",
                &json!({"job_id":job["id"],"_agent_id":"agent-b"})
            )
            .await
            .is_err()
    );
    let mut retry = args.clone();
    retry["session_ref"] = json!("new-delegated-session");
    assert_eq!(
        state
            .remote_job_start(devices.alice.as_str(), &retry)
            .await
            .unwrap()["id"],
        job["id"]
    );
    assert!(
        state
            .remote_job_action(devices.bob.as_str(), "job_get", &query)
            .await
            .is_err()
    );
    assert!(
        state
            .remote_job_action(
                devices.alice.as_str(),
                "job_get",
                &json!({"job_id":job["id"],"session_ref":"other"})
            )
            .await
            .is_err()
    );
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let status = state
                .remote_job_action(devices.alice.as_str(), "job_get", &query)
                .await
                .unwrap();
            if status["finished_at"].is_number() {
                assert_eq!(status["status"], "succeeded", "{status}");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let output = state
        .remote_job_action(devices.alice.as_str(), "job_output", &query)
        .await
        .unwrap();
    assert!(output["text"].as_str().unwrap().contains("durable-result"));
    let loaded = Runtime::load(&directory.path().join("config.toml")).unwrap();
    assert_eq!(
        loaded.jobs.lock().await[job["id"].as_str().unwrap()].status,
        "succeeded"
    );
    let mut saved = state.remote.jobs.lock().await[job["id"].as_str().unwrap()].clone();
    saved.status = "running".into();
    storage::save_json(
        &state
            .remote
            .directory()
            .unwrap()
            .join("jobs")
            .join(&saved.id)
            .join("job.json"),
        &saved,
    )
    .unwrap();
    let loaded = Runtime::load(&directory.path().join("config.toml")).unwrap();
    assert_eq!(loaded.jobs.lock().await[&saved.id].status, "interrupted");
    state.shutdown().await;
}

#[tokio::test]
async fn only_roster_devices_open_sessions_and_removal_ends_them() {
    let (_directory, state, devices) = fixture().await;
    let stranger = Identity::generate().unwrap().id().unwrap();
    assert!(state.remote_open(&stranger, &json!({})).await.is_err());
    assert!(
        state
            .remote_dispatch(&stranger, json!({"operation":"host_info"}))
            .await
            .is_err()
    );
    let session = state.remote_open(&devices.alice, &json!({})).await.unwrap();
    let reference = session["session_ref"].as_str().unwrap();
    assert!(state.remote.session(&devices.bob, reference).await.is_err());
    // A roster update must come from the pinned owner.
    let impostor = Authenticator::new(b"owner");
    let wrong_owner = owner_for(&impostor, "https://backend.example");
    assert!(
        state
            .apply_roster(localshelld_sync_protocol::RosterResponse {
                owner: wrong_owner,
                statements: vec![],
            })
            .await
            .is_err()
    );
    assert!(
        state
            .remote
            .session(&devices.alice, reference)
            .await
            .is_ok()
    );
    // An owner-signed revocation removes the device and ends its sessions.
    let mut statements = devices.statements.clone();
    statements.push(devices.passkey.sign(
        &devices.owner,
        &owner::Statement::DeviceRevoke {
            device_id: devices.alice.clone(),
            issued_at: 2,
        },
    ));
    state
        .apply_roster(localshelld_sync_protocol::RosterResponse {
            owner: devices.owner.clone(),
            statements,
        })
        .await
        .unwrap();
    assert!(
        state
            .remote
            .session(&devices.alice, reference)
            .await
            .is_err()
    );
    assert!(state.remote_open(&devices.alice, &json!({})).await.is_err());
    assert!(state.remote_open(&devices.bob, &json!({})).await.is_ok());
    state.shutdown().await;
}

#[tokio::test]
async fn granted_credentials_reach_commands_but_are_never_returned() {
    let (_directory, state, devices) = fixture().await;
    state
        .put_secret("REMOTE_GRANTED", "remote-fixture-value")
        .await
        .unwrap();
    // Devices of one owner see every credential name, never values.
    assert_eq!(
        state
            .remote_dispatch(devices.alice.as_str(), json!({"operation":"secrets_list"}))
            .await
            .unwrap(),
        json!({"keys":["REMOTE_GRANTED"]})
    );
    let session = state
        .remote_open(
            devices.alice.as_str(),
            &json!({"secret_keys":["REMOTE_GRANTED"]}),
        )
        .await
        .unwrap();
    let reference = session["session_ref"].clone();
    // There is no plaintext read operation; the value is only injected.
    let error = state
        .remote_dispatch(
            devices.alice.as_str(),
            json!({"operation":"secrets_get","arguments":{"session_ref":reference,"key":"REMOTE_GRANTED"}}),
        )
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("remote-fixture-value"));
    let mut args = execution(if cfg!(windows) {
        "[Console]::Write($env:REMOTE_GRANTED)"
    } else {
        "printf '%s' \"$REMOTE_GRANTED\""
    });
    args["session_ref"] = reference;
    let result = state
        .remote_dispatch(
            devices.alice.as_str(),
            json!({"operation":"run","arguments":args}),
        )
        .await
        .unwrap();
    assert_eq!(result["stdout"], "remote-fixture-value");
    let info = state
        .remote_dispatch(devices.alice.as_str(), json!({"operation":"host_info"}))
        .await
        .unwrap();
    let operations: Vec<_> = info["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|operation| operation["name"].as_str().unwrap())
        .collect();
    assert!(operations.contains(&"run"));
    assert!(!operations.contains(&"secrets_get"));
    state.shutdown().await;
}

#[tokio::test]
async fn one_way_rules_withhold_execution_or_elevation_and_cannot_be_rolled_back() {
    let (_directory, state, devices) = fixture().await;
    let own = state.remote.identity().await.unwrap().id().unwrap();
    let rule = |from: &str, deny| owner::Rule {
        from: from.into(),
        to: own.clone(),
        deny,
    };
    let rules = devices.passkey.sign(
        &devices.owner,
        &owner::Statement::RuleSet {
            rules: vec![
                rule(&devices.bob, owner::Deny::Execution),
                rule(&devices.alice, owner::Deny::Elevation),
            ],
            issued_at: 10,
        },
    );
    let mut statements = devices.statements.clone();
    statements.push(rules);
    state
        .apply_roster(localshelld_sync_protocol::RosterResponse {
            owner: devices.owner.clone(),
            statements: statements.clone(),
        })
        .await
        .unwrap();
    // Bob may not run anything here; Alice may, but never as Administrator/root.
    assert!(state.remote_open(&devices.bob, &json!({})).await.is_err());
    assert!(
        state
            .remote_dispatch(&devices.bob, json!({"operation":"host_info"}))
            .await
            .is_err()
    );
    assert!(state.remote_open(&devices.alice, &json!({})).await.is_ok());
    let elevated = state
        .remote_open(&devices.alice, &json!({"sudo":true}))
        .await
        .unwrap_err();
    assert!(elevated.to_string().contains("Administrator/root"));
    assert_eq!(
        state.remote.status().await.unwrap()["rules"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // A backend cannot drop the rule set (or any seen statement) to relax it.
    assert!(
        state
            .apply_roster(localshelld_sync_protocol::RosterResponse {
                owner: devices.owner.clone(),
                statements: devices.statements.clone(),
            })
            .await
            .is_err()
    );
    assert!(state.remote_open(&devices.bob, &json!({})).await.is_err());
    state.shutdown().await;
}

#[tokio::test]
async fn cancelling_a_job_wakes_its_waiters_and_leaves_its_session_usable() {
    let (_directory, state, devices) = fixture().await;
    let session = state.remote_open(&devices.alice, &json!({})).await.unwrap();
    let reference = session["session_ref"].clone();
    let mut args = execution(if cfg!(windows) {
        "Start-Sleep -Seconds 30"
    } else {
        "sleep 30"
    });
    args["session_ref"] = reference.clone();
    args["idempotency_key"] = json!("long");
    let job = state.remote_job_start(&devices.alice, &args).await.unwrap();
    let query = json!({"job_id":job["id"]});
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while state
            .remote_job_action(&devices.alice, "job_get", &query)
            .await
            .unwrap()["status"]
            != "running"
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    // A wait that elapses reports the job as still running.
    let brief = json!({"job_id":job["id"],"timeout_seconds":1});
    let waited = state
        .remote_job_action(&devices.alice, "job_wait", &brief)
        .await
        .unwrap();
    assert_eq!(waited["status"], "running", "{waited}");
    let patient = json!({"job_id":job["id"],"timeout_seconds":60});
    let waiter = {
        let state = state.clone();
        let alice = devices.alice.clone();
        tokio::spawn(async move {
            state
                .remote_job_action(&alice, "job_wait", &patient)
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    state
        .remote_dispatch(
            &devices.alice,
            json!({"operation":"job_cancel","arguments":query}),
        )
        .await
        .unwrap();
    let woken = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
        .await
        .expect("cancelling wakes job_wait")
        .unwrap();
    assert_eq!(woken["status"], "cancelled");
    let mut run = execution("echo still-here");
    run["session_ref"] = reference;
    let result = state
        .remote_dispatch(&devices.alice, json!({"operation":"run","arguments":run}))
        .await
        .unwrap();
    assert!(
        result["stdout"].as_str().unwrap().contains("still-here"),
        "{result}"
    );
    state.shutdown().await;
}

#[tokio::test]
async fn a_finished_job_can_be_awaited_and_its_output_read_in_the_remote_shell() {
    let (_directory, state, devices) = fixture().await;
    let session = state.remote_open(&devices.alice, &json!({})).await.unwrap();
    let mut args = execution("echo first-line; echo job-output-tail");
    args["session_ref"] = session["session_ref"].clone();
    args["idempotency_key"] = json!("awaited");
    let job = state.remote_job_start(&devices.alice, &args).await.unwrap();
    for internal in ["fingerprint", "owner"] {
        assert!(job.get(internal).is_none(), "{job}");
    }
    let wait = json!({"job_id":job["id"],"timeout_seconds":30});
    let finished = state
        .remote_job_action(&devices.alice, "job_wait", &wait)
        .await
        .unwrap();
    assert_eq!(finished["status"], "succeeded", "{finished}");
    assert!(finished["stdout_size"].as_u64().unwrap() > 0, "{finished}");
    assert!(finished["result"].get("identity").is_none(), "{finished}");
    // The job's output stays on the target; a shell there can process it.
    let path = finished["stdout_path"].as_str().unwrap();
    let mut read = execution(&if cfg!(windows) {
        format!("Get-Content -LiteralPath '{}'", path.replace('\'', "''"))
    } else {
        format!("cat '{}'", path.replace('\'', "'\\''"))
    });
    read["session_ref"] = session["session_ref"].clone();
    let result = state
        .remote_dispatch(&devices.alice, json!({"operation":"run","arguments":read}))
        .await
        .unwrap();
    assert!(
        result["stdout"]
            .as_str()
            .unwrap()
            .contains("job-output-tail"),
        "{result}"
    );
    let tail = state
        .remote_job_action(
            &devices.alice,
            "job_output",
            &json!({"job_id":job["id"],"tail":18}),
        )
        .await
        .unwrap();
    let text = tail["text"].as_str().unwrap();
    assert!(
        text.contains("tail") && !text.contains("first-line"),
        "{tail}"
    );
    assert!(tail.get("data").is_none() && tail["eof"] == true, "{tail}");
    state.shutdown().await;
}

#[test]
fn chunks_are_text_or_base64_and_never_split_a_character() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("chunk");
    std::fs::write(&path, "aé😀z").unwrap();
    let read = |args: Value| jobs::read_chunk(path.clone(), &args).unwrap();
    let whole = read(json!({}));
    assert_eq!(whole["text"], "aé😀z");
    assert!(whole.get("data").is_none());
    // "a" plus half of "é": the chunk stops before the split character.
    let first = read(json!({"limit":2}));
    assert_eq!(
        (first["text"].as_str(), first["next_offset"].as_u64()),
        (Some("a"), Some(1))
    );
    let second = read(json!({"offset":1,"limit":6}));
    assert_eq!(second["text"], "é😀");
    // A tail that begins inside the emoji starts at the next character.
    let tail = read(json!({"tail":3}));
    assert_eq!(
        (tail["text"].as_str(), tail["offset"].as_u64()),
        (Some("z"), Some(7))
    );
    let base64 = read(json!({"encoding":"base64"}));
    assert_eq!(base64["data"], STANDARD.encode("aé😀z"));
    assert!(base64.get("text").is_none());
    std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
    let binary = read(json!({}));
    assert_eq!(
        (binary["encoding"].as_str(), binary["data"].as_str()),
        (Some("base64"), Some("//4A"))
    );
    assert!(jobs::read_chunk(path.clone(), &json!({"encoding":"hex"})).is_err());
}

#[tokio::test]
async fn targets_resolve_by_id_name_or_id_prefix() {
    let (_directory, state, devices) = fixture().await;
    for target in [
        devices.alice.as_str(),
        "alice",
        "ALICE",
        &devices.alice[..8],
    ] {
        assert_eq!(state.remote.resolve(target).await.unwrap(), devices.alice);
    }
    let unknown = state.remote.resolve("carol").await.unwrap_err().to_string();
    assert!(
        unknown.contains("alice (") && unknown.contains("bob ("),
        "{unknown}"
    );
    // A short prefix could name a different device tomorrow.
    assert!(state.remote.resolve(&devices.bob[..4]).await.is_err());
    state.shutdown().await;
}
