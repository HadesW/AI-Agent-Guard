use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn validates_bundled_policies() {
    for (path, count) in [
        ("policies/default.yaml", 3),
        ("policies/balanced.yaml", 10),
        ("policies/strict.yaml", 9),
        ("policies/ci.yaml", 6),
    ] {
        Command::cargo_bin("agent-guard")
            .unwrap()
            .args(["policy", "validate", path])
            .assert()
            .success()
            .stdout(predicate::str::contains(format!(
                "valid policy: {count} rules"
            )));
    }
}

#[test]
fn dispatch_denies_with_enforce_policy_and_audits() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let policy = temp.path().join("policy.yaml");
    fs::write(
        &policy,
        r#"
version: "1"
rules:
  - id: no-shell
    title: No shell
    mode: enforce
    severity: critical
    events: [pre_tool_use]
    when: { field: tool.name, eq: Bash }
    action: deny
    message: shell blocked
"#,
    )
    .unwrap();
    Command::cargo_bin("agent-guard")
        .unwrap()
        .env("AGENT_GUARD_DATA_DIR", &data)
        .args([
            "hook",
            "dispatch",
            "--agent",
            "claude",
            "--workspace",
            temp.path().to_str().unwrap(),
            "--policy",
            policy.to_str().unwrap(),
        ])
        .write_stdin(
            r#"{"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"Bash","tool_input":{"command":"date"}}"#,
        )
        .assert()
        .success()
        .stdout(predicate::str::contains("\"permissionDecision\":\"deny\""));
    assert!(data.join("audit.db").exists());
}

#[test]
fn installs_and_uninstalls_opencode_project_plugin() {
    let temp = TempDir::new().unwrap();

    Command::cargo_bin("agent-guard")
        .unwrap()
        .args([
            "install",
            "--agent",
            "opencode",
            "--scope",
            "project",
            "--workspace",
            temp.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let plugin = temp.path().join(".opencode/plugins/agent-guard.js");
    let source = fs::read_to_string(&plugin).unwrap();
    assert!(source.contains("agent-guard-opencode"));
    assert!(source.contains("export const AgentGuardPlugin"));

    Command::cargo_bin("agent-guard")
        .unwrap()
        .args([
            "uninstall",
            "--agent",
            "opencode",
            "--scope",
            "project",
            "--workspace",
            temp.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(!plugin.exists());
}

#[test]
fn opencode_dispatch_denies_with_exit_two() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let policy = temp.path().join("policy.yaml");
    fs::write(
        &policy,
        r#"
version: "1"
rules:
  - id: no-shell
    title: No shell
    mode: enforce
    severity: critical
    events: [pre_tool_use]
    when: { field: tool.name, eq: bash }
    action: deny
    message: shell blocked by policy
"#,
    )
    .unwrap();

    Command::cargo_bin("agent-guard")
        .unwrap()
        .env("AGENT_GUARD_DATA_DIR", &data)
        .args([
            "hook",
            "dispatch",
            "--agent",
            "opencode",
            "--workspace",
            temp.path().to_str().unwrap(),
            "--policy",
            policy.to_str().unwrap(),
        ])
        .write_stdin(
            r#"{"hook_event_name":"tool.execute.before","input":{"tool":"bash","sessionID":"s1"},"output":{"args":{"command":"date"}},"cwd":"/workspace"}"#,
        )
        .assert()
        .code(2)
        .stderr(predicate::str::contains("shell blocked by policy"));

    assert!(data.join("audit.db").exists());
}

#[test]
fn installs_and_uninstalls_additional_project_hooks() {
    let temp = TempDir::new().unwrap();
    let cases = [
        ("codebuddy", ".codebuddy/settings.json"),
        ("codex", ".codex/hooks.json"),
        ("cursor", ".cursor/hooks.json"),
        ("gemini", ".gemini/settings.json"),
        ("qoder", ".qoder/settings.json"),
        ("qwen", ".qwen/settings.json"),
    ];

    for (agent, relative_path) in cases {
        Command::cargo_bin("agent-guard")
            .unwrap()
            .args([
                "install",
                "--agent",
                agent,
                "--scope",
                "project",
                "--workspace",
                temp.path().to_str().unwrap(),
            ])
            .assert()
            .success();

        let path = temp.path().join(relative_path);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(&format!("hook dispatch --agent {agent} --workspace")));

        Command::cargo_bin("agent-guard")
            .unwrap()
            .args([
                "uninstall",
                "--agent",
                agent,
                "--scope",
                "project",
                "--workspace",
                temp.path().to_str().unwrap(),
            ])
            .assert()
            .success();

        assert!(!fs::read_to_string(path)
            .unwrap()
            .contains(&format!("hook dispatch --agent {agent} --workspace")));
    }
}

#[test]
fn installs_and_uninstalls_kiro_owned_hook_file() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join(".kiro/hooks/agent-guard.json");

    Command::cargo_bin("agent-guard")
        .unwrap()
        .args([
            "install",
            "--agent",
            "kiro",
            "--scope",
            "project",
            "--workspace",
            temp.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let config: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(config["version"], "v1");
    assert_eq!(config["hooks"][0]["trigger"], "PreToolUse");

    Command::cargo_bin("agent-guard")
        .unwrap()
        .args([
            "uninstall",
            "--agent",
            "kiro",
            "--scope",
            "project",
            "--workspace",
            temp.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(!path.exists());
}

#[test]
fn additional_agents_deny_with_exit_two() {
    let temp = TempDir::new().unwrap();
    let policy = temp.path().join("policy.yaml");
    fs::write(
        &policy,
        r#"
version: "1"
rules:
  - id: no-shell
    title: No shell
    mode: enforce
    severity: critical
    events: [pre_tool_use]
    when: { field: tool.name, in: [shell, Shell, run_shell_command] }
    action: deny
    message: shell blocked by policy
"#,
    )
    .unwrap();
    let cases = [
        (
            "codebuddy",
            r#"{"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"Shell","tool_input":{"command":"date"}}"#,
        ),
        (
            "codex",
            r#"{"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"shell","tool_input":{"command":"date"}}"#,
        ),
        (
            "cursor",
            r#"{"hook_event_name":"preToolUse","conversation_id":"s1","tool_name":"Shell","tool_input":{"command":"date"}}"#,
        ),
        (
            "gemini",
            r#"{"hook_event_name":"BeforeTool","session_id":"s1","tool_name":"run_shell_command","tool_input":{"command":"date"}}"#,
        ),
        (
            "kiro",
            r#"{"hook_event_name":"preToolUse","session_id":"s1","tool_name":"Shell","tool_input":{"command":"date"}}"#,
        ),
        (
            "qoder",
            r#"{"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"Shell","tool_input":{"command":"date"}}"#,
        ),
        (
            "qwen",
            r#"{"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"run_shell_command","tool_input":{"command":"date"}}"#,
        ),
    ];

    for (agent, payload) in cases {
        Command::cargo_bin("agent-guard")
            .unwrap()
            .env(
                "AGENT_GUARD_DATA_DIR",
                temp.path().join(format!("data-{agent}")),
            )
            .args([
                "hook",
                "dispatch",
                "--agent",
                agent,
                "--workspace",
                temp.path().to_str().unwrap(),
                "--policy",
                policy.to_str().unwrap(),
            ])
            .write_stdin(payload)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("shell blocked by policy"));
    }
}
