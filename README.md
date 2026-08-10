# Relay

Relay 用来把一段 Claude Code 或 ChatGPT 开发会话交给另一个人继续。

发送者选择要分享的聊天记录、项目说明和代码改动，Relay 生成一个链接。接收者可以先在浏览器中查看内容，也可以在 Relay 中把它导入为一条新的 ChatGPT 任务或 Claude Code 会话，然后直接发送下一条消息。

## 可以分享什么

- 用户和助手的消息。
- 已经发生过的工具调用和结果。这些内容只能查看，不会再次执行。
- `AGENTS.md`、`CLAUDE.md` 等已经出现在会话中的项目说明。
- 操作系统、架构和已发现工具的简要信息。
- Git 中尚未推送的提交、暂存修改、未暂存修改，以及用户选中的新文件。

发送前可以逐项检查和取消选择。Relay 发现疑似密钥、连接串或私钥时，会要求再次确认。

## 接收者需要安装 Relay 吗

不一定。

接收者直接打开分享链接，可以在浏览器中：

- 查看完整聊天记录。
- 搜索消息、工具名称和正文。
- 按“消息”或“工具”筛选。
- 复制单条记录。
- 查看并复制项目说明。
- 下载分享文件，之后在 Relay 中继续处理。

以下操作需要 Relay 桌面应用：

- 恢复分享包中的 Git 修改。
- 创建新的本机工作目录。
- 把分享的聊天记录导入 Claude Code 会话列表。
- 把分享的聊天记录导入 ChatGPT 本机任务列表，并打开导入后的任务。

## 怎么使用

### 发送分享

1. 打开 Relay，从项目列表中选择一段 Claude Code 或 ChatGPT 会话。
2. 阅读聊天记录，点击“创建分享链接”。
3. 检查要发送的内容；如需调整，展开“修改发送内容”，再创建链接。
4. 把链接发给接收者。

Relay 会自动连接内置的分享服务。普通用户不需要填写服务器地址或上传令牌。

### 浏览器查看

接收者打开链接后，可以查看聊天记录和项目说明，也可以下载分享文件。工具记录只供阅读，不会运行。

分享链接包含查看权限。不要把链接转发给无关人员；不再需要时，可以在 Relay 的“分享记录”中撤销。

### 在本机继续处理

1. 在 Relay 的“接收”页面粘贴分享链接，或选择已经下载的分享文件。
2. 验证内容。如果分享包含 Git 改动，选择发送者使用的同一个 Git 项目；如果本机还没有，先从同一个远程仓库运行 `git clone`。
3. 选择新工作目录的保存位置。
4. 点击“导入到 ChatGPT”或“导入到 Claude Code”。

分享包中的 Git 内容不是完整项目副本，而是发送者相对原项目产生的提交、暂存、未暂存和所选新文件。Relay 会检查接收方选择的仓库是否来自同一个远程项目，然后另建工作目录并恢复这些改动；所选仓库本身不会被修改。

Relay 会先恢复代码或保存附件，再创建一条新的本机会话。导入的是发送者允许分享的消息、项目说明和历史工具记录；已有任务不会被覆盖，历史工具也不会再次执行。

ChatGPT 导入成功后，Relay 会等待 ChatGPT 重新读取本机任务，再直接打开新任务。较长的聊天记录首次显示时可能需要稍等。Claude Code 导入成功后会显示会话 ID 和可复制的 `claude --resume <会话 ID>` 命令。

分享文件中仍保留一份可读说明和结构化记录，供浏览器查看或导入失败时备用。正常使用不需要手工打开这些文件。

## 常见问题

### 能直接把 Claude Code 会话导入 ChatGPT 吗

可以。Relay 会把发送者选中的可见消息、项目说明和历史工具记录转换成一条新的 ChatGPT 本机任务。反过来，也可以把 ChatGPT 分享包导入 Claude Code。

### 导入时会复制发送方的 ChatGPT 数据库吗

不会。ChatGPT 的聊天正文主要保存在会话 JSONL 中，`state_5.sqlite` 主要保存接收者本机的任务索引和显示信息。Relay 会根据发送者允许分享的内容创建一份新的 JSONL，再向接收者自己的任务数据库新增一条记录；不会复制或替换发送方的数据库，也不会修改接收者已有任务。

来自 ChatGPT 的工具调用会尽量保留原生记录类型，因此导入后可以继续看到“读取文件、运行命令、编辑文件”等历史活动。上下文自动压缩只保存“这里发生过压缩”这一可见标记，不包含模型私有内容或隐藏状态。

模型的私有推理、登录状态、密钥、厂商内部记录和文件系统检查点不会被复制。

### 能导入 Claude 或 ChatGPT 官方生成的网页分享链接吗

目前不能。厂商网页分享通常只包含可见对话，不能提供完整工具记录、附件原件和 Git 修改。当前支持范围见 [外部分享链接说明](docs/provider-share-links.md)。

### 接收分享后会自动运行命令吗

不会。安装依赖、运行测试、执行工具调用、更新 submodule 和下载 Git LFS 文件，都必须由用户自行决定。

### 项目不是 Git 仓库还能分享吗

可以。Relay 会自动取消 Git 内容，只分享会话和项目说明。接收时会创建普通文件夹，并仍然可以导入为新的 ChatGPT 任务或 Claude Code 会话。

### 接收 Git 改动前为什么要有同一个仓库

因为 Relay 分享的是相对原项目产生的改动，不是把整个项目重新上传一遍。接收方需要选择从同一个远程仓库克隆出的本机项目，Relay 才能确定这些改动应该应用到哪份代码上。

如果本机还没有这个项目，请先运行 `git clone <仓库地址>`。Relay 随后会另建工作目录和分支，不会修改原来的工作目录。

### 分享链接有访问密码吗

分享链接包含查看权限。任何拿到链接的人都可能读取内容。请只把链接发送给需要查看的人，并在不再需要时撤销分享。

## 当前状态

Relay 目前面向 macOS，处于早期版本。

- 已支持读取本机 Claude Code 和 ChatGPT 会话。
- 已支持普通文件夹和 Git 项目。
- 已支持加密分享包、本机导入和 HTTPS 分享链接。
- 已支持接收者在浏览器中查看聊天记录和项目说明。
- 已支持把分享内容导入为新的 ChatGPT 任务或 Claude Code 会话。
- Relay 显示 ChatGPT 时会检查应用签名，只接受官方 ChatGPT 应用。
- 线上分享服务已经部署。自定义域名、Developer ID 签名和 Apple 公证仍未完成。

不要把当前早期版本用于无人检查的敏感代码分享。

ChatGPT 运行时可能保留旧的任务列表。Relay 完成写入检查后，会先显示经过签名检查的官方 ChatGPT，等待本机任务列表刷新，再打开新任务。打开失败时，接收页面仍会保留任务编号，并提供“打开导入的任务”和“显示任务列表”；再次点击不会重复导入，也不会创建第二条任务。

## 下载

[下载 Relay v0.1.0（macOS Apple Silicon）](https://github.com/serein431/Relay/releases/tag/v0.1.0)

当前 DMG 使用 ad-hoc 签名，应用和内部程序的签名结构已经过检查，但没有 Developer ID 签名和 Apple 公证。陌生 Mac 仍可能提示无法验证开发者；请先确认文件来自本仓库，再到“系统设置 → 隐私与安全性”中允许本次打开。

## 安全说明

- 每个分享文件使用独立密钥加密，分享服务只保存加密后的内容。
- 查看密钥保存在链接中，通常不会随网页请求发送给服务器。
- 接收内容必须先通过格式、大小、摘要和路径检查，之后才能写入本机目录。
- 只有用户点击导入后，Relay 才会新增 ChatGPT 或 Claude Code 会话；写入前会备份相关索引和 ChatGPT 置顶列表。
- Relay 不修改已有会话文件，导入失败时只清理本次创建的会话记录。
- Relay 不读取环境变量、钥匙串、SSH 配置或 Git 凭据作为分享内容。
- 工具调用始终是历史记录，不会进入执行队列。
- 失败的 Git 恢复不会强行删除新建目录，避免误删用户文件。

详细规则见 [安全说明](docs/security.md)。

## 本地开发

如果你只是想使用 Relay，可以跳过这一节。

需要准备：

- macOS
- Node.js
- pnpm 11
- Rust 1.77.2 或更高版本
- Go 1.24 或更高版本
- Git

安装依赖并运行主要检查：

```bash
pnpm install --frozen-lockfile
pnpm check
```

启动桌面开发版：

```bash
pnpm desktop:dev
```

构建当前机器架构的本地测试 `.app` 和 DMG：

```bash
pnpm desktop:build:local
```

本地测试包使用 ad-hoc 签名，签名结构完整，但没有 Apple 开发者身份和公证，只能用于本机测试，不能作为公开 Release。正式发布使用 `pnpm desktop:build`，并由构建环境提供 Developer ID 与 Apple 公证凭据。

检查构建结果：

```bash
pnpm desktop:verify src-tauri/target/release/bundle/macos/Relay.app
pnpm desktop:verify src-tauri/target/release/bundle/dmg/Relay_0.1.0_aarch64.dmg
```

正式发布前必须运行更严格的检查：

```bash
pnpm desktop:verify:release src-tauri/target/release/bundle/macos/Relay.app
pnpm desktop:verify:release src-tauri/target/release/bundle/dmg/Relay_0.1.0_aarch64.dmg
```

该检查要求 Developer ID Application 签名、有效 Team ID、Gatekeeper 验证和 app 公证票据，任何一项缺失都会停止发布。

只运行前端界面：

```bash
pnpm dev
```

浏览器开发模式只显示桌面应用说明，不提供虚构的项目或会话。读取真实会话、生成分享包和恢复 Git 内容必须在桌面应用中测试。

Cloud 分享服务的开发和测试命令：

```bash
pnpm cloud:check
```

## 项目文档

- [架构说明](docs/architecture.md)
- [安全说明](docs/security.md)
- [验收清单](docs/v0.1-acceptance.md)
- [Adapter 进程协议](docs/adapter-protocol.md)
- [外部分享链接支持范围](docs/provider-share-links.md)
- [Relay Handoff v1 Schema](schemas/relay-handoff-v1.schema.json)

## 许可证

仓库目前没有许可证文件。在许可证确定前，请不要假定项目可以自由转载、修改或重新发布。
