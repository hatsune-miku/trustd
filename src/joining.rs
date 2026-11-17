//! Join this device to its owner's backend through the running user daemon.
//! Works on headless hosts: the owner approves on any browser, such as a phone.
use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use localshelld::config::{Config, user_config_path};
use serde_json::{Value, json};

fn config(path: Option<PathBuf>) -> Result<(PathBuf, Config)> {
    let path = path.map_or_else(user_config_path, Ok)?;
    let config = Config::load(&path)
        .with_context(|| format!("user daemon configuration {}", path.display()))?;
    Ok((path, config))
}

/// Print a single-use management link for this user's daemon.
pub fn login_url(path: Option<PathBuf>) -> Result<()> {
    let (path, config) = config(path)?;
    let ticket = localshelld::login::write_ticket(&path)?;
    println!("http://localhost:{}/#ticket={ticket}", config.bind.port());
    Ok(())
}

fn post(
    client: &reqwest::blocking::Client,
    base: &str,
    path: &str,
    session: Option<&str>,
    body: Value,
) -> Result<Value> {
    let mut request = client.post(format!("{base}{path}")).json(&body);
    if let Some(session) = session {
        request = request.bearer_auth(session);
    }
    let response = request
        .send()
        .context("the user daemon is not reachable; install or start trustd first")?;
    let status = response.status();
    let value: Value = response.json().unwrap_or(Value::Null);
    if !status.is_success() {
        bail!(
            "{}",
            value["error"]
                .as_str()
                .map_or_else(|| format!("HTTP {status}"), str::to_owned)
        );
    }
    Ok(value)
}

pub(crate) fn device_name() -> String {
    let name = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
        .unwrap_or_else(|| "trustd device".into());
    name.chars().take(80).collect()
}

/// Print this device's backend, connection, peers and trust rules.
pub fn status(path: Option<PathBuf>) -> Result<()> {
    let (path, config) = config(path)?;
    let base = format!("http://127.0.0.1:{}", config.bind.port());
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let ticket = localshelld::login::write_ticket(&path)?;
    let login = post(
        &client,
        &base,
        "/v1/admin/login",
        None,
        json!({"credential":{"type":"ticket","value":ticket}}),
    )?;
    let session = login["session"]
        .as_str()
        .context("invalid management login response")?;
    let remote: Value = client
        .get(format!("{base}/v2/admin/remote"))
        .bearer_auth(session)
        .send()?
        .error_for_status()?
        .json()?;
    let _ = post(&client, &base, "/v1/admin/logout", Some(session), json!({}));
    let Some(backend) = remote["backend"]
        .as_str()
        .filter(|_| remote["joined"] == true)
    else {
        println!("Not joined. Run `trustd join <backend URL>`.");
        return Ok(());
    };
    println!(
        "Backend: {backend} ({})\nThis device: {}",
        if remote["connected"] == true {
            "connected"
        } else {
            "not connected"
        },
        remote["device_id"].as_str().unwrap_or_default()
    );
    let own = remote["device_id"].as_str().unwrap_or_default();
    let rules = remote["rules"].as_array().cloned().unwrap_or_default();
    for (id, peer) in remote["peers"].as_object().into_iter().flatten() {
        let restrictions: Vec<String> = rules
            .iter()
            .filter(|rule| {
                (rule["from"] == id.as_str() && rule["to"] == own)
                    || (rule["from"] == own && rule["to"] == id.as_str())
            })
            .map(|rule| {
                format!(
                    "{} denies {}",
                    if rule["from"] == id.as_str() {
                        "peer → here"
                    } else {
                        "here → peer"
                    },
                    rule["deny"].as_str().unwrap_or_default()
                )
            })
            .collect();
        println!(
            "Peer: {} {id} — {}",
            peer["label"].as_str().unwrap_or_default(),
            if restrictions.is_empty() {
                "trusted".into()
            } else {
                restrictions.join("; ")
            }
        );
    }
    Ok(())
}

/// Join the backend at `url`; the owner approves with one passkey tap.
pub fn join(url: String, name: Option<String>, path: Option<PathBuf>) -> Result<()> {
    join_with(url, name, path, |uri, code| {
        println!(
            "Open this link in any browser and approve this device with the owner's passkey:\n{uri}\nVerification code: {code}"
        );
    })?;
    println!("Joined. This device now trusts the owner's other devices.");
    Ok(())
}

/// Join through the running user daemon; `started` receives the approval
/// link and verification code to show the owner.
pub(crate) fn join_with(
    url: String,
    name: Option<String>,
    path: Option<PathBuf>,
    started: impl FnOnce(&str, &str),
) -> Result<()> {
    let (path, config) = config(path)?;
    let base = format!("http://127.0.0.1:{}", config.bind.port());
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let ticket = localshelld::login::write_ticket(&path)?;
    let login = post(
        &client,
        &base,
        "/v1/admin/login",
        None,
        json!({"credential":{"type":"ticket","value":ticket}}),
    )?;
    let session = login["session"]
        .as_str()
        .context("invalid management login response")?;
    let request = post(
        &client,
        &base,
        "/v1/admin/sync",
        Some(session),
        json!({"action":"start","url":url,"name":name.unwrap_or_else(device_name)}),
    )?;
    started(
        request["verification_uri"].as_str().unwrap_or_default(),
        request["user_code"].as_str().unwrap_or_default(),
    );
    let deadline =
        Instant::now() + Duration::from_secs(request["expires_in"].as_u64().unwrap_or(600));
    let interval = Duration::from_secs(request["interval"].as_u64().unwrap_or(3).max(3));
    while Instant::now() < deadline {
        thread::sleep(interval);
        let result = post(
            &client,
            &base,
            "/v1/admin/sync",
            Some(session),
            json!({"action":"poll"}),
        )?;
        if result["connected"] == true {
            return Ok(());
        }
    }
    bail!("the approval expired; run `trustd join` again")
}
