//! Daemon-owned process discovery and path-scoped human policy.
//! No registration endpoint and no client-supplied PID/path are accepted.
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    time::Duration,
};

use localshelld_telegram::agents::{Agent, Change, Policy, Process};
#[cfg(not(target_os = "macos"))]
use netstat2::{AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System, UpdateKind};

use crate::{
    bridge,
    consent::{Choice, Decision, Prompt},
    server::{ApiError, AppState, DaemonMode},
};

#[cfg(test)]
pub(crate) mod tests;

#[cfg(any(target_os = "macos", test))]
mod sockets;
#[cfg(target_os = "macos")]
use sockets::socket_owners;

mod catalog;
pub use catalog::PROCESS_NAMES;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Identity {
    pub pid: u32,
    pub started_at: u64,
    pub name: String,
    pub path: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SavedAgent {
    pub name: String,
    pub path: String,
    pub policy: Policy,
    pub revision: u64,
    #[serde(default)]
    pub last_command: String,
}

#[derive(Default)]
pub(crate) struct Registry {
    observed: BTreeMap<String, Agent>,
    worker: Option<tokio::task::AbortHandle>,
}

impl Drop for Registry {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

fn path_id(path: &str) -> String {
    // Preserve the OS-reported full path. Windows also supports case-sensitive
    // directories; folding case could merge two distinct executable paths.
    format!("{:x}", Sha256::digest(path.as_bytes()))
}

fn recognized(name: &str) -> bool {
    let name = name.to_lowercase();
    PROCESS_NAMES.contains(&name.strip_suffix(".exe").unwrap_or(&name))
}

fn snapshot() -> System {
    // Deliberately do not read process environments or command-line arguments.
    // sysinfo otherwise lists every Linux thread as a separate process.
    System::new_with_specifics(
        RefreshKind::nothing().with_processes(
            ProcessRefreshKind::nothing()
                .with_exe(UpdateKind::Always)
                .with_user(UpdateKind::Always)
                .without_tasks(),
        ),
    )
}

fn name(process: &sysinfo::Process) -> String {
    executable_name(process.exe(), process.name())
}

fn executable_name(executable: Option<&std::path::Path>, reported: &std::ffi::OsStr) -> String {
    // Claude's native installer resolves ~/.local/bin/claude to
    // .../claude/versions/<semver>. Do not fall back to arbitrary process titles:
    // that could turn a shared node/python runtime into a remembered agent.
    if reported == "claude"
        && let Some(path) = executable
        && path.is_absolute()
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| semver::Version::parse(n).is_ok())
        && let Some(versions) = path.parent()
        && versions.file_name().is_some_and(|n| n == "versions")
        && versions
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|n| n == "claude")
    {
        return "claude".into();
    }
    executable
        .and_then(|p| p.file_name())
        .unwrap_or(reported)
        .to_string_lossy()
        .into_owned()
}

fn identity(process: &sysinfo::Process) -> Option<Identity> {
    let path = process.exe()?.to_str()?;
    if !process.exe()?.is_absolute() || path.contains('\0') {
        return None;
    }
    Some(Identity {
        pid: process.pid().as_u32(),
        started_at: process.start_time(),
        name: name(process),
        path: path.into(),
    })
}

fn same_user(system: &System, process: &sysinfo::Process) -> bool {
    let current = system.process(Pid::from_u32(std::process::id()));
    current
        .and_then(|p| p.user_id())
        .is_some_and(|uid| process.user_id() == Some(uid))
}

fn discover() -> Vec<Agent> {
    let system = snapshot();
    let mut found = BTreeMap::<String, Agent>::new();
    for process in system.processes().values() {
        let name = name(process);
        if !recognized(&name) || !same_user(&system, process) {
            continue;
        }
        let path = identity(process).map(|i| i.path);
        let id = path
            .as_deref()
            .map(path_id)
            .unwrap_or_else(|| format!("unresolved-{}-{}", process.pid(), process.start_time()));
        let entry = found.entry(id.clone()).or_insert_with(|| Agent {
            id,
            name,
            path,
            processes: vec![],
            policy: Policy::Ask,
            revision: 0,
        });
        entry.processes.push(Process {
            pid: process.pid().as_u32(),
            started_at: process.start_time(),
        });
    }
    found
        .into_values()
        .map(|mut a| {
            a.processes.sort_by_key(|p| p.pid);
            a
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn socket_owners(peer: SocketAddr, local: SocketAddr) -> Option<BTreeSet<u32>> {
    let sockets = netstat2::get_sockets_info(
        if peer.is_ipv4() {
            AddressFamilyFlags::IPV4
        } else {
            AddressFamilyFlags::IPV6
        },
        ProtocolFlags::TCP,
    )
    .ok()?;
    let owners: BTreeSet<_> = sockets
        .into_iter()
        .filter_map(|socket| {
            let ProtocolSocketInfo::Tcp(tcp) = socket.protocol_socket_info else {
                return None;
            };
            (tcp.local_addr == peer.ip()
                && tcp.local_port == peer.port()
                && tcp.remote_addr == local.ip()
                && tcp.remote_port == local.port()
                && tcp.state == netstat2::TcpState::Established)
                .then_some(socket.associated_pids)
        })
        .flatten()
        .collect();
    (!owners.is_empty()).then_some(owners)
}

fn find_caller(peer: SocketAddr, local: SocketAddr) -> Option<Identity> {
    if !peer.ip().is_loopback() || !local.ip().is_loopback() {
        return None;
    }
    let before = socket_owners(peer, local)?;
    // An inherited/shared socket with several owners is not an unambiguous caller.
    if before.len() != 1 {
        return None;
    }
    let system = snapshot();
    let mut pid = Pid::from_u32(*before.first()?);
    let mut visited = BTreeSet::new();
    let mut child_start = u64::MAX;
    let result = loop {
        if visited.len() >= 32 || !visited.insert(pid) {
            return None;
        }
        let process = system.process(pid)?;
        if process.start_time() > child_start {
            return None;
        }
        child_start = process.start_time();
        if recognized(&name(process)) {
            break identity(process)?;
        }
        pid = process.parent()?;
    };
    // Do not trust a cached PID or a TCP tuple that changed while examining ancestry.
    if socket_owners(peer, local)? != before {
        return None;
    }
    let fresh = snapshot();
    let original_owner = Pid::from_u32(*before.first()?);
    if fresh.process(original_owner)?.start_time() != system.process(original_owner)?.start_time()
        || identity(fresh.process(Pid::from_u32(result.pid))?)? != result
    {
        return None;
    }
    Some(result)
}

impl AppState {
    pub(crate) async fn caller_identity(&self, peer: Option<SocketAddr>) -> Option<Identity> {
        let peer = peer?;
        let local = self.config.bind;
        tokio::task::spawn_blocking(move || find_caller(peer, local))
            .await
            .ok()
            .flatten()
    }

    pub(crate) async fn refresh_agents(&self) -> Result<(), ApiError> {
        let found = tokio::task::spawn_blocking(discover)
            .await
            .map_err(|_| ApiError::unavailable("could not scan agent processes"))?;
        let mut registry = self.agents.lock().await;
        for agent in registry.observed.values_mut() {
            agent.processes.clear();
        }
        for agent in found {
            registry.observed.insert(agent.id.clone(), agent);
        }
        // Retain stopped paths during this daemon run, but not unverifiable PID rows.
        registry
            .observed
            .retain(|_, a| a.path.is_some() || !a.processes.is_empty());
        Ok(())
    }

    pub(crate) async fn agent_list(&self) -> Result<Vec<Agent>, ApiError> {
        if let Some(pairing) = self.vault_peer().await {
            return bridge::call(
                &pairing,
                DaemonMode::User,
                bridge::Request::AgentsList,
                Some(Duration::from_secs(15)),
                1024 * 1024,
            )
            .await;
        }
        self.refresh_agents().await?;
        let mut agents = self.agents.lock().await.observed.clone();
        for (id, saved) in &self.settings.lock().await.saved.agent_policies {
            let entry = agents.entry(id.clone()).or_insert_with(|| Agent {
                id: id.clone(),
                name: saved.name.clone(),
                path: Some(saved.path.clone()),
                processes: vec![],
                policy: Policy::Ask,
                revision: 0,
            });
            entry.policy = saved.policy;
            entry.revision = saved.revision;
        }
        let mut result: Vec<_> = agents.into_values().collect();
        result.sort_by(|a, b| a.name.cmp(&b.name).then(a.path.cmp(&b.path)));
        Ok(result)
    }

    pub(crate) async fn set_agent_policy(&self, change: Change) -> Result<(), ApiError> {
        if let Some(pairing) = self.vault_peer().await {
            let _: serde_json::Value = bridge::call(
                &pairing,
                DaemonMode::User,
                bridge::Request::AgentPolicy { change },
                Some(Duration::from_secs(15)),
                65_536,
            )
            .await?;
            return Ok(());
        }
        if change.command_id.is_empty() || change.command_id.len() > 64 {
            return Err(ApiError::bad_request("invalid policy command"));
        }
        if change
            .expires_at_ms
            .is_some_and(|t| t <= chrono::Utc::now().timestamp_millis())
        {
            return Err(ApiError::bad_request("agent policy command expired"));
        }
        self.refresh_agents().await?;
        let observed = self.agents.lock().await.observed.get(&change.id).cloned();
        let mut settings = self.settings.lock().await;
        let saved = settings.saved.agent_policies.get(&change.id);
        if saved.is_some_and(|p| p.last_command == change.command_id) {
            return Ok(());
        }
        if saved.map_or(0, |p| p.revision) != change.revision {
            return Err(ApiError::conflict(
                "agent policy changed; refresh and retry",
            ));
        }
        let (name, path) = match (observed, saved) {
            (Some(a), _) if a.path.is_some() => (a.name, a.path.unwrap()),
            (_, Some(a)) => (a.name.clone(), a.path.clone()),
            _ => {
                return Err(ApiError::bad_request(
                    "agent path has not been discovered by this daemon",
                ));
            }
        };
        let mut next = settings.saved.clone();
        next.agent_policies.insert(
            change.id,
            SavedAgent {
                name,
                path,
                policy: change.policy,
                revision: change.revision.saturating_add(1),
                last_command: change.command_id,
            },
        );
        settings.save(next).await
    }

    pub(crate) async fn authorize_agent(
        &self,
        mut prompt: Prompt,
        agent: Option<Identity>,
    ) -> Result<Decision, ApiError> {
        let agent = if let Some(agent) = agent {
            let expected = agent.clone();
            let valid = tokio::task::spawn_blocking(move || {
                let system = snapshot();
                system
                    .process(Pid::from_u32(expected.pid))
                    .is_some_and(|p| {
                        same_user(&system, p) && identity(p).as_ref() == Some(&expected)
                    })
            })
            .await
            .unwrap_or(false);
            valid.then_some(agent)
        } else {
            None
        };
        prompt.agent_path = agent.as_ref().map(|a| a.path.clone());
        if let Some(agent) = &agent {
            prompt.agent_name = agent.name.clone();
        }
        let mut prompt_revision = 0;
        if let Some(agent) = &agent {
            let id = path_id(&agent.path);
            let mut settings = self.settings.lock().await;
            let policy = settings
                .saved
                .agent_policies
                .get(&id)
                .map_or(Policy::Ask, |a| a.policy);
            prompt_revision = settings
                .saved
                .agent_policies
                .get(&id)
                .map_or(0, |a| a.revision);
            if policy == Policy::Once {
                let mut next = settings.saved.clone();
                let saved = next.agent_policies.get_mut(&id).unwrap();
                saved.policy = Policy::Ask;
                saved.revision = saved.revision.saturating_add(1);
                // Persist consumption before releasing the lock or creating any session.
                settings.save(next).await?;
            }
            match policy {
                Policy::Trust | Policy::Once => {
                    return Ok(Decision {
                        choice: Choice::Accept,
                        remember: false,
                        btw: String::new(),
                    });
                }
                Policy::Deny => {
                    return Ok(Decision {
                        choice: Choice::Deny,
                        remember: false,
                        btw: String::new(),
                    });
                }
                Policy::Ask => {}
            }
        }
        let decision = self.request_consent(prompt).await?;
        // The checkbox means long-term trust, never a name-based remembered denial.
        if decision.choice == Choice::Accept
            && decision.remember
            && let Some(agent) = agent
        {
            let id = path_id(&agent.path);
            let mut settings = self.settings.lock().await;
            let revision = settings
                .saved
                .agent_policies
                .get(&id)
                .map_or(0, |a| a.revision);
            // A later explicit management decision wins over an older open prompt.
            if revision != prompt_revision {
                return Ok(decision);
            }
            let mut next = settings.saved.clone();
            next.agent_policies.insert(
                id,
                SavedAgent {
                    name: agent.name,
                    path: agent.path,
                    policy: Policy::Trust,
                    revision: revision.saturating_add(1),
                    last_command: String::new(),
                },
            );
            settings.save(next).await?;
        }
        Ok(decision)
    }

    pub(crate) async fn start_agent_worker(&self) {
        if self.mode != DaemonMode::User {
            return;
        }
        let mut registry = self.agents.lock().await;
        if registry.worker.is_some() {
            return;
        }
        let state = self.clone();
        let worker = tokio::spawn(async move {
            let mut acknowledgements = vec![];
            loop {
                let _ = state.refresh_agents().await;
                let enabled = {
                    let settings = state.settings.lock().await;
                    settings.saved.sync.is_some()
                        && crate::telegram::read_config(&settings)
                            .ok()
                            .flatten()
                            .is_some_and(|c| c.enabled && c.binding.is_some())
                };
                if enabled && let Ok(agents) = state.agent_list().await {
                    let exchange = localshelld_telegram::agents::Exchange {
                        device: System::host_name().unwrap_or_else(|| "trustd device".into()),
                        agents,
                        acknowledgements: acknowledgements.clone(),
                    };
                    if let Ok(response) = state
                        .telegram_cloud_request(
                            "telegram/agents",
                            serde_json::json!(exchange),
                            Duration::from_secs(15),
                        )
                        .await
                        && let Ok(reply) = serde_json::from_value::<
                            localshelld_telegram::agents::ExchangeReply,
                        >(response)
                    {
                        acknowledgements.clear();
                        for command in reply.commands {
                            let command_id = command.command_id.clone();
                            let result = state.set_agent_policy(command).await;
                            acknowledgements.push(localshelld_telegram::agents::Acknowledgement {
                                command_id,
                                error: result.err().map(|e| e.to_string()),
                            });
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        registry.worker = Some(worker.abort_handle());
    }

    pub(crate) async fn stop_agent_worker(&self) {
        if let Some(worker) = self.agents.lock().await.worker.take() {
            worker.abort();
        }
    }
}
