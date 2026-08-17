pub use crate::claude::Scope;
use crate::core::{AgentInfo, CanonicalEvent, EventDescriptor, EventKind, SessionInfo, ToolInfo};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Qoder,
    CodeBuddy,
    Qwen,
}

impl Agent {
    fn id(self) -> &'static str {
        match self {
            Self::Qoder => "qoder",
            Self::CodeBuddy => "codebuddy",
            Self::Qwen => "qwen",
        }
    }

    fn directory(self) -> &'static str {
        match self {
            Self::Qoder => ".qoder",
            Self::CodeBuddy => ".codebuddy",
            Self::Qwen => ".qwen",
        }
    }

    fn matcher(self) -> &'static str {
        match self {
            Self::Qoder => "*",
            Self::CodeBuddy | Self::Qwen => ".*",
        }
    }

    fn timeout(self) -> u64 {
        match self {
            Self::Qoder | Self::CodeBuddy => 10,
            Self::Qwen => 10_000,
        }
    }

    fn events(self) -> &'static [&'static str] {
        const QODER: [&str; 4] = [
            "PreToolUse",
            "PostToolUse",
            "PostToolUseFailure",
            "UserPromptSubmit",
        ];
        const WITH_PERMISSION: [&str; 5] = [
            "PreToolUse",
            "PostToolUse",
            "PostToolUseFailure",
            "UserPromptSubmit",
            "PermissionRequest",
        ];
        match self {
            Self::Qoder => &QODER,
            Self::CodeBuddy | Self::Qwen => &WITH_PERMISSION,
        }
    }
}

pub fn normalize_event(agent: Agent, payload: Value) -> Result<CanonicalEvent> {
    let object = payload
        .as_object()
        .with_context(|| format!("{} hook payload must be a JSON object", agent.id()))?;
    let source_event_type =
        string_field(object, &["hook_event_name", "event_name", "event_type"]).unwrap_or("Unknown");
    let event_kind = match source_event_type {
        "PreToolUse" | "pre_tool_use" => EventKind::PreToolUse,
        "PostToolUse" | "post_tool_use" | "PostToolUseFailure" | "post_tool_use_failure" => {
            EventKind::PostToolUse
        }
        "PermissionRequest" | "permission_request" => EventKind::PermissionRequest,
        "UserPromptSubmit" | "user_prompt_submit" | "PromptSubmit" | "prompt_submit" => {
            EventKind::UserPrompt
        }
        "SessionStart" | "session_start" => EventKind::SessionStart,
        "SessionEnd" | "session_end" | "Stop" | "stop" => EventKind::SessionEnd,
        _ => EventKind::Unknown,
    };
    let session_id = string_field(object, &["session_id", "conversation_id"])
        .unwrap_or("unknown")
        .to_owned();
    let tool = string_field(object, &["tool_name"]).map(|name| ToolInfo {
        name: name.to_owned(),
        input: value_field(object, &["tool_input"]).unwrap_or(Value::Null),
        output: value_field(object, &["tool_response", "tool_output", "error_message"]),
    });

    Ok(CanonicalEvent {
        schema_version: "1".to_owned(),
        event_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        agent: AgentInfo {
            kind: agent.id().to_owned(),
            version: string_field(object, &["agent_version", "version"]).map(str::to_owned),
        },
        session: SessionInfo { id: session_id },
        event: EventDescriptor { kind: event_kind },
        tool,
        prompt: string_field(object, &["prompt", "user_prompt"]).map(str::to_owned),
        cwd: string_field(object, &["cwd", "working_directory"]).map(str::to_owned),
        source_event_type: source_event_type.to_owned(),
    })
}

fn string_field<'a>(object: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
}

fn value_field(object: &Map<String, Value>, keys: &[&str]) -> Option<Value> {
    keys.iter().find_map(|key| object.get(*key).cloned())
}

pub fn settings_path(agent: Agent, scope: Scope, workspace: &Path) -> Result<PathBuf> {
    let root = match scope {
        Scope::Project => workspace.to_path_buf(),
        Scope::User => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?),
    };
    Ok(root.join(agent.directory()).join("settings.json"))
}

pub fn install_hook(
    agent: Agent,
    scope: Scope,
    workspace: &Path,
    executable: &Path,
) -> Result<PathBuf> {
    let path = settings_path(agent, scope, workspace)?;
    let mut config = read_settings(agent, &path)?;
    remove_managed_hooks(&mut config, agent);
    let command = format!(
        "{} hook dispatch --agent {} --workspace {}",
        shell_quote(executable),
        agent.id(),
        shell_quote(workspace)
    );
    let hooks = ensure_hooks(&mut config)?;
    for event in agent.events() {
        let entries = hooks
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} must be an array"))?;
        entries.push(json!({
            "matcher": agent.matcher(),
            "hooks": [{
                "type": "command",
                "command": command,
                "timeout": agent.timeout()
            }]
        }));
    }
    write_settings(&path, &config)?;
    Ok(path)
}

pub fn uninstall_hook(agent: Agent, scope: Scope, workspace: &Path) -> Result<PathBuf> {
    let path = settings_path(agent, scope, workspace)?;
    if !path.exists() {
        return Ok(path);
    }
    let mut config = read_settings(agent, &path)?;
    if remove_managed_hooks(&mut config, agent) {
        write_settings(&path, &config)?;
    }
    Ok(path)
}

pub fn hook_installed(agent: Agent, scope: Scope, workspace: &Path) -> Result<bool> {
    let path = settings_path(agent, scope, workspace)?;
    if !path.exists() {
        return Ok(false);
    }
    let config = read_settings(agent, &path)?;
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
                            .is_some_and(|commands| {
                                commands.iter().any(|hook| is_managed_hook(hook, agent))
                            })
                    })
                })
            })
        })
        .unwrap_or(false))
}

fn read_settings(agent: Agent, path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("refusing to modify invalid JSON in {}", path.display()))?;
    if !value.is_object() {
        bail!(
            "{} settings {} must contain a JSON object",
            agent.id(),
            path.display()
        );
    }
    Ok(value)
}

fn ensure_hooks(config: &mut Value) -> Result<&mut Map<String, Value>> {
    config
        .as_object_mut()
        .context("settings root must be an object")?
        .entry("hooks".to_owned())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("hooks must be an object")
}

fn remove_managed_hooks(config: &mut Value, agent: Agent) -> bool {
    let Some(hooks) = config.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut removed = false;
    let mut empty_events = Vec::new();
    for (event, value) in hooks.iter_mut() {
        let Some(entries) = value.as_array_mut() else {
            continue;
        };
        let mut changed_event = false;
        for index in (0..entries.len()).rev() {
            let Some(commands) = entries[index]
                .get_mut("hooks")
                .and_then(Value::as_array_mut)
            else {
                continue;
            };
            let old_len = commands.len();
            commands.retain(|hook| !is_managed_hook(hook, agent));
            if commands.len() != old_len {
                removed = true;
                changed_event = true;
                if commands.is_empty() {
                    entries.remove(index);
                }
            }
        }
        if changed_event && entries.is_empty() {
            empty_events.push(event.clone());
        }
    }
    for event in empty_events {
        hooks.remove(&event);
    }
    if removed && hooks.is_empty() {
        config.as_object_mut().unwrap().remove("hooks");
    }
    removed
}

fn is_managed_hook(hook: &Value, agent: Agent) -> bool {
    let marker = format!("hook dispatch --agent {} --workspace", agent.id());
    hook.get("type").and_then(Value::as_str) == Some("command")
        && hook
            .get("command")
            .and_then(Value::as_str)
            .map(|command| command.contains(&marker))
            .unwrap_or(false)
}

fn write_settings(path: &Path, config: &Value) -> Result<()> {
    let parent = path.parent().context("settings path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("json.{}.tmp", Uuid::new_v4()));
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
    use tempfile::TempDir;

    const AGENTS: [Agent; 3] = [Agent::Qoder, Agent::CodeBuddy, Agent::Qwen];

    #[test]
    fn paths_match_each_agent_for_project_and_user_scopes() {
        let workspace = Path::new("/work/project");
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        for agent in AGENTS {
            let relative = Path::new(agent.directory()).join("settings.json");
            assert_eq!(
                settings_path(agent, Scope::Project, workspace).unwrap(),
                workspace.join(&relative)
            );
            assert_eq!(
                settings_path(agent, Scope::User, workspace).unwrap(),
                home.join(relative)
            );
        }
    }

    #[test]
    fn installs_agent_specific_events_matchers_and_timeouts() {
        for agent in AGENTS {
            let temp = TempDir::new().unwrap();
            let path = install_hook(
                agent,
                Scope::Project,
                temp.path(),
                Path::new("/opt/Agent Guard/guard"),
            )
            .unwrap();
            let config: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            let hooks = config["hooks"].as_object().unwrap();
            assert_eq!(hooks.len(), agent.events().len());
            assert_eq!(managed_hook_count(&config, agent), agent.events().len());
            for event in agent.events() {
                let entry = &hooks[*event][0];
                assert_eq!(entry["matcher"], agent.matcher());
                assert_eq!(entry["hooks"][0]["timeout"], agent.timeout());
                assert!(entry["hooks"][0]["command"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("--agent {}", agent.id())));
            }
            assert_eq!(
                agent.events().contains(&"PermissionRequest"),
                agent != Agent::Qoder
            );
            assert!(["PreToolUse", "PostToolUse", "PostToolUseFailure"]
                .iter()
                .all(|event| hooks.contains_key(*event)));
        }
    }

    #[test]
    fn normalizes_tool_permission_prompt_and_session_events_for_each_agent() {
        let cases = [
            ("pre_tool_use", EventKind::PreToolUse),
            ("PostToolUse", EventKind::PostToolUse),
            ("PostToolUseFailure", EventKind::PostToolUse),
            ("permission_request", EventKind::PermissionRequest),
            ("UserPromptSubmit", EventKind::UserPrompt),
            ("session_start", EventKind::SessionStart),
            ("Stop", EventKind::SessionEnd),
        ];
        for agent in AGENTS {
            for (source, expected) in &cases {
                let event = normalize_event(
                    agent,
                    json!({
                        "hook_event_name": source,
                        "session_id": "session-1",
                        "tool_name": "Shell",
                        "tool_input": {"command": "false"},
                        "error_message": "exit 1",
                        "prompt": "run this",
                        "cwd": "/work"
                    }),
                )
                .unwrap();
                assert_eq!(&event.event.kind, expected);
                assert_eq!(event.agent.kind, agent.id());
                assert_eq!(event.session.id, "session-1");
                assert_eq!(event.source_event_type, *source);
                assert_eq!(event.tool.as_ref().unwrap().output, Some(json!("exit 1")));
                assert_eq!(event.prompt.as_deref(), Some("run this"));
            }
        }
        assert!(normalize_event(Agent::Qoder, json!([])).is_err());
    }

    #[test]
    fn install_is_idempotent_and_uninstall_preserves_unrelated_data() {
        for agent in AGENTS {
            let temp = TempDir::new().unwrap();
            let path = settings_path(agent, Scope::Project, temp.path()).unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                r#"{"theme":"dark","hooks":{"PreToolUse":[{"matcher":"Read","hooks":[{"type":"command","command":"other-hook"}]}],"SessionStart":[{"matcher":"*","hooks":[{"type":"command","command":"lifecycle"}]}]}}"#,
            )
            .unwrap();
            let executable = Path::new("/opt/agent-guard");
            install_hook(agent, Scope::Project, temp.path(), executable).unwrap();
            install_hook(agent, Scope::Project, temp.path(), executable).unwrap();
            let config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(config["theme"], "dark");
            assert_eq!(managed_hook_count(&config, agent), agent.events().len());
            assert!(hook_installed(agent, Scope::Project, temp.path()).unwrap());

            uninstall_hook(agent, Scope::Project, temp.path()).unwrap();
            let config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(managed_hook_count(&config, agent), 0);
            assert_eq!(
                config["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
                "other-hook"
            );
            assert_eq!(
                config["hooks"]["SessionStart"][0]["hooks"][0]["command"],
                "lifecycle"
            );
            assert!(!hook_installed(agent, Scope::Project, temp.path()).unwrap());
        }
    }

    #[test]
    fn malformed_json_is_refused_without_overwrite() {
        for agent in AGENTS {
            let temp = TempDir::new().unwrap();
            let path = settings_path(agent, Scope::Project, temp.path()).unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "{invalid").unwrap();
            assert!(install_hook(agent, Scope::Project, temp.path(), Path::new("guard")).is_err());
            assert!(uninstall_hook(agent, Scope::Project, temp.path()).is_err());
            assert!(hook_installed(agent, Scope::Project, temp.path()).is_err());
            assert_eq!(fs::read_to_string(path).unwrap(), "{invalid");
        }
    }

    fn managed_hook_count(config: &Value, agent: Agent) -> usize {
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
                    .filter(|hook| is_managed_hook(hook, agent))
                    .count()
            })
            .unwrap_or(0)
    }
}
