use super::*;
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use localshelld_sync_protocol::owner::testing::{Authenticator, owner_for};
use tower::ServiceExt;

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn setup() -> (tempfile::TempDir, App, Router, Tokens, Tokens) {
    let directory = tempfile::tempdir().unwrap();
    let app = App::open(directory.path(), "https://sync.example.com").unwrap();
    let (browser, device) = {
        let mut store = app.store.lock().await;
        (
            app.new_session(&mut store, "Test browser".into(), true, None)
                .unwrap(),
            app.new_session(&mut store, "Test device".into(), false, None)
                .unwrap(),
        )
    };
    let router = router(app.clone());
    (directory, app, router, browser, device)
}

#[test]
fn second_server_cannot_start_a_competing_bot_worker_on_the_same_store() {
    let directory = tempfile::tempdir().unwrap();
    let first = App::open(directory.path(), "https://sync.example.com").unwrap();
    assert!(App::open(directory.path(), "https://sync.example.com").is_err());
    drop(first);
    assert!(App::open(directory.path(), "https://sync.example.com").is_ok());
}

#[tokio::test]
async fn telegram_configuration_is_private_cas_persisted_and_legacy_compatible() {
    let (directory, state, app, _, device) = setup().await;
    let config = localshelld_telegram::Config {
        bot_token: "123456:abcdefghijklmnopqrstuvwxyz".into(),
        bot_username: "fixture_bot".into(),
        enabled: false,
        binding: None,
    };
    {
        let mut store = state.store.lock().await;
        let mut next = store.saved.clone();
        next.vault.telegram = Some(config.clone());
        store.commit(next).unwrap();
    }
    let command = json!({"expected_revision":0,"command":{"action":"status"}});
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/telegram",
            Some(command.clone()),
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, body) = call(
        &app,
        "POST",
        "/sync/v1/telegram",
        Some(command),
        Some(&device.access_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["configured"], true);
    assert!(!body.to_string().contains(&config.bot_token));
    let (_, old) = call(
        &app,
        "GET",
        "/sync/v1/vault",
        None,
        Some(&device.access_token),
    )
    .await;
    assert!(old.get("telegram").is_none());
    let mut headers = HeaderMap::new();
    headers.insert("x-localshelld-features", "telegram-v1".parse().unwrap());
    assert_eq!(
        wire_snapshot(&state.store.lock().await.saved.vault, &headers)["telegram"]["bot_token"],
        config.bot_token
    );
    let (status, _) = call(
        &app,
        "PUT",
        "/sync/v1/vault",
        Some(json!({"expected_revision":0,"credentials":{"KEY":"new"}})),
        Some(&device.access_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(state.store.lock().await.saved.vault.telegram == Some(config.clone()));
    let (status, _) = call(
        &app,
        "POST",
        "/sync/v1/telegram",
        Some(json!({"expected_revision":0,"command":{"action":"remove"}})),
        Some(&device.access_token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    state.telegram.stop().await;
    drop(app);
    drop(state);
    assert!(
        !std::fs::read(directory.path().join("state.aes"))
            .unwrap()
            .windows(config.bot_token.len())
            .any(|v| v == config.bot_token.as_bytes())
    );
    let reopened = App::open(directory.path(), "https://sync.example.com").unwrap();
    assert!(reopened.store.lock().await.saved.vault.telegram == Some(config));
}

#[tokio::test]
async fn telegram_binding_capability_belongs_to_the_cloud_device_session() {
    let (_directory, state, app, browser, device) = setup().await;
    let config = localshelld_telegram::Config {
        bot_token: "123456:abcdefghijklmnopqrstuvwxyz".into(),
        bot_username: "fixture_bot".into(),
        enabled: false,
        binding: None,
    };
    {
        let mut store = state.store.lock().await;
        let mut next = store.saved.clone();
        next.vault.telegram = Some(config);
        store.commit(next).unwrap();
    }
    // Generate without starting an external Telegram poller in this test.
    let (_, body) = call(
        &app,
        "POST",
        "/sync/v1/telegram",
        Some(json!({"expected_revision":0,"command":{"action":"status"}})),
        Some(&device.access_token),
    )
    .await;
    assert_eq!(body["configured"], true);
    state.telegram.stop().await;
    let sid = {
        let store = state.store.lock().await;
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {}", device.access_token).parse().unwrap(),
        );
        state.authenticate(&store, &headers).unwrap().sid
    };
    // A fake code, even presented by another authenticated device, is rejected.
    for token in [&device.access_token, &browser.access_token] {
        assert_eq!(
            call(
                &app,
                "POST",
                "/sync/v1/telegram",
                Some(
                    json!({"expected_revision":0,"command":{"action":"bind_poll","code":"unknown"}})
                ),
                Some(token)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert!(!sid.is_empty());
    state.telegram.stop().await;
}

#[tokio::test]
async fn vault_requires_authentication_and_origin_is_enforced() {
    let (_dir, _state, app, _, device) = setup().await;
    for (method, path, body) in [
        ("GET", "/sync/v1/vault", None),
        (
            "PUT",
            "/sync/v1/vault",
            Some(json!({"expected_revision":0,"credentials":{}})),
        ),
        ("GET", "/sync/v1/sessions", None),
        (
            "POST",
            "/sync/v1/telegram/record",
            Some(json!({"device":"fixture", "choice":"accept", "prompt":{
                "id":"fixture", "agent_name":"agent", "request_reason":"test",
                "scope":"user", "expires_at_ms":0
            }})),
        ),
        (
            "POST",
            "/sync/v1/telegram/agents",
            Some(json!({"device":"fixture", "agents":[], "acknowledgements":[]})),
        ),
        ("GET", "/sync/v1/roster", None),
        (
            "POST",
            "/sync/v1/statements/prepare",
            Some(json!({"kind":"device_revoke","device_id":"0".repeat(64)})),
        ),
    ] {
        assert_eq!(
            call(&app, method, path, body, None).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        call(
            &app,
            "GET",
            "/sync/v1/sessions",
            None,
            Some(&device.access_token)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let response = app
        .oneshot(
            Request::get("/sync/v1/info")
                .header("origin", "https://attacker.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn bootstrap_requires_the_private_code_and_never_returns_it() {
    let directory = tempfile::tempdir().unwrap();
    let app = router(App::open(directory.path(), "https://sync.example.com").unwrap());
    let code = std::fs::read_to_string(directory.path().join("bootstrap-code")).unwrap();
    let (_, info) = call(&app, "GET", "/sync/v1/info", None, None).await;
    assert_eq!(info["initialized"], false);
    assert!(!info.to_string().contains(&code));
    let path = "/sync/v1/passkeys/register/start";
    assert_eq!(
        call(
            &app,
            "POST",
            path,
            Some(json!({"name":"first","bootstrap_code":"wrong"})),
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, challenge) = call(
        &app,
        "POST",
        path,
        Some(json!({"name":"first","bootstrap_code":code})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        challenge["options"]["publicKey"]["rp"]["id"],
        "sync.example.com"
    );
    assert!(!challenge.to_string().contains(&code));
}

#[tokio::test]
async fn concurrent_full_snapshot_writes_use_compare_and_swap_and_wake_readers() {
    let (_directory, state, app, _, device) = setup().await;
    let mut revision = state.revision.subscribe();
    let token = Some(device.access_token.as_str());
    let a = json!({"expected_revision":0,"credentials":{"TOKEN_A":"value-a"}});
    let b = json!({"expected_revision":0,"credentials":{"TOKEN_B":"value-b"}});
    let (a, b) = tokio::join!(
        call(&app, "PUT", "/sync/v1/vault", Some(a), token),
        call(&app, "PUT", "/sync/v1/vault", Some(b), token)
    );
    assert!(
        (a.0 == StatusCode::OK && b.0 == StatusCode::CONFLICT)
            || (b.0 == StatusCode::OK && a.0 == StatusCode::CONFLICT)
    );
    tokio::time::timeout(Duration::from_secs(1), revision.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*revision.borrow(), 1);
    let (_, snapshot) = call(&app, "GET", "/sync/v1/vault", None, token).await;
    assert_eq!(snapshot["revision"], 1);
    assert_eq!(snapshot["credentials"].as_object().unwrap().len(), 1);
    let (status, empty) = call(
        &app,
        "PUT",
        "/sync/v1/vault",
        Some(json!({"expected_revision":1,"credentials":{}})),
        token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty["revision"], 2);
    assert_eq!(empty["credentials"], json!({}));
    assert_eq!(
        call(
            &app,
            "PUT",
            "/sync/v1/vault",
            Some(json!({"expected_revision":2,"credentials":{"lower":"no"}})),
            token
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn refresh_rotation_replay_revokes_the_entire_device_session() {
    let (_directory, _state, app, _, device) = setup().await;
    let body = json!({"refresh_token":device.refresh_token});
    let (status, renewed) = call(
        &app,
        "POST",
        "/sync/v1/session/refresh",
        Some(body.clone()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(renewed["refresh_token"], body["refresh_token"]);
    assert_eq!(
        call(
            &app,
            "GET",
            "/sync/v1/vault",
            None,
            renewed["access_token"].as_str()
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "POST", "/sync/v1/session/refresh", Some(body), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &app,
            "GET",
            "/sync/v1/vault",
            None,
            renewed["access_token"].as_str()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &app,
            "GET",
            "/sync/v1/vault",
            None,
            Some(&device.access_token)
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn forged_refresh_does_not_revoke_a_known_session_and_jwt_audience_is_checked() {
    let (_directory, state, app, _, device) = setup().await;
    let sid = device.refresh_token.split_once('.').unwrap().0;
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/session/refresh",
            Some(json!({"refresh_token":format!("{sid}.invalid")})),
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &app,
            "GET",
            "/sync/v1/vault",
            None,
            Some(&device.access_token)
        )
        .await
        .0,
        StatusCode::OK
    );
    let store = state.store.lock().await;
    let mut claims = Claims {
        sub: store.saved.user_id.to_string(),
        sid: sid.into(),
        iss: state.origin.clone(),
        aud: "wrong".into(),
        exp: Utc::now().timestamp() + 30,
        iat: Utc::now().timestamp(),
    };
    let key = EncodingKey::from_secret(store.saved.jwt_key.as_bytes());
    let wrong_audience =
        jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &key).unwrap();
    claims.aud = "localshelld-sync".into();
    claims.iss = "https://wrong.example".into();
    let wrong_issuer = jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &key).unwrap();
    claims.iss = state.origin.clone();
    claims.exp = Utc::now().timestamp() - 10;
    let expired = jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &key).unwrap();
    drop(store);
    for token in [wrong_audience, wrong_issuer, expired] {
        assert_eq!(
            call(&app, "GET", "/sync/v1/vault", None, Some(&token))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
}

/// Make `passkey` the service's genesis owner key, as initialization does.
async fn initialize_owner(app: &App) -> (Authenticator, Owner) {
    let passkey = Authenticator::new(b"owner-passkey");
    let owner = owner_for(&passkey, "https://sync.example.com");
    let mut store = app.store.lock().await;
    let mut next = store.saved.clone();
    next.genesis = owner.keys.clone();
    store.commit(next).unwrap();
    (passkey, owner)
}

fn device_statement(identity: &trustd_remote::Identity, name: &str) -> String {
    Statement::DeviceApprove {
        device_id: identity.id().unwrap(),
        public_key: identity.public_key.clone(),
        name: name.into(),
        issued_at: Utc::now().timestamp(),
    }
    .to_text()
}

/// Run the join flow up to the owner's signature; returns the poll result.
async fn join(
    app: &Router,
    state: &App,
    passkey: &Authenticator,
    owner: &Owner,
    statement: &str,
) -> Joined {
    let (status, started) = call(
        app,
        "POST",
        "/sync/v1/device/start",
        Some(json!({"statement":statement})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let code = json!({"device_code":started["device_code"]});
    let user_code = started["user_code"].clone();
    let signed = passkey.assert(owner, &owner::challenge(statement), 0x05);
    let (status, approved) = call(
        app,
        "POST",
        "/sync/v1/device/approve",
        Some(json!({"user_code":user_code,"assertion":signed})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    for pending in state.pending.lock().await.devices.values_mut() {
        pending.last_poll = 0;
    }
    let (status, joined) = call(app, "POST", "/sync/v1/device/poll", Some(code), None).await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    serde_json::from_value(joined).unwrap()
}

#[tokio::test]
async fn join_needs_the_owner_signature_over_the_devices_own_statement() {
    let (_directory, state, app, browser, _) = setup().await;
    let identity = trustd_remote::Identity::generate().unwrap();
    let statement = device_statement(&identity, "Laptop");
    // An uninitialized service cannot admit devices.
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/device/start",
            Some(json!({"statement":statement})),
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (passkey, owner) = initialize_owner(&state).await;
    for bad_statement in [
        json!({"statement":"{}"}),
        json!({"statement":Statement::DeviceRevoke{device_id:identity.id().unwrap(),issued_at:Utc::now().timestamp()}.to_text()}),
        json!({"statement":Statement::DeviceApprove{device_id:identity.id().unwrap(),public_key:identity.public_key.clone(),name:"Old".into(),issued_at:0}.to_text()}),
    ] {
        assert_eq!(
            call(
                &app,
                "POST",
                "/sync/v1/device/start",
                Some(bad_statement),
                None
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let (_, started) = call(
        &app,
        "POST",
        "/sync/v1/device/start",
        Some(json!({"statement":statement})),
        None,
    )
    .await;
    let code = json!({"device_code":started["device_code"]});
    let user_code = json!({"user_code":started["user_code"]});
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/device/poll",
            Some(code.clone()),
            None
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    // The approval page needs no login: it shows the exact text to sign.
    let (status, inspected) = call(
        &app,
        "POST",
        "/sync/v1/device/inspect",
        Some(user_code.clone()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inspected["statement"], statement.as_str());
    assert_eq!(inspected["challenge"], b64(&owner::challenge(&statement)));
    assert_eq!(inspected["name"], "Laptop");
    // A stranger's key, or the owner's signature over other text, is refused.
    let other = device_statement(&trustd_remote::Identity::generate().unwrap(), "Other");
    for assertion in [
        Authenticator::new(b"owner-passkey").assert(&owner, &owner::challenge(&statement), 0x05),
        passkey.assert(&owner, &owner::challenge(&other), 0x05),
    ] {
        let mut body = user_code.clone();
        body["assertion"] = json!(assertion);
        assert_eq!(
            call(&app, "POST", "/sync/v1/device/approve", Some(body), None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let mut body = user_code;
    body["assertion"] = json!(passkey.assert(&owner, &owner::challenge(&statement), 0x05));
    assert_eq!(
        call(&app, "POST", "/sync/v1/device/approve", Some(body), None)
            .await
            .0,
        StatusCode::OK
    );
    for pending in state.pending.lock().await.devices.values_mut() {
        pending.last_poll = 0;
    }
    let (status, joined) = call(
        &app,
        "POST",
        "/sync/v1/device/poll",
        Some(code.clone()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    let joined: Joined = serde_json::from_value(joined).unwrap();
    // The device verifies its own approval with the pinned owner keys.
    assert_eq!(joined.owner, owner);
    assert_eq!(joined.approval.statement, statement);
    joined.approval.verify(&owner, &owner.keys).unwrap();
    assert!(
        owner::roster(&owner, &joined.statements)
            .devices
            .contains_key(&identity.id().unwrap())
    );
    assert_eq!(
        call(&app, "POST", "/sync/v1/device/poll", Some(code), None)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let token = joined.tokens.access_token.clone();
    assert_eq!(
        call(&app, "GET", "/sync/v1/vault", None, Some(&token))
            .await
            .0,
        StatusCode::OK
    );

    // Revocation is an owner-signed statement prepared for the logged-in owner.
    let (status, prepared) = call(
        &app,
        "POST",
        "/sync/v1/statements/prepare",
        Some(json!({"kind":"device_revoke","device_id":identity.id().unwrap()})),
        Some(&browser.access_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let revoke = prepared["statement"].as_str().unwrap().to_owned();
    let forged = SignedStatement {
        assertion: Authenticator::new(b"owner-passkey").assert(
            &owner,
            &owner::challenge(&revoke),
            0x05,
        ),
        statement: revoke.clone(),
    };
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/statements",
            Some(json!(forged)),
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let signed = SignedStatement {
        assertion: passkey.assert(&owner, &owner::challenge(&revoke), 0x05),
        statement: revoke,
    };
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/statements",
            Some(json!(signed)),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    // The revoked device loses its session and can never rejoin with that key.
    assert_eq!(
        call(&app, "GET", "/sync/v1/vault", None, Some(&token))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (_, listed) = call(
        &app,
        "GET",
        "/sync/v1/roster",
        None,
        Some(&browser.access_token),
    )
    .await;
    assert!(listed["devices"].as_object().unwrap().is_empty());
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/device/start",
            Some(json!({"statement":device_statement(&identity, "Laptop")})),
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    // Device approvals are only recorded through the join flow.
    let direct = passkey.sign(&owner, &Statement::parse(&other).unwrap());
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/statements",
            Some(json!(direct)),
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn relay_admits_only_roster_devices_with_their_own_session() {
    let (_directory, state, app, _, _) = setup().await;
    let (passkey, owner) = initialize_owner(&state).await;
    let member = trustd_remote::Identity::generate().unwrap();
    let outsider = trustd_remote::Identity::generate().unwrap();
    let joined = join(
        &app,
        &state,
        &passkey,
        &owner,
        &device_statement(&member, "Member"),
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let handler: trustd_remote::Handler = Arc::new(|_, _| Box::pin(async { Ok(Value::Null) }));
    let run = |identity: &trustd_remote::Identity, token: &str| {
        let node = trustd_remote::Node::new(identity.clone(), Default::default());
        let task = tokio::spawn(node.clone().run(
            url.clone(),
            trustd_remote::static_token(token),
            handler.clone(),
        ));
        (node, task)
    };
    let (admitted, admitted_task) = run(&member, &joined.tokens.access_token);
    // The member's token does not admit another identity.
    let (outsider_node, outsider_task) = run(&outsider, &joined.tokens.access_token);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !admitted.connected() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!outsider_node.connected());
    admitted_task.abort();
    outsider_task.abort();
    server.abort();
}

#[tokio::test]
async fn encrypted_state_survives_restart_and_only_one_writer_can_open_it() {
    let (directory, state, app, _, device) = setup().await;
    let secret = "fixture-secret-never-plaintext";
    assert_eq!(
        call(
            &app,
            "PUT",
            "/sync/v1/vault",
            Some(json!({"expected_revision":0,"credentials":{"TOKEN":secret}})),
            Some(&device.access_token)
        )
        .await
        .0,
        StatusCode::OK
    );
    let encrypted = std::fs::read(directory.path().join("state.aes")).unwrap();
    assert!(
        !encrypted
            .windows(secret.len())
            .any(|bytes| bytes == secret.as_bytes())
    );
    assert!(App::open(directory.path(), "https://sync.example.com").is_err());
    drop(app);
    drop(state);
    let restarted = router(App::open(directory.path(), "https://sync.example.com").unwrap());
    let (status, snapshot) = call(
        &restarted,
        "GET",
        "/sync/v1/vault",
        None,
        Some(&device.access_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["credentials"]["TOKEN"], secret);
    assert_eq!(snapshot["revision"], 1);
}

#[tokio::test]
async fn long_poll_returns_after_write_and_rechecks_revocation() {
    let (_directory, _state, app, browser, device) = setup().await;
    let polling = {
        let app = app.clone();
        let token = device.access_token.clone();
        tokio::spawn(async move {
            call(&app, "GET", "/sync/v1/events?after=0", None, Some(&token)).await
        })
    };
    tokio::task::yield_now().await;
    let sid = device.refresh_token.split_once('.').unwrap().0;
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/session/revoke",
            Some(json!({"id":sid})),
            Some(&browser.access_token)
        )
        .await
        .0,
        StatusCode::OK
    );
    let response = tokio::time::timeout(Duration::from_secs(2), polling)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.0, StatusCode::UNAUTHORIZED);
}

#[test]
fn production_origin_requires_https_and_no_extra_url_parts() {
    for origin in [
        "http://sync.example.com",
        "https://sync.example.com/path",
        "https://user:password@sync.example.com",
        "https://sync.example.com?token=hello",
    ] {
        let directory = tempfile::tempdir().unwrap();
        assert!(App::open(directory.path(), origin).is_err());
    }
}

#[tokio::test]
async fn devices_request_owner_signatures_only_about_themselves() {
    let (_directory, state, app, browser, _) = setup().await;
    let (passkey, owner) = initialize_owner(&state).await;
    let laptop = trustd_remote::Identity::generate().unwrap();
    let server = trustd_remote::Identity::generate().unwrap();
    let joined = join(
        &app,
        &state,
        &passkey,
        &owner,
        &device_statement(&laptop, "Laptop"),
    )
    .await;
    let other = join(
        &app,
        &state,
        &passkey,
        &owner,
        &device_statement(&server, "Server"),
    )
    .await;
    let token = joined.tokens.access_token.clone();
    let login = |identity: &trustd_remote::Identity| {
        Statement::ManagementLogin {
            device_id: identity.id().unwrap(),
            nonce: "fixture-nonce-0123456789".into(),
            issued_at: Utc::now().timestamp(),
        }
        .to_text()
    };
    // Browser sessions, statements about another device and roster-changing
    // statements cannot be requested.
    for (body, bearer, expected) in [
        (
            json!({"statement":login(&laptop)}),
            browser.access_token.as_str(),
            StatusCode::FORBIDDEN,
        ),
        (
            json!({"statement":login(&server)}),
            token.as_str(),
            StatusCode::FORBIDDEN,
        ),
        (
            json!({"statement":Statement::DeviceRevoke{device_id:server.id().unwrap(),issued_at:Utc::now().timestamp()}.to_text()}),
            token.as_str(),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(
            call(
                &app,
                "POST",
                "/sync/v1/signatures",
                Some(body),
                Some(bearer)
            )
            .await
            .0,
            expected
        );
    }
    let statement = login(&laptop);
    let (status, started) = call(
        &app,
        "POST",
        "/sync/v1/signatures",
        Some(json!({"statement":statement})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let id = json!({"id":started["id"]});
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/signatures/poll",
            Some(id.clone()),
            Some(&token)
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    let code = json!({"user_code":started["code"]});
    let (_, inspected) = call(
        &app,
        "POST",
        "/sync/v1/signatures/inspect",
        Some(code.clone()),
        None,
    )
    .await;
    assert_eq!(inspected["statement"], statement.as_str());
    assert_eq!(inspected["device_name"], "Laptop");
    let mut forged = code.clone();
    forged["assertion"] = json!(Authenticator::new(b"owner-passkey").assert(
        &owner,
        &owner::challenge(&statement),
        0x05
    ));
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/signatures/approve",
            Some(forged),
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let mut approval = code;
    approval["assertion"] = json!(passkey.assert(&owner, &owner::challenge(&statement), 0x05));
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/signatures/approve",
            Some(approval),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    // Another device cannot collect it; the requesting device collects it once.
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/signatures/poll",
            Some(id.clone()),
            Some(&other.tokens.access_token)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status, result) = call(
        &app,
        "POST",
        "/sync/v1/signatures/poll",
        Some(id.clone()),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let signed = SignedStatement {
        statement: statement.clone(),
        assertion: serde_json::from_value(result["assertion"].clone()).unwrap(),
    };
    signed.verify(&owner, &owner.keys).unwrap();
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/signatures/poll",
            Some(id),
            Some(&token)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}
