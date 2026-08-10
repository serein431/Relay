import { writeText } from "@tauri-apps/plugin-clipboard-manager";

export async function copyText(value: string): Promise<void> {
  if (typeof window !== "undefined" && "__TAURI_INTERNALS__" in window) {
    await writeText(value);
    return;
  }
  throw new Error("请在 Relay 桌面应用中使用复制功能");
}
