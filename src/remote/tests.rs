use super::*;
use crate::{auth::AuthManager, config::Config};
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
