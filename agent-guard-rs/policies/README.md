# Policy Presets

Agent Guard ships policy examples for different trust boundaries:

| Policy | Default mode | Intended use |
| --- | --- | --- |
| `default.yaml` | `observe` | Minimal, low-noise starting point |
| `balanced.yaml` | `observe` | Developer workstations with only critical rules enforced |
| `strict.yaml` | `enforce` | Sensitive repositories and constrained workspaces |
| `ci.yaml` | `enforce` | Non-interactive build and test jobs |

Copy a preset to `.agent-guard/policy.yaml`, review every pattern for the repository, then validate it:

```bash
agent-guard policy validate .agent-guard/policy.yaml
```

The presets are examples, not universal security boundaries. Tool names and argument shapes vary across Agents and versions. Test representative events with `agent-guard policy explain` before enabling enforcement.

## Rule Scoping

Use `agents` to apply a rule only to named normalized Agent kinds, or `exclude_agents` to omit them. Names are case-sensitive and currently include `claude`, `codebuddy`, `codex`, `cursor`, `gemini`, `kiro`, `opencode`, `qoder`, and `qwen`.

```yaml
agents: [claude, codex]
exclude_agents: [opencode]
```

Do not put the same Agent in both fields. Empty lists mean no restriction.

## Exceptions And Metadata

`unless` suppresses a rule when its expression matches. This is clearer than embedding a large negated expression in `when`:

```yaml
when:
  field: tool.name
  in: [Bash, shell]
unless:
  field: tool.input.command
  regex: '^git (status|diff)(\s+[-\w./]+)*$'
```

Optional `description` and `tags` fields document intent and support future policy tooling. They do not affect decisions.
