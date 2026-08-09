import { Decompress } from "fzstd";

const PACKAGE_MAGIC = new TextEncoder().encode("RELAYPK1");
const NONCE_LENGTH = 12;
const KEY_LENGTH = 32;
const MAX_CIPHERTEXT_BYTES = 32 * 1024 * 1024;
const MAX_PLAINTEXT_BYTES = 32 * 1024 * 1024;
const MAX_HANDOFF_BYTES = 2 * 1024 * 1024;

export interface BrowserPackageEnvelope {
  schema: string;
  package_id: string;
  handoff: Record<string, unknown>;
  payloads: BrowserPackagePayload[];
}

interface BrowserPackagePayload {
  asset_id: string;
  archive_path: string;
  kind: string;
  byte_length: number;
  sha256: string;
  data_base64: string;
}

export interface BrowserRelaypack {
  envelope: BrowserPackageEnvelope;
  handoffMarkdown: string;
  ciphertextSha256: string;
}

export async function decodeBrowserRelaypack(
  packageBytes: Uint8Array,
  keyFragment: string,
  expectedSha256?: string,
): Promise<BrowserRelaypack> {
  if (packageBytes.byteLength < PACKAGE_MAGIC.length + NONCE_LENGTH + 16) {
    throw new Error("relaypack_invalid");
  }
  if (packageBytes.byteLength > MAX_CIPHERTEXT_BYTES) {
    throw new Error("relaypack_too_large");
  }
  for (let index = 0; index < PACKAGE_MAGIC.length; index += 1) {
    if (packageBytes[index] !== PACKAGE_MAGIC[index]) {
      throw new Error("relaypack_invalid");
    }
  }

  const ciphertextSha256 = await sha256Hex(packageBytes);
  if (expectedSha256 && ciphertextSha256 !== expectedSha256.toLowerCase()) {
    throw new Error("relaypack_digest_mismatch");
  }

  const keyBytes = decodeBase64Url(keyFragment);
  if (keyBytes.byteLength !== KEY_LENGTH) {
    throw new Error("relaypack_key_invalid");
  }
  const key = await crypto.subtle.importKey(
    "raw",
    ownedArrayBuffer(keyBytes),
    { name: "AES-GCM" },
    false,
    ["decrypt"],
  );
  const nonceStart = PACKAGE_MAGIC.length;
  const cipherStart = nonceStart + NONCE_LENGTH;
  let compressed: ArrayBuffer;
  try {
    compressed = await crypto.subtle.decrypt(
      {
        name: "AES-GCM",
        iv: packageBytes.slice(nonceStart, cipherStart),
        additionalData: PACKAGE_MAGIC,
        tagLength: 128,
      },
      key,
      packageBytes.slice(cipherStart),
    );
  } catch {
    throw new Error("relaypack_auth_failed");
  }

  const plaintext = decompressLimited(new Uint8Array(compressed), MAX_PLAINTEXT_BYTES);
  let parsed: unknown;
  try {
    parsed = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(plaintext));
  } catch {
    throw new Error("relaypack_invalid");
  }
  const envelope = validateEnvelope(parsed);
  const handoffMarkdown = await readHandoffMarkdown(envelope.payloads);
  return { envelope, handoffMarkdown, ciphertextSha256 };
}

export function decodeBase64Url(value: string): Uint8Array {
  if (!/^[A-Za-z0-9_-]{43}$/u.test(value)) {
    throw new Error("relaypack_key_invalid");
  }
  const padded = value.replaceAll("-", "+").replaceAll("_", "/") + "=";
  let binary: string;
  try {
    binary = atob(padded);
  } catch {
    throw new Error("relaypack_key_invalid");
  }
  return Uint8Array.from(binary, (character) => character.charCodeAt(0));
}

export async function sha256Hex(value: Uint8Array): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", ownedArrayBuffer(value));
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function ownedArrayBuffer(value: Uint8Array): ArrayBuffer {
  return Uint8Array.from(value).buffer;
}

export function zstdWindowSize(value: Uint8Array): number {
  if (
    value.byteLength < 6 ||
    value[0] !== 0x28 ||
    value[1] !== 0xb5 ||
    value[2] !== 0x2f ||
    value[3] !== 0xfd
  ) {
    throw new Error("relaypack_compression_invalid");
  }
  const descriptor = value[4] as number;
  if ((descriptor & 0x08) !== 0) {
    throw new Error("relaypack_compression_invalid");
  }
  const singleSegment = (descriptor & 0x20) !== 0;
  const contentSizeFlag = descriptor >>> 6;
  const dictionaryFlag = descriptor & 0x03;
  let cursor = 5;
  let windowSize: number | undefined;
  if (!singleSegment) {
    const windowDescriptor = value[cursor];
    if (windowDescriptor === undefined) throw new Error("relaypack_compression_invalid");
    cursor += 1;
    const windowLog = 10 + (windowDescriptor >>> 3);
    const windowBase = 2 ** windowLog;
    windowSize = windowBase + (windowBase >>> 3) * (windowDescriptor & 0x07);
  }
  cursor += [0, 1, 2, 4][dictionaryFlag] as number;
  const contentSizeLength = contentSizeFlag === 0
    ? (singleSegment ? 1 : 0)
    : contentSizeFlag === 1
      ? 2
      : contentSizeFlag === 2
        ? 4
        : 8;
  if (cursor + contentSizeLength > value.byteLength) {
    throw new Error("relaypack_compression_invalid");
  }
  let contentSize: number | undefined;
  if (contentSizeLength > 0) {
    contentSize = readLittleEndian(value, cursor, contentSizeLength);
    if (contentSizeLength === 2) contentSize += 256;
  }
  if (singleSegment) {
    if (contentSize === undefined) throw new Error("relaypack_compression_invalid");
    windowSize = contentSize;
  }
  if (windowSize === undefined || !Number.isSafeInteger(windowSize) || windowSize < 1) {
    throw new Error("relaypack_compression_invalid");
  }
  if (windowSize > MAX_PLAINTEXT_BYTES || (contentSize ?? 0) > MAX_PLAINTEXT_BYTES) {
    throw new Error("relaypack_too_large");
  }
  return windowSize;
}

function decompressLimited(value: Uint8Array, limit: number): Uint8Array {
  zstdWindowSize(value);
  const chunks: Uint8Array[] = [];
  let length = 0;
  const decoder = new Decompress((chunk) => {
    length += chunk.byteLength;
    if (length > limit) throw new Error("relaypack_too_large");
    chunks.push(chunk.slice());
  });
  try {
    decoder.push(value, true);
  } catch (error) {
    if (error instanceof Error && error.message === "relaypack_too_large") throw error;
    throw new Error("relaypack_compression_invalid");
  }
  const output = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    output.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return output;
}

function readLittleEndian(value: Uint8Array, offset: number, length: number): number {
  let result = 0;
  let multiplier = 1;
  for (let index = 0; index < length; index += 1) {
    const byte = value[offset + index];
    if (byte === undefined) throw new Error("relaypack_compression_invalid");
    result += byte * multiplier;
    multiplier *= 256;
    if (!Number.isSafeInteger(result) || !Number.isSafeInteger(multiplier)) {
      throw new Error("relaypack_too_large");
    }
  }
  return result;
}

function validateEnvelope(value: unknown): BrowserPackageEnvelope {
  if (!isRecord(value)) throw new Error("relaypack_invalid");
  if (value.schema !== "relay.package.v1" || typeof value.package_id !== "string") {
    throw new Error("relaypack_invalid");
  }
  if (!isRecord(value.handoff) || value.handoff.schema !== "relay.handoff.v1") {
    throw new Error("relaypack_invalid");
  }
  if (!Array.isArray(value.payloads)) throw new Error("relaypack_invalid");
  const payloads = value.payloads.map((payload) => {
    if (!isRecord(payload)) throw new Error("relaypack_invalid");
    if (
      typeof payload.asset_id !== "string" ||
      typeof payload.archive_path !== "string" ||
      typeof payload.kind !== "string" ||
      !Number.isSafeInteger(payload.byte_length) ||
      (payload.byte_length as number) < 0 ||
      typeof payload.sha256 !== "string" ||
      !/^[a-f0-9]{64}$/u.test(payload.sha256) ||
      typeof payload.data_base64 !== "string"
    ) {
      throw new Error("relaypack_invalid");
    }
    return payload as unknown as BrowserPackagePayload;
  });
  return {
    schema: value.schema,
    package_id: value.package_id,
    handoff: value.handoff,
    payloads,
  };
}

async function readHandoffMarkdown(payloads: BrowserPackagePayload[]): Promise<string> {
  const matches = payloads.filter((payload) =>
    payload.kind === "handoff_document" && payload.archive_path === "handoff/HANDOFF.md"
  );
  if (matches.length !== 1) throw new Error("relaypack_invalid");
  const payload = matches[0] as BrowserPackagePayload;
  if (payload.byte_length > MAX_HANDOFF_BYTES || payload.data_base64.length > MAX_HANDOFF_BYTES * 2) {
    throw new Error("relaypack_too_large");
  }
  const bytes = decodeBase64(payload.data_base64);
  if (bytes.byteLength !== payload.byte_length || await sha256Hex(bytes) !== payload.sha256) {
    throw new Error("relaypack_invalid");
  }
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    throw new Error("relaypack_invalid");
  }
}

function decodeBase64(value: string): Uint8Array {
  let binary: string;
  try {
    binary = atob(value);
  } catch {
    throw new Error("relaypack_invalid");
  }
  return Uint8Array.from(binary, (character) => character.charCodeAt(0));
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
