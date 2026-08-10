import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  open: vi.fn(),
  save: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: mocks.invoke,
  isTauri: () => true,
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: mocks.open,
  save: mocks.save,
}));

import { chooseDirectory } from "./dialog";

describe("文件夹选择", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("返回已经确认的普通文件夹", async () => {
    mocks.open.mockResolvedValue("/Users/demo/Downloads");
    mocks.invoke.mockResolvedValue({
      exists: true,
      is_directory: true,
      is_symlink: false,
    });

    await expect(chooseDirectory("选择文件夹")).resolves.toBe("/Users/demo/Downloads");
    expect(mocks.invoke).toHaveBeenCalledWith("inspect_path", {
      path: "/Users/demo/Downloads",
    });
  });

  it("拒绝文件选择器误返回的普通文件", async () => {
    mocks.open.mockResolvedValue("/Users/demo/Downloads/share.relaypack");
    mocks.invoke.mockResolvedValue({
      exists: true,
      is_directory: false,
      is_symlink: false,
    });

    await expect(chooseDirectory("选择文件夹")).rejects.toThrow("请选择一个本机文件夹");
  });

  it("拒绝符号链接目录", async () => {
    mocks.open.mockResolvedValue("/Users/demo/Downloads/link");
    mocks.invoke.mockResolvedValue({
      exists: true,
      is_directory: false,
      is_symlink: true,
    });

    await expect(chooseDirectory("选择文件夹")).rejects.toThrow("符号链接");
  });
});
