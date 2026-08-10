import { useEffect, useMemo, useState } from "react";
import {
  DEFAULT_SESSION_LIMIT,
  EXPANDED_SESSION_LIMIT,
  exportRelaypack,
  inspectRepository,
  loadWorkspaceSnapshot,
  previewSession,
} from "./lib/tauri";
import { groupProjects } from "./lib/projects";
import { userErrorMessage } from "./lib/errors";
import { buildSessionState } from "./lib/session-state";
import { sessionKey, uniqueSessionDisplayTitles } from "./lib/sessions";
import CloudShareActions from "./CloudShareActions";
import ConversationViewer from "./ConversationViewer";
import ContentSelectionPanel, {
  type ContentSelectionSummary,
} from "./ContentSelectionPanel";
import ReceivePanel from "./ReceivePanel";
import ShareHistoryPanel from "./ShareHistoryPanel";
import type {
  AgentKind,
  ExcludedContentBlock,
  ExportRelaypackResult,
  RepositoryInspection,
  SessionContentPreview,
  SessionSummary,
  WorkspaceLoadIssue,
  WorkspaceSnapshot,
} from "./types";

type ShareOptions = {
  conversation: boolean;
  toolEvidence: boolean;
  gitState: boolean;
  projectInstructions: boolean;
  environment: boolean;
};

const shareOptionOrder: (keyof ShareOptions)[] = [
  "conversation",
  "toolEvidence",
  "gitState",
  "projectInstructions",
  "environment",
];

type AppView = "sessions" | "shares" | "receive";

type Notice = {
  message: string;
  tone: "success" | "warning" | "error";
};

type SensitiveExportFinding = {
  code: string;
  label: string;
  scope: string;
  count: number;
};

export const initialOptions: ShareOptions = {
  conversation: true,
  toolEvidence: true,
  gitState: true,
  projectInstructions: true,
  environment: false,
};

export const shareOptionCopy: Record<
  keyof ShareOptions,
  { title: string; description: string }
> = {
  conversation: {
    title: "聊天记录",
    description: "用户与助手在这条会话中的可见消息。",
  },
  toolEvidence: {
    title: "工具记录",
    description: "已经执行的命令和返回结果，只供接收者查看。",
  },
  gitState: {
    title: "代码改动",
    description: "本地提交、已暂存、未暂存以及已选择的新文件。",
  },
  projectInstructions: {
    title: "项目说明",
    description: "会话中已经出现的 AGENTS.md、CLAUDE.md 等说明。",
  },
  environment: {
    title: "设备信息",
    description: "操作系统、处理器架构和已发现的本机工具。",
  },
};

export function shareOptionsSummary(options: ShareOptions): string {
  const selected = shareOptionOrder
    .filter((key) => options[key])
    .map((key) => shareOptionCopy[key].title);
  return selected.length > 0
    ? `当前发送：${selected.join("、")}。`
    : "当前没有选择发送内容。";
}

function shortPath(path: string): string {
  if (!path) return "路径未知";
  const parts = path.replace(/\\/g, "/").split("/").filter(Boolean);
  return parts.length > 3 ? `…/${parts.slice(-3).join("/")}` : path;
}

function relativeTime(value?: string): string {
  if (!value) return "时间未知";
  const time = new Date(value).getTime();
  if (Number.isNaN(time)) return value;
  const diff = Date.now() - time;
  const minutes = Math.floor(diff / 60_000);
  if (minutes < 1) return "刚刚";
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  const days = Math.floor(hours / 24);
  return `${days} 天前`;
}

function safeFilename(value: string): string {
  return value
    .normalize("NFKC")
    .replace(/[\\/:*?"<>|\u0000-\u001f]/g, "-")
    .replace(/\s+/g, "-")
    .replace(/-+/g, "-")
    .replace(/^-|-$/g, "")
    .slice(0, 64) || "handoff";
}

function defaultPackPath(home: string | undefined, session: SessionSummary): string {
  const stamp = new Date().toISOString().replace(/[-:]/g, "").slice(0, 15);
  const filename = `Relay-${safeFilename(session.projectName)}-${stamp}.relaypack`;
  return `${home ?? session.cwd.replace(/\/[^/]+$/, "")}/Downloads/${filename}`;
}

function agentName(agent: AgentKind): string {
  return agent === "claude_code" ? "Claude Code" : "ChatGPT";
}

function issueStageName(stage: WorkspaceLoadIssue["stage"]): string {
  if (stage === "environment_status") return "读取本机环境";
  if (stage === "adapter_health") return "检查会话读取组件";
  return "读取本机会话";
}

function issueTitle(issue: WorkspaceLoadIssue): string {
  if (issue.code === "claude_home_missing") return "没有找到 Claude Code 会话目录";
  if (issue.code === "codex_home_missing") return "没有找到 ChatGPT 会话目录";
  if (issue.code === "claude_scan_failed") return "部分 Claude Code 会话无法读取";
  if (issue.code === "codex_scan_failed") return "部分 ChatGPT 会话无法读取";
  if (issue.code === "session_too_large") return "一条超大会话未显示";
  if (issue.code === "session_parse_failed") return "一条会话无法读取";
  if (issue.code === "adapter_not_found") return "找不到会话读取组件";
  if (issue.code === "adapter_timeout") return "会话读取组件响应超时";
  if (issue.code === "adapter_incompatible") return "会话读取组件版本不兼容";
  return `${issueStageName(issue.stage)}失败`;
}

export function issueDescription(issue: WorkspaceLoadIssue): string {
  if (issue.code === "claude_home_missing") {
    return "尚未发现 Claude Code 会话。请先在 Claude Code 中打开项目并发送一条消息，然后重新读取。";
  }
  if (issue.code === "codex_home_missing") {
    return "尚未发现 ChatGPT 本机任务。请先打开 ChatGPT 并创建一条任务，然后重新读取。";
  }
  if (issue.code === "claude_scan_failed") {
    return "Claude Code 的部分会话没有显示，已经读取的会话仍可正常使用。";
  }
  if (issue.code === "codex_scan_failed") {
    return "ChatGPT 的部分任务没有显示，已经读取的任务仍可正常使用。";
  }
  if (issue.code === "session_too_large") {
    return "一条会话超过当前读取上限，其他会话不受影响。";
  }
  if (issue.code === "session_parse_failed") {
    return "一条会话记录无法安全读取，Relay 已跳过该记录。";
  }
  if (issue.code === "adapter_not_found") {
    return "Relay 安装不完整，请重新安装当前版本。";
  }
  if (issue.code === "adapter_timeout") {
    return "本机会话较多或文件较大，读取时间超过限制。请稍后重试。";
  }
  if (issue.code === "adapter_incompatible") {
    return "会话读取组件与 Relay 版本不一致，请重新安装当前版本。";
  }
  return "本机数据暂时无法读取，已经显示的会话不受影响。";
}

function inferredNoticeTone(message: string): Notice["tone"] {
  if (/失败|不能|无法|错误/.test(message)) return "error";
  if (/尚未|未完成|未知|请检查|请通过另一个渠道/.test(message)) return "warning";
  return "success";
}

export function nativeErrorCode(error: unknown): string | undefined {
  if (typeof error !== "object" || error === null || !("code" in error)) return undefined;
  const code = (error as { code: unknown }).code;
  return typeof code === "string" && code.trim() ? code : undefined;
}

export function reconcilePreviewSelection(
  latest: SessionContentPreview,
  excludedMessageIds: string[],
  excludedBlocks: ExcludedContentBlock[],
): { excludedMessageIds: string[]; excludedBlocks: ExcludedContentBlock[] } {
  const messages = new Map(
    latest.conversation.messages.map((message) => [message.id, message]),
  );
  return {
    excludedMessageIds: excludedMessageIds.filter((messageId) => messages.has(messageId)),
    excludedBlocks: excludedBlocks.filter((block) => {
      const message = messages.get(block.message_id);
      return Boolean(message?.blocks[block.block_index]);
    }),
  };
}

export function previewUpdateMessage(
  previous: SessionContentPreview,
  latest: SessionContentPreview,
): string {
  const previousIds = new Set(previous.conversation.messages.map((message) => message.id));
  const added = latest.conversation.messages.filter((message) => !previousIds.has(message.id)).length;
  if (added > 0) {
    return `会话已新增 ${added} 条记录。Relay 已重新读取最新内容，并保留仍然有效的选择；请检查新增内容后再次生成。`;
  }
  return "会话内容已发生变化。Relay 已重新读取最新内容，并保留仍然有效的选择；请再次检查后生成。";
}

function sensitiveFindingsFromError(error: unknown): SensitiveExportFinding[] {
  if (typeof error !== "object" || error === null) return [];
  const value = error as { code?: unknown; details?: unknown };
  if (value.code !== "sensitive_content_confirmation_required") return [];
  if (typeof value.details !== "object" || value.details === null) return [];
  const findings = (value.details as { findings?: unknown }).findings;
  if (!Array.isArray(findings)) return [];
  return findings.flatMap((finding) => {
    if (typeof finding !== "object" || finding === null) return [];
    const item = finding as Record<string, unknown>;
    if (
      typeof item.code !== "string" ||
      typeof item.label !== "string" ||
      typeof item.scope !== "string"
    ) {
      return [];
    }
    return [{
      code: item.code,
      label: item.label,
      scope: item.scope,
      count: typeof item.count === "number" && item.count > 0 ? item.count : 1,
    }];
  });
}

function sensitiveScopeName(scope: string): string {
  if (scope === "conversation") return "会话";
  if (scope === "session_state") return "任务说明";
  if (scope === "git_patch") return "Git 改动";
  if (scope === "untracked_file") return "新文件";
  return "已选内容";
}

function Icon({ name, size = 18 }: { name: string; size?: number }) {
  const paths: Record<string, React.ReactNode> = {
    sessions: <><path d="M4 5.5h12M4 10h12M4 14.5h8" /><circle cx="2.25" cy="5.5" r=".55" fill="currentColor" stroke="none" /><circle cx="2.25" cy="10" r=".55" fill="currentColor" stroke="none" /><circle cx="2.25" cy="14.5" r=".55" fill="currentColor" stroke="none" /></>,
    package: <><path d="m3 6 7-3 7 3-7 3-7-3Z" /><path d="m3 6 7 3v8l-7-3V6Zm14 0-7 3v8l7-3V6Z" /></>,
    receive: <><path d="M10 2v10m0 0 4-4m-4 4L6 8" /><path d="M3 14v3h14v-3" /></>,
    search: <><circle cx="8.5" cy="8.5" r="5" /><path d="m12.2 12.2 4 4" /></>,
    chevron: <path d="m7 4 6 6-6 6" />,
    refresh: <><path d="M15.5 6A6.5 6.5 0 1 0 16 13" /><path d="M15.5 2.5V6H12" /></>,
    shield: <path d="M10 2.5 16 5v4.5c0 3.8-2.4 6.5-6 8-3.6-1.5-6-4.2-6-8V5l6-2.5Z" />,
    arrow: <><path d="M3 10h13" /><path d="m12 6 4 4-4 4" /></>,
    warning: <><path d="m10 2.5 8 14H2l8-14Z" /><path d="M10 7v4m0 2.6v.1" /></>,
  };
  return (
    <svg
      aria-hidden="true"
      width={size}
      height={size}
      viewBox="0 0 20 20"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {paths[name] ?? paths.sessions}
    </svg>
  );
}

function Toggle({
  checked,
  title,
  description,
  onChange,
}: {
  checked: boolean;
  title: string;
  description: string;
  onChange: () => void;
}) {
  return (
    <button
      className="share-toggle"
      type="button"
      onClick={onChange}
      role="switch"
      aria-checked={checked}
      aria-label={`${title}，当前${checked ? "已包含" : "未包含"}`}
    >
      <span className={`toggle-track ${checked ? "is-on" : ""}`}>
        <span />
      </span>
      <span className="toggle-copy">
        <span className="toggle-heading">
          <strong>{title}</strong>
          <span className={`toggle-state ${checked ? "is-on" : ""}`}>{checked ? "已包含" : "未包含"}</span>
        </span>
        <small>{description}</small>
      </span>
    </button>
  );
}

function FilePicker({
  title,
  paths,
  selected,
  onChange,
}: {
  title: string;
  paths: string[];
  selected: string[];
  onChange: (value: string[]) => void;
}) {
  if (paths.length === 0) return null;
  const selectedSet = new Set(selected);
  return (
    <details className="untracked-picker">
      <summary className="picker-heading">
        <span>{title}</span>
        <small>{selected.length} / {paths.length}</small>
      </summary>
      <div className="picker-list">
        {paths.map((path) => {
          const checked = selectedSet.has(path);
          return (
            <label key={path}>
              <input
                type="checkbox"
                checked={checked}
                onChange={() => onChange(checked
                  ? selected.filter((item) => item !== path)
                  : [...selected, path])}
              />
              <code>{path}</code>
            </label>
          );
        })}
      </div>
    </details>
  );
}

function WorkspaceLoadingScreen() {
  const [elapsedSeconds, setElapsedSeconds] = useState(0);

  useEffect(() => {
    const startedAt = Date.now();
    const timer = window.setInterval(() => {
      setElapsedSeconds(Math.floor((Date.now() - startedAt) / 1_000));
    }, 1_000);
    return () => window.clearInterval(timer);
  }, []);

  const waitingCopy = elapsedSeconds >= 8
    ? "会话较多或文件较大，Relay 仍在继续读取。"
    : "正在等待本机读取完成。";

  return (
    <main className="loading-screen" aria-busy="true" aria-live="polite">
      <section className="workspace-loading-card">
        <header>
          <div className="relay-mark large"><span>R</span><i /><b /></div>
          <div>
            <span>Relay</span>
            <h1>正在读取本机会话</h1>
            <p>正在检查 ChatGPT 和 Claude Code 的本机记录。会话较多时需要一些时间。</p>
          </div>
        </header>

        <div
          className="workspace-loading-progress"
          role="progressbar"
          aria-label="读取本机会话"
          aria-valuetext={`${waitingCopy} 已等待 ${elapsedSeconds} 秒`}
        >
          <i />
        </div>

        <div className="workspace-loading-status" role="status">
          <span>{waitingCopy}</span>
          <time>{elapsedSeconds} 秒</time>
        </div>

        <div className="workspace-loading-stages" aria-label="正在读取的内容">
          <span><i />查找会话目录</span>
          <span><i />读取标题和时间</span>
          <span><i />整理项目与会话</span>
        </div>

        <div className="workspace-loading-preview" aria-hidden="true">
          {[0, 1, 2].map((item) => (
            <div key={item}>
              <i />
              <span><b /><em /></span>
            </div>
          ))}
        </div>

        <small>Relay 只读取本机记录，不会修改原会话。</small>
      </section>
    </main>
  );
}

function App() {
  const [view, setView] = useState<AppView>("sessions");
  const [snapshot, setSnapshot] = useState<WorkspaceSnapshot | null>(null);
  const [query, setQuery] = useState("");
  const [selectedProject, setSelectedProject] = useState<string | null>(null);
  const [selectedSessionKey, setSelectedSessionKey] = useState<string | null>(null);
  const [options, setOptions] = useState<ShareOptions>(initialOptions);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [repository, setRepository] = useState<RepositoryInspection | null>(null);
  const [contentPreview, setContentPreview] = useState<SessionContentPreview | null>(null);
  const [contentPreviewError, setContentPreviewError] = useState<string | null>(null);
  const [contentPreviewBusy, setContentPreviewBusy] = useState(false);
  const [excludedMessageIds, setExcludedMessageIds] = useState<string[]>([]);
  const [excludedBlocks, setExcludedBlocks] = useState<ExcludedContentBlock[]>([]);
  const [selectedStaged, setSelectedStaged] = useState<string[]>([]);
  const [selectedUnstaged, setSelectedUnstaged] = useState<string[]>([]);
  const [packPath, setPackPath] = useState("");
  const [selectedUntracked, setSelectedUntracked] = useState<string[]>([]);
  const [packResult, setPackResult] = useState<ExportRelaypackResult | null>(null);
  const [packError, setPackError] = useState<string | null>(null);
  const [packReviewNotice, setPackReviewNotice] = useState<string | null>(null);
  const [gitUnavailableNotice, setGitUnavailableNotice] = useState<string | null>(null);
  const [packBusy, setPackBusy] = useState(false);
  const [sensitiveSelectedCount, setSensitiveSelectedCount] = useState(0);
  const [contentSelectionSummary, setContentSelectionSummary] = useState<ContentSelectionSummary | null>(null);
  const [backendSensitiveFindings, setBackendSensitiveFindings] = useState<SensitiveExportFinding[]>([]);
  const [sensitiveAcknowledged, setSensitiveAcknowledged] = useState(false);
  const [showPackDialog, setShowPackDialog] = useState(false);
  const [historyVersion, setHistoryVersion] = useState(0);
  const [shareExpiresInSeconds, setShareExpiresInSeconds] = useState(7 * 24 * 60 * 60);
  const [olderSessionsBusy, setOlderSessionsBusy] = useState(false);
  const [workspaceBusy, setWorkspaceBusy] = useState(false);

  const showNotice = (message: string, tone = inferredNoticeTone(message)) => {
    setNotice({ message, tone });
    window.setTimeout(() => setNotice(null), 5200);
  };

  const load = async (
    requestedLimit = snapshot?.sessionLimit ?? DEFAULT_SESSION_LIMIT,
    preserveCurrent = Boolean(snapshot),
  ) => {
    setWorkspaceBusy(true);
    if (!preserveCurrent) setSnapshot(null);
    try {
      const result = await loadWorkspaceSnapshot(undefined, requestedLimit);
      setSnapshot(result);
      const projects = groupProjects(result.sessions);
      setSelectedProject((current) =>
        current && projects.some((project) => project.id === current)
          ? current
          : projects[0]?.id ?? null);
      setSelectedSessionKey((current) =>
        current && result.sessions.some((session) => sessionKey(session) === current)
          ? current
          : projects[0]?.sessions[0] ? sessionKey(projects[0].sessions[0]) : null);
    } finally {
      setWorkspaceBusy(false);
    }
  };

  useEffect(() => {
    void load();
  }, []);

  const loadOlderSessions = async () => {
    setOlderSessionsBusy(true);
    try {
      await load(EXPANDED_SESSION_LIMIT, true);
    } finally {
      setOlderSessionsBusy(false);
    }
  };

  const projects = useMemo(() => groupProjects(snapshot?.sessions ?? []), [snapshot]);
  const activeProject = projects.find((project) => project.id === selectedProject) ?? projects[0];
  const sessions = useMemo(() => {
    const source = activeProject?.sessions ?? [];
    const needle = query.trim().toLocaleLowerCase("zh-CN");
    if (!needle) return source;
    return source.filter((session) =>
      [session.title, session.preview, session.workspace, session.cwd, agentName(session.agent)]
        .filter(Boolean)
        .some((value) => value!.toLocaleLowerCase("zh-CN").includes(needle)),
    );
  }, [activeProject, query]);
  const activeSession =
    sessions.find((session) => sessionKey(session) === selectedSessionKey) ??
    activeProject?.sessions.find((session) => sessionKey(session) === selectedSessionKey) ??
    sessions[0] ??
    activeProject?.sessions[0];

  const sessionDisplayTitles = useMemo(
    () => uniqueSessionDisplayTitles(activeProject?.sessions ?? []),
    [activeProject],
  );
  const activeSessionIdentity = activeSession ? sessionKey(activeSession) : null;
  const activeSessionTitle = activeSession
    ? sessionDisplayTitles.get(activeSessionIdentity!) ?? activeSession.title
    : "";
  const activeContentPreview = contentPreview && activeSession &&
    contentPreview.source.session_id === activeSession.id &&
    contentPreview.source.agent === activeSession.agent
    ? contentPreview
    : null;

  useEffect(() => {
    if (activeSessionIdentity && activeSessionIdentity !== selectedSessionKey) {
      setSelectedSessionKey(activeSessionIdentity);
    }
  }, [activeSessionIdentity, selectedSessionKey]);

  useEffect(() => {
    setContentPreview(null);
    setContentPreviewError(null);
    setContentPreviewBusy(false);
    setExcludedMessageIds([]);
    setExcludedBlocks([]);
    setSensitiveSelectedCount(0);
    setContentSelectionSummary(null);
    setBackendSensitiveFindings([]);
    setSensitiveAcknowledged(false);
    setPackReviewNotice(null);
  }, [activeSessionIdentity]);

  useEffect(() => {
    if (!activeSession || snapshot?.source !== "native") return;
    let cancelled = false;
    setContentPreviewBusy(true);
    setContentPreviewError(null);
    void previewSession({
      agent: activeSession.agent,
      session_id: activeSession.id,
    }).then((result) => {
      if (!cancelled) setContentPreview(result);
    }).catch((error) => {
      if (!cancelled) setContentPreviewError(userErrorMessage(error, "无法读取会话内容，请重试。"));
    }).finally(() => {
      if (!cancelled) setContentPreviewBusy(false);
    });
    return () => {
      cancelled = true;
    };
  }, [activeSession?.agent, activeSession?.id, snapshot?.source]);

  useEffect(() => {
    setSensitiveAcknowledged(false);
    setBackendSensitiveFindings([]);
  }, [excludedMessageIds, excludedBlocks, selectedStaged, selectedUnstaged, selectedUntracked, options]);

  const hasSensitiveSelection = sensitiveSelectedCount > 0 || backendSensitiveFindings.length > 0;

  const selectedOptionsSummary = shareOptionsSummary(options);
  const contentSelectionOptions = useMemo(() => ({
    conversation: options.conversation,
    toolEvidence: options.toolEvidence,
    projectInstructions: options.projectInstructions,
  }), [options.conversation, options.projectInstructions, options.toolEvidence]);
  const loadErrors = snapshot?.issues.filter((issue) => issue.severity === "error") ?? [];
  const loadWarnings = snapshot?.issues.filter((issue) => issue.severity === "warning") ?? [];
  const discoveryError = loadErrors.find((issue) => issue.stage === "discover_sessions");
  const selectedGitFiles = selectedStaged.length + selectedUnstaged.length + selectedUntracked.length;

  const flipOption = (key: keyof ShareOptions) => {
    setOptions((current) => ({ ...current, [key]: !current[key] }));
  };

  const retryConversation = async () => {
    if (!activeSession) return;
    if (snapshot?.source !== "native") {
      showNotice("当前环境无法读取本机会话，请打开 Relay 桌面应用。", "warning");
      return;
    }
    setContentPreviewBusy(true);
    setContentPreviewError(null);
    try {
      setContentPreview(await previewSession({
        agent: activeSession.agent,
        session_id: activeSession.id,
      }));
    } catch (error) {
      setContentPreviewError(userErrorMessage(error, "无法读取会话内容，请重试。"));
    } finally {
      setContentPreviewBusy(false);
    }
  };

  const preparePackage = async () => {
    if (!activeSession) return;
    if (snapshot?.source !== "native") {
      showNotice("当前环境无法读取本机会话，请打开 Relay 桌面应用。", "warning");
      return;
    }
    setPackBusy(true);
    setPackError(null);
    setPackReviewNotice(null);
    setGitUnavailableNotice(null);
    setPackResult(null);
    setRepository(null);
    setExcludedMessageIds([]);
    setExcludedBlocks([]);
    setSensitiveSelectedCount(0);
    setContentSelectionSummary(null);
    setBackendSensitiveFindings([]);
    setSensitiveAcknowledged(false);
    setOptions(initialOptions);
    setSelectedStaged([]);
    setSelectedUnstaged([]);
    setSelectedUntracked([]);
    setPackPath(defaultPackPath(snapshot.environment.home, activeSession));
    setShowPackDialog(true);
    const previewTask = activeContentPreview
      ? Promise.resolve(activeContentPreview)
      : previewSession({
          agent: activeSession.agent,
          session_id: activeSession.id,
        });
    const repositoryTask = inspectRepository(activeSession.cwd);
    const [previewResult, repositoryResult] = await Promise.allSettled([
      previewTask,
      repositoryTask,
    ]);
    if (previewResult.status === "fulfilled") {
      setContentPreview(previewResult.value);
    } else {
      setContentPreviewError(userErrorMessage(previewResult.reason, "无法读取会话内容，请重试。"));
    }
    if (repositoryResult.status === "fulfilled") {
      const inspection = repositoryResult.value;
      setRepository(inspection);
      setSelectedStaged(inspection?.staged.map((item) => item.path) ?? []);
      setSelectedUnstaged(inspection?.unstaged.map((item) => item.path) ?? []);
    } else {
      if (nativeErrorCode(repositoryResult.reason) === "not_a_git_repository") {
        setOptions((current) => ({ ...current, gitState: false }));
        setRepository(null);
        setGitUnavailableNotice(
          "当前文件夹不是 Git 项目，本次不包含代码改动。",
        );
        showNotice("当前文件夹不是 Git 项目，本次不包含代码改动。", "warning");
      } else {
        setPackError(userErrorMessage(repositoryResult.reason, "无法读取代码改动，请确认项目目录可以正常访问。"));
      }
    }
    setPackBusy(false);
  };

  const generatePack = async () => {
    if (!activeSession || !packPath.trim()) return;
    if (contentPreviewError || !contentPreview) {
      setPackError("无法读取会话内容，无法创建分享链接。");
      return;
    }
    if (!contentPreview.preview_sha256?.trim()) {
      setPackError("无法确认当前会话内容是否完整，请重新打开这条会话后再试。");
      return;
    }
    if (hasSensitiveSelection && !sensitiveAcknowledged) {
      setPackError("仍有疑似敏感内容被选中。请取消对应内容，或明确确认后再生成。");
      return;
    }
    setPackBusy(true);
    setPackError(null);
    setPackReviewNotice(null);
    try {
      const result = await exportRelaypack({
        agent: activeSession.agent,
        session_id: activeSession.id,
        preview_sha256: contentPreview.preview_sha256,
        output_path: packPath.trim(),
        repository_path: options.gitState && repository ? repository.root : undefined,
        include_conversation: options.conversation,
        include_tool_evidence: options.toolEvidence,
        include_project_instructions: options.projectInstructions,
        include_environment: options.environment,
        include_git: options.gitState && Boolean(repository),
        include_local_commits: options.gitState,
        include_staged: options.gitState && selectedStaged.length > 0,
        include_unstaged: options.gitState && selectedUnstaged.length > 0,
        selected_staged: options.gitState ? selectedStaged : [],
        selected_unstaged: options.gitState ? selectedUnstaged : [],
        selected_untracked: options.gitState ? selectedUntracked : [],
        excluded_message_ids: excludedMessageIds,
        excluded_blocks: excludedBlocks,
        allow_sensitive_content: sensitiveAcknowledged,
        session_state: buildSessionState({
          preview: contentPreview,
          fallbackTitle: activeSession.title,
          repository,
          includeConversation: options.conversation,
          includeToolEvidence: options.toolEvidence,
          includeGit: options.gitState && Boolean(repository),
          selectedStaged,
          selectedUnstaged,
          selectedUntracked,
          excludedMessageIds,
          excludedBlocks,
        }),
      });
      setBackendSensitiveFindings([]);
      setPackResult(result);
    } catch (error) {
      if (nativeErrorCode(error) === "session_preview_changed") {
        try {
          const latest = await previewSession({
            agent: activeSession.agent,
            session_id: activeSession.id,
          });
          const reconciled = reconcilePreviewSelection(
            latest,
            excludedMessageIds,
            excludedBlocks,
          );
          setContentPreview(latest);
          setExcludedMessageIds(reconciled.excludedMessageIds);
          setExcludedBlocks(reconciled.excludedBlocks);
          setBackendSensitiveFindings([]);
          setSensitiveAcknowledged(false);
          setPackReviewNotice(previewUpdateMessage(contentPreview, latest));
          return;
        } catch (refreshError) {
          setPackError(`会话已经更新，但重新读取失败：${userErrorMessage(refreshError, "请关闭窗口后重试。")}`);
          return;
        }
      }
      const findings = sensitiveFindingsFromError(error);
      if (findings.length > 0) {
        setBackendSensitiveFindings(findings);
        setSensitiveAcknowledged(false);
        setPackError("后端在最终内容中发现了疑似敏感信息。这里只显示类型，不显示原文；请检查后再确认是否继续。");
      } else {
        setPackError(userErrorMessage(error, "无法创建分享链接，请根据页面提示重试。"));
      }
    } finally {
      setPackBusy(false);
    }
  };

  if (!snapshot) {
    return <WorkspaceLoadingScreen />;
  }

  if (snapshot.source === "unavailable") {
    return (
      <main className="desktop-required-screen">
        <div className="desktop-required-card">
          <div className="relay-mark large"><span>R</span><i /><b /></div>
          <span className="eyebrow">Relay 桌面应用</span>
          <h1>请从应用程序中打开 Relay</h1>
          <p>
            浏览器不能读取本机的 ChatGPT 和 Claude Code 会话，也不能恢复代码或创建新会话。
            打开安装后的 Relay，应用会读取本机可分享的会话。
          </p>
          <div className="desktop-required-note">
            <strong>浏览器分享链接仍可直接查看</strong>
            <span>接收者打开分享链接时，不需要安装 Relay；只有恢复文件和导入会话需要桌面应用。</span>
          </div>
        </div>
      </main>
    );
  }

  return (
    <div className="app-shell">
      <header className="topbar">
        <div className="window-drag">
          <div className="relay-mark"><span>R</span><i /><b /></div>
          <div className="product-title">
            <strong>Relay</strong>
            <span>会话与代码分享</span>
          </div>
        </div>
        <div className="topbar-status">
          {view === "sessions" ? (
            <button
              className="icon-button"
              type="button"
              onClick={() => void load()}
              aria-label={workspaceBusy ? "正在重新读取会话" : "重新读取会话"}
              disabled={workspaceBusy}
            >
              <Icon name="refresh" />
            </button>
          ) : null}
        </div>
      </header>

      <aside className="rail" aria-label="主导航">
        <nav>
          <button
            className={`rail-button ${view === "sessions" ? "is-active" : ""}`}
            type="button"
            aria-label="会话"
            onClick={() => setView("sessions")}
          >
            <Icon name="sessions" />
            <span>会话</span>
          </button>
          <button
            className={`rail-button ${view === "shares" ? "is-active" : ""}`}
            type="button"
            aria-label="分享记录"
            onClick={() => setView("shares")}
          >
            <Icon name="package" />
            <span>分享</span>
          </button>
          <button
            className={`rail-button ${view === "receive" ? "is-active" : ""}`}
            type="button"
            aria-label="接收"
            onClick={() => setView("receive")}
          >
            <Icon name="receive" />
            <span>接收</span>
          </button>
        </nav>
      </aside>

      {view === "sessions" ? (
        <>
      <aside className="project-pane">
        <div className="pane-heading">
          <h2>项目</h2>
          <span className="count-chip">{projects.length}</span>
        </div>

        <div className="project-list">
          {projects.length === 0 ? (
            <div className="project-empty">
              <strong>{discoveryError ? "会话读取失败" : "还没有发现项目"}</strong>
              <p>{discoveryError ? "查看右侧错误后重新读取。" : "Relay 会按 Git 项目整理 Claude Code 和 ChatGPT 会话。"}</p>
            </div>
          ) : projects.map((project) => {
            const active = project.id === activeProject?.id;
            return (
              <button
                className={`project-row ${active ? "is-active" : ""}`}
                key={project.id}
                type="button"
                onClick={() => {
                  setSelectedProject(project.id);
                  setSelectedSessionKey(
                    project.sessions[0] ? sessionKey(project.sessions[0]) : null,
                  );
                  setQuery("");
                }}
              >
                <span className="project-copy">
                  <strong title={project.name}>{project.name}</strong>
                  <small>{shortPath(project.path)}</small>
                </span>
                <span className="project-meta">
                  <b>{project.sessions.length}<small>会话</small></b>
                </span>
              </button>
            );
          })}
        </div>

        {snapshot.hasMoreSessions && snapshot.sessionLimit < EXPANDED_SESSION_LIMIT ? (
          <div className="project-history-more">
            <p>当前显示最近 {snapshot.sessionLimit} 条会话。需要查找较早的项目时，可以继续读取。</p>
            <button
              type="button"
              disabled={olderSessionsBusy || workspaceBusy}
              onClick={() => void loadOlderSessions()}
            >
              {olderSessionsBusy ? "正在读取较早记录" : "显示更早记录"}
            </button>
          </div>
        ) : null}

      </aside>

      <main className="workspace">
        <section className="session-column">
          <div className="workspace-heading">
            <h1>会话</h1>
            <div className="workspace-count"><b>{sessions.length}</b><span>个会话</span></div>
          </div>

          {snapshot.issues.length > 0 ? (
            <section className={`workspace-issues ${loadErrors.length > 0 ? "has-error" : ""}`} aria-label="本机读取状态">
              <div className="workspace-issues-heading">
                <div>
                  <strong>{loadErrors.length > 0 ? "部分本机数据没有读取成功" : "有些会话未显示"}</strong>
                  <span>{loadErrors.length > 0 ? "已经读到的会话仍会保留。" : "其余会话已经正常读取。"}</span>
                </div>
                <button type="button" disabled={workspaceBusy} onClick={() => void load()}>
                  {workspaceBusy ? "正在读取" : "重新读取"}
                </button>
              </div>
              <div className="workspace-issue-list">
                {[...loadErrors, ...loadWarnings].map((issue, index) => (
                  <article key={`${issue.stage}-${issue.code}-${index}`}>
                    <span>{issue.severity === "error" ? "失败" : "提示"}</span>
                    <div>
                      <strong>{issueTitle(issue)}</strong>
                      <p>{issueDescription(issue)}</p>
                      <details className="workspace-issue-details">
                        <summary>查看技术信息</summary>
                        <code>{issueStageName(issue.stage)} · {issue.code}</code>
                        {issue.message ? <pre>{issue.message}</pre> : null}
                      </details>
                    </div>
                  </article>
                ))}
              </div>
            </section>
          ) : null}

          <label className="search-box">
            <Icon name="search" />
            <input
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="搜索标题、路径或摘要"
            />
          </label>

          <div className="session-list">
            {sessions.length === 0 ? (
              <div className="empty-state">
                <h3>{discoveryError ? "会话读取失败" : projects.length === 0 ? "没有发现本机会话" : "没有匹配的会话"}</h3>
                <p>
                  {discoveryError
                    ? "上方显示了失败步骤和原因。处理后点“重新读取”。"
                    : projects.length === 0
                      ? "请先在 Claude Code 或 ChatGPT 中打开项目并产生一条会话，然后重新读取。"
                      : "换一个关键词，或者清空搜索内容。"}
                </p>
              </div>
            ) : (
              sessions.map((session, index) => {
                const key = sessionKey(session);
                const active = key === activeSessionIdentity;
                const displayTitle = sessionDisplayTitles.get(key) ?? session.title;
                return (
                  <button
                    className={`session-row ${active ? "is-active" : ""}`}
                    key={key}
                    type="button"
                    onClick={() => setSelectedSessionKey(key)}
                  >
                    <span className="session-number">{String(index + 1).padStart(2, "0")}</span>
                    <span className="session-body">
                      <span className="session-topline">
                        <span className={`agent-label ${session.agent}`}>
                          <i /> {agentName(session.agent)}
                        </span>
                        <time>{relativeTime(session.updatedAt)}</time>
                      </span>
                      <strong title={displayTitle}>
                        {displayTitle}
                      </strong>
                      {session.preview ? <p>{session.preview}</p> : null}
                      <span className="session-footer">
                        <span>{session.messageCount ?? "—"} 条消息</span>
                        {session.health !== "complete" ? (
                          <span className={`health ${session.health}`}><i /> {session.health === "partial" ? "部分可读" : "版本未知"}</span>
                        ) : null}
                      </span>
                    </span>
                    <Icon name="chevron" />
                  </button>
                );
              })
            )}
          </div>
        </section>

        <section className="handoff-column">
          {activeSession ? (
            <>
              <div className="handoff-title">
                <div>
                  <h2 title={activeSessionTitle}>{activeSessionTitle}</h2>
                  <p>{agentName(activeSession.agent)} · {shortPath(activeSession.cwd)}</p>
                </div>
              </div>

              <ConversationViewer
                preview={activeContentPreview}
                loading={contentPreviewBusy}
                error={contentPreviewError}
                onRetry={() => void retryConversation()}
                onNotice={(message) => showNotice(message)}
              />

              <div className="session-share-action">
                <div>
                  <strong>分享这条会话</strong>
                  <span>发送前可以检查聊天记录、工具记录和代码改动。</span>
                </div>
                <button className="primary-action compact" type="button" onClick={preparePackage}>
                  <span>创建分享链接</span>
                  <Icon name="arrow" />
                </button>
              </div>
            </>
          ) : (
            <div className="empty-state detail-empty"><h3>选择一条会话</h3><p>Relay 会在这里显示聊天记录和工具记录。</p></div>
          )}
        </section>
      </main>

        </>
      ) : view === "shares" ? (
        <ShareHistoryPanel
          refreshKey={historyVersion}
          onNotice={showNotice}
        />
      ) : (
        <ReceivePanel
          home={snapshot.environment.home}
          onNotice={showNotice}
        />
      )}

      {view === "sessions" && showPackDialog && activeSession ? (
        <div className="dialog-backdrop" role="presentation">
          <section className="pack-dialog" role="dialog" aria-modal="true" aria-labelledby="pack-dialog-title">
            <header className="dialog-header">
              <div>
                <h2 id="pack-dialog-title">{packResult ? "发送分享链接" : "检查要发送的内容"}</h2>
                <p>{activeSessionTitle} · {agentName(activeSession.agent)}</p>
              </div>
              <button
                type="button"
                className="dialog-close"
                onClick={() => setShowPackDialog(false)}
                aria-label="关闭"
              >
                ×
              </button>
            </header>

            {packResult ? (
              <div className="pack-success">
                <CloudShareActions
                  pack={packResult}
                  autoUpload
                  initialExpiresInSeconds={shareExpiresInSeconds}
                  onHistoryChanged={() => setHistoryVersion((value) => value + 1)}
                  onNotice={showNotice}
                />
              </div>
            ) : (
              <div className="dialog-content">
                {packBusy && !contentPreview && !repository ? (
                  <div className="dialog-loading"><i /><span>正在读取会话内容和 Git 变更</span></div>
                ) : null}

                {contentPreviewError ? (
                  <div className="dialog-error">
                    <Icon name="warning" />
                    <div>
                      <strong>无法读取会话内容</strong>
                      <p>{contentPreviewError}</p>
                      <small>Relay 只有在你能逐项检查内容后才会创建分享链接。</small>
                    </div>
                  </div>
                ) : null}

                {contentPreview ? (
                  <>
                    <details className="share-options-section">
                      <summary>
                        <div>
                          <h3>发送内容</h3>
                          <p>{selectedOptionsSummary}</p>
                        </div>
                        <span>修改发送内容</span>
                      </summary>
                      <div className="toggle-list">
                        <Toggle
                          checked={options.conversation}
                          {...shareOptionCopy.conversation}
                          onChange={() => flipOption("conversation")}
                        />
                        <Toggle
                          checked={options.toolEvidence}
                          {...shareOptionCopy.toolEvidence}
                          onChange={() => flipOption("toolEvidence")}
                        />
                        <Toggle
                          checked={options.gitState}
                          {...shareOptionCopy.gitState}
                          onChange={() => flipOption("gitState")}
                        />
                        <Toggle
                          checked={options.projectInstructions}
                          {...shareOptionCopy.projectInstructions}
                          onChange={() => flipOption("projectInstructions")}
                        />
                        <Toggle
                          checked={options.environment}
                          {...shareOptionCopy.environment}
                          onChange={() => flipOption("environment")}
                        />
                      </div>
                      <ContentSelectionPanel
                        preview={contentPreview}
                        options={contentSelectionOptions}
                        excludedMessageIds={excludedMessageIds}
                        excludedBlocks={excludedBlocks}
                        onExcludedMessageIdsChange={setExcludedMessageIds}
                        onExcludedBlocksChange={setExcludedBlocks}
                        onSensitiveCountChange={setSensitiveSelectedCount}
                        onSelectionSummaryChange={setContentSelectionSummary}
                      />
                    </details>
                    {hasSensitiveSelection ? (
                      <label className="sensitive-confirmation">
                        <input
                          type="checkbox"
                          checked={sensitiveAcknowledged}
                          onChange={(event) => setSensitiveAcknowledged(event.target.checked)}
                        />
                        <span>
                          我已经逐项检查，确认仍要保留这些疑似敏感内容。
                          {backendSensitiveFindings.length > 0 ? (
                            <small>
                              {backendSensitiveFindings.map((finding) =>
                                `${sensitiveScopeName(finding.scope)}：${finding.label}${finding.count > 1 ? ` × ${finding.count}` : ""}`,
                              ).join("；")}
                            </small>
                          ) : null}
                        </span>
                      </label>
                    ) : null}
                  </>
                ) : null}

                {repository ? (
                  <>
                    <div className="repository-strip">
                      <div><span>分支</span><strong>{repository.branch ?? "detached"}</strong></div>
                      <div><span>暂存</span><strong>{repository.staged.length}</strong></div>
                      <div><span>未暂存</span><strong>{repository.unstaged.length}</strong></div>
                      <div><span>新文件</span><strong>{repository.untracked.length}</strong></div>
                    </div>

                    <div className="git-selection-note">
                      <Icon name="shield" size={15} />
                      <p><strong>暂存和未暂存改动已经默认选中。</strong> 新文件可能是构建产物或私密配置，Relay 默认不选，请在下面逐项确认。</p>
                    </div>

                    {repository.warnings.length > 0 || repository.ignored_sensitive_files.length > 0 ? (
                      <div className="safety-list">
                        <span className="eyebrow">需要确认</span>
                        {repository.warnings.map((warning) => (
                          <p key={warning.code}><Icon name="warning" size={14} /><span>{warning.message}</span></p>
                        ))}
                        {repository.ignored_sensitive_files.length > 0 ? (
                          <p><Icon name="shield" size={14} /><span>发现 {repository.ignored_sensitive_files.length} 个可能含敏感信息且已被 Git 忽略的文件；它们不会进入分享内容。</span></p>
                        ) : null}
                      </div>
                    ) : null}

                    <FilePicker
                      title="选择暂存改动"
                      paths={repository.staged.map((item) => item.path)}
                      selected={selectedStaged}
                      onChange={setSelectedStaged}
                    />
                    <FilePicker
                      title="选择未暂存改动"
                      paths={repository.unstaged.map((item) => item.path)}
                      selected={selectedUnstaged}
                      onChange={setSelectedUnstaged}
                    />
                    <FilePicker
                      title="选择新文件"
                      paths={repository.untracked}
                      selected={selectedUntracked}
                      onChange={setSelectedUntracked}
                    />
                  </>
                ) : null}

                {gitUnavailableNotice ? (
                  <div className="conversation-only-note">
                    <strong>不包含代码改动</strong>
                    <p>{gitUnavailableNotice}</p>
                  </div>
                ) : null}

                {packError ? (
                  <div className="dialog-error">
                    <Icon name="warning" />
                    <div><strong>无法创建分享链接</strong><p>{packError}</p></div>
                  </div>
                ) : null}

                {packReviewNotice ? (
                  <div className="dialog-update">
                    <Icon name="refresh" />
                    <div>
                      <strong>会话内容已更新</strong>
                      <p>{packReviewNotice}</p>
                    </div>
                  </div>
                ) : null}

                <div className="share-link-settings">
                  <label>
                    <span>链接有效期</span>
                    <select
                      value={shareExpiresInSeconds}
                      onChange={(event) => setShareExpiresInSeconds(Number(event.target.value))}
                    >
                      <option value={24 * 60 * 60}>1 天</option>
                      <option value={7 * 24 * 60 * 60}>7 天</option>
                      <option value={30 * 24 * 60 * 60}>30 天</option>
                    </select>
                  </label>
                  <p>
                    将发送 {contentSelectionSummary?.messages ?? 0} 条消息、
                    {contentSelectionSummary?.toolBlocks ?? 0} 项工具记录、
                    {contentSelectionSummary?.instructionBlocks ?? 0} 段项目说明
                    {options.gitState ? `和 ${selectedGitFiles} 个改动文件` : ""}。
                  </p>
                </div>

                <div className="dialog-actions">
                  <button type="button" className="secondary-action" onClick={() => setShowPackDialog(false)}>取消</button>
                  <button
                    type="button"
                    className="primary-action compact"
                    onClick={() => void generatePack()}
                    disabled={
                      packBusy ||
                      !packPath.trim() ||
                      !contentPreview ||
                      !contentPreview.preview_sha256?.trim() ||
                      Boolean(contentPreviewError) ||
                      (options.gitState && !repository) ||
                      (hasSensitiveSelection && !sensitiveAcknowledged)
                    }
                  >
                    <span>{packBusy ? "正在创建" : "创建分享链接"}</span>
                    <Icon name="arrow" />
                  </button>
                </div>
              </div>
            )}
          </section>
        </div>
      ) : null}

      {notice ? (
        <div className={`toast is-${notice.tone}`} role="status" aria-live="polite">
          <Icon name={notice.tone === "success" ? "check" : "warning"} />
          {notice.message}
        </div>
      ) : null}
    </div>
  );
}

export default App;
