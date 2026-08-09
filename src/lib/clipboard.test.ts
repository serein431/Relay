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

  it("keeps the browser clipboard fallback for the interface preview", async () => {
    const browserWriteText = vi.fn();
    vi.stubGlobal("window", {});
    vi.stubGlobal("navigator", { clipboard: { writeText: browserWriteText } });

    await copyText("preview text");

    expect(browserWriteText).toHaveBeenCalledWith("preview text");
    expect(clipboardPlugin.writeText).not.toHaveBeenCalled();
  });
});
