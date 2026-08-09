import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { Miniflare } from "miniflare";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

interface CreatedShare {
  schema: string;
  share_id: string;
  share_url: string;
  upload_url: string;
  metadata_url: string;
  expires_at: string;
  upload_expires_at: string;
  upload_token: string;
  revoke_token: string;
}

const workerPath = resolve(process.cwd(), "dist/index.js");
const allowedOrigin = "https://app.example.test";
let miniflare: Miniflare;

beforeEach(async () => {
  await readFile(workerPath);
  miniflare = makeMiniflare();
});

function makeMiniflare(bindingOverrides: Record<string, string> = {}): Miniflare {
  return new Miniflare({
    modules: true,
    scriptPath: workerPath,
    compatibilityDate: "2026-08-07",
    r2Buckets: ["RELAY_BLOBS"],
    durableObjects: {
      RELAY_SHARES: "RelayShare",
      RELAY_RATE_LIMITS: "RelayRateLimit",
    },
    bindings: {
      PUBLIC_BASE_URL: "https://share.example.test",
      ALLOWED_ORIGINS: allowedOrigin,
      UPLOAD_TOKEN: "test-service-upload-token",
      RATE_LIMIT_SALT: "test-only-rate-limit-salt",
      MAX_CIPHERTEXT_BYTES: "33554432",
      DEFAULT_TTL_SECONDS: "604800",
      MIN_TTL_SECONDS: "1",
      MAX_TTL_SECONDS: "2592000",
      UPLOAD_EXPIRY_SECONDS: "900",
      CREATE_RATE_LIMIT: "1000",
      UPLOAD_RATE_LIMIT: "1000",
      DOWNLOAD_RATE_LIMIT: "1000",
      REVOKE_RATE_LIMIT: "1000",
      ...bindingOverrides,
    },
  });
}

afterEach(async () => {
  await miniflare.dispose();
});

describe("Relay share Worker", () => {
  it("reserves, uploads, describes, and downloads an encrypted blob", async () => {
    const ciphertext = new TextEncoder().encode("opaque encrypted relay package");
    const reservedAfter = Date.now();
    const share = await createShare(ciphertext);
    const reservationFinishedAt = Date.now();

    expect(share.share_id).toMatch(/^[A-Za-z0-9_-]{32}$/u);
    expect(share.upload_token).toMatch(/^[A-Za-z0-9_-]{43}$/u);
    expect(share.revoke_token).toMatch(/^[A-Za-z0-9_-]{43}$/u);
    expect(share.upload_token).not.toBe(share.revoke_token);
    expect(share.share_url).toBe(`https://share.example.test/s/v1/${share.share_id}`);
    expect(Date.parse(share.upload_expires_at) - reservedAfter).toBeGreaterThanOrEqual(895_000);
    expect(Date.parse(share.upload_expires_at) - reservationFinishedAt).toBeLessThanOrEqual(905_000);
    expect(Date.parse(share.upload_expires_at)).toBeLessThan(Date.parse(share.expires_at));
    expect(`${share.share_url}#k=${"D".repeat(43)}`).toMatch(
      /^https:\/\/share\.example\.test\/s\/v1\/[A-Za-z0-9_-]{32}#k=[A-Za-z0-9_-]{43}$/u,
    );

    const reservedMetadata = await miniflare.dispatchFetch(share.metadata_url, {
      headers: { Origin: allowedOrigin, "CF-Connecting-IP": "203.0.113.10" },
    });
    expect(reservedMetadata.status).toBe(200);
    expect(await reservedMetadata.json()).toMatchObject({
      schema: "relay.share.public.v1",
      status: "awaiting_upload",
      expires_at: share.expires_at,
      upload_expires_at: share.upload_expires_at,
    });
    const prematureDownload = await miniflare.dispatchFetch(`${share.metadata_url}/blob`, {
      headers: { Origin: allowedOrigin, "CF-Connecting-IP": "203.0.113.10" },
    });
    expect(prematureDownload.status).toBe(409);
    expect(await prematureDownload.json()).toMatchObject({ error: { code: "share_not_ready" } });
    const bucket = await miniflare.getR2Bucket("RELAY_BLOBS");
    expect((await bucket.list()).objects).toHaveLength(0);

    const upload = await uploadShare(share, ciphertext);
    expect(upload.status).toBe(201);
    expect(upload.headers.get("cache-control")).toContain("no-store");
    expect(upload.headers.get("access-control-allow-origin")).toBe(allowedOrigin);
    const uploadedBody = await upload.json() as Record<string, unknown>;
    expect(uploadedBody).toMatchObject({ status: "ready", expires_at: share.expires_at });
    expect(JSON.stringify(uploadedBody)).not.toContain("object_key");
    expect(JSON.stringify(uploadedBody)).not.toContain("ciphertext/");
    const duplicateUpload = await uploadShare(share, ciphertext);
    expect(duplicateUpload.status).toBe(200);
    expect(await duplicateUpload.json()).toMatchObject({
      status: "ready",
      expires_at: share.expires_at,
    });

    const metadata = await miniflare.dispatchFetch(share.metadata_url, {
      headers: { Origin: allowedOrigin, "CF-Connecting-IP": "203.0.113.10" },
    });
    expect(metadata.status).toBe(200);
    const publicBody = await metadata.json() as Record<string, unknown>;
    expect(publicBody).toMatchObject({
      schema: "relay.share.public.v1",
      status: "ready",
      ciphertext: {
        bytes: ciphertext.byteLength,
        sha256: sha256(ciphertext),
        content_type: "application/octet-stream",
      },
    });
    expect(JSON.stringify(publicBody)).not.toContain("objectKey");
    expect(JSON.stringify(publicBody)).not.toContain("Token");
    expect(JSON.stringify(publicBody)).not.toContain("project");

    const download = await miniflare.dispatchFetch(`${share.metadata_url}/blob`, {
      headers: { Origin: allowedOrigin, "CF-Connecting-IP": "203.0.113.10" },
    });
    expect(download.status).toBe(200);
    expect(new Uint8Array(await download.arrayBuffer())).toEqual(ciphertext);
    expect(download.headers.get("x-relay-ciphertext-sha256")).toBe(sha256(ciphertext));
    expect(download.headers.get("content-type")).toBe("application/octet-stream");

    const objects = await bucket.list();
    expect(objects.objects).toHaveLength(1);
    expect(objects.objects[0]?.key).toMatch(/^ciphertext\/[A-Za-z0-9_-]{43}$/u);
    expect(objects.objects[0]?.key).not.toContain(share.share_id);
  });

  it("serializes concurrent immutable uploads as one creation and one retry", async () => {
    const ciphertext = new TextEncoder().encode("same immutable ciphertext uploaded concurrently");
    const share = await createShare(ciphertext);

    const responses = await Promise.all([
      uploadShare(share, Uint8Array.from(ciphertext)),
      uploadShare(share, Uint8Array.from(ciphertext)),
    ]);
    expect(responses.map((response) => response.status).sort()).toEqual([200, 201]);
    for (const response of responses) {
      expect(await response.json()).toMatchObject({
        status: "ready",
        expires_at: share.expires_at,
      });
    }

    const bucket = await miniflare.getR2Bucket("RELAY_BLOBS");
    expect((await bucket.list()).objects).toHaveLength(1);
    const downloaded = await miniflare.dispatchFetch(`${share.metadata_url}/blob`, {
      headers: { "CF-Connecting-IP": "203.0.113.30" },
    });
    expect(downloaded.status).toBe(200);
    expect(new Uint8Array(await downloaded.arrayBuffer())).toEqual(ciphertext);
  });

  it("authenticates retry uploads before checking immutable size and digest conflicts", async () => {
    const ciphertext = new TextEncoder().encode("immutable ciphertext");
    const share = await createShare(ciphertext);
    const wrongToken = "A".repeat(43);
    const otherSameSize = Uint8Array.from(ciphertext);
    otherSameSize[0] = (otherSameSize[0] ?? 0) ^ 1;

    const deniedBeforeUpload = await uploadShare(share, otherSameSize, {
      token: wrongToken,
      digest: sha256(otherSameSize),
    });
    expect(deniedBeforeUpload.status).toBe(403);
    expect(await deniedBeforeUpload.json()).toMatchObject({
      error: { code: "invalid_upload_token" },
    });

    expect((await uploadShare(share, ciphertext)).status).toBe(201);

    const deniedAfterUpload = await uploadShare(share, otherSameSize, {
      token: wrongToken,
      digest: sha256(otherSameSize),
    });
    expect(deniedAfterUpload.status).toBe(403);
    expect(await deniedAfterUpload.json()).toMatchObject({
      error: { code: "invalid_upload_token" },
    });

    const digestConflict = await uploadShare(share, ciphertext, {
      digest: sha256(otherSameSize),
    });
    expect(digestConflict.status).toBe(409);
    expect(await digestConflict.json()).toMatchObject({
      error: { code: "ciphertext_digest_conflict" },
    });

    const longerCiphertext = new Uint8Array(ciphertext.byteLength + 1);
    longerCiphertext.set(ciphertext);
    const sizeConflict = await uploadShare(share, longerCiphertext);
    expect(sizeConflict.status).toBe(409);
    expect(await sizeConflict.json()).toMatchObject({
      error: { code: "ciphertext_size_conflict" },
    });
  });

  it("rejects finite ReadableStream PUT bodies without an unhandled rejection", async () => {
    const ciphertext = new TextEncoder().encode("streamed ciphertext that must be rejected safely");
    const share = await createShare(ciphertext);
    const unhandledRejections: unknown[] = [];
    const collectUnhandledRejection = (reason: unknown): void => {
      unhandledRejections.push(reason);
    };
    process.on("unhandledRejection", collectUnhandledRejection);

    try {
      const invalidToken = await uploadStreamingShare(share, ciphertext, {
        token: "A".repeat(43),
      });
      expect(invalidToken.status).toBe(403);
      expect(await invalidToken.json()).toMatchObject({
        error: { code: "invalid_upload_token" },
      });

      const digestConflict = await uploadStreamingShare(share, ciphertext, {
        digest: "f".repeat(64),
      });
      expect(digestConflict.status).toBe(409);
      expect(await digestConflict.json()).toMatchObject({
        error: { code: "ciphertext_digest_conflict" },
      });

      await new Promise<void>((resolvePromise) => setImmediate(resolvePromise));
      expect(unhandledRejections).toEqual([]);
    } finally {
      process.off("unhandledRejection", collectUnhandledRejection);
    }
  });

  it("removes an uncompleted reservation after its short upload deadline", async () => {
    await miniflare.dispose();
    miniflare = makeMiniflare({ UPLOAD_EXPIRY_SECONDS: "1" });
    const ciphertext = new TextEncoder().encode("never uploaded ciphertext");
    const share = await createShare(ciphertext, 600);

    expect(Date.parse(share.upload_expires_at) - Date.now()).toBeLessThanOrEqual(1_000);
    expect(Date.parse(share.upload_expires_at)).toBeLessThan(Date.parse(share.expires_at));
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 1_100));

    const expiredUpload = await uploadShare(share, ciphertext);
    expect(expiredUpload.status).toBe(404);
    expect(await expiredUpload.json()).toMatchObject({ error: { code: "share_not_found" } });
    const metadata = await miniflare.dispatchFetch(share.metadata_url, {
      headers: { "CF-Connecting-IP": "203.0.113.14" },
    });
    expect(metadata.status).toBe(404);
    const bucket = await miniflare.getR2Bucket("RELAY_BLOBS");
    expect((await bucket.list()).objects).toHaveLength(0);
  });

  it("keeps a ready share retryable after the upload deadline without extending expiry", async () => {
    await miniflare.dispose();
    miniflare = makeMiniflare({ UPLOAD_EXPIRY_SECONDS: "1" });
    const ciphertext = new TextEncoder().encode("completed before upload deadline");
    const share = await createShare(ciphertext, 10);

    const firstUpload = await uploadShare(share, ciphertext);
    expect(firstUpload.status).toBe(201);
    expect(await firstUpload.json()).toMatchObject({ expires_at: share.expires_at });
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 1_100));

    const retry = await uploadShare(share, ciphertext);
    expect(retry.status).toBe(200);
    expect(await retry.json()).toMatchObject({ status: "ready", expires_at: share.expires_at });
    const download = await miniflare.dispatchFetch(`${share.metadata_url}/blob`, {
      headers: { "CF-Connecting-IP": "203.0.113.15" },
    });
    expect(download.status).toBe(200);
    expect(new Uint8Array(await download.arrayBuffer())).toEqual(ciphertext);
  });

  it("accepts the direct ciphertext POST contract", async () => {
    const ciphertext = new TextEncoder().encode("direct encrypted relay package");
    const response = await miniflare.dispatchFetch("https://share.example.test/v1/shares", {
      method: "POST",
      headers: {
        Authorization: "Bearer test-service-upload-token",
        "Content-Type": "application/octet-stream",
        "Content-Length": String(ciphertext.byteLength),
        "X-Relay-Bytes": String(ciphertext.byteLength),
        "X-Relay-Sha256": sha256(ciphertext),
        "X-Relay-TTL": "600",
        Origin: allowedOrigin,
        "CF-Connecting-IP": "203.0.113.20",
      },
      body: ciphertext,
    });
    expect(response.status).toBe(201);
    const created = await response.json() as CreatedShare & { blob_url: string };
    expect(created.share_id).toMatch(/^[A-Za-z0-9_-]{32}$/u);
    expect(created.share_url).toBe(`https://share.example.test/s/v1/${created.share_id}`);
    expect(created.upload_token).toBeUndefined();
    expect(created.revoke_token).toMatch(/^[A-Za-z0-9_-]{43}$/u);

    const downloaded = await miniflare.dispatchFetch(created.blob_url, {
      headers: { "CF-Connecting-IP": "203.0.113.20" },
    });
    expect(downloaded.status).toBe(200);
    expect(new Uint8Array(await downloaded.arrayBuffer())).toEqual(ciphertext);
  });

  it("requires the configured service upload token", async () => {
    const ciphertext = new TextEncoder().encode("protected ciphertext");
    const response = await miniflare.dispatchFetch("https://share.example.test/v1/shares", {
      method: "POST",
      headers: {
        "Content-Type": "application/octet-stream",
        "Content-Length": String(ciphertext.byteLength),
        "X-Relay-Bytes": String(ciphertext.byteLength),
        "X-Relay-Sha256": sha256(ciphertext),
        "CF-Connecting-IP": "203.0.113.21",
      },
      body: ciphertext,
    });
    expect(response.status).toBe(401);
    expect(await response.json()).toMatchObject({
      error: { code: "service_upload_token_required" },
    });
  });

  it("rejects bytes that do not match the reserved SHA-256", async () => {
    const reserved = new TextEncoder().encode("expected ciphertext");
    const uploaded = new TextEncoder().encode("tampered ciphertext");
    expect(uploaded.byteLength).toBe(reserved.byteLength);
    const share = await createShare(reserved);

    const response = await uploadShare(share, uploaded, { digest: sha256(reserved) });
    expect(response.status).toBe(422);
    expect(await response.json()).toMatchObject({
      error: { code: "ciphertext_checksum_mismatch" },
    });

    const bucket = await miniflare.getR2Bucket("RELAY_BLOBS");
    expect((await bucket.list()).objects).toHaveLength(0);
  });

  it("makes an expired share unavailable and removes its R2 object", async () => {
    const ciphertext = new TextEncoder().encode("short lived ciphertext");
    const share = await createShare(ciphertext, 1);
    expect((await uploadShare(share, ciphertext)).status).toBe(201);

    await new Promise((resolvePromise) => setTimeout(resolvePromise, 1_100));
    const response = await miniflare.dispatchFetch(`${share.metadata_url}/blob`, {
      headers: { "CF-Connecting-IP": "203.0.113.11" },
    });
    expect(response.status).toBe(404);
    expect(await response.json()).toMatchObject({ error: { code: "share_not_found" } });

    const bucket = await miniflare.getR2Bucket("RELAY_BLOBS");
    expect((await bucket.list()).objects).toHaveLength(0);
  });

  it("requires the independent revoke token and deletes revoked ciphertext", async () => {
    const ciphertext = new TextEncoder().encode("revocable ciphertext");
    const share = await createShare(ciphertext);
    expect((await uploadShare(share, ciphertext)).status).toBe(201);

    const wrongToken = "A".repeat(43);
    const denied = await miniflare.dispatchFetch(share.metadata_url, {
      method: "DELETE",
      headers: {
        Authorization: `Bearer ${wrongToken}`,
        "CF-Connecting-IP": "203.0.113.12",
      },
    });
    expect(denied.status).toBe(403);
    expect(await denied.json()).toMatchObject({ error: { code: "invalid_revoke_token" } });

    const revoked = await miniflare.dispatchFetch(share.metadata_url, {
      method: "DELETE",
      headers: {
        Authorization: `Bearer ${share.revoke_token}`,
        "CF-Connecting-IP": "203.0.113.12",
      },
    });
    expect(revoked.status).toBe(204);

    const download = await miniflare.dispatchFetch(`${share.metadata_url}/blob`, {
      headers: { "CF-Connecting-IP": "203.0.113.12" },
    });
    expect(download.status).toBe(404);
    const bucket = await miniflare.getR2Bucket("RELAY_BLOBS");
    expect((await bucket.list()).objects).toHaveLength(0);
  });

  it("serves a browser receiver that decrypts locally without embedding the fragment", async () => {
    const shareId = "a".repeat(32);
    const key = "K".repeat(43);
    const response = await miniflare.dispatchFetch(
      `https://share.example.test/s/v1/${shareId}#k=${key}`,
    );
    expect(response.status).toBe(200);
    const html = await response.text();
    expect(html).toContain("分享包的解密在当前浏览器中完成");
    expect(html).toContain("页面代码理论上可以读取地址中的密钥");
    expect(html).toContain("此页面只连接当前分享服务器，不连接第三方服务");
    expect(html).toContain("恢复 Git 修改、创建本机工作目录或打开新的 ChatGPT 任务");
    expect(html).toContain("工具记录仅供查看，不会执行");
    expect(html).not.toContain(key);
    expect(html).toContain("<script nonce=");
    expect(html).toContain("location.hash");
    expect(html).not.toContain("relay://");
    expect(html).not.toMatch(/https?:\/\/(?!share\.example\.test)/u);
    expect(html).not.toContain("sendBeacon");
    expect(html).not.toContain("WebSocket");
    expect(html).not.toContain("XMLHttpRequest");
    const csp = response.headers.get("content-security-policy") ?? "";
    const nonce = /script-src 'nonce-([^']+)'/u.exec(csp)?.[1];
    expect(nonce).toMatch(/^[A-Za-z0-9_-]{24}$/u);
    expect(html).toContain(`<script nonce="${nonce}">`);
    expect(csp).toContain("connect-src 'self'");
    expect(csp).toContain("default-src 'none'");
    expect(csp).toContain("frame-ancestors 'none'");
    expect(response.headers.get("referrer-policy")).toBe("no-referrer");
    expect(response.headers.get("cache-control")).toContain("no-store");
  });

  it("returns receiver headers without a body for HEAD", async () => {
    const shareId = "h".repeat(32);
    const response = await miniflare.dispatchFetch(
      `https://share.example.test/s/v1/${shareId}`,
      { method: "HEAD" },
    );
    expect(response.status).toBe(200);
    expect(await response.text()).toBe("");
    expect(response.headers.get("content-type")).toContain("text/html");
    expect(response.headers.get("content-security-policy")).toContain("connect-src 'self'");
  });

  it("redirects legacy receiver paths without copying a fragment into Location", async () => {
    const shareId = "b".repeat(32);
    const key = "L".repeat(43);
    const response = await miniflare.dispatchFetch(
      `https://share.example.test/s/${shareId}#k=${key}`,
      { redirect: "manual" },
    );
    expect(response.status).toBe(308);
    expect(response.headers.get("location")).toBe(`https://share.example.test/s/v1/${shareId}`);
    expect(response.headers.get("location")).not.toContain("#");
    expect(response.headers.get("location")).not.toContain(key);
  });

  it("accepts receiver routes only for 32-character share IDs", async () => {
    const valid = await miniflare.dispatchFetch(
      `https://share.example.test/s/v1/${"c".repeat(32)}`,
    );
    expect(valid.status).toBe(200);

    for (const invalidId of ["c".repeat(31), "c".repeat(33), `${"c".repeat(31)}!`]) {
      const canonical = await miniflare.dispatchFetch(
        `https://share.example.test/s/v1/${invalidId}`,
      );
      const legacy = await miniflare.dispatchFetch(`https://share.example.test/s/${invalidId}`, {
        redirect: "manual",
      });
      expect(canonical.status).toBe(404);
      expect(legacy.status).toBe(404);
    }
  });

  it("rejects extra public metadata and secrets in query strings", async () => {
    const ciphertext = new TextEncoder().encode("ciphertext");
    const body = JSON.stringify({
      ciphertext_bytes: ciphertext.byteLength,
      ciphertext_sha256: sha256(ciphertext),
      project_name: "must not reach the service",
    });
    const extraMetadata = await miniflare.dispatchFetch("https://share.example.test/v1/shares", {
      method: "POST",
      headers: {
        Authorization: "Bearer test-service-upload-token",
        "Content-Type": "application/json",
        "Content-Length": String(Buffer.byteLength(body)),
        "CF-Connecting-IP": "203.0.113.13",
      },
      body,
    });
    expect(extraMetadata.status).toBe(400);
    expect(await extraMetadata.json()).toMatchObject({ error: { code: "unexpected_metadata" } });

    for (const query of ["key=secret", `k=${"S".repeat(43)}`]) {
      const querySecret = await miniflare.dispatchFetch(
        `https://share.example.test/v1/shares/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?${query}`,
      );
      expect(querySecret.status).toBe(400);
      expect(await querySecret.json()).toMatchObject({
        error: { code: "secret_in_query_rejected" },
      });
    }
  });
});

async function createShare(ciphertext: Uint8Array, expiresInSeconds?: number): Promise<CreatedShare> {
  const input: Record<string, unknown> = {
    ciphertext_bytes: ciphertext.byteLength,
    ciphertext_sha256: sha256(ciphertext),
  };
  if (expiresInSeconds !== undefined) {
    input.expires_in_seconds = expiresInSeconds;
  }
  const body = JSON.stringify(input);
  const response = await miniflare.dispatchFetch("https://share.example.test/v1/shares", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "Content-Length": String(Buffer.byteLength(body)),
      Authorization: "Bearer test-service-upload-token",
      Origin: allowedOrigin,
      "CF-Connecting-IP": "203.0.113.10",
    },
    body,
  });
  expect(response.status).toBe(201);
  return response.json() as Promise<CreatedShare>;
}

async function uploadShare(
  share: CreatedShare,
  ciphertext: Uint8Array,
  options: { digest?: string; token?: string } = {},
): Promise<Awaited<ReturnType<Miniflare["dispatchFetch"]>>> {
  return miniflare.dispatchFetch(share.upload_url, {
    method: "PUT",
    headers: {
      Authorization: `Bearer ${options.token ?? share.upload_token}`,
      "Content-Type": "application/octet-stream",
      "Content-Length": String(ciphertext.byteLength),
      "X-Relay-Ciphertext-Sha256": options.digest ?? sha256(ciphertext),
      Origin: allowedOrigin,
      "CF-Connecting-IP": "203.0.113.10",
    },
    body: ciphertext,
  });
}

async function uploadStreamingShare(
  share: CreatedShare,
  ciphertext: Uint8Array,
  options: { digest?: string; token?: string } = {},
): Promise<Awaited<ReturnType<Miniflare["dispatchFetch"]>>> {
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(ciphertext);
      controller.close();
    },
  });
  const init: Parameters<Miniflare["dispatchFetch"]>[1] = {
    method: "PUT",
    headers: {
      Authorization: `Bearer ${options.token ?? share.upload_token}`,
      "Content-Type": "application/octet-stream",
      "Content-Length": String(ciphertext.byteLength),
      "X-Relay-Ciphertext-Sha256": options.digest ?? sha256(ciphertext),
      Origin: allowedOrigin,
      "CF-Connecting-IP": "203.0.113.10",
    },
    body,
  };
  Object.assign(init, { duplex: "half" });
  return miniflare.dispatchFetch(share.upload_url, init);
}

function sha256(value: Uint8Array): string {
  return createHash("sha256").update(value).digest("hex");
}
