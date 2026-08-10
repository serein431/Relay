import { isTauri } from "@tauri-apps/api/core";
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
  return singlePath(await open({
    title,
    defaultPath,
    multiple: false,
    directory: true,
  }));
}

export async function chooseRelaypackSavePath(defaultPath: string): Promise<string | null> {
  requireDesktop();
  return save({
    title: "保存 Relay 分享文件",
    defaultPath,
    filters: [{ name: "Relay 分享文件", extensions: ["relaypack"] }],
  });
}
