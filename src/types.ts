export type AgentKind = "claude_code" | "codex";

export type SessionHealth = "complete" | "partial" | "unsupported";

export interface SessionSummary {
  id: string;
  agent: AgentKind;
  title: string;
  projectKey?: string;
  projectName: string;
  projectRoot?: string;
  workspace: string;
  cwd: string;
  createdAt?: string;
  updatedAt?: string;
  preview?: string;
  sourcePath?: string;
  messageCount?: number;
  health: SessionHealth;
  warnings: string[];
}

export interface ProjectGroup {
  id: string;
  name: string;
  path: string;
  remote?: string;
  sessions: SessionSummary[];
}

export interface AgentInstallation {
  installed: boolean;
  version?: string;
  path?: string;
}

export interface EnvironmentStatus {
  git: AgentInstallation;
  claudeCode: AgentInstallation;
  codex: AgentInstallation;
  adapter: AgentInstallation;
  home?: string;
}

export interface GitFileChange {
  path: string;
  status: string;
  kind: string;
  original_path?: string;
}

export interface RepositoryWarning {
  code: string;
  message: string;
}

export interface GitLfsStatus {
  status: string;
  available: boolean;
  configured: boolean;
  version?: string;
  tracked_file_count?: number;
  matching_path_count: number;
}

export interface RepositoryInspection {
  requested_path: string;
  root: string;
  branch?: string;
  head?: string;
  primary_remote?: string;
  staged: GitFileChange[];
  unstaged: GitFileChange[];
  untracked: string[];
  lfs: GitLfsStatus;
  ignored_sensitive_files: string[];
  warnings: RepositoryWarning[];
}

export interface RelaypackDiagnostic {
  code: string;
  severity: string;
  scope: string;
  message: string;
}

export interface RelaypackPreview {
  package_id: string;
  created_at: string;
  source_agent: AgentKind;
  session_id: string;
  title: string;
  project_name: string;
  git_included: boolean;
  branch?: string;
  head?: string;
  conversation_records: number;
  asset_count: number;
  untracked_file_count: number;
  diagnostics: RelaypackDiagnostic[];
}

export interface AdapterPreviewBlock {
  kind: string;
  classification: string;
  text?: string;
  call_id?: string;
  name?: string;
  status?: string;
  input?: unknown;
  output?: unknown;
  is_error?: boolean;
  native_type?: string;
  replay_policy?: string;
  source?: unknown;
}

export interface AdapterPreviewMessage {
  id: string;
  parent_id?: string;
  turn_id?: string;
  branch_id?: string;
  timestamp?: string;
  role: string;
  phase?: string;
  blocks: AdapterPreviewBlock[];
}

export interface SessionContentPreview {
  schema: "relay.adapter.handoff-preview.v1";
  preview_sha256: string;
  source: {
    agent: AgentKind;
    session_id: string;
    read_only: boolean;
  };
  session: {
    title: string;
    created_at?: string;
    updated_at?: string;
  };
  conversation: {
    messages: AdapterPreviewMessage[];
  };
  diagnostics: {
    warnings: Array<{ code: string; message: string; line?: number; record_type?: string }>;
    completeness: Record<string, unknown>;
  };
}

export interface PreviewSessionRequest {
  agent: AgentKind;
  session_id: string;
}

export interface ExcludedContentBlock {
  message_id: string;
  block_index: number;
}

export interface ExportRelaypackRequest {
  agent: AgentKind;
  session_id: string;
  preview_sha256: string;
  output_path: string;
  repository_path?: string;
  include_conversation: boolean;
  include_tool_evidence: boolean;
  include_project_instructions: boolean;
  include_environment: boolean;
  include_git: boolean;
  include_local_commits: boolean;
  include_staged: boolean;
  include_unstaged: boolean;
  selected_staged: string[];
  selected_unstaged: string[];
  selected_untracked: string[];
  excluded_message_ids: string[];
  excluded_blocks: ExcludedContentBlock[];
  allow_sensitive_content: boolean;
  session_state?: {
    objective?: string;
    summary?: string;
    current_status?: string;
    next_steps?: Array<{ text: string; status?: string }>;
    tests?: Array<{ name: string; command?: string; status?: string; note?: string }>;
    important_files?: string[];
    constraints?: string[];
    open_questions?: string[];
  };
}

export interface ExportRelaypackResult {
  package_path: string;
  key_fragment: string;
  ciphertext_sha256: string;
  ciphertext_bytes: number;
  preview: RelaypackPreview;
  warnings: RelaypackDiagnostic[];
}

export interface InspectRelaypackResult {
  package_path: string;
  ciphertext_sha256: string;
  ciphertext_bytes: number;
  preview: RelaypackPreview;
  warnings: RelaypackDiagnostic[];
}

export interface RestoreRelaypackRequest {
  package_path: string;
  key: string;
  repository_path?: string;
  target_path: string;
  branch_name?: string;
}

export interface RestoreRelaypackResult {
  worktree_path: string;
  branch_name: string | null;
  head: string | null;
  handoff_directory: string;
  handoff_markdown_path: string;
  handoff_json_path: string;
  staged_applied: boolean;
  unstaged_applied: boolean;
  untracked_files_restored: number;
  preview: RelaypackPreview;
}

export interface UploadShareRequest {
  package_path: string;
  key: string;
  service_base_url: string;
  project_title?: string;
  project_name?: string;
  expires_in_seconds?: number;
  upload_token?: string;
}

export interface UploadShareResult {
  share_id: string;
  share_url: string;
  expires_at: string;
  ciphertext_sha256: string;
  ciphertext_bytes: number;
}

export type ShareHistoryStatus = "pending_upload" | "active" | "revoked";

export interface ShareHistoryRecord {
  share_id: string;
  share_url: string;
  service_base_url: string;
  created_at: string;
  expires_at: string;
  package_path: string;
  package_exists: boolean;
  project_title?: string;
  project_name?: string;
  ciphertext_sha256: string;
  ciphertext_bytes: number;
  status: ShareHistoryStatus;
  revoked_at?: string;
}

export interface ListShareHistoryResult {
  records: ShareHistoryRecord[];
}

export interface RevokeSavedShareRequest {
  share_id: string;
}

export interface RevokeSavedShareResult {
  record: ShareHistoryRecord;
}

export interface ResumeSavedShareUploadRequest {
  share_id: string;
}

export interface ResumeSavedShareUploadResult {
  record: ShareHistoryRecord;
}

export interface DownloadShareRequest {
  share_url: string;
  service_base_url: string;
  output_path: string;
}

export interface DownloadShareResult extends InspectRelaypackResult {
  key: string;
  share_id: string;
}

export interface LaunchAgentRequest {
  agent: AgentKind;
  worktree_path: string;
  handoff_markdown_path: string;
}

export interface LaunchAgentResult {
  agent: AgentKind;
  worktree_path: string;
  executable_path: string;
  process_id: number;
  launch_mode: "background" | "deep_link";
  startup_prompt: string;
  verification_status: "VERIFIED" | "OPEN_REQUESTED" | "UNVERIFIED";
  session_id?: string;
  session_state?: string;
  waiting_reason?: string;
}

export interface WorkspaceSnapshot {
  environment: EnvironmentStatus;
  sessions: SessionSummary[];
  source: "native" | "demo";
  issues: WorkspaceLoadIssue[];
}

export type WorkspaceLoadStage =
  | "environment_status"
  | "adapter_health"
  | "discover_sessions";

export interface WorkspaceLoadIssue {
  stage: WorkspaceLoadStage;
  code: string;
  message: string;
  severity: "warning" | "error";
}
