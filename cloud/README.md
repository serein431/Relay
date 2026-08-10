# Relay 分享服务

这个目录是 Relay 的最小 Cloudflare 分享服务。它只保存客户端已经加密的二进制包，不解密、不解析包内容，也不接受项目名、会话标题、Git remote 或本机路径。

当前公开实例为 `https://relay-share.relay-share-cloud.workers.dev`。桌面应用默认使用这个地址；自行部署时再修改 `PUBLIC_BASE_URL`。

服务由三部分组成：

- Worker 提供 HTTP API 和接收页面。
- R2 保存 `application/octet-stream` 密文。
- 每个分享对应一个 `RelayShare` Durable Object，保存最少状态并处理过期、撤销和不可变上传；`RelayRateLimit` 只保存按来源哈希后的固定窗口计数。

解密密钥不属于 API 参数。桌面客户端把 256 位密钥附加到 `share_url` 的 fragment：

```text
https://share.example.com/s/v1/SHARE_ID#k=BASE64URL_32_BYTE_KEY
```

API 返回的 `share_url` 不带密钥。桌面客户端生成 32 字节随机密钥后，必须以 `#k=` 加 43 个 base64url 字符的形式附在链接末尾。分享 ID 仍由 24 个随机字节生成，编码后严格为 32 个 base64url 字符。

HTTP 请求不会把 `#` 及其后面的内容发给服务端。`/s/v1/:id` 是浏览器接收页：页面脚本读取 fragment 中的密钥，只请求同源的 `/v1/shares/:id` 和 `/v1/shares/:id/blob`，并在浏览器中完成摘要检查、AES-256-GCM 解密、zstd 解压和分享包检查。页面不连接第三方服务。

虽然服务端请求看不到 fragment，但服务端返回的页面代码理论上可以读取它。生产部署必须保证 Worker 代码、构建产物和发布流程可信，也不能加载第三方脚本、字体、图片或分析服务。高敏感内容应优先使用桌面应用，并把 `.relaypack` 与密钥分开传递。

完整链接本身就是解密凭据，不能把这种做法理解成接收者身份认证。

## API

### 1. 两步上传

桌面端先预留分享：

```http
POST /v1/shares
Content-Type: application/json

{
  "ciphertext_bytes": 12345,
  "ciphertext_sha256": "64 位十六进制 SHA-256",
  "expires_in_seconds": 604800
}
```

`expires_in_seconds` 可省略。`ciphertext_bytes` 受服务端 32 MiB 上限约束。服务返回 `share_id`、`share_url`、`upload_url`、`metadata_url`、`expires_at`、`upload_expires_at`，以及这条分享独有的 `upload_token` 和 `revoke_token`。预留时 Durable Object 状态为 `awaiting_upload`，R2 中还没有密文。

桌面端必须先把 `pending_upload` 记录写入本机。记录包含上传令牌、撤销令牌、包路径、密文长度和 SHA-256；文件与目录同步完成后，才能上传密文：

```http
PUT /v1/shares/:id/blob
Authorization: Bearer UPLOAD_TOKEN
Content-Type: application/octet-stream
Content-Length: 12345
X-Relay-Ciphertext-Sha256: 64 位十六进制 SHA-256

<ciphertext>
```

公开 PUT 的处理顺序固定如下：

1. Worker 向对应 Durable Object 发送没有正文的 `authorize` 请求，只转交上传鉴权和密文元数据请求头。
2. Durable Object 检查上传令牌、期限、长度、摘要和当前状态，再把随机 R2 object key 返回给 Worker。
3. Worker 把公开请求的正文直接流式写入 R2，不让密文正文经过 Durable Object，也不把整个包读进 Worker 内存。
4. R2 写入成功后，Worker 向 Durable Object 发送没有正文的 `complete` 请求。Durable Object 用 R2 `HEAD` 核对实际大小和 SHA-256，成功后把服务端状态改为 `ready`。
5. 桌面端核对成功响应后，把本机记录从 `pending_upload` 改为 `active`，并删除上传令牌；撤销令牌继续保留。

默认上传期限是 900 秒，可用 `UPLOAD_EXPIRY_SECONDS` 调整，但不会超过分享最终到期时间。期限内第一次成功 PUT 返回 201；相同令牌、长度和摘要的重试在 R2 对象一致时返回 200，不会延长最终有效期。错误令牌返回 403，长度或摘要不同返回 409。分享进入 `ready` 后不能被不同内容覆盖。

预留时 Durable Object 会为上传期限设置 alarm。若上传没有完成，或 Worker 写入 R2 后未能完成状态更新，upload deadline 到达后 alarm 会删除 Durable Object 状态和可能存在的 R2 对象；R2 删除暂时失败时会再次设置 alarm。桌面端在上传中断后仍保留 `pending_upload`，只要本机包仍存在且长度、摘要未变，就可以继续；否则只能撤销。

如果部署时设置了 `UPLOAD_TOKEN` secret，JSON 预留请求必须带：

```http
Authorization: Bearer SERVICE_UPLOAD_TOKEN
```

它只限制创建分享。后续 PUT 使用每条分享自己的上传令牌；公开元数据、下载和接收页面不要求登录。

### 2. 查询和下载

```http
GET  /v1/shares/:id
HEAD /v1/shares/:id
GET  /v1/shares/:id/blob
HEAD /v1/shares/:id/blob
```

公开元数据只有状态、过期时间、密文字节数、密文 SHA-256 和 MIME，不含任何项目内容。下载响应带 `Digest`、`ETag` 和 `X-Relay-Ciphertext-Sha256`，客户端仍须完成 AEAD 验证，不能只信任服务端摘要。

### 3. 撤销

```http
DELETE /v1/shares/:id
Authorization: Bearer REVOKE_TOKEN
```

撤销状态先写入 Durable Object，再删除 R2 对象。即使 R2 暂时失败，旧链接也会先失效，alarm 会重试物理删除。过期时使用同一套清理路径。

所有 capability token 只允许放在 `Authorization`。API 会拒绝 `k`、`key`、`token`、`revoke_token` 等 query 参数，避免它们进入访问日志。

## 本地运行

```bash
cd cloud
npm install
npm run check
npm run dev
```

测试使用 Miniflare 的真实 Worker、R2 和 Durable Object 模拟实现，覆盖：

- 预留阶段不可下载，且 R2 中没有对象。
- 第一次 PUT、相同内容重试、错误令牌和冲突内容。
- Worker 向 Durable Object 发没有正文的授权与完成请求，并把公开 PUT 正文流式写入 R2。
- 未完成预留的短期过期与清理。
- 上传、公开元数据和下载。
- JSON 预留请求和可选的服务上传令牌。
- 密文字节与 SHA-256 不符。
- 自动过期和 R2 删除。
- 错误撤销令牌、正确撤销和撤销后下载失败。
- 接收页只运行带随机 nonce 的内置脚本，只连接同源接口，并由 CSP 禁止第三方脚本和连接。
- 拒绝多余项目元数据和 query 中的秘密。

## 部署

1. 创建 R2 bucket，并把名称写入 `wrangler.jsonc`：

   ```bash
   npx wrangler r2 bucket create relay-share-blobs
   ```

2. 把 `PUBLIC_BASE_URL` 改成正式 HTTPS origin。它只应包含 scheme 和 host。`UPLOAD_EXPIRY_SECONDS` 默认是 900，只有确实需要更长上传时间时再调整。

3. 设置允许调用 API 的浏览器 origin。桌面端若由 Rust 发请求，可以把 `ALLOWED_ORIGINS` 留空；没有 `Origin` 的原生 HTTP 请求仍可用。不要为了省事在正式环境中写 `*`。

4. 如果需要限制谁能上传，设置 `UPLOAD_TOKEN`。不要把它写进桌面端公开代码或 `wrangler.jsonc`：

   ```bash
   openssl rand -hex 32 | npx wrangler secret put UPLOAD_TOKEN
   ```

5. 为按来源限流设置随机盐。它不会发给客户端，也不能写进 `wrangler.jsonc`：

   ```bash
   openssl rand -base64 32 | npx wrangler secret put RATE_LIMIT_SALT
   ```

6. 检查登录和配置，再部署：

   ```bash
   npx wrangler whoami
   npm run build
   npm run deploy
   ```

`wrangler.jsonc` 已包含首次 SQLite Durable Object migration。后续修改 class 名称时必须新增 migration tag，不能改写已经发布的 `v1`。

## 运行边界

- 代码中没有主动请求日志，也不会记录 URL。Cloudflare 自带的访问日志最多能看到 fragment 之前的路径，因为浏览器不会发送 fragment。不要启用会采集 `Authorization` 或请求正文的第三方日志。
- 示例配置关闭 Worker observability。若正式环境需要指标，只记录状态码、耗时、字节数和错误码，不记录完整 URL、请求头或响应令牌。
- R2 中不写 custom metadata；对象 key 与公开 share ID 无关。
- 限流使用加盐的来源地址哈希，不把原始 IP 写入 Durable Object。它只是基础防护，公开部署仍应在 Cloudflare 上设置请求大小规则、DDoS/WAF 规则和费用告警。
- Worker 入站请求上限、R2 配额和套餐限制仍以 Cloudflare 当前规则为准。部署前应在 staging 做接近 32 MiB 的真实上传，不以单元测试代替线上流式验证。
