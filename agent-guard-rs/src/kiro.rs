use crate::claude::Scope;
use crate::core::{AgentInfo, CanonicalEvent, EventDescriptor, EventKind, SessionInfo, ToolInfo};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MARKER: &str = "hook dispatch --agent kiro --workspace";
const HOOK_NAMES: [(&str, &str); 3] = [
    ("Agent Guard PreToolUse", "PreToolUse"),
    ("Agent Guard PostToolUse", "PostToolUse"),
    ("Agent Guard UserPromptSubmit", "UserPromptSubmit"),
];

pub fn normalize_event(payload: Value) -> Result<CanonicalEvent> {
    let object = payload
        .as_object()
        .context("Kiro hook payload must be a JSON object")?;
    let source_event_type = first_string(
        object,
        &[
            "hook_event_name",
            "hookEventName",
            "event_name",
            "eventName",
            "event",
        ],
    )
    .unwrap_or("unknown");
    let event_kind = match source_event_type {
        "PreToolUse" | "preToolUse" => EventKind::PreToolUse,
        "PostToolUse" | "postToolUse" => EventKind::PostToolUse,
        "UserPromptSubmit" | "userPromptSubmit" => EventKind::UserPrompt,
        "SessionStart" | "sessionStart" | "AgentSpawn" | "agentSpawn" => EventKind::SessionStart,
        "SessionEnd" | "sessionEnd" | "Stop" | "stop" => EventKind::SessionEnd,
        _ => EventKind::Unknown,
    };
    let session_id = first_string(
        object,
        &[
            "session_id",
            "sessionId",
            "conversation_id",
            "conversationId",
        ],
    )
    .unwrap_or("unknown")
    .to_owned();
    let tool = first_string(object, &["tool_name", "toolName", "tool"]).map(|name| ToolInfo {
        name: name.to_owned(),
        input: first_value(object, &["tool_input", "toolInput", "input"])
            .cloned()
            .unwrap_or(Value::Null),
        output: first_value(
            object,
            &[
                "tool_response",
                "toolResponse",
                "tool_output",
                "toolOutput",
                "tool_result",
                "toolResult",
                "output",
                "result",
            ],
        )
        .cloned(),
    });

    Ok(CanonicalEvent {
        schema_version: "1".to_owned(),
        event_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        agent: AgentInfo {
            kind: "kiro".to_owned(),
            version: first_string(object, &["kiro_version", "kiroVersion", "agent_version"])
                .map(str::to_owned),
        },
        session: SessionInfo { id: session_id },
        event: EventDescriptor { kind: event_kind },
        tool,
        prompt: first_string(object, &["prompt", "user_prompt", "userPrompt"]).map(str::to_owned),
        cwd: first_string(object, &["cwd", "working_directory", "workingDirectory"])
            .map(str::to_owned),
        source_event_type: source_event_type.to_owned(),
    })
}

fn first_string<'a>(object: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
}

fn first_value<'a>(object: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| object.get(*key))
}

pub fn hooks_path(scope: Scope, workspace: &Path) -> Result<PathBuf> {
    // Kiro CLI v3 migrated hooks from .kiro/agents/*.json to standalone .kiro/hooks files.
    match scope {
        Scope::Project => Ok(workspace.join(".kiro/hooks/agent-guard.json")),
        Scope::User => {
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            Ok(PathBuf::from(home).join(".kiro/hooks/agent-guard.json"))
        }
    }
}

pub fn install_hook(scope: Scope, workspace: &Path, executable: &Path) -> Result<PathBuf> {
    let path = hooks_path(scope, workspace)?;
    let current = read_existing(&path)?;
    if let Some(config) = current.as_ref() {
        ensure_owned(&path, config)?;
    }

    let command = format!(
        "{} hook dispatch --agent kiro --workspace {}",
        shell_quote(executable),
        shell_quote(workspace)
    );
    let config = json!({
        "version": "v1",
        "hooks": HOOK_NAMES.iter().map(|(name, event)| json!({
            "name": name,
            "trigger": event,
            "matcher": ".*",
            "action": {
                "type": "command",
                "command": command
            },
            "timeout": 10,
            "enabled": true
        })).collect::<Vec<_>>()
    });
    if current.as_ref() != Some(&config) {
        write_atomic(&path, &config)?;
    }
    Ok(path)
}

pub fn uninstall_hook(scope: Scope, workspace: &Path) -> Result<PathBuf> {
    let path = hooks_path(scope, workspace)?;
    let Some(config) = read_existing(&path)? else {
        return Ok(path);
    };
    ensure_owned(&path, &config)?;
    fs::remove_file(&path)
        .with_context(|| format!("failed to remove Kiro hooks file {}", path.display()))?;
    Ok(path)
}

pub fn hook_installed(scope: Scope, workspace: &Path) -> Result<bool> {
    let path = hooks_path(scope, workspace)?;
    let Some(config) = read_existing(&path)? else {
        return Ok(false);
    };
    Ok(is_owned(&config))
}

fn read_existing(path: &Path) -> Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    let config: Value = serde_json::from_str(&text)
        .with_context(|| format!("refusing to modify invalid JSON in {}", path.display()))?;
    if !config.is_object() {
        bail!("Kiro hooks {} must contain a JSON object", path.display());
    }
    Ok(Some(config))
}

fn ensure_owned(path: &Path, config: &Value) -> Result<()> {
    if !is_owned(config) {
        bail!(
            "refusing to modify unowned Kiro hooks file {}",
            path.display()
        );
    }
    Ok(())
}

fn is_owned(config: &Value) -> bool {
    let Some(hooks) = config.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    hooks.len() == HOOK_NAMES.len()
        && HOOK_NAMES.iter().all(|(name, _)| {
            hooks.iter().any(|hook| {
                hook.get("name").and_then(Value::as_str) == Some(*name) && has_owned_command(hook)
            })
        })
}

fn has_owned_command(hook: &Value) -> bool {
    hook.get("action")
        .and_then(|action| action.get("command"))
        .and_then(Value::as_str)
        .map(|command| command.contains(MARKER))
        .unwrap_or(false)
}

fn write_atomic(path: &Path, config: &Value) -> Result<()> {
    let parent = path.parent().context("Kiro hooks path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".agent-guard.json.{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(config)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
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
    format!("\"{}\"", path.to_string_lossy().replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn installs_v1_shape_atomically_and_idempotently() {
        let temp = TempDir::new().unwrap();
        let executable = Path::new("/opt/Agent Guard/agent-guard");
        let path = install_hook(Scope::Project, temp.path(), executable).unwrap();
        let first = fs::read(&path).unwrap();
        install_hook(Scope::Project, temp.path(), executable).unwrap();
        assert_eq!(fs::read(&path).unwrap(), first);

        let config: Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(config["version"], "v1");
        let hooks = config["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 3);
        for ((name, event), hook) in HOOK_NAMES.iter().zip(hooks) {
            assert_eq!(hook["name"], *name);
            assert_eq!(hook["trigger"], *event);
            assert_eq!(hook["matcher"], ".*");
            assert_eq!(hook["action"]["type"], "command");
            assert!(hook["action"]["command"].as_str().unwrap().contains(MARKER));
            assert_eq!(hook["timeout"], 10);
            assert_eq!(hook["enabled"], true);
        }
        assert!(hook_installed(Scope::Project, temp.path()).unwrap());
    }

    #[test]
    fn refuses_to_overwrite_or_remove_unowned_files() {
        let temp = TempDir::new().unwrap();
        let path = hooks_path(Scope::Project, temp.path()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let foreign = r#"{"version":"v1","hooks":[{"name":"Other","action":{"type":"command","command":"other"}}]}"#;
        fs::write(&path, foreign).unwrap();

        assert!(install_hook(Scope::Project, temp.path(), Path::new("/bin/guard")).is_err());
        assert!(uninstall_hook(Scope::Project, temp.path()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), foreign);
        assert!(!hook_installed(Scope::Project, temp.path()).unwrap());
    }

    #[test]
    fn ownership_requires_all_stable_names_and_dispatch_commands() {
        let temp = TempDir::new().unwrap();
        let path = hooks_path(Scope::Project, temp.path()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let hooks = HOOK_NAMES
            .iter()
            .map(|(name, event)| {
                json!({
                    "name": name,
                    "trigger": event,
                    "action": {"type": "command", "command": "other"}
                })
            })
            .collect::<Vec<_>>();
        fs::write(&path, serde_json::to_vec(&json!({"hooks": hooks})).unwrap()).unwrap();

        assert!(!hook_installed(Scope::Project, temp.path()).unwrap());
        assert!(uninstall_hook(Scope::Project, temp.path()).is_err());
        assert!(path.exists());
    }

    #[test]
    fn mixed_owned_and_foreign_hooks_are_never_overwritten_or_removed() {
        let temp = TempDir::new().unwrap();
        let path =
            install_hook(Scope::Project, temp.path(), Path::new("/opt/agent-guard")).unwrap();
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "User Hook", "trigger": "SessionStart"}));
        fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();

        assert!(install_hook(Scope::Project, temp.path(), Path::new("/opt/agent-guard")).is_err());
        assert!(uninstall_hook(Scope::Project, temp.path()).is_err());
        assert!(path.exists());
        assert!(!hook_installed(Scope::Project, temp.path()).unwrap());
    }

    #[test]
    fn uninstalls_an_owned_file() {
        let temp = TempDir::new().unwrap();
        let path =
            install_hook(Scope::Project, temp.path(), Path::new("/opt/agent-guard")).unwrap();
        uninstall_hook(Scope::Project, temp.path()).unwrap();
        assert!(!path.exists());
        assert!(!hook_installed(Scope::Project, temp.path()).unwrap());
    }

    #[test]
    fn normalizes_pascal_and_lower_camel_payloads() {
        for (source, expected) in [
            ("PreToolUse", EventKind::PreToolUse),
            ("preToolUse", EventKind::PreToolUse),
            ("PostToolUse", EventKind::PostToolUse),
            ("postToolUse", EventKind::PostToolUse),
            ("UserPromptSubmit", EventKind::UserPrompt),
            ("userPromptSubmit", EventKind::UserPrompt),
        ] {
            let event = normalize_event(json!({"eventName": source})).unwrap();
            assert_eq!(event.event.kind, expected);
            assert_eq!(event.source_event_type, source);
        }

        let event = normalize_event(json!({
            "eventName": "postToolUse",
            "sessionId": "session-1",
            "toolName": "shell",
            "toolInput": {"command": "date"},
            "toolResult": {"stdout": "today"},
            "userPrompt": "run date",
            "workingDirectory": "/workspace",
            "kiroVersion": "3.0.0"
        }))
        .unwrap();
        assert_eq!(event.session.id, "session-1");
        assert_eq!(event.tool.as_ref().unwrap().name, "shell");
        assert_eq!(event.tool.as_ref().unwrap().input["command"], "date");
        assert_eq!(event.tool.unwrap().output.unwrap()["stdout"], "today");
        assert_eq!(event.prompt.as_deref(), Some("run date"));
        assert_eq!(event.cwd.as_deref(), Some("/workspace"));
        assert_eq!(event.agent.version.as_deref(), Some("3.0.0"));
    }

    #[test]
    fn malformed_json_is_never_modified() {
        let temp = TempDir::new().unwrap();
        let path = hooks_path(Scope::Project, temp.path()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{invalid").unwrap();

        assert!(install_hook(Scope::Project, temp.path(), Path::new("/bin/guard")).is_err());
        assert!(uninstall_hook(Scope::Project, temp.path()).is_err());
        assert!(hook_installed(Scope::Project, temp.path()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "{invalid");
        assert!(normalize_event(json!([])).is_err());
    }
}
