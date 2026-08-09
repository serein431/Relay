# Adapter 进程协议

## 用途

`relay-agent-adapter` 是 Relay Desktop 随应用发布的 Go sidecar。它只读扫描 Claude Code 和 ChatGPT 桌面应用的本地会话，把频繁变化的原生 JSONL 转成稳定的 `relay.adapter.v1` 数据。

面向用户时应用名称统一为 ChatGPT。为兼容已有记录，协议仍使用 provider 值 `codex`、参数 `codex_home` 和目录 `~/.codex`；`codex://`、`com.openai.codex`、Codex CLI 以及 Schema 技术值也继续保留原名。

Adapter 不负责 Git、压缩、加密、上传、解密、worktree 恢复，也不启动 Claude Code 或 ChatGPT。它返回的绝对路径和 `raw` 扩展字段只能在本机使用，不能直接进入分享包。

当前 Adapter 已实现 `health`、`discover_sessions`、`inspect_session` 和 `export_session`。桌面调用层会调用 `health`、`discover_sessions` 和 `export_session`；`inspect_session` 当前只在 Adapter 协议和测试中使用。

## 启动与传输

桌面端每次请求启动一个 Adapter 进程：

```text
Relay Desktop --stdin JSONL--> relay-agent-adapter --stdout JSONL--> Relay Desktop
```

每行是一个完整 JSON 对象，UTF-8 编码，以 `\n` 结束。stdout 只用于协议响应；日志只能写 stderr。一个进程当前只处理一个请求，然后退出。

请求格式：

```json
{
  "id": "relay-123",
  "method": "discover_sessions",
  "params": {
    "limit": 250,
    "agents": ["claude_code", "codex"]
  }
}
```

成功响应：

```json
{
  "id": "relay-123",
  "ok": true,
  "result": {}
}
```

错误响应：

```json
{
  "id": "relay-123",
  "ok": false,
  "error": {
    "code": "invalid_params",
    "message": "limit must be between 1 and 1000"
  }
}
```

`id` 必须原样返回。成功响应必须有 `result`，失败响应必须有 `error`。一次请求只能输出一个响应对象；输出多行、空输出、无法解析的 JSON 或不匹配的 `id` 都是协议错误。

## 进程限制

Rust 调用层已经设置以下界限：

| 命令 | 超时 | stdout 上限 | stderr 上限 |
| --- | ---: | ---: | ---: |
| `health` | 5 秒 | 1 MiB | 128 KiB |
| `discover_sessions` | 30 秒 | 32 MiB | 128 KiB |
| `export_session` | 30 秒 | 64 MiB | 128 KiB |

`inspect_session` 当前没有单独的 Rust 调用入口；若以后接入，必须单独设置超时和输出上限。输出超过上限、进程超时或异常退出时，Rust 将丢弃结果，不使用截断 JSON。

Adapter 解析单个会话时也应限制：单行长度、单个文件大小、总辅助文件大小、消息数、块数和递归深度。遇到上限时返回 `partial` 和明确 warning，不得默默截断后标记为 `complete`。

## 可执行文件查找

Rust 按以下顺序查找 Adapter：

1. 环境变量 `RELAY_AGENT_ADAPTER` 指向的可执行文件。
2. 构建时设置的 Adapter 路径。
3. 应用二进制目录、`Contents/Resources` 和开发目录中的候选文件。
4. `PATH` 中的 `relay-agent-adapter`。

环境变量路径主要用于开发测试。正式应用应使用签名并随应用发布的 sidecar，避免运行 PATH 中同名的未知程序。发布构建在启动后还应校验签名或已知摘要。

## 命令

### `health`

状态：Adapter 和桌面调用层均已接入。

参数为空对象。示例结果：

```json
{
  "protocol": "relay.adapter.v1",
  "schema": "relay.adapter.v1",
  "version": "0.1.0",
  "adapter_version": "0.1.0",
  "handoff_preview_schema": "relay.adapter.handoff-preview.v1",
  "read_only": true,
  "supported_agents": ["claude_code", "codex"],
  "supported_methods": [
    "health",
    "discover_sessions",
    "inspect_session",
    "export_session"
  ]
}
```

桌面端必须检查 `protocol`、主版本、`read_only` 和所需命令。不能因为进程能启动就认为兼容。

### `discover_sessions`

状态：Adapter 和 Rust 调用层均已接入，Claude Code 与 `codex` provider 扫描器已有 fixture 测试。

参数：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `limit` | integer | 可选，1 到 1000 |
| `agents` | string[] | 可选，只允许 `claude_code`、`codex` |
| `claude_home` | string | 可选，开发或测试用的既有目录 |
| `codex_home` | string | 可选，开发或测试用的既有目录 |

Rust 会先把显式 home 解析成现有目录。正式界面不应让接收到的分享包指定这些路径。

结果：

```json
{
  "schema": "relay.adapter.v1",
  "scanned_at": "2026-08-07T08:00:00Z",
  "sessions": [
    {
      "agent": "codex",
      "session_id": "session-id",
      "title": "Implement repository inspection",
      "cwd": "/Users/alice/project-feature",
      "project_key": "git-common-dir:0123456789abcdef",
      "project_name": "project",
      "project_root": "/Users/alice/project",
      "preview": "已经完成登录接口，下一步检查刷新令牌。",
      "created_at": "2026-08-07T07:00:00Z",
      "updated_at": "2026-08-07T07:45:00Z",
      "native_version": "0.142.5",
      "source_path": "/Users/alice/.codex/sessions/example.jsonl",
      "size_bytes": 10240,
      "message_count": 24,
      "tool_call_count": 7,
      "tool_result_count": 7,
      "warning_count": 0,
      "completeness": "complete"
    }
  ],
  "warnings": []
}
```

`source_path` 只是入口文件，不代表一个会话的完整文件集合。附件、Claude Code 的辅助目录和副 Agent 记录应由对应 Adapter 显式发现，桌面端不能根据入口路径自己猜。

存在 Git 仓库时，`project_key` 根据 Git common directory 计算，同一主仓库及其工作树会得到相同标识；没有 Git 仓库时，根据绝对 `cwd` 计算。该字段只用于本机分组，不能进入分享包，也不能用于跨电脑匹配项目。

`project_root` 是 Adapter 已确认的主仓库根目录，用于项目列表显示；`cwd` 始终保留该会话实际使用的工作目录。两者都属于本机路径，不得原样写入正式分享包。`preview` 是最近一条用户可见普通文本的短预览，不得包含工具输入输出、系统提示、隐藏推理或提供方内部记录。

### `inspect_session`

状态：Adapter 已实现；桌面端当前不单独调用此命令。

参数：

```json
{
  "agent": "codex",
  "session_id": "session-id",
  "codex_home": "/Users/alice/.codex",
  "preview_limit": 100
}
```

`agent` 和 `session_id` 必填；`claude_home`、`codex_home` 只用于开发与 fixture；`preview_limit` 只供此命令使用。此命令返回本地预览、警告和完整度。它不得修改或锁定原生文件。Adapter 必须重新在对应 home 中查找 session ID，不能接受调用方直接传入任意入口文件，并且仍需检查 symlink 逃逸。

### `export_session`

状态：Adapter 和桌面调用层均已接入。Rust 用它生成本机预览输入，并在生成包前再次调用以比较 `preview_sha256`。

参数包含必填的 `agent`、`session_id`，以及可选的 `claude_home`、`codex_home`。它返回经过分类和清理的 `relay.adapter.handoff-preview.v1`，作为 Rust 生成正式 `relay.handoff.v1` 的输入。该结果不是可上传包，允许包含本机绝对 cwd 和入口路径，不能直接写进 `handoff.json`。

命令名使用 `export_session`，不使用 `export_handoff`，避免让调用方误以为 Adapter 已经完成 Git 捕获、项目路径改写和分享安全检查。

## Adapter 消息结构

Adapter 的中间消息使用：

```json
{
  "id": "message-id",
  "parent_id": "parent-id",
  "turn_id": "turn-id",
  "branch_id": "branch-id",
  "timestamp": "2026-08-07T07:10:00Z",
  "role": "assistant",
  "phase": "commentary",
  "blocks": []
}
```

支持的块：

| `kind` | 主要字段 | 说明 |
| --- | --- | --- |
| `text` | `text` | 可见文本 |
| `tool_call` | `call_id`, `name`, `input`, `status` | 历史工具调用 |
| `tool_result` | `call_id`, `output`, `status`, `is_error` | 历史工具结果 |
| `asset_ref` | `source` | 指向待收集附件 |
| `source_context` | `source`, `text` | 项目文件或说明来源 |
| `unsupported` | `native_type`, `source` | 无法准确表示的可分享记录 |

每个块都必须有 `classification`。当前允许 Adapter 使用：

- `user_visible`
- `project_owned`
- `provider_internal`
- `private_reasoning`
- `unknown`

进入 `relay.handoff.v1` 前，Rust 必须移除后三种。工具块还必须输出 `replay_policy: "never"`。Adapter 不能把工具调用压成普通文本来绕过这条规则。

## 完整度

Adapter 对会话的检查至少记录：

```json
{
  "status": "partial",
  "total_lines": 100,
  "parsed_lines": 98,
  "damaged_lines": 1,
  "unknown_records": 1,
  "hidden_records": 3,
  "unsupported_blocks": 1,
  "orphan_tool_results": 0,
  "unmatched_tool_calls": 0
}
```

状态含义：

- `complete`：已解析所有可见记录，没有未知、损坏、孤立工具结果或缺失调用。
- `partial`：至少存在上述一种情况。
- `metadata_only`：只找到摘要，没有可用消息。

`hidden_records` 可以是非零而仍为 `complete`，前提是它们明确属于 provider 内部数据或私有推理，并且已省略。发送端生成分享包时仍要把这个数量记入 `export.omissions`。

## 警告

警告格式：

```json
{
  "code": "unknown_record",
  "message": "A user-visible record type is not supported.",
  "line": 42,
  "record_type": "future_event"
}
```

warning 不应包含整条原始 JSON、聊天正文、token、绝对附件内容或私有推理。需要调试 fixture 时，由开发者在本地对测试样例单独运行详细模式，正式应用不打开详细日志。

建议使用稳定的小写错误码，例如：

- `damaged_json_line`
- `unknown_record`
- `unsupported_block`
- `orphan_tool_result`
- `unmatched_tool_call`
- `missing_sidecar`
- `path_outside_agent_home`
- `session_too_large`

## 与分享格式的映射

Rust 应逐项映射，不能整体复制 Adapter JSON：

| Adapter | `relay.handoff.v1` |
| --- | --- |
| `agent` | `source.agent` |
| `session_id` | `source.session_id` |
| Adapter version | `source.adapter.version` |
| `cwd` | 只用于找到仓库，包内改写为 `repo://` |
| `source_path` | 不进入分享包 |
| `project_key` | 不进入分享包 |
| `project_root` | 不进入分享包 |
| `messages[]` | `conversation.records[]` |
| `blocks[].name` | `blocks[].tool_name` |
| `blocks[].input` | `blocks[].arguments` |
| `blocks[].output` | `tool_result.content[]` |
| warning | `diagnostics[]`，重新分级和清理 |
| completeness | `conversation.completeness` 和 `export.completeness` |

映射过程中为记录和块生成稳定 ID，保存原类型与映射状态。`unsupported` 不应转成看似完整的 `text`；它继续作为 `unsupported` 或顶层 `unknown`，并使完整度至少为 `partial`。

## 只读要求

Adapter 对 `~/.claude` 和 `~/.codex` 只允许打开读取和元数据查询，不创建索引、不修复文件、不改权限、不写临时文件。测试必须使用 fixture 目录，不能拿用户真实历史做写入测试。

读取正在增长的 JSONL 时，应使用打开时的文件大小作为快照上限。末尾半行可以记为暂时不完整，但不能等待文件无限增长，也不能把半行当作损坏后覆写原文件。
