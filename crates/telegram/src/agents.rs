//! Human-facing agent policies. These types are never part of the MCP schema.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    Once,
    Trust,
    #[default]
    Ask,
    Deny,
}

impl Policy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Once => "授权 1 次",
            Self::Trust => "长期信任",
            Self::Ask => "无预授权",
            Self::Deny => "总是拒绝",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub started_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub path: Option<String>,
    pub processes: Vec<Process>,
    pub policy: Policy,
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub id: String,
    pub policy: Policy,
    pub revision: u64,
    /// Idempotency key assigned by the human-facing transport.
    pub command_id: String,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Acknowledgement {
    pub command_id: String,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Exchange {
    pub device: String,
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub acknowledgements: Vec<Acknowledgement>,
}

#[derive(Deserialize, Serialize)]
pub struct ExchangeReply {
    pub commands: Vec<Change>,
}
