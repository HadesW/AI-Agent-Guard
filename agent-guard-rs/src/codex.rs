use crate::claude::Scope;
use crate::core::{
    AgentInfo, CanonicalEvent, Decision, EventDescriptor, EventKind, SessionInfo, ToolInfo,
};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MARKER: &str = "hook dispatch --agent codex --workspace";
const MATCHER: &str = ".*";
const EVENTS: [&str; 4] = [
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "UserPromptSubmit",
];

pub fn normalize_event(payload: Value) -> Result<CanonicalEvent> {
    let object = payload
        .as_object()
        .context("Codex hook payload must be a JSON object")?;
    let source_event_type = string_field(object, "hook_event_name").unwrap_or("Unknown");
    let event_kind = match source_event_type {
        "PreToolUse" => EventKind::PreToolUse,
        "PermissionRequest" => EventKind::PermissionRequest,
        "PostToolUse" => EventKind::PostToolUse,
        "UserPromptSubmit" => EventKind::UserPrompt,
        "SessionStart" => EventKind::SessionStart,
        "SessionEnd" => EventKind::SessionEnd,
        _ => EventKind::Unknown,
    };
    let session_id = string_field(object, "session_id")
        .or_else(|| string_field(object, "sessionId"))
        .unwrap_or("unknown")
        .to_owned();
    let tool = string_field(object, "tool_name").map(|name| ToolInfo {
        name: name.to_owned(),
        input: object.get("tool_input").cloned().unwrap_or(Value::Null),
        output: object.get("tool_response").cloned(),
    });

    Ok(CanonicalEvent {
        schema_version: "1".to_owned(),
        event_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        agent: AgentInfo {
            kind: "codex".to_owned(),
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

pub fn hooks_path(scope: Scope, workspace: &Path) -> Result<PathBuf> {
    match scope {
        Scope::Project => Ok(workspace.join(".codex/hooks.json")),
        Scope::User => {
            if let Some(codex_home) = std::env::var_os("CODEX_HOME") {
                return Ok(PathBuf::from(codex_home).join("hooks.json"));
            }
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            Ok(PathBuf::from(home).join(".codex/hooks.json"))
        }
    }
}

pub fn install_hook(scope: Scope, workspace: &Path, executable: &Path) -> Result<PathBuf> {
    let path = hooks_path(scope, workspace)?;
    let mut config = read_hooks(&path)?;
    remove_managed_hooks(&mut config);
    let command = format!(
        "{} hook dispatch --agent codex --workspace {}",
        shell_quote(executable),
        shell_quote(workspace)
    );
    let hooks = ensure_hooks_object(&mut config)?;
    for event in EVENTS {
        let entries = hooks
            .entry(event.to_owned())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} must be an array"))?;
        entries.push(json!({
            "matcher": MATCHER,
            "hooks": [{
                "type": "command",
                "command": command,
                "timeout": 10,
                "async": false
            }]
        }));
    }
    write_hooks(&path, &config)?;
    Ok(path)
}

pub fn uninstall_hook(scope: Scope, workspace: &Path) -> Result<PathBuf> {
    let path = hooks_path(scope, workspace)?;
    if !path.exists() {
        return Ok(path);
    }
    let mut config = read_hooks(&path)?;
    if remove_managed_hooks(&mut config) {
        write_hooks(&path, &config)?;
    }
    Ok(path)
}

pub fn hook_installed(scope: Scope, workspace: &Path) -> Result<bool> {
    let path = hooks_path(scope, workspace)?;
    if !path.exists() {
        return Ok(false);
    }
    let mut config = read_hooks(&path)?;
    Ok(remove_managed_hooks(&mut config))
}

fn read_hooks(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("refusing to modify invalid JSON in {}", path.display()))?;
    if !value.is_object() {
        bail!("Codex hooks {} must contain a JSON object", path.display());
    }
    Ok(value)
}

fn ensure_hooks_object(config: &mut Value) -> Result<&mut Map<String, Value>> {
    let object = config
        .as_object_mut()
        .context("hooks root must be an object")?;
    object
        .entry("hooks".to_owned())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("hooks must be an object")
}

fn remove_managed_hooks(config: &mut Value) -> bool {
    let Some(hooks) = config.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut removed = false;
    for entries in hooks.values_mut() {
        let Some(entries) = entries.as_array_mut() else {
            continue;
        };
        entries.retain_mut(|entry| {
            let Some(commands) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let original_len = commands.len();
            commands.retain(|hook| !is_managed_hook(hook));
            let removed_from_entry = commands.len() != original_len;
            removed |= removed_from_entry;
            !removed_from_entry || !commands.is_empty()
        });
    }
    removed
}

fn is_managed_hook(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .map(|command| command.contains(MARKER))
        .unwrap_or(false)
}

fn write_hooks(path: &Path, config: &Value) -> Result<()> {
    let parent = path.parent().context("hooks path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("json.{}.tmp", Uuid::new_v4()));
    fs::write(&temporary, serde_json::to_vec_pretty(config)?)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
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
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn normalizes_current_lifecycle_payloads() {
        let cases = [
            ("PreToolUse", EventKind::PreToolUse),
            ("PermissionRequest", EventKind::PermissionRequest),
            ("PostToolUse", EventKind::PostToolUse),
            ("UserPromptSubmit", EventKind::UserPrompt),
            ("SessionStart", EventKind::SessionStart),
            ("SessionEnd", EventKind::SessionEnd),
        ];
        for (name, kind) in cases {
            let event = normalize_event(json!({
                "hook_event_name": name,
                "session_id": "session-1",
                "tool_name": "Bash",
                "tool_input": {"command": "pwd"},
                "tool_response": {"stdout": "/workspace"},
                "prompt": "inspect the workspace",
                "cwd": "/workspace"
            }))
            .unwrap();
            assert_eq!(event.agent.kind, "codex");
            assert_eq!(event.event.kind, kind);
            assert_eq!(event.source_event_type, name);
            assert_eq!(event.session.id, "session-1");
            assert_eq!(event.tool.as_ref().unwrap().input["command"], "pwd");
            assert_eq!(
                event.tool.as_ref().unwrap().output.as_ref().unwrap()["stdout"],
                "/workspace"
            );
        }
    }

    #[test]
    fn install_is_nested_idempotent_and_preserves_unrelated_json() {
        let temp = TempDir::new().unwrap();
        let path = hooks_path(Scope::Project, temp.path()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"custom":{"enabled":true},"hooks":{"PreToolUse":[{"matcher":"Read","hooks":[{"type":"command","command":"other-hook"}]}]}}"#,
        )
        .unwrap();

        let executable = Path::new("/opt/Agent Guard/bin/agent-guard");
        install_hook(Scope::Project, temp.path(), executable).unwrap();
        let first_install = fs::read(&path).unwrap();
        install_hook(Scope::Project, temp.path(), executable).unwrap();
        assert_eq!(fs::read(&path).unwrap(), first_install);

        let config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(config.get("version").is_none());
        assert_eq!(config["custom"]["enabled"], true);
        assert_eq!(
            fs::read_to_string(&path).unwrap().matches(MARKER).count(),
            4
        );
        assert_eq!(
            config["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "other-hook"
        );
        for event in EVENTS {
            let entry = config["hooks"][event].as_array().unwrap().last().unwrap();
            assert_eq!(entry["matcher"], MATCHER);
            assert_eq!(entry["hooks"][0]["type"], "command");
            assert_eq!(entry["hooks"][0]["timeout"], 10);
            assert_eq!(entry["hooks"][0]["async"], false);
        }
        assert!(hook_installed(Scope::Project, temp.path()).unwrap());
    }

    #[test]
    fn uninstall_removes_only_owned_commands() {
        let temp = TempDir::new().unwrap();
        let path =
            install_hook(Scope::Project, temp.path(), Path::new("/opt/agent-guard")).unwrap();
        let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        config["hooks"]["PreToolUse"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "command", "command": "other-hook"}));
        write_hooks(&path, &config).unwrap();

        uninstall_hook(Scope::Project, temp.path()).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains(MARKER));
        assert!(text.contains("other-hook"));
        assert!(!hook_installed(Scope::Project, temp.path()).unwrap());
    }

    #[test]
    fn malformed_json_is_never_replaced() {
        let temp = TempDir::new().unwrap();
        let path = hooks_path(Scope::Project, temp.path()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not-json").unwrap();

        assert!(install_hook(Scope::Project, temp.path(), Path::new("/bin/guard")).is_err());
        assert!(uninstall_hook(Scope::Project, temp.path()).is_err());
        assert!(hook_installed(Scope::Project, temp.path()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "{not-json");
    }
}
