use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    UserPrompt,
    PreToolUse,
    PostToolUse,
    PermissionRequest,
    SessionStart,
    SessionEnd,
    TranscriptEvent,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalEvent {
    pub schema_version: String,
    pub event_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub agent: AgentInfo,
    pub session: SessionInfo,
    pub event: EventDescriptor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub source_event_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventDescriptor {
    pub kind: EventKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Allow,
    Flag,
    Deny,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub rule_id: String,
    pub title: String,
    pub severity: Severity,
    pub action: Action,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub action: Action,
    pub findings: Vec<Finding>,
    pub evaluation_ms: u128,
}

impl Decision {
    pub fn reason(&self) -> Option<String> {
        self.findings
            .iter()
            .filter(|finding| finding.action == Action::Deny)
            .map(|finding| finding.message.as_str())
            .next()
            .map(str::to_owned)
    }
}
