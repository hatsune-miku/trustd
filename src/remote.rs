//! Remote execution is owned by the destination daemon, never by a relay socket.
use crate::{
    execution::{EnterOptions, RunOptions},
    server::{ApiError, AppState, DaemonMode, EnterResult},
    storage,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::State,
    http::HeaderMap,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, RwLock};
use trustd_remote::{Identity, Node};

pub mod agent_stdio;
mod setup;
pub use setup::{SetupCommand, configure_local};
mod codex;
mod files;
mod jobs;
#[cfg(test)]
mod tests;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub label: String,
    pub public_key: String,
    #[serde(default)]
    pub allow_execution: bool,
    #[serde(default)]
    pub sudo: bool,
    #[serde(default)]
    pub secret_keys: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Saved {
    identity: Identity,
    #[serde(default)]
    relay_url: String,
    #[serde(default)]
    relay_token: String,
    #[serde(default)]
    peers: BTreeMap<String, Peer>,
}

#[derive(Clone)]
struct Session {
    owner: String,
    handle: String,
    keys: Vec<String>,
    options: EnterOptions,
}

#[derive(Default)]
pub struct Runtime {
    path: Option<PathBuf>,
    saved: Mutex<Option<Saved>>,
    node: RwLock<Option<Arc<Node>>>,
    worker: Mutex<Option<tokio::task::AbortHandle>>,
    sessions: Mutex<BTreeMap<String, Session>>,
    jobs: Mutex<BTreeMap<String, jobs::Job>>,
    job_handles: Mutex<BTreeMap<String, String>>,
    submit: Mutex<()>,
    agent_start: Mutex<()>,
    agents: Mutex<BTreeMap<String, Arc<codex::Agent>>>,
}

impl Runtime {
    pub fn load(config: &Path) -> Result<Self> {
        let path = config.with_extension("remote.json");
        let saved: Option<Saved> = if path.exists() {
            Some(serde_json::from_slice(&std::fs::read(&path)?)?)
        } else {
            None
        };
        let mut jobs = BTreeMap::new();
        let directory = config.with_extension("remote").join("jobs");
        if directory.exists() {
            for entry in std::fs::read_dir(&directory)? {
                let entry = entry?;
                let metadata = entry.path().join("job.json");
                if metadata.is_file() {
                    let mut job: jobs::Job = serde_json::from_slice(&std::fs::read(&metadata)?)?;
                    if matches!(job.status.as_str(), "starting" | "running" | "cancelling") {
                        job.status = "interrupted".into();
                        job.error = Some("daemon restarted; execution was not replayed".into());
                        storage::save_json(&metadata, &job)?;
                    }
                    jobs.insert(job.id.clone(), job);
                }
            }
        }
        Ok(Self {
            path: Some(path),
            saved: Mutex::new(saved),
            jobs: Mutex::new(jobs),
            ..Self::default()
        })
    }

    fn directory(&self) -> Result<PathBuf> {
        Ok(self
            .path
            .as_ref()
            .context("remote storage is unavailable")?
            .with_extension(""))
    }

    async fn initialize(&self) -> Result<()> {
        let mut saved = self.saved.lock().await;
        if saved.is_none() {
            let new = Saved {
                identity: Identity::generate()?,
                relay_url: String::new(),
                relay_token: String::new(),
                peers: BTreeMap::new(),
            };
            storage::save_json(
                self.path
                    .as_ref()
                    .context("remote storage is unavailable")?,
                &new,
            )?;
            *saved = Some(new);
        }
        Ok(())
    }

    async fn status(&self) -> Result<Value> {
        self.initialize().await?;
        let saved = self.saved.lock().await;
        let saved = saved.as_ref().unwrap();
        Ok(
            json!({"protocol":2,"device_id":saved.identity.id()?,"public_key":saved.identity.public_key,
            "relay_url":saved.relay_url,"token_configured":!saved.relay_token.is_empty(),
            "connected":self.node.read().await.as_ref().is_some_and(|n| n.connected()),"peers":saved.peers}),
        )
    }

    async fn peer(&self, id: &str) -> Result<Peer> {
        self.saved
            .lock()
            .await
            .as_ref()
            .and_then(|s| s.peers.get(id))
            .cloned()
            .context("device is not paired")
    }

    async fn session(&self, owner: &str, id: &str) -> Result<Session> {
        let session = self
            .sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .context("session not found; open a new remote session")?;
        ensure!(session.owner == owner, "session belongs to another device");
        Ok(session)
    }
}

impl AppState {
    pub async fn start_remote_worker(&self) -> Result<()> {
        if self.mode != DaemonMode::User {
            return Ok(());
        }
        if let Some(worker) = self.remote.worker.lock().await.take() {
            worker.abort();
        }
        if let Some(node) = self.remote.node.write().await.take() {
            node.disconnect().await;
        }
        let Some(saved) = self.remote.saved.lock().await.clone() else {
            return Ok(());
        };
        if saved.relay_url.is_empty() || saved.relay_token.is_empty() {
            return Ok(());
        }
        let peers = saved
            .peers
            .iter()
            .map(|(id, p)| (id.clone(), p.public_key.clone()))
            .collect();
        let node = Node::new(saved.identity, peers);
        *self.remote.node.write().await = Some(node.clone());
        let state = self.clone();
        let handler: trustd_remote::Handler = Arc::new(move |peer, request| {
            let state = state.clone();
            Box::pin(async move { state.remote_dispatch(&peer, request).await })
        });
        let worker = tokio::spawn(node.run(saved.relay_url, saved.relay_token, handler));
        *self.remote.worker.lock().await = Some(worker.abort_handle());
        Ok(())
    }

    /// Only a locally authenticated owner or an authenticated paired user daemon
    /// reaches this method after checking the destination's saved device grant.
    pub(crate) async fn enter_remote_granted(
        &self,
        peer: &str,
        keys: &[String],
        options: EnterOptions,
    ) -> Result<EnterResult, ApiError> {
        if self.mode == DaemonMode::User && options.sudo {
            let value: Value = crate::bridge::call(
                &self.pairing().await?,
                DaemonMode::Privileged,
                crate::bridge::Request::RemoteEnter {
                    peer_id: peer.into(),
                    secret_keys: keys.to_vec(),
                    options,
                },
                Some(std::time::Duration::from_secs(60)),
                65536,
            )
            .await?;
            let handle = value["handle"]
                .as_str()
                .ok_or_else(|| ApiError::internal("missing remote session handle"))?;
            let entered = self.attach_remote(handle).await?;
            self.set_remote_session_peer(&entered.handle, peer).await;
            return Ok(entered);
        }
        let token = self.auth.lock().await.issue_token(None)?.0;
        let result = self.enter_with_options(&token, keys, options).await;
        if let Ok(entered) = &result {
            self.set_remote_session_peer(&entered.handle, peer).await;
        }
        if result.is_err() {
            let _ = self.revoke_token(&token).await;
        }
        self.audit(
            "remote.authorization",
            json!({"peer":peer,"secret_keys":keys,"success":result.is_ok()}),
        )
        .await;
        result
    }

    async fn remote_open(&self, owner: &str, args: &Value) -> Result<Value> {
        let peer = self.remote.peer(owner).await?;
        ensure!(
            peer.allow_execution,
            "this device has not been granted execution access"
        );
        let keys = crate::api::secret_keys_arg(args)?;
        let options = crate::api::enter_options_arg(args)?;
        ensure!(
            !options.sudo || peer.sudo,
            "elevation is not granted to this device"
        );
        ensure!(
            keys.iter().all(|key| peer.secret_keys.contains(key)),
            "requested credentials are outside this device's grant"
        );
        let entered = self
            .enter_remote_granted(owner, &keys, options.clone())
            .await?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut result = serde_json::to_value(&entered)?;
        result.as_object_mut().unwrap().remove("handle");
        result.as_object_mut().unwrap().remove("message");
        result["session_ref"] = json!(id);
        self.remote.sessions.lock().await.insert(
            id,
            Session {
                owner: owner.into(),
                handle: entered.handle,
                keys,
                options,
            },
        );
        Ok(result)
    }

    async fn remote_dispatch(&self, owner: &str, request: Value) -> Result<Value> {
        let peer = self.remote.peer(owner).await?;
        ensure!(
            peer.allow_execution,
            "remote execution is not granted to this device"
        );
        let operation = string(&request, "operation")?;
        let args = request
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        ensure!(args.is_object(), "arguments must be an object");
        let result = match operation {
            "logs_page" => {
                let kind = match args["kind"].as_str() {
                    Some("command") => crate::logs::Kind::Command,
                    Some("audit") => crate::logs::Kind::Audit,
                    _ => anyhow::bail!("kind must be command or audit"),
                };
                let mut page = self
                    .logs
                    .page(
                        kind,
                        args["before"].as_u64(),
                        args["limit"].as_u64().unwrap_or(30).clamp(1, 100) as usize,
                    )
                    .await?;
                page["entries"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|entry| entry["peer"].as_str() == Some(owner));
                Ok(page)
            }
            "host_info" => Ok(
                json!({"protocol":2,"version":crate::VERSION,"platform":std::env::consts::OS,"arch":std::env::consts::ARCH,
                "shell":crate::shell::ShellKind::native(),"identities":self.execution_identities().await,
                "operations":agent_stdio::operation_schemas()}),
            ),
            "secrets_list" => {
                let query = crate::api::secret_query_arg(&args)?;
                Ok(
                    json!({"keys":peer.secret_keys.into_iter().filter(|key| key.to_ascii_lowercase().contains(&query)).collect::<Vec<_>>()}),
                )
            }
            "session_open" => self.remote_open(owner, &args).await,
            "session_close" => {
                let id = string(&args, "session_ref")?;
                let session = self.remote.session(owner, id).await?;
                let agents: Vec<_> = self
                    .remote
                    .agents
                    .lock()
                    .await
                    .values()
                    .filter(|agent| agent.parent_session_ref == id)
                    .cloned()
                    .collect();
                for agent in agents {
                    agent.stop().await;
                }
                self.destroy_session(&session.handle).await?;
                self.remote.sessions.lock().await.remove(id);
                Ok(json!({"closed":true}))
            }
            "run" => {
                let session = self
                    .remote
                    .session(owner, string(&args, "session_ref")?)
                    .await?;
                let level = crate::api::privilege_arg(&args)?;
                let result = self
                    .run_command(
                        &session.handle,
                        string(&args, "command")?,
                        level,
                        RunOptions {
                            stdin: args["stdin"].as_str().unwrap_or_default().into(),
                            timeout_seconds: timeout(&args)?,
                        },
                    )
                    .await?;
                Ok(serde_json::to_value(result)?)
            }
            "job_start" => self.remote_job_start(owner, &args).await,
            "job_get" | "job_list" | "job_cancel" | "job_output" => {
                self.remote_job_action(owner, operation, &args).await
            }
            "file_stat" | "file_read" | "file_write" => {
                self.remote_file(owner, operation, &args).await
            }
            "agent_list" | "agent_start" | "agent_resume" | "agent_send" | "agent_get"
            | "agent_events" | "agent_interrupt" | "agent_respond" | "agent_close" => {
                self.remote_codex(owner, operation, &args).await
            }
            _ => anyhow::bail!("unknown remote operation"),
        };
        self.audit("remote.request", json!({"peer":owner,"operation":operation,"success":result.is_ok(),"job_id":args.get("job_id"),"agent_id":args.get("agent_id")})).await;
        result
    }

    pub(crate) async fn remote_invoke(&self, args: &Value) -> Result<Value> {
        ensure!(
            args["operation"] != "agent_respond",
            "answer Codex requests in the management UI"
        );
        self.remote_invoke_human(args).await
    }

    async fn remote_invoke_human(&self, args: &Value) -> Result<Value> {
        let target = string(args, "target")?;
        let node = self
            .remote
            .node
            .read()
            .await
            .clone()
            .context("remote connection is not configured")?;
        node.call(target, json!({"operation":string(args,"operation")?,"arguments":args.get("arguments").cloned().unwrap_or_else(|| json!({}))})).await
    }

    pub(crate) async fn remote_hosts(&self) -> Result<Value> {
        let value = self.remote.status().await?;
        Ok(
            json!({"connected":value["connected"],"hosts":value["peers"].as_object().map(|p| p.iter().map(|(id,v)| json!({"id":id,"label":v["label"]})).collect::<Vec<_>>()).unwrap_or_default()}),
        )
    }

    pub(crate) async fn stop_remote_worker(&self) {
        if let Some(worker) = self.remote.worker.lock().await.take() {
            worker.abort();
        }
        if let Some(node) = self.remote.node.write().await.take() {
            node.disconnect().await;
        }
        let agents: Vec<_> = self.remote.agents.lock().await.values().cloned().collect();
        for agent in agents {
            agent.stop().await;
        }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/admin/remote", get(status).post(configure))
        .route("/v2/admin/remote/peer", post(pair))
        .route("/v2/admin/remote/invoke", post(admin_invoke))
        .route("/v2/agent/invoke", post(agent_invoke))
}

fn api(error: anyhow::Error) -> ApiError {
    ApiError::bad_request(error.to_string())
}
pub(super) fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .with_context(|| format!("missing {key}"))
}
pub(super) fn timeout(args: &Value) -> Result<Option<u64>> {
    match args.get("timeout_seconds") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => Ok(Some(
            value
                .as_u64()
                .filter(|n| *n > 0)
                .context("timeout_seconds must be positive")?,
        )),
    }
}

async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    crate::management::authorize(&state, &headers, None).await?;
    state.remote.status().await.map(Json).map_err(api)
}

async fn configure(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    crate::management::authorize(&state, &headers, None).await?;
    let result: Result<Value> = async {
        ensure!(
            state.mode == DaemonMode::User,
            "configure remote access on the user daemon"
        );
        state.remote.initialize().await?;
        let mut guard = state.remote.saved.lock().await;
        let mut next = guard.as_ref().unwrap().clone();
        next.relay_url = string(&args, "relay_url")?.trim_end_matches('/').into();
        let url = reqwest::Url::parse(&next.relay_url)?;
        ensure!(
            matches!(url.scheme(), "https" | "wss")
                || url.host_str().is_some_and(|h| h == "localhost"
                    || h.parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())),
            "relay URL must use HTTPS or WSS"
        );
        if let Some(token) = args
            .get("relay_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            next.relay_token = token.into();
        }
        storage::save_json(state.remote.path.as_ref().unwrap(), &next)?;
        *guard = Some(next);
        drop(guard);
        state.start_remote_worker().await?;
        state.remote.status().await
    }
    .await;
    result.map(Json).map_err(api)
}

async fn pair(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    crate::management::authorize(&state, &headers, None).await?;
    let result: Result<Value> = async {
        state.remote.initialize().await?;
        let mut guard = state.remote.saved.lock().await;
        let mut next = guard.as_ref().unwrap().clone();
        let id = if args["remove"].as_bool() == Some(true) {
            let id = string(&args, "device_id")?.to_owned();
            next.peers.remove(&id);
            id
        } else {
            let mut peer: Peer = serde_json::from_value(args)?;
            peer.secret_keys = crate::secrets::normalize_keys(&peer.secret_keys)?;
            ensure!(
                !peer.label.trim().is_empty() && peer.label.len() <= 128,
                "invalid device label"
            );
            let id = trustd_remote::node_id(&peer.public_key)?;
            ensure!(
                id != next.identity.id()?,
                "cannot pair this device with itself"
            );
            next.peers.insert(id.clone(), peer);
            id
        };
        storage::save_json(state.remote.path.as_ref().unwrap(), &next)?;
        if let Some(node) = state.remote.node.read().await.as_ref() {
            node.set_peers(
                next.peers
                    .iter()
                    .map(|(id, p)| (id.clone(), p.public_key.clone()))
                    .collect(),
            )
            .await;
        }
        let previous = guard.as_ref().and_then(|s| s.peers.get(&id));
        let revoked = next.peers.get(&id).is_none_or(|p| !p.allow_execution)
            || previous.is_some_and(|old| {
                next.peers.get(&id).is_some_and(|new| {
                    (old.sudo && !new.sudo)
                        || old
                            .secret_keys
                            .iter()
                            .any(|key| !new.secret_keys.contains(key))
                })
            });
        *guard = Some(next);
        drop(guard);
        if revoked {
            state.revoke_remote_peer(&id).await;
        }
        state
            .audit("remote.peer.updated", json!({"peer":id,"revoked":revoked}))
            .await;
        state.remote.status().await
    }
    .await;
    result.map(Json).map_err(api)
}

async fn admin_invoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    crate::management::authorize(&state, &headers, None).await?;
    state
        .remote_invoke_human(&args)
        .await
        .map(Json)
        .map_err(api)
}
async fn agent_invoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut args): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    if !args["arguments"].is_object() {
        return Err(ApiError::bad_request("arguments must be an object"));
    }
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or_default();
    let agent = state
        .remote
        .agents
        .lock()
        .await
        .values()
        .find(|a| a.capability == token && a.is_running())
        .cloned()
        .ok_or_else(|| ApiError::forbidden("invalid agent capability"))?;
    if !agent_stdio::allowed(args["operation"].as_str().unwrap_or_default()) {
        return Err(ApiError::forbidden(
            "operation is not delegated to this agent",
        ));
    }
    args["arguments"]["session_ref"] = json!(agent.session_ref);
    args["arguments"]["_agent_id"] = json!(agent.id);
    state
        .remote_dispatch(&agent.owner, args)
        .await
        .map(Json)
        .map_err(api)
}
