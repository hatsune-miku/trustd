use super::*;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use std::{collections::HashSet, sync::atomic::AtomicUsize};

#[derive(Default)]
struct Fake {
    sessions: Mutex<HashSet<String>>,
    sequence: AtomicUsize,
    requests: Mutex<Vec<Value>>,
    ended: tokio::sync::Notify,
    delayed: AtomicBool,
    release: tokio::sync::Notify,
    reused: AtomicBool,
    destroy_failed: AtomicBool,
}

async fn enter(State(state): State<Arc<Fake>>, Json(args): Json<Value>) -> Json<Value> {
    state.requests.lock().await.push(args);
    if state.delayed.load(Ordering::SeqCst) {
        state.release.notified().await;
    }
    let handle = format!(
        "private-handle-{}",
        state.sequence.fetch_add(1, Ordering::SeqCst)
    );
    state.sessions.lock().await.insert(handle.clone());
    Json(
        json!({"handle":handle,"status":"authorized","btw":"owner note","reused":state.reused.load(Ordering::SeqCst)}),
    )
}

async fn destroy(
    State(state): State<Arc<Fake>>,
    Json(args): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    if state.destroy_failed.load(Ordering::SeqCst) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let removed = state
        .sessions
        .lock()
        .await
        .remove(args["handle"].as_str().unwrap());
    if !removed {
        return Err(StatusCode::UNAUTHORIZED);
    }
    state.ended.notify_waiters();
    Ok(Json(json!({"destroyed":true})))
}

async fn run(
    State(state): State<Arc<Fake>>,
    Json(args): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let waiting = state.ended.notified();
    let handle = args["handle"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
    if !state.sessions.lock().await.contains(handle) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    state.requests.lock().await.push(args.clone());
    if args["command"] == "wait" {
        waiting.await;
    }
    Ok(Json(
        json!({"stdout":"ok","session_ended":args["command"] == "wait", "timed_out":args["command"] == "timeout"}),
    ))
}

async fn fake() -> (SocketAddr, Arc<Fake>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(Fake::default());
    let router = Router::new()
        .route("/v1/sessions/enter/interactive", post(enter))
        .route("/v1/sessions/destroy", post(destroy))
        .route("/v1/commands/run", post(run))
        .route(
            "/v1/secrets/keys",
            post(
                |State(state): State<Arc<Fake>>, Json(args): Json<Value>| async move {
                    assert_eq!(args, json!({"query":"tEsT"}));
                    state.requests.lock().await.push(args);
                    Json(json!({"keys":["TEST_TOKEN"]}))
                },
            ),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (address, state, task)
}

fn approval() -> Value {
    json!({"name":"enter_interactive","arguments":{"agent_name":"Test","request_reason":"test","confirm_text":"OK"}})
}

#[test]
fn interactive_transport_timeout_outlasts_the_full_consent_deadline() {
    assert_eq!(
        request_timeout("/v1/sessions/enter/interactive"),
        Duration::from_secs(615)
    );
    assert!(request_timeout("/v1/sessions/enter/interactive") > crate::consent::CONSENT_TIMEOUT);
    assert_eq!(request_timeout("/v1/sessions/destroy"), CONTROL_TIMEOUT);
    assert_eq!(request_timeout("/v1/secrets/keys"), CONTROL_TIMEOUT);
    let adapter = Adapter::new("127.0.0.1:1".parse().unwrap()).unwrap();
    for definitions in [adapter.definitions(), crate::mcp_text::tool_definitions()] {
        let description = definitions
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "enter_interactive")
            .unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(description.contains("600 seconds"));
    }
}

fn command(value: &str) -> Value {
    let mut args = json!({"command":value});
    if cfg!(windows) {
        args["privilege_level"] = json!("current-user");
    } else {
        args["sudo"] = json!(false);
    }
    json!({"name":"run","arguments":args})
}

async fn call(adapter: &Adapter, params: Value) -> Result<Value> {
    let (_tx, rx) = watch::channel(false);
    adapter.call(params, rx).await
}

#[tokio::test]
async fn credential_search_validates_locally_and_forwards_only_explicit_queries() {
    let (address, state, task) = fake().await;
    let adapter = Adapter::new(address).unwrap();
    for args in [
        json!({}),
        json!({"query":""}),
        json!({"query":" \n\t"}),
        json!({"query":null}),
        json!({"query":1}),
        json!({"query":false}),
        json!({"query":[]}),
    ] {
        assert!(
            call(&adapter, json!({"name":"secrets_list","arguments":args}))
                .await
                .is_err()
        );
    }
    assert!(state.requests.lock().await.is_empty());
    let result = call(
        &adapter,
        json!({"name":"secrets_list","arguments":{"query":"tEsT"}}),
    )
    .await
    .unwrap();
    assert_eq!(result, json!({"keys":["TEST_TOKEN"]}));
    assert_eq!(*state.requests.lock().await, vec![json!({"query":"tEsT"})]);
    assert!(adapter.handle.lock().await.is_none());
    task.abort();
}

#[tokio::test]
async fn credential_search_does_not_fall_back_to_an_old_daemons_full_list() {
    let router = Router::new().route(
        "/v1/secrets/keys",
        get(|| async { Json(json!({"keys":["TEST_TOKEN","UNRELATED_PRIVATE_KEY"]})) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adapter = Adapter::new(listener.local_addr().unwrap()).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let error = call(
        &adapter,
        json!({"name":"secrets_list","arguments":{"query":"test"}}),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("HTTP 405"));
    assert!(!error.to_string().contains("UNRELATED_PRIVATE_KEY"));
    task.abort();
}

#[tokio::test]
async fn interactive_http_wait_survives_the_old_control_timeout() {
    let (address, state, task) = fake().await;
    state.delayed.store(true, Ordering::SeqCst);
    let adapter = Arc::new(Adapter::new(address).unwrap());
    let waiting = adapter.clone();
    let request = tokio::spawn(async move { call(&waiting, approval()).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.requests.lock().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    // Advance only after HTTP delivery, then resume real I/O before releasing
    // the fake approval. This exercises reqwest's timeout, not just a constant.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(90)).await;
    tokio::task::yield_now().await;
    tokio::time::resume();
    state.release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result["status"], "authorized");
    adapter.close().await.unwrap();
    task.abort();
}

#[test]
fn field_projection_omits_missing_null_and_empty_required_without_losing_properties() {
    let mut minimal = json!({"type":"object"});
    remove_field(&mut minimal, "handle");
    assert_eq!(minimal, json!({"type":"object"}));

    for required in [
        None,
        Some(json!(null)),
        Some(json!([])),
        Some(json!(["handle"])),
    ] {
        let mut schema = json!({
            "type":"object",
            "properties":{"handle":{"type":"string"},"key":{"type":"string"}},
            "additionalProperties":false
        });
        if let Some(required) = required {
            schema["required"] = required;
        }
        remove_field(&mut schema, "handle");
        remove_field(&mut schema, "token");
        assert_eq!(
            schema,
            json!({
                "type":"object","properties":{"key":{"type":"string"}},"additionalProperties":false
            })
        );
    }

    let mut schema = json!({
        "type":"object",
        "properties":{"handle":{"type":"string"},"key":{"type":"string"}},
        "required":["handle","key"],"additionalProperties":false
    });
    remove_field(&mut schema, "handle");
    remove_field(&mut schema, "token");
    assert_eq!(
        schema,
        json!({
            "type":"object","properties":{"key":{"type":"string"}},
            "required":["key"],"additionalProperties":false
        })
    );
}

#[tokio::test]
async fn command_timeout_keeps_the_adapters_authorized_handle() {
    let (address, state, task) = fake().await;
    let adapter = Adapter::new(address).unwrap();
    call(&adapter, approval()).await.unwrap();
    let result = call(&adapter, command("timeout")).await.unwrap();
    assert_eq!(result["timed_out"], true);
    assert_eq!(state.sessions.lock().await.len(), 1);
    assert!(adapter.handle.lock().await.is_some());
    call(&adapter, command("next")).await.unwrap();
    assert_eq!(state.sessions.lock().await.len(), 1);
    adapter.close().await.unwrap();
    task.abort();
}

#[test]
fn all_transport_schemas_omit_empty_required_and_require_only_declared_fields() {
    let mut definitions = vec![crate::mcp_text::tool_definitions()];
    definitions.push(
        Adapter::new("127.0.0.1:1".parse().unwrap())
            .unwrap()
            .definitions(),
    );
    for definitions in definitions {
        for tool in definitions.as_array().unwrap() {
            for name in ["inputSchema", "outputSchema"] {
                let schema = &tool[name];
                assert_eq!(schema["type"], "object");
                let properties = schema["properties"].as_object().unwrap();
                if let Some(required) = schema.get("required") {
                    let required = required.as_array().expect("required must never be null");
                    assert!(!required.is_empty(), "empty required must be omitted");
                    for key in required {
                        assert!(
                            properties
                                .contains_key(key.as_str().expect("required contains strings"))
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn no_bearer_fields_or_remote_endpoint_in_model_contract() {
    {
        let adapter = Adapter::new("127.0.0.1:32120".parse().unwrap()).unwrap();
        for entry in adapter.definitions().as_array().unwrap() {
            for schema in ["inputSchema", "outputSchema"] {
                let text = entry[schema].to_string();
                assert!(!text.contains("\"handle\""));
                assert!(!text.contains("\"token\""));
                assert!(!text.contains("private-token"));
            }
        }
    }
    assert!(Adapter::new("192.0.2.1:32120".parse().unwrap()).is_err());
    assert!(Adapter::new("127.0.0.1:0".parse().unwrap()).is_err());
}

#[tokio::test]
async fn holds_handle_in_process_and_keeps_independent_instances_isolated() {
    let (address, state, server) = fake().await;
    let first = Adapter::new(address).unwrap();
    let second = Adapter::new(address).unwrap();
    assert!(call(&first, command("echo")).await.is_err());
    let result = call(&first, approval()).await.unwrap();
    assert!(!result.to_string().contains("private-handle"));
    assert_eq!(result["btw"], "owner note");
    assert!(call(&first, approval()).await.is_err());
    assert!(call(&second, command("echo")).await.is_err());
    call(&second, approval()).await.unwrap();
    call(&first, command("echo")).await.unwrap();
    call(&second, command("echo")).await.unwrap();
    let requests = state.requests.lock().await;
    assert_ne!(requests[2]["handle"], requests[3]["handle"]);
    drop(requests);
    call(&first, json!({"name":"destroy"})).await.unwrap();
    assert_eq!(state.sessions.lock().await.len(), 1);
    call(&second, command("echo")).await.unwrap();
    let mut forged = command("echo");
    forged["arguments"]["handle"] = json!("forged-handle");
    assert!(call(&second, forged).await.is_err());
    second.close().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn explicit_close_forgets_expired_handle_but_retains_unknown_cleanup() {
    let (address, state, server) = fake().await;
    let adapter = Adapter::new(address).unwrap();
    call(&adapter, approval()).await.unwrap();
    state.destroy_failed.store(true, Ordering::SeqCst);
    assert!(adapter.close().await.is_err());
    assert!(adapter.handle.lock().await.is_some());
    assert!(call(&adapter, approval()).await.is_err());

    state.destroy_failed.store(false, Ordering::SeqCst);
    state.sessions.lock().await.clear();
    assert!(call(&adapter, command("echo")).await.is_err());
    // No automatic authorization or session substitution after expiry.
    assert!(call(&adapter, approval()).await.is_err());
    adapter.close().await.unwrap();
    assert!(adapter.handle.lock().await.is_none());
    call(&adapter, approval()).await.unwrap();
    call(&adapter, command("echo")).await.unwrap();
    adapter.close().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn cancellation_closes_only_the_originating_adapters_session() {
    let (address, state, server) = fake().await;
    let first = Arc::new(Adapter::new(address).unwrap());
    let second = Adapter::new(address).unwrap();
    call(&first, approval()).await.unwrap();
    call(&second, approval()).await.unwrap();
    let (cancel, receiver) = watch::channel(false);
    let worker = first.clone();
    let running = tokio::spawn(async move { worker.call(command("wait"), receiver).await });
    while state.requests.lock().await.len() < 3 {
        tokio::task::yield_now().await;
    }
    cancel.send(true).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), running)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(first.handle.lock().await.is_none());
    assert_eq!(state.sessions.lock().await.len(), 1);
    call(&second, command("echo")).await.unwrap();
    second.close().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn cancelled_authorization_cleans_up_late_approval() {
    let (address, state, server) = fake().await;
    state.delayed.store(true, Ordering::SeqCst);
    let adapter = Arc::new(Adapter::new(address).unwrap());
    let (cancel, receiver) = watch::channel(false);
    let worker = adapter.clone();
    let pending = tokio::spawn(async move { worker.call(approval(), receiver).await });
    while state.requests.lock().await.is_empty() {
        tokio::task::yield_now().await;
    }
    cancel.send(true).unwrap();
    state.release.notify_one();
    assert!(pending.await.unwrap().is_err());
    assert!(state.sessions.lock().await.is_empty());
    assert!(adapter.handle.lock().await.is_none());
    server.abort();
}

#[tokio::test]
async fn removed_entry_is_rejected_and_authorization_source_is_not_relayed() {
    let (address, state, server) = fake().await;
    state.reused.store(true, Ordering::SeqCst);
    let adapter = Adapter::new(address).unwrap();
    let result = call(
        &adapter,
        json!({"name":"enter_preauthorized","arguments":{"confirm_text":"OK"}}),
    )
    .await;
    assert!(result.is_err());
    assert!(adapter.handle.lock().await.is_none());
    assert!(state.sessions.lock().await.is_empty());
    let result = call(&adapter, approval()).await.unwrap();
    assert!(result.get("reused").is_none());
    assert!(result.get("automatic").is_none());
    assert!(state.requests.lock().await[0].get("token").is_none());
    adapter.close().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn stdio_is_newline_json_and_eof_destroys_the_session() {
    let (address, state, server) = fake().await;
    let adapter = Adapter::new(address).unwrap();
    let (client, io) = tokio::io::duplex(65536);
    let (input, output) = tokio::io::split(io);
    let worker = tokio::spawn(adapter.serve(input, output));
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read);
    write_message(
        &mut write,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await
    .unwrap();
    let mut line = String::new();
    read.read_line(&mut line).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["result"]["serverInfo"]["name"],
        "trustd-stdio"
    );
    write_message(
        &mut write,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":approval()}),
    )
    .await
    .unwrap();
    line.clear();
    read.read_line(&mut line).await.unwrap();
    assert!(!line.contains("private-handle"));
    assert_eq!(state.sessions.lock().await.len(), 1);
    write.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(state.sessions.lock().await.is_empty());
    server.abort();
}

#[tokio::test]
async fn eof_waits_for_late_approval_through_the_full_consent_window() {
    struct SignalEof<R> {
        input: R,
        eof: Arc<tokio::sync::Notify>,
    }
    impl<R: AsyncRead + Unpin> AsyncRead for SignalEof<R> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            context: &mut std::task::Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let before = buffer.filled().len();
            let result = std::pin::Pin::new(&mut self.input).poll_read(context, buffer);
            if matches!(result, std::task::Poll::Ready(Ok(()))) && buffer.filled().len() == before {
                self.eof.notify_one();
            }
            result
        }
    }
    let (address, state, server) = fake().await;
    state.delayed.store(true, Ordering::SeqCst);
    let (client, io) = tokio::io::duplex(65536);
    let (input, output) = tokio::io::split(io);
    let eof = Arc::new(tokio::sync::Notify::new());
    let worker = tokio::spawn(Adapter::new(address).unwrap().serve(
        SignalEof {
            input,
            eof: eof.clone(),
        },
        output,
    ));
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read);
    write_message(
        &mut write,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await
    .unwrap();
    read.read_line(&mut String::new()).await.unwrap();
    write_message(
        &mut write,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":approval()}),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.requests.lock().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    write.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), eof.notified())
        .await
        .unwrap();
    // EOF is observed, so the adapter is now waiting on its cleanup deadline.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(590)).await;
    tokio::task::yield_now().await;
    tokio::time::resume();
    assert!(
        !worker.is_finished(),
        "cleanup ended before the consent window"
    );
    state.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(state.sequence.load(Ordering::SeqCst), 1);
    assert!(state.sessions.lock().await.is_empty());
    server.abort();
}

#[tokio::test]
async fn framing_rejects_oversized_and_unterminated_messages() {
    for value in [vec![b'x'; MAX_MESSAGE + 1], b"{}".to_vec()] {
        assert!(
            read_message(&mut BufReader::new(value.as_slice()), &mut Vec::new())
                .await
                .is_err()
        );
    }
    let mut partial = b"{\"jsonrpc\":".to_vec();
    let message = read_message(&mut BufReader::new(&b"\"2.0\"}\n"[..]), &mut partial)
        .await
        .unwrap()
        .unwrap();
    assert!(serde_json::from_slice::<Value>(&message).is_ok());
}
