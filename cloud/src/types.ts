export interface Env {
  RELAY_BLOBS: R2Bucket;
  RELAY_SHARES: DurableObjectNamespace;
  RELAY_RATE_LIMITS: DurableObjectNamespace;
  PUBLIC_BASE_URL?: string;
  ALLOWED_ORIGINS?: string;
  UPLOAD_TOKEN?: string;
  RATE_LIMIT_SALT?: string;
  MAX_CIPHERTEXT_BYTES?: string;
  DEFAULT_TTL_SECONDS?: string;
  MIN_TTL_SECONDS?: string;
  MAX_TTL_SECONDS?: string;
  UPLOAD_EXPIRY_SECONDS?: string;
  CREATE_RATE_LIMIT?: string;
  UPLOAD_RATE_LIMIT?: string;
  DOWNLOAD_RATE_LIMIT?: string;
  REVOKE_RATE_LIMIT?: string;
}

export type ShareStatus = "awaiting_upload" | "ready" | "revoked" | "expired";

export interface ShareRecord {
  version: 1;
  status: ShareStatus;
  objectKey: string;
  createdAt: number;
  expiresAt: number;
  /**
   * Deadline for completing a reserved upload. Older direct-upload records may
   * omit this field and fall back to expiresAt.
   */
  uploadExpiresAt?: number;
  ciphertextBytes: number;
  ciphertextSha256: string;
  uploadTokenHash: string;
  revokeTokenHash: string;
  completedAt?: number;
  etag?: string;
}

export interface InitShareRequest {
  record: ShareRecord;
}

export interface UploadAuthorization {
  object_key: string;
  ciphertext_bytes: number;
  ciphertext_sha256: string;
  already_ready: boolean;
  expires_at: string;
  etag?: string;
}

export interface RateLimitRecord {
  windowStartedAt: number;
  resetAt: number;
  count: number;
}
