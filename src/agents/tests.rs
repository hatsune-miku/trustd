use super::*;
use crate::{
    auth::AuthManager,
    config::Config,
    consent::{ConsentBroker, DesktopEvent},
    settings::Settings,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub(crate) fn current_identity() -> Identity {
    identity(
        snapshot()
            .process(Pid::from_u32(std::process::id()))
            .unwrap(),
    )
    .unwrap()
}

pub(crate) async fn save_policy(state: &AppState, agent: &Identity, policy: Policy) {
    let mut settings = state.settings.lock().await;
    let mut next = settings.saved.clone();
    next.agent_policies.insert(
        path_id(&agent.path),
        SavedAgent {
            name: agent.name.clone(),
            path: agent.path.clone(),
            policy,
            revision: 1,
            last_command: String::new(),
        },
    );
    settings.save(next).await.unwrap();
}

fn prompt() -> Prompt {
    Prompt {
        id: uuid::Uuid::new_v4().to_string(),
        agent_name: "self-reported name".into(),
        agent_path: None,
        request_reason: "test".into(),
        scope: "test".into(),
        secret_keys: vec![],
        expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
    }
}

fn state(choice: Choice, remember: bool) -> (AppState, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let state = AppState::user(
        Config {
            bind: "127.0.0.1:32120".parse().unwrap(),
            ..Config::default()
        },
        AuthManager::new("unused".into(), None),
    )
    .unwrap()
    .with_consent(ConsentBroker::new(move |event| {
        if let DesktopEvent::Show(_, reply) = event {
            calls.fetch_add(1, Ordering::SeqCst);
            let _ = reply.send(Decision {
                choice,
                remember,
                btw: String::new(),
            });
        }
        Ok(())
    }));
    (state, count)
}

#[test]
fn exact_names_and_full_paths_never_collapse_same_name_programs() {
    assert!(recognized("Codex.exe"));
    assert!(recognized("claude"));
    assert!(!recognized("not-codex.exe"));
    assert!(!recognized("trustd-stdio.exe"));
    assert!(!recognized("node.exe"));
    assert_ne!(path_id("/one/codex"), path_id("/two/codex"));
    assert_ne!(
        path_id("C:\\Agents\\codex.exe"),
        path_id("C:\\agents\\codex.exe")
    );
    let legacy: crate::settings::SavedSettings = serde_json::from_value(serde_json::json!({
        "remembered":{"codex":true}, "remembered_secrets":{"codex":["OLD_KEY"]}
    }))
    .unwrap();
    assert!(legacy.agent_policies.is_empty());
    assert!(
        serde_json::to_value(legacy)
            .unwrap()
            .get("remembered")
            .is_none()
    );
}

#[test]
fn discovery_catalog_is_exact_normalized_and_excludes_generic_runtimes() {
    let mut seen = BTreeSet::new();
    for name in PROCESS_NAMES {
        assert_eq!(*name, name.to_lowercase());
        assert!(!name.ends_with(".exe"));
        assert!(seen.insert(*name), "duplicate executable: {name}");
        assert!(recognized(name));
        assert!(recognized(&format!("{}.EXE", name.to_uppercase())));
        assert!(!recognized(&format!("not-{name}")));
        assert!(!recognized(&format!("{name}-unrelated")));
    }
    for name in [
        "DeepSeek Harness",
        "DeepSeek Harness Helper (Plugin)",
        "OpenCode",
        "opencode-cli",
        "ai.opencode",
        "Goose",
        "Kiro",
        "kiro-cli",
        "kimi",
        "copilot",
        "droid",
        "amp",
        "kilo",
        "crush",
        "grok",
        "Antigravity",
        "agy",
        "Trae CN",
        "CodeBuddy",
        "Qoder",
        "ZCode",
        "Cherry Studio",
        "Jan-Desktop",
        "Zed",
        "cline",
        "qwen",
        "auggie",
        "aider",
        "pi",
        "hermes",
        "openhands",
        "dsh",
    ] {
        assert!(recognized(name), "missing discovery entry: {name}");
    }
    for name in [
        "node",
        "node.exe",
        "python",
        "python3",
        "python3.14",
        "pythonw.exe",
        "bun",
        "deno",
        "electron",
        "java",
        "bash",
        "sh",
        "pwsh",
        "cmd.exe",
        "agent",
        "helper",
        "unknown helper (plugin)",
        "DeepSeek Harness Updater",
        "DeepSeek Harness Helper (GPU)",
        "dsh.js",
        "node /opt/dsh/lib/bin.js",
        "/Applications/DeepSeek Harness",
        "copilot.exe.bak",
    ] {
        assert!(!recognized(name), "unsafe discovery entry: {name}");
    }
}

#[tokio::test]
async fn once_is_atomic_persisted_and_not_replayed_after_consumption() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let (state, calls) = state(Choice::Deny, false);
    let state = state.with_storage(config_path.clone()).unwrap();
    let agent = current_identity();
    save_policy(&state, &agent, Policy::Ask).await;
    let command = Change {
        id: path_id(&agent.path),
        policy: Policy::Once,
        revision: 1,
        command_id: "once-test".into(),
        expires_at_ms: None,
    };
    state.set_agent_policy(command.clone()).await.unwrap();
    let (first, second) = tokio::join!(
        state.authorize_agent(prompt(), Some(agent.clone())),
        state.authorize_agent(prompt(), Some(agent.clone()))
    );
    let decisions = [first.unwrap().choice, second.unwrap().choice];
    assert_eq!(
        decisions.iter().filter(|c| **c == Choice::Accept).count(),
        1
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        Settings::load(config_path, (*state.config).clone())
            .unwrap()
            .saved
            .agent_policies[&path_id(&agent.path)]
            .policy,
        Policy::Ask
    );
    state.set_agent_policy(command).await.unwrap();
    assert_eq!(
        state
            .authorize_agent(prompt(), Some(agent))
            .await
            .unwrap()
            .choice,
        Choice::Deny
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn all_four_policies_and_unverified_callers_are_distinct_only_to_the_owner() {
    let (state, calls) = state(Choice::Accept, false);
    let agent = current_identity();
    for (policy, choice, asked) in [
        (Policy::Trust, Choice::Accept, 0),
        (Policy::Deny, Choice::Deny, 0),
        (Policy::Ask, Choice::Accept, 1),
    ] {
        calls.store(0, Ordering::SeqCst);
        save_policy(&state, &agent, policy).await;
        assert_eq!(
            state
                .authorize_agent(prompt(), Some(agent.clone()))
                .await
                .unwrap()
                .choice,
            choice
        );
        assert_eq!(calls.load(Ordering::SeqCst), asked);
    }
    save_policy(&state, &agent, Policy::Trust).await;
    calls.store(0, Ordering::SeqCst);
    let mut wrong = agent.clone();
    wrong.path.push_str("-different-path");
    state.authorize_agent(prompt(), Some(wrong)).await.unwrap();
    let mut reused_pid = agent.clone();
    reused_pid.started_at += 1;
    state
        .authorize_agent(prompt(), Some(reused_pid))
        .await
        .unwrap();
    state.authorize_agent(prompt(), None).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn remember_accept_adds_path_trust_and_does_not_limit_future_secret_keys() {
    let (state, calls) = state(Choice::Accept, true);
    let agent = current_identity();
    state
        .authorize_agent(prompt(), Some(agent.clone()))
        .await
        .unwrap();
    let mut more = prompt();
    more.secret_keys = vec!["NEW_SECRET".into()];
    state
        .authorize_agent(more, Some(agent.clone()))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        state.settings.lock().await.saved.agent_policies[&path_id(&agent.path)].policy,
        Policy::Trust
    );
    let (denied, _) = self::state(Choice::Deny, true);
    denied.authorize_agent(prompt(), Some(agent)).await.unwrap();
    assert!(denied.settings.lock().await.saved.agent_policies.is_empty());
    let (unverified, _) = self::state(Choice::Accept, true);
    unverified.authorize_agent(prompt(), None).await.unwrap();
    assert!(
        unverified
            .settings
            .lock()
            .await
            .saved
            .agent_policies
            .is_empty()
    );
}

#[tokio::test]
async fn policy_updates_require_discovered_paths_current_revision_and_unexpired_command() {
    let (state, _) = state(Choice::Deny, false);
    let agent = current_identity();
    let mut change = Change {
        id: path_id(&agent.path),
        policy: Policy::Trust,
        revision: 0,
        command_id: "test".into(),
        expires_at_ms: None,
    };
    assert!(state.set_agent_policy(change.clone()).await.is_err());
    save_policy(&state, &agent, Policy::Ask).await;
    assert_eq!(
        state
            .set_agent_policy(change.clone())
            .await
            .unwrap_err()
            .status_code(),
        409
    );
    change.revision = 1;
    change.expires_at_ms = Some(1);
    assert!(state.set_agent_policy(change.clone()).await.is_err());
    change.expires_at_ms = None;
    state.set_agent_policy(change).await.unwrap();
    assert_eq!(
        state
            .agent_list()
            .await
            .unwrap()
            .iter()
            .find(|a| a.id == path_id(&agent.path))
            .unwrap()
            .path
            .as_deref(),
        Some(agent.path.as_str())
    );
}

#[tokio::test]
async fn failed_persistence_does_not_spend_or_allow_a_single_use() {
    let dir = tempfile::tempdir().unwrap();
    let (state, calls) = state(Choice::Deny, false);
    let agent = current_identity();
    save_policy(&state, &agent, Policy::Once).await;
    let path = dir.path().join("blocked.toml");
    std::fs::create_dir(path.with_extension("authorization.json")).unwrap();
    state.settings.lock().await.config_path = Some(path);
    assert!(
        state
            .authorize_agent(prompt(), Some(agent.clone()))
            .await
            .is_err()
    );
    assert_eq!(
        state.settings.lock().await.saved.agent_policies[&path_id(&agent.path)].policy,
        Policy::Once
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
#[ignore = "subprocess fixture for process ownership tests"]
fn process_probe() {
    let Ok(endpoint) = std::env::var("TRUSTD_TEST_AGENT_ENDPOINT") else {
        return;
    };
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    if let Ok(adapter) = std::env::var("TRUSTD_TEST_CHILD_ADAPTER") {
        let mut command = std::process::Command::new(adapter);
        command
            .args([
                "--ignored",
                "--exact",
                "agents::tests::process_probe",
                "--nocapture",
            ])
            .env_remove("TRUSTD_TEST_CHILD_ADAPTER")
            .stdin(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        print!("{}", String::from_utf8_lossy(&result.stdout));
        return;
    }
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap();
    for _ in 0..2 {
        let value: serde_json::Value = client.post(format!("{endpoint}/v1/sessions/enter/interactive"))
            .json(&serde_json::json!({"agent_name":"forged friendly name", "request_reason":"test", "confirm_text":"OK"}))
            .send().unwrap().error_for_status().unwrap().json().unwrap();
        println!("AGENT_STATUS={}", value["status"].as_str().unwrap());
        for field in ["automatic", "policy", "reused", "remember"] {
            assert!(value.get(field).is_none());
        }
    }
}

#[tokio::test]
async fn real_process_scan_and_socket_owner_use_actual_path_not_self_report() {
    verify_real_process("codex").await;
}

#[tokio::test]
async fn deepseek_harness_is_discovered_and_its_adapter_uses_the_verified_parent() {
    verify_real_process("DeepSeek Harness").await;
}

async fn verify_real_process(executable_name: &str) {
    use tokio::io::AsyncWriteExt;
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join(if cfg!(windows) {
        format!("{executable_name}.exe")
    } else {
        executable_name.to_owned()
    });
    std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let adapter = directory.path().join(if cfg!(windows) {
        "fixture-adapter.exe"
    } else {
        "fixture-adapter"
    });
    std::fs::copy(std::env::current_exe().unwrap(), &adapter).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (mut state, calls) = self::state(Choice::Deny, false);
    let mut config = (*state.config).clone();
    config.bind = address;
    state.config = Arc::new(config);
    let server_state = state.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            crate::server::router(server_state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let mut command = tokio::process::Command::new(&executable);
    command
        .args([
            "--ignored",
            "--exact",
            "agents::tests::process_probe",
            "--nocapture",
        ])
        .env("TRUSTD_TEST_AGENT_ENDPOINT", format!("http://{address}"))
        .env("TRUSTD_TEST_CHILD_ADAPTER", &adapter)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    // Concurrent fork/exec fixtures can briefly retain a writable descriptor
    // to the freshly copied program. Retry only ETXTBSY; never hide a different
    // launch error or relax the process identity assertions below.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut child = loop {
        match command.spawn() {
            Ok(child) => break child,
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("could not launch process fixture: {error}"),
        }
    };
    let pid = child.id().unwrap();
    let agent = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(agent) = state
                .agent_list()
                .await
                .unwrap()
                .into_iter()
                .find(|a| a.processes.iter().any(|p| p.pid == pid))
            {
                break agent;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(agent.policy, Policy::Ask);
    assert!(
        agent
            .path
            .as_ref()
            .unwrap()
            .ends_with(executable.file_name().unwrap().to_str().unwrap())
    );
    state
        .set_agent_policy(Change {
            id: agent.id.clone(),
            policy: Policy::Once,
            revision: agent.revision,
            command_id: "process-test".into(),
            expires_at_ms: None,
        })
        .await
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"request\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(25), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("AGENT_STATUS=authorized"), "{stdout}");
    assert!(stdout.contains("AGENT_STATUS=denied"), "{stdout}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    state.shutdown().await;
    server.abort();
}
