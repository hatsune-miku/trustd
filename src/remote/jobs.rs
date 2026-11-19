use super::{string, timeout};
use crate::{execution::RunOptions, server::AppState, storage};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    time::Duration,
};

/// Largest chunk a caller may ask for. Base64 grows data by a third and JSON
/// escaping can grow control characters sixfold; both must stay under the
/// relay's 2 MiB payload limit.
pub(super) const MAX_BASE64_CHUNK: usize = 1024 * 1024;
const MAX_TEXT_CHUNK: usize = 256 * 1024;
const DEFAULT_WAIT_SECONDS: u64 = 300;
const MAX_WAIT_SECONDS: u64 = 3600;

fn running(status: &str) -> bool {
    matches!(status, "starting" | "running" | "cancelling")
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Job {
    pub id: String,
    pub owner: String,
    #[serde(default)]
    pub session_ref: String,
    #[serde(default)]
    pub agent_id: Option<String>,
    pub fingerprint: String,
    pub status: String,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    pub command: String,
    pub result: Option<Value>,
    pub error: Option<String>,
}

impl AppState {
    pub(super) async fn remote_job_start(&self, owner: &str, args: &Value) -> Result<Value> {
        let key = string(args, "idempotency_key")?;
        ensure!(key.len() <= 128, "idempotency key is too long");
        let namespace = args["_agent_id"]
            .as_str()
            .map_or_else(|| owner.to_owned(), |agent| format!("{owner}/{agent}"));
        let id = format!("{:x}", Sha256::digest(format!("{namespace}/{key}")));
        let mut fingerprint_args = args.clone();
        if args["_agent_id"].is_string() {
            fingerprint_args
                .as_object_mut()
                .unwrap()
                .remove("session_ref");
        }
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&fingerprint_args)?)
        );
        let _gate = self.remote.submit.lock().await;
        if let Some(job) = self.remote.jobs.lock().await.get(&id).cloned() {
            ensure!(
                job.fingerprint == fingerprint,
                "idempotency key was already used with different arguments"
            );
            return self.job_view(&job);
        }
        let session = self
            .remote
            .session(owner, string(args, "session_ref")?)
            .await?;
        let env = self.session_environment(&session.handle).await?;
        let command = string(args, "command")?.to_owned();
        ensure!(!command.contains('\0'), "command must not contain NUL");
        let level = crate::api::privilege_arg(args)?;
        ensure!(
            !level.is_privileged() || session.options.sudo,
            "elevation was not granted to this session"
        );
        let options = RunOptions {
            stdin: args["stdin"].as_str().unwrap_or_default().into(),
            timeout_seconds: timeout(args)?,
        };
        let directory = self.remote.directory()?.join("jobs").join(&id);
        std::fs::create_dir_all(&directory)?;
        crate::config::set_private_dir(&directory)?;
        for name in ["stdout", "stderr"] {
            storage::write_private(&directory.join(name), &[])?;
        }
        let job = Job {
            id: id.clone(),
            owner: owner.into(),
            session_ref: string(args, "session_ref")?.into(),
            agent_id: args["_agent_id"].as_str().map(str::to_owned),
            fingerprint,
            status: "starting".into(),
            created_at: chrono::Utc::now().timestamp(),
            finished_at: None,
            command: crate::logs::redact(&command, &session.handle, Some(&env)),
            result: None,
            error: None,
        };
        storage::save_json(&directory.join("job.json"), &job)?;
        self.remote
            .jobs
            .lock()
            .await
            .insert(id.clone(), job.clone());
        self.audit("remote.job.accepted", json!({"peer":owner,"job_id":id}))
            .await;
        let view = self.job_view(&job)?;
        let state = self.clone();
        let owner = owner.to_owned();
        tokio::spawn(async move {
            let result: Result<Value> = async {
                if state.remote.jobs.lock().await[&id].status == "cancelled" {
                    anyhow::bail!("cancelled before execution");
                }
                // A device removed from the roster since submission loses its jobs.
                state
                    .remote
                    .peer(&owner)
                    .await
                    .context("device was removed from the roster")?;
                state.remote.permit(&owner, session.options.sudo).await?;
                let entered = state
                    .enter_remote_granted(&owner, &session.keys, session.options)
                    .await?;
                state
                    .remote
                    .job_handles
                    .lock()
                    .await
                    .insert(id.clone(), entered.handle.clone());
                {
                    let mut jobs = state.remote.jobs.lock().await;
                    if jobs[&id].status == "cancelled" {
                        drop(jobs);
                        let _ = state.destroy_session(&entered.handle).await;
                        anyhow::bail!("cancelled before execution");
                    }
                    let job = jobs.get_mut(&id).unwrap();
                    job.status = "running".into();
                    storage::save_json(&directory.join("job.json"), job)?;
                }
                let wrapped = redirect(&command, &directory);
                let result = state
                    .run_command(&entered.handle, &wrapped, level, options)
                    .await;
                let _ = state.destroy_session(&entered.handle).await;
                Ok(result?.view())
            }
            .await;
            if let Some(handle) = state.remote.job_handles.lock().await.remove(&id) {
                let _ = state.destroy_session(&handle).await;
            }
            let mut jobs = state.remote.jobs.lock().await;
            let job = jobs.get_mut(&id).unwrap();
            if job.status != "cancelled" {
                job.status = if result.as_ref().is_ok_and(|r| r["success"] == true) {
                    "succeeded"
                } else {
                    "failed"
                }
                .into();
            }
            match result {
                Ok(result) => job.result = Some(result),
                Err(error) => job.error = Some(error.to_string()),
            }
            job.finished_at = Some(chrono::Utc::now().timestamp());
            if let Err(error) = storage::save_json(&directory.join("job.json"), job) {
                tracing::error!(%error,"could not persist job completion");
            }
            let status = job.status.clone();
            drop(jobs);
            state.remote.job_finished.notify_waiters();
            state
                .audit(
                    "remote.job.finished",
                    json!({"peer":owner,"job_id":id,"status":status}),
                )
                .await;
        });
        Ok(view)
    }

    /// A job as callers see it: the bookkeeping fields stay on disk, and the
    /// output files are named so a shell on the target can process them in
    /// place (for example compress them before reading them back).
    fn job_view(&self, job: &Job) -> Result<Value> {
        let directory = self.remote.directory()?.join("jobs").join(&job.id);
        let mut value = serde_json::to_value(job)?;
        let object = value.as_object_mut().unwrap();
        object.remove("fingerprint");
        object.remove("owner");
        object.retain(|_, field| !field.is_null());
        for stream in ["stdout", "stderr"] {
            let path = directory.join(stream);
            object.insert(format!("{stream}_path"), json!(shell_path(&path)));
            let size = std::fs::metadata(&path).map_or(0, |metadata| metadata.len());
            object.insert(format!("{stream}_size"), json!(size));
        }
        Ok(value)
    }

    /// Block until the job finishes or the wait elapses, whichever is first.
    async fn wait_for_job(&self, id: &str, args: &Value) -> Result<Value> {
        let seconds = timeout(args)?
            .unwrap_or(DEFAULT_WAIT_SECONDS)
            .min(MAX_WAIT_SECONDS);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        loop {
            // Register before reading the status so a finish in between still wakes us.
            let finished = self.remote.job_finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            let job = self
                .remote
                .jobs
                .lock()
                .await
                .get(id)
                .cloned()
                .context("job not found")?;
            if !running(&job.status) || tokio::time::timeout_at(deadline, finished).await.is_err() {
                return self.job_view(&job);
            }
        }
    }

    pub(super) async fn remote_job_action(
        &self,
        owner: &str,
        operation: &str,
        args: &Value,
    ) -> Result<Value> {
        if operation == "job_list" {
            let jobs = self.remote.jobs.lock().await;
            let mut values: Vec<_> = jobs
                .values()
                .filter(|j| {
                    j.owner == owner
                        && if let Some(agent) = args["_agent_id"].as_str() {
                            j.agent_id.as_deref() == Some(agent)
                        } else {
                            args["session_ref"]
                                .as_str()
                                .is_none_or(|id| j.session_ref == id)
                        }
                })
                .cloned()
                .collect();
            values.sort_by_key(|j| std::cmp::Reverse((j.created_at, j.id.clone())));
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            let limit = args["limit"].as_u64().unwrap_or(30).clamp(1, 100) as usize;
            let total = values.len();
            let page = values
                .iter()
                .skip(offset)
                .take(limit)
                .map(|job| self.job_view(job))
                .collect::<Result<Vec<_>>>()?;
            return Ok(
                json!({"jobs":page,"total":total,"next_offset":(offset+limit < total).then_some(offset+limit)}),
            );
        }
        let id = string(args, "job_id")?;
        let job = self
            .remote
            .jobs
            .lock()
            .await
            .get(id)
            .cloned()
            .context("job not found")?;
        ensure!(job.owner == owner, "job belongs to another device");
        ensure!(
            if let Some(agent) = args["_agent_id"].as_str() {
                job.agent_id.as_deref() == Some(agent)
            } else {
                args["session_ref"]
                    .as_str()
                    .is_none_or(|id| job.session_ref == id)
            },
            "job is outside this agent's delegated session"
        );
        if operation == "job_cancel" {
            self.cancel_remote_job(id).await?;
        }
        if operation == "job_wait" {
            return self.wait_for_job(id, args).await;
        }
        if operation == "job_output" {
            let stream = args["stream"].as_str().unwrap_or("stdout");
            ensure!(
                matches!(stream, "stdout" | "stderr"),
                "stream must be stdout or stderr"
            );
            return read_chunk(
                self.remote.directory()?.join("jobs").join(id).join(stream),
                args,
            );
        }
        let job = self.remote.jobs.lock().await.get(id).cloned();
        job.map_or(Ok(Value::Null), |job| self.job_view(&job))
    }

    async fn cancel_remote_job(&self, id: &str) -> Result<()> {
        let mut jobs = self.remote.jobs.lock().await;
        let job = jobs.get_mut(id).context("job not found")?;
        if !running(&job.status) {
            return Ok(());
        }
        job.status = "cancelled".into();
        job.finished_at = Some(chrono::Utc::now().timestamp());
        storage::save_json(
            &self
                .remote
                .directory()?
                .join("jobs")
                .join(id)
                .join("job.json"),
            job,
        )?;
        drop(jobs);
        self.remote.job_finished.notify_waiters();
        let handle = self.remote.job_handles.lock().await.get(id).cloned();
        if let Some(handle) = handle {
            self.destroy_session(&handle).await?;
        }
        Ok(())
    }

    pub(super) async fn revoke_remote_peer(&self, owner: &str) {
        let ids: Vec<_> = self
            .remote
            .jobs
            .lock()
            .await
            .values()
            .filter(|j| j.owner == owner)
            .map(|j| j.id.clone())
            .collect();
        for id in ids {
            let _ = self.cancel_remote_job(&id).await;
        }
        let mut sessions = self.remote.sessions.lock().await;
        let mut handles = Vec::new();
        sessions.retain(|_, s| {
            if s.owner == owner {
                handles.push(s.handle.clone());
                false
            } else {
                true
            }
        });
        drop(sessions);
        for handle in handles {
            let _ = self.destroy_session(&handle).await;
        }
        let agents: BTreeMap<_, _> = self
            .remote
            .agents
            .lock()
            .await
            .iter()
            .filter(|(_, a)| a.owner == owner)
            .map(|(id, a)| (id.clone(), a.clone()))
            .collect();
        for (_, agent) in agents {
            agent.stop().await;
        }
    }
}

/// A path as a shell on this host should be given it. Installed Windows
/// paths are canonicalized with the verbatim `\\?\` prefix, which Windows
/// PowerShell's file cmdlets reject; a drive path works everywhere.
pub(super) fn shell_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_owned(),
        _ => text.into_owned(),
    }
}

/// Read part of a file as UTF-8 `text`, or as base64 `data` when the caller
/// asks for it or the bytes are not text. Never both: callers pay for every
/// byte. `tail` reads the end of the file when no offset is given.
pub(super) fn read_chunk(path: PathBuf, args: &Value) -> Result<Value> {
    let base64 = match args["encoding"].as_str() {
        None | Some("text") => false,
        Some("base64") => true,
        Some(_) => bail!("encoding must be text or base64"),
    };
    let maximum = if base64 {
        MAX_BASE64_CHUNK
    } else {
        MAX_TEXT_CHUNK
    };
    let limit = args["limit"]
        .as_u64()
        .unwrap_or(16 * 1024)
        .clamp(1, maximum as u64) as usize;
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let tail = args["offset"].is_null() && args["tail"].is_u64();
    let mut offset = match args["offset"].as_u64() {
        Some(offset) => offset,
        None => size.saturating_sub(args["tail"].as_u64().unwrap_or(size)),
    };
    ensure!(offset <= size, "offset exceeds file size");
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::with_capacity(limit);
    file.take(limit as u64).read_to_end(&mut bytes)?;
    let mut value = json!({});
    let mut text = None;
    if !base64 {
        if tail {
            // The tail may begin inside a character; start at the next one.
            let skip = bytes
                .iter()
                .take(3)
                .take_while(|byte| *byte & 0xc0 == 0x80)
                .count();
            bytes.drain(..skip);
            offset += skip as u64;
        }
        let more = offset + (bytes.len() as u64) < size;
        let end = match std::str::from_utf8(&bytes) {
            Ok(_) => Some(bytes.len()),
            // A chunk may end inside a character; the next read resumes there.
            Err(error) if error.error_len().is_none() && more && error.valid_up_to() > 0 => {
                Some(error.valid_up_to())
            }
            Err(_) => None,
        };
        if let Some(end) = end.filter(|end| *end > 0 || bytes.is_empty()) {
            bytes.truncate(end);
            text = Some(String::from_utf8(std::mem::take(&mut bytes)).unwrap());
        }
    }
    let count = text.as_ref().map_or(bytes.len(), String::len) as u64;
    match text {
        Some(text) => value["text"] = json!(text),
        None => {
            value["data"] = json!(STANDARD.encode(&bytes));
            value["encoding"] = json!("base64");
        }
    }
    value["offset"] = json!(offset);
    value["next_offset"] = json!(offset + count);
    value["size"] = json!(size);
    value["eof"] = json!(offset + count == size);
    Ok(value)
}

fn redirect(command: &str, directory: &std::path::Path) -> String {
    let out = directory
        .join("stdout")
        .to_string_lossy()
        .replace('\'', "''");
    let err = directory
        .join("stderr")
        .to_string_lossy()
        .replace('\'', "''");
    if cfg!(windows) {
        // PowerShell redirection expands wildcards even in quoted paths. Keep
        // verbatim Windows prefixes (\\?\) and brackets in directory names literal.
        format!(
            "& {{\n{command}\n}} 1>> ([System.Management.Automation.WildcardPattern]::Escape('{out}')) 2>> ([System.Management.Automation.WildcardPattern]::Escape('{err}'))"
        )
    } else {
        let quote =
            |path: &std::path::Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
        format!(
            "(\n{command}\n) > {} 2> {}",
            quote(&directory.join("stdout")),
            quote(&directory.join("stderr"))
        )
    }
}
