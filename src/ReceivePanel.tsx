import { useMemo, useState } from "react";
import {
  downloadShare,
  inspectRelaypack,
  launchAgent,
  restoreRelaypack,
} from "./lib/tauri";
import { shareServiceOriginFromLink } from "./lib/share-service";
import type {
  AgentKind,
  InspectRelaypackResult,
  LaunchAgentResult,
  RestoreRelaypackResult,
} from "./types";

type ReceiveSource = "share_link" | "local_file";

type ReceivePanelProps = {
  home?: string;
  onNotice: (message: string) => void;
};

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  if (typeof error === "object" && error !== null && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return String(error);
}

function safeName(value: string): string {
  return value
    .normalize("NFKC")
    .replace(/[^\p{L}\p{N}._-]+/gu, "-")
    .replace(/-+/g, "-")
    .replace(/^-|-$/g, "")
    .slice(0, 42) || "handoff";
}

function defaultDownloadPath(home?: string): string {
  const stamp = new Date().toISOString().replace(/[-:]/g, "").slice(0, 13);
  return `${home ?? ""}/Downloads/Relay-Incoming-${stamp}.relaypack`;
}

function shortCommit(value?: string): string {
  return value ? value.slice(0, 10) : "—";
}

function agentLabel(agent: AgentKind): string {
  return agent === "claude_code" ? "Claude Code" : "ChatGPT";
}

function diagnosticCopy(code: string, message: string): { title: string; message: string } {
  const copies: Record<string, { title: string; message: string }> = {
    CONVERSATION_REDACTED_BY_USER: {
      title: "未包含完整会话",
      message: "发送方取消了部分会话内容。",
    },
    TOOL_EVIDENCE_REDACTED_BY_USER: {
      title: "未包含工具调用记录",
      message: "发送方取消了工具调用及其结果记录。",
    },
    PROJECT_INSTRUCTIONS_REDACTED_BY_USER: {
      title: "未包含项目指令",
      message: "发送方取消了 AGENTS.md、CLAUDE.md 等项目说明。",
    },
    ENVIRONMENT_REDACTED_BY_USER: {
      title: "未包含运行环境",
      message: "发送方取消了系统、架构和本机工具信息。",
    },
    GIT_EXCLUDED: {
      title: "未包含代码改动",
      message: "发送方未选择 Git 提交或文件改动。",
    },
    GIT_REMOTE_MISSING: {
      title: "未记录远程仓库",
      message: "分享包没有可用于核对的远程仓库地址。",
    },
    GIT_UPSTREAM_UNKNOWN: {
      title: "未找到上游分支",
      message: "分享包没有可用于核对的上游分支。",
    },
    SESSION_STATE_NOT_PROVIDED: {
      title: "任务状态不完整",
      message: "发送方没有填写结构化任务状态，请以会话内容为准。",
    },
    UNPAIRED_TOOL_HISTORY_OMITTED: {
      title: "已省略不完整的工具记录",
      message: "有些工具调用或结果缺少对应记录，Relay 没有将其写入分享包。",
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

type ClaudeSessionStatus = "running" | "waiting" | "completed" | "failed" | "stopped" | "unknown";

function claudeSessionStatus(result: LaunchAgentResult): ClaudeSessionStatus {
  const state = result.session_state?.trim().toLowerCase().replace(/[\s-]+/g, "_") ?? "";
  const waiting = result.waiting_reason?.trim();
  if (waiting || ["waiting", "blocked", "needs_input", "needs_you", "permission_required"].includes(state)) {
    return "waiting";
  }
  if (["completed", "complete", "done", "success", "succeeded"].includes(state)) return "completed";
  if (["failed", "failure", "error", "crashed"].includes(state)) return "failed";
  if (["stopped", "cancelled", "canceled", "killed"].includes(state)) return "stopped";
  if (["running", "working", "active", "starting", "resuming"].includes(state)) return "running";
  return "unknown";
}

function claudeLaunchMessage(result: LaunchAgentResult): string {
  const session = result.session_id ? `（会话 ${result.session_id.slice(0, 12)}）` : "";
  const waitingReason = result.waiting_reason?.trim();
  switch (claudeSessionStatus(result)) {
    case "running":
      return `已确认 Claude Code 后台会话正在运行${session}。`;
    case "waiting":
      return `已确认 Claude Code 后台会话正在等待处理${session}：${waitingReason || "Claude Code 没有提供具体等待原因"}`;
    case "completed":
      return `已确认 Claude Code 后台会话已经完成${session}。`;
    case "failed":
      return `已确认 Claude Code 后台会话已经失败${session}，请在 Claude Code 中查看原因。`;
    case "stopped":
      return `已确认 Claude Code 后台会话已经停止${session}。`;
    default:
      return `已确认 Claude Code 创建了新的后台会话${session}；Claude Code 返回了暂不认识的状态“${result.session_state ?? "未提供"}”。`;
  }
}

function launchMessage(result: LaunchAgentResult): string {
  if (result.verification_status === "OPEN_REQUESTED" && result.launch_mode === "deep_link") {
    return "macOS 已把打开请求提交给验证过的 ChatGPT 应用。Relay 无法确认新任务是否已创建；如果窗口已经打开，请检查交接说明后手动发送。";
  }
  if (result.verification_status === "VERIFIED" && result.launch_mode === "background") {
    return claudeLaunchMessage(result);
  }
  return "Relay 未能确认 Claude Code 或 ChatGPT 会话已经创建，本次启动不能视为成功。";
}

export default function ReceivePanel({ home, onNotice }: ReceivePanelProps) {
  const [source, setSource] = useState<ReceiveSource>("share_link");
  const [shareUrl, setShareUrl] = useState("");
  const [downloadPath, setDownloadPath] = useState(() => defaultDownloadPath(home));
  const [packagePath, setPackagePath] = useState("");
  const [key, setKey] = useState("");
  const [inspection, setInspection] = useState<InspectRelaypackResult | null>(null);
  const [repositoryPath, setRepositoryPath] = useState("");
  const [targetPath, setTargetPath] = useState("");
  const [branchName, setBranchName] = useState("");
  const [restoreResult, setRestoreResult] = useState<RestoreRelaypackResult | null>(null);
  const [launchResult, setLaunchResult] = useState<LaunchAgentResult | null>(null);
  const [busy, setBusy] = useState<"inspect" | "restore" | "launch" | null>(null);
  const [error, setError] = useState<string | null>(null);

  const preview = inspection?.preview;
  const warningCount = useMemo(
    () => preview?.diagnostics.filter((item) => item.severity !== "info").length ?? 0,
    [preview],
  );

  const inspect = async () => {
    setBusy("inspect");
    setError(null);
    setInspection(null);
    setRestoreResult(null);
    setLaunchResult(null);
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
          ? "分享包已验证。创建新目录前不会写入代码，也不会执行工具调用记录。"
          : "分享包已验证。它不含 Git 内容，可保存为普通交接文件夹。",
      );
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setBusy(null);
    }
  };

  const restore = async () => {
    if (!inspection) return;
    setBusy("restore");
    setError(null);
    setLaunchResult(null);
    try {
      const result = await restoreRelaypack({
        package_path: inspection.package_path,
        key,
        repository_path: preview?.git_included ? repositoryPath.trim() : undefined,
        target_path: targetPath.trim(),
        branch_name: preview?.git_included ? branchName.trim() : undefined,
      });
      setRestoreResult(result);
      onNotice(
        preview?.git_included
          ? "新的 Git 工作树已恢复，原仓库及 Claude Code、ChatGPT 历史均未修改。"
          : "交接文件夹已经创建，Claude Code 和 ChatGPT 的原会话历史均未修改。",
      );
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setBusy(null);
    }
  };

  const openAgent = async (agent: AgentKind) => {
    if (!restoreResult) return;
    setBusy("launch");
    setError(null);
    setLaunchResult(null);
    try {
      const result = await launchAgent({
        agent,
        worktree_path: restoreResult.worktree_path,
        handoff_markdown_path: restoreResult.handoff_markdown_path,
      });
      setLaunchResult(result);
      onNotice(launchMessage(result));
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setBusy(null);
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

  return (
    <>
      <aside className="receive-sidebar">
        <div className="pane-heading">
          <div>
            <span className="eyebrow">导入分享包</span>
            <h2>接收</h2>
          </div>
          <span className="count-chip">01</span>
        </div>

        <div className="receive-source-list">
          <button
            className={source === "share_link" ? "is-active" : ""}
            type="button"
            onClick={() => {
              setSource("share_link");
              setInspection(null);
              setError(null);
            }}
          >
            <b>01</b>
            <span><strong>加密分享链接</strong><small>恢复代码或创建本机任务</small></span>
          </button>
          <button
            className={source === "local_file" ? "is-active" : ""}
            type="button"
            onClick={() => {
              setSource("local_file");
              setInspection(null);
              setError(null);
            }}
          >
            <b>02</b>
            <span><strong>本地 .relaypack</strong><small>适合 AirDrop、U 盘和网盘</small></span>
          </button>
        </div>

        <div className="receive-safety-note">
          <span>导入说明</span>
          <p>Relay 会先验证并展示内容。不含代码改动的分享包只需保存到一个新文件夹；工具调用记录不会自动执行。</p>
        </div>
      </aside>

      <main className="receive-workspace">
        <section className="receive-input-panel">
          <div className="workspace-heading">
            <div>
              <span className="eyebrow">接收分享</span>
              <h1>验证分享包</h1>
            </div>
            <span className={`receive-status ${inspection ? "is-ready" : ""}`}>
              {inspection ? "已验证" : "未读取"}
            </span>
          </div>

          <p className="receive-lead">
            {source === "share_link"
              ? "如果只需阅读，直接在浏览器中打开链接即可。这里用于下载分享包、恢复代码，或从交接目录创建新的 Claude Code、ChatGPT 任务。"
              : "选择 Relay 生成的本地加密包并填写 43 位密钥。导入不会写入 Claude Code 或 ChatGPT 的原生历史。"}
          </p>

          <div className="receive-form">
            {source === "share_link" ? (
              <>
                <label>
                  <span>完整分享链接</span>
                  <textarea
                    value={shareUrl}
                    onChange={(event) => setShareUrl(event.target.value)}
                    placeholder="粘贴 Relay 生成的完整分享链接"
                    spellCheck={false}
                  />
                </label>
                <label>
                  <span>保存密文包</span>
                  <input
                    value={downloadPath}
                    onChange={(event) => setDownloadPath(event.target.value)}
                    spellCheck={false}
                  />
                  <small>验证失败会删除这个文件，不会留下无法认证的包。</small>
                </label>
              </>
            ) : (
              <>
                <label>
                  <span>.relaypack 路径</span>
                  <input
                    value={packagePath}
                    onChange={(event) => setPackagePath(event.target.value)}
                    placeholder="/Users/name/Downloads/example.relaypack"
                    spellCheck={false}
                  />
                </label>
                <label>
                  <span>解密密钥</span>
                  <textarea
                    value={key}
                    onChange={(event) => setKey(event.target.value)}
                    placeholder="43 位 Base64URL 密钥"
                    spellCheck={false}
                  />
                </label>
              </>
            )}
          </div>

          {error ? <div className="receive-error"><strong>不能继续</strong><p>{error}</p></div> : null}

          <button
            className="primary-action"
            type="button"
            disabled={!canInspect || busy !== null}
            onClick={() => void inspect()}
          >
            <span>{busy === "inspect" ? "正在下载并验证" : "验证分享包"}</span>
            <b>→</b>
          </button>
        </section>

        <section className="receive-preview-panel">
          {preview ? (
            <>
              <div className="preview-heading">
                <span className={`agent-seal small ${preview.source_agent}`}>
                  {preview.source_agent === "claude_code" ? "C" : "GPT"}
                </span>
                <div>
                  <span className="eyebrow">已验证的分享包</span>
                  <h2 title={preview.title}>{preview.title}</h2>
                  <p>{preview.project_name} · {agentLabel(preview.source_agent)}</p>
                </div>
              </div>

              <div className="receive-metrics">
                <div><span>会话消息</span><strong>{preview.conversation_records}</strong></div>
                <div><span>包内文件</span><strong>{preview.asset_count}</strong></div>
                <div><span>代码改动</span><strong>{preview.git_included ? "包含" : "无"}</strong></div>
                <div><span>提示</span><strong>{warningCount}</strong></div>
              </div>

              <div className="package-identity">
                <span>分享包编号</span><code>{preview.package_id}</code>
                {preview.git_included ? (
                  <>
                    <span>基准提交</span><code>{shortCommit(preview.head)}</code>
                    <span>来源分支</span><code>{preview.branch ?? "—"}</code>
                  </>
                ) : (
                  <>
                    <span>内容类型</span><code>会话与项目说明</code>
                  </>
                )}
              </div>

              {preview.diagnostics.length > 0 ? (
                <div className="receive-diagnostics">
                  {preview.diagnostics.slice(0, 6).map((item) => (
                    <p key={`${item.code}-${item.scope}`} className={`severity-${item.severity}`}>
                      <b>{diagnosticCopy(item.code, item.message).title}</b>
                      <span>{diagnosticCopy(item.code, item.message).message}</span>
                    </p>
                  ))}
                </div>
              ) : null}

              <div className="restore-form">
                <div className="restore-heading">
                  <div>
                    <span className="eyebrow">保存交接内容</span>
                    <h3>{preview.git_included ? "创建新的 Git 工作树" : "创建交接文件夹"}</h3>
                  </div>
                  <small>{preview.git_included ? "不会覆盖已有目录或分支" : "不会修改现有项目或会话历史"}</small>
                </div>
                {!preview.git_included ? (
                  <div className="conversation-only-note">
                    <strong>无需选择 Git 仓库</strong>
                    <p>这个分享包只包含会话与项目说明。Relay 会创建一个普通文件夹，并在其中写入 HANDOFF.md，供新的 Claude Code 或 ChatGPT 任务读取。</p>
                  </div>
                ) : null}
                {preview.git_included ? (
                  <label>
                    <span>接收方仓库根目录</span>
                    <input value={repositoryPath} onChange={(event) => setRepositoryPath(event.target.value)} spellCheck={false} />
                  </label>
                ) : null}
                <label>
                  <span>{preview.git_included ? "新 Git 工作树路径" : "新交接文件夹位置"}</span>
                  <input value={targetPath} onChange={(event) => setTargetPath(event.target.value)} spellCheck={false} />
                </label>
                {preview.git_included ? (
                  <label>
                    <span>新分支名</span>
                    <input value={branchName} onChange={(event) => setBranchName(event.target.value)} spellCheck={false} />
                  </label>
                ) : null}
                <button
                  className="primary-action"
                  type="button"
                  disabled={!canRestore || busy !== null || restoreResult !== null}
                  onClick={() => void restore()}
                >
                  <span>
                    {busy === "restore"
                      ? "正在保存"
                      : restoreResult
                        ? "已经保存"
                        : preview.git_included
                          ? "确认并创建 Git 工作树"
                          : "创建交接文件夹"}
                  </span>
                  <b>→</b>
                </button>
              </div>

              {restoreResult ? (
                <div className="restore-success">
                  <span className="eyebrow">交接内容已就绪</span>
                  <h3>{preview.git_included ? "Git 工作树已经创建" : "交接文件夹已经创建"}</h3>
                  <code>{restoreResult.worktree_path}</code>
                  <p>HANDOFF.md 位于 {restoreResult.handoff_markdown_path}</p>
                  <p className="agent-continuation-note">
                    下方操作会在官方应用中新建任务并提交读取 HANDOFF.md 的说明，不会把原会话复制成 ChatGPT 或 Claude Code 的历史记录。
                  </p>
                  <div className="agent-launch-grid">
                    <button type="button" disabled={busy !== null} onClick={() => void openAgent("claude_code")}>
                      <b>C</b><span><strong>在 Claude Code 中继续</strong><small>创建后台会话并读取 HANDOFF.md</small></span>
                    </button>
                    <button type="button" disabled={busy !== null} onClick={() => void openAgent("codex")}>
                      <b>GPT</b><span><strong>在 ChatGPT 新任务中继续</strong><small>需要 macOS 官方 ChatGPT 应用</small></span>
                    </button>
                  </div>
                  {launchResult ? (
                    <p className="launch-note">
                      {launchMessage(launchResult)}
                    </p>
                  ) : null}
                </div>
              ) : null}
            </>
          ) : (
            <div className="receive-empty">
              <span>R</span>
              <h2>尚未载入分享包</h2>
              <p>在左侧输入分享链接或本地文件。验证完成后，这里将显示项目、会话内容、Git 信息和安全提示。</p>
            </div>
          )}
        </section>
      </main>
    </>
  );
}
