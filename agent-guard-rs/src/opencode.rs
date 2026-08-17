use crate::claude::Scope;
use crate::core::{AgentInfo, CanonicalEvent, EventDescriptor, EventKind, SessionInfo, ToolInfo};
use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const PLUGIN_NAME: &str = "agent-guard.js";
const MARKER: &str = "agent-guard-opencode";

pub fn normalize_event(payload: Value) -> Result<CanonicalEvent> {
    let object = payload
        .as_object()
        .context("OpenCode hook payload must be a JSON object")?;
    let source_event_type = string_field(object, "hook_event_name")
        .or_else(|| string_field(object, "hookEvent"))
        .unwrap_or("unknown");
    let input = object.get("input").and_then(Value::as_object);
    let output = object.get("output").and_then(Value::as_object);
    let event_kind = match source_event_type {
        "tool.execute.before" => EventKind::PreToolUse,
        "tool.execute.after" => EventKind::PostToolUse,
        "chat.message" => EventKind::UserPrompt,
        "session.idle" => EventKind::SessionEnd,
        _ => EventKind::Unknown,
    };
    let session_id = string_field(object, "session_id")
        .or_else(|| string_field(object, "sessionID"))
        .or_else(|| string_field(object, "sessionId"))
        .or_else(|| input.and_then(|value| string_field(value, "sessionID")))
        .or_else(|| input.and_then(|value| string_field(value, "sessionId")))
        .unwrap_or("unknown")
        .to_owned();
    let tool_name = string_field(object, "tool_name")
        .or_else(|| string_field(object, "toolName"))
        .or_else(|| input.and_then(|value| string_field(value, "tool")))
        .or_else(|| input.and_then(|value| string_field(value, "name")))
        .map(str::to_owned);
    let tool = tool_name.map(|name| ToolInfo {
        name,
        input: object
            .get("tool_input")
            .or_else(|| object.get("toolInput"))
            .or_else(|| output.and_then(|value| value.get("args")))
            .or_else(|| input.and_then(|value| value.get("args")))
            .cloned()
            .unwrap_or(Value::Null),
        output: object
            .get("tool_output")
            .or_else(|| object.get("toolOutput"))
            .or_else(|| output.and_then(|value| value.get("output")))
            .or_else(|| output.and_then(|value| value.get("result")))
            .or_else(|| output.and_then(|value| value.get("error")))
            .cloned(),
    });

    Ok(CanonicalEvent {
        schema_version: "1".to_owned(),
        event_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        agent: AgentInfo {
            kind: "opencode".to_owned(),
            version: None,
        },
        session: SessionInfo { id: session_id },
        event: EventDescriptor { kind: event_kind },
        tool,
        prompt: string_field(object, "prompt").map(str::to_owned),
        cwd: string_field(object, "cwd").map(str::to_owned),
        source_event_type: source_event_type.to_owned(),
    })
}

fn string_field<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

pub fn plugin_path(scope: Scope, workspace: &Path) -> Result<PathBuf> {
    match scope {
        Scope::Project => Ok(workspace.join(".opencode/plugins").join(PLUGIN_NAME)),
        Scope::User => {
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            Ok(PathBuf::from(home)
                .join(".config/opencode/plugins")
                .join(PLUGIN_NAME))
        }
    }
}

pub fn install_plugin(scope: Scope, workspace: &Path, executable: &Path) -> Result<PathBuf> {
    let path = plugin_path(scope, workspace)?;
    let parent = path
        .parent()
        .context("OpenCode plugin path has no parent")?;
    fs::create_dir_all(parent)?;
    let source = plugin_source(executable)?;
    let temporary = path.with_extension(format!("js.{}.tmp", std::process::id()));
    fs::write(&temporary, source)?;
    fs::rename(&temporary, &path)?;
    Ok(path)
}

pub fn uninstall_plugin(scope: Scope, workspace: &Path) -> Result<PathBuf> {
    let path = plugin_path(scope, workspace)?;
    if path.exists() {
        let content = fs::read_to_string(&path)?;
        if content.contains(MARKER) {
            fs::remove_file(&path)?;
        }
    }
    Ok(path)
}

pub fn plugin_installed(scope: Scope, workspace: &Path) -> Result<bool> {
    let path = plugin_path(scope, workspace)?;
    if !path.exists() {
        return Ok(false);
    }
    Ok(fs::read_to_string(path)?.contains(MARKER))
}

fn plugin_source(executable: &Path) -> Result<String> {
    let binary = serde_json::to_string(&executable.to_string_lossy())?;
    Ok(format!(
        r#"// {MARKER}: managed by Agent Guard. Manual changes are overwritten.
import {{ spawnSync }} from "node:child_process";

const AGENT_GUARD = {binary};

function dispatch(directory, eventName, input, output) {{
  const payload = {{
    hook_event_name: eventName,
    session_id: input?.sessionID ?? input?.sessionId ?? "unknown",
    tool_name: input?.tool ?? input?.name ?? "unknown",
    tool_input: output?.args ?? input?.args ?? {{}},
    tool_output: eventName === "tool.execute.after"
      ? (output?.output ?? output?.result ?? output?.error ?? null)
      : null,
    cwd: directory,
  }};
  const result = spawnSync(
    AGENT_GUARD,
    ["hook", "dispatch", "--agent", "opencode", "--workspace", directory],
    {{ input: JSON.stringify(payload), encoding: "utf8", timeout: 10000, maxBuffer: 1024 * 1024 }}
  );
  if (result.status === 2) {{
    if (eventName === "tool.execute.before") {{
      throw new Error((result.stderr || "Blocked by Agent Guard").trim());
    }}
    return;
  }}
  if (result.error || result.status !== 0) {{
    console.error("[agent-guard] policy dispatch failed; allowing tool call", result.error || result.stderr);
  }}
}}

export const AgentGuardPlugin = async (context) => {{
  const directory = context?.directory ?? process.cwd();
  return {{
    "tool.execute.before": async (input, output) => dispatch(directory, "tool.execute.before", input, output),
    "tool.execute.after": async (input, output) => dispatch(directory, "tool.execute.after", input, output),
  }};
}};
"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::EventKind;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn normalizes_before_hook_using_mutable_output_args() {
        let event = normalize_event(json!({
            "hook_event_name": "tool.execute.before",
            "input": {"sessionID": "session-1", "tool": "bash"},
            "output": {"args": {"command": "date"}},
            "cwd": "/workspace"
        }))
        .unwrap();
        assert_eq!(event.agent.kind, "opencode");
        assert_eq!(event.event.kind, EventKind::PreToolUse);
        assert_eq!(event.tool.unwrap().input["command"], "date");
    }

    #[test]
    fn install_is_idempotent_and_uninstall_is_owned() {
        let temp = TempDir::new().unwrap();
        let binary = Path::new("/opt/agent-guard");
        let path = install_plugin(Scope::Project, temp.path(), binary).unwrap();
        install_plugin(Scope::Project, temp.path(), binary).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        assert!(source.contains(MARKER));
        assert!(source.contains("export const AgentGuardPlugin"));
        assert!(source.contains("tool.execute.before"));
        assert!(source.contains("output?.args"));
        assert!(plugin_installed(Scope::Project, temp.path()).unwrap());
        uninstall_plugin(Scope::Project, temp.path()).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn uninstall_preserves_file_without_ownership_marker() {
        let temp = TempDir::new().unwrap();
        let path = plugin_path(Scope::Project, temp.path()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "export default {};").unwrap();
        uninstall_plugin(Scope::Project, temp.path()).unwrap();
        assert!(path.exists());
    }
}
