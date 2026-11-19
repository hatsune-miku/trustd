//! Scoped MCP adapter for a trustd-owned remote Codex conversation.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{net::SocketAddr, path::Path};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub(super) fn allowed(operation: &str) -> bool {
    matches!(
        operation,
        "run"
            | "job_start"
            | "job_get"
            | "job_wait"
            | "job_list"
            | "job_cancel"
            | "job_output"
            | "file_stat"
            | "file_read"
            | "file_write"
    )
}

pub fn operation_schemas() -> Value {
    let text = json!({"type":"string"});
    let number = json!({"type":"integer","minimum":0});
    let boolean = json!({"type":"boolean"});
    let mut run = json!({"session_ref":text,"command":text,"stdin":text,"timeout_seconds":{"type":["integer","null"],"minimum":1}});
    let mut required = vec!["session_ref", "command"];
    if cfg!(windows) {
        run["privilege_level"] = json!({"type":"string","enum":["current-user","as-administrator","nt-authority/system"]});
        required.push("privilege_level");
    } else {
        run["sudo"] = boolean.clone();
        required.push("sudo");
    }
    let mut job = run.clone();
    job["idempotency_key"] = text.clone();
    let mut job_required = required.clone();
    job_required.push("idempotency_key");
    let specs = vec![
        (
            "logs_page",
            "Read this device's command or audit records on the target host.",
            json!({"kind":{"type":"string","enum":["command","audit"]},"before":number,"limit":number}),
            vec!["kind"],
        ),
        (
            "secrets_list",
            "List all credential names within this device's grant.",
            json!({}),
            vec![],
        ),
        (
            "session_open",
            "Open a remote session; load profiles, default cwd is the target identity's home.",
            json!({"secret_keys":{"type":"array","items":text},"sudo":boolean,"cwd":{"type":["string","null"]}}),
            vec![],
        ),
        (
            "session_close",
            "Close the session; independent jobs continue.",
            json!({"session_ref":text}),
            vec!["session_ref"],
        ),
        (
            "run",
            "Run in the persistent shell. No default timeout. Do not retry an ambiguous execution.",
            run,
            required,
        ),
        (
            "job_start",
            "Submit a durable task with a stable idempotency_key. Wait with job_wait instead of polling. Output goes to stdout_path/stderr_path on the target: read it with job_output, or process it there with run (for example compress it first). No default runtime limit.",
            job,
            job_required,
        ),
        (
            "job_get",
            "Inspect task status and result.",
            json!({"job_id":text}),
            vec!["job_id"],
        ),
        (
            "job_wait",
            "Wait until the task finishes or timeout_seconds pass (default 300, at most 3600), then return it; check status to tell which.",
            json!({"job_id":text,"timeout_seconds":{"type":"integer","minimum":1}}),
            vec!["job_id"],
        ),
        (
            "job_list",
            "List this device's tasks.",
            json!({"offset":number,"limit":number}),
            vec![],
        ),
        (
            "job_cancel",
            "Cancel this job and its process tree.",
            json!({"job_id":text}),
            vec!["job_id"],
        ),
        (
            "job_output",
            "Read stdout/stderr as text, or as base64 data when the bytes are not text or encoding is base64. tail reads the last N bytes; next_offset continues losslessly.",
            json!({"job_id":text,"stream":{"type":"string","enum":["stdout","stderr"]},"offset":number,"tail":number,"limit":number,"encoding":{"type":"string","enum":["text","base64"]}}),
            vec!["job_id"],
        ),
        (
            "file_stat",
            "Inspect an absolute remote path; compute optional checksum on this host.",
            json!({"session_ref":text,"path":text,"checksum":boolean}),
            vec!["session_ref", "path"],
        ),
        (
            "file_read",
            "Read an absolute remote path as text (at most 256 KiB), or as base64 data (at most 1 MiB) when the bytes are not text or encoding is base64. tail reads the last N bytes.",
            json!({"session_ref":text,"path":text,"offset":number,"tail":number,"limit":number,"encoding":{"type":"string","enum":["text","base64"]}}),
            vec!["session_ref", "path"],
        ),
        (
            "file_write",
            "Upload a base64 chunk of at most 1 MiB. Stable transfer_id is a UUID; complete returns sha256 and verifies it when supplied. Existing files require replace.",
            json!({"session_ref":text,"path":text,"transfer_id":text,"offset":number,"data":text,"complete":boolean,"sha256":text,"replace":boolean}),
            vec!["session_ref", "path", "transfer_id", "offset", "data"],
        ),
        (
            "agent_list",
            "List this device's persisted Codex conversations, including stopped processes.",
            json!({"offset":number,"limit":number}),
            vec![],
        ),
        (
            "agent_start",
            "Create a trustd-owned Codex conversation; local Codex configuration supplies defaults.",
            json!({"session_ref":text,"cwd":text,"model":text,"approval_policy":text,"sandbox":text}),
            vec!["session_ref"],
        ),
        (
            "agent_resume",
            "Resume a trustd-owned conversation after its process stopped.",
            json!({"session_ref":text,"agent_id":text,"cwd":text}),
            vec!["session_ref", "agent_id"],
        ),
        (
            "agent_send",
            "Start one turn; observe events and status. Do not repeat after an unknown outcome.",
            json!({"agent_id":text,"message":text}),
            vec!["agent_id", "message"],
        ),
        (
            "agent_get",
            "Inspect state and pending user questions.",
            json!({"agent_id":text}),
            vec!["agent_id"],
        ),
        (
            "agent_events",
            "Read persisted events with an offset cursor.",
            json!({"agent_id":text,"offset":number}),
            vec!["agent_id"],
        ),
        (
            "agent_interrupt",
            "Interrupt the active turn; independent jobs continue.",
            json!({"agent_id":text}),
            vec!["agent_id"],
        ),
        (
            "agent_close",
            "Stop Codex; independent jobs continue.",
            json!({"agent_id":text}),
            vec!["agent_id"],
        ),
    ];
    json!(specs.into_iter().map(|(name,description,properties,required)| {
        let mut value = json!({"name":name,"description":description,
            "inputSchema":{"type":"object","properties":properties,"additionalProperties":false},"outputSchema":{"type":"object","properties":{},"additionalProperties":true}});
        if !required.is_empty() { value["inputSchema"]["required"] = json!(required); }
        value
    }).collect::<Vec<_>>())
}

pub async fn serve(endpoint: SocketAddr, token_file: &Path) -> Result<()> {
    ensure!(
        endpoint.ip().is_loopback(),
        "agent endpoint must be loopback"
    );
    let token = zeroize::Zeroizing::new(std::fs::read_to_string(token_file)?);
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
    while let Some(line) = input.next_line().await? {
        ensure!(line.len() <= 2 * 1024 * 1024, "MCP request too large");
        let request: Value = serde_json::from_str(&line)?;
        let Some(id) = request.get("id") else {
            continue;
        };
        let result:Result<Value> = async {
            match request["method"].as_str() {
                Some("initialize") => Ok(json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"trustd-remote-jobs","version":crate::VERSION},"instructions":"Execute on this host. Durable jobs survive this conversation; wait for them with job_wait. Use explicit cancellation to stop them."})),
                Some("ping") => Ok(json!({})),
                Some("tools/list") => {
                    let tools: Vec<_> = operation_schemas().as_array().unwrap().iter().filter(|d| allowed(d["name"].as_str().unwrap())).map(|d| {
                        let mut d=d.clone(); d["inputSchema"]["properties"].as_object_mut().unwrap().remove("session_ref");
                        if let Some(required) = d["inputSchema"].get_mut("required").and_then(Value::as_array_mut) {
                            required.retain(|v| v != "session_ref");
                            if required.is_empty() { d["inputSchema"].as_object_mut().unwrap().remove("required"); }
                        }
                        d
                    }).collect();
                    Ok(json!({"tools":tools}))
                }
                Some("tools/call") => {
                    let name=request["params"]["name"].as_str().context("missing tool name")?;
                    ensure!(allowed(name),"unknown delegated tool");
                    let response=client.post(format!("http://{endpoint}/v2/agent/invoke")).bearer_auth(token.trim()).json(&json!({"operation":name,"arguments":request["params"].get("arguments").cloned().unwrap_or_else(|| json!({}))})).send().await?;
                    let status=response.status(); let value:Value=response.json().await?;
                    ensure!(status.is_success(),"{}",value["error"].as_str().unwrap_or("delegated operation failed"));
                    Ok(json!({"content":[{"type":"text","text":serde_json::to_string(&value)?}],"structuredContent":value,"isError":false}))
                }
                _ => anyhow::bail!("unknown MCP method"),
            }
        }.await;
        let response = match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(error) => {
                json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":error.to_string()}],"isError":true}})
            }
        };
        let mut bytes = serde_json::to_vec(&response)?;
        bytes.push(b'\n');
        output.write_all(&bytes).await?;
        output.flush().await?;
    }
    Ok(())
}
