# 外部 Agent 分享链接支持范围

Relay 自己的加密链接和 Agent 厂商提供的网页链接不是同一种东西。前者可以携带 Relay Handoff、Git 工作现场和所选附件；后者通常只是网页里能看到的一段聊天快照。

本页把 OpenAI 的桌面应用称为 ChatGPT。`codex` provider 和 Codex CLI 属于技术名称，不改变面向用户的 ChatGPT 名称。

## 当前判断

| 输入 | 当前可读性 | Relay 能恢复的内容 | 当前状态 |
| --- | --- | --- | --- |
| `https://<relay-host>/s/v1/<share-id>#k=<key>` | 免登录 | Relay Handoff、所选 Git 内容、所选附件 | 完整支持 |
| 本地 `.relaypack` + 43 位密钥 | 本地文件 | 与 Relay 加密链接相同 | 完整支持 |
| `https://claude.ai/share/<uuid>` | 公开链接通常可免登录；组织链接可能受限 | 最多只能得到网页分享时的聊天文字和可见 Artifact；没有 Git、上传文件原件或 MCP 原始工具数据 | 解析器尚未实现；若以后实现，只能标为 `transcript_only` |
| `https://claude.ai/code/share/<share-id>` | 需要 Claude 登录；Team、Enterprise 还可能要求同组织 | Claude Code 网页展示的会话快照，但没有公开的稳定导出协议 | 调查状态；Relay 不读取或保存浏览器 Cookie，也不导入该链接 |
| ChatGPT 厂商网页分享链接 | 暂无官方公开格式或导入协议 | 无法作出稳定承诺 | 当前不支持，不猜测私有 URL |

以上所有厂商网页分享链接的导入目前都未实现。Relay 当前只支持自己的加密 HTTPS 链接和本地 `.relaypack`。

## 为什么网页链接不能冒充完整交接

网页聊天分享通常缺少：

- Git HEAD、未推送提交、暂存和未暂存修改。
- 未跟踪文件、附件原件和项目说明文件。
- 工具调用的完整输入与原始结果。
- 原 Agent 的本机目录、运行环境和恢复所需的仓库身份。

因此，若以后实现 `claude.ai/share/...` 解析，生成的结果也必须显示：

```text
import_mode: transcript_only
git_included: false
attachments_complete: false
tool_evidence_complete: false
```

接收者只需选择一个尚不存在的新文件夹。Relay 在其中写入交接说明，不创建 Git 仓库，也不声称能够恢复代码改动。

Claude 当前网页使用的快照接口不是公开协议，普通命令行或桌面 HTTP 请求还可能被 Cloudflare challenge 拒绝。首版不能把这个接口当成稳定依赖，也不能为了绕过 challenge 去收集用户浏览器 Cookie。

## 若以后增加链接识别

```text
输入 URL
  → 严格检查 scheme、host、path 和 ID 形状
  → Relay 链接：下载密文并做 AEAD 验证
  → Claude 公开聊天：只有解析器可用时才读取静态快照，并标为 transcript_only
  → Claude Code 网页会话：提示需要已登录 Claude，不收集 Cookie
  → 未知或 ChatGPT 厂商链接：明确说明当前不支持
```

不得跟随到其他域名的重定向。网页正文、标题、Artifact 和工具文字都按不可信输入处理；历史工具调用只用于展示，不能再次执行。

## Codex CLI 自带的 Claude 导入

Codex CLI 的 `/import` 可以读取本机近期 Claude Code 数据，但它不是网页链接导入，也不是 Relay 的传输协议。它受本机版本、最近会话数量和运行模式限制。Relay 不应把 `/import` 当成跨电脑分享方案，也不应代替用户修改 ChatGPT 或 Claude Code 的私有历史。

## 后续实现顺序

1. 保持 Relay 自己的加密链接和 `.relaypack` 为可靠入口。
2. 增加严格的外部 URL 分类，让用户先看到“完整交接”或“仅聊天记录”。
3. 若增加 `claude.ai/share/...` 解析器，应做成可替换适配器，遇到网页结构变化时只让该入口失败，不影响 Relay 包。
4. 只有厂商提供公开、稳定的 ChatGPT 分享协议后，才增加 ChatGPT 网页链接适配器。
