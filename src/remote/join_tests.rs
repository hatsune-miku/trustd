//! End to end: real backend, real daemons, one owner signature per device.
use super::*;
use crate::{auth::AuthManager, config::Config, sync::Action};
use localshelld_sync_protocol::owner::testing::{Authenticator, owner_for};
use std::time::Duration;

fn daemon(directory: &Path) -> AppState {
    let config = Config {
        password_hash: "unused".into(),
        bind: "127.0.0.1:1".parse().unwrap(),
        privileged_daemon: "127.0.0.1:2".parse().unwrap(),
        ..Config::default()
    };
    AppState::user(config, AuthManager::new("unused".into(), None))
        .unwrap()
        .with_storage(directory.join("config.toml"))
        .unwrap()
}

async fn local(app: &axum::Router, path: &str, body: Value) -> (u16, Value) {
    use tower::ServiceExt;
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::post(path)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// What the owner does on the approval page: read the exact statement and
/// sign it with the passkey. No login is needed for this single tap.
async fn approve(url: &str, passkey: &Authenticator, owner: &owner::Owner, user_code: &str) {
    let client = reqwest::Client::new();
    let inspected: Value = client
        .post(format!("{url}/sync/v1/device/inspect"))
        .json(&json!({"user_code":user_code}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let statement = inspected["statement"].as_str().unwrap();
    let assertion = passkey.assert(owner, &owner::challenge(statement), 0x05);
    let response = client
        .post(format!("{url}/sync/v1/device/approve"))
        .json(&json!({"user_code":user_code,"assertion":assertion}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
}

#[tokio::test]
async fn devices_join_with_one_owner_signature_and_reach_each_other_over_the_backend() {
    let backend = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://localhost:{}", listener.local_addr().unwrap().port());
    let app = localshelld_sync_server::App::open(backend.path(), &url).unwrap();
    let passkey = Authenticator::new(b"owner");
    let owner = owner_for(&passkey, &url);
    app.initialize_owner_for_tests(owner.keys.clone()).await;
    let server = tokio::spawn(async move {
        axum::serve(listener, localshelld_sync_server::router(app))
            .await
            .unwrap()
    });

    let mut devices = Vec::new();
    for name in ["laptop", "server"] {
        let directory = tempfile::tempdir().unwrap();
        let state = daemon(directory.path());
        let started = state
            .sync_action(Action::Start {
                url: url.clone(),
                name: name.into(),
            })
            .await
            .unwrap();
        assert_eq!(
            state.sync_action(Action::Poll).await.unwrap()["pending"],
            true
        );
        approve(
            &url,
            &passkey,
            &owner,
            started["user_code"].as_str().unwrap(),
        )
        .await;
        // The service rate-limits polling to one request every few seconds.
        tokio::time::sleep(Duration::from_millis(2100)).await;
        assert_eq!(
            state.sync_action(Action::Poll).await.unwrap()["connected"],
            true
        );
        assert_eq!(state.remote.status().await.unwrap()["joined"], true);
        devices.push((directory, state));
    }
    let laptop = &devices[0].1;
    let server_device = &devices[1].1;
    let server_id = server_device.remote.identity().await.unwrap().id().unwrap();
    let laptop_id = laptop.remote.identity().await.unwrap().id().unwrap();
    // The second device saw the first in its join response; the first learns
    // about the second from the owner-signed roster.
    assert!(
        server_device.remote.status().await.unwrap()["peers"]
            .get(&laptop_id)
            .is_some()
    );
    laptop.sync_action(Action::Refresh).await.unwrap();
    assert!(
        laptop.remote.status().await.unwrap()["peers"]
            .get(&server_id)
            .is_some()
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let connected = [laptop, server_device].iter().all(|state| {
                state
                    .remote
                    .node
                    .try_read()
                    .is_ok_and(|node| node.as_ref().is_some_and(|n| n.connected()))
            });
            if connected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("both devices connect to the backend relay");
    let info = laptop
        .remote_invoke_human(&json!({"target":server_id,"operation":"host_info"}))
        .await
        .unwrap();
    assert_eq!(info["platform"], std::env::consts::OS);

    // Devices answer to their names, and files move between them through the
    // daemons, in several relay-sized chunks, without the caller carrying them.
    let session = laptop
        .remote_invoke(&json!({"target":"SERVER","operation":"session_open","arguments":{}}))
        .await
        .unwrap()["session_ref"]
        .clone();
    let files = tempfile::tempdir().unwrap();
    let payload: Vec<u8> = (0..2 * 1024 * 1024 + 12345)
        .map(|i: usize| (i * 7 % 251) as u8)
        .collect();
    let source = files.path().join("source.bin");
    std::fs::write(&source, &payload).unwrap();
    let remote = files.path().join("remote.bin");
    let transfer = |operation: &'static str, arguments: Value| async move {
        laptop
            .remote_invoke(&json!({"target":"server","operation":operation,"arguments":arguments}))
            .await
    };
    let push = json!({"session_ref":session,"local_path":source,"path":remote});
    let pushed = transfer("file_push", push.clone()).await.unwrap();
    assert_eq!(pushed["size"], payload.len());
    assert_eq!(std::fs::read(&remote).unwrap(), payload);
    let back = files.path().join("back.bin");
    let pull = json!({"session_ref":session,"path":remote,"local_path":back});
    let pulled = transfer("file_pull", pull.clone()).await.unwrap();
    assert_eq!(pulled["sha256"], pushed["sha256"]);
    assert_eq!(std::fs::read(&back).unwrap(), payload);
    // Neither direction overwrites an existing file unless asked to.
    assert!(transfer("file_pull", pull.clone()).await.is_err());
    std::fs::write(&source, b"replacement").unwrap();
    assert!(transfer("file_push", push.clone()).await.is_err());
    let mut replace = push;
    replace["replace"] = json!(true);
    transfer("file_push", replace).await.unwrap();
    let mut replace = pull;
    replace["replace"] = json!(true);
    transfer("file_pull", replace).await.unwrap();
    assert_eq!(std::fs::read(&back).unwrap(), b"replacement");

    // Management login: the daemon has its own one-time statement signed by
    // the owner's passkey on the backend, and verifies the signature itself.
    let app = crate::server::router(laptop.clone());
    let (status, started) = local(&app, "/v1/admin/login/passkey/start", json!({})).await;
    assert_eq!(status, 200, "{started}");
    let poll = json!({"login_id":started["login_id"]});
    assert_eq!(
        local(&app, "/v1/admin/login/passkey/poll", poll.clone())
            .await
            .1["pending"],
        true
    );
    let client = reqwest::Client::new();
    let inspected: Value = client
        .post(format!("{url}/sync/v1/signatures/inspect"))
        .json(&json!({"user_code":started["user_code"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let statement = inspected["statement"].as_str().unwrap();
    assert!(statement.contains("management_login"));
    let response = client
        .post(format!("{url}/sync/v1/signatures/approve"))
        .json(&json!({"user_code":started["user_code"],
            "assertion":passkey.assert(&owner, &owner::challenge(statement), 0x05)}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let (status, session) = local(&app, "/v1/admin/login/passkey/poll", poll.clone()).await;
    assert_eq!(status, 200, "{session}");
    assert!(session["session"].is_string());
    // The same login cannot be used twice.
    assert_eq!(
        local(&app, "/v1/admin/login/passkey/poll", poll).await.0,
        400
    );

    for (_, state) in &devices {
        state.shutdown().await;
    }
    server.abort();
}

#[tokio::test]
async fn a_revoked_identity_is_replaced_when_the_device_joins_again() {
    let backend = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://localhost:{}", listener.local_addr().unwrap().port());
    let app = localshelld_sync_server::App::open(backend.path(), &url).unwrap();
    let passkey = Authenticator::new(b"owner");
    let owner = owner_for(&passkey, &url);
    app.initialize_owner_for_tests(owner.keys.clone()).await;
    let server = tokio::spawn(async move {
        axum::serve(listener, localshelld_sync_server::router(app))
            .await
            .unwrap()
    });
    let directory = tempfile::tempdir().unwrap();
    let state = daemon(directory.path());
    let revoked = state.remote.identity().await.unwrap().id().unwrap();
    // The owner revokes this identity before it ever finished joining.
    let revoke = passkey.sign(
        &owner,
        &owner::Statement::DeviceRevoke {
            device_id: revoked.clone(),
            issued_at: chrono::Utc::now().timestamp(),
        },
    );
    let response = reqwest::Client::new()
        .post(format!("{url}/sync/v1/statements"))
        .json(&revoke)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    // Joining again asks with a new identity instead of failing.
    let started = state
        .sync_action(Action::Start {
            url: url.clone(),
            name: "laptop".into(),
        })
        .await
        .unwrap();
    let replaced = state.remote.identity().await.unwrap().id().unwrap();
    assert_ne!(replaced, revoked);
    let inspected: Value = reqwest::Client::new()
        .post(format!("{url}/sync/v1/device/inspect"))
        .json(&json!({"user_code":started["user_code"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(inspected["device_id"], replaced.as_str());
    state.shutdown().await;
    server.abort();
}
