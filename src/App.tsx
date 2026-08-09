import { useEffect, useMemo, useState } from "react";
import { copyText } from "./lib/clipboard";
import {
  exportRelaypack,
  inspectRepository,
  loadWorkspaceSnapshot,
  previewSession,
} from "./lib/tauri";
import { groupProjects } from "./lib/projects";
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
  EnvironmentStatus,
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

type AppView = "sessions" | "shares" | "receive";
type SessionDetailView = "history" | "share";

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
  toolEvidence: false,
  gitState: true,
  projectInstructions: true,
  environment: false,
};

export const shareOptionCopy: Record<
  keyof ShareOptions,
  { title: string; description: string; detail: string; badges: string[] }
> = {
  conversation: {
    title: "会话内容",
    badges: ["默认包含"],
    description: "导出本次会话中用户与助手可见的消息，用于说明任务要求、处理过程和当前结果。",
    detail: "不包含系统提示、模型私有推理或提供方内部记录。",
  },
  toolEvidence: {
    title: "工具调用记录",
    badges: ["默认不包含", "需检查敏感信息"],
    description: "导出终端命令、文件读取与修改、搜索等工具调用及其返回结果。",
    detail: "记录仅供查阅，不会重新执行；其中可能包含本机路径、账号名或凭据。",
  },
  gitState: {
    title: "Git 状态与变更",
    badges: ["默认包含", "未跟踪文件需确认"],
    description: "导出尚未推送的本地提交、已暂存变更、未暂存变更和用户选中的未跟踪文件。",
    detail: "接收时恢复到新建的 Git 工作树，不修改现有项目目录。",
  },
  projectInstructions: {
    title: "项目指令",
    badges: ["默认包含"],
    description: "导出会话中已出现的 AGENTS.md、CLAUDE.md 等项目指令内容。",
    detail: "若会话中没有读取或引用这类文件，分享包不会添加额外内容。",
  },
  environment: {
    title: "运行环境信息",
    badges: ["默认不包含"],
    description: "导出操作系统、处理器架构，以及会话来源应用等基础信息。",
    detail: "不包含环境变量值、密钥、用户名、本机路径或完整软件配置。",
  },
};

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
  const stamp = new Date().toISOString().replace(/[-:]/g, "").slice(0, 13);
  const filename = `Relay-${safeFilename(session.projectName)}-${stamp}.relaypack`;
  return `${home ?? session.cwd.replace(/\/[^/]+$/, "")}/Downloads/${filename}`;
}

function formatBytes(value: number): string {
  if (value < 1024) return `${value} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`;
  return `${(value / (1024 * 1024)).toFixed(1)} MB`;
}

function agentName(agent: AgentKind): string {
  return agent === "claude_code" ? "Claude Code" : "ChatGPT";
}

function agentMark(agent: AgentKind): string {
  return agent === "claude_code" ? "C" : "GPT";
}

function issueStageName(stage: WorkspaceLoadIssue["stage"]): string {
  if (stage === "environment_status") return "读取本机环境";
  if (stage === "adapter_health") return "检查会话读取组件";
  return "扫描本机会话";
}

function issueTitle(issue: WorkspaceLoadIssue): string {
  if (issue.code === "claude_home_missing") return "没有找到 Claude Code 会话目录";
  if (issue.code === "codex_home_missing") return "没有找到 ChatGPT 会话目录";
  if (issue.code === "claude_scan_failed") return "部分 Claude Code 会话无法读取";
  if (issue.code === "codex_scan_failed") return "部分 ChatGPT 会话无法读取";
  if (issue.code === "session_too_large") return "一条超大会话未显示";
  if (issue.code === "adapter_not_found") return "找不到会话读取组件";
  if (issue.code === "adapter_timeout") return "会话读取组件响应超时";
  if (issue.code === "adapter_incompatible") return "会话读取组件版本不兼容";
  return `${issueStageName(issue.stage)}失败`;
}

function inferredNoticeTone(message: string): Notice["tone"] {
  if (/失败|不能|无法|错误/.test(message)) return "error";
  if (/尚未|未完成|未知|请检查|请通过另一个渠道/.test(message)) return "warning";
  return "success";
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  if (typeof error === "object" && error !== null && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return String(error);
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
    return `会话刚刚新增了 ${added} 条记录。Relay 已重新读取最新内容，并保留仍然有效的选择；请检查新增内容后再次生成。`;
  }
  return "会话内容刚刚发生变化。Relay 已重新读取最新内容，并保留仍然有效的选择；请再次检查后生成。";
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
    settings: <><circle cx="10" cy="10" r="2.5" /><path d="M10 2.5v2m0 11v2m7.5-7.5h-2m-11 0h-2m12.8-5.3-1.4 1.4M6.1 13.9l-1.4 1.4m10.6 0-1.4-1.4M6.1 6.1 4.7 4.7" /></>,
    search: <><circle cx="8.5" cy="8.5" r="5" /><path d="m12.2 12.2 4 4" /></>,
    branch: <><circle cx="5" cy="4" r="1.5" /><circle cx="15" cy="5" r="1.5" /><circle cx="5" cy="16" r="1.5" /><path d="M5 5.5v9m1.5-4.5h3A5.5 5.5 0 0 0 15 4.5" /></>,
    chevron: <path d="m7 4 6 6-6 6" />,
    refresh: <><path d="M15.5 6A6.5 6.5 0 1 0 16 13" /><path d="M15.5 2.5V6H12" /></>,
    shield: <path d="M10 2.5 16 5v4.5c0 3.8-2.4 6.5-6 8-3.6-1.5-6-4.2-6-8V5l6-2.5Z" />,
    arrow: <><path d="M3 10h13" /><path d="m12 6 4 4-4 4" /></>,
    check: <path d="m4 10 3.5 3.5L16 5" />,
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

function EnvironmentPill({ label, item }: { label: string; item: EnvironmentStatus["git"] }) {
  return (
    <span className={`environment-pill ${item.installed ? "is-ready" : "is-missing"}`}>
      <i />
      {label}
      {item.version ? <small>{item.version}</small> : null}
    </span>
  );
}

function Toggle({
  checked,
  title,
  description,
  detail,
  badges = [],
  onChange,
}: {
  checked: boolean;
  title: string;
  description: string;
  detail: string;
  badges?: string[];
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
          {badges.map((badge) => <i key={badge}>{badge}</i>)}
          <span className={`toggle-state ${checked ? "is-on" : ""}`}>{checked ? "已包含" : "未包含"}</span>
        </span>
        <small>{description}</small>
        <em>{detail}</em>
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
    <div className="untracked-picker">
      <div className="picker-heading">
        <span>{title}</span>
        <small>{selected.length} / {paths.length}</small>
      </div>
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
    </div>
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
  const [sessionDetailView, setSessionDetailView] = useState<SessionDetailView>("history");
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

  const showNotice = (message: string, tone = inferredNoticeTone(message)) => {
    setNotice({ message, tone });
    window.setTimeout(() => setNotice(null), 5200);
  };

  const load = async () => {
    setSnapshot(null);
    const result = await loadWorkspaceSnapshot();
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
  };

  useEffect(() => {
    void load();
  }, []);

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
    setSessionDetailView("history");
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
      if (!cancelled) setContentPreviewError(errorMessage(error));
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

  const selectedCount = Object.values(options).filter(Boolean).length;
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
      showNotice("当前是界面预览。请打开 Relay 桌面应用读取本机会话。", "warning");
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
      setContentPreviewError(errorMessage(error));
    } finally {
      setContentPreviewBusy(false);
    }
  };

  const preparePackage = async () => {
    if (!activeSession) return;
    if (snapshot?.source !== "native") {
      showNotice("当前是浏览器界面预览。请打开 Relay 桌面应用读取本机会话。", "warning");
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
    const repositoryTask = options.gitState
      ? inspectRepository(activeSession.cwd)
      : Promise.resolve<RepositoryInspection | null>(null);
    const [previewResult, repositoryResult] = await Promise.allSettled([
      previewTask,
      repositoryTask,
    ]);
    if (previewResult.status === "fulfilled") {
      setContentPreview(previewResult.value);
    } else {
      setContentPreviewError(errorMessage(previewResult.reason));
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
          "当前会话来自普通文件夹，未发现 Git 仓库。本次分享将不包含 Git 提交或文件改动，仍可继续生成会话分享包。",
        );
        showNotice("当前文件夹未使用 Git，已取消“Git 状态与变更”。", "warning");
      } else {
        setPackError(errorMessage(repositoryResult.reason));
      }
    }
    setPackBusy(false);
  };

  const generatePack = async () => {
    if (!activeSession || !packPath.trim()) return;
    if (contentPreviewError || !contentPreview) {
      setPackError("会话内容尚未成功读取，不能生成分享包。");
      return;
    }
    if (!contentPreview.preview_sha256?.trim()) {
      setPackError("会话预览缺少校验摘要，请重新检查可分享内容。");
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
        session_state: {
          objective: activeSession.title,
          summary: activeSession.preview,
          current_status: activeSession.preview,
          next_steps: [],
          tests: [],
          important_files: [],
          constraints: ["工具调用记录仅表示已经发生的操作，不得自动重新执行。"],
          open_questions: [],
        },
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
          setPackError(`会话已经更新，但重新读取失败：${errorMessage(refreshError)}`);
          return;
        }
      }
      const findings = sensitiveFindingsFromError(error);
      if (findings.length > 0) {
        setBackendSensitiveFindings(findings);
        setSensitiveAcknowledged(false);
        setPackError("后端在最终内容中发现了疑似敏感信息。这里只显示类型，不显示原文；请检查后再确认是否继续。");
      } else {
        setPackError(errorMessage(error));
      }
    } finally {
      setPackBusy(false);
    }
  };

  const copyKey = async () => {
    if (!packResult) return;
    try {
      await copyText(packResult.key_fragment);
      showNotice("解密密钥已复制。请通过另一个渠道发送，不要和 .relaypack 放在同一条消息里。", "warning");
    } catch {
      showNotice("复制失败，请手动选择解密密钥复制。", "error");
    }
  };

  if (!snapshot) {
    return (
      <main className="loading-screen">
        <div className="relay-mark large"><span>R</span><i /><b /></div>
        <p>正在读取本机会话</p>
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
          {snapshot.source === "demo" ? <span className="demo-flag">界面预览</span> : null}
          <EnvironmentPill label="Claude" item={snapshot.environment.claudeCode} />
          <EnvironmentPill label="ChatGPT" item={snapshot.environment.codex} />
          <button
            className="icon-button"
            type="button"
            onClick={() => {
              if (view === "shares") setHistoryVersion((value) => value + 1);
              else void load();
            }}
            aria-label={view === "shares" ? "重新读取分享记录" : "重新扫描"}
          >
            <Icon name="refresh" />
          </button>
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
          <div>
            <span className="eyebrow">本机会话</span>
            <h2>项目</h2>
          </div>
          <span className="count-chip">{projects.length}</span>
        </div>

        <div className="project-list">
          {projects.length === 0 ? (
            <div className="project-empty">
              <strong>{discoveryError ? "会话读取失败" : "还没有发现项目"}</strong>
              <p>{discoveryError ? "查看右侧错误后重新扫描。" : "Relay 会按 Git 项目整理 Claude Code 和 ChatGPT 会话。"}</p>
            </div>
          ) : projects.map((project, index) => {
            const active = project.id === activeProject?.id;
            const agents = new Set(project.sessions.map((session) => session.agent));
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
                <span className="project-index">{String(index + 1).padStart(2, "0")}</span>
                <span className="project-copy">
                  <strong title={project.name}>{project.name}</strong>
                  <small>{shortPath(project.path)}</small>
                </span>
                <span className="project-meta">
                  <b>{project.sessions.length}<small>会话</small></b>
                  <span className="agent-dots" aria-label={[...agents].map(agentName).join("、")}>
                    {agents.has("claude_code") ? <i className="claude" /> : null}
                    {agents.has("codex") ? <i className="codex" /> : null}
                  </span>
                </span>
              </button>
            );
          })}
        </div>

        <div className="privacy-note">
          <Icon name="shield" />
          <div>
            <strong>数据仅保存在本机</strong>
            <p>仅所选内容写入分享包。</p>
          </div>
        </div>
      </aside>

      <main className="workspace">
        <section className="session-column">
          <div className="workspace-heading">
            <div>
              <span className="eyebrow">当前项目</span>
              <h1 title={activeProject?.name ?? "项目会话"}>
                {activeProject?.name ?? "项目会话"}
              </h1>
            </div>
            <div className="workspace-count"><b>{sessions.length}</b><span>个会话</span></div>
          </div>

          {snapshot.issues.length > 0 ? (
            <section className={`workspace-issues ${loadErrors.length > 0 ? "has-error" : ""}`} aria-label="本机读取状态">
              <div className="workspace-issues-heading">
                <div>
                  <strong>{loadErrors.length > 0 ? "部分本机数据没有读取成功" : "有些会话未显示"}</strong>
                  <span>{loadErrors.length > 0 ? "已经读到的会话仍会保留。" : "其余会话已经正常读取。"}</span>
                </div>
                <button type="button" onClick={() => void load()}>重新扫描</button>
              </div>
              <div className="workspace-issue-list">
                {[...loadErrors, ...loadWarnings].map((issue, index) => (
                  <article key={`${issue.stage}-${issue.code}-${index}`}>
                    <span>{issue.severity === "error" ? "失败" : "提示"}</span>
                    <div>
                      <strong>{issueTitle(issue)}</strong>
                      <p>{issue.message}</p>
                      <code>{issueStageName(issue.stage)}</code>
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
                <span>00</span>
                <h3>{discoveryError ? "会话扫描失败" : projects.length === 0 ? "没有发现本机会话" : "没有匹配的会话"}</h3>
                <p>
                  {discoveryError
                    ? "上方显示了失败步骤和原因。处理后点“重新扫描”。"
                    : projects.length === 0
                      ? "请先在 Claude Code 或 ChatGPT 中打开项目并产生一条会话，然后重新扫描。"
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
                        <span><Icon name="branch" size={15} /> {session.workspace}</span>
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
                <span className={`agent-seal ${activeSession.agent}`}>{agentMark(activeSession.agent)}</span>
                <div>
                  <span className="eyebrow">已选会话</span>
                  <h2 title={activeSessionTitle}>{activeSessionTitle}</h2>
                  <p>工作目录 · {shortPath(activeSession.cwd)}</p>
                </div>
              </div>

              <div className="session-detail-tabs" role="tablist" aria-label="会话详情">
                <button
                  type="button"
                  role="tab"
                  aria-selected={sessionDetailView === "history"}
                  className={sessionDetailView === "history" ? "is-active" : ""}
                  onClick={() => setSessionDetailView("history")}
                >
                  聊天记录
                </button>
                <button
                  type="button"
                  role="tab"
                  aria-selected={sessionDetailView === "share"}
                  className={sessionDetailView === "share" ? "is-active" : ""}
                  onClick={() => setSessionDetailView("share")}
                >
                  创建分享包
                </button>
              </div>

              {sessionDetailView === "history" ? (
                <ConversationViewer
                  preview={activeContentPreview}
                  loading={contentPreviewBusy}
                  error={snapshot.source === "demo"
                    ? "当前是界面预览。请打开 Relay 桌面应用读取本机会话。"
                    : contentPreviewError}
                  onRetry={() => void retryConversation()}
                  onNotice={(message) => showNotice(message)}
                />
              ) : (
                <>
                  <div className="handoff-scroll-content">
                    <div className="share-mode">
                      <div>
                        <span className="eyebrow">导出设置</span>
                        <h3>选择分享包内容</h3>
                      </div>
                      <span className="mode-curated">已选择 {selectedCount} / 5 项</span>
                    </div>
                    <p className="share-mode-help">以下选项决定分享包包含的内容。下一步可查看并排除具体消息、工具调用和文件。</p>
                    <p className="share-package-explanation">
                      分享包是 Relay 生成的加密文件，用于把所选会话、项目说明和可选代码改动交给另一位 Relay 用户。它不是 ChatGPT 的网页分享链接，也不会复制原会话历史。
                    </p>

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

                    <div className="share-boundary-note">
                      <Icon name="shield" />
                      <p><strong>分享包中的命令和工具调用仅作为记录保存，接收时不会执行。</strong> 系统提示、模型私有推理和环境变量值不会导出。</p>
                    </div>

                    {activeSession.warnings.length > 0 ? (
                      <div className="parse-warning">
                        <span>解析提示</span>
                        {activeSession.warnings.map((warning) => <p key={warning}>{warning}</p>)}
                      </div>
                    ) : null}
                  </div>

                  <div className="handoff-action-bar">
                    <button className="primary-action" type="button" onClick={preparePackage}>
                      <span>检查并生成分享包</span>
                      <Icon name="arrow" />
                    </button>
                    <p className="action-caption">确认前不会生成或上传分享包。</p>
                  </div>
                </>
              )}
            </>
          ) : (
            <div className="empty-state detail-empty"><span>R</span><h3>选择一条会话</h3><p>Relay 会在这里显示准备分享的内容。</p></div>
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
                <span className="eyebrow">{packResult ? "生成完成" : "导出检查"}</span>
                <h2 id="pack-dialog-title">{packResult ? "生成并发送" : "检查要发出的内容"}</h2>
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
                <div className="success-emblem"><Icon name="check" size={25} /></div>
                <span className="eyebrow">本地加密包已就绪</span>
                <h3>加密包已经生成</h3>
                <p>{packResult.package_path}</p>
                <div className="result-grid">
                  <div><span>密文大小</span><strong>{formatBytes(packResult.ciphertext_bytes)}</strong></div>
                  <div><span>消息记录</span><strong>{packResult.preview.conversation_records}</strong></div>
                  <div><span>新文件</span><strong>{packResult.preview.untracked_file_count}</strong></div>
                </div>
                <CloudShareActions
                  pack={packResult}
                  onHistoryChanged={() => setHistoryVersion((value) => value + 1)}
                  onNotice={showNotice}
                />
                <div className="local-share-heading">
                  <span className="eyebrow">发送方式二 · 本地包</span>
                  <strong>单独发送文件和密钥</strong>
                </div>
                <label className="key-field">
                  <span>解密密钥</span>
                  <code>{packResult.key_fragment}</code>
                </label>
                <div className="key-warning">
                  <Icon name="shield" />
                  <p>密钥不会写进包里。发送本地包时，请通过另一个渠道发送密钥，不要把两者放在同一条消息里。</p>
                </div>
                <div className="dialog-actions">
                  <button type="button" className="secondary-action" onClick={() => setShowPackDialog(false)}>完成</button>
                  <button type="button" className="primary-action compact" onClick={() => void copyKey()}>
                    <span>复制密钥</span><Icon name="arrow" />
                  </button>
                </div>
              </div>
            ) : (
              <div className="dialog-content">
                <div className="dialog-session">
                  <span className={`agent-seal small ${activeSession.agent}`}>{agentMark(activeSession.agent)}</span>
                  <div><strong>{activeSessionTitle}</strong><small>{agentName(activeSession.agent)} · {activeSession.messageCount ?? "—"} 条消息</small></div>
                </div>

                {packBusy && !contentPreview && !repository ? (
                  <div className="dialog-loading"><i /><span>正在读取会话内容和 Git 变更</span></div>
                ) : null}

                {contentPreviewError ? (
                  <div className="dialog-error">
                    <Icon name="warning" />
                    <div>
                      <strong>会话内容暂时无法读取</strong>
                      <p>{contentPreviewError}</p>
                      <small>Relay 不会在无法逐项检查内容时继续生成分享包。</small>
                    </div>
                  </div>
                ) : null}

                {contentPreview ? (
                  <>
                    <ContentSelectionPanel
                      preview={contentPreview}
                      options={{
                        conversation: options.conversation,
                        toolEvidence: options.toolEvidence,
                        projectInstructions: options.projectInstructions,
                      }}
                      excludedMessageIds={excludedMessageIds}
                      excludedBlocks={excludedBlocks}
                      onExcludedMessageIdsChange={setExcludedMessageIds}
                      onExcludedBlocksChange={setExcludedBlocks}
                      onSensitiveCountChange={setSensitiveSelectedCount}
                      onSelectionSummaryChange={setContentSelectionSummary}
                    />
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
                    {repository.primary_remote ? <p className="remote-line">remote · {repository.primary_remote}</p> : null}

                    <div className="git-selection-note">
                      <Icon name="shield" size={15} />
                      <p><strong>暂存和未暂存改动已经默认选中。</strong> 新文件可能是构建产物或私密配置，Relay 默认不选，请在下面逐项确认。</p>
                    </div>

                    {repository.lfs.configured && repository.lfs.matching_path_count === 0 ? (
                      <div className="safety-list">
                        <span className="eyebrow">Git LFS</span>
                        <p>
                          <Icon name="check" size={14} />
                          <span>仓库有 LFS 规则，但当前没有文件命中，可以正常分享。</span>
                        </p>
                      </div>
                    ) : null}

                    {repository.warnings.length > 0 || repository.ignored_sensitive_files.length > 0 ? (
                      <div className="safety-list">
                        <span className="eyebrow">需要你确认</span>
                        {repository.warnings.map((warning) => (
                          <p key={warning.code}><Icon name="warning" size={14} /><span>{warning.message}</span></p>
                        ))}
                        {repository.ignored_sensitive_files.length > 0 ? (
                          <p><Icon name="shield" size={14} /><span>发现 {repository.ignored_sensitive_files.length} 个可能敏感的 ignored 文件；它们不会进入包。</span></p>
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
                    <strong>本次只分享会话与说明</strong>
                    <p>{gitUnavailableNotice}</p>
                  </div>
                ) : null}

                {packError ? (
                  <div className="dialog-error">
                    <Icon name="warning" />
                    <div><strong>分享包暂时不能生成</strong><p>{packError}</p><small>修改选择后可以再次尝试。</small></div>
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

                <label className="path-field">
                  <span>保存位置</span>
                  <input value={packPath} onChange={(event) => setPackPath(event.target.value)} spellCheck={false} />
                </label>

                <div className="dialog-note">
                  <Icon name="shield" />
                  <p>生成前不会上传内容。工具调用记录仅供查阅，接收时不会执行。</p>
                </div>

                <div className="dialog-final-summary">
                  <strong>本次将包含</strong>
                  <p>
                    {contentSelectionSummary?.messages ?? 0} 条会话消息、
                    {contentSelectionSummary?.toolBlocks ?? 0} 项工具调用记录、
                    {contentSelectionSummary?.instructionBlocks ?? 0} 段项目指令，
                    以及 {options.gitState ? selectedGitFiles : 0} 个 Git 变更文件。
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
                    <span>{packBusy ? "正在生成" : "确认内容并生成本地加密包"}</span>
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
