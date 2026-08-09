import { writeText } from "@tauri-apps/plugin-clipboard-manager";

export async function copyText(value: string): Promise<void> {
  if (typeof window !== "undefined" && "__TAURI_INTERNALS__" in window) {
    await writeText(value);
    return;
  }
  if (typeof navigator !== "undefined" && navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(value);
    return;
  }
  throw new Error("当前环境不支持自动复制");
}
