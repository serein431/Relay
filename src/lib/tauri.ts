import { invoke, isTauri } from "@tauri-apps/api/core";
import { demoEnvironment, demoSessions } from "../mock";
import type {
  AgentInstallation,
  AgentKind,
  EnvironmentStatus,
  DownloadShareRequest,
  DownloadShareResult,
  ExportRelaypackRequest,
  ExportRelaypackResult,
  InspectRelaypackResult,
  LaunchAgentRequest,
  LaunchAgentResult,
  ListShareHistoryResult,
  PreviewSessionRequest,
  RepositoryInspection,
  RestoreRelaypackRequest,
  RestoreRelaypackResult,
  ResumeSavedShareUploadRequest,
  ResumeSavedShareUploadResult,
  RevokeSavedShareRequest,
  RevokeSavedShareResult,
  SessionHealth,
  SessionContentPreview,
  SessionSummary,
  UploadShareRequest,
  UploadShareResult,
  WorkspaceLoadIssue,
  WorkspaceLoadStage,
  WorkspaceSnapshot,
} from "../types";

type UnknownRecord = Record<string, unknown>;

export type WorkspaceRuntime = {
  isTauri: () => boolean;
  invoke: (command: string, args?: Record<string, unknown>) => Promise<unknown>;
};

const unavailableEnvironment: EnvironmentStatus = {
  git: { installed: false },
  claudeCode: { installed: false },
  codex: { installed: false },
  adapter: { installed: false },
};

const defaultWorkspaceRuntime: WorkspaceRuntime = {
  isTauri,
  invoke: (command, args) => invoke<unknown>(command, args),
};

function isRecord(value: unknown): value is UnknownRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function asBoolean(value: unknown): boolean | undefined {
  return typeof value === "boolean" ? value : undefined;
}

function installation(value: unknown): AgentInstallation {
  if (typeof value === "boolean") return { installed: value };
  if (!isRecord(value)) return { installed: false };
  return {
    installed:
      asBoolean(value.installed) ??
      asBoolean(value.available) ??
      Boolean(asString(value.path) || asString(value.version)),
    version: asString(value.version),
    path: asString(value.path),
  };
}

function normalizeEnvironment(value: unknown): EnvironmentStatus {
  if (!isRecord(value)) return unavailableEnvironment;
  const tools = isRecord(value.tools) ? value.tools : value;
  const homes = isRecord(value.homes) ? value.homes : undefined;
  const homePath = [homes?.claude, homes?.codex]
    .map((entry) => (isRecord(entry) ? asString(entry.path) : undefined))
    .find(Boolean);
  const home = homePath?.replace(/\/(?:\.claude|\.codex)\/?$/, "");
  return {
    git: installation(tools.git),
    claudeCode: installation(tools.claude_code ?? tools.claudeCode ?? tools.claude),
    codex: installation(tools.codex),
    adapter: installation(value.adapter ?? value.agent_adapter),
    home,
  };
}

function environmentWithAdapterHealth(
  environment: EnvironmentStatus,
  health: unknown,
): EnvironmentStatus {
  if (!isRecord(health)) return environment;
  return {
    ...environment,
    adapter: {
      installed: true,
      path: asString(health.executable_path ?? health.executablePath) ?? environment.adapter.path,
      version: asString(health.version) ?? environment.adapter.version,
    },
  };
}

function normalizeAgent(value: unknown): AgentKind {
  return String(value).toLowerCase().includes("claude") ? "claude_code" : "codex";
}

function normalizeHealth(value: unknown, warnings: string[]): SessionHealth {
  if (value === "unsupported") return "unsupported";
  if (value === "partial" || warnings.length > 0) return "partial";
  return "complete";
}

function baseName(path: string): string {
  const normalized = path.replace(/\\/g, "/").replace(/\/$/, "");
  return normalized.split("/").filter(Boolean).at(-1) ?? "未归类项目";
}

function normalizeSession(value: unknown, index: number): SessionSummary | null {
  if (!isRecord(value)) return null;
  const cwd = asString(value.cwd ?? value.project_dir ?? value.projectDir) ?? "";
  const warningValue = value.warnings;
  const warnings = Array.isArray(warningValue)
    ? warningValue.filter((item): item is string => typeof item === "string")
    : [];
  const id = asString(value.id ?? value.session_id ?? value.sessionId) ?? `session-${index}`;
  const projectName =
    asString(value.project_name ?? value.projectName ?? value.repository_name) ?? baseName(cwd);
  return {
    id,
    agent: normalizeAgent(value.agent ?? value.provider ?? value.source_agent),
    title: asString(value.title ?? value.first_prompt ?? value.firstPrompt) ?? "未命名会话",
    projectKey: asString(value.project_key ?? value.projectKey),
    projectName,
    projectRoot: asString(value.project_root ?? value.projectRoot),
    workspace:
      asString(value.workspace ?? value.worktree ?? value.branch) ?? "main",
    cwd,
    createdAt: asString(value.created_at ?? value.createdAt),
    updatedAt: asString(value.updated_at ?? value.updatedAt ?? value.modified_at),
    preview: asString(value.preview ?? value.summary ?? value.last_message),
    sourcePath: asString(value.source_path ?? value.sourcePath ?? value.path),
    messageCount:
      typeof (value.message_count ?? value.messageCount) === "number"
        ? Number(value.message_count ?? value.messageCount)
        : undefined,
    health: normalizeHealth(value.health ?? value.status ?? value.completeness, warnings),
    warnings,
  };
}

function normalizeSessions(value: unknown): SessionSummary[] {
  const source = Array.isArray(value)
    ? value
    : isRecord(value) && Array.isArray(value.sessions)
      ? value.sessions
      : [];
  return source
    .map((item, index) => normalizeSession(item, index))
    .filter((item): item is SessionSummary => item !== null);
}

function issueMessage(error: unknown): string {
  if (error instanceof Error && error.message.trim()) return error.message.trim();
  if (typeof error === "string" && error.trim()) return error.trim();
  if (isRecord(error)) {
    const message = asString(error.message ?? error.error ?? error.reason);
    if (message) return message;
  }
  return "本机调用失败，但没有返回可读的错误说明。";
}

function invokeIssue(stage: WorkspaceLoadStage, error: unknown): WorkspaceLoadIssue {
  return {
    stage,
    code: isRecord(error) ? asString(error.code) ?? "native_command_failed" : "native_command_failed",
    message: issueMessage(error),
    severity: "error",
  };
}

function discoveryIssues(value: unknown): WorkspaceLoadIssue[] {
  if (!isRecord(value) || !Array.isArray(value.warnings)) return [];
  return value.warnings.flatMap((warning) => {
    if (!isRecord(warning)) return [];
    const code = asString(warning.code) ?? "session_scan_warning";
    const message = code === "session_too_large"
      ? "有一条会话文件超过 256 MB。Relay 为避免占用过多内存没有显示它，其他会话不受影响。"
      : asString(warning.message) ?? "部分本机会话没有成功读取。";
    return [{
      stage: "discover_sessions" as const,
      code,
      message,
      severity: "warning" as const,
    }];
  });
}

export async function loadWorkspaceSnapshot(
  runtime: WorkspaceRuntime = defaultWorkspaceRuntime,
): Promise<WorkspaceSnapshot> {
  if (!runtime.isTauri()) {
    return {
      environment: demoEnvironment,
      sessions: demoSessions,
      source: "demo",
      issues: [],
    };
  }

  const [environmentResult, healthResult, sessionsResult] = await Promise.allSettled([
    runtime.invoke("environment_status"),
    runtime.invoke("adapter_health"),
    runtime.invoke("discover_sessions", { request: { limit: 250 } }),
  ]);

  const issues: WorkspaceLoadIssue[] = [];
  let environment = unavailableEnvironment;
  if (environmentResult.status === "fulfilled") {
    environment = normalizeEnvironment(environmentResult.value);
  } else {
    issues.push(invokeIssue("environment_status", environmentResult.reason));
  }

  if (healthResult.status === "fulfilled") {
    environment = environmentWithAdapterHealth(environment, healthResult.value);
  } else {
    issues.push(invokeIssue("adapter_health", healthResult.reason));
  }

  let sessions: SessionSummary[] = [];
  if (sessionsResult.status === "fulfilled") {
    sessions = normalizeSessions(sessionsResult.value);
    issues.push(...discoveryIssues(sessionsResult.value));
  } else {
    issues.push(invokeIssue("discover_sessions", sessionsResult.reason));
  }

  return {
    environment,
    sessions,
    source: "native",
    issues,
  };
}

export async function inspectRepository(path: string): Promise<RepositoryInspection> {
  return invoke<RepositoryInspection>("inspect_repository", { path });
}

export async function previewSession(
  request: PreviewSessionRequest,
): Promise<SessionContentPreview> {
  return invoke<SessionContentPreview>("preview_session", { request });
}

export async function exportRelaypack(
  request: ExportRelaypackRequest,
): Promise<ExportRelaypackResult> {
  return invoke<ExportRelaypackResult>("export_relaypack", { request });
}

export async function inspectRelaypack(
  path: string,
  key: string,
): Promise<InspectRelaypackResult> {
  return invoke<InspectRelaypackResult>("inspect_relaypack", { path, key });
}

export async function restoreRelaypack(
  request: RestoreRelaypackRequest,
): Promise<RestoreRelaypackResult> {
  return invoke<RestoreRelaypackResult>("restore_relaypack", { request });
}

export async function uploadShare(request: UploadShareRequest): Promise<UploadShareResult> {
  return invoke<UploadShareResult>("upload_share", { request });
}

export async function listShareHistory(): Promise<ListShareHistoryResult> {
  if (!isTauri()) return { records: [] };
  return invoke<ListShareHistoryResult>("list_share_history");
}

export async function revokeSavedShare(
  request: RevokeSavedShareRequest,
): Promise<RevokeSavedShareResult> {
  return invoke<RevokeSavedShareResult>("revoke_saved_share", { request });
}

export async function resumeSavedShareUpload(
  request: ResumeSavedShareUploadRequest,
): Promise<ResumeSavedShareUploadResult> {
  return invoke<ResumeSavedShareUploadResult>("resume_saved_share_upload", { request });
}

export async function downloadShare(
  request: DownloadShareRequest,
): Promise<DownloadShareResult> {
  return invoke<DownloadShareResult>("download_share", { request });
}

export async function launchAgent(request: LaunchAgentRequest): Promise<LaunchAgentResult> {
  return invoke<LaunchAgentResult>("launch_agent", { request });
}
