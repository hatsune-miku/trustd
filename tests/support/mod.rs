//! A real, daemon-discovered client process for integration tests. Human policy
//! is set through authenticated management; no test-only daemon routes exist.
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub fn authorize(base: &str, password: &str) -> Value {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory
        .path()
        .join(if cfg!(windows) { "codex.exe" } else { "codex" });
    std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let mut command = Command::new(&executable);
    command
        .args([
            "--ignored",
            "--exact",
            "support::agent_client",
            "--nocapture",
        ])
        .env("TRUSTD_TEST_CLIENT_BASE", base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().unwrap();
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();
    let login: Value = http
        .post(format!("{base}/v1/admin/login"))
        .json(&json!({"credential":{"type":"password","value":password}}))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    let session = login["session"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let agent = loop {
        let list: Value = http
            .get(format!("{base}/v1/admin/agents"))
            .bearer_auth(session)
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        if let Some(agent) = list["agents"].as_array().unwrap().iter().find(|a| {
            a["processes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["pid"] == child.id())
        }) {
            break agent.clone();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("test agent was not discovered: {list}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        agent["path"]
            .as_str()
            .unwrap()
            .ends_with(executable.file_name().unwrap().to_str().unwrap())
    );
    http.post(format!("{base}/v1/admin/agents/policy")).bearer_auth(session)
        .json(&json!({"id":agent["id"],"policy":"once","revision":agent["revision"],"command_id":uuid::Uuid::new_v4().to_string()}))
        .send().unwrap().error_for_status().unwrap();
    child.stdin.take().unwrap().write_all(b"request\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(
        text.lines()
            .find_map(|line| line.strip_prefix("AGENT_JSON="))
            .expect("client response"),
    )
    .unwrap()
}

#[test]
#[ignore = "subprocess integration fixture"]
fn agent_client() {
    let Ok(base) = std::env::var("TRUSTD_TEST_CLIENT_BASE") else {
        return;
    };
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(70))
        .build()
        .unwrap();
    let result: Value = client.post(format!("{base}/v1/sessions/enter/interactive"))
        .json(&json!({"agent_name":"integration client","request_reason":"disposable fixture","confirm_text":"OK"}))
        .send().unwrap().error_for_status().unwrap().json().unwrap();
    assert_eq!(result["status"], "authorized");
    println!("AGENT_JSON={result}");
}
