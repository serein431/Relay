import { useEffect, useMemo, useRef, useState } from "react";
import {
  downloadShare,
  importNativeSession,
  inspectRelaypack,
  openImportedChatgptTask,
  restoreRelaypack,
} from "./lib/tauri";
import { copyText } from "./lib/clipboard";
import { chooseDirectory, chooseRelaypackFile } from "./lib/dialog";
import { userErrorMessage } from "./lib/errors";
import { shareServiceOriginFromLink } from "./lib/share-service";
import type {
  AgentKind,
  ImportNativeSessionResult,
  InspectRelaypackResult,
  RestoreRelaypackResult,
} from "./types";

type ReceiveSource = "share_link" | "local_file";

export const receiveTargetCopy: Record<AgentKind, { title: string; description: string }> = {
  codex: { title: "导入到 ChatGPT", description: "保存文件并新建一条任务，随后打开 ChatGPT。" },
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
    handoff_not_found: "没有找到会话记录，请重新读取或重新保存分享内容。",
    handoff_invalid: "会话记录文件无法读取或格式不受支持。",
    invalid_handoff_path: "会话记录文件不在本次接收目录中，请重新保存分享内容。",
    invalid_session_id: "任务编号无效，无法打开 ChatGPT。",
    home_unavailable: "无法确定目标应用的本机数据目录。",
    chatgpt_state_not_found: "没有找到 ChatGPT 的本机任务数据。请先打开一次 ChatGPT，再返回 Relay 重新导入。",
    chatgpt_handler_not_found: "没有找到可打开本机任务的 ChatGPT 应用。请安装或打开官方 ChatGPT 应用。",
    chatgpt_identity_unverified: "本机注册的应用未通过 ChatGPT 签名检查。请使用官方 ChatGPT 应用。",
    chatgpt_signature_check_failed: "Relay 无法完成 ChatGPT 签名检查。请确认官方 ChatGPT 应用安装完整。",
    chatgpt_signature_untrusted: "本机注册的应用未通过 ChatGPT 签名检查。请使用官方 ChatGPT 应用。",
    chatgpt_open_failed: "macOS 未能把任务打开请求交给 ChatGPT。请稍后重试。",
    unsupported_platform: "当前系统不支持自动打开 ChatGPT 任务。",
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
    if (result.open_status === "requested") {
      return `ChatGPT 任务“${result.title}”已经导入（${shortID}），并已向 ChatGPT 发送打开请求。`;
    }
    if (result.open_status === "failed") {
      return `ChatGPT 任务“${result.title}”已经导入（${shortID}），但 Relay 未能自动打开。请在 ChatGPT 的本机任务列表中打开。`;
    }
    return `ChatGPT 任务“${result.title}”已经导入（${shortID}）。请在 ChatGPT 的本机任务列表中打开。`;
  }
  return `Claude Code 会话“${result.title}”已经导入（${shortID}）。`;
}

export function nativeImportOpenNotice(result: ImportNativeSessionResult): string | null {
  if (result.target !== "codex" || result.open_status === "requested") return null;
  if (result.open_status === "manual") {
    return "任务已经导入。Relay 未找到可自动打开任务的 ChatGPT 应用，请在 ChatGPT 的本机任务列表中打开。";
  }
  const messages: Record<string, string> = {
    chatgpt_identity_unverified: "任务已经导入，但本机注册的应用没有通过 ChatGPT 签名检查。请使用官方 ChatGPT 应用从任务列表中打开。",
    chatgpt_signature_check_failed: "任务已经导入，但 Relay 无法完成 ChatGPT 签名检查。请在 ChatGPT 的本机任务列表中打开。",
    chatgpt_signature_untrusted: "任务已经导入，但本机注册的应用没有通过 ChatGPT 签名检查。请使用官方 ChatGPT 应用从任务列表中打开。",
    chatgpt_open_failed: "任务已经导入，但 macOS 未完成自动打开。请在 ChatGPT 的本机任务列表中打开。",
  };
  return messages[result.open_error_code ?? ""]
    ?? "任务已经导入，但 Relay 未完成自动打开。请在 ChatGPT 的本机任务列表中打开。";
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
      if (result.preview.git_included) {
        setBranchName(`relay/${safeName(result.preview.project_name)}-${suffix}`);
      } else {
        setRepositoryPath("");
        setBranchName("");
      }
      onNotice(
        result.preview.git_included
          ? "分享已打开。请选择所属项目、接收位置和要继续使用的应用。"
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
      const prefix = attempt.restored
        ? `分享内容已经保存到 ${attempt.restored.worktree_path}，但会话导入失败。`
        : "接收分享失败。";
      setError(`${prefix} ${receiveErrorMessage(attempt.error)}`);
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

  const reopenChatgpt = async () => {
    if (nativeImportResult?.target !== "codex") return;
    setOpeningChatgpt(true);
    setChatgptOpenNotice(null);
    try {
      await openImportedChatgptTask(nativeImportResult.session_id);
      setChatgptOpenNotice("已向 ChatGPT 重新发送打开请求。");
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
        resetDestination();
        setRepositoryPath(selected);
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
  const canRetryChatgptOpen = nativeImportResult?.target === "codex" && (
    nativeImportResult.open_status === "requested" ||
    nativeImportResult.open_error_code === "chatgpt_open_failed"
  );

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
                <div><span>代码</span><strong>{preview.git_included ? "已包含" : "未包含"}</strong></div>
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
                <div className="receive-path-selector">
                  <span>所属项目</span>
                  <button type="button" onClick={() => void selectRepository()}>
                    <strong>{repositoryPath ? fileName(repositoryPath) : "选择本机项目"}</strong>
                    <small>{repositoryPath || "选择发送者修改代码时使用的项目"}</small>
                  </button>
                  <small>Relay 不会修改所选项目，接收到的文件会保存在新的文件夹中。</small>
                </div>
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
                        {nativeImportResult.open_status === "requested"
                          ? "已向 ChatGPT 发送打开请求。"
                          : "任务已导入，可从 ChatGPT 的本机任务列表打开。"}
                      </p>
                      {canRetryChatgptOpen ? (
                        <button
                          type="button"
                          disabled={openingChatgpt}
                          onClick={() => void reopenChatgpt()}
                        >
                          {openingChatgpt ? "正在打开" : "再次打开 ChatGPT"}
                        </button>
                      ) : null}
                      {chatgptOpenNotice ? <small role="status">{chatgptOpenNotice}</small> : null}
                    </div>
                  ) : null}
                  {openNotice ? (
                    <p className="receive-open-warning">{openNotice}</p>
                  ) : null}

                  {nativeImportResult ? (
                    <details className="receive-import-details">
                      <summary>查看导入记录</summary>
                      <dl>
                        <div><dt>会话编号</dt><dd>{nativeImportResult.session_id}</dd></div>
                        <div><dt>会话文件</dt><dd>{nativeImportResult.verification.session_file ? "检查通过" : "检查失败"}</dd></div>
                        <div><dt>会话列表记录</dt><dd>{nativeImportResult.verification.index ? "检查通过" : "检查失败"}</dd></div>
                        {nativeImportResult.target === "codex" ? (
                          <>
                            <div><dt>ChatGPT 任务列表记录</dt><dd>{nativeImportResult.verification.state ? "检查通过" : "检查失败"}</dd></div>
                            <div><dt>置顶状态</dt><dd>{nativeImportResult.verification.pinned ? "检查通过" : "检查失败"}</dd></div>
                          </>
                        ) : null}
                        {nativeImportResult.backup_dir ? (
                          <div><dt>导入前备份</dt><dd>{nativeImportResult.backup_dir}</dd></div>
                        ) : null}
                        <div><dt>本次新增文件</dt><dd>{nativeImportResult.created_files.length}</dd></div>
                      </dl>
                    </details>
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
