use super::*;
use crate::{auth::AuthManager, config::Config};

async fn fixture() -> (tempfile::TempDir, AppState) {
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
    state.remote.initialize().await.unwrap();
    let mut saved = state.remote.saved.lock().await;
    for owner in ["alice", "bob"] {
        saved.as_mut().unwrap().peers.insert(
            owner.into(),
            Peer {
                label: owner.into(),
                public_key: Identity::generate().unwrap().public_key.clone(),
                allow_execution: true,
                sudo: false,
                secret_keys: vec![],
            },
        );
    }
    drop(saved);
    (directory, state)
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
    let (directory, state) = fixture().await;
    let session = state.remote_open("alice", &json!({})).await.unwrap();
    let command = if cfg!(windows) {
        "Start-Sleep -Seconds 1; Write-Output 'durable-result'"
    } else {
        "sleep 1; printf durable-result"
    };
    let mut args = execution(command);
    args["session_ref"] = session["session_ref"].clone();
    args["idempotency_key"] = json!("once");
    args["_agent_id"] = json!("agent-a");
    let job = state.remote_job_start("alice", &args).await.unwrap();
    assert_eq!(
        state.remote_job_start("alice", &args).await.unwrap()["id"],
        job["id"]
    );
    let mut changed = args.clone();
    changed["command"] = json!("echo twice");
    assert!(state.remote_job_start("alice", &changed).await.is_err());
    state
        .remote_dispatch(
            "alice",
            json!({"operation":"session_close","arguments":{"session_ref":session["session_ref"]}}),
        )
        .await
        .unwrap();
    let query = json!({"job_id":job["id"]});
    let rebound =
        json!({"job_id":job["id"],"session_ref":"new-delegated-session","_agent_id":"agent-a"});
    assert!(
        state
            .remote_job_action("alice", "job_get", &rebound)
            .await
            .is_ok()
    );
    assert!(
        state
            .remote_job_action(
                "alice",
                "job_get",
                &json!({"job_id":job["id"],"_agent_id":"agent-b"})
            )
            .await
            .is_err()
    );
    let mut retry = args.clone();
    retry["session_ref"] = json!("new-delegated-session");
    assert_eq!(
        state.remote_job_start("alice", &retry).await.unwrap()["id"],
        job["id"]
    );
    assert!(
        state
            .remote_job_action("bob", "job_get", &query)
            .await
            .is_err()
    );
    assert!(
        state
            .remote_job_action(
                "alice",
                "job_get",
                &json!({"job_id":job["id"],"session_ref":"other"})
            )
            .await
            .is_err()
    );
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let status = state
                .remote_job_action("alice", "job_get", &query)
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
        .remote_job_action("alice", "job_output", &query)
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
async fn grants_restrict_elevation_credentials_and_peer_sessions() {
    let (_directory, state) = fixture().await;
    assert!(
        state
            .remote_open("alice", &json!({"sudo":true}))
            .await
            .is_err()
    );
    assert!(
        state
            .remote_open("alice", &json!({"secret_keys":["UNGRANTED"]}))
            .await
            .is_err()
    );
    let session = state.remote_open("alice", &json!({})).await.unwrap();
    assert!(
        state
            .remote
            .session("bob", session["session_ref"].as_str().unwrap())
            .await
            .is_err()
    );
    state.revoke_remote_peer("alice").await;
    assert!(
        state
            .remote
            .session("alice", session["session_ref"].as_str().unwrap())
            .await
            .is_err()
    );
    state.shutdown().await;
}
