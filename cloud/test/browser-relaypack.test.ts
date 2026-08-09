import { describe, expect, it } from "vitest";
import {
  decodeBase64Url,
  decodeBrowserRelaypack,
  zstdWindowSize,
} from "../src/browser-relaypack";

const MINIMUM_RELAYPACK_BYTES = 8 + 12 + 16;

describe("browser relaypack validation", () => {
  it("rejects malformed base64url keys", () => {
    for (const value of ["", "A".repeat(42), "A".repeat(44), `${"A".repeat(42)}+`]) {
      expect(() => decodeBase64Url(value)).toThrowError("relaypack_key_invalid");
    }
  });

  it("rejects bytes without the RELAYPK1 file header", async () => {
    const bytes = new Uint8Array(MINIMUM_RELAYPACK_BYTES);
    await expect(decodeBrowserRelaypack(bytes, "A".repeat(43)))
      .rejects.toThrowError("relaypack_invalid");
  });

  it("rejects a zstd frame whose declared window is too large", () => {
    const frame = Uint8Array.of(0x28, 0xb5, 0x2f, 0xfd, 0x00, 0xff);
    expect(() => zstdWindowSize(frame)).toThrowError("relaypack_too_large");
  });

  it("rejects ciphertext that does not match the public digest", async () => {
    const bytes = new Uint8Array(MINIMUM_RELAYPACK_BYTES);
    bytes.set(new TextEncoder().encode("RELAYPK1"));
    await expect(decodeBrowserRelaypack(bytes, "A".repeat(43), "f".repeat(64)))
      .rejects.toThrowError("relaypack_digest_mismatch");
  });
});
