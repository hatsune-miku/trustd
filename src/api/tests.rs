use super::*;
use crate::{
    auth::AuthManager,
    config::Config,
    consent::{Choice, ConsentBroker, Decision, DesktopEvent},
    server::router,
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tower::ServiceExt;

fn user_state() -> (AppState, String) {
    let mut auth = AuthManager::new("unused".into(), None);
    let token = auth.issue_token(None).unwrap().0;
    let state = AppState::user(
        Config {
            bind: "127.0.0.1:32120".parse().unwrap(),
            ..Config::default()
        },
        auth,
    )
    .unwrap();
    (state, token)
}

fn consent_state(choice: Choice, remember: bool) -> (AppState, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let captured = count.clone();
    let broker = ConsentBroker::new(move |event| {
        if let DesktopEvent::Show(prompt, reply) = event {
            assert_eq!(prompt.request_reason, "fixture <reason>\nsecond line");
            captured.fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Decision {
                choice,
                remember,
                btw: "fixture note".into(),
            });
        }
        Ok(())
    });
    (user_state().0.with_consent(broker), count)
}

fn consent_arguments() -> Value {
    json!({"agent_name":"fixture","request_reason":"fixture <reason>\nsecond line","confirm_text":"OK"})
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
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

async fn http(app: &Router, operation: Operation, arguments: Value) -> (StatusCode, Value) {
    request(app, "POST", operation.path(), Some(arguments)).await
}

async fn mcp(app: &Router, name: &str, arguments: Value) -> Value {
    request(app, "POST", "/mcp", Some(json!({
        "jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}
    }))).await.1
}

fn run_arguments(handle: &str, command: &str) -> Value {
    let mut args = crate::execution::execution_argument(PrivilegeLevel::CurrentUser);
    args["handle"] = json!(handle);
    args["command"] = json!(command);
    args
}

#[tokio::test]
async fn retired_authorization_routes_are_absent_and_client_identity_is_not_input() {
    let (state, _) = user_state();
    let app = router(state);
    for path in [
        "/v1/admin/tokens/issue",
        "/v1/admin/tokens/list",
        "/v1/admin/tokens/revoke",
        "/v1/tokens/revoke",
        "/v1/sessions/enter",
        "/v1/sessions/enter/preauthorized",
        "/v1/admin/remembered/reset",
    ] {
        assert_eq!(
            request(&app, "POST", path, Some(json!({}))).await.0,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    for (key, value) in [
        ("pid", json!(std::process::id())),
        ("agent_path", json!("/trusted/codex")),
        ("policy", json!("trust")),
        ("agent", json!({"path":"/trusted/codex"})),
    ] {
        let mut args = consent_arguments();
        args[key] = value;
        assert_eq!(
            http(&app, Operation::EnterInteractive, args).await.0,
            StatusCode::BAD_REQUEST,
            "{key}"
        );
    }
}

#[tokio::test]
async fn llms_is_public_plain_text_with_live_schemas_on_both_daemons() {
    for state in [
        user_state().0,
        AppState::new(
            Config::default(),
            AuthManager::new("private-verifier-fixture".into(), None),
        ),
    ] {
        state
            .put_secret("PRIVATE_DOC_FIXTURE", "private-doc-secret-value")
            .await
            .unwrap();
        let token = state.auth.lock().await.issue_token(None).unwrap().0;
        let app = router(state.clone());
        // No Authorization, Cookie, MCP session header, token, or handle.
        let response = app
            .clone()
            .oneshot(
                Request::get("/llms.txt")
                    .header("host", state.config.bind.to_string())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            "text/plain; charset=utf-8"
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.starts_with("# trustd API\n"));
        assert!(!text.contains('\r'));
        for removed in [
            "preauthor",
            "pre-author",
            "remember",
            "\"automatic\"",
            "reused",
            "token-env",
        ] {
            assert!(
                !text.contains(removed),
                "AI documentation disclosed {removed}"
            );
        }
        assert!(text.contains(&format!("Base URL: http://{}\n", state.config.bind)));
        let mode = serde_json::to_value(state.mode).unwrap();
        assert!(text.contains(&format!("Daemon: {}\n", mode.as_str().unwrap())));
        assert!(text.contains(&format!("Platform: {}\n", std::env::consts::OS)));
        assert!(text.contains(&format!("Version: {}\n", crate::VERSION)));
        assert!(text.contains(&format!("Commit: {}\n", crate::COMMIT)));
        for secret in [
            "PRIVATE_DOC_FIXTURE",
            "private-doc-secret-value",
            "private-verifier-fixture",
            token.as_str(),
        ] {
            assert!(!text.contains(secret));
        }
        let operations_text = text.split("## Operations\n").nth(1).unwrap();
        let published_schemas: Vec<Value> = operations_text
            .split("```json\n")
            .skip(1)
            .map(|block| serde_json::from_str(block.split("\n```").next().unwrap()).unwrap())
            .collect();
        let (_, capabilities) = request(&app, "GET", "/v1/capabilities", None).await;
        let operations = capabilities["operations"].as_array().unwrap();
        assert_eq!(published_schemas.len(), operations.len() * 2);
        for (operation, schemas) in operations.iter().zip(published_schemas.as_chunks::<2>().0) {
            assert!(operations_text.contains(&format!(
                "### {} {}\n\nMCP tool: `{}`\n",
                operation["method"].as_str().unwrap(),
                operation["path"].as_str().unwrap(),
                operation["name"].as_str().unwrap(),
            )));
            assert_eq!(schemas[0], operation["inputSchema"]);
            assert_eq!(schemas[1], operation["outputSchema"]);
        }
        // Publishing docs must not open protected execution/management routes.
        assert_eq!(
            http(&app, Operation::Run, run_arguments("invalid", "echo never"))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(&app, "GET", "/v1/admin/settings", None).await.0,
            StatusCode::UNAUTHORIZED
        );
        let head = app
            .clone()
            .oneshot(Request::head("/llms.txt").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()["content-type"], "text/plain; charset=utf-8");
        assert!(
            head.into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );
        assert_eq!(
            request(&app, "POST", "/llms.txt", None).await.0,
            StatusCode::METHOD_NOT_ALLOWED
        );
        state.shutdown().await;
    }
}

#[tokio::test]
async fn capabilities_publish_all_mcp_schemas_and_registered_http_operations() {
    let (state, _) = user_state();
    let app = router(state.clone());
    let (status, capabilities) = request(&app, "GET", "/v1/capabilities", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(capabilities["version"], crate::VERSION);
    assert_eq!(capabilities["commit"], crate::COMMIT);
    assert_eq!(capabilities["daemon"], "user");
    assert_eq!(capabilities["interactive_timeout_ms"], 600_000);
    assert_eq!(capabilities["cancellation"]["disconnect_cancels"], false);
    let tools = tool_definitions();
    let operations = capabilities["operations"].as_array().unwrap();
    assert_eq!(operations.len(), tools.as_array().unwrap().len());
    for (entry, tool) in operations.iter().zip(tools.as_array().unwrap()) {
        let mut stripped = entry.clone();
        stripped.as_object_mut().unwrap().remove("method");
        stripped.as_object_mut().unwrap().remove("path");
        assert_eq!(&stripped, tool);
        let (status, _) = request(
            &app,
            entry["method"].as_str().unwrap(),
            entry["path"].as_str().unwrap(),
            Some(json!({})),
        )
        .await;
        assert_ne!(status, StatusCode::NOT_FOUND);
        assert_ne!(status, StatusCode::METHOD_NOT_ALLOWED);
    }
    state.shutdown().await;
}

#[tokio::test]
async fn credential_search_requires_a_nonblank_literal_substring_on_every_transport() {
    for state in [
        user_state().0,
        AppState::new(Config::default(), AuthManager::new("unused".into(), None)),
    ] {
        for key in ["GITHUB_TOKEN", "GITHUB_PRIVATE_KEY", "JENKINS_TOKEN"] {
            state.put_secret(key, "fixture-secret-value").await.unwrap();
        }
        let app = router(state.clone());
        for (query, expected) in [
            ("gItHuB", vec!["GITHUB_PRIVATE_KEY", "GITHUB_TOKEN"]),
            ("HUB_T", vec!["GITHUB_TOKEN"]),
            ("token", vec!["GITHUB_TOKEN", "JENKINS_TOKEN"]),
            ("GITHUB_TOKEN", vec!["GITHUB_TOKEN"]),
            ("absent", vec![]),
            ("*", vec![]),
            (".*", vec![]),
            (" github ", vec![]),
            ("凭据", vec![]),
        ] {
            let args = json!({"query":query});
            let (status, found) = http(&app, Operation::SecretsList, args.clone()).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(found, json!({"keys":expected}));
            assert_eq!(
                mcp(&app, "secrets_list", args).await["result"]["structuredContent"],
                found
            );
            assert!(!found.to_string().contains("fixture-secret-value"));
        }
        for args in [
            json!({}),
            json!({"query":""}),
            json!({"query":" \t\r\n"}),
            json!({"query":"\u{3000}"}),
            json!({"query":null}),
            json!({"query":42}),
            json!({"query":false}),
            json!({"query":[]}),
            json!({"query":{}}),
            json!({"query":"github","fixture-secret-value":true}),
            json!([]),
        ] {
            let (status, error) = http(&app, Operation::SecretsList, args.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let rpc = mcp(&app, "secrets_list", args).await;
            assert_eq!(rpc["error"]["code"], -32602);
            for response in [error, rpc] {
                assert!(!response.to_string().contains("fixture-secret-value"));
                assert!(!response.to_string().contains("GITHUB_TOKEN"));
                assert!(response.get("keys").is_none());
            }
        }
        // Old clients cannot enumerate by omitting a body or passing a query in the URL.
        for path in ["/v1/secrets/keys", "/v1/secrets/keys?query=github"] {
            assert_eq!(
                request(&app, "GET", path, None).await.0,
                StatusCode::METHOD_NOT_ALLOWED
            );
        }
        state.shutdown().await;
    }
}

#[tokio::test]
async fn interactive_secrets_execution_and_lifecycle_are_interoperable() {
    let (state, _) = consent_state(Choice::Accept, false);
    state
        .put_secret("TEST_API_SECRET", "fixture-secret-value")
        .await
        .unwrap();
    let app = router(state.clone());
    let (status, keys) = http(&app, Operation::SecretsList, json!({"query":"api"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        keys,
        mcp(&app, "secrets_list", json!({"query":"api"})).await["result"]["structuredContent"]
    );
    assert_eq!(keys, json!({"keys":["TEST_API_SECRET"]}));

    let mut arguments = consent_arguments();
    arguments["secret_keys"] = json!(["TEST_API_SECRET"]);
    let (status, entered) = http(&app, Operation::EnterInteractive, arguments.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(entered.get("reused").is_none());
    assert!(!entered.to_string().contains("fixture-secret-value"));
    let next =
        mcp(&app, "enter_interactive", arguments).await["result"]["structuredContent"].clone();
    assert_eq!(next["status"], "authorized");
    assert_ne!(next["handle"], entered["handle"]);
    let handle = entered["handle"].as_str().unwrap();
    let (legacy_status, _) = request(
        &app,
        "POST",
        "/v1/sessions/enter",
        Some(json!({"token":"removed"})),
    )
    .await;
    assert_eq!(legacy_status, StatusCode::NOT_FOUND);

    let command = if cfg!(windows) {
        "[Console]::Out.Write([Console]::In.ReadToEnd()); [Console]::Error.Write('fixture stderr')"
    } else {
        "cat; printf 'fixture stderr' >&2"
    };
    let mut args = run_arguments(handle, command);
    args["stdin"] = json!("fixture stdin\n");
    // Compare transport semantics independently of cold PowerShell startup on CI.
    // Explicit deadlines and interruption have dedicated tests.
    let (status, mut output) = http(&app, Operation::Run, args.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(output["stdout"], "fixture stdin\n");
    assert_eq!(output["stderr"], "fixture stderr");
    let mut rpc_output = mcp(&app, "run", args).await["result"]["structuredContent"].clone();
    output.as_object_mut().unwrap().remove("duration_ms");
    rpc_output.as_object_mut().unwrap().remove("duration_ms");
    assert_eq!(output, rpc_output);

    let elevated = crate::execution::execution_argument(PrivilegeLevel::privileged());
    let mut forbidden = run_arguments(handle, "echo must-not-run");
    forbidden
        .as_object_mut()
        .unwrap()
        .extend(elevated.as_object().unwrap().clone());
    assert_eq!(
        http(&app, Operation::Run, forbidden.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    assert!(mcp(&app, "run", forbidden).await.get("error").is_some());

    assert_eq!(
        mcp(&app, "destroy", json!({"handle":handle})).await["result"]["structuredContent"],
        json!({"destroyed":true})
    );
    assert_eq!(
        http(&app, Operation::Run, run_arguments(handle, "echo never"))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (_, next) = http(&app, Operation::EnterInteractive, consent_arguments()).await;
    assert_ne!(next["handle"], handle);
    assert_eq!(
        http(&app, Operation::Destroy, json!({"handle":next["handle"]}))
            .await
            .1,
        json!({"destroyed":true})
    );
    assert!(
        mcp(
            &app,
            "run",
            run_arguments(next["handle"].as_str().unwrap(), "echo never")
        )
        .await
        .get("error")
        .is_some()
    );
    state.shutdown().await;
}

#[tokio::test]
async fn plaintext_reads_share_scoped_authorization_and_snapshots_across_transports() {
    for state in [
        user_state().0,
        AppState::new(Config::default(), AuthManager::new("unused".into(), None)),
    ] {
        let token = state.auth.lock().await.issue_token(None).unwrap().0;
        let value = "fixture secret \" ' \\ \r\n中文 $env:NOT_CODE";
        state.put_secret("TEST_READ_SECRET", value).await.unwrap();
        state
            .put_secret("TEST_OTHER_SECRET", "not-authorized")
            .await
            .unwrap();
        let app = router(state.clone());
        let entered = serde_json::to_value(
            state
                .enter_with_secrets(&token, &["TEST_READ_SECRET".into()])
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(!entered.to_string().contains(value));
        let handle = entered["handle"].as_str().unwrap();
        let arguments = json!({"handle":handle,"key":"TEST_READ_SECRET"});
        let (status, revealed) = http(&app, Operation::SecretsGet, arguments.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(revealed, json!({"key":"TEST_READ_SECRET","value":value}));
        assert_eq!(
            revealed,
            mcp(&app, "secrets_get", arguments.clone()).await["result"]["structuredContent"]
        );
        for key in ["TEST_OTHER_SECRET", "MISSING_SECRET"] {
            let denied = json!({"handle":handle,"key":key});
            let (status, error) = http(&app, Operation::SecretsGet, denied.clone()).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(
                error,
                json!({"error":"secret key is not authorized for this session"})
            );
            assert_eq!(
                error["error"],
                mcp(&app, "secrets_get", denied).await["error"]["message"]
            );
        }
        // No implicit vault access, even with a valid execution session.
        let other_token = state.auth.lock().await.issue_token(None).unwrap().0;
        let other = state.enter(&other_token).await.unwrap();
        assert_eq!(
            http(
                &app,
                Operation::SecretsGet,
                json!({"handle":other.handle,"key":"TEST_READ_SECRET"})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            http(
                &app,
                Operation::SecretsGet,
                json!({"handle":"invalid","key":"TEST_READ_SECRET"})
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(&app, "GET", Operation::SecretsGet.path(), None)
                .await
                .0,
            StatusCode::METHOD_NOT_ALLOWED
        );
        let response = app
            .clone()
            .oneshot(
                Request::post(Operation::SecretsGet.path())
                    .header("content-type", "application/json")
                    .body(Body::from(arguments.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["content-type"], "application/json");
        // Reads reveal the approved snapshot, not a newly rotated/deleted vault value.
        state
            .put_secret("TEST_READ_SECRET", "rotated-value")
            .await
            .unwrap();
        state.remove_secret("TEST_READ_SECRET").await.unwrap();
        assert_eq!(
            http(&app, Operation::SecretsGet, arguments.clone()).await.1,
            revealed
        );
        state.destroy_session(handle).await.unwrap();
        assert_eq!(
            http(&app, Operation::SecretsGet, arguments).await.0,
            StatusCode::UNAUTHORIZED
        );
        state
            .put_secret("TEST_READ_SECRET", "fresh-value")
            .await
            .unwrap();
        let fresh = state
            .enter_with_secrets(&token, &["TEST_READ_SECRET".into()])
            .await
            .unwrap();
        let arguments = json!({"handle":fresh.handle,"key":"TEST_READ_SECRET"});
        assert_eq!(
            http(&app, Operation::SecretsGet, arguments.clone()).await.1["value"],
            "fresh-value"
        );
        state.revoke_token(&token).await.unwrap();
        assert_eq!(
            http(&app, Operation::SecretsGet, arguments.clone()).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert!(
            mcp(&app, "secrets_get", arguments)
                .await
                .get("error")
                .is_some()
        );
        state.shutdown().await;
    }
}

#[tokio::test]
async fn credential_only_authorization_runs_without_the_paired_privileged_daemon() {
    let (state, calls) = consent_state(Choice::Accept, false);
    state
        .put_secret("TEST_API_SECRET", "fixture-private")
        .await
        .unwrap();
    let unavailable = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    state.settings.lock().await.saved.pairing = Some(crate::settings::Pairing {
        key: crate::bridge::generate_key(),
        peer: unavailable.local_addr().unwrap(),
    });
    drop(unavailable);
    let directory = tempfile::tempdir().unwrap();
    let app = router(state.clone());
    let mut args = consent_arguments();
    args["secret_keys"] = json!(["TEST_API_SECRET"]);
    args["cwd"] = json!(directory.path());
    let (status, entered) = http(&app, Operation::EnterInteractive, args).await;
    assert_eq!(status, StatusCode::OK, "{entered}");
    assert_eq!(entered["status"], "authorized");
    assert_eq!(entered["sudo_available"], false);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let handle = entered["handle"].as_str().unwrap();
    let command = if cfg!(windows) {
        "[Console]::Write($env:TEST_API_SECRET)"
    } else {
        "printf '%s' \"$TEST_API_SECRET\""
    };
    let (status, result) = http(&app, Operation::Run, run_arguments(handle, command)).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["stdout"], "fixture-private");
    assert_eq!(
        std::fs::canonicalize(result["cwd"].as_str().unwrap()).unwrap(),
        directory.path().canonicalize().unwrap()
    );
    assert_eq!(
        state
            .run(handle, "echo forbidden", true, None)
            .await
            .unwrap_err()
            .status_code(),
        StatusCode::FORBIDDEN.as_u16()
    );
    state.shutdown().await;
}

#[tokio::test]
async fn http_interactive_approval_uses_the_same_owner_decision_and_secret_selection() {
    let (state, calls) = consent_state(Choice::Accept, true);
    state
        .put_secret("TEST_API_SECRET", "fixture-private")
        .await
        .unwrap();
    let app = router(state.clone());
    let mut args = consent_arguments();
    args["secret_keys"] = json!(["TEST_API_SECRET"]);
    let (status, result) = http(&app, Operation::EnterInteractive, args.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["status"], "authorized");
    assert_eq!(result["btw"], "fixture note");
    assert_eq!(result["secret_keys"], json!(["TEST_API_SECRET"]));
    assert!(result.get("token").is_none());
    assert!(!result.to_string().contains("fixture-private"));
    assert_eq!(
        mcp(
            &app,
            "secrets_get",
            json!({"handle":result["handle"],"key":"TEST_API_SECRET"})
        )
        .await["result"]["structuredContent"],
        json!({"key":"TEST_API_SECRET","value":"fixture-private"})
    );
    let rpc = mcp(
        &app,
        "run",
        run_arguments(result["handle"].as_str().unwrap(), "echo http-authorized"),
    )
    .await;
    assert!(
        rpc["result"]["structuredContent"]["stdout"]
            .as_str()
            .unwrap()
            .contains("http-authorized")
    );
    let remembered = mcp(&app, "enter_interactive", args).await;
    assert_eq!(
        remembered["result"]["structuredContent"]["status"],
        "authorized"
    );
    assert_eq!(
        remembered["result"]["structuredContent"]["btw"],
        "fixture note"
    );
    assert!(
        remembered["result"]["structuredContent"]
            .get("automatic")
            .is_none()
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    state.shutdown().await;
}

#[tokio::test]
async fn http_denial_is_neutral_and_self_reported_names_do_not_create_policy() {
    let (state, calls) = consent_state(Choice::Deny, true);
    let directory = tempfile::tempdir().unwrap();
    let state = state
        .with_storage(directory.path().join("config.toml"))
        .unwrap();
    let app = router(state.clone());
    let (status, result) = http(&app, Operation::EnterInteractive, consent_arguments()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result, json!({"status":"denied","btw":"fixture note"}));
    let (_, remembered) = http(&app, Operation::EnterInteractive, consent_arguments()).await;
    let rpc = mcp(&app, "enter_interactive", consent_arguments()).await;
    assert_eq!(remembered, rpc["result"]["structuredContent"]);
    assert!(remembered.get("automatic").is_none());
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    let page = state
        .logs
        .page(crate::logs::Kind::Audit, None, 100)
        .await
        .unwrap();
    let authorizations: Vec<_> = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["event"] == "authorization")
        .collect();
    assert_eq!(authorizations.len(), 3);
    assert!(
        authorizations
            .iter()
            .all(|entry| entry["success"] == false && entry["status"] == "denied")
    );
    state.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn http_interactive_ignore_preserves_the_fixed_deadline_and_note() {
    let (state, _) = consent_state(Choice::Ignore, true);
    let app = router(state.clone());
    let started = tokio::time::Instant::now();
    let (status, result) = http(&app, Operation::EnterInteractive, consent_arguments()).await;
    assert_eq!(started.elapsed(), Duration::from_secs(600));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        result,
        json!({"status":"timed_out","timeout_ms":600000,"btw":"fixture note"})
    );
    assert!(state.settings.lock().await.saved.agent_policies.is_empty());
    state.shutdown().await;
}

#[tokio::test]
async fn both_transports_reject_invalid_arguments_before_authorization_or_execution() {
    let (state, calls) = consent_state(Choice::Accept, false);
    let app = router(state.clone());
    let cases = [
        ("secrets_get", json!({})),
        ("secrets_get", json!({"handle":"fixture-private"})),
        ("secrets_get", json!({"handle":123,"key":"TEST_SECRET"})),
        ("secrets_get", json!({"handle":"fixture-private","key":123})),
        (
            "secrets_get",
            json!({"handle":"fixture-private","key":"lowercase"}),
        ),
        (
            "secrets_get",
            json!({"handle":"fixture-private","key":"TEST_SECRET","token":"fixture-private"}),
        ),
        ("enter_interactive", json!(null)),
        ("enter_interactive", json!([])),
        ("enter_interactive", json!({})),
        (
            "enter_interactive",
            json!({"agent_name":"fixture","request_reason":"reason","confirm_text":"fixture-private"}),
        ),
        (
            "enter_interactive",
            json!({"agent_name":"fixture","request_reason":"reason","confirm_text":"OK","choice":"accept"}),
        ),
        (
            "enter_interactive",
            json!({"agent_name":"","request_reason":"reason","confirm_text":"OK"}),
        ),
        (
            "enter_interactive",
            json!({"agent_name":"fixture","request_reason":"reason","confirm_text":"OK","secret_keys":null}),
        ),
        (
            "enter_interactive",
            json!({"agent_name":"fixture","request_reason":"reason","confirm_text":"OK","secret_keys":["lowercase"]}),
        ),
        (
            "enter_interactive",
            json!({"agent_name":"fixture","request_reason":"reason","confirm_text":"OK","agent_path":"forged"}),
        ),
        (
            "run",
            json!({"handle":"fixture-private","command":"echo never"}),
        ),
        (
            "destroy",
            json!({"handle":"fixture-private","fixture-private":"unexpected"}),
        ),
    ];
    for (name, arguments) in cases {
        let (status, error) =
            http(&app, Operation::from_name(name).unwrap(), arguments.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(
            error["error"],
            mcp(&app, name, arguments).await["error"]["message"]
        );
        assert!(!error.to_string().contains("fixture-private"));
    }
    for (field, value) in [
        ("stdin", Value::Null),
        ("timeout_seconds", json!(0)),
        ("timeout_seconds", json!(-1)),
        ("timeout_seconds", json!("10")),
    ] {
        let mut arguments = run_arguments("invalid", "echo never");
        arguments[field] = value;
        let (status, error) = http(&app, Operation::Run, arguments.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            error["error"],
            mcp(&app, "run", arguments).await["error"]["message"]
        );
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    state.shutdown().await;
}

#[tokio::test]
async fn new_http_routes_keep_the_loopback_browser_boundary() {
    let (state, calls) = consent_state(Choice::Accept, false);
    let app = router(state.clone());
    for path in [
        "/llms.txt",
        "/v1/capabilities",
        Operation::SecretsGet.path(),
        Operation::EnterInteractive.path(),
        "/v1/admin/agents",
    ] {
        for (header, value) in [
            ("origin", "https://evil.example"),
            ("origin", "http://localhost:32119"),
            ("host", "evil.example:32120"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(if matches!(path, "/v1/capabilities" | "/llms.txt") {
                            "GET"
                        } else {
                            "POST"
                        })
                        .uri(path)
                        .header("content-type", "application/json")
                        .header(header, value)
                        .body(Body::from(consent_arguments().to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }
    let response = app
        .oneshot(
            Request::get("/v1/capabilities")
                .header("host", "localhost:32120")
                .header("origin", "http://localhost:32120")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    state.shutdown().await;
}

#[tokio::test]
async fn http_destroy_cancels_an_inflight_run_and_invalidates_its_handle() {
    let (state, _) = consent_state(Choice::Accept, false);
    let app = router(state.clone());
    let (_, entered) = http(&app, Operation::EnterInteractive, consent_arguments()).await;
    let handle = entered["handle"].as_str().unwrap();
    // Start the persistent interpreter before timing cancellation of a command.
    let (_, ready) = http(&app, Operation::Run, run_arguments(handle, "echo ready")).await;
    assert_eq!(ready["success"], true, "{ready}");
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("started");
    let path = marker.to_string_lossy();
    let command = if cfg!(windows) {
        format!(
            "[System.IO.File]::WriteAllText('{}','started'); Start-Sleep -Seconds 30",
            path.replace('\'', "''")
        )
    } else {
        format!(
            "printf started > '{}'; sleep 30",
            path.replace('\'', "'\\''")
        )
    };
    let running = {
        let app = app.clone();
        let args = run_arguments(handle, &command);
        tokio::spawn(async move { http(&app, Operation::Run, args).await })
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        http(&app, Operation::Destroy, json!({"handle":"not-the-handle"}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(!running.is_finished());
    assert_eq!(
        http(&app, Operation::Destroy, json!({"handle":handle}))
            .await
            .1,
        json!({"destroyed":true})
    );
    let (status, result) = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["session_ended_reason"], "cancelled");
    assert_eq!(result["session_alive"], false);
    assert_eq!(
        http(&app, Operation::Run, run_arguments(handle, "echo never"))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    state.shutdown().await;
}
#[test]
fn session_options_default_to_credentials_without_elevation_and_validate_cwd() {
    let options =
        super::enter_options_arg(&serde_json::json!({"secret_keys":["GITHUB_TOKEN"]})).unwrap();
    assert!(!options.sudo);
    assert!(options.cwd.is_none());
    for value in [
        serde_json::json!({"sudo":"true"}),
        serde_json::json!({"cwd":42}),
        serde_json::json!({"cwd":"relative/path"}),
    ] {
        assert!(super::enter_options_arg(&value).is_err());
    }
    assert!(
        super::enter_options_arg(&serde_json::json!({"sudo":true,"cwd":""}))
            .unwrap()
            .sudo
    );
}
