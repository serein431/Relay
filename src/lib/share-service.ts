export const DEFAULT_SHARE_SERVICE_BASE_URL =
  "https://relay-share.relay-share-cloud.workers.dev";

function isLoopback(hostname: string): boolean {
  return hostname === "localhost" || hostname === "127.0.0.1" || hostname === "[::1]";
}

export function shareServiceOriginFromLink(raw: string): string {
  let url: URL;
  try {
    url = new URL(raw.trim());
  } catch {
    throw new Error("分享链接格式不正确，请粘贴 Relay 生成的完整链接。");
  }
  if (url.protocol !== "https:" && !(url.protocol === "http:" && isLoopback(url.hostname))) {
    throw new Error("分享链接必须使用 HTTPS；只有本机调试地址可以使用 HTTP。");
  }
  return url.origin;
}
