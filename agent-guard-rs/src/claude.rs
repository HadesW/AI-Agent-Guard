use crate::core::{
    AgentInfo, CanonicalEvent, Decision, EventDescriptor, EventKind, SessionInfo, ToolInfo,
};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MARKER: &str = "hook dispatch --agent claude --workspace";
const MATCHER: &str = "Task|Agent|Bash|Read|Edit|MultiEdit|Write|WebFetch|WebSearch|mcp__.*";

#[derive(Debug, Clone, Copy)]
pub enum Scope {
    Project,
    User,
}

pub fn normalize_event(payload: Value) -> Result<CanonicalEvent> {
    let object = payload
        .as_object()
        .context("Claude hook payload must be a JSON object")?;
    let source_event_type = string_field(object, "hook_event_name").unwrap_or("Unknown");
    let event_kind = match source_event_type {
        "UserPromptSubmit" => EventKind::UserPrompt,
        "PreToolUse" => EventKind::PreToolUse,
        "PostToolUse" => EventKind::PostToolUse,
        "PermissionRequest" => EventKind::PermissionRequest,
        "SessionStart" => EventKind::SessionStart,
        "SessionEnd" | "Stop" => EventKind::SessionEnd,
        _ => EventKind::Unknown,
    };
    let session_id = string_field(object, "session_id")
        .or_else(|| string_field(object, "sessionId"))
        .unwrap_or("unknown")
        .to_owned();
    let tool_name = string_field(object, "tool_name").map(str::to_owned);
    let tool = tool_name.map(|name| ToolInfo {
        name,
        input: object.get("tool_input").cloned().unwrap_or(Value::Null),
        output: object.get("tool_response").cloned(),
    });
    let prompt = string_field(object, "prompt").map(str::to_owned);

    Ok(CanonicalEvent {
        schema_version: "1".to_owned(),
        event_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        agent: AgentInfo {
            kind: "claude".to_owned(),
            version: None,
        },
        session: SessionInfo { id: session_id },
        event: EventDescriptor { kind: event_kind },
        tool,
        prompt,
        cwd: string_field(object, "cwd").map(str::to_owned),
        source_event_type: source_event_type.to_owned(),
    })
}

fn string_field<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

pub fn render_decision(decision: &Decision, event_name: &str) -> Option<Value> {
    decision.reason().map(|reason| {
        json!({
            "hookSpecificOutput": {
                "hookEventName": event_name,
                "permissionDecision": "deny",
                "permissionDecisionReason": reason
            }
        })
    })
}

pub fn settings_path(scope: Scope, workspace: &Path) -> Result<PathBuf> {
    match scope {
        Scope::Project => Ok(workspace.join(".claude/settings.json")),
        Scope::User => {
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            Ok(PathBuf::from(home).join(".claude/settings.json"))
        }
    }
}

pub fn install_hook(scope: Scope, workspace: &Path, executable: &Path) -> Result<PathBuf> {
    let path = settings_path(scope, workspace)?;
    let mut config = read_settings(&path)?;
    remove_managed_hooks(&mut config);
    let command = format!(
        "{} hook dispatch --agent claude --workspace {}",
        shell_quote(executable),
        shell_quote(workspace)
    );
    let hooks = ensure_object(&mut config, "hooks")?;
    for event in [
        "PreToolUse",
        "PostToolUse",
        "UserPromptSubmit",
        "SessionStart",
    ] {
        let entries = hooks
            .entry(event.to_owned())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} must be an array"))?;
        let matcher = if event == "PreToolUse" || event == "PostToolUse" {
            MATCHER
        } else {
            "*"
        };
        entries.push(json!({
            "matcher": matcher,
            "hooks": [{"type": "command", "command": command}]
        }));
    }
    write_settings(&path, &config)?;
    Ok(path)
}

pub fn uninstall_hook(scope: Scope, workspace: &Path) -> Result<PathBuf> {
    let path = settings_path(scope, workspace)?;
    if !path.exists() {
        return Ok(path);
    }
    let mut config = read_settings(&path)?;
    remove_managed_hooks(&mut config);
    write_settings(&path, &config)?;
    Ok(path)
}

pub fn hook_installed(scope: Scope, workspace: &Path) -> Result<bool> {
    let path = settings_path(scope, workspace)?;
    if !path.exists() {
        return Ok(false);
    }
    Ok(fs::read_to_string(path)?.contains(MARKER))
}

fn read_settings(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("refusing to modify invalid JSON in {}", path.display()))?;
    if !value.is_object() {
        bail!(
            "Claude settings {} must contain a JSON object",
            path.display()
        );
    }
    Ok(value)
}

fn ensure_object<'a>(root: &'a mut Value, key: &str) -> Result<&'a mut Map<String, Value>> {
    let object = root
        .as_object_mut()
        .context("settings root must be an object")?;
    let value = object.entry(key.to_owned()).or_insert_with(|| json!({}));
    value
        .as_object_mut()
        .with_context(|| format!("{key} must be an object"))
}

fn remove_managed_hooks(config: &mut Value) {
    let Some(hooks) = config.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for entries in hooks.values_mut() {
        let Some(entries) = entries.as_array_mut() else {
            continue;
        };
        for entry in entries.iter_mut() {
            let Some(commands) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            commands.retain(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .map(|command| !command.contains(MARKER))
                    .unwrap_or(true)
            });
        }
        entries.retain(|entry| {
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .map(|commands| !commands.is_empty())
                .unwrap_or(true)
        });
    }
}

fn write_settings(path: &Path, config: &Value) -> Result<()> {
    let parent = path.parent().context("settings path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&temporary, serde_json::to_vec_pretty(config)?)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(not(windows))]
fn shell_quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(windows)]
fn shell_quote(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn install_is_idempotent_and_preserves_other_hooks() {
        let temp = TempDir::new().unwrap();
        let settings = temp.path().join(".claude/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(
            &settings,
            r#"{"hooks":{"PreToolUse":[{"matcher":"Read","hooks":[{"type":"command","command":"other-hook"}]}]}}"#,
        )
        .unwrap();
        install_hook(Scope::Project, temp.path(), Path::new("/opt/agent-guard")).unwrap();
        install_hook(Scope::Project, temp.path(), Path::new("/opt/agent-guard")).unwrap();
        let text = fs::read_to_string(settings).unwrap();
        assert_eq!(text.matches(MARKER).count(), 4);
        assert!(text.contains("other-hook"));
    }

    #[test]
    fn uninstall_only_removes_managed_hooks() {
        let temp = TempDir::new().unwrap();
        install_hook(Scope::Project, temp.path(), Path::new("/opt/agent-guard")).unwrap();
        uninstall_hook(Scope::Project, temp.path()).unwrap();
        let text = fs::read_to_string(temp.path().join(".claude/settings.json")).unwrap();
        assert!(!text.contains(MARKER));
    }
}
