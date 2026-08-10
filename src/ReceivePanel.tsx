import { useEffect, useMemo, useRef, useState } from "react";
import {
  downloadShare,
  importNativeSession,
  inspectRepository,
  inspectRelaypack,
  restoreRelaypack,
  showChatgptTasks,
} from "./lib/tauri";
import { copyText } from "./lib/clipboard";
import { chooseDirectory, chooseRelaypackFile } from "./lib/dialog";
import { userErrorMessage } from "./lib/errors";
import { shareServiceOriginFromLink } from "./lib/share-service";
import ConversationViewer from "./ConversationViewer";
import LoadingState from "./LoadingState";
import type {
  AgentKind,
  ImportNativeSessionResult,
  InspectRelaypackResult,
  RestoreRelaypackResult,
} from "./types";

type ReceiveSource = "share_link" | "local_file";

export const receiveTargetCopy: Record<AgentKind, { title: string; description: string }> = {
  codex: { title: "导入到 ChatGPT", description: "保存文件并新建一条置顶任务，随后显示 ChatGPT 任务列表。" },
  claude_code: { title: "导入到 Claude Code", description: "保存文件并新建一条会话，随后显示继续命令。" },
};

export const receiveSaveCopy = {
  title: "只保存文件",
  description: "保存发送者选择的文件，不创建任务或会话。",
};

type ReceivePanelProps = {
  home?: string;
  onNotice: (message: string) => void;
};

export function receiveErrorMessage(error: unknown): string {
  const code = typeof error === "object" && error !== null && "code" in error
    ? String((error as { code: unknown }).code)
    : "";
  const known: Record<string, string> = {
    no_importable_content: "发送者未包含可导入的聊天记录或项目说明。已接收的文件仍可保留。",
    session_importer_not_found: "Relay 安装不完整，请重新安装最新版 Relay。",
    invalid_target_cwd: "接收目录不存在，请重新选择保存位置。",
    invalid_agent: "请选择 ChatGPT 或 Claude Code。",
    invalid_target: "所选导入目标不受支持，请更新 Relay 后重试。",
    invalid_request: "导入请求不完整，请重新读取分享内容后重试。",
    not_a_git_repository: "所选文件夹不是 Git 仓库。请选择这个项目在本机的 Git 仓库根目录。",
    repository_path_required: "这份分享包含代码，请先选择这个项目在本机的 Git 仓库。",
    repository_root_required: "请选择 Git 仓库的根目录，不要选择仓库中的子文件夹。",
    repository_identity_mismatch: "所选 Git 仓库与发送者的项目不一致。请重新选择同一个项目的本机仓库。",
    branch_name_required: "无法确定新分支名称，请重新打开这份分享后再试。",
    invalid_branch_name: "新分支名称不可用，请重新打开这份分享后再试。",
    unsafe_target_path: "接收位置不能放在所选 Git 仓库内部，请选择其他文件夹。",
    git_command_failed: "Git 无法从所选仓库恢复代码。请确认仓库可以正常打开，并且是发送者使用的同一个项目。",
    restore_preflight_failed: "所选仓库无法恢复这份代码。请确认这是发送者使用的同一个项目，并先完成仓库中正在进行的 Git 操作。",
    restore_directory_failed: "无法创建接收文件夹，请重新选择接收位置。",
    restore_write_failed: "无法写入接收文件，请检查磁盘空间和文件夹权限。",
    restore_verification_failed: "文件已经写入，但恢复结果检查未通过。本次未完成的内容已撤销。",
    handoff_write_failed: "代码已经恢复，但无法保存随分享附带的会话文件。请检查磁盘空间和文件夹权限。",
    handoff_not_found: "没有找到会话记录，请重新读取或重新保存分享内容。",
    handoff_invalid: "会话记录文件无法读取或格式不受支持。",
    invalid_handoff_path: "会话记录文件不在本次接收目录中，请重新保存分享内容。",
    home_unavailable: "无法确定目标应用的本机数据目录。",
    chatgpt_state_not_found: "没有找到 ChatGPT 的本机任务数据。请先打开一次 ChatGPT，再返回 Relay 重新导入。",
    chatgpt_handler_not_found: "没有找到可打开本机任务的 ChatGPT 应用。请安装或打开官方 ChatGPT 应用。",
    chatgpt_identity_unverified: "本机注册的应用未通过 ChatGPT 签名检查。请使用官方 ChatGPT 应用。",
    chatgpt_signature_check_failed: "Relay 无法完成 ChatGPT 签名检查。请确认官方 ChatGPT 应用安装完整。",
    chatgpt_signature_untrusted: "本机注册的应用未通过 ChatGPT 签名检查。请使用官方 ChatGPT 应用。",
    chatgpt_open_failed: "macOS 未能显示 ChatGPT。请手动打开官方 ChatGPT 应用。",
    chatgpt_catalog_refresh_failed: "Relay 无法通知当前运行的 ChatGPT 重新读取本机任务列表。请重新启动 ChatGPT 后再试。",
    chatgpt_ipc_untrusted: "Relay 未使用权限不安全的 ChatGPT 本机通信文件。请重新启动 ChatGPT 后再试。",
    unsupported_platform: "当前系统不支持由 Relay 显示 ChatGPT。请手动打开 ChatGPT。",
    session_id_failed: "无法为新会话分配编号，请重试。",
    backup_failed: "创建导入前备份失败，Relay 没有开始写入新会话。",
    session_build_failed: "无法生成目标应用可读取的会话记录。",
    session_write_failed: "无法新建会话文件，请检查磁盘空间和目录权限。",
    index_write_failed: "会话文件已经撤销，但会话列表更新失败。",
    state_write_failed: "ChatGPT 任务记录更新失败，本次导入已经撤销。",
    pin_write_failed: "ChatGPT 置顶记录更新失败，本次导入已经撤销。",
    native_import_unverified: "导入后的检查没有全部通过，本次写入已经撤销。",
    native_import_failed: "Relay 未能创建新会话，请重试。",
    rollback_incomplete: "本次导入失败，自动撤销没有全部完成。请保留导入前备份，并查看恢复信息。",
  };
  return known[code] ?? userErrorMessage(error, "接收分享失败，请稍后重试。");
}

function errorBackupPath(error: unknown): string | undefined {
  if (typeof error !== "object" || error === null || !("details" in error)) return undefined;
  const details = (error as { details?: unknown }).details;
  if (typeof details !== "object" || details === null || !("backup_dir" in details)) return undefined;
  const value = (details as { backup_dir?: unknown }).backup_dir;
  return typeof value === "string" && value ? value : undefined;
}

function errorCompletedSteps(error: unknown): string[] {
  if (typeof error !== "object" || error === null || !("details" in error)) return [];
  const details = (error as { details?: unknown }).details;
  if (typeof details !== "object" || details === null || !("steps" in details)) return [];
  const value = (details as { steps?: unknown }).steps;
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === "string" && item.length > 0)
    : [];
}

function importStepLabel(step: string): string {
  const labels: Record<string, string> = {
    backup_created: "已创建导入前备份",
    session_created: "已新建会话文件",
    index_updated: "已更新会话索引",
    state_updated: "已更新 ChatGPT 任务列表记录",
    pin_updated: "已加入 ChatGPT 置顶列表",
    session_removed: "已删除本次未完成的会话文件",
    session_and_partial_index_rolled_back: "已撤销本次会话文件和索引记录",
    session_index_and_possible_state_record_rolled_back: "已撤销本次会话和任务列表记录",
    session_index_and_state_rolled_back: "已撤销本次会话和任务列表记录",
    verification_failed_and_changes_rolled_back: "检查失败，已撤销本次写入",
    rollback_incomplete: "自动撤销没有全部完成，请按备份位置人工检查",
  };
  return labels[step] ?? step;
}

function safeName(value: string): string {
  return value
    .normalize("NFKC")
    .replace(/[^\p{L}\p{N}._-]+/gu, "-")
    .replace(/-+/g, "-")
    .replace(/^-|-$/g, "")
    .slice(0, 42) || "handoff";
}

function defaultDownloadPath(): string {
  const stamp = new Date().toISOString().replace(/[-:.]/g, "");
  const suffix = globalThis.crypto?.randomUUID?.().slice(0, 8)
    ?? Math.random().toString(36).slice(2, 10);
  return `/tmp/Relay-Incoming-${stamp}-${suffix}.relaypack`;
}

function parentPath(path: string): string {
  const normalized = path.replace(/\/+$/, "");
  const index = normalized.lastIndexOf("/");
  return index > 0 ? normalized.slice(0, index) : normalized;
}

function fileName(path: string): string {
  return path.replace(/\/+$/, "").split("/").filter(Boolean).at(-1) ?? "Relay-Received";
}

function joinPath(parent: string, child: string): string {
  return `${parent.replace(/\/+$/, "")}/${child}`;
}

function agentLabel(agent: AgentKind): string {
  return agent === "claude_code" ? "Claude Code" : "ChatGPT";
}

const OMITTED_DIAGNOSTIC_LABELS: Record<string, string> = {
  CONVERSATION_REDACTED_BY_USER: "部分聊天记录",
  TOOL_EVIDENCE_REDACTED_BY_USER: "工具记录",
  PROJECT_INSTRUCTIONS_REDACTED_BY_USER: "项目说明",
  ENVIRONMENT_REDACTED_BY_USER: "设备信息",
  GIT_EXCLUDED: "代码改动",
};

const HIDDEN_DIAGNOSTIC_CODES = new Set([
  "GIT_REMOTE_MISSING",
  "GIT_UPSTREAM_UNKNOWN",
  "SESSION_STATE_NOT_PROVIDED",
]);

function diagnosticCopy(code: string, message: string): { title: string; message: string } {
  const copies: Record<string, { title: string; message: string }> = {
    UNPAIRED_TOOL_HISTORY_OMITTED: {
      title: "已省略不完整的工具记录",
      message: "有些工具调用或结果缺少对应记录，Relay 没有将其包含在分享内容中。",
    },
  };
  if (code.startsWith("SENSITIVE_CONTENT_INCLUDED_")) {
    return {
      title: "包含疑似敏感内容",
      message: "发送方已经检查并确认保留相关内容，接收前请再次核对。",
    };
  }
  return copies[code] ?? { title: "内容提示", message };
}

export function nativeImportMessage(result: ImportNativeSessionResult): string {
  const shortID = result.session_id.slice(0, 12);
  if (result.target === "codex") {
    if (result.open_status === "ready") {
      if (result.catalog_refresh_status === "sent") {
        return `ChatGPT 任务“${result.title}”已经导入（${shortID}），并已置顶。Relay 已让 ChatGPT 重新读取本机任务列表，请从列表顶部打开。`;
      }
      if (result.catalog_refresh_status === "failed") {
        return `ChatGPT 任务“${result.title}”已经导入（${shortID}），但 Relay 未能通知当前运行的 ChatGPT 重新读取任务列表。`;
      }
      return `ChatGPT 任务“${result.title}”已经导入（${shortID}），并已置顶。请在 ChatGPT 任务列表顶部打开。`;
    }
    if (result.open_status === "failed") {
      return `ChatGPT 任务“${result.title}”已经导入（${shortID}），但 Relay 未能显示 ChatGPT。请打开 ChatGPT，并从任务列表顶部进入。`;
    }
    return `ChatGPT 任务“${result.title}”已经导入（${shortID}），并已置顶。请在 ChatGPT 任务列表顶部打开。`;
  }
  return `Claude Code 会话“${result.title}”已经导入（${shortID}）。`;
}

export function nativeImportOpenNotice(result: ImportNativeSessionResult): string | null {
  if (result.target !== "codex") return null;
  if (result.catalog_refresh_status === "failed") {
    return "任务已经导入，但 Relay 未能通知当前运行的 ChatGPT 重新读取本机任务列表。请点击“显示 ChatGPT 任务列表”；如果仍未显示，请重新启动 ChatGPT 后从列表顶部打开。";
  }
  if (result.open_status === "ready") return null;
  if (result.open_status === "manual") {
    return "任务已经导入并置顶。Relay 未找到可显示的 ChatGPT 应用，请打开 ChatGPT 后从任务列表顶部进入。";
  }
  const messages: Record<string, string> = {
    chatgpt_identity_unverified: "任务已经导入，但本机注册的应用没有通过 ChatGPT 签名检查。请使用官方 ChatGPT 应用从任务列表中打开。",
    chatgpt_signature_check_failed: "任务已经导入，但 Relay 无法完成 ChatGPT 签名检查。请在 ChatGPT 的本机任务列表中打开。",
    chatgpt_signature_untrusted: "任务已经导入，但本机注册的应用没有通过 ChatGPT 签名检查。请使用官方 ChatGPT 应用从任务列表中打开。",
    chatgpt_open_failed: "任务已经导入，但 macOS 未能显示 ChatGPT。请打开 ChatGPT 后从任务列表顶部进入。",
  };
  return messages[result.open_error_code ?? ""]
    ?? "任务已经导入，但 Relay 未能显示 ChatGPT。请打开 ChatGPT 后从任务列表顶部进入。";
}

export type NativeImportAttempt = {
  restored: RestoreRelaypackResult | null;
  result: ImportNativeSessionResult | null;
  error: unknown | null;
};

export async function importReceivedSession(
  agent: AgentKind,
  existingRestore: RestoreRelaypackResult | null,
  restore: () => Promise<RestoreRelaypackResult>,
  importer: typeof importNativeSession,
): Promise<NativeImportAttempt> {
  let restored = existingRestore;
  try {
    restored ??= await restore();
    const result = await importer({
      agent,
      worktree_path: restored.worktree_path,
      handoff_json_path: restored.handoff_json_path,
    });
    return { restored, result, error: null };
  } catch (error) {
    return { restored, result: null, error };
  }
}

export function nativeContinueCommand(result: ImportNativeSessionResult | null): string {
  return result?.continue_command ?? "";
}

export function nativeImportActionState(
  agent: AgentKind,
  canImport: boolean,
  busy: "inspect" | "save" | AgentKind | null,
  importedAgents: AgentKind[],
  failedAgent: AgentKind | null,
): { label: string; disabled: boolean; complete: boolean } {
  const complete = importedAgents.includes(agent);
  const target = agentLabel(agent);
  const label = busy === agent
    ? `正在导入 ${target}`
    : complete
      ? `已导入到 ${target}`
      : failedAgent === agent
        ? `重新导入到 ${target}`
        : receiveTargetCopy[agent].title;
  return {
    label,
    disabled: !canImport || busy !== null || complete,
    complete,
  };
}

function receiveBusyCopy(
  busy: "inspect" | "save" | AgentKind,
  source: ReceiveSource,
): { title: string; description: string; stages: string[] } {
  if (busy === "inspect") {
    return source === "share_link"
      ? {
          title: "正在打开分享",
          description: "Relay 正在下载、验证并读取发送者允许查看的内容。",
          stages: ["下载加密文件", "验证文件内容", "准备聊天预览"],
        }
      : {
          title: "正在读取分享文件",
          description: "Relay 正在验证本地文件并准备可查看的聊天记录。",
          stages: ["读取本地文件", "验证文件内容", "准备聊天预览"],
        };
  }
  if (busy === "save") {
    return {
      title: "正在保存分享内容",
      description: "Relay 正在创建新的接收目录并写入所选文件。",
      stages: ["创建接收目录", "保存代码和附件", "检查保存结果"],
    };
  }
  if (busy === "codex") {
    return {
      title: "正在导入到 ChatGPT",
      description: "Relay 正在保存文件、创建新任务并检查本机任务记录。",
      stages: ["保存分享内容", "创建 ChatGPT 任务", "刷新任务列表"],
    };
  }
  return {
    title: "正在导入到 Claude Code",
    description: "Relay 正在保存文件、创建新会话并检查会话记录。",
    stages: ["保存分享内容", "创建 Claude Code 会话", "检查导入结果"],
  };
}

export default function ReceivePanel({ home, onNotice }: ReceivePanelProps) {
  const [source, setSource] = useState<ReceiveSource>("share_link");
  const [shareUrl, setShareUrl] = useState("");
  const [downloadPath] = useState(defaultDownloadPath);
  const [packagePath, setPackagePath] = useState("");
  const [key, setKey] = useState("");
  const [inspection, setInspection] = useState<InspectRelaypackResult | null>(null);
  const [repositoryPath, setRepositoryPath] = useState("");
  const [targetPath, setTargetPath] = useState("");
  const [branchName, setBranchName] = useState("");
  const [restoreResult, setRestoreResult] = useState<RestoreRelaypackResult | null>(null);
  const [nativeImportResult, setNativeImportResult] = useState<ImportNativeSessionResult | null>(null);
  const [importedAgents, setImportedAgents] = useState<AgentKind[]>([]);
  const [failedAgent, setFailedAgent] = useState<AgentKind | null>(null);
  const [busy, setBusy] = useState<"inspect" | "save" | AgentKind | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [errorBackup, setErrorBackup] = useState<string | null>(null);
  const [errorSteps, setErrorSteps] = useState<string[]>([]);
  const [copiedCommand, setCopiedCommand] = useState(false);
  const [openingChatgpt, setOpeningChatgpt] = useState(false);
  const [chatgptOpenNotice, setChatgptOpenNotice] = useState<string | null>(null);
  const resultRef = useRef<HTMLDivElement | null>(null);

  const preview = inspection?.preview;
  const visibleDiagnostics = useMemo(
    () => preview?.diagnostics.filter((item) =>
      item.severity !== "info" &&
      !OMITTED_DIAGNOSTIC_LABELS[item.code] &&
      !HIDDEN_DIAGNOSTIC_CODES.has(item.code)
    ).slice(0, 6) ?? [],
    [preview],
  );
  const omittedContents = useMemo(() => {
    const labels = preview?.diagnostics.flatMap((item) => {
      const label = OMITTED_DIAGNOSTIC_LABELS[item.code];
      return label ? [label] : [];
    }) ?? [];
    return [...new Set(labels)];
  }, [preview]);
  const warningCount = visibleDiagnostics.length;
  const openNotice = nativeImportResult ? nativeImportOpenNotice(nativeImportResult) : null;

  useEffect(() => {
    if (!restoreResult && !nativeImportResult) return;
    const frame = window.requestAnimationFrame(() => {
      resultRef.current?.scrollIntoView({ behavior: "smooth", block: "nearest" });
    });
    return () => window.cancelAnimationFrame(frame);
  }, [restoreResult, nativeImportResult]);

  const inspect = async () => {
    setBusy("inspect");
    setError(null);
    setErrorBackup(null);
    setErrorSteps([]);
    setInspection(null);
    setRestoreResult(null);
    setNativeImportResult(null);
    setImportedAgents([]);
    setFailedAgent(null);
    try {
      let result: InspectRelaypackResult;
      if (source === "share_link") {
        const base = shareServiceOriginFromLink(shareUrl);
        const downloaded = await downloadShare({
          share_url: shareUrl.trim(),
          service_base_url: base,
          output_path: downloadPath.trim(),
        });
        setPackagePath(downloaded.package_path);
        setKey(downloaded.key);
        result = downloaded;
      } else {
        result = await inspectRelaypack(packagePath.trim(), key.trim());
      }
      setInspection(result);
      const suffix = result.preview.package_id.replace(/^pkg\./, "").slice(0, 8);
      const folderName = `Relay-${safeName(result.preview.project_name)}-${suffix}`;
      setTargetPath(home ? `${home}/Downloads/${folderName}` : "");
      setRepositoryPath("");
      if (result.preview.git_included) {
        setBranchName(`relay/${safeName(result.preview.project_name)}-${suffix}`);
      } else {
        setBranchName("");
      }
      onNotice(
        result.preview.git_included
          ? "分享已打开。这份分享包含 Git 改动，请选择同一个项目的本机仓库和新工作目录的保存位置。"
          : "分享已打开。请确认接收位置并选择要继续使用的应用。",
      );
    } catch (caught) {
      setError(receiveErrorMessage(caught));
      setErrorBackup(errorBackupPath(caught) ?? null);
      setErrorSteps(errorCompletedSteps(caught));
    } finally {
      setBusy(null);
    }
  };

  const restoreContents = async (): Promise<RestoreRelaypackResult> => {
    if (restoreResult) return restoreResult;
    if (!inspection) throw new Error("请先验证分享内容");
    const result = await restoreRelaypack({
      package_path: inspection.package_path,
      key,
      repository_path: preview?.git_included ? repositoryPath.trim() : undefined,
      target_path: targetPath.trim(),
      branch_name: preview?.git_included ? branchName.trim() : undefined,
    });
    setRestoreResult(result);
    return result;
  };

  const saveOnly = async () => {
    setBusy("save");
    setError(null);
    setErrorBackup(null);
    setErrorSteps([]);
    setNativeImportResult(null);
    try {
      const result = await restoreContents();
      onNotice(`分享内容已保存到 ${result.worktree_path}`);
    } catch (caught) {
      setError(receiveErrorMessage(caught));
      setErrorBackup(errorBackupPath(caught) ?? null);
      setErrorSteps(errorCompletedSteps(caught));
    } finally {
      setBusy(null);
    }
  };

  const importAgent = async (agent: AgentKind) => {
    if (!inspection) return;
    setBusy(agent);
    setError(null);
    setErrorBackup(null);
    setErrorSteps([]);
    setNativeImportResult(null);
    setFailedAgent(null);
    setCopiedCommand(false);
    setChatgptOpenNotice(null);
    const attempt = await importReceivedSession(
      agent,
      restoreResult,
      restoreContents,
      importNativeSession,
    );
    if (attempt.restored && attempt.restored !== restoreResult) {
      setRestoreResult(attempt.restored);
    }
    if (attempt.result) {
      setNativeImportResult(attempt.result);
      setImportedAgents((current) => current.includes(agent) ? current : [...current, agent]);
      onNotice(nativeImportMessage(attempt.result));
    } else {
      setFailedAgent(agent);
      const message = receiveErrorMessage(attempt.error);
      setError(attempt.restored
        ? `分享内容已经保存到 ${attempt.restored.worktree_path}，但会话导入失败。${message}`
        : message);
      setErrorBackup(errorBackupPath(attempt.error) ?? null);
      setErrorSteps(errorCompletedSteps(attempt.error));
    }
    setBusy(null);
  };

  const copyContinueCommand = async () => {
    const command = nativeContinueCommand(nativeImportResult);
    if (!command) return;
    try {
      await copyText(command);
      setCopiedCommand(true);
      onNotice("继续命令已复制");
    } catch (caught) {
      setError(receiveErrorMessage(caught));
      setErrorSteps(errorCompletedSteps(caught));
    }
  };

  const showImportedChatgptTask = async () => {
    if (nativeImportResult?.target !== "codex") return;
    setOpeningChatgpt(true);
    setChatgptOpenNotice(null);
    try {
      await showChatgptTasks();
      setChatgptOpenNotice("已显示 ChatGPT。导入的任务已置顶，请从任务列表顶部打开。");
    } catch (caught) {
      setChatgptOpenNotice(receiveErrorMessage(caught));
    } finally {
      setOpeningChatgpt(false);
    }
  };

  const selectPackageFile = async () => {
    setError(null);
    try {
      const selected = await chooseRelaypackFile(
        packagePath || (home ? `${home}/Downloads` : undefined),
      );
      if (selected) {
        setInspection(null);
        setRestoreResult(null);
        setNativeImportResult(null);
        setImportedAgents([]);
        setFailedAgent(null);
        setPackagePath(selected);
      }
    } catch (caught) {
      setError(receiveErrorMessage(caught));
    }
  };

  const selectRepository = async () => {
    setError(null);
    try {
      const selected = await chooseDirectory(
        "选择本机 Git 项目",
        repositoryPath || home,
      );
      if (selected) {
        const repository = await inspectRepository(selected);
        resetDestination();
        setRepositoryPath(repository.root);
      }
    } catch (caught) {
      setError(receiveErrorMessage(caught));
    }
  };

  const selectTargetParent = async () => {
    setError(null);
    try {
      const selected = await chooseDirectory(
        preview?.git_included ? "选择新项目的保存位置" : "选择文件保存位置",
        targetPath ? parentPath(targetPath) : home,
      );
      if (selected) {
        resetDestination();
        setTargetPath(joinPath(selected, fileName(targetPath)));
      }
    } catch (caught) {
      setError(receiveErrorMessage(caught));
    }
  };

  const canInspect =
    source === "share_link"
      ? Boolean(shareUrl.trim() && downloadPath.trim())
      : Boolean(packagePath.trim() && key.trim());
  const canRestore = Boolean(
    inspection && targetPath.trim() && (
      !preview?.git_included || (repositoryPath.trim() && branchName.trim())
    ),
  );
  const canImport = Boolean(canRestore && preview?.importable_session);

  const resetDestination = () => {
    setRestoreResult(null);
    setNativeImportResult(null);
    setImportedAgents([]);
    setFailedAgent(null);
    setError(null);
    setErrorBackup(null);
    setErrorSteps([]);
    setCopiedCommand(false);
    setChatgptOpenNotice(null);
  };

  const changeShare = () => {
    setInspection(null);
    setRepositoryPath("");
    setTargetPath("");
    setBranchName("");
    setShareUrl("");
    setPackagePath("");
    setKey("");
    resetDestination();
  };

  const selectSource = (next: ReceiveSource) => {
    if (next === source) return;
    setSource(next);
    changeShare();
  };

  const retryingImport = Boolean(error && restoreResult && failedAgent && !nativeImportResult);
  const chatgptAction = nativeImportActionState(
    "codex", canImport, busy, importedAgents, failedAgent,
  );
  const claudeAction = nativeImportActionState(
    "claude_code", canImport, busy, importedAgents, failedAgent,
  );
  const canShowChatgptTasks = nativeImportResult?.target === "codex";
  const activeBusyCopy = busy ? receiveBusyCopy(busy, source) : null;

  return (
    <main className="receive-workspace receive-page">
      <div className="receive-page-inner">
        <header className="receive-page-header">
          <div>
            <h1>接收分享</h1>
            <p>
              {preview
                ? "确认接收位置后，可直接导入 ChatGPT 或 Claude Code。"
                : "粘贴 Relay 分享链接或选择本地分享文件。打开后可查看内容并继续工作。"}
            </p>
          </div>
        </header>

        {!preview ? (
          <section className="receive-entry-panel">
            <div className="receive-source-tabs" role="tablist" aria-label="分享来源">
              <button
                type="button"
                role="tab"
                aria-selected={source === "share_link"}
                className={source === "share_link" ? "is-active" : ""}
                onClick={() => selectSource("share_link")}
              >
                分享链接
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={source === "local_file"}
                className={source === "local_file" ? "is-active" : ""}
                onClick={() => selectSource("local_file")}
              >
                本地文件
              </button>
            </div>

            <div className="receive-form receive-entry-form">
              {source === "share_link" ? (
                <input
                  aria-label="Relay 分享链接"
                  value={shareUrl}
                  onChange={(event) => setShareUrl(event.target.value)}
                  placeholder="粘贴完整的 Relay 分享链接"
                  spellCheck={false}
                />
              ) : (
                <div className="receive-local-fields">
                  <div className="receive-file-selector">
                    <span>分享文件</span>
                    <button type="button" onClick={() => void selectPackageFile()}>
                      <strong>{packagePath ? fileName(packagePath) : "选择分享文件"}</strong>
                      <small>{packagePath || "从访达中选择接收到的文件"}</small>
                    </button>
                  </div>
                  <label>
                    <span>文件密码</span>
                    <input
                      value={key}
                      onChange={(event) => setKey(event.target.value)}
                      placeholder="输入发送者提供的文件密码"
                      spellCheck={false}
                    />
                  </label>
                </div>
              )}
            </div>

            <button
              className="primary-action receive-inspect-button"
              type="button"
              disabled={!canInspect || busy !== null}
              onClick={() => void inspect()}
            >
              <span>{busy === "inspect" ? "正在打开" : "打开分享"}</span>
              <b>→</b>
            </button>
          </section>
        ) : null}

        {busy === "inspect" && activeBusyCopy ? (
          <LoadingState
            className="receive-operation-loading"
            title={activeBusyCopy.title}
            description={activeBusyCopy.description}
            stages={activeBusyCopy.stages}
          />
        ) : null}

        {error ? (
          <div className="receive-error" role="alert">
            <strong>{restoreResult ? "会话导入失败" : inspection ? "无法保存分享内容" : "无法打开分享"}</strong>
            <p>{error}</p>
            {(errorBackup || errorSteps.length > 0) ? (
              <details>
                <summary>查看恢复信息</summary>
                {errorBackup ? <p>导入前备份：{errorBackup}</p> : null}
                {errorSteps.length > 0 ? (
                  <ul>{errorSteps.map((step) => <li key={step}>{importStepLabel(step)}</li>)}</ul>
                ) : null}
              </details>
            ) : null}
          </div>
        ) : null}

        {preview ? (
          <section className="receive-review-panel">
            <div className="receive-review-content">
              <article className="receive-summary-card">
              <div className="preview-heading">
                <div>
                  <span className="receive-verified-label"><i aria-hidden="true">✓</i> 内容已验证</span>
                  <h2 title={preview.title}>{preview.title}</h2>
                  <p>{preview.project_name} · 来自 {agentLabel(preview.source_agent)} · {source === "share_link" ? "分享链接" : fileName(packagePath)}</p>
                </div>
                <button className="receive-change-share" type="button" onClick={changeShare}>
                  {nativeImportResult ? "接收其他分享" : "更换分享"}
                </button>
              </div>

              <div className="receive-metrics">
                <div><span>聊天记录</span><strong>{preview.conversation_records}</strong></div>
                <div><span>附件</span><strong>{preview.asset_count}</strong></div>
                <div><span>Git 改动</span><strong>{preview.git_included ? "已包含" : "未包含"}</strong></div>
                {warningCount > 0 ? <div><span>需要注意</span><strong>{warningCount}</strong></div> : null}
              </div>

              {omittedContents.length > 0 ? (
                <p className="receive-omissions">发送方未包含：{omittedContents.join("、")}。</p>
              ) : null}

              {visibleDiagnostics.length > 0 ? (
                <div className="receive-diagnostics" aria-label="分享内容提示">
                  {visibleDiagnostics.map((item) => {
                    const copy = diagnosticCopy(item.code, item.message);
                    return (
                      <p key={`${item.code}-${item.scope}`} className={`severity-${item.severity}`}>
                        <b>{copy.title}</b>
                        <span>{copy.message}</span>
                      </p>
                    );
                  })}
                </div>
              ) : null}

              </article>

              <section className="receive-conversation-preview" aria-label="分享的聊天记录">
                <header>
                  <div>
                    <h3>聊天记录</h3>
                    <p>这里显示发送者允许分享的消息。历史工具记录只用于阅读，不会执行。</p>
                  </div>
                  <span>只读</span>
                </header>
                <ConversationViewer
                  preview={inspection.content_preview}
                  loading={false}
                  error={null}
                  onRetry={() => void inspect()}
                  onNotice={onNotice}
                />
              </section>
            </div>

            {!nativeImportResult ? <section className="restore-form receive-destination-card">
              <div className="restore-heading">
                <div>
                  <h3>{retryingImport ? "重新导入会话" : "继续使用"}</h3>
                  <p>
                    {retryingImport
                      ? "文件已经保存，只需重新创建会话。"
                      : "Relay 会先保存文件，再创建一条新的任务或会话。"}
                  </p>
                </div>
              </div>

              {preview.git_included ? (
                <>
                  <div className="receive-git-note">
                    <strong>接收方需要有同一个 Git 项目</strong>
                    <p>分享包保存的是发送者相对原项目产生的改动，不是完整项目副本。请选择同一个远程仓库克隆出的本机项目；如果尚未克隆，请先运行 <code>git clone</code>。</p>
                    <p>Relay 会检查仓库来源，再创建新的工作目录并恢复提交、暂存、未暂存和所选新文件。所选仓库本身不会被修改。</p>
                  </div>
                  <div className="receive-path-selector">
                    <span>同一项目的本机 Git 仓库</span>
                    <button type="button" onClick={() => void selectRepository()}>
                      <strong>{repositoryPath ? fileName(repositoryPath) : "选择 Git 仓库根目录"}</strong>
                      <small>{repositoryPath || "例如：从同一个 GitHub 仓库克隆出的项目文件夹"}</small>
                    </button>
                    <small>不要选择 Downloads 等普通文件夹，也不要选择仓库中的子文件夹。</small>
                  </div>
                </>
              ) : null}
              <div className="receive-path-selector">
                <span>接收位置</span>
                <button type="button" onClick={() => void selectTargetParent()}>
                  <strong>{targetPath ? fileName(targetPath) : "选择接收位置"}</strong>
                  <small>{targetPath || "从访达中选择一个文件夹"}</small>
                </button>
                <small>Relay 会在所选位置创建新文件夹，不会覆盖已有文件。</small>
              </div>

              {!preview.importable_session ? (
                <div className="receive-file-only-note">
                  <strong>无法创建会话</strong>
                  <p>发送者未包含可导入的聊天记录或项目说明。代码和附件仍可保存。</p>
                </div>
              ) : null}

              <div className="receive-target-actions" aria-label="选择导入目标">
                <button
                  className={`receive-target-button chatgpt${chatgptAction.complete ? " is-complete" : ""}`}
                  type="button"
                  disabled={chatgptAction.disabled}
                  onClick={() => void importAgent("codex")}
                >
                  <b>GPT</b>
                  <span>
                    <strong>{chatgptAction.label}</strong>
                    <small>{receiveTargetCopy.codex.description}</small>
                  </span>
                  <i>→</i>
                </button>
                <button
                  className={`receive-target-button claude${claudeAction.complete ? " is-complete" : ""}`}
                  type="button"
                  disabled={claudeAction.disabled}
                  onClick={() => void importAgent("claude_code")}
                >
                  <b>C</b>
                  <span>
                    <strong>{claudeAction.label}</strong>
                    <small>{receiveTargetCopy.claude_code.description}</small>
                  </span>
                  <i>→</i>
                </button>
              </div>

              {preview.importable_session ? (
                <p className="receive-import-note">每次导入都会创建新的任务或会话，不会修改已有记录。</p>
              ) : null}

              <div className="receive-save-row">
                <button
                  className="receive-save-link"
                  type="button"
                  disabled={!canRestore || busy !== null || restoreResult !== null}
                  onClick={() => void saveOnly()}
                >
                  {busy === "save" ? "正在保存" : restoreResult ? "文件已保存" : receiveSaveCopy.title}
                </button>
                <small>{receiveSaveCopy.description}</small>
              </div>
            </section> : null}

            {busy && busy !== "inspect" && activeBusyCopy ? (
              <LoadingState
                compact
                className="receive-operation-loading is-in-review"
                title={activeBusyCopy.title}
                description={activeBusyCopy.description}
                stages={activeBusyCopy.stages}
              />
            ) : null}

            {restoreResult || nativeImportResult ? (
              <div className="restore-success" ref={resultRef}>
                <span className="restore-success-mark" aria-hidden="true">✓</span>
                <div>
                  <span className="eyebrow">
                    {nativeImportResult ? "导入完成" : retryingImport ? "文件已保存" : "保存完成"}
                  </span>
                  <h3>
                    {nativeImportResult
                      ? `已导入到 ${agentLabel(nativeImportResult.target)}`
                      : retryingImport ? "会话尚未导入" : "文件已保存"}
                  </h3>
                  {nativeImportResult ? <p>{nativeImportResult.title}</p> : null}
                  {retryingImport ? <p>代码和附件已经保存。重新导入时不会再次创建接收目录。</p> : null}
                  {restoreResult ? (
                    <p className="restore-success-path"><span>保存位置</span><code>{restoreResult.worktree_path}</code></p>
                  ) : null}

                  {nativeImportResult?.target === "claude_code" && nativeContinueCommand(nativeImportResult) ? (
                    <>
                      <p className="receive-session-id">会话 ID：<code>{nativeImportResult.session_id}</code></p>
                      <div className="continue-command">
                        <code>{nativeContinueCommand(nativeImportResult)}</code>
                        <button type="button" onClick={() => void copyContinueCommand()}>
                          {copiedCommand ? "已复制" : "复制继续命令"}
                        </button>
                      </div>
                    </>
                  ) : null}
                  {nativeImportResult?.target === "codex" ? (
                    <div className="receive-chatgpt-open">
                      <p>
                        {nativeImportResult.catalog_refresh_status === "sent"
                          ? "任务已导入并置顶。Relay 已通知 ChatGPT 重新读取本机任务列表，请从列表顶部打开。"
                          : nativeImportResult.catalog_refresh_status === "failed"
                            ? "任务已导入，但 Relay 未能通知当前运行的 ChatGPT 重新读取任务列表。"
                            : "任务已导入并置顶，请从 ChatGPT 任务列表顶部打开。"}
                      </p>
                      <p className="receive-session-id">任务 ID：<code>{nativeImportResult.session_id}</code></p>
                      {canShowChatgptTasks ? (
                        <button
                          type="button"
                          disabled={openingChatgpt}
                          onClick={() => void showImportedChatgptTask()}
                        >
                          {openingChatgpt ? "正在显示" : "显示 ChatGPT 任务列表"}
                        </button>
                      ) : null}
                      {chatgptOpenNotice ? <small role="status">{chatgptOpenNotice}</small> : null}
                    </div>
                  ) : null}
                  {openNotice ? (
                    <p className="receive-open-warning">{openNotice}</p>
                  ) : null}

                </div>
              </div>
            ) : null}
          </section>
        ) : null}
      </div>
    </main>
  );
}
