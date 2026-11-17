use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use trustd_remote::{Handler, Identity, Node, relay, static_token};

/// Admit exactly the listed device/token pairs; heartbeats keep them admitted.
fn admit(devices: BTreeMap<String, &'static str>) -> relay::Authorize {
    let devices = Arc::new(devices);
    Arc::new(move |device, token| {
        let devices = devices.clone();
        Box::pin(async move {
            Ok(match token {
                Some(token) => devices.get(&device).is_some_and(|t| *t == token),
                None => devices.contains_key(&device),
            })
        })
    })
}

#[tokio::test]
async fn two_outbound_nodes_authenticate_and_exchange_fragmented_requests() {
    let alice = Identity::generate().unwrap();
    let bob = Identity::generate().unwrap();
    let alice_id = alice.id().unwrap();
    let bob_id = bob.id().unwrap();
    let authorize = admit(BTreeMap::from([
        (alice_id.clone(), "alice-test-token"),
        (bob_id.clone(), "bob-test-token"),
    ]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, relay::router::<()>(authorize))
            .await
            .unwrap();
    });
    let a = Node::new(
        alice.clone(),
        BTreeMap::from([(bob_id.clone(), bob.public_key.clone())]),
    );
    let b = Node::new(
        bob.clone(),
        BTreeMap::from([(alice_id.clone(), alice.public_key.clone())]),
    );
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let handler: Handler = Arc::new(move |peer, value| {
        let calls = calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            if value["wait"] == true {
                std::future::pending::<()>().await;
            }
            Ok(json!({"peer":peer,"request":value}))
        })
    });
    let task_a = tokio::spawn(a.clone().run(
        url.clone(),
        static_token("alice-test-token"),
        handler.clone(),
    ));
    let task_b = tokio::spawn(
        b.clone()
            .run(url.clone(), static_token("bob-test-token"), handler),
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while !a.connected() || !b.connected() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let payload = json!({"text":"hello世界".repeat(30000)});
    let result = tokio::time::timeout(Duration::from_secs(10), a.call(&bob_id, payload.clone()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["request"], payload);
    assert_eq!(result["peer"], alice_id);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let reverse = b.call(&alice_id, json!({"reverse":true})).await.unwrap();
    assert_eq!(reverse["request"]["reverse"], true);
    // A paired public key is required in both directions, independently of relay authentication.
    let wrong = Identity::generate().unwrap();
    b.set_peers(BTreeMap::from([(
        alice_id.clone(),
        wrong.public_key.clone(),
    )]))
    .await;
    assert!(
        tokio::time::timeout(
            Duration::from_secs(3),
            a.call(&bob_id, json!({"must_not_execute":true}))
        )
        .await
        .unwrap()
        .is_err()
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    b.set_peers(BTreeMap::from([(
        alice_id.clone(),
        alice.public_key.clone(),
    )]))
    .await;
    let caller = a.clone();
    let pending = tokio::spawn(async move { caller.call(&bob_id, json!({"wait":true})).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while count.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task_b.abort();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(
        a.connected(),
        "target disconnect should not disconnect the caller"
    );
    task_a.abort();
    server.abort();
}

#[tokio::test]
async fn relay_rejects_unadmitted_devices_and_remote_cleartext() {
    let failing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fail = failing.clone();
    let authorize: relay::Authorize = Arc::new(move |_, _| {
        let fail = fail.load(Ordering::SeqCst);
        Box::pin(async move {
            anyhow::ensure!(!fail, "admission store unavailable");
            Ok(false)
        })
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, relay::router::<()>(authorize))
            .await
            .unwrap();
    });
    let request = reqwest::Client::new()
        .get(format!("http://{address}/v2/relay/{}", "a".repeat(64)))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==");
    let response = request.try_clone().unwrap().send().await.unwrap();
    assert_eq!(response.status(), 401);
    failing.store(true, Ordering::SeqCst);
    let response = request.send().await.unwrap();
    assert_eq!(response.status(), 503);
    let node = Node::new(Identity::generate().unwrap(), BTreeMap::new());
    let handler: Handler = Arc::new(|_, _| Box::pin(async { Ok(Value::Null) }));
    let task = tokio::spawn(node.clone().run(
        "http://192.0.2.1".into(),
        static_token("test"),
        handler,
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!node.connected());
    task.abort();
    server.abort();
}
