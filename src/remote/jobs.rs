use super::{string, timeout};
use crate::{execution::RunOptions, server::AppState, storage};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

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
            return Ok(serde_json::to_value(job)?);
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
                Ok(serde_json::to_value(result?)?)
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
            state
                .audit(
                    "remote.job.finished",
                    json!({"peer":owner,"job_id":id,"status":status}),
                )
                .await;
        });
        Ok(serde_json::to_value(job)?)
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
            let page: Vec<_> = values.into_iter().skip(offset).take(limit).collect();
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
        Ok(serde_json::to_value(self.remote.jobs.lock().await.get(id))?)
    }

    async fn cancel_remote_job(&self, id: &str) -> Result<()> {
        let mut jobs = self.remote.jobs.lock().await;
        let job = jobs.get_mut(id).context("job not found")?;
        if !matches!(job.status.as_str(), "starting" | "running" | "cancelling") {
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

pub(super) fn read_chunk(path: PathBuf, args: &Value) -> Result<Value> {
    let offset = args["offset"].as_u64().unwrap_or(0);
    let limit = args["limit"]
        .as_u64()
        .unwrap_or(16 * 1024)
        .clamp(1, 48 * 1024) as usize;
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    ensure!(offset <= size, "offset exceeds file size");
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; limit];
    let count = file.read(&mut bytes)?;
    bytes.truncate(count);
    Ok(
        json!({"offset":offset,"next_offset":offset+count as u64,"size":size,"data":STANDARD.encode(&bytes),"text":String::from_utf8_lossy(&bytes),"eof":offset+count as u64 == size}),
    )
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
