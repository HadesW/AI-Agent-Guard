use crate::claude::Scope;
use crate::core::{AgentInfo, CanonicalEvent, EventDescriptor, EventKind, SessionInfo, ToolInfo};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MARKER: &str = "hook dispatch --agent gemini --workspace";
const HOOK_NAME: &str = "agent-guard";
const MATCHER: &str = ".*";

pub fn normalize_event(payload: Value) -> Result<CanonicalEvent> {
    let object = payload
        .as_object()
        .context("Gemini hook payload must be a JSON object")?;
    let source_event_type = string_field(object, "hook_event_name")
        .or_else(|| string_field(object, "hookEventName"))
        .unwrap_or("Unknown");
    let event_kind = match source_event_type {
        "BeforeTool" => EventKind::PreToolUse,
        "AfterTool" => EventKind::PostToolUse,
        "BeforeAgent" | "UserPrompt" | "UserPromptSubmit" => EventKind::UserPrompt,
        "SessionStart" => EventKind::SessionStart,
        "SessionEnd" => EventKind::SessionEnd,
        _ => EventKind::Unknown,
    };
    let session_id = string_field(object, "session_id")
        .or_else(|| string_field(object, "sessionId"))
        .unwrap_or("unknown")
        .to_owned();
    let tool = string_field(object, "tool_name")
        .or_else(|| string_field(object, "toolName"))
        .map(|name| ToolInfo {
            name: name.to_owned(),
            input: object
                .get("tool_input")
                .or_else(|| object.get("toolInput"))
                .cloned()
                .unwrap_or(Value::Null),
            output: object
                .get("tool_response")
                .or_else(|| object.get("toolResponse"))
                .or_else(|| object.get("tool_output"))
                .or_else(|| object.get("toolOutput"))
                .cloned(),
        });

    Ok(CanonicalEvent {
        schema_version: "1".to_owned(),
        event_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        agent: AgentInfo {
            kind: "gemini".to_owned(),
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

pub fn settings_path(scope: Scope, workspace: &Path) -> Result<PathBuf> {
    match scope {
        Scope::Project => Ok(workspace.join(".gemini/settings.json")),
        Scope::User => {
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            Ok(PathBuf::from(home).join(".gemini/settings.json"))
        }
    }
}

pub fn install_hook(scope: Scope, workspace: &Path, executable: &Path) -> Result<PathBuf> {
    let path = settings_path(scope, workspace)?;
    let mut config = read_settings(&path)?;
    remove_managed_hooks(&mut config);
    let command = format!(
        "{} hook dispatch --agent gemini --workspace {}",
        shell_quote(executable),
        shell_quote(workspace)
    );
    let hooks = ensure_object(&mut config, "hooks")?;
    for event in ["BeforeTool", "AfterTool"] {
        let entries = hooks
            .entry(event.to_owned())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} must be an array"))?;
        entries.push(json!({
            "matcher": MATCHER,
            "hooks": [{
                "name": HOOK_NAME,
                "type": "command",
                "command": command,
                "timeout": 10000
            }]
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
    if remove_managed_hooks(&mut config) {
        write_settings(&path, &config)?;
    }
    Ok(path)
}

pub fn hook_installed(scope: Scope, workspace: &Path) -> Result<bool> {
    let path = settings_path(scope, workspace)?;
    if !path.exists() {
        return Ok(false);
    }
    let config = read_settings(&path)?;
    Ok(config
        .get("hooks")
        .and_then(Value::as_object)
        .map(|hooks| {
            hooks.values().any(|entries| {
                entries.as_array().is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry
                            .get("hooks")
                            .and_then(Value::as_array)
                            .is_some_and(|commands| commands.iter().any(is_managed_hook))
                    })
                })
            })
        })
        .unwrap_or(false))
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
            "Gemini settings {} must contain a JSON object",
            path.display()
        );
    }
    Ok(value)
}

fn ensure_object<'a>(root: &'a mut Value, key: &str) -> Result<&'a mut Map<String, Value>> {
    let object = root
        .as_object_mut()
        .context("settings root must be an object")?;
    object
        .entry(key.to_owned())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .with_context(|| format!("{key} must be an object"))
}

fn remove_managed_hooks(config: &mut Value) -> bool {
    let Some(hooks) = config.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut removed = false;
    let mut empty_events = Vec::new();
    for (event, value) in hooks.iter_mut() {
        let Some(entries) = value.as_array_mut() else {
            continue;
        };
        let mut removed_from_event = false;
        for index in (0..entries.len()).rev() {
            let Some(commands) = entries[index]
                .get_mut("hooks")
                .and_then(Value::as_array_mut)
            else {
                continue;
            };
            let old_len = commands.len();
            commands.retain(|hook| !is_managed_hook(hook));
            if commands.len() != old_len {
                removed = true;
                removed_from_event = true;
                if commands.is_empty() {
                    entries.remove(index);
                }
            }
        }
        if removed_from_event && entries.is_empty() {
            empty_events.push(event.clone());
        }
    }
    for event in empty_events {
        hooks.remove(&event);
    }
    let hooks_are_empty = hooks.is_empty();
    if hooks_are_empty {
        config.as_object_mut().unwrap().remove("hooks");
    }
    removed
}

fn is_managed_hook(hook: &Value) -> bool {
    hook.get("name").and_then(Value::as_str) == Some(HOOK_NAME)
        && hook.get("type").and_then(Value::as_str) == Some("command")
        && hook
            .get("command")
            .and_then(Value::as_str)
            .map(|command| command.contains(MARKER))
            .unwrap_or(false)
}

fn write_settings(path: &Path, config: &Value) -> Result<()> {
    let parent = path.parent().context("settings path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
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
    use tempfile::TempDir;

    #[test]
    fn normalizes_supported_gemini_events() {
        let before = normalize_event(json!({
            "hook_event_name": "BeforeTool",
            "session_id": "session-1",
            "tool_name": "run_shell_command",
            "tool_input": {"command": "date"},
            "cwd": "/workspace"
        }))
        .unwrap();
        assert_eq!(before.agent.kind, "gemini");
        assert_eq!(before.event.kind, EventKind::PreToolUse);
        assert_eq!(before.tool.unwrap().input["command"], "date");

        let after = normalize_event(json!({
            "hook_event_name": "AfterTool",
            "tool_name": "read_file",
            "tool_input": {"path": "README.md"},
            "tool_response": {"content": "hello"}
        }))
        .unwrap();
        assert_eq!(after.event.kind, EventKind::PostToolUse);
        assert_eq!(after.tool.unwrap().output.unwrap()["content"], "hello");

        for (source, kind) in [
            ("BeforeAgent", EventKind::UserPrompt),
            ("SessionStart", EventKind::SessionStart),
            ("SessionEnd", EventKind::SessionEnd),
        ] {
            let event = normalize_event(json!({
                "hook_event_name": source,
                "prompt": "check this"
            }))
            .unwrap();
            assert_eq!(event.event.kind, kind);
        }
    }

    #[test]
    fn install_preserves_settings_and_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let settings = temp.path().join(".gemini/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(
            &settings,
            r#"{"theme":"dark","hooks":{"BeforeTool":[{"matcher":"read_file","hooks":[{"name":"other","type":"command","command":"other-hook","timeout":500}]}]}}"#,
        )
        .unwrap();

        let executable = Path::new("/opt/Agent Guard/agent-guard");
        install_hook(Scope::Project, temp.path(), executable).unwrap();
        install_hook(Scope::Project, temp.path(), executable).unwrap();
        let config: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(managed_hook_count(&config), 2);
        assert_eq!(config["hooks"]["BeforeTool"][1]["matcher"], MATCHER);
        assert_eq!(
            config["hooks"]["BeforeTool"][1]["hooks"][0]["timeout"],
            10000
        );
        assert!(fs::read_to_string(&settings)
            .unwrap()
            .contains("other-hook"));
    }

    #[test]
    fn uninstall_removes_only_owned_entries_and_empty_containers() {
        let temp = TempDir::new().unwrap();
        let executable = Path::new("/opt/agent-guard");
        let settings = install_hook(Scope::Project, temp.path(), executable).unwrap();
        let mut config: Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        config["keep"] = json!(true);
        config["hooks"]["BeforeTool"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "name": "other",
                "type": "command",
                "command": "hook dispatch --agent gemini --workspace elsewhere"
            }));
        write_settings(&settings, &config).unwrap();

        uninstall_hook(Scope::Project, temp.path()).unwrap();
        let config: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(config["keep"], true);
        assert_eq!(managed_hook_count(&config), 0);
        assert_eq!(
            config["hooks"]["BeforeTool"][0]["hooks"][0]["name"],
            "other"
        );
        assert!(config["hooks"].get("AfterTool").is_none());

        let owned_only = TempDir::new().unwrap();
        install_hook(Scope::Project, owned_only.path(), executable).unwrap();
        let owned_settings = uninstall_hook(Scope::Project, owned_only.path()).unwrap();
        let config: Value =
            serde_json::from_str(&fs::read_to_string(owned_settings).unwrap()).unwrap();
        assert!(config.get("hooks").is_none());
    }

    #[test]
    fn invalid_json_is_not_overwritten() {
        let temp = TempDir::new().unwrap();
        let settings = temp.path().join(".gemini/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, "{invalid").unwrap();

        assert!(install_hook(Scope::Project, temp.path(), Path::new("/bin/guard")).is_err());
        assert_eq!(fs::read_to_string(settings).unwrap(), "{invalid");
    }

    fn managed_hook_count(config: &Value) -> usize {
        config
            .get("hooks")
            .and_then(Value::as_object)
            .map(|hooks| {
                hooks
                    .values()
                    .filter_map(Value::as_array)
                    .flatten()
                    .filter_map(|entry| entry.get("hooks").and_then(Value::as_array))
                    .flatten()
                    .filter(|hook| is_managed_hook(hook))
                    .count()
            })
            .unwrap_or(0)
    }
}
