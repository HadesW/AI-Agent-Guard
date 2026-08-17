# Agent Guard RS 技术架构

## 目标与边界

Agent Guard RS 通过 AI Coding Agent 的官方 Hook 或插件接口，在本机完成确定性的策略判断和审计。当前实现支持 Claude Code、CodeBuddy Code、Codex CLI、Cursor、Gemini CLI、Kiro CLI、OpenCode、Qoder 与 Qwen Code CLI，不代理模型网络流量，也不依赖远程服务或常驻进程。

## 请求链路

```text
Agent Hook / OpenCode Plugin
             |
             | JSON over stdin
             v
      agent-guard hook dispatch
             |
             v
       Agent Normalizer
             |
             v
       CanonicalEvent
          /      \
         v        v
 Policy Engine   Redaction + SQLite
         |
         v
 Agent-specific decision
```

同步路径由同一个 Rust 二进制完成：适配器把 Agent 原始事件转换为 `CanonicalEvent`，规则引擎计算 `allow`、`flag` 或 `deny`，审计模块以 best-effort 方式记录脱敏事件，最后由适配器按 Agent 协议返回结果。

## 模块

- `src/core.rs`：标准事件、Finding 和 Decision 数据模型。
- `src/claude.rs`：Claude Code 事件归一化、Hook 配置管理和 deny 响应渲染。
- `src/codex.rs`：Codex 生命周期事件归一化和嵌套 Hook 配置管理。
- `src/compatible.rs`：Qoder、CodeBuddy、Qwen 的共享嵌套 Hook 部署和事件归一化。
- `src/cursor.rs`：Cursor 事件归一化和扁平 Hook 配置管理。
- `src/gemini.rs`：Gemini CLI 事件归一化和 settings Hook 配置管理。
- `src/kiro.rs`：Kiro CLI v3 独立 Hook 文件部署和事件归一化。
- `src/opencode.rs`：OpenCode 事件归一化以及 drop-in JavaScript 插件的生成、安装和卸载。
- `src/policy.rs`：YAML 加载、结构验证、正则和 glob 预编译、规则求值。
- `src/audit.rs`：敏感字段脱敏、SQLite 事务写入、Finding 查询和 JSONL 导出。
- `src/paths.rs`：项目策略、用户策略和数据目录解析。
- `src/main.rs`：CLI 和各模块编排。

## Agent 适配

### Claude Code

安装命令只修改 Agent Guard 自己管理的 Hook 条目。Hook 调用隐藏命令 `agent-guard hook dispatch --agent claude`，deny 结果通过 Claude Code 的 JSON 权限响应返回。

### Codex CLI

项目级配置写入 `.codex/hooks.json`，用户级配置写入 `$CODEX_HOME/hooks.json` 或 `~/.codex/hooks.json`。适配器注册 `PreToolUse`、`PermissionRequest`、`PostToolUse` 和 `UserPromptSubmit`，同步前置事件通过退出码 `2` 与 stderr 原因阻断。当前 Codex 生命周期 Hook 默认启用，但未受信任的 Hook 仍需用户确认。

### Cursor

适配器维护带 `version: 1` 的 `.cursor/hooks.json`，注册 `preToolUse` 和 `postToolUse`。前置 Hook 设置 `failClosed: true`，deny 使用退出码 `2`；项目 Hook 仅在受信任工作区执行。

### Gemini CLI

适配器在 `.gemini/settings.json` 中注册 `BeforeTool` 和 `AfterTool` 命令 Hook。deny 使用 Gemini 支持的系统阻断协议，即退出码 `2` 和 stderr 原因。安装过程保留已有设置和其他 Hook。

### Qoder、CodeBuddy 与 Qwen

三者使用相近的嵌套 Hook 配置和 snake_case 事件 payload，因此共享部署与归一化代码，但保留独立路径、事件集合、matcher 和超时单位。Qoder 与 CodeBuddy 的 timeout 使用秒，Qwen 使用毫秒。三者的前置 deny 均通过退出码 `2` 和 stderr 返回。

### Kiro CLI

Kiro CLI v3 使用 `.kiro/hooks/*.json` 独立配置。Agent Guard 拥有 `agent-guard.json` 整个文件，写入 `version: "v1"` 及 `trigger`/`action` 条目；若同名文件不含完整所有权标记，安装和卸载都会拒绝修改。

### OpenCode

安装命令生成 `.opencode/plugins/agent-guard.js` 或 `~/.config/opencode/plugins/agent-guard.js`。插件使用 OpenCode 的命名导出形式注册 `tool.execute.before` 和 `tool.execute.after`，并把 `output.args` 作为最终工具参数发送给 Rust 进程。

前置调用得到退出码 `2` 时，JavaScript 插件抛出包含 stderr 原因的异常，从而阻断执行。其他非零退出码被视为 Guard 故障并 fail-open，避免安装或运行问题中断所有工具调用；故障会输出到 OpenCode 进程的 stderr。

插件文件包含所有权标记。卸载只删除带该标记的文件，不覆盖或删除用户创建的同名插件。写入使用同目录临时文件加 rename，保证原子替换。

## 规则语义

规则按声明顺序求值，所有命中都会生成 Finding，最终动作取最高优先级：`allow < flag < deny`。

- `observe`：规则中的 `deny` 降级为 `flag`，仅审计。
- `enforce`：保留规则动作，可实际阻断。
- `disabled`：跳过规则。

支持 `all`、`any`、`not` 组合，以及 `eq`、`neq`、`in`、`not_in`、`contains`、`starts_with`、`glob`、`regex` 和 `exists` 条件。规则可通过 `agents`、`exclude_agents` 限定 Agent，通过 `unless` 表达规则局部例外，并用 `description`、`tags` 携带不影响决策的元数据。作用域和事件类型会在表达式求值前过滤；`when` 命中且 `unless` 未命中时才生成 Finding。

项目策略 `.agent-guard/policy.yaml` 优先于平台用户配置目录中的 `policy.yaml`；两者都不存在时使用编译进二进制的默认策略。当前优先级表示整体选取，不会合并多个策略层，因此项目策略可能替代用户策略；不可放宽的分层合并仍是后续加固项。

## 数据与故障策略

- Hook stdin 限制为 1 MiB。
- 审计库存储在平台数据目录的 `audit.db`，可用 `AGENT_GUARD_DATA_DIR` 覆盖。
- 名称包含 `password`、`secret`、`token`、`authorization`、`api_key` 等凭据标识的字段写盘前会脱敏。
- 审计失败不会改变策略决策，只输出 warning。
- 策略解析或求值失败会使 dispatcher 返回错误；各 Agent 按自身 Hook 故障协议处理。Cursor 前置 Hook 显式配置为 fail-closed，其余集成默认遵循 Agent 行为。
- 默认规则处于 `observe`，用户必须明确启用 `enforce` 才会阻断。

## 扩展方式

新增 Agent 时应实现三项能力：原始事件到 `CanonicalEvent` 的归一化、安全且幂等的部署管理，以及 Agent 原生的决策返回协议。规则引擎和审计格式不应包含 Agent 特有逻辑。
