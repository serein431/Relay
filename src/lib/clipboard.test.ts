import { afterEach, describe, expect, it, vi } from "vitest";

const clipboardPlugin = vi.hoisted(() => ({
  writeText: vi.fn(),
}));

vi.mock("@tauri-apps/plugin-clipboard-manager", () => ({
  writeText: clipboardPlugin.writeText,
}));

import { copyText } from "./clipboard";

describe("copyText", () => {
  afterEach(() => {
    clipboardPlugin.writeText.mockReset();
    vi.unstubAllGlobals();
  });

  it("uses the native Tauri clipboard when the desktop bridge is available", async () => {
    vi.stubGlobal("window", { __TAURI_INTERNALS__: {} });
    vi.stubGlobal("navigator", { clipboard: { writeText: vi.fn() } });

    await copyText("relay link");

    expect(clipboardPlugin.writeText).toHaveBeenCalledWith("relay link");
  });

  it("rejects browser-only previews instead of using a second clipboard path", async () => {
    const browserWriteText = vi.fn();
    vi.stubGlobal("window", {});
    vi.stubGlobal("navigator", { clipboard: { writeText: browserWriteText } });

    await expect(copyText("preview text")).rejects.toThrow("请在 Relay 桌面应用中使用复制功能");

    expect(browserWriteText).not.toHaveBeenCalled();
    expect(clipboardPlugin.writeText).not.toHaveBeenCalled();
  });
});
