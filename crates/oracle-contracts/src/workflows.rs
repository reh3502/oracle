//! Host-owned durable workflow state; never exposed as module storage.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowKind {
    StructurePlan,
    ResourceBinding,
    CommandBinding,
    Configuration,
    AgentRun,
    AgentCall,
    AgentSpend,
}
impl WorkflowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StructurePlan => "structure_plan",
            Self::ResourceBinding => "resource_binding",
            Self::CommandBinding => "command_binding",
            Self::Configuration => "configuration",
            Self::AgentRun => "agent_run",
            Self::AgentCall => "agent_call",
            Self::AgentSpend => "agent_spend",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRecord {
    pub key: String,
    pub revision: u64,
    pub value: Value,
}
