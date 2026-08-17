use crate::claude::Scope;
use crate::core::{AgentInfo, CanonicalEvent, EventDescriptor, EventKind, SessionInfo, ToolInfo};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MARKER: &str = "hook dispatch --agent cursor --workspace";

pub fn normalize_event(payload: Value) -> Result<CanonicalEvent> {
    let object = payload
        .as_object()
        .context("Cursor hook payload must be a JSON object")?;
    let source_event_type = string_field(object, "hook_event_name")
        .or_else(|| string_field(object, "hookEvent"))
        .or_else(|| string_field(object, "hookEventName"))
        .unwrap_or("unknown");
    let event_kind = match source_event_type {
        "preToolUse" => EventKind::PreToolUse,
        "postToolUse" | "postToolUseFailure" => EventKind::PostToolUse,
        "beforeSubmitPrompt" => EventKind::UserPrompt,
        "sessionStart" => EventKind::SessionStart,
        "sessionEnd" => EventKind::SessionEnd,
        _ => EventKind::Unknown,
    };
    let session_id = string_field(object, "conversation_id")
        .or_else(|| string_field(object, "session_id"))
        .unwrap_or("unknown")
        .to_owned();
    let tool = string_field(object, "tool_name").map(|name| ToolInfo {
        name: name.to_owned(),
        input: object.get("tool_input").cloned().unwrap_or(Value::Null),
        output: object
            .get("tool_output")
            .or_else(|| object.get("error_message"))
            .cloned(),
    });

    Ok(CanonicalEvent {
        schema_version: "1".to_owned(),
        event_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        agent: AgentInfo {
            kind: "cursor".to_owned(),
            version: string_field(object, "cursor_version").map(str::to_owned),
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

pub fn hooks_path(scope: Scope, workspace: &Path) -> Result<PathBuf> {
    match scope {
        Scope::Project => Ok(workspace.join(".cursor/hooks.json")),
        Scope::User => {
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            Ok(PathBuf::from(home).join(".cursor/hooks.json"))
        }
    }
}

pub fn install_hook(scope: Scope, workspace: &Path, executable: &Path) -> Result<PathBuf> {
    let path = hooks_path(scope, workspace)?;
    let mut config = read_config(&path)?;
    let hooks = ensure_hooks_object(&mut config)?;
    remove_managed_hooks(hooks);

    let command = format!(
        "{} hook dispatch --agent cursor --workspace {}",
        shell_quote(executable),
        shell_quote(workspace)
    );
    for (event, fail_closed) in [("preToolUse", true), ("postToolUse", false)] {
        let entries = hooks
            .entry(event.to_owned())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} must be an array"))?;
        let mut entry = json!({
            "type": "command",
            "command": command,
            "matcher": ".*",
            "timeout": 10
        });
        if fail_closed {
            entry["failClosed"] = Value::Bool(true);
        }
        entries.push(entry);
    }
    config["version"] = json!(1);
    write_config(&path, &config)?;
    Ok(path)
}

pub fn uninstall_hook(scope: Scope, workspace: &Path) -> Result<PathBuf> {
    let path = hooks_path(scope, workspace)?;
    if !path.exists() {
        return Ok(path);
    }
    let mut config = read_config(&path)?;
    let Some(hooks) = config.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(path);
    };
    if remove_managed_hooks(hooks) {
        write_config(&path, &config)?;
    }
    Ok(path)
}

pub fn hook_installed(scope: Scope, workspace: &Path) -> Result<bool> {
    let path = hooks_path(scope, workspace)?;
    if !path.exists() {
        return Ok(false);
    }
    let config = read_config(&path)?;
    Ok(config
        .get("hooks")
        .and_then(Value::as_object)
        .map(|hooks| {
            hooks.values().any(|entries| {
                entries
                    .as_array()
                    .is_some_and(|entries| entries.iter().any(is_managed_entry))
            })
        })
        .unwrap_or(false))
}

fn read_config(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("refusing to modify invalid JSON in {}", path.display()))?;
    if !value.is_object() {
        bail!("Cursor hooks {} must contain a JSON object", path.display());
    }
    Ok(value)
}

fn ensure_hooks_object(config: &mut Value) -> Result<&mut Map<String, Value>> {
    let root = config
        .as_object_mut()
        .context("Cursor hooks root must be an object")?;
    root.entry("hooks".to_owned())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("hooks must be an object")
}

fn remove_managed_hooks(hooks: &mut Map<String, Value>) -> bool {
    let mut removed = false;
    for entries in hooks.values_mut() {
        let Some(entries) = entries.as_array_mut() else {
            continue;
        };
        let old_len = entries.len();
        entries.retain(|entry| !is_managed_entry(entry));
        removed |= entries.len() != old_len;
    }
    removed
}

fn is_managed_entry(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(Value::as_str)
        .map(|command| command.contains(MARKER))
        .unwrap_or(false)
}

fn write_config(path: &Path, config: &Value) -> Result<()> {
    let parent = path.parent().context("Cursor hooks path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        fs::write(&temporary, serde_json::to_vec_pretty(config)?)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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
    fn normalizes_all_supported_event_kinds() {
        let cases = [
            ("preToolUse", EventKind::PreToolUse),
            ("postToolUse", EventKind::PostToolUse),
            ("postToolUseFailure", EventKind::PostToolUse),
            ("beforeSubmitPrompt", EventKind::UserPrompt),
            ("sessionStart", EventKind::SessionStart),
            ("sessionEnd", EventKind::SessionEnd),
        ];
        for (source, expected) in cases {
            let event = normalize_event(json!({
                "hook_event_name": source,
                "conversation_id": "conversation-1"
            }))
            .unwrap();
            assert_eq!(event.event.kind, expected);
            assert_eq!(event.session.id, "conversation-1");
            assert_eq!(event.source_event_type, source);
        }
    }

    #[test]
    fn normalizes_tool_prompt_failure_and_metadata() {
        let event = normalize_event(json!({
            "hook_event_name": "postToolUseFailure",
            "conversation_id": "conversation-2",
            "session_id": "fallback",
            "tool_name": "Shell",
            "tool_input": {"command": "false"},
            "error_message": "exit 1",
            "prompt": "run it",
            "cwd": "/workspace",
            "cursor_version": "1.2.3"
        }))
        .unwrap();
        let tool = event.tool.unwrap();
        assert_eq!(tool.name, "Shell");
        assert_eq!(tool.input["command"], "false");
        assert_eq!(tool.output, Some(json!("exit 1")));
        assert_eq!(event.prompt.as_deref(), Some("run it"));
        assert_eq!(event.cwd.as_deref(), Some("/workspace"));
        assert_eq!(event.agent.version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn install_uses_flat_shape_is_idempotent_and_preserves_json() {
        let temp = TempDir::new().unwrap();
        let path = hooks_path(Scope::Project, temp.path()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"theme":"dark","hooks":{"preToolUse":[{"type":"command","command":"other"}],"sessionStart":[{"type":"command","command":"lifecycle"}]}}"#,
        )
        .unwrap();

        install_hook(Scope::Project, temp.path(), Path::new("/opt/agent guard")).unwrap();
        install_hook(Scope::Project, temp.path(), Path::new("/opt/agent guard")).unwrap();
        let config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(config["version"], 1);
        assert_eq!(config["theme"], "dark");
        assert_eq!(config["hooks"]["sessionStart"][0]["command"], "lifecycle");
        assert_eq!(config["hooks"]["preToolUse"].as_array().unwrap().len(), 2);
        let pre = &config["hooks"]["preToolUse"][1];
        assert_eq!(pre["type"], "command");
        assert_eq!(pre["matcher"], ".*");
        assert_eq!(pre["timeout"], 10);
        assert_eq!(pre["failClosed"], true);
        assert!(pre["command"].as_str().unwrap().contains(MARKER));
        let post = &config["hooks"]["postToolUse"][0];
        assert_eq!(post["timeout"], 10);
        assert!(post.get("failClosed").is_none());
        assert!(hook_installed(Scope::Project, temp.path()).unwrap());
    }

    #[test]
    fn uninstall_removes_only_owned_flat_entries() {
        let temp = TempDir::new().unwrap();
        install_hook(Scope::Project, temp.path(), Path::new("/opt/agent-guard")).unwrap();
        let path = hooks_path(Scope::Project, temp.path()).unwrap();
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["hooks"]["postToolUse"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "command", "command": "other"}));
        write_config(&path, &config).unwrap();

        uninstall_hook(Scope::Project, temp.path()).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains(MARKER));
        assert!(text.contains("other"));
        assert!(!hook_installed(Scope::Project, temp.path()).unwrap());
    }

    #[test]
    fn malformed_json_is_never_overwritten() {
        let temp = TempDir::new().unwrap();
        let path = hooks_path(Scope::Project, temp.path()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not-json").unwrap();

        assert!(install_hook(Scope::Project, temp.path(), Path::new("/bin/tool")).is_err());
        assert!(uninstall_hook(Scope::Project, temp.path()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "{not-json");
    }
}
