//! One owned Codex app-server per remote conversation. Computation jobs are not
//! children of this process and therefore survive a turn/agent interruption.
use super::string;
use crate::{server::AppState, shell::transport::Child, storage};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Mutex, mpsc, oneshot, watch},
};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Metadata {
    pub id: String,
    pub owner: String,
    pub thread_id: Option<String>,
    pub cwd: String,
    pub status: String,
    pub active_turn: Option<String>,
    pub sequence: u64,
    pub last_message: String,
    pub error: Option<String>,
}

pub(super) struct Agent {
    pub id: String,
    pub owner: String,
    metadata: Mutex<Metadata>,
    directory: PathBuf,
    sender: mpsc::Sender<Value>,
    pending: Mutex<BTreeMap<String, oneshot::Sender<Value>>>,
    questions: Mutex<BTreeMap<String, Value>>,
    counter: AtomicU64,
    stopped: watch::Sender<bool>,
    finished: watch::Sender<bool>,
    turn_gate: Mutex<()>,
    pub capability: String,
    pub session_ref: String,
    pub parent_session_ref: String,
}

impl Agent {
    pub fn is_running(&self) -> bool {
        !*self.stopped.borrow()
    }
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        ensure!(
            !*self.stopped.borrow(),
            "Codex process is stopped; resume the conversation"
        );
        let id = format!("trustd-{}", self.counter.fetch_add(1, Ordering::Relaxed));
        let (reply, result) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), reply);
        let message = json!({"id":id,"method":method,"params":params});
        if self.sender.send(message).await.is_err() {
            self.pending.lock().await.remove(&id);
            anyhow::bail!("Codex process is unavailable");
        }
        let response = tokio::time::timeout(Duration::from_secs(90), result).await;
        self.pending.lock().await.remove(&id);
        let response = response.context(
            "Codex control request timed out; inspect the conversation before retrying",
        )??;
        if let Some(error) = response.get("error") {
            anyhow::bail!("Codex rejected request: {error}");
        }
        response
            .get("result")
            .cloned()
            .context("invalid Codex response")
    }

    async fn event(&self, mut message: Value) -> Result<()> {
        let mut metadata = self.metadata.lock().await;
        metadata.sequence += 1;
        match message["method"].as_str() {
            Some("trustd/userResponded") => metadata.status = "running".into(),
            Some("turn/started") => {
                metadata.status = "running".into();
                metadata.active_turn = message["params"]["turn"]["id"].as_str().map(str::to_owned);
            }
            Some("turn/completed") => {
                metadata.status = match message["params"]["turn"]["status"].as_str() {
                    Some("completed") => "idle",
                    Some("interrupted") => "interrupted",
                    _ => "failed",
                }
                .into();
                metadata.active_turn = None;
                metadata.error = message["params"]["turn"]["error"]
                    .as_object()
                    .map(|_| message["params"]["turn"]["error"].to_string());
            }
            Some("item/completed") if message["params"]["item"]["type"] == "agentMessage" => {
                metadata.last_message = message["params"]["item"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(32768)
                    .collect();
            }
            _ => {}
        }
        if message.get("method").is_some() && message.get("id").is_some() {
            metadata.status = "waiting_user".into();
            self.questions
                .lock()
                .await
                .insert(message["id"].to_string(), message.clone());
        }
        message["trustd_sequence"] = json!(metadata.sequence);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.directory.join("events.jsonl"))?;
        serde_json::to_writer(&mut file, &message)?;
        file.write_all(b"\n")?;
        storage::save_json(&self.directory.join("agent.json"), &*metadata)?;
        Ok(())
    }

    async fn view(&self) -> Result<Value> {
        let mut value = serde_json::to_value(&*self.metadata.lock().await)?;
        value["questions"] = json!(self.questions.lock().await.values().collect::<Vec<_>>());
        Ok(value)
    }

    pub async fn stop(&self) {
        self.stopped.send_replace(true);
        let mut finished = self.finished.subscribe();
        while !*finished.borrow() {
            if finished.changed().await.is_err() {
                break;
            }
        }
    }
}

impl AppState {
    pub(super) async fn remote_codex(
        &self,
        owner: &str,
        operation: &str,
        args: &Value,
    ) -> Result<Value> {
        if operation == "agent_list" {
            let directory = self.remote.directory()?.join("agents");
            let mut conversations = Vec::new();
            if directory.is_dir() {
                for entry in std::fs::read_dir(directory)? {
                    let entry = entry?;
                    let id = entry.file_name().to_string_lossy().into_owned();
                    if let Ok(mut metadata) = self.codex_metadata(owner, &id) {
                        if !self
                            .remote
                            .agents
                            .lock()
                            .await
                            .get(&id)
                            .is_some_and(|a| a.is_running())
                        {
                            metadata.status = "stopped".into();
                            metadata.active_turn = None;
                        }
                        conversations.push(metadata);
                    }
                }
            }
            conversations.sort_by(|a, b| a.id.cmp(&b.id));
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            let limit = args["limit"].as_u64().unwrap_or(30).clamp(1, 100) as usize;
            let total = conversations.len();
            return Ok(
                json!({"agents":conversations.into_iter().skip(offset).take(limit).collect::<Vec<_>>(),"next_offset":(offset+limit<total).then_some(offset+limit)}),
            );
        }
        if matches!(operation, "agent_start" | "agent_resume") {
            return self
                .codex_start(owner, args, operation == "agent_resume")
                .await;
        }
        let id = string(args, "agent_id")?;
        let agent = self.remote.agents.lock().await.get(id).cloned();
        let Some(agent) = agent else {
            let mut metadata = self.codex_metadata(owner, id)?;
            metadata.status = "stopped".into();
            metadata.active_turn = None;
            if operation == "agent_get" {
                return Ok(serde_json::to_value(metadata)?);
            }
            if operation == "agent_events" {
                return read_events(&self.remote.directory()?.join("agents").join(id), args);
            }
            anyhow::bail!(
                "Codex process is not running; use agent_resume with a new authorized session"
            );
        };
        ensure!(
            agent.owner == owner,
            "conversation belongs to another device"
        );
        match operation {
            "agent_get" => agent.view().await,
            "agent_events" => read_events(&agent.directory, args),
            "agent_send" => {
                let _gate = agent.turn_gate.lock().await;
                let metadata = agent.metadata.lock().await.clone();
                ensure!(
                    metadata.active_turn.is_none(),
                    "a turn is already running; interrupt it or wait before sending another message"
                );
                let thread = metadata.thread_id.context("conversation has not started")?;
                agent.metadata.lock().await.status = "starting_turn".into();
                let result = agent.request("turn/start",json!({"threadId":thread,"input":[{"type":"text","text":string(args,"message")?}]})).await?;
                let mut metadata = agent.metadata.lock().await;
                if metadata.status == "starting_turn" {
                    metadata.active_turn = result["turn"]["id"].as_str().map(str::to_owned);
                    metadata.status = "running".into();
                }
                storage::save_json(&agent.directory.join("agent.json"), &*metadata)?;
                Ok(result)
            }
            "agent_interrupt" => {
                let metadata = agent.metadata.lock().await.clone();
                if let Some(turn) = metadata.active_turn {
                    agent
                        .request(
                            "turn/interrupt",
                            json!({"threadId":metadata.thread_id,"turnId":turn}),
                        )
                        .await?;
                }
                Ok(json!({"interrupted":true}))
            }
            "agent_respond" => {
                let request_id = args.get("request_id").context("missing request_id")?;
                let key = request_id.to_string();
                ensure!(
                    agent.questions.lock().await.contains_key(&key),
                    "question is not pending"
                );
                let result = args.get("result").context("missing response result")?;
                agent
                    .sender
                    .send(json!({"id":request_id,"result":result}))
                    .await?;
                agent.questions.lock().await.remove(&key);
                agent
                    .event(
                        json!({"method":"trustd/userResponded","params":{"request_id":request_id}}),
                    )
                    .await?;
                Ok(json!({"accepted":true}))
            }
            "agent_close" => {
                agent.stop().await;
                Ok(json!({"closed":true}))
            }
            _ => anyhow::bail!("unknown Codex operation"),
        }
    }

    fn codex_metadata(&self, owner: &str, id: &str) -> Result<Metadata> {
        uuid::Uuid::parse_str(id)?;
        let metadata: Metadata = serde_json::from_slice(&std::fs::read(
            self.remote
                .directory()?
                .join("agents")
                .join(id)
                .join("agent.json"),
        )?)?;
        ensure!(
            metadata.owner == owner,
            "conversation belongs to another device"
        );
        Ok(metadata)
    }

    async fn codex_start(&self, owner: &str, args: &Value, resume: bool) -> Result<Value> {
        ensure!(
            self.mode == crate::server::DaemonMode::User,
            "Codex runs in the user daemon"
        );
        let session_ref = string(args, "session_ref")?.to_owned();
        let session = self.remote.session(owner, &session_ref).await?;
        let environment = self.session_environment(&session.handle).await?;
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or(session.options.cwd.clone())
            .or_else(|| {
                directories::BaseDirs::new().map(|d| d.home_dir().to_string_lossy().into_owned())
            })
            .context("cannot determine user directory")?;
        ensure!(
            Path::new(&cwd).is_absolute() && Path::new(&cwd).is_dir(),
            "Codex cwd must be an existing absolute directory"
        );
        let mut metadata = if resume {
            self.codex_metadata(owner, string(args, "agent_id")?)?
        } else {
            Metadata {
                id: uuid::Uuid::new_v4().to_string(),
                owner: owner.into(),
                thread_id: None,
                cwd: cwd.clone(),
                status: "starting".into(),
                active_turn: None,
                sequence: 0,
                last_message: String::new(),
                error: None,
            }
        };
        let creation_gate = self.remote.agent_start.lock().await;
        if let Some(existing) = self.remote.agents.lock().await.get(&metadata.id).cloned() {
            ensure!(
                *existing.finished.borrow(),
                "conversation already has a running Codex process"
            );
        }
        if resume {
            ensure!(
                metadata.cwd == cwd,
                "resume requires the conversation's original cwd"
            );
        }
        metadata.status = "starting".into();
        metadata.active_turn = None;
        let directory = self.remote.directory()?.join("agents").join(&metadata.id);
        std::fs::create_dir_all(&directory)?;
        crate::config::set_private_dir(&directory)?;
        let capability = crate::server::strong_handle();
        storage::write_private(&directory.join("capability"), capability.as_bytes())?;
        storage::save_json(&directory.join("agent.json"), &metadata)?;
        let adapter = std::env::current_exe()?.with_file_name(if cfg!(windows) {
            "trustd-agent-stdio.exe"
        } else {
            "trustd-agent-stdio"
        });
        ensure!(
            adapter.is_file(),
            "trustd-agent-stdio is missing from this installation"
        );
        let mcp_args = vec![
            "--endpoint".to_owned(),
            self.config.bind.to_string(),
            "--token-file".into(),
            directory.join("capability").to_string_lossy().into_owned(),
        ];
        let command = vec![
            "codex".to_owned(),
            "-c".into(),
            format!(
                "mcp_servers.trustd_remote.command={}",
                serde_json::to_string(&adapter.to_string_lossy())?
            ),
            "-c".into(),
            format!(
                "mcp_servers.trustd_remote.args={}",
                serde_json::to_string(&mcp_args)?
            ),
            "app-server".into(),
        ];
        let (mut child, mut stdin, stdout, stderr) = spawn(&command, &cwd, &environment.0)?;
        let delegated = match self
            .remote_open(
                owner,
                &json!({"cwd":cwd,"sudo":session.options.sudo,"secret_keys":session.keys}),
            )
            .await
        {
            Ok(value) => value["session_ref"].as_str().unwrap().to_owned(),
            Err(error) => {
                child.kill_tree().await;
                return Err(error);
            }
        };
        let (sender, mut receive) = mpsc::channel::<Value>(32);
        let (stop, mut stopped) = watch::channel(false);
        let (finished, _) = watch::channel(false);
        let agent = Arc::new(Agent {
            id: metadata.id.clone(),
            owner: owner.into(),
            metadata: Mutex::new(metadata.clone()),
            directory: directory.clone(),
            sender,
            pending: Default::default(),
            questions: Default::default(),
            counter: AtomicU64::new(1),
            stopped: stop,
            finished,
            turn_gate: Default::default(),
            capability,
            session_ref: delegated,
            parent_session_ref: session_ref,
        });
        self.remote
            .agents
            .lock()
            .await
            .insert(metadata.id.clone(), agent.clone());
        drop(creation_gate);
        let stdout_agent = agent.clone();
        let secret_values: Vec<_> = environment
            .0
            .values()
            .cloned()
            .chain(std::iter::once(agent.capability.clone()))
            .collect();
        let stderr_secrets = secret_values.clone();
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                let count = lines.read_until(b'\n', &mut line).await?;
                if count == 0 {
                    break;
                }
                ensure!(
                    line.len() <= 2 * 1024 * 1024,
                    "Codex event exceeds size limit"
                );
                let Ok(mut message) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                if message.get("method").is_none()
                    && let Some(id) = message.get("id").and_then(Value::as_str)
                    && let Some(reply) = stdout_agent.pending.lock().await.remove(id)
                {
                    let _ = reply.send(message);
                    continue;
                }
                // Bearers are not part of the conversation/event API.
                redact(&mut message, &secret_values);
                stdout_agent.event(message).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let stderr_path = directory.join("stderr");
        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while reader.read_line(&mut line).await? != 0 {
                for value in &stderr_secrets {
                    if !value.is_empty() {
                        line = line.replace(value, "[credential]");
                    }
                }
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&stderr_path)?;
                file.write_all(line.as_bytes())?;
                line.clear();
            }
            Ok::<_, anyhow::Error>(())
        });
        let control_agent = agent.clone();
        let control_state = self.clone();
        tokio::spawn(async move {
            let mut stdout_task = stdout_task;
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    _ = &mut stdout_task => break,
                    message = receive.recv() => {
                        let Some(message) = message else { break; };
                        let Ok(mut bytes) = serde_json::to_vec(&message) else { break; };
                        bytes.push(b'\n');
                        if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() { break; }
                    }
                }
            }
            child.kill_tree().await;
            stdout_task.abort();
            stderr_task.abort();
            let delegated = control_state
                .remote
                .sessions
                .lock()
                .await
                .remove(&control_agent.session_ref);
            if let Some(session) = delegated {
                let _ = control_state.destroy_session(&session.handle).await;
            }
            let _ = control_agent.stopped.send(true);
            control_agent.pending.lock().await.clear();
            let mut metadata = control_agent.metadata.lock().await;
            metadata.status = "stopped".into();
            metadata.active_turn = None;
            let _ = storage::save_json(&control_agent.directory.join("agent.json"), &*metadata);
            control_agent.finished.send_replace(true);
        });
        let initialized: Result<Value> = async {
            agent.request("initialize",json!({"clientInfo":{"name":"trustd","title":"trustd Remote Subagent","version":crate::VERSION},"capabilities":{"experimentalApi":false}})).await?;
            agent.sender.send(json!({"method":"initialized","params":{}})).await?;
            let mut params = json!({"cwd":cwd});
            if let Some(model) = args.get("model").filter(|v| v.is_string()) { params["model"] = model.clone(); }
            if let Some(policy) = args.get("approval_policy").filter(|v| v.is_string()) { params["approvalPolicy"] = policy.clone(); }
            if let Some(sandbox) = args.get("sandbox").filter(|v| v.is_string()) { params["sandbox"] = sandbox.clone(); }
            params["developerInstructions"] = json!("You run on the remote host. Keep file processing and computation here. Use trustd_remote job_start for work that must outlive your turn; job_get and job_output observe it. Ending or interrupting this conversation does not cancel those jobs. Return concise summaries and remote artifact paths.");
            let method = if resume { params["threadId"] = json!(metadata.thread_id); "thread/resume" } else { "thread/start" };
            let thread = agent.request(method,params).await?;
            let id = thread["thread"]["id"].as_str().context("Codex did not return a thread ID")?.to_owned();
            let mut metadata = agent.metadata.lock().await;
            metadata.thread_id = Some(id); metadata.status = "idle".into();
            storage::save_json(&directory.join("agent.json"),&*metadata)?; drop(metadata);
            agent.view().await
        }.await;
        if initialized.is_err() {
            agent.stop().await;
        }
        initialized
    }
}

type Reader = Box<dyn AsyncRead + Unpin + Send>;
type Writer = Box<dyn AsyncWrite + Unpin + Send>;
fn redact(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => {
            for secret in secrets.iter().filter(|s| !s.is_empty()) {
                *text = text.replace(secret, "[credential]");
            }
        }
        Value::Array(values) => values.iter_mut().for_each(|v| redact(v, secrets)),
        Value::Object(values) => values.values_mut().for_each(|v| redact(v, secrets)),
        _ => {}
    }
}
fn spawn(
    args: &[String],
    cwd: &str,
    environment: &BTreeMap<String, String>,
) -> Result<(Child, Writer, Reader, Reader)> {
    #[cfg(windows)]
    {
        use base64::Engine as _;
        let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let script = format!(
            "Set-Location -LiteralPath {}; & {}",
            quote(cwd),
            args.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ")
        );
        let encoded = base64::engine::general_purpose::STANDARD.encode(
            script
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        );
        let path = crate::config::windows_shell_path("pwsh")?;
        let process = localshelld_native_process::spawn_with_environment(
            &path.to_string_lossy(),
            &["-NoLogo", "-NonInteractive", "-EncodedCommand", &encoded],
            None,
            environment,
        )?;
        Ok((
            Child::Windows(process.child),
            Box::new(tokio::fs::File::from_std(process.stdin)),
            Box::new(tokio::fs::File::from_std(process.stdout)),
            Box::new(tokio::fs::File::from_std(process.stderr)),
        ))
    }
    #[cfg(unix)]
    {
        let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let script = format!(
            "cd -- {} && exec {}",
            quote(cwd),
            args.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ")
        );
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| Path::new(s).is_absolute())
            .unwrap_or_else(|| "/bin/bash".into());
        let mut command = tokio::process::Command::from(crate::process::background_command(shell));
        command
            .args(["-l", "-i", "-c", &script])
            .current_dir(cwd)
            .envs(environment)
            .process_group(0)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().context("missing stdin")?;
        let stdout = child.stdout.take().context("missing stdout")?;
        let stderr = child.stderr.take().context("missing stderr")?;
        Ok((
            Child::Native(child),
            Box::new(stdin),
            Box::new(stdout),
            Box::new(stderr),
        ))
    }
}

fn read_events(directory: &Path, args: &Value) -> Result<Value> {
    let offset = args["offset"].as_u64().unwrap_or(0);
    let file = match std::fs::File::open(directory.join("events.jsonl")) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"events":[],"next_offset":0}));
        }
        Err(e) => return Err(e.into()),
    };
    let size = file.metadata()?.len();
    ensure!(offset <= size, "event offset exceeds log size");
    let mut reader = std::io::BufReader::new(file);
    reader.seek(SeekFrom::Start(offset))?;
    let mut events = Vec::new();
    let mut line = String::new();
    let mut next = offset;
    for _ in 0..100 {
        let count = reader.read_line(&mut line)?;
        if count == 0 || !line.ends_with('\n') {
            break;
        }
        if next > offset && next - offset + count as u64 > 64 * 1024 {
            break;
        }
        events.push(serde_json::from_str::<Value>(&line)?);
        next += count as u64;
        line.clear();
    }
    Ok(json!({"events":events,"next_offset":next,"eof":next == size}))
}
