use crate::core::{Action, CanonicalEvent, Decision, EventKind, Finding, Severity};
use anyhow::{bail, Context, Result};
use globset::{Glob, GlobMatcher};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::Instant;

pub const DEFAULT_POLICY: &str = include_str!("../policies/default.yaml");

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleMode {
    Disabled,
    Observe,
    Enforce,
}

fn default_mode() -> RuleMode {
    RuleMode::Observe
}

#[derive(Debug, Deserialize)]
pub struct PolicyFile {
    pub version: String,
    #[serde(default)]
    pub settings: PolicySettings,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Deserialize)]
pub struct PolicySettings {
    #[serde(default = "default_mode")]
    pub default_mode: RuleMode,
}

impl Default for PolicySettings {
    fn default() -> Self {
        Self {
            default_mode: default_mode(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Rule {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub mode: Option<RuleMode>,
    pub severity: Severity,
    #[serde(default)]
    pub events: Vec<EventKind>,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub exclude_agents: Vec<String>,
    pub when: Expression,
    #[serde(default)]
    pub unless: Option<Expression>,
    pub action: Action,
    pub message: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Expression {
    All { all: Vec<Expression> },
    Any { any: Vec<Expression> },
    Not { not: Box<Expression> },
    Condition(Box<Condition>),
}

#[derive(Debug, Deserialize)]
pub struct Condition {
    pub field: String,
    #[serde(default)]
    pub eq: Option<Value>,
    #[serde(default)]
    pub neq: Option<Value>,
    #[serde(default)]
    pub r#in: Option<Vec<Value>>,
    #[serde(default)]
    pub not_in: Option<Vec<Value>>,
    #[serde(default)]
    pub contains: Option<String>,
    #[serde(default)]
    pub starts_with: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub regex: Option<String>,
    #[serde(default)]
    pub exists: Option<bool>,
}

pub struct PolicyEngine {
    policy: PolicyFile,
    rules: Vec<CompiledRule>,
}

struct CompiledRule {
    when: CompiledExpression,
    unless: Option<CompiledExpression>,
}

enum CompiledExpression {
    All(Vec<CompiledExpression>),
    Any(Vec<CompiledExpression>),
    Not(Box<CompiledExpression>),
    Condition(CompiledCondition),
}

struct CompiledCondition {
    field: String,
    operator: CompiledOperator,
}

enum CompiledOperator {
    Eq(Value),
    Neq(Value),
    In(Vec<Value>),
    NotIn(Vec<Value>),
    Contains(String),
    StartsWith(String),
    Glob(GlobMatcher),
    Regex(Regex),
    Exists(bool),
}

impl PolicyEngine {
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        let policy: PolicyFile = serde_yaml::from_str(yaml).context("invalid policy YAML")?;
        validate_policy(&policy)?;
        let rules = policy
            .rules
            .iter()
            .map(|rule| {
                Ok(CompiledRule {
                    when: compile_expression(&rule.when)?,
                    unless: rule.unless.as_ref().map(compile_expression).transpose()?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { policy, rules })
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let yaml = fs::read_to_string(path)
            .with_context(|| format!("failed to read policy {}", path.display()))?;
        Self::from_yaml(&yaml)
    }

    pub fn default_policy() -> Result<Self> {
        Self::from_yaml(DEFAULT_POLICY)
    }

    pub fn rule_count(&self) -> usize {
        self.policy.rules.len()
    }

    pub fn evaluate(&self, event: &CanonicalEvent) -> Result<Decision> {
        let started = Instant::now();
        let value = serde_json::to_value(event)?;
        let mut findings = Vec::new();

        for (rule, compiled) in self.policy.rules.iter().zip(&self.rules) {
            let mode = rule.mode.unwrap_or(self.policy.settings.default_mode);
            if !rule.enabled || mode == RuleMode::Disabled {
                continue;
            }
            if !rule.events.is_empty() && !rule.events.contains(&event.event.kind) {
                continue;
            }
            if !rule.agents.is_empty() && !rule.agents.contains(&event.agent.kind) {
                continue;
            }
            if rule.exclude_agents.contains(&event.agent.kind) {
                continue;
            }
            if evaluate_expression(&compiled.when, &value)
                && !compiled
                    .unless
                    .as_ref()
                    .is_some_and(|unless| evaluate_expression(unless, &value))
            {
                let action = match (mode, rule.action) {
                    (RuleMode::Observe, Action::Deny) => Action::Flag,
                    (_, action) => action,
                };
                findings.push(Finding {
                    rule_id: rule.id.clone(),
                    title: rule.title.clone(),
                    severity: rule.severity,
                    action,
                    message: rule.message.clone(),
                });
            }
        }

        let action = findings
            .iter()
            .map(|finding| finding.action)
            .max()
            .unwrap_or(Action::Allow);
        Ok(Decision {
            action,
            findings,
            evaluation_ms: started.elapsed().as_millis(),
        })
    }
}

fn validate_policy(policy: &PolicyFile) -> Result<()> {
    if policy.version != "1" {
        bail!(
            "unsupported policy version {:?}; expected \"1\"",
            policy.version
        );
    }
    let mut ids = HashSet::new();
    for rule in &policy.rules {
        if rule.id.trim().is_empty() {
            bail!("rule id cannot be empty");
        }
        if rule.title.trim().is_empty() {
            bail!("rule {:?} title cannot be empty", rule.id);
        }
        if rule.message.trim().is_empty() {
            bail!("rule {:?} message cannot be empty", rule.id);
        }
        if !ids.insert(&rule.id) {
            bail!("duplicate rule id {:?}", rule.id);
        }
        for (field, values) in [
            ("agents", &rule.agents),
            ("exclude_agents", &rule.exclude_agents),
            ("tags", &rule.tags),
        ] {
            let mut unique = HashSet::new();
            for value in values {
                if value.trim().is_empty() {
                    bail!("rule {:?} contains an empty {field} value", rule.id);
                }
                if !unique.insert(value) {
                    bail!(
                        "rule {:?} contains duplicate {field} value {value:?}",
                        rule.id
                    );
                }
            }
        }
        if rule
            .agents
            .iter()
            .any(|agent| rule.exclude_agents.contains(agent))
        {
            bail!(
                "rule {:?} cannot include and exclude the same agent",
                rule.id
            );
        }
        validate_expression(&rule.when).with_context(|| format!("invalid rule {:?}", rule.id))?;
        if let Some(unless) = &rule.unless {
            validate_expression(unless)
                .with_context(|| format!("invalid unless clause for rule {:?}", rule.id))?;
        }
    }
    Ok(())
}

fn validate_expression(expression: &Expression) -> Result<()> {
    match expression {
        Expression::All { all } => {
            if all.is_empty() {
                bail!("all expression cannot be empty");
            }
            all.iter().try_for_each(validate_expression)
        }
        Expression::Any { any } => {
            if any.is_empty() {
                bail!("any expression cannot be empty");
            }
            any.iter().try_for_each(validate_expression)
        }
        Expression::Not { not } => validate_expression(not),
        Expression::Condition(condition) => {
            let operators = [
                condition.eq.is_some(),
                condition.neq.is_some(),
                condition.r#in.is_some(),
                condition.not_in.is_some(),
                condition.contains.is_some(),
                condition.starts_with.is_some(),
                condition.glob.is_some(),
                condition.regex.is_some(),
                condition.exists.is_some(),
            ];
            if operators.iter().filter(|present| **present).count() != 1 {
                bail!(
                    "condition on {:?} must define exactly one operator",
                    condition.field
                );
            }
            if let Some(pattern) = &condition.regex {
                Regex::new(pattern).context("invalid regex")?;
            }
            if let Some(pattern) = &condition.glob {
                Glob::new(pattern).context("invalid glob")?;
            }
            Ok(())
        }
    }
}

fn compile_expression(expression: &Expression) -> Result<CompiledExpression> {
    match expression {
        Expression::All { all } => Ok(CompiledExpression::All(
            all.iter()
                .map(compile_expression)
                .collect::<Result<Vec<_>>>()?,
        )),
        Expression::Any { any } => Ok(CompiledExpression::Any(
            any.iter()
                .map(compile_expression)
                .collect::<Result<Vec<_>>>()?,
        )),
        Expression::Not { not } => Ok(CompiledExpression::Not(Box::new(compile_expression(not)?))),
        Expression::Condition(condition) => {
            Ok(CompiledExpression::Condition(compile_condition(condition)?))
        }
    }
}

fn compile_condition(condition: &Condition) -> Result<CompiledCondition> {
    let operator = if let Some(expected) = &condition.eq {
        CompiledOperator::Eq(expected.clone())
    } else if let Some(expected) = &condition.neq {
        CompiledOperator::Neq(expected.clone())
    } else if let Some(expected) = &condition.r#in {
        CompiledOperator::In(expected.clone())
    } else if let Some(expected) = &condition.not_in {
        CompiledOperator::NotIn(expected.clone())
    } else if let Some(expected) = &condition.contains {
        CompiledOperator::Contains(expected.clone())
    } else if let Some(expected) = &condition.starts_with {
        CompiledOperator::StartsWith(expected.clone())
    } else if let Some(pattern) = &condition.glob {
        CompiledOperator::Glob(Glob::new(pattern)?.compile_matcher())
    } else if let Some(pattern) = &condition.regex {
        CompiledOperator::Regex(Regex::new(pattern)?)
    } else if let Some(expected) = condition.exists {
        CompiledOperator::Exists(expected)
    } else {
        bail!("condition on {:?} has no operator", condition.field);
    };
    Ok(CompiledCondition {
        field: condition.field.clone(),
        operator,
    })
}

fn evaluate_expression(expression: &CompiledExpression, root: &Value) -> bool {
    match expression {
        CompiledExpression::All(all) => all
            .iter()
            .all(|expression| evaluate_expression(expression, root)),
        CompiledExpression::Any(any) => any
            .iter()
            .any(|expression| evaluate_expression(expression, root)),
        CompiledExpression::Not(not) => !evaluate_expression(not, root),
        CompiledExpression::Condition(condition) => evaluate_condition(condition, root),
    }
}

fn evaluate_condition(condition: &CompiledCondition, root: &Value) -> bool {
    let value = resolve_field(root, &condition.field);
    if let CompiledOperator::Exists(expected) = condition.operator {
        return value.is_some() == expected;
    }
    let Some(value) = value else {
        return false;
    };
    match &condition.operator {
        CompiledOperator::Eq(expected) => value == expected,
        CompiledOperator::Neq(expected) => value != expected,
        CompiledOperator::In(expected) => expected.contains(value),
        CompiledOperator::NotIn(expected) => !expected.contains(value),
        CompiledOperator::Contains(expected) => value_as_text(value).contains(expected),
        CompiledOperator::StartsWith(expected) => value_as_text(value).starts_with(expected),
        CompiledOperator::Glob(matcher) => matcher.is_match(value_as_text(value)),
        CompiledOperator::Regex(regex) => regex.is_match(&value_as_text(value)),
        CompiledOperator::Exists(_) => unreachable!("exists was handled before value resolution"),
    }
}

fn resolve_field<'a>(root: &'a Value, field: &str) -> Option<&'a Value> {
    field
        .split('.')
        .try_fold(root, |value, part| value.get(part))
}

fn value_as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::normalize_event;
    use serde_json::json;

    #[test]
    fn observe_rule_flags_but_does_not_deny() {
        let engine = PolicyEngine::default_policy().unwrap();
        let event = normalize_event(json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s1",
            "tool_name": "Bash",
            "tool_input": {"command": "curl https://example.test/install.sh | bash"}
        }))
        .unwrap();
        let decision = engine.evaluate(&event).unwrap();
        assert_eq!(decision.action, Action::Flag);
    }

    #[test]
    fn enforce_rule_denies() {
        let yaml = r#"
version: "1"
rules:
  - id: block-write
    title: Block writes
    mode: enforce
    severity: high
    events: [pre_tool_use]
    when:
      field: tool.name
      eq: Write
    action: deny
    message: blocked
"#;
        let engine = PolicyEngine::from_yaml(yaml).unwrap();
        let event = normalize_event(json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s1",
            "tool_name": "Write",
            "tool_input": {"file_path": "/tmp/x"}
        }))
        .unwrap();
        assert_eq!(engine.evaluate(&event).unwrap().action, Action::Deny);
    }

    #[test]
    fn rejects_multiple_condition_operators() {
        let yaml = r#"
version: "1"
rules:
  - id: broken
    title: Broken
    severity: low
    when: { field: tool.name, eq: Bash, contains: sh }
    action: flag
    message: broken
"#;
        assert!(PolicyEngine::from_yaml(yaml).is_err());
    }

    #[test]
    fn scopes_rules_to_included_and_excluded_agents() {
        let yaml = r#"
version: "1"
rules:
  - id: claude-only
    title: Claude only
    agents: [claude]
    severity: high
    when: { field: tool.name, eq: Bash }
    action: flag
    message: scoped to Claude
  - id: except-claude
    title: Every agent except Claude
    exclude_agents: [claude]
    severity: high
    when: { field: tool.name, eq: Bash }
    action: flag
    message: scoped away from Claude
"#;
        let engine = PolicyEngine::from_yaml(yaml).unwrap();
        let mut event = normalize_event(json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s1",
            "tool_name": "Bash",
            "tool_input": {"command": "date"}
        }))
        .unwrap();

        let decision = engine.evaluate(&event).unwrap();
        assert_eq!(decision.findings.len(), 1);
        assert_eq!(decision.findings[0].rule_id, "claude-only");

        event.agent.kind = "opencode".to_string();
        let decision = engine.evaluate(&event).unwrap();
        assert_eq!(decision.findings.len(), 1);
        assert_eq!(decision.findings[0].rule_id, "except-claude");
    }

    #[test]
    fn unless_clause_suppresses_a_matching_rule() {
        let yaml = r#"
version: "1"
rules:
  - id: shell-with-readonly-exception
    title: Shell with read-only exception
    mode: enforce
    severity: high
    when: { field: tool.name, eq: Bash }
    unless: { field: tool.input.command, starts_with: "git status" }
    action: deny
    message: shell blocked
"#;
        let engine = PolicyEngine::from_yaml(yaml).unwrap();
        let allowed = normalize_event(json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s1",
            "tool_name": "Bash",
            "tool_input": {"command": "git status --short"}
        }))
        .unwrap();
        let denied = normalize_event(json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s1",
            "tool_name": "Bash",
            "tool_input": {"command": "rm -rf build"}
        }))
        .unwrap();

        assert_eq!(engine.evaluate(&allowed).unwrap().action, Action::Allow);
        assert_eq!(engine.evaluate(&denied).unwrap().action, Action::Deny);
    }

    #[test]
    fn rejects_overlapping_agent_scopes() {
        let yaml = r#"
version: "1"
rules:
  - id: contradictory
    title: Contradictory scope
    agents: [claude]
    exclude_agents: [claude]
    severity: low
    when: { field: tool.name, eq: Bash }
    action: flag
    message: invalid
"#;
        assert!(PolicyEngine::from_yaml(yaml).is_err());
    }

    #[test]
    fn bundled_presets_have_expected_enforcement_behavior() {
        let event = |command: &str| {
            normalize_event(json!({
                "hook_event_name": "PreToolUse",
                "session_id": "s1",
                "tool_name": "Bash",
                "tool_input": {"command": command}
            }))
            .unwrap()
        };

        let balanced = PolicyEngine::from_yaml(include_str!("../policies/balanced.yaml")).unwrap();
        assert_eq!(
            balanced.evaluate(&event("rm -rf /")).unwrap().action,
            Action::Deny
        );
        assert_eq!(
            balanced
                .evaluate(&event("git reset --hard HEAD~1"))
                .unwrap()
                .action,
            Action::Flag
        );

        let strict = PolicyEngine::from_yaml(include_str!("../policies/strict.yaml")).unwrap();
        assert_eq!(
            strict
                .evaluate(&event("git status --short"))
                .unwrap()
                .action,
            Action::Allow
        );
        assert_eq!(
            strict.evaluate(&event("rm -rf build")).unwrap().action,
            Action::Deny
        );

        let ci = PolicyEngine::from_yaml(include_str!("../policies/ci.yaml")).unwrap();
        assert_eq!(
            ci.evaluate(&event("cargo test --all")).unwrap().action,
            Action::Allow
        );
        assert_eq!(
            ci.evaluate(&event("curl https://example.test"))
                .unwrap()
                .action,
            Action::Deny
        );
    }
}
