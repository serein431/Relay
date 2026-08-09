import {
  arrayBufferToHex,
  constantTimeEqualHex,
  hashCapability,
  hexToBase64,
  isSha256Hex,
} from "./crypto";
import {
  apiError,
  bearerToken,
  jsonResponse,
  methodNotAllowed,
} from "./http";
import type {
  Env,
  InitShareRequest,
  ShareRecord,
  UploadAuthorization,
} from "./types";

const RECORD_KEY = "share";
const RETRY_DELETE_AFTER_MS = 60_000;

export class RelayShare {
  constructor(
    private readonly state: DurableObjectState,
    private readonly env: Env,
  ) {}

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/internal/init") {
      return request.method === "POST"
        ? this.initialize(request)
        : methodNotAllowed(["POST"]);
    }
    if (url.pathname === "/internal/upload/authorize") {
      return request.method === "POST"
        ? this.authorizeUpload(request)
        : methodNotAllowed(["POST"]);
    }
    if (url.pathname === "/internal/upload/complete") {
      return request.method === "POST"
        ? this.completeUpload(request)
        : methodNotAllowed(["POST"]);
    }

    const isBlob = url.pathname.endsWith("/blob");
    if (isBlob) {
      if (request.method === "GET" || request.method === "HEAD") {
        return this.download(request);
      }
      return methodNotAllowed(["GET", "HEAD"]);
    }

    if (request.method === "GET" || request.method === "HEAD") {
      return this.publicInfo(request.method === "HEAD");
    }
    if (request.method === "DELETE") {
      return this.revoke(request);
    }
    return methodNotAllowed(["GET", "HEAD", "DELETE"]);
  }

  async alarm(): Promise<void> {
    const record = await this.state.storage.get<ShareRecord>(RECORD_KEY);
    if (record === undefined) {
      await this.state.storage.deleteAlarm();
      return;
    }

    const now = Date.now();
    const deadline = activeDeadline(record);
    if (record.status === "revoked" || record.status === "expired" || now >= deadline) {
      if (record.status !== "revoked") {
        record.status = "expired";
        await this.state.storage.put(RECORD_KEY, record);
      }
      await this.removeCiphertextAndMetadata(record);
      return;
    }

    await this.state.storage.setAlarm(deadline);
  }

  private async initialize(request: Request): Promise<Response> {
    let input: InitShareRequest;
    try {
      input = (await request.json()) as InitShareRequest;
    } catch {
      return apiError(400, "invalid_internal_request", "The share reservation is invalid.");
    }

    const record = input?.record;
    if (!isValidInitialRecord(record)) {
      return apiError(400, "invalid_internal_request", "The share reservation is invalid.");
    }

    return this.state.blockConcurrencyWhile(async () => {
      const existing = await this.state.storage.get<ShareRecord>(RECORD_KEY);
      if (existing !== undefined) {
        return apiError(409, "share_id_collision", "The generated share identifier already exists.");
      }
      const existingObject = await this.env.RELAY_BLOBS.head(record.objectKey);
      if (existingObject !== null) {
        return apiError(409, "object_key_collision", "The generated object key already exists.");
      }
      await this.state.storage.put(RECORD_KEY, record);
      await this.state.storage.setAlarm(activeDeadline(record));
      return new Response(null, { status: 201 });
    });
  }

  private async publicInfo(headOnly: boolean): Promise<Response> {
    const record = await this.loadUsableRecord();
    if (record === null || record.status === "revoked" || record.status === "expired") {
      return apiError(404, "share_not_found", "The share does not exist or is no longer available.");
    }

    const response = jsonResponse(publicMetadata(record));
    if (!headOnly) {
      return response;
    }
    return new Response(null, { status: response.status, headers: response.headers });
  }

  private async authorizeUpload(request: Request): Promise<Response> {
    const record = await this.loadUsableRecord();
    if (record === null || record.status === "revoked" || record.status === "expired") {
      return apiError(404, "share_not_found", "The share does not exist or is no longer available.");
    }

    const token = bearerToken(request);
    if (token === null) {
      return apiError(401, "upload_token_required", "A valid upload token is required.", {
        "WWW-Authenticate": 'Bearer realm="relay-upload"',
      });
    }
    const tokenHash = await hashCapability("upload", token);
    if (!constantTimeEqualHex(tokenHash, record.uploadTokenHash)) {
      return apiError(403, "invalid_upload_token", "The upload token is invalid.");
    }

    if (record.status !== "awaiting_upload" && record.status !== "ready") {
      return apiError(409, "upload_state_conflict", "This share cannot accept a ciphertext upload.");
    }

    const contentType = request.headers.get("Content-Type")?.trim().toLowerCase();
    if (contentType !== "application/octet-stream") {
      return apiError(415, "unsupported_media_type", "Ciphertext must use application/octet-stream.");
    }
    const contentEncoding = request.headers.get("Content-Encoding");
    if (contentEncoding !== null && contentEncoding.toLowerCase() !== "identity") {
      return apiError(415, "content_encoding_not_allowed", "Encoded request bodies are not accepted.");
    }

    const lengthHeader = request.headers.get("Content-Length");
    if (lengthHeader === null) {
      return apiError(411, "content_length_required", "Content-Length is required for ciphertext uploads.");
    }
    if (!/^[1-9][0-9]*$/u.test(lengthHeader)) {
      return apiError(400, "invalid_content_length", "Content-Length must be a positive integer.");
    }
    const actualLength = Number(lengthHeader);
    if (!Number.isSafeInteger(actualLength)) {
      return apiError(400, "invalid_content_length", "Content-Length must be a positive integer.");
    }
    if (actualLength !== record.ciphertextBytes) {
      return apiError(409, "ciphertext_size_conflict", "The ciphertext size differs from the reservation.");
    }

    const suppliedSha256 = request.headers.get("X-Relay-Ciphertext-Sha256")?.toLowerCase();
    if (suppliedSha256 !== record.ciphertextSha256) {
      return apiError(409, "ciphertext_digest_conflict", "The ciphertext digest differs from the reservation.");
    }

    if (record.status === "ready") {
      const existing = await this.env.RELAY_BLOBS.head(record.objectKey);
      if (!storedObjectMatchesRecord(existing, record)) {
        return apiError(409, "stored_ciphertext_conflict", "The stored ciphertext is not consistent with this immutable share.");
      }
      return jsonResponse(uploadAuthorization(record, true, existing.etag), 200, {
        ETag: existing.httpEtag,
      });
    }

    return jsonResponse(uploadAuthorization(record, false), 200);
  }

  private async completeUpload(_request: Request): Promise<Response> {
    return this.state.blockConcurrencyWhile(async () => {
      const completionTime = Date.now();
      const current = await this.state.storage.get<ShareRecord>(RECORD_KEY);
      if (current === undefined || current.status === "revoked" || current.status === "expired") {
        return apiError(404, "share_not_found", "The share does not exist or is no longer available.");
      }
      if (completionTime >= activeDeadline(current)) {
        current.status = "expired";
        await this.state.storage.put(RECORD_KEY, current);
        await this.removeCiphertextAndMetadata(current);
        return apiError(404, "share_not_found", "The share does not exist or is no longer available.");
      }

      const stored = await this.env.RELAY_BLOBS.head(current.objectKey);
      if (current.status === "ready") {
        if (!storedObjectMatchesRecord(stored, current)) {
          return apiError(409, "stored_ciphertext_conflict", "The stored ciphertext is not consistent with this immutable share.");
        }
        return jsonResponse(publicMetadata(current), 200, { ETag: stored.httpEtag });
      }
      if (current.status !== "awaiting_upload") {
        await this.env.RELAY_BLOBS.delete(current.objectKey);
        return apiError(409, "upload_state_conflict", "This share cannot accept a ciphertext upload.");
      }
      if (!storedObjectMatchesRecord(stored, current)) {
        await this.env.RELAY_BLOBS.delete(current.objectKey);
        return apiError(422, "stored_ciphertext_invalid", "The stored ciphertext failed its size or digest check.");
      }

      current.status = "ready";
      current.completedAt = completionTime;
      current.etag = stored.etag;
      await this.state.storage.put(RECORD_KEY, current);
      await this.state.storage.setAlarm(current.expiresAt);
      return jsonResponse(publicMetadata(current), 201, { ETag: stored.httpEtag });
    });
  }

  private async download(request: Request): Promise<Response> {
    const record = await this.loadUsableRecord();
    if (record === null || record.status === "revoked" || record.status === "expired") {
      return apiError(404, "share_not_found", "The share does not exist or is no longer available.");
    }
    if (record.status !== "ready") {
      return apiError(409, "share_not_ready", "The ciphertext upload has not completed.");
    }

    const object = request.method === "HEAD"
      ? await this.env.RELAY_BLOBS.head(record.objectKey)
      : await this.env.RELAY_BLOBS.get(record.objectKey);
    if (object === null) {
      return apiError(404, "ciphertext_not_found", "The ciphertext is no longer available.");
    }
    if (object.size !== record.ciphertextBytes) {
      return apiError(502, "stored_ciphertext_invalid", "The stored ciphertext failed its size check.");
    }
    const storedSha256 = object.checksums.sha256;
    if (
      storedSha256 !== undefined &&
      arrayBufferToHex(storedSha256) !== record.ciphertextSha256
    ) {
      return apiError(502, "stored_ciphertext_invalid", "The stored ciphertext failed its digest check.");
    }

    const headers = new Headers();
    headers.set("Content-Type", "application/octet-stream");
    headers.set("Content-Disposition", 'attachment; filename="relay-share.bin"');
    headers.set("Content-Length", String(record.ciphertextBytes));
    headers.set("ETag", object.httpEtag);
    headers.set("Digest", `sha-256=${hexToBase64(record.ciphertextSha256)}`);
    headers.set("X-Relay-Ciphertext-Sha256", record.ciphertextSha256);

    if (request.method === "HEAD") {
      return new Response(null, { status: 200, headers });
    }
    const body = (object as R2ObjectBody).body;
    return new Response(body, { status: 200, headers });
  }

  private async revoke(request: Request): Promise<Response> {
    const record = await this.loadUsableRecord();
    if (record === null || record.status === "expired") {
      return apiError(404, "share_not_found", "The share does not exist or is no longer available.");
    }

    const token = bearerToken(request);
    if (token === null) {
      return apiError(401, "revoke_token_required", "A valid revoke token is required.", {
        "WWW-Authenticate": 'Bearer realm="relay-revoke"',
      });
    }
    const tokenHash = await hashCapability("revoke", token);
    if (!constantTimeEqualHex(tokenHash, record.revokeTokenHash)) {
      return apiError(403, "invalid_revoke_token", "The revoke token is invalid.");
    }

    record.status = "revoked";
    await this.state.storage.put(RECORD_KEY, record);
    await this.removeCiphertextAndMetadata(record);
    return new Response(null, { status: 204 });
  }

  private async loadUsableRecord(): Promise<ShareRecord | null> {
    const record = await this.state.storage.get<ShareRecord>(RECORD_KEY);
    if (record === undefined) {
      return null;
    }
    if (record.status === "revoked" || record.status === "expired") {
      return record;
    }
    if (Date.now() < activeDeadline(record)) {
      return record;
    }
    record.status = "expired";
    await this.state.storage.put(RECORD_KEY, record);
    await this.removeCiphertextAndMetadata(record);
    return null;
  }

  private async removeCiphertextAndMetadata(record: ShareRecord): Promise<void> {
    try {
      await this.env.RELAY_BLOBS.delete(record.objectKey);
      await this.state.storage.deleteAll();
      await this.state.storage.deleteAlarm();
    } catch {
      await this.state.storage.setAlarm(Date.now() + RETRY_DELETE_AFTER_MS);
    }
  }
}

function uploadAuthorization(
  record: ShareRecord,
  alreadyReady: boolean,
  etag?: string,
): UploadAuthorization {
  return {
    object_key: record.objectKey,
    ciphertext_bytes: record.ciphertextBytes,
    ciphertext_sha256: record.ciphertextSha256,
    already_ready: alreadyReady,
    expires_at: new Date(record.expiresAt).toISOString(),
    ...(etag === undefined ? {} : { etag }),
  };
}


function isValidInitialRecord(record: ShareRecord | undefined): record is ShareRecord {
  return (
    record !== undefined &&
    record.version === 1 &&
    record.status === "awaiting_upload" &&
    /^ciphertext\/[A-Za-z0-9_-]{43}$/u.test(record.objectKey) &&
    Number.isSafeInteger(record.createdAt) &&
    Number.isSafeInteger(record.expiresAt) &&
    record.expiresAt > record.createdAt &&
    (record.uploadExpiresAt === undefined ||
      (Number.isSafeInteger(record.uploadExpiresAt) &&
        record.uploadExpiresAt > record.createdAt &&
        record.uploadExpiresAt <= record.expiresAt)) &&
    Number.isSafeInteger(record.ciphertextBytes) &&
    record.ciphertextBytes > 0 &&
    isSha256Hex(record.ciphertextSha256) &&
    isSha256Hex(record.uploadTokenHash) &&
    isSha256Hex(record.revokeTokenHash)
  );
}

function publicMetadata(record: ShareRecord): Record<string, unknown> {
  const metadata: Record<string, unknown> = {
    schema: "relay.share.public.v1",
    status: record.status,
    expires_at: new Date(record.expiresAt).toISOString(),
    ciphertext: {
      bytes: record.ciphertextBytes,
      sha256: record.ciphertextSha256,
      content_type: "application/octet-stream",
    },
  };
  if (record.status === "awaiting_upload" && record.uploadExpiresAt !== undefined) {
    metadata.upload_expires_at = new Date(record.uploadExpiresAt).toISOString();
  }
  return metadata;
}

function activeDeadline(record: ShareRecord): number {
  if (record.status !== "awaiting_upload") {
    return record.expiresAt;
  }
  return Math.min(record.uploadExpiresAt ?? record.expiresAt, record.expiresAt);
}

function storedObjectMatchesRecord(
  object: R2Object | null,
  record: ShareRecord,
): object is R2Object {
  if (object === null || object.size !== record.ciphertextBytes) {
    return false;
  }
  const storedSha256 = object.checksums.sha256;
  if (storedSha256 === undefined || arrayBufferToHex(storedSha256) !== record.ciphertextSha256) {
    return false;
  }
  return record.etag === undefined || object.etag === record.etag;
}
