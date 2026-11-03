//! The bot only queues owner decisions. The authenticated device commits and
//! acknowledges them; cloud snapshots never grant execution by themselves.
use super::*;
use crate::agents::{Agent, Change, Exchange, ExchangeReply, Policy};

const ONLINE_MS: i64 = 30_000;
const COMMAND_MS: i64 = 60_000;
const PANEL_MS: i64 = 300_000;
const PAGE_SIZE: usize = 5;

#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct Catalog {
    devices: HashMap<String, Device>,
    panels: HashMap<String, Panel>,
}
struct Device {
    name: String,
    updated: i64,
    agents: Vec<Agent>,
}
struct Panel {
    owner: String,
    agent: String,
    revision: u64,
    message: Option<i64>,
    text: String,
    expires: i64,
    command: Option<(Change, i64)>,
    done: bool,
}

impl Service {
    pub async fn exchange_agents(&self, owner: &str, exchange: Exchange) -> Result<ExchangeReply> {
        if exchange.device.is_empty()
            || exchange.device.len() > 256
            || exchange.device.chars().any(char::is_control)
            || exchange.agents.len() > 256
            || exchange.acknowledgements.len() > 32
        {
            return Err("invalid agent catalogue");
        }
        let mut ids = std::collections::HashSet::new();
        for agent in &exchange.agents {
            if agent.id.len() > 96
                || agent.id.is_empty()
                || !agent
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
                || !ids.insert(&agent.id)
                || agent.name.is_empty()
                || agent.name.len() > 512
                || agent
                    .path
                    .as_ref()
                    .is_some_and(|p| p.len() > 32768 || p.contains('\0'))
                || agent.processes.len() > 1024
            {
                return Err("invalid agent catalogue entry");
            }
        }
        let mut state = self.inner.state.lock().await;
        let config = state
            .config
            .clone()
            .filter(|c| c.enabled && c.binding.is_some())
            .ok_or("Telegram 尚未启用或绑定。")?;
        state
            .agents
            .devices
            .retain(|_, d| d.updated + 86_400_000 > now());
        if state.agents.devices.len() >= 128 && !state.agents.devices.contains_key(owner) {
            return Err("too many agent devices");
        }
        state.agents.devices.insert(
            owner.into(),
            Device {
                name: exchange.device,
                updated: now(),
                agents: exchange.agents,
            },
        );
        let mut edits = vec![];
        for panel in state
            .agents
            .panels
            .values_mut()
            .filter(|p| p.owner == owner && !p.done)
        {
            if let Some((command, expires)) = &panel.command {
                if let Some(ack) = exchange
                    .acknowledgements
                    .iter()
                    .find(|a| a.command_id == command.command_id)
                {
                    let result = match &ack.error {
                        None => format!("已更新：{}", command.policy.label()),
                        Some(_) => "更新未完成。请发送 /agents 刷新后重试。".into(),
                    };
                    if let Some(message) = panel.message {
                        edits.push((message, format!("{}\n\n{result}", panel.text)));
                    }
                    panel.done = true;
                } else if *expires <= now() {
                    if let Some(message) = panel.message {
                        edits.push((
                            message,
                            format!("{}\n\n操作已超时，未确认更新。", panel.text),
                        ));
                    }
                    panel.done = true;
                }
            }
        }
        state.agents.panels.retain(|_, p| p.expires > now());
        let commands = state
            .agents
            .panels
            .values()
            .filter(|p| p.owner == owner && !p.done)
            .filter_map(|p| {
                p.command
                    .as_ref()
                    .filter(|(_, expiry)| *expiry > now())
                    .map(|(c, _)| c.clone())
            })
            .collect();
        drop(state);
        for (message, text) in edits {
            let _ = self
                .inner
                .api
                .edit(&config, message, &text, json!([]))
                .await;
        }
        Ok(ExchangeReply { commands })
    }

    pub(super) async fn agent_update(&self, generation: u64, update: &Value) -> bool {
        let query = &update["callback_query"];
        let data = query["data"].as_str().unwrap_or("");
        let is_callback = data.starts_with("a:") || data.starts_with("ap:");
        let message = &update["message"];
        let is_command = message["text"].as_str().is_some_and(|t| t == "/agents");
        if !is_callback && !is_command {
            return false;
        }
        let (config, valid) = {
            let state = self.inner.state.lock().await;
            if state.generation != generation {
                return true;
            }
            let Some(config) = state.config.clone() else {
                return true;
            };
            let (from, chat) = if is_callback {
                (&query["from"], &query["message"]["chat"])
            } else {
                (&message["from"], &message["chat"])
            };
            let valid = config.enabled
                && config.binding.as_ref().is_some_and(|b| {
                    from["id"].as_i64() == Some(b.user_id)
                        && from["is_bot"] == false
                        && chat["id"].as_i64() == Some(b.chat_id)
                        && chat["type"] == "private"
                });
            (config, valid)
        };
        if !valid {
            if is_callback {
                self.answer_agent(&config, query, "此账号无权管理 Agent")
                    .await;
            }
            return true;
        }
        if is_command || data.starts_with("ap:") {
            let page = data
                .strip_prefix("ap:")
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(0);
            if is_callback {
                self.answer_agent(&config, query, "正在读取 Agent 列表")
                    .await;
            }
            let _ = self.send_agent_page(&config, page).await;
            return true;
        }
        let mut parts = data.split(':');
        let _ = parts.next();
        let ticket = parts.next().unwrap_or("");
        let policy = match parts.next() {
            Some("once") => Some(Policy::Once),
            Some("trust") => Some(Policy::Trust),
            Some("ask") => Some(Policy::Ask),
            Some("deny") => Some(Policy::Deny),
            _ => None,
        };
        let response = if let Some(policy) = policy {
            self.queue_agent_change(
                generation,
                ticket,
                query["message"]["message_id"].as_i64(),
                policy,
            )
            .await
        } else {
            "无效操作"
        };
        self.answer_agent(&config, query, response).await;
        true
    }

    async fn answer_agent(&self, config: &Config, query: &Value, text: &str) {
        let _ = self
            .inner
            .api
            .call(
                &config.bot_token,
                "answerCallbackQuery",
                json!({"callback_query_id":query["id"],"text":text}),
            )
            .await;
    }

    async fn queue_agent_change(
        &self,
        generation: u64,
        ticket: &str,
        message: Option<i64>,
        policy: Policy,
    ) -> &'static str {
        let mut state = self.inner.state.lock().await;
        if state.generation != generation {
            return "设置已变化，请重新打开列表";
        }
        let Some(panel) = state.agents.panels.get(ticket) else {
            return "列表已失效，请发送 /agents 刷新";
        };
        if panel.expires <= now()
            || panel.message.is_none()
            || panel.message != message
            || panel.done
        {
            return "列表已失效，请发送 /agents 刷新";
        }
        if panel.command.is_some() {
            return "已提交，等待设备确认";
        }
        let Some(device) = state
            .agents
            .devices
            .get(&panel.owner)
            .filter(|d| d.updated + ONLINE_MS > now())
        else {
            return "设备离线，请稍后重试";
        };
        let Some(agent) = device
            .agents
            .iter()
            .find(|a| a.id == panel.agent && a.path.is_some())
        else {
            return "程序路径不可用";
        };
        if agent.revision != panel.revision {
            return "授权状态已变化，请发送 /agents 刷新";
        }
        if state
            .agents
            .panels
            .values()
            .filter(|p| p.owner == panel.owner && p.command.is_some() && !p.done)
            .count()
            >= 16
        {
            return "等待设备确认的操作过多";
        }
        let panel = state.agents.panels.get_mut(ticket).unwrap();
        panel.command = Some((
            Change {
                id: panel.agent.clone(),
                policy,
                revision: panel.revision,
                command_id: uuid::Uuid::new_v4().simple().to_string(),
                expires_at_ms: Some(now() + COMMAND_MS),
            },
            now() + COMMAND_MS,
        ));
        "已提交，等待设备确认"
    }

    async fn send_agent_page(&self, config: &Config, offset: usize) -> Result<()> {
        let (entries, total) = {
            let mut state = self.inner.state.lock().await;
            state.agents.panels.retain(|_, p| p.expires > now());
            if state.agents.panels.len() > 1000 {
                return Err("too many open agent panels");
            }
            let mut entries = vec![];
            for (owner, device) in &state.agents.devices {
                for agent in &device.agents {
                    entries.push((
                        owner.clone(),
                        device.name.clone(),
                        device.updated + ONLINE_MS > now(),
                        agent.clone(),
                    ));
                }
            }
            entries.sort_by(|a, b| {
                a.1.cmp(&b.1)
                    .then(a.3.name.cmp(&b.3.name))
                    .then(a.3.path.cmp(&b.3.path))
            });
            let total = entries.len();
            (
                entries
                    .into_iter()
                    .skip(offset)
                    .take(PAGE_SIZE)
                    .collect::<Vec<_>>(),
                total,
            )
        };
        if entries.is_empty() {
            self.inner.api.call(&config.bot_token, "sendMessage", json!({"chat_id":config.binding.as_ref().unwrap().chat_id,"text":"尚无 Agent。请在已连接的设备上启动 Agent，再发送 /agents 刷新。"})).await?;
            return Ok(());
        }
        for (owner, device, online, agent) in entries {
            let ticket = uuid::Uuid::new_v4().simple().to_string();
            let path = agent.path.as_deref().unwrap_or("无法读取程序路径");
            let processes = if agent.processes.is_empty() {
                "未运行".into()
            } else {
                format!(
                    "PID {}",
                    agent
                        .processes
                        .iter()
                        .map(|p| p.pid.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let text = format!(
                "{device}{}\n{}\n{path}\n{processes}\n当前：{}",
                if online { "" } else { "（离线）" },
                agent.name,
                agent.policy.label()
            );
            let can_change = online && agent.path.is_some();
            self.inner.state.lock().await.agents.panels.insert(
                ticket.clone(),
                Panel {
                    owner,
                    agent: agent.id,
                    revision: agent.revision,
                    message: None,
                    text: text.clone(),
                    expires: now() + PANEL_MS,
                    command: None,
                    done: false,
                },
            );
            let parts = chunks(&text, 3400);
            for (index, part) in parts.iter().enumerate() {
                let last = index + 1 == parts.len();
                let keys = if last && can_change {
                    policy_keyboard(&ticket)
                } else {
                    json!([])
                };
                let result = self.inner.api.call(&config.bot_token, "sendMessage", json!({"chat_id":config.binding.as_ref().unwrap().chat_id,"text":part,"reply_markup":{"inline_keyboard":keys},"link_preview_options":{"is_disabled":true}})).await?;
                if last
                    && let Some(panel) =
                        self.inner.state.lock().await.agents.panels.get_mut(&ticket)
                {
                    panel.message = result["message_id"].as_i64();
                    panel.text = part.clone();
                }
            }
        }
        if offset.saturating_add(PAGE_SIZE) < total {
            self.inner.api.call(&config.bot_token, "sendMessage", json!({"chat_id":config.binding.as_ref().unwrap().chat_id,"text":format!("共 {total} 个程序路径"),"reply_markup":{"inline_keyboard":[[{"text":"下一页","callback_data":format!("ap:{}", offset + PAGE_SIZE)}]]}})).await?;
        }
        Ok(())
    }
}

fn policy_keyboard(ticket: &str) -> Value {
    json!([
        [{"text":"授权 1 次","callback_data":format!("a:{ticket}:once")},{"text":"长期信任","callback_data":format!("a:{ticket}:trust")}],
        [{"text":"无预授权","callback_data":format!("a:{ticket}:ask")},{"text":"总是拒绝","callback_data":format!("a:{ticket}:deny")}]
    ])
}
