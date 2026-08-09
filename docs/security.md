# 安全说明

## 安全目标

Relay 会处理聊天记录、源码、未提交改动和附件。这些内容可能含有密钥，也可能由不可信的人制作。首版以三个原则为准：发送前看清楚，接收后先检查，任何项目命令都由用户自己决定是否运行。

安全边界包括本机 Tauri/Rust 进程、只读 Adapter、加密包和密文存储服务。Claude Code、ChatGPT、Git 仓库内容、分享链接、接收到的包和远端仓库都不能默认可信。

面向用户时，桌面应用名称统一写作 ChatGPT。`codex` provider、`~/.codex`、`codex://`、`com.openai.codex`、Codex CLI 以及 Schema 和协议中的技术值仍保留原名。

## 不会自动做的事

Relay 不会因为导入一个包而自动运行以下内容：

- `npm install`、`pnpm install`、`pip install`、`cargo build` 等安装或构建命令。
- 测试、格式化、迁移、开发服务器和项目脚本。
- 会话中保存的任何工具调用。
- submodule 更新、LFS 下载、Git hook、smudge/clean filter。
- 附件中的程序、脚本、快捷方式或应用。

`tool_call.arguments`、测试命令和交接文档中的 shell 文本都只是历史材料。Schema 要求工具块带 `semantics: historical_record` 和 `replay_policy: never`。接收端不能把它们放入执行队列。

启动 Claude Code 或 ChatGPT 也必须在 Git 恢复完成并经用户确认后进行。首条提示只要求阅读交接目录，不要求运行命令。

## 内容分类

分享包只允许两种分类：

- `user_visible`：用户在会话界面中能看到的消息、工具记录或附件。
- `project_owned`：仓库文件、用户明确选择的项目说明和 Git 内容。

以下内容不能进入可分享消息块、未知记录或资产：

- `provider_internal`：服务商内部事件、缓存、鉴权信息、传输字段和未公开控制信息。
- `private_reasoning`：模型的私有推理或被原生格式标为隐藏思考的内容。

它们可以出现在 `export.omissions` 的原因和数量中，但不能保存原文。未知原生记录默认省略；只有 Adapter 能确认它属于前两种分类，并生成安全摘要后，才可作为 `unknown` 或 `unsupported` 保存。

## 发送前检查

生成包之前必须出现本地预览，至少列出：

- 会话标题、来源 Agent、消息数量和未知记录数量。
- 选择的消息、工具参数、工具结果和附件。
- 远端地址、分支、HEAD、本地提交、staged、unstaged 和 untracked 文件。
- 因规则省略的项目及原因。
- 可能含有密钥的文件名和文本命中项。

用户取消任意消息、工具记录或附件后，导出模式变成 `selected`。首版只生成通用 Handoff，不把原生会话文件当作隐藏附件一并打包。

本地预览带 `preview_sha256`。真正生成包时，Adapter 会重新读取会话，Rust 会比较新的摘要；若会话在预览后发生变化，生成过程以 `session_preview_changed` 停止，不开始 Git 捕获，也不创建包。用户必须重新查看最新内容。

界面先标出疑似敏感的已选内容。后端生成最终 Handoff 前还会重新扫描消息、工具记录、会话说明和 Git 内容；只要仍有疑似密钥、连接串或私钥，必须由用户再次明确确认。错误只返回类型、范围和数量，不返回命中的原文。

环境变量、钥匙串、SSH 配置、Git credential helper 的输出以及 HTTP header 不进入包。远端 URL 在保存前移除 userinfo、query 和 fragment；形如 `https://token@host/repo` 的地址必须阻止导出，而不是只在界面遮住。

## 忽略文件和密钥

Git ignored 文件默认不分享。原因很简单：`.env`、本机配置、凭据和构建产物通常就在其中。界面可以提示存在可疑 ignored 文件，但不应把文件内容或完整名单写进 Handoff。

untracked 文件必须逐项选择，并经过以下检查：

- 常见凭据文件名，例如 `.env`、私钥、云服务凭据和认证缓存。
- 常见 token、私钥块、连接串和带凭据的 URL。
- 文件类型、大小和二进制状态。
- 文件路径是否位于仓库根目录内。

扫描只能减少误分享，不能证明内容安全。最终预览仍然必须保留。

## 加密和链接

当前 `.relaypack` 已实现以下加密方式：

- 每个包生成独立的 256 位随机密钥。
- 明文 envelope 先用 zstd 压缩，再使用 AES-256-GCM 和独立的 96 位随机 nonce 加密。
- 固定的 `RELAYPK1` 文件头作为 AAD。文件内容依次为文件头、nonce 和带认证标签的密文。
- 服务端只保存密文、大小、创建时间和过期时间。
- 主要分享方式是 `https://<relay-host>/s/v1/<share-id>#k=<key>`。解密密钥使用无填充 base64url 放在 URL fragment 中，浏览器发 HTTP 请求时不会把 fragment 发给服务端。
- 本地 `.relaypack` 也可以与 43 位密钥分开传递。接收端在展示或恢复前检查 AES-GCM、Schema、引用、路径、长度和 SHA-256。

浏览器接收页会读取 fragment 中的密钥，从同源地址下载公开元数据和密文，并在当前页面完成解密。内容只通过 `textContent` 写入页面，不解析包内 HTML；Content Security Policy 只允许带随机 nonce 的内嵌脚本、内嵌样式和同源网络连接。页面不使用 CDN、第三方字体、分析服务、WebSocket、`sendBeacon` 或 `XMLHttpRequest`。

“HTTP 请求不会发送 fragment”不表示托管页面无法读取 fragment。分享服务器提供的页面代码理论上可以访问完整密钥，浏览器扩展和同一页面中的恶意脚本也可能读取它。包含私有代码、账号信息或其他敏感资料时，应优先使用桌面应用，并通过可信方式分别传递 `.relaypack` 与密钥。浏览器查看功能不恢复 Git，不创建工作目录，也不启动 Claude Code 或 ChatGPT。

URL fragment 不是身份认证。完整链接本身就是解密凭据，任何拿到链接的人都可以读取内容。链接可能通过剪贴板历史、聊天转发、截图、浏览器历史、扩展程序或恶意页面泄露。Relay 因此还需要：

- 默认较短的过期时间。
- 单独的撤销令牌；撤销接口不能只靠对象 ID。
- 服务端日志永不记录完整链接。
- 应用界面明确提示“拥有链接即可读取”。
- 可选的一次下载限制只能减少暴露时间，不能代替身份认证。

若将来需要确定接收者身份，应另加登录、组织权限或接收者公钥加密，不能继续把 fragment 方案描述成访问控制。

## 密文存储服务

新版桌面端先用 JSON `POST /v1/shares` 预留分享。对象 ID 使用 32 位随机 base64url 字符串，撤销令牌、每条分享自己的上传令牌、对象 ID 和解密密钥相互独立。预留阶段 R2 中没有密文；客户端把完整 `pending_upload` 凭据安全写入本机并完成文件与目录同步后，才使用授权 `PUT /v1/shares/:id/blob` 传密文。

公开 PUT 先由 Worker 向对应 Durable Object 发送没有正文的 `authorize` 请求。Durable Object 只检查请求头中的令牌、期限、长度、摘要和状态，并返回随机 R2 object key；密文正文不经过 Durable Object。Worker 把请求正文直接流式写入 R2，然后发送没有正文的 `complete` 请求。Durable Object 用 R2 `HEAD` 核对实际大小与 SHA-256，成功后把服务端状态改为 `ready`。桌面端核对成功响应后，才把本机记录改为 `active` 并删除上传令牌。

客户端与服务端的云分享密文上限都是 32 MiB。服务端还限制有效期，并可配置上传令牌和请求频率。即使看不到明文，也不信任客户端声明的类型或大小。删除接口使用单独的随机撤销令牌；分享落地页不加载脚本，也不读取 fragment。

待上传记录会显示在分享记录页。上传中断后，本机仍有撤销和重试所需的凭据；相同令牌、长度和摘要可以安全重试。包仍在且长度、摘要未变时可以继续，包丢失或改变时只能撤销。转为 `active` 后，每条分享自己的上传令牌会从本机记录中删除。

Worker 给预留设置短期上传期限，默认 900 秒，并让 Durable Object 为 upload deadline 设置 alarm。未完成的预留以及已经写入 R2 但未完成确认的对象会在期限到达后清理；R2 删除暂时失败时，alarm 会再次运行。旧版 `application/octet-stream` direct POST 仅作为兼容接口保留，新版桌面端不会自动退回。

## 本机分享记录

分享 ID、服务地址和撤销令牌只保存在 Relay 的应用数据目录，不进入 `~/.claude`、`~/.codex` 或前端持久状态。当前实现包含以下保护：

- 应用数据目录和分享记录目录设为仅当前用户可进入，记录文件与锁文件设为仅当前用户可读写。
- 打开锁文件和记录文件时拒绝 symlink，并在打开后重新核对文件身份。
- 线程锁加 `flock` 进程锁保护读取、写入、迁移、隔离和撤销更新，多个 Relay 进程不会同时改同一批记录。
- 启动时检查 `.record.*.tmp` 和待上传凭据的临时文件：能确认完整的记录会恢复，损坏记录会移到隔离目录，仍被另一个进程锁定的活动文件会跳过。
- macOS 上给分享记录目录写入并回读 Time Machine 排除属性，避免撤销令牌进入普通系统备份。

旧版单文件历史会移到新目录中作为私有迁移备份，不会因为旧文件存在而阻止新的上传。旧客户端的 direct POST 只是兼容接口；新版两步上传不使用这条路径。

## 接收包的处理顺序

接收端必须先预览，再写仓库文件。安全顺序如下：

1. 把密文下载到 Relay 自己创建的临时目录，设置仅当前用户可读写。
2. 检查密文大小和固定头，再尝试 AEAD 解密。认证失败时立即停止，不提取任何文件。
3. 读取 `handoff.json`，检查 JSON Schema、协议版本和数量限制。
4. 扫描所有归档项，检查路径、类型、声明大小和总展开大小。
5. 验证每个资产的 SHA-256，并检查所有 `asset_id`、`call_id` 和记录引用。
6. 展示预览和警告，等待用户确认。
7. 有 Git 内容时创建新 worktree，并按安全顺序恢复 Git 数据；纯会话包创建新的普通文件夹，只写固定名称的交接文件。
8. 完成状态比较后才允许启动 Agent。

任何一步失败都不能“尽量导入剩余文件”。接收端可以保留不含明文的错误信息，但失败后的解密临时目录应安全清理。

## 恶意压缩包

无论最后选择 ZIP、TAR 还是自定义容器，提取器都必须拒绝：

- 绝对路径、盘符、UNC 路径、反斜杠混淆、NUL 和 `.` / `..` 路径段。
- 规范化后落到目标目录外的路径。
- 重复路径、大小写折叠后冲突的路径和 Unicode 规范化后冲突的路径。
- symlink、hardlink、设备文件、FIFO、socket 和其他特殊条目。
- 文件数量、单文件大小、总展开大小或压缩比超过限制的包。
- 声明大小和实际读取大小不一致的条目。

当前 Rust 限制为：明文 envelope 不超过 32 MiB，加密包不超过 32 MiB，payload 解码后合计不超过 20 MiB，单个所选文件不超过 5 MiB，所选 untracked 文件不超过 500 个。云端上传和下载也拒绝超过 32 MiB 的密文。程序使用有界读取和累计计数，不能只信任声明长度。

写文件时使用“新建且不得已存在”的方式。检查路径字符串后，还要逐级确认父目录不是 symlink。不能先检查再用普通路径覆盖，因为中间目录可能在检查后被替换。

## Git 恢复风险

### 新 worktree

含 Git 内容时，接收方始终创建新 worktree。目标目录必须不存在，分支名由 Relay 生成并经过 Git ref 检查。Relay 不使用 `--force` 覆盖现有分支或 worktree。

纯会话包不接触 Git。目标目录同样必须不存在，Relay 新建普通文件夹，并用“仅当文件不存在时创建”的方式写入只读的 `HANDOFF.md` 和 `handoff.json`。写入失败时，只清理由 Relay 本次创建且文件身份未改变的内容。

Git hook 必须关闭，Git 配置使用受限环境。仍需注意 checkout 可能触发 filter；首版遇到需要外部 clean/smudge filter 的仓库时应阻止自动恢复，并让用户手动处理。

### 本地提交和补丁

恢复顺序是：

1. 验证 Git bundle，并把对象导入目标仓库。
2. 从包内指定 HEAD 创建不覆盖现有内容的新 worktree。
3. 对 staged patch 运行只读检查，再应用到 index 和 working tree。
4. 对 unstaged patch运行只读检查，再应用到 working tree。
5. 最后写 untracked 文件，并拒绝覆盖任何已有路径。
6. 比较 HEAD、index、working tree 和包内摘要。

补丁不能写 `.git`，也不能通过 symlink 父目录写到仓库外。`git worktree add -b` 成功后若后续步骤失败，Relay 停止继续写入，并保留新 worktree 和分支，不自动运行 `git worktree remove` 或强制删除分支。错误详情包含 `cleanup_incomplete`、`preserved_worktree_path`、`preserved_branch_ref` 和清理诊断，用户可以先检查现场再自行处理。

Relay 的交接目录使用随机名称，并在 Git common directory 的 `info/exclude` 中加入一条精确规则。更新时会检查普通文件、大小、内容、权限和文件身份，使用应用内互斥锁与同目录临时文件替换；回退前也会确认文件仍是 Relay 写入的版本。这个锁不能约束外部 Git 进程，最后一次检查与 `rename` 之间仍存在很短的并发修改窗口，基于路径的父目录检查也不能提供目录句柄级保证。若检测到外部修改，Relay 不覆盖它，并保留失败 worktree 和分支。需要严格消除这类窗口时，应改用目录句柄配合 `openat` / `renameat` 一类操作，或采用不修改 common `info/exclude` 的设计。

### submodule

Relay 不自动初始化或更新 submodule。dirty submodule、提交指针改动伴随本地未提交内容、缺少目标对象等情况会阻止代码分享。干净 submodule 只记录路径和提交，不包含它的工作目录内容。

### Git LFS

Relay 不自动下载 LFS 对象。仓库存在 LFS 规则、但 HEAD、index、工作树和所选相关路径都没有命中时，可以继续分享代码。一旦任何相关路径实际由 LFS 管理，首版就停止代码分享，并让发送方改为只分享会话；本机内容是 pointer、对象缺失或对象完整都不改变这个结果。

### symlink

untracked symlink 和包内 symlink 一律拒绝。对于基准提交中已有的 tracked symlink，Relay 可以在预览中列出，但后续写补丁和 untracked 文件时不得跟随它，也不得把它当作目录。

## 深链接和启动 Agent

Relay 分享使用 HTTPS 链接。链接只包含服务 origin、分享 ID 和 fragment 中的密钥，不携带 shell 命令、目标目录或 Agent 参数。接收者在 Relay 中导入链接并确认恢复位置，不能把 URL 内容拼成 shell 字符串。

启动 Agent 时使用参数数组，不经过 shell。工作目录固定为 Relay 新建的 worktree 或普通交接文件夹，交接文件由 Relay 创建并设为只读。提示文本是固定模板加本地路径，不包含从链接直接传来的命令。

Claude Code 启动前后都会运行 `claude agents --json --all --cwd <工作目录>`。Relay 只有在启动后找到一个此前不存在、类型为 background、目录匹配且开始时间合理的新会话 ID 时，才返回 `VERIFIED`；同时保留 Claude 报告的 `working`、`blocked` 等状态和可用的等待原因。仅仅看到 `claude --bg` 进程成功退出不算验证通过。

ChatGPT 使用两种 `codex://` URL。Relay 先用不含本机路径和提示的 `codex://threads/new` 查询 macOS 注册的处理应用，再用 Security.framework 检查 bundle ID `com.openai.codex`、OpenAI Team ID、嵌套代码和所有架构。只有候选应用通过固定签名要求后，Relay 才构造含工作目录路径和提示的 `codex://new?path=...&prompt=...`，并明确指定 ChatGPT 打开。没有通过验证的应用时，Relay 拒绝发送本机路径，也不提供绕过选项。

macOS 的完成回调只能证明打开请求已交给通过验证的 ChatGPT 应用。返回状态因此是 `OPEN_REQUESTED`，不表示新任务已经创建；`prompt` 只是预填输入框，仍需用户点击发送。

## 日志和诊断

日志不得记录：

- 完整分享链接和解密密钥。
- 会话正文、工具参数、工具结果和附件内容。
- 带凭据的远端 URL。
- 用户主目录下的完整绝对路径，除非用户主动导出诊断包。

默认日志只记录随机请求 ID、阶段、耗时、字节数和错误码。用户主动导出诊断时，应先展示将包含的路径和系统信息。

## 仍需发行前确认

包加密、HTTPS 密文分享、路径检查、Git 恢复和 Agent 启动已经有实现与自动测试，但这不等于发行版已经通过安全检查。正式发布前仍需完成应用签名、公证、正式域名和安装包测试，并按 [v0.1-acceptance.md](v0.1-acceptance.md) 记录恶意输入、接近 32 MiB 的真实传输、ChatGPT 签名检查和 Claude 后台会话验证结果。

当前明确保留的限制包括外部进程并发修改 `info/exclude` 时的路径级窗口。外部 `claude.ai` 或 ChatGPT 厂商网页分享链接也不能当作完整 Relay 包，而且这些厂商链接的导入目前尚未实现；当前支持范围见 [provider-share-links.md](provider-share-links.md)。
