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
use localshelld_sync_protocol::owner::{self, Owner, Roster, SignedStatement};
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
mod codex;
mod files;
mod jobs;
#[cfg(test)]
mod join_tests;
#[cfg(test)]
mod tests;

/// Another device of the same owner, from the verified roster. Devices of one
/// owner trust each other by default: execution, elevation and credentials.
#[derive(Clone, Serialize)]
pub struct Peer {
    pub label: String,
    pub public_key: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Saved {
    identity: Identity,
    /// The owner pinned when this device joined; every later roster update is
    /// verified against it, never taken from the backend's word.
    #[serde(default)]
    owner: Option<Owner>,
    /// Owner-signed statements; the roster is derived from them locally.
    #[serde(default)]
    statements: Vec<SignedStatement>,
}

impl Saved {
    fn roster(&self) -> Option<Roster> {
        self.owner
            .as_ref()
            .map(|owner| owner::roster(owner, &self.statements))
    }

    /// Roster devices other than this one, keyed by device ID.
    fn peers(&self) -> BTreeMap<String, Peer> {
        let own = self.identity.id().unwrap_or_default();
        self.roster()
            .map(|roster| {
                roster
                    .devices
                    .into_iter()
                    .filter(|(id, _)| *id != own)
                    .map(|(id, device)| {
                        (
                            id,
                            Peer {
                                label: device.name,
                                public_key: device.public_key,
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
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
                owner: None,
                statements: vec![],
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
            json!({"protocol":3,"device_id":saved.identity.id()?,"public_key":saved.identity.public_key,
            "joined":saved.owner.is_some(),"backend":saved.owner.as_ref().map(|o| &o.origin),
            "connected":self.node.read().await.as_ref().is_some_and(|n| n.connected()),"peers":saved.peers()}),
        )
    }

    async fn peer(&self, id: &str) -> Result<Peer> {
        self.saved
            .lock()
            .await
            .as_ref()
            .and_then(|s| s.peers().remove(id))
            .context("device is not in the owner's roster")
    }

    /// This device's ID once it has joined a backend.
    pub(crate) async fn joined_device_id(&self) -> Option<String> {
        let saved = self.saved.lock().await;
        let saved = saved.as_ref()?;
        saved.owner.as_ref()?;
        saved.identity.id().ok()
    }

    /// Verify an owner-signed statement against the owner pinned at join and
    /// the keys trusted by the verified roster.
    pub(crate) async fn verify_owner_statement(
        &self,
        signed: &SignedStatement,
    ) -> Result<owner::Statement> {
        let saved = self.saved.lock().await;
        let saved = saved
            .as_ref()
            .context("this device has not joined a backend")?;
        let pinned = saved
            .owner
            .as_ref()
            .context("this device has not joined a backend")?;
        let keys = owner::roster(pinned, &saved.statements).keys;
        Ok(signed.verify(pinned, &keys)?)
    }

    /// Whether this device joined a backend and pinned its owner.
    pub(crate) async fn joined(&self) -> bool {
        self.saved
            .lock()
            .await
            .as_ref()
            .is_some_and(|saved| saved.owner.is_some())
    }

    /// This device's Noise identity, created on first use.
    pub(crate) async fn identity(&self) -> Result<Identity> {
        self.initialize().await?;
        Ok(self.saved.lock().await.as_ref().unwrap().identity.clone())
    }

    /// Pin the owner and adopt the statements of a verified join.
    pub(crate) async fn join(&self, owner: Owner, statements: Vec<SignedStatement>) -> Result<()> {
        self.initialize().await?;
        let mut guard = self.saved.lock().await;
        let mut next = guard.as_ref().unwrap().clone();
        next.owner = Some(owner);
        next.statements = statements;
        self.persist(&next)?;
        *guard = Some(next);
        Ok(())
    }

    /// Leave the owner's devices; the identity is kept for a later join.
    pub(crate) async fn leave(&self) -> Result<()> {
        let mut guard = self.saved.lock().await;
        if let Some(mut next) = guard.clone() {
            next.owner = None;
            next.statements.clear();
            self.persist(&next)?;
            *guard = Some(next);
        }
        Ok(())
    }

    fn persist(&self, saved: &Saved) -> Result<()> {
        storage::save_json(
            self.path
                .as_ref()
                .context("remote storage is unavailable")?,
            saved,
        )
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
        // The relay is part of the joined backend; its sync session admits us.
        let backend = self
            .settings
            .lock()
            .await
            .saved
            .sync
            .as_ref()
            .map(|s| s.url.clone());
        let Some(url) = backend.filter(|_| saved.owner.is_some()) else {
            return Ok(());
        };
        let peers = saved
            .peers()
            .into_iter()
            .map(|(id, p)| (id, p.public_key))
            .collect();
        let node = Node::new(saved.identity, peers);
        *self.remote.node.write().await = Some(node.clone());
        let state = self.clone();
        let handler: trustd_remote::Handler = Arc::new(move |peer, request| {
            let state = state.clone();
            Box::pin(async move { state.remote_dispatch(&peer, request).await })
        });
        let state = self.clone();
        let token: trustd_remote::TokenSource = Arc::new(move || {
            let state = state.clone();
            Box::pin(async move {
                state
                    .sync_access_token()
                    .await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            })
        });
        let worker = tokio::spawn(node.run(url, token, handler));
        *self.remote.worker.lock().await = Some(worker.abort_handle());
        Ok(())
    }

    /// Adopt a roster update after verifying it against the pinned owner.
    /// Devices that left the roster lose their sessions, jobs and agents.
    pub(crate) async fn apply_roster(
        &self,
        response: localshelld_sync_protocol::RosterResponse,
    ) -> Result<()> {
        let mut guard = self.remote.saved.lock().await;
        let Some(current) = guard.clone() else {
            return Ok(());
        };
        let Some(pinned) = current.owner.as_ref() else {
            return Ok(());
        };
        ensure!(
            response.owner == *pinned,
            "the backend presented a different owner; roster update ignored"
        );
        let unchanged = response.statements.len() == current.statements.len()
            && response
                .statements
                .iter()
                .zip(&current.statements)
                .all(|(a, b)| a.statement == b.statement);
        if unchanged {
            return Ok(());
        }
        let before = current.peers();
        let mut next = current;
        next.statements = response.statements;
        self.remote.persist(&next)?;
        let after = next.peers();
        *guard = Some(next);
        drop(guard);
        if let Some(node) = self.remote.node.read().await.as_ref() {
            node.set_peers(
                after
                    .iter()
                    .map(|(id, p)| (id.clone(), p.public_key.clone()))
                    .collect(),
            )
            .await;
        }
        for id in before.keys().filter(|id| !after.contains_key(*id)) {
            self.revoke_remote_peer(id).await;
            self.audit("remote.peer.removed", json!({"peer":id})).await;
        }
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
        // Devices of the same owner trust each other by default.
        self.remote.peer(owner).await?;
        let keys = crate::api::secret_keys_arg(args)?;
        let options = crate::api::enter_options_arg(args)?;
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
        self.remote.peer(owner).await?;
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
            "secrets_list" => Ok(json!({"keys":self.secret_keys().await?})),
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
        .route("/v2/admin/remote", get(status))
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
