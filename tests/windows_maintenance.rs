//! Read-only wizard smoke tests and disposable elevated user-daemon fixtures.
#![cfg(windows)]

use std::{
    io::{BufRead, BufReader},
    process::{Child, Stdio},
    time::Duration,
};

use localshelld::{auth::hash_password, config::Config, process::background_command};
use localshelld_native_process::current_identity;
use serde_json::{Value, json};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

#[test]
fn wizard_accepts_the_interactive_login_even_when_elevated() {
    let owner = current_identity().unwrap();
    let mut command = background_command(env!("CARGO_BIN_EXE_trustd"));
    command.args(["maintenance", "--no-open"]);
    if !owner.is_interactive_user() {
        let result = command.output().unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("logged-in account"));
        return;
    }
    let mut process = Process(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let output = process.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let line = line.unwrap();
            if let Some(url) = line.strip_prefix("Installation and maintenance: ") {
                let _ = send.send(url.to_owned());
                break;
            }
        }
    });
    // The URL's fragment is a temporary credential; never print it in test logs.
    let url = receive
        .recv_timeout(Duration::from_secs(15))
        .expect("wizard did not start");
    reader.join().unwrap();
    let (page, token) = url.split_once('#').unwrap();
    let base = page.strip_suffix("/maintenance.html").unwrap();
    let http = client();
    let status: Value = http
        .get(format!("{base}/setup/v1/status"))
        .bearer_auth(token)
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(status["version"], localshelld::VERSION);
    assert_eq!(status["platform"], "windows");
    assert_eq!(status["job"]["running"], false);
    assert_eq!(
        http.post(format!("{base}/setup/v1/install"))
            .json(&json!({}))
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    http.post(format!("{base}/setup/v1/close"))
        .bearer_auth(token)
        .json(&json!({}))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(result) = process.0.try_wait().unwrap() {
            assert!(result.success());
            break;
        }
        assert!(std::time::Instant::now() < deadline, "wizard did not close");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn elevated_daemon_supervises_only_a_medium_same_user_child() {
    let owner = current_identity().unwrap();
    if !owner.elevated || !owner.is_interactive_user() {
        // Run this same test executable in an interactive HighestAvailable task
        // for the elevated regression; SYSTEM/Session 0 are intentionally refused.
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("user/config.toml");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = port.local_addr().unwrap();
    let config = Config {
        password_hash: hash_password(b"disposable elevation fixture").unwrap(),
        bind: address,
        privileged_daemon: "127.0.0.1:0".parse().unwrap(),
        ..Config::default()
    };
    config.save(&path).unwrap();
    let protected = directory.path().join("administrators-only.txt");
    std::fs::write(&protected, b"privileged fixture").unwrap();
    localshelld_native_process::restrict_path(
        &protected,
        false,
        localshelld_native_process::FileAccess::System,
    )
    .unwrap();
    let config_before = std::fs::read(&path).unwrap();
    drop(port);
    let mut process = Process(
        background_command(env!("CARGO_BIN_EXE_trustd-user"))
            .arg("--config")
            .arg(&path)
            .arg("--headless")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let http = client();
    let base = format!("http://{address}");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "elevated supervisor exited"
        );
        if let Ok(response) = http.get(format!("{base}/health")).send() {
            let health: Value = response.error_for_status().unwrap().json().unwrap();
            assert_eq!(health["daemon"], "user");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "reduced user daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let issued: Value = http
        .post(format!("{base}/v1/admin/tokens/issue"))
        .json(&json!({"credential":{"type":"password", "value":"disposable elevation fixture"}}))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    let entered: Value = http
        .post(format!("{base}/v1/sessions/enter"))
        .json(&json!({"token":issued["token"]}))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(entered["privilege_levels"], json!(["current-user"]));
    let handle = entered["handle"].as_str().unwrap();
    let result: Value = http.post(format!("{base}/v1/commands/run"))
        .json(&json!({"handle":handle,"privilege_level":"current-user","command":"[Security.Principal.WindowsIdentity]::GetCurrent().User.Value"}))
        .send().unwrap().error_for_status().unwrap().json().unwrap();
    assert_eq!(result["identity"]["elevated"], owner.elevated);
    assert_eq!(result["identity"]["restricted_user"], true);
    assert_eq!(result["identity"]["is_system"], false);
    assert_eq!(result["identity"]["integrity"], "Medium");
    assert_eq!(result["identity"]["sid"], owner.sid);
    assert_eq!(result["identity"]["session_id"], owner.session_id);
    assert_eq!(result["stdout"].as_str().unwrap().trim(), owner.sid);
    let protected = protected.to_str().unwrap().replace('\'', "''");
    let command = format!(
        "$p = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent()); if ($p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {{ throw 'administrator membership survived' }}; try {{ [IO.File]::ReadAllText('{protected}') | Out-Null; throw 'protected file was readable' }} catch {{ if ($_.Exception.GetBaseException() -is [UnauthorizedAccessException]) {{ 'access denied as expected' }} else {{ throw }} }}"
    );
    let denied: Value = http
        .post(format!("{base}/v1/commands/run"))
        .json(&json!({"handle":handle,"privilege_level":"current-user","command":command}))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    // The shell deliberately reports recorded PowerShell errors even when the
    // command catches them. The expected access denial must be the only result.
    assert_eq!(
        denied["success"], false,
        "restricted access probe: {denied}"
    );
    assert_eq!(
        denied["stdout"].as_str().unwrap().trim(),
        "access denied as expected"
    );
    http.post(format!("{base}/v1/sessions/destroy"))
        .json(&json!({"handle":handle}))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), config_before);
    drop(process);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(address).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "supervisor left its daemon running"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
