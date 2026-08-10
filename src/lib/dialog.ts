import { invoke, isTauri } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";

function requireDesktop(): void {
  if (!isTauri()) {
    throw new Error("请在 Relay 桌面应用中使用系统文件选择窗口。");
  }
}

function singlePath(value: string | string[] | null): string | null {
  if (typeof value === "string") return value;
  return Array.isArray(value) ? value[0] ?? null : null;
}

export async function chooseRelaypackFile(defaultPath?: string): Promise<string | null> {
  requireDesktop();
  return singlePath(await open({
    title: "选择 Relay 分享文件",
    defaultPath,
    multiple: false,
    directory: false,
    filters: [{ name: "Relay 分享文件", extensions: ["relaypack"] }],
  }));
}

export async function chooseDirectory(
  title: string,
  defaultPath?: string,
): Promise<string | null> {
  requireDesktop();
  const selected = singlePath(await open({
    title,
    defaultPath,
    multiple: false,
    directory: true,
  }));
  if (!selected) return null;
  const inspection = await invoke<{
    exists: boolean;
    is_directory: boolean;
    is_symlink: boolean;
  }>("inspect_path", { path: selected });
  if (!inspection.exists || !inspection.is_directory || inspection.is_symlink) {
    throw new Error("请选择一个本机文件夹，不能选择普通文件或符号链接。");
  }
  return selected;
}

export async function chooseRelaypackSavePath(defaultPath: string): Promise<string | null> {
  requireDesktop();
  return save({
    title: "保存 Relay 分享文件",
    defaultPath,
    filters: [{ name: "Relay 分享文件", extensions: ["relaypack"] }],
  });
}
