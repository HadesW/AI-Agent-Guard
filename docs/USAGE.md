# Agent Guard RS 使用手册

## 构建

需要 Rust 1.75 或更高版本。

```bash
cargo build --release
install -m 0755 target/release/agent-guard ~/.local/bin/agent-guard
```

确认目标 Agent 是否存在：

```bash
agent-guard detect
```

## 安装集成

项目级集成只对指定工作区生效：

```bash
agent-guard install --agent claude --scope project --workspace .
agent-guard install --agent codebuddy --scope project --workspace .
agent-guard install --agent codex --scope project --workspace .
agent-guard install --agent cursor --scope project --workspace .
agent-guard install --agent gemini --scope project --workspace .
agent-guard install --agent kiro --scope project --workspace .
agent-guard install --agent opencode --scope project --workspace .
agent-guard install --agent qoder --scope project --workspace .
agent-guard install --agent qwen --scope project --workspace .
```

用户级集成对当前用户生效：

```bash
agent-guard install --agent claude --scope user --workspace .
agent-guard install --agent codebuddy --scope user --workspace .
agent-guard install --agent codex --scope user --workspace .
agent-guard install --agent cursor --scope user --workspace .
agent-guard install --agent gemini --scope user --workspace .
agent-guard install --agent kiro --scope user --workspace .
agent-guard install --agent opencode --scope user --workspace .
agent-guard install --agent qoder --scope user --workspace .
agent-guard install --agent qwen --scope user --workspace .
```

安装时会把当前 `agent-guard` 二进制的绝对路径写入 Hook 或插件，因此移动或删除二进制后应重新安装。检查状态：

```bash
agent-guard status --workspace .
```

项目级 Hook 只有在目标 Agent 信任该工作区后才会执行。当前配置位置：

| Agent | 项目级 | 用户级 |
| --- | --- | --- |
| Claude Code | `.claude/settings.json` | `~/.claude/settings.json` |
| CodeBuddy Code | `.codebuddy/settings.json` | `~/.codebuddy/settings.json` |
| Codex CLI | `.codex/hooks.json` | `$CODEX_HOME/hooks.json` 或 `~/.codex/hooks.json` |
| Cursor | `.cursor/hooks.json` | `~/.cursor/hooks.json` |
| Gemini CLI | `.gemini/settings.json` | `~/.gemini/settings.json` |
| Kiro CLI | `.kiro/hooks/agent-guard.json` | `~/.kiro/hooks/agent-guard.json` |
| OpenCode | `.opencode/plugins/agent-guard.js` | `~/.config/opencode/plugins/agent-guard.js` |
| Qoder | `.qoder/settings.json` | `~/.qoder/settings.json` |
| Qwen Code CLI | `.qwen/settings.json` | `~/.qwen/settings.json` |

CLI 同时接受 `codebuddy-code`、`kiro-cli`、`qoder-cli`、`qwen-code` 和 `qwen-code-cli` 别名。Qoder CN/Work、WorkBuddy、MiMo 等变体目前没有经过验证的前置阻断契约，因此不宣称受 enforce 保护。

## 配置策略

在项目中创建 `.agent-guard/policy.yaml`。工具名称由各 Agent 定义，以下规则覆盖当前九类 Agent 常见的 shell 工具名称：

```yaml
version: "1"
settings:
  default_mode: observe

rules:
  - id: block-shell
    title: Block shell commands
    mode: enforce
    severity: high
    events: [pre_tool_use]
    when:
      field: tool.name
      in: [Bash, bash, shell, Shell, run_shell_command]
    action: deny
    message: Shell commands are disabled in this workspace.
```

如果省略规则的 `mode`，则使用 `settings.default_mode`。推荐先用 `observe` 检查误报，再把选定规则改为 `enforce`。

仓库提供四个可调整的起点：

| 文件 | 默认模式 | 用途 |
| --- | --- | --- |
| `policies/default.yaml` | `observe` | 最小、低噪声默认规则 |
| `policies/balanced.yaml` | `observe` | 开发工作站，仅强制阻断关键风险 |
| `policies/strict.yaml` | `enforce` | 敏感仓库和受限工作区 |
| `policies/ci.yaml` | `enforce` | 非交互构建与测试任务 |

规则可使用以下可选字段：

- `description`、`tags`：记录规则用途和分类，不影响决策。
- `agents`：只对列出的标准 Agent 名称生效。
- `exclude_agents`：跳过列出的标准 Agent 名称。
- `unless`：表达式命中时抑制本规则，用于清晰表达窄范围例外。

```yaml
  - id: shell-with-readonly-exception
    title: Restrict shell commands
    tags: [shell, allowlist]
    agents: [claude, codex]
    mode: enforce
    severity: high
    events: [pre_tool_use]
    when:
      field: tool.name
      in: [Bash, shell]
    unless:
      field: tool.input.command
      regex: '^git (status|diff)(\s+[-\w./]+)*$'
    action: deny
    message: This command is not on the read-only allowlist.
```

Agent 名称区分大小写，当前为 `claude`、`codebuddy`、`codex`、`cursor`、`gemini`、`kiro`、`opencode`、`qoder`、`qwen`。同一名称不能同时出现在 `agents` 和 `exclude_agents` 中。项目策略仍会整体替代用户策略，而不是合并；`unless` 只抑制其所属规则，不能放宽其他规则或策略层。

验证策略：

```bash
agent-guard policy validate .agent-guard/policy.yaml
```

用标准事件或 Claude Hook JSON 进行离线测试：

```bash
agent-guard policy test --event event.json --policy .agent-guard/policy.yaml
agent-guard policy explain --event event.json --policy .agent-guard/policy.yaml
```

决策为 deny 时，这两个命令退出码为 `2`。

## 查看审计

查看最近的规则命中：

```bash
agent-guard audit findings --limit 50
```

导出全部审计事件为 JSONL：

```bash
agent-guard audit export --output audit.jsonl
```

默认数据库位于操作系统的平台数据目录。测试或独立部署时可覆盖目录：

```bash
AGENT_GUARD_DATA_DIR=/var/lib/agent-guard agent-guard status
```

## 卸载

卸载范围必须与安装范围一致：

```bash
agent-guard uninstall --agent claude --scope project --workspace .
agent-guard uninstall --agent codebuddy --scope project --workspace .
agent-guard uninstall --agent codex --scope project --workspace .
agent-guard uninstall --agent cursor --scope project --workspace .
agent-guard uninstall --agent gemini --scope project --workspace .
agent-guard uninstall --agent kiro --scope project --workspace .
agent-guard uninstall --agent opencode --scope project --workspace .
agent-guard uninstall --agent qoder --scope project --workspace .
agent-guard uninstall --agent qwen --scope project --workspace .
```

卸载只移除 Agent Guard 管理的配置。OpenCode 同名插件如果不含 Agent Guard 所有权标记，会被保留。

## 排障

- `workspace ... does not exist`：`--workspace` 必须指向已存在目录。
- 项目 Hook 不执行：先在对应 Agent 中信任工作区；Codex 还可能要求确认新 Hook 的指纹。
- Qwen 配置包含 JSONC 注释：当前安装器会拒绝修改，需先转换为有效 JSON，避免静默丢失注释。
- OpenCode 未加载插件：确认文件位于 `.opencode/plugins/agent-guard.js` 或 `~/.config/opencode/plugins/agent-guard.js`，然后重启 OpenCode。
- 规则命中但未阻断：检查规则或 `settings.default_mode` 是否为 `observe`；实际阻断需要 `mode: enforce`。
- 安装后提示找不到二进制：重新把二进制放入固定路径并再次运行安装命令。
- 查看审计写入位置和安装状态：运行 `agent-guard status --workspace .`。
