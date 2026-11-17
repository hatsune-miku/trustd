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
    for (_, state) in &devices {
        state.shutdown().await;
    }
    server.abort();
}
