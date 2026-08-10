import {
  constantTimeEqualHex,
  hashCapability,
  isBase64UrlToken,
  randomBase64Url,
  sha256HexText,
} from "./crypto";
import {
  apiError,
  corsPreflight,
  finalizeResponse,
  jsonResponse,
  methodNotAllowed,
  parsePositiveInteger,
  requestOriginAllowed,
} from "./http";
import { RelayRateLimit } from "./rate-limit";
import { RECEIVER_SCRIPT } from "./receiver-bundle.generated";
import { RelayShare } from "./share-object";
import type { Env, ShareRecord, UploadAuthorization } from "./types";

export { RelayRateLimit, RelayShare };

const SHARE_ID_PATTERN = /^[A-Za-z0-9_-]{32}$/u;
const SHA256_INPUT_PATTERN = /^[A-Fa-f0-9]{64}$/u;
const CREATE_BODY_LIMIT = 4096;
const DEFAULT_MAX_BYTES = 90 * 1024 * 1024;
const DEFAULT_TTL_SECONDS = 7 * 24 * 60 * 60;
const DEFAULT_MIN_TTL_SECONDS = 60;
const DEFAULT_MAX_TTL_SECONDS = 30 * 24 * 60 * 60;
const DEFAULT_UPLOAD_EXPIRY_SECONDS = 15 * 60;

interface CreateShareInput {
  ciphertext_bytes: number;
  ciphertext_sha256: string;
  expires_in_seconds?: number;
}

type RateBucket = "create" | "upload" | "download" | "revoke";

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    let response: Response;
    try {
      response = await route(request, env);
    } catch {
      response = apiError(500, "internal_error", "The request could not be completed.");
    }
    return finalizeResponse(response, request, env);
  },
};

async function route(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  if (containsSensitiveQuery(url)) {
    return apiError(
      400,
      "secret_in_query_rejected",
      "Secrets and capability tokens must not be placed in the query string.",
    );
  }
  if (request.method === "OPTIONS") {
    return corsPreflight(request, env);
  }
  if (!requestOriginAllowed(request, env)) {
    return apiError(403, "origin_not_allowed", "This browser origin is not allowed.");
  }

  if (url.pathname === "/v1/shares") {
    if (request.method !== "POST") {
      return methodNotAllowed(["POST"]);
    }
    const limited = await enforceRateLimit(request, env, "create");
    if (limited !== null) {
      return limited;
    }
    const authorizationFailure = await requireConfiguredUploadToken(request, env);
    if (authorizationFailure !== null) {
      return authorizationFailure;
    }
    const contentType = request.headers.get("Content-Type")?.split(";", 1)[0]?.trim().toLowerCase();
    if (contentType !== "application/json") {
      return apiError(
        415,
        "unsupported_media_type",
        "Use application/json to reserve a share before uploading ciphertext.",
      );
    }
    return createShareReservation(request, env);
  }

  const canonicalLandingMatch = /^\/s\/v1\/([A-Za-z0-9_-]{32})$/u.exec(url.pathname);
  if (canonicalLandingMatch !== null) {
    if (request.method !== "GET" && request.method !== "HEAD") {
      return methodNotAllowed(["GET", "HEAD"]);
    }
    const response = landingPage(canonicalLandingMatch[1] as string);
    return request.method === "HEAD"
      ? new Response(null, { status: response.status, headers: response.headers })
      : response;
  }

  const shareMatch = /^\/v1\/shares\/([A-Za-z0-9_-]{32})(\/blob)?$/u.exec(url.pathname);
  if (shareMatch === null) {
    return apiError(404, "not_found", "The requested resource was not found.");
  }
  const shareId = shareMatch[1] as string;
  if (!SHARE_ID_PATTERN.test(shareId)) {
    return apiError(404, "not_found", "The requested resource was not found.");
  }

  let bucket: RateBucket;
  if (request.method === "PUT") {
    bucket = "upload";
  } else if (request.method === "DELETE") {
    bucket = "revoke";
  } else {
    bucket = "download";
  }
  const limited = await enforceRateLimit(request, env, bucket);
  if (limited !== null) {
    return limited;
  }

  const id = env.RELAY_SHARES.idFromName(shareId);
  const stub = env.RELAY_SHARES.get(id);
  if (request.method === "PUT") {
    return shareMatch[2] === "/blob"
      ? uploadReservedShare(request, env, stub)
      : methodNotAllowed(["GET", "HEAD", "DELETE"]);
  }
  return stub.fetch(new Request(request.url, { method: request.method, headers: request.headers }));
}

async function uploadReservedShare(
  request: Request,
  env: Env,
  stub: DurableObjectStub,
): Promise<Response> {
  const authorized = await stub.fetch("https://relay.internal/internal/upload/authorize", {
    method: "POST",
    headers: copyUploadHeaders(request.headers),
  });
  if (!authorized.ok) {
    return authorized;
  }

  let authorization: UploadAuthorization;
  try {
    authorization = (await authorized.json()) as UploadAuthorization;
  } catch {
    return apiError(503, "upload_authorization_failed", "The ciphertext upload could not be authorized.");
  }
  if (!isUploadAuthorization(authorization)) {
    return apiError(503, "upload_authorization_failed", "The ciphertext upload could not be authorized.");
  }
  if (authorization.already_ready) {
    return jsonResponse(
      {
        schema: "relay.share.public.v1",
        status: "ready",
        expires_at: authorization.expires_at,
        ciphertext: {
          bytes: authorization.ciphertext_bytes,
          sha256: authorization.ciphertext_sha256,
          content_type: "application/octet-stream",
        },
      },
      200,
      authorization.etag === undefined ? undefined : { ETag: `"${authorization.etag}"` },
    );
  }
  if (request.body === null) {
    return apiError(400, "ciphertext_body_required", "The ciphertext body is required.");
  }

  let stored: R2Object;
  try {
    stored = await env.RELAY_BLOBS.put(authorization.object_key, request.body, {
      httpMetadata: {
        contentType: "application/octet-stream",
        contentDisposition: 'attachment; filename="relay-share.bin"',
      },
      sha256: authorization.ciphertext_sha256,
    });
  } catch (error) {
    const reason = error instanceof Error ? error.message : "";
    if (/checksum|sha-?256|digest/iu.test(reason)) {
      return apiError(422, "ciphertext_checksum_mismatch", "The uploaded bytes do not match the declared digest.");
    }
    return apiError(502, "storage_write_failed", "The ciphertext could not be stored.");
  }
  const completed = await stub.fetch("https://relay.internal/internal/upload/complete", {
    method: "POST",
  });
  if (completed.status === 404) {
    await env.RELAY_BLOBS.delete(authorization.object_key);
  }
  return completed;
}

function isUploadAuthorization(value: UploadAuthorization): boolean {
  return (
    value !== null &&
    typeof value === "object" &&
    /^ciphertext\/[A-Za-z0-9_-]{43}$/u.test(value.object_key) &&
    Number.isSafeInteger(value.ciphertext_bytes) &&
    value.ciphertext_bytes > 0 &&
    /^[a-f0-9]{64}$/u.test(value.ciphertext_sha256) &&
    typeof value.already_ready === "boolean" &&
    Number.isFinite(Date.parse(value.expires_at)) &&
    (value.etag === undefined ||
      (typeof value.etag === "string" && value.etag.length > 0 && value.etag.length <= 256))
  );
}

function copyUploadHeaders(headers: Headers): Headers {
  const copied = new Headers();
  for (const name of [
    "Authorization",
    "Content-Type",
    "Content-Encoding",
    "Content-Length",
    "X-Relay-Ciphertext-Sha256",
  ]) {
    const value = headers.get(name);
    if (value !== null) {
      copied.set(name, value);
    }
  }
  return copied;
}

async function createShareReservation(request: Request, env: Env): Promise<Response> {
  const lengthHeader = request.headers.get("Content-Length");
  if (lengthHeader !== null && Number(lengthHeader) > CREATE_BODY_LIMIT) {
    return apiError(413, "request_too_large", "The share reservation body is too large.");
  }

  let input: CreateShareInput;
  try {
    const text = await request.text();
    if (new TextEncoder().encode(text).byteLength > CREATE_BODY_LIMIT) {
      return apiError(413, "request_too_large", "The share reservation body is too large.");
    }
    input = JSON.parse(text) as CreateShareInput;
  } catch {
    return apiError(400, "invalid_json", "The share reservation body is not valid JSON.");
  }
  if (!isPlainObject(input)) {
    return apiError(400, "invalid_request", "The share reservation must be a JSON object.");
  }
  const allowedKeys = new Set(["ciphertext_bytes", "ciphertext_sha256", "expires_in_seconds"]);
  if (Object.keys(input).some((key) => !allowedKeys.has(key))) {
    return apiError(
      400,
      "unexpected_metadata",
      "The share service accepts only ciphertext size, digest, and expiration.",
    );
  }

  const maxBytes = Math.min(
    parsePositiveInteger(env.MAX_CIPHERTEXT_BYTES, DEFAULT_MAX_BYTES),
    DEFAULT_MAX_BYTES,
  );
  if (
    !Number.isSafeInteger(input.ciphertext_bytes) ||
    input.ciphertext_bytes < 1 ||
    input.ciphertext_bytes > maxBytes
  ) {
    return apiError(400, "invalid_ciphertext_size", `Ciphertext must be between 1 and ${maxBytes} bytes.`);
  }
  if (typeof input.ciphertext_sha256 !== "string" || !SHA256_INPUT_PATTERN.test(input.ciphertext_sha256)) {
    return apiError(400, "invalid_ciphertext_sha256", "ciphertext_sha256 must be a 64-character SHA-256 hex digest.");
  }

  const maxTtl = Math.min(
    parsePositiveInteger(env.MAX_TTL_SECONDS, DEFAULT_MAX_TTL_SECONDS),
    DEFAULT_MAX_TTL_SECONDS,
  );
  const minTtl = Math.min(
    parsePositiveInteger(env.MIN_TTL_SECONDS, DEFAULT_MIN_TTL_SECONDS),
    maxTtl,
  );
  const defaultTtl = Math.min(
    Math.max(parsePositiveInteger(env.DEFAULT_TTL_SECONDS, DEFAULT_TTL_SECONDS), minTtl),
    maxTtl,
  );
  const ttl = input.expires_in_seconds ?? defaultTtl;
  if (!Number.isSafeInteger(ttl) || ttl < minTtl || ttl > maxTtl) {
    return apiError(400, "invalid_expiration", `expires_in_seconds must be between ${minTtl} and ${maxTtl}.`);
  }

  const now = Date.now();
  const uploadExpirySeconds = Math.min(
    parsePositiveInteger(env.UPLOAD_EXPIRY_SECONDS, DEFAULT_UPLOAD_EXPIRY_SECONDS),
    ttl,
  );
  const uploadExpiresAt = now + uploadExpirySeconds * 1000;
  for (let attempt = 0; attempt < 5; attempt += 1) {
    const shareId = randomBase64Url(24);
    const objectKey = `ciphertext/${randomBase64Url(32)}`;
    const uploadToken = randomBase64Url(32);
    const revokeToken = randomBase64Url(32);
    if (
      !isBase64UrlToken(shareId, 32) ||
      !isBase64UrlToken(uploadToken, 43) ||
      !isBase64UrlToken(revokeToken, 43)
    ) {
      return apiError(500, "random_generation_failed", "Secure identifiers could not be generated.");
    }

    const record: ShareRecord = {
      version: 1,
      status: "awaiting_upload",
      objectKey,
      createdAt: now,
      expiresAt: now + ttl * 1000,
      uploadExpiresAt,
      ciphertextBytes: input.ciphertext_bytes,
      ciphertextSha256: input.ciphertext_sha256.toLowerCase(),
      uploadTokenHash: await hashCapability("upload", uploadToken),
      revokeTokenHash: await hashCapability("revoke", revokeToken),
    };
    const id = env.RELAY_SHARES.idFromName(shareId);
    const stub = env.RELAY_SHARES.get(id);
    const initialized = await stub.fetch("https://relay.internal/internal/init", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ record }),
    });
    if (initialized.status === 409) {
      continue;
    }
    if (!initialized.ok) {
      return apiError(503, "share_reservation_failed", "The share could not be reserved.");
    }

    const baseUrl = publicBaseUrl(request, env);
    return jsonResponse(
      {
        schema: "relay.share.created.v1",
        share_id: shareId,
        share_url: `${baseUrl}/s/v1/${shareId}`,
        upload_url: `${baseUrl}/v1/shares/${shareId}/blob`,
        metadata_url: `${baseUrl}/v1/shares/${shareId}`,
        expires_at: new Date(record.expiresAt).toISOString(),
        upload_expires_at: new Date(uploadExpiresAt).toISOString(),
        upload_token: uploadToken,
        revoke_token: revokeToken,
      },
      201,
    );
  }

  return apiError(503, "identifier_generation_failed", "A unique share identifier could not be reserved.");
}

async function requireConfiguredUploadToken(request: Request, env: Env): Promise<Response | null> {
  const configured = env.UPLOAD_TOKEN;
  if (configured === undefined || configured === "") {
    return null;
  }
  const match = /^Bearer ([A-Za-z0-9._~-]{16,256})$/u.exec(
    request.headers.get("Authorization") ?? "",
  );
  if (match === null) {
    return apiError(401, "service_upload_token_required", "A valid service upload token is required.", {
      "WWW-Authenticate": 'Bearer realm="relay-service-upload"',
    });
  }
  const [providedHash, configuredHash] = await Promise.all([
    sha256HexText(`relay-service-upload-v1\0${match[1]}`),
    sha256HexText(`relay-service-upload-v1\0${configured}`),
  ]);
  return constantTimeEqualHex(providedHash, configuredHash)
    ? null
    : apiError(403, "invalid_service_upload_token", "The service upload token is invalid.");
}

async function enforceRateLimit(
  request: Request,
  env: Env,
  bucket: RateBucket,
): Promise<Response | null> {
  const settings: Record<RateBucket, { limit: number; windowSeconds: number }> = {
    create: {
      limit: parsePositiveInteger(env.CREATE_RATE_LIMIT, 30),
      windowSeconds: 3600,
    },
    upload: {
      limit: parsePositiveInteger(env.UPLOAD_RATE_LIMIT, 60),
      windowSeconds: 3600,
    },
    download: {
      limit: parsePositiveInteger(env.DOWNLOAD_RATE_LIMIT, 600),
      windowSeconds: 3600,
    },
    revoke: {
      limit: parsePositiveInteger(env.REVOKE_RATE_LIMIT, 60),
      windowSeconds: 3600,
    },
  };
  const source = request.headers.get("CF-Connecting-IP") ?? "unknown";
  const identity = await sha256HexText(
    `relay-rate-limit-v1\0${env.RATE_LIMIT_SALT ?? ""}\0${source}`,
  );
  const id = env.RELAY_RATE_LIMITS.idFromName(`${bucket}:${identity}`);
  const stub = env.RELAY_RATE_LIMITS.get(id);
  const response = await stub.fetch("https://relay.internal/consume", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(settings[bucket]),
  });
  if (!response.ok) {
    return apiError(503, "rate_limit_unavailable", "The request cannot be accepted right now.");
  }
  const result = (await response.json()) as {
    allowed?: boolean;
    retry_after?: number;
  };
  if (result.allowed === true) {
    return null;
  }
  const retryAfter = Number.isSafeInteger(result.retry_after) ? String(result.retry_after) : "60";
  return apiError(429, "rate_limited", "Too many requests were received. Try again later.", {
    "Retry-After": retryAfter,
  });
}

function publicBaseUrl(request: Request, env: Env): string {
  const configured = env.PUBLIC_BASE_URL?.trim();
  if (configured !== undefined && configured !== "") {
    try {
      return new URL(configured).origin;
    } catch {
      // A bad deployment value falls back to the request origin without exposing it in logs.
    }
  }
  return new URL(request.url).origin;
}

function landingPage(shareId: string): Response {
  const nonce = randomBase64Url(18);
  const receiverScript = RECEIVER_SCRIPT.replace(/<\/script/giu, "<\\/script");
  const html = `<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="referrer" content="no-referrer">
  <link rel="canonical" href="/s/v1/${shareId}">
  <title>Relay 分享</title>
  <style nonce="${nonce}">
    :root {
      color-scheme: light;
      font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI", sans-serif;
      font-synthesis: none;
      background: #f6f7f9;
      color: #172033;
    }
    * { box-sizing: border-box; }
    [hidden] { display: none !important; }
    body { min-width: 20rem; min-height: 100vh; margin: 0; background: #f6f7f9; }
    button, input { font: inherit; }
    button { color: inherit; }
    button:focus-visible, input:focus-visible, summary:focus-visible { outline: 3px solid rgba(37, 99, 235, .24); outline-offset: 2px; }
    .site-header { height: 3.5rem; display: flex; align-items: center; justify-content: space-between; padding: 0 max(1rem, calc((100vw - 76rem) / 2)); border-bottom: 1px solid #e3e7ee; background: rgba(255, 255, 255, .92); }
    .brand { font-size: .95rem; font-weight: 700; letter-spacing: -.01em; color: #172033; }
    .header-note { font-size: .76rem; color: #667085; }
    main { width: min(76rem, calc(100vw - 2rem)); margin: 0 auto; padding: 2rem 0 4rem; }
    .state-card { width: min(34rem, 100%); margin: 12vh auto 0; padding: 2rem; border: 1px solid #e1e6ed; border-radius: 1rem; background: #fff; box-shadow: 0 .75rem 2.5rem rgba(20, 31, 52, .06); text-align: center; }
    .state-card h1 { margin: 0; font-size: 1.35rem; letter-spacing: -.025em; }
    .state-card p { margin: .75rem auto 0; max-width: 27rem; color: #667085; line-height: 1.65; }
    .loading-line { width: 8rem; height: .28rem; margin: 1.25rem auto 0; border-radius: 99px; overflow: hidden; background: #e8edf5; }
    .loading-line::after { content: ""; display: block; width: 45%; height: 100%; background: #2563eb; border-radius: inherit; animation: loading 1.15s ease-in-out infinite alternate; }
    @keyframes loading { from { transform: translateX(-15%); } to { transform: translateX(145%); } }
    .viewer-header { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 1.5rem; align-items: end; margin-bottom: 1.25rem; }
    .viewer-header > div:first-child { min-width: 0; }
    h1 { margin: 0; font-size: clamp(1.7rem, 3vw, 2.35rem); line-height: 1.15; letter-spacing: -.04em; overflow-wrap: anywhere; }
    #share-title { max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .project-name { margin: .55rem 0 0; color: #667085; font-size: .94rem; }
    #share-project { display: inline-block; max-width: min(100%, 42rem); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; vertical-align: bottom; }
    .header-actions { display: flex; flex-wrap: wrap; gap: .55rem; justify-content: flex-end; }
    .button { min-height: 2.35rem; padding: .58rem .82rem; border: 1px solid #d9e0e9; border-radius: .55rem; background: #fff; cursor: pointer; font-size: .82rem; font-weight: 650; transition: border-color .16s, background .16s, transform .16s; }
    .button:hover { border-color: #b9c4d4; background: #fafbfc; }
    .button:active { transform: translateY(1px); }
    .button.primary { border-color: #2563eb; background: #2563eb; color: #fff; }
    .button.primary:hover { background: #1d4ed8; }
    .summary-bar { display: flex; flex-wrap: wrap; gap: 0; margin-bottom: 1rem; border: 1px solid #e1e6ed; border-radius: .8rem; background: #fff; overflow: hidden; }
    .summary-item { min-width: 9rem; padding: .85rem 1rem; border-right: 1px solid #edf0f4; }
    .summary-item:last-child { border-right: 0; }
    .summary-item span { display: block; color: #7a8495; font-size: .72rem; }
    .summary-item strong { display: block; margin-top: .25rem; font-size: .9rem; font-weight: 650; }
    .share-note { margin: 0 0 1rem; padding: .85rem 1rem; border: 1px solid #d9e5fb; border-radius: .8rem; background: #f5f8ff; color: #36445e; font-size: .82rem; line-height: 1.6; }
    .tabs { display: flex; gap: 1.5rem; margin-bottom: 0; padding: 0 1rem; border: 1px solid #e1e6ed; border-bottom: 0; border-radius: .8rem .8rem 0 0; background: #fff; }
    .tab { position: relative; min-height: 3.15rem; padding: 0 .1rem; border: 0; background: transparent; color: #697386; cursor: pointer; font-size: .86rem; font-weight: 650; }
    .tab.is-active { color: #172033; }
    .tab.is-active::after { content: ""; position: absolute; right: 0; bottom: 0; left: 0; height: 2px; border-radius: 2px; background: #2563eb; }
    .panel { border: 1px solid #e1e6ed; border-radius: 0 0 .8rem .8rem; background: #fff; }
    .toolbar { display: flex; align-items: center; gap: .75rem; padding: .8rem 1rem; border-bottom: 1px solid #edf0f4; background: #fbfcfd; }
    .search { flex: 1 1 20rem; min-width: 10rem; height: 2.35rem; padding: 0 .75rem; border: 1px solid #d9e0e9; border-radius: .55rem; background: #fff; color: #172033; }
    .search::placeholder { color: #98a2b3; }
    .filters { display: flex; flex: 0 0 auto; gap: .2rem; padding: .2rem; border: 1px solid #e1e6ed; border-radius: .55rem; background: #fff; }
    .filter { min-height: 1.82rem; padding: .3rem .55rem; border: 0; border-radius: .36rem; background: transparent; color: #667085; cursor: pointer; font-size: .76rem; }
    .filter.is-active { background: #edf3ff; color: #1d4ed8; font-weight: 700; }
    .message-list { display: grid; gap: .85rem; padding: 1rem; }
    .message { border: 1px solid #e4e8ef; border-radius: .7rem; background: #fff; overflow: hidden; }
    .message.role-user { border-left: 3px solid #2563eb; }
    .message.role-assistant { border-left: 3px solid #8b5cf6; }
    .message.role-tool { border-left: 3px solid #64748b; }
    .message > header { min-height: 3rem; display: flex; align-items: center; justify-content: space-between; gap: 1rem; padding: .65rem .8rem; border-bottom: 1px solid #edf0f4; background: #fbfcfd; }
    .message-identity { display: flex; align-items: center; gap: .65rem; min-width: 0; }
    .role-mark { width: 1.75rem; height: 1.75rem; display: grid; place-items: center; flex: 0 0 auto; border-radius: .45rem; background: #edf3ff; color: #1d4ed8; font-size: .72rem; font-weight: 750; }
    .role-assistant .role-mark { background: #f3efff; color: #6d3fd8; }
    .role-tool .role-mark, .role-unknown .role-mark { background: #eef1f5; color: #536174; }
    .message-identity strong { display: block; font-size: .82rem; }
    .message-identity time { display: block; margin-top: .14rem; color: #8a94a5; font-size: .7rem; font-weight: 400; }
    .message > header button { padding: .35rem .55rem; border: 0; border-radius: .4rem; background: transparent; color: #667085; cursor: pointer; font-size: .75rem; }
    .message > header button:hover { background: #edf1f6; color: #273247; }
    .message-body { display: grid; gap: .65rem; padding: .8rem; }
    pre { margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; tab-size: 2; }
    .text-block { color: #2d3748; font: 400 .86rem/1.7 -apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI", sans-serif; }
    .tool-block { border: 1px solid #e1e6ed; border-radius: .55rem; background: #f8fafc; }
    .tool-block summary { padding: .65rem .75rem; cursor: pointer; color: #475569; font-size: .77rem; font-weight: 700; }
    .tool-block pre { max-height: 34rem; overflow: auto; padding: .75rem; border-top: 1px solid #e1e6ed; color: #334155; font: 400 .76rem/1.58 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
    .empty { padding: 4rem 1rem; text-align: center; color: #7b8494; font-size: .86rem; }
    .handoff-toolbar { display: flex; justify-content: flex-end; gap: .55rem; padding: .8rem 1rem; border-bottom: 1px solid #edf0f4; background: #fbfcfd; }
    .handoff-text { max-height: 68vh; overflow: auto; padding: 1.25rem; color: #273247; font: 400 .82rem/1.7 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
    .toast { position: fixed; right: 1.25rem; bottom: 1.25rem; z-index: 10; padding: .7rem .9rem; border-radius: .55rem; background: #172033; color: #fff; box-shadow: 0 .6rem 2rem rgba(16, 24, 40, .22); font-size: .8rem; }
    @media (max-width: 720px) {
      .site-header { padding-inline: 1rem; }
      main { width: min(100% - 1rem, 76rem); padding-top: 1.25rem; }
      .viewer-header { grid-template-columns: 1fr; align-items: start; }
      .header-actions { justify-content: flex-start; }
      .summary-item { flex: 1 1 50%; border-bottom: 1px solid #edf0f4; }
      .toolbar { align-items: stretch; flex-direction: column; }
      .search { flex-basis: auto; width: 100%; }
      .filters { align-self: flex-start; }
      .tabs { gap: 1rem; }
      .message-list { padding: .65rem; }
      .header-note { display: none; }
    }
    @media (prefers-reduced-motion: reduce) { *, *::before, *::after { scroll-behavior: auto !important; animation-duration: .01ms !important; animation-iteration-count: 1 !important; } }
  </style>
</head>
<body>
  <header class="site-header">
    <div class="brand">Relay</div>
    <div class="header-note">开发会话分享</div>
  </header>
  <main>
    <section class="state-card" id="loading" aria-live="polite">
      <h1>正在读取分享内容</h1>
      <p>正在验证链接并读取内容。</p>
      <div class="loading-line" aria-hidden="true"></div>
    </section>

    <section class="state-card" id="error" hidden role="alert">
      <h1 id="error-title">无法读取分享</h1>
      <p id="error-message">分享内容没有成功下载或验证。</p>
    </section>

    <section id="viewer" hidden>
      <header class="viewer-header">
        <div>
          <h1 id="share-title">未命名会话</h1>
          <p class="project-name">项目：<span id="share-project">未命名项目</span></p>
        </div>
        <div class="header-actions">
          <button class="button" id="copy-link" type="button">复制链接</button>
          <button class="button primary" id="download-package" type="button">下载分享文件</button>
        </div>
      </header>

      <div class="summary-bar" aria-label="分享信息">
        <div class="summary-item"><span>来源应用</span><strong id="share-agent">ChatGPT</strong></div>
        <div class="summary-item"><span>记录数量</span><strong id="message-count">0</strong></div>
        <div class="summary-item"><span>工具记录</span><strong id="tool-count">0</strong></div>
        <div class="summary-item"><span>有效期至</span><strong id="share-expiry">时间未知</strong></div>
      </div>

      <p class="share-note">此链接可以查看分享内容，请不要转发给无关人员。工具记录只供阅读，不会运行。下载分享文件并用 Relay 打开后，可以保存发送者选择的文件，并导入到 ChatGPT 或 Claude Code。</p>

      <div class="tabs" role="tablist" aria-label="分享内容">
        <button class="tab is-active" type="button" role="tab" aria-selected="true" data-tab="transcript">聊天记录</button>
        <button class="tab" type="button" role="tab" aria-selected="false" data-tab="handoff">项目说明</button>
      </div>

      <section class="panel" id="transcript-panel" role="tabpanel">
        <div class="toolbar">
          <input class="search" id="message-search" type="search" placeholder="搜索会话内容" aria-label="搜索会话内容">
          <div class="filters" aria-label="记录类型">
            <button class="filter is-active" type="button" data-filter="all">全部</button>
            <button class="filter" type="button" data-filter="conversation">消息</button>
            <button class="filter" type="button" data-filter="tools">工具</button>
          </div>
        </div>
        <div class="message-list" id="message-list"></div>
        <div class="empty" id="message-empty" hidden>没有符合条件的记录，请更换搜索内容或记录类型。</div>
      </section>

      <section class="panel" id="handoff-panel" role="tabpanel" hidden>
        <div class="handoff-toolbar">
          <button class="button" id="copy-handoff" type="button">复制项目说明</button>
        </div>
        <pre class="handoff-text" id="handoff-text"></pre>
      </section>
    </section>
  </main>
  <div class="toast" id="toast" hidden role="status"></div>
  <script nonce="${nonce}">${receiverScript}</script>
</body>
</html>`;
  return new Response(html, {
    status: 200,
    headers: {
      "Content-Type": "text/html; charset=utf-8",
      "Content-Security-Policy":
        `default-src 'none'; script-src 'nonce-${nonce}'; style-src 'nonce-${nonce}'; connect-src 'self'; img-src 'none'; font-src 'none'; media-src 'none'; object-src 'none'; frame-src 'none'; worker-src 'none'; manifest-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`,
    },
  });
}

function containsSensitiveQuery(url: URL): boolean {
  const sensitive = new Set([
    "k",
    "key",
    "secret",
    "token",
    "upload_token",
    "revoke_token",
    "decryption_key",
  ]);
  for (const key of url.searchParams.keys()) {
    if (sensitive.has(key.toLowerCase())) {
      return true;
    }
  }
  return false;
}

function isPlainObject(value: unknown): value is CreateShareInput {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
