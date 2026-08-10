# Relay Agent Adapter

这个目录包含 Relay 使用的两个本机辅助程序：

- `relay-agent-adapter` 只读扫描本机 Claude Code 和 ChatGPT 会话，返回会话列表、预览和通用对话结构。
- `relay-session-importer` 只在用户点击导入后，根据分享包新增一条 Claude Code 会话或 ChatGPT 任务。

`relay-agent-adapter` 只负责读取：

- 不写入 `~/.claude` 或 `~/.codex`。
- 不运行历史里的工具调用。
- 不读取或发送 Git 仓库内容。
- 不上传数据，也不生成最终的 `.relaypack`。

Git 状态、附件文件、选择分享哪些内容、加密和上传由桌面核心负责。

`relay-session-importer` 不复制发送方的原始会话文件。它只使用分享包中已经允许分享的内容，分配新的会话 ID，并新建会话文件和索引记录。ChatGPT 桌面状态文件存在时，导入器也会把新任务加入置顶列表。已有会话文件不会被覆盖；历史工具记录只用于阅读，不会再次执行。

## 构建和测试

```bash
cd adapter
go test ./...
go build -o bin/relay-agent-adapter ./cmd/relay-agent-adapter
go build -o bin/relay-session-importer ./cmd/relay-session-importer
```

`bin/` 已在仓库根目录的 `.gitignore` 中忽略。桌面程序发布时应为目标架构重新构建，不应提交本机二进制。

## JSONL 协议

适配程序持续读取标准输入，每行是一个请求；每个请求在标准输出产生一行响应。标准输出不会混入日志。

请求：

```json
{"id":"req-1","method":"health","params":{}}
```

成功响应：

```json
{"id":"req-1","ok":true,"result":{}}
```

失败响应：

```json
{"id":"req-1","ok":false,"error":{"code":"invalid_params","message":"session_id is required"}}
```

协议号是 `relay.adapter.v1`。桌面核心连接后应先调用 `health`，检查：

- `protocol == "relay.adapter.v1"`
- `read_only == true`
- `supported_methods` 包含即将调用的方法

支持的方法如下。

### `health`

返回协议号、适配程序版本、只读声明、支持的 Agent 和方法。

### `discover_sessions`

参数：

```json
{
  "agents": ["claude_code", "codex"],
  "claude_home": "/optional/claude/home",
  "codex_home": "/optional/codex/home",
  "limit": 200
}
```

不传 home 时，适配程序遵循 `CLAUDE_CONFIG_DIR`、`CODEX_HOME`，否则读取 `~/.claude`、`~/.codex`。扫描范围是：

- Claude Code：`projects/<project>/*.jsonl`，不把 `subagents/` 当作独立主会话。
- Codex：`sessions/**/*.jsonl` 和 `archived_sessions/**/*.jsonl`。

结果包含 `sessions` 和扫描级 `warnings`。每条会话带 `project_key`、`project_name`、`project_root`、`preview`、消息与工具计数、完整度和警告数。`preview` 取最近一条用户可见的普通文本，不包含工具输入输出、系统提示、隐藏推理或提供方内部记录。界面使用 `project_key` 分组，使用 `project_root` 显示主仓库路径，同时保留 `cwd` 作为该会话实际使用的工作目录。

### `inspect_session`

参数：

```json
{
  "agent": "claude_code",
  "session_id": "session-id",
  "preview_limit": 40,
  "claude_home": "/optional/claude/home",
  "codex_home": "/optional/codex/home"
}
```

`session_id` 由适配程序在对应 home 下定位。接口不接受任意 `source_path`，避免桌面调用方借它读取会话目录外的文件。

### `export_session`

参数和 `inspect_session` 相同（不使用 `preview_limit`），返回完整通用对话。

返回值的 schema 是 `relay.adapter.handoff-preview.v1`，它是本机的会话中间结构，不是最终分享包。它包含本机绝对 `cwd` 和 `source_path`，方便桌面程序检查来源；桌面核心生成正式分享包之前必须按公开 schema 重新组装，并去除或改写本机路径。

## 会话结构

消息结构保留：

- 原生消息 ID、父消息 ID、turn ID、时间、角色和 phase（有值时）。
- `text` 文本块。
- `tool_call` 的 `call_id`、名称、输入和状态。
- `tool_result` 的 `call_id`、输出、状态和错误标记。
- `asset_ref` 元数据；文件内容由桌面核心另行收集。
- 已确认属于项目的 Claude 附件以 `source_context` 保存。
- 无法解释但需要保留其存在的新记录使用 `unsupported` 块，只保存经过检查的原类型、`mapping.status: "unmapped"` 和固定的 `safe_summary`，不复制未知载荷。

每条工具调用和结果都带：

```json
{"replay_policy":"never"}
```

这表示它们只是历史记录，接收方不得自动执行。

## 内容分类和完整度

当前分类值：

- `user_visible`：用户消息、助手公开回复、工具调用与结果、用户附件引用。
- `project_owned`：已确认来自项目的文件上下文。

适配程序不会输出私有推理、加密内容、开发者/系统提示、Codex world state、Claude file-history snapshot、权限/模式记录和内部协作消息。

完整度有三种：

- `complete`：可见记录均已识别，工具调用与结果能够关联。
- `partial`：有损坏 JSON、未知记录、未知内容块或未配对的工具记录。
- `metadata_only`：找到了会话，但没有可输出消息。

发现新格式时采用保守策略：返回警告并标成 `partial`，不把未知载荷直接复制进分享数据。

## 读取上限

原生会话文件是不受信任的输入。当前固定上限如下：

- 单个会话文件最多 1 GiB。超过后返回 `session_too_large`。
- 单条 JSONL 记录最多 32 MiB。超过后跳过该行、返回 `line_too_large`，并继续读取下一行。
- 工具返回中的内嵌图片、音频和超过 1 MiB 的单个字符串不会原样保留。适配程序会改写成固定的省略说明和原始字节数，继续保留工具名称、调用关系及其他可读字段。
- JSON 最多嵌套 64 层。超过后跳过该行并返回 `json_too_deep`；Codex 工具调用中以字符串保存的 JSON 参数也会单独检查，超过后用固定省略说明替换并返回 `embedded_json_too_deep`。
- 文件末尾没有换行的残缺记录不会尝试导出，返回 `truncated_final_line`。

这些错误和警告只包含固定说明、行号和经过检查的记录类型，不包含原始正文、token 或完整 JSON。

## 合成测试数据

`testdata/` 只包含人为编写的 Claude/Codex 历史，其中故意加入：

- 损坏的 JSON 行。
- 未知记录和未知内容块。
- 未配对的工具结果。
- 工具调用缺少结果。
- 私有推理、加密内容、provider 内部状态。
- 当前 Codex rollout 格式和早期无 envelope 格式。
- Claude `subagents/` 辅助目录、sidechain 消息、Claude/Codex 分支标识和 Codex 副 Agent 记录。
- 超大行、超大文件、过深 JSON 和末尾残缺行。

测试会检查可见消息和工具调用仍在、敏感标记不会出现在导出结果中、四个协议方法均可调用，并用文件列表、大小、修改时间和 SHA-256 确认读取和导出没有修改 provider home。
