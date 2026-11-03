use super::*;
use crate::agents::{Acknowledgement, Process};
use crate::service::tests::setup;

fn exchange(revision: u64) -> Exchange {
    Exchange {
        device: "Test device".into(),
        agents: vec![Agent {
            id: "path-id".into(),
            name: "codex".into(),
            path: Some("/Applications/Agent with spaces/codex".into()),
            processes: vec![Process {
                pid: 123,
                started_at: 1,
            }],
            policy: Policy::Ask,
            revision,
        }],
        acknowledgements: vec![],
    }
}

async fn open(service: &Service, user: i64) {
    service.update(0, json!({"message":{"text":"/agents","from":{"id":user,"is_bot":false},"chat":{"id":user,"type":"private"}}})).await;
}

async fn click(service: &Service, ticket: &str, user: i64, message: i64, policy: &str) {
    service.update(0, json!({"callback_query":{"id":"query","data":format!("a:{ticket}:{policy}"),"from":{"id":user,"is_bot":false},"message":{"message_id":message,"chat":{"id":user,"type":"private"}}}})).await;
}

#[tokio::test]
async fn owner_commands_are_device_scoped_idempotent_and_wait_for_acknowledgement() {
    let (service, fixture, server) = setup().await;
    service
        .exchange_agents("device-a", exchange(7))
        .await
        .unwrap();
    open(&service, 77).await;
    assert!(fixture.calls.lock().await.is_empty());
    open(&service, 42).await;
    let (ticket, message) = {
        let state = service.inner.state.lock().await;
        let (ticket, panel) = state.agents.panels.iter().next().unwrap();
        (ticket.clone(), panel.message.unwrap())
    };
    let text = fixture.calls.lock().await[0].1["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(text.contains("/Applications/Agent with spaces/codex"));
    assert!(text.contains("PID 123"));
    assert!(fixture.calls.lock().await[0].1.get("parse_mode").is_none());
    click(&service, &ticket, 77, message, "once").await;
    click(&service, &ticket, 42, message + 1000, "once").await;
    assert!(
        service
            .exchange_agents("device-a", exchange(7))
            .await
            .unwrap()
            .commands
            .is_empty()
    );
    click(&service, &ticket, 42, message, "once").await;
    click(&service, &ticket, 42, message, "trust").await;
    let first = service
        .exchange_agents("device-a", exchange(7))
        .await
        .unwrap()
        .commands;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].policy, Policy::Once);
    assert_eq!(first[0].revision, 7);
    let retry = service
        .exchange_agents("device-a", exchange(7))
        .await
        .unwrap()
        .commands;
    assert_eq!(first[0].command_id, retry[0].command_id);
    assert!(
        service
            .exchange_agents("device-b", exchange(7))
            .await
            .unwrap()
            .commands
            .is_empty()
    );
    assert!(
        !fixture
            .calls
            .lock()
            .await
            .iter()
            .any(|(m, _)| m == "editMessageText")
    );
    let mut done = exchange(8);
    done.acknowledgements.push(Acknowledgement {
        command_id: first[0].command_id.clone(),
        error: None,
    });
    assert!(
        service
            .exchange_agents("device-a", done)
            .await
            .unwrap()
            .commands
            .is_empty()
    );
    click(&service, &ticket, 42, message, "once").await;
    assert!(
        service
            .exchange_agents("device-a", exchange(9))
            .await
            .unwrap()
            .commands
            .is_empty()
    );
    let calls = fixture.calls.lock().await;
    let edit = &calls
        .iter()
        .find(|(m, _)| m == "editMessageText")
        .unwrap()
        .1;
    assert!(edit["text"].as_str().unwrap().contains("已更新：授权 1 次"));
    assert_eq!(edit["reply_markup"]["inline_keyboard"], json!([]));
    drop(calls);
    service.stop().await;
    server.abort();
}

#[tokio::test]
async fn stale_revision_offline_expired_or_unresolved_panels_cannot_change_policy() {
    for invalid in ["revision", "offline", "expiry", "path"] {
        let (service, _, server) = setup().await;
        service
            .exchange_agents("device", exchange(1))
            .await
            .unwrap();
        open(&service, 42).await;
        let (ticket, message) = {
            let mut state = service.inner.state.lock().await;
            let (ticket, panel) = state.agents.panels.iter().next().unwrap();
            let result = (ticket.clone(), panel.message.unwrap());
            match invalid {
                "revision" => {
                    state.agents.devices.get_mut("device").unwrap().agents[0].revision = 2
                }
                "offline" => {
                    state.agents.devices.get_mut("device").unwrap().updated = now() - ONLINE_MS - 1
                }
                "expiry" => state.agents.panels.get_mut(&result.0).unwrap().expires = now() - 1,
                "path" => state.agents.devices.get_mut("device").unwrap().agents[0].path = None,
                _ => unreachable!(),
            }
            result
        };
        click(&service, &ticket, 42, message, "trust").await;
        assert!(
            service
                .exchange_agents("device", exchange(1))
                .await
                .unwrap()
                .commands
                .is_empty(),
            "{invalid}"
        );
        service.stop().await;
        server.abort();
    }
}

#[tokio::test]
async fn complete_unicode_paths_are_split_without_truncation_and_callbacks_fit() {
    let (service, fixture, server) = setup().await;
    let mut data = exchange(1);
    let path = format!("/{}codex", "目录😀/".repeat(1000));
    data.agents[0].path = Some(path.clone());
    service.exchange_agents("device", data).await.unwrap();
    open(&service, 42).await;
    let calls = fixture.calls.lock().await;
    let messages: Vec<_> = calls
        .iter()
        .filter(|(m, _)| m == "sendMessage")
        .map(|(_, b)| b)
        .collect();
    let joined = messages
        .iter()
        .map(|b| b["text"].as_str().unwrap())
        .collect::<String>();
    assert!(joined.contains(&path));
    assert!(
        messages
            .iter()
            .all(|b| b["text"].as_str().unwrap().encode_utf16().count() <= 4096)
    );
    for row in messages.last().unwrap()["reply_markup"]["inline_keyboard"]
        .as_array()
        .unwrap()
    {
        for button in row.as_array().unwrap() {
            assert!(button["callback_data"].as_str().unwrap().len() <= 64);
        }
    }
    drop(calls);
    service.stop().await;
    server.abort();
}
