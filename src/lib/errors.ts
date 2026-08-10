type ErrorRecord = Record<string, unknown>;

function isRecord(value: unknown): value is ErrorRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function errorCode(error: unknown): string | undefined {
  if (!isRecord(error) || typeof error.code !== "string") return undefined;
  const value = error.code.trim();
  return value || undefined;
}

export function technicalErrorMessage(error: unknown): string {
  if (error instanceof Error && error.message.trim()) return error.message.trim();
  if (typeof error === "string" && error.trim()) return error.trim();
  if (isRecord(error)) {
    for (const value of [error.message, error.error, error.reason]) {
      if (typeof value === "string" && value.trim()) return value.trim();
    }
  }
  return "";
}

const knownMessages: Record<string, string> = {
  adapter_not_found: "Relay 安装不完整，请重新安装当前版本。",
  adapter_start_error: "会话读取组件无法启动，请重新打开 Relay 后重试。",
  adapter_timeout: "本机会话较多或文件较大，读取时间超过限制。请稍后重试。",
  adapter_incompatible: "会话读取组件与 Relay 版本不一致，请重新安装当前版本。",
  adapter_protocol_error: "本机会话返回的数据无法验证，请重新安装当前版本。",
  git_not_found: "没有找到 Git。请先安装 Git，再恢复代码改动。",
  git_timeout: "Git 操作等待时间过长，请确认项目目录可以正常访问后重试。",
  invalid_output_path: "所选保存位置不可用，请重新选择。",
  invalid_path: "所选文件或目录不可用，请重新选择。",
  invalid_share_link: "分享链接格式不正确，请完整复制发送者提供的链接。",
  invalid_share_service: "分享服务地址无效，请更新 Relay 后重试。",
  relaypack_invalid: "分享文件无法读取，可能已经损坏或不受当前版本支持。",
  relaypack_key_invalid: "文件密码不正确，请确认已经完整复制。",
  relaypack_too_large: "分享文件超过当前版本允许的大小。",
  share_network_error: "无法连接分享服务，请检查网络或代理设置后重试。",
  share_protocol_error: "分享服务返回的数据无法验证，请稍后重试。",
  share_read_failed: "分享内容下载失败，请稍后重试。",
  unsafe_path: "分享内容包含不安全的文件路径，Relay 已停止处理。",
};

export function userErrorMessage(
  error: unknown,
  fallback = "操作未完成，请稍后重试。",
): string {
  const code = errorCode(error);
  if (code && knownMessages[code]) return knownMessages[code];

  const technical = technicalErrorMessage(error);
  if (!technical) return fallback;
  if (/连接分享服务|无法连接分享服务|分享服务请求失败/u.test(technical)) {
    return technical;
  }
  if (/not allowed by the user agent|platform in the current context/iu.test(technical)) {
    return "当前系统不允许执行此操作。请确认你正在使用 Relay 桌面应用，并已允许所需权限。";
  }
  if (/[^\u0000-\u007f]/u.test(technical)) return technical;
  return fallback;
}
