use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::fmt;

#[derive(Debug, Clone, Serialize)]
pub struct CommandError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl CommandError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CommandError {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AgentProvider {
    ClaudeCode,
    Codex,
    #[default]
    Unknown,
}

impl AgentProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
            Self::Unknown => "unknown",
        }
    }
}

impl Serialize for AgentProvider {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentProvider {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "claude" | "claude_code" | "claude-code" => Self::ClaudeCode,
            "codex" | "openai_codex" => Self::Codex,
            _ => Self::Unknown,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentStatus {
    pub platform: String,
    pub architecture: String,
    pub tools: EnvironmentTools,
    pub homes: AgentHomes,
    pub adapter: AdapterExecutableStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentTools {
    pub git: ToolStatus,
    pub claude: ToolStatus,
    pub codex: ToolStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolStatus {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentHomes {
    pub claude: PathStatus,
    pub codex: PathStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct PathStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub exists: bool,
    pub is_directory: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdapterExecutableStatus {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdapterHealth {
    pub executable_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(default)]
    pub supported_methods: Vec<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub details: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DiscoverSessionsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentProvider>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_home: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PreviewSessionRequest {
    pub agent: AgentProvider,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_home: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiscoverSessionsResult {
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub scanned_at: Option<String>,
    #[serde(default)]
    pub sessions: Vec<SessionSummary>,
    #[serde(default)]
    pub warnings: Vec<AdapterWarning>,
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionSummary {
    #[serde(default, rename = "agent", alias = "provider", alias = "source")]
    pub provider: AgentProvider,
    #[serde(default, alias = "id")]
    pub session_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, alias = "project_path", alias = "workspace_path")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub project_key: Option<String>,
    #[serde(default)]
    pub project_name: Option<String>,
    #[serde(default)]
    pub project_root: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default, alias = "adapter_version")]
    pub native_version: Option<String>,
    #[serde(default, alias = "session_path")]
    pub source_path: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub message_count: Option<u64>,
    #[serde(default)]
    pub tool_call_count: Option<u64>,
    #[serde(default)]
    pub tool_result_count: Option<u64>,
    #[serde(default)]
    pub warning_count: Option<u64>,
    #[serde(default)]
    pub completeness: Option<String>,
    #[serde(default)]
    pub preview: Option<String>,
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdapterWarning {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_type: Option<String>,
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepositoryInspection {
    pub requested_path: String,
    pub root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub detached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_remote: Option<String>,
    pub remotes: Vec<GitRemote>,
    pub staged: Vec<GitFileChange>,
    pub unstaged: Vec<GitFileChange>,
    pub untracked: Vec<String>,
    pub operation: GitOperationState,
    pub submodules: Vec<GitSubmodule>,
    pub lfs: GitLfsStatus,
    pub ignored_sensitive_files: Vec<String>,
    pub warnings: Vec<RepositoryWarning>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GitRemote {
    pub name: String,
    pub url: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GitFileChange {
    pub path: String,
    pub status: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submodule: Option<GitSubmoduleWorktreeState>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GitSubmoduleWorktreeState {
    pub commit_changed: bool,
    pub tracked_changes: bool,
    pub untracked_changes: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitOperationState {
    pub merge: bool,
    pub rebase: bool,
    pub cherry_pick: bool,
    pub revert: bool,
    pub bisect: bool,
}

impl GitOperationState {
    pub fn any(&self) -> bool {
        self.merge || self.rebase || self.cherry_pick || self.revert || self.bisect
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GitSubmodule {
    pub path: String,
    pub commit: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitLfsStatus {
    pub status: String,
    pub available: bool,
    pub configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracked_file_count: Option<usize>,
    pub matching_path_count: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RepositoryWarning {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExportRelaypackRequest {
    pub agent: AgentProvider,
    pub session_id: String,
    pub preview_sha256: String,
    pub output_path: String,
    #[serde(default)]
    pub claude_home: Option<String>,
    #[serde(default)]
    pub codex_home: Option<String>,
    #[serde(default)]
    pub repository_path: Option<String>,
    #[serde(default)]
    pub include_git: bool,
    #[serde(default)]
    pub include_local_commits: Option<bool>,
    #[serde(default)]
    pub include_staged: Option<bool>,
    #[serde(default)]
    pub include_unstaged: Option<bool>,
    #[serde(default)]
    pub selected_staged: Vec<String>,
    #[serde(default)]
    pub selected_unstaged: Vec<String>,
    #[serde(default)]
    pub selected_untracked: Vec<String>,
    #[serde(default)]
    pub excluded_message_ids: Vec<String>,
    #[serde(default)]
    pub excluded_blocks: Vec<ExcludedContentBlock>,
    #[serde(default)]
    pub allow_sensitive_content: bool,
    #[serde(default)]
    pub session_state: Option<SessionStateInput>,
    #[serde(default = "default_true")]
    pub include_conversation: bool,
    #[serde(default = "default_true")]
    pub include_tool_evidence: bool,
    #[serde(default = "default_true")]
    pub include_project_instructions: bool,
    #[serde(default = "default_true")]
    pub include_environment: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct ExcludedContentBlock {
    pub message_id: String,
    pub block_index: usize,
}

fn default_true() -> bool {
    true
}

impl ExportRelaypackRequest {
    pub fn wants_local_commits(&self) -> bool {
        self.include_local_commits.unwrap_or(true)
    }

    pub fn wants_staged(&self) -> bool {
        self.include_staged.unwrap_or(true)
    }

    pub fn wants_unstaged(&self) -> bool {
        self.include_unstaged.unwrap_or(true)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionStateInput {
    #[serde(default)]
    pub objective: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub current_status: Option<String>,
    #[serde(default)]
    pub next_steps: Vec<SessionNextStepInput>,
    #[serde(default)]
    pub tests: Vec<SessionTestInput>,
    #[serde(default)]
    pub important_files: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub open_questions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionNextStepInput {
    pub text: String,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionTestInput {
    pub name: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportRelaypackResult {
    pub package_path: String,
    pub key_fragment: String,
    pub ciphertext_sha256: String,
    pub ciphertext_bytes: u64,
    pub preview: RelaypackPreview,
    pub warnings: Vec<RelaypackDiagnosticPreview>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InspectRelaypackResult {
    pub package_path: String,
    pub ciphertext_sha256: String,
    pub ciphertext_bytes: u64,
    pub preview: RelaypackPreview,
    pub content_preview: Value,
    pub warnings: Vec<RelaypackDiagnosticPreview>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelaypackPreview {
    pub package_id: String,
    pub created_at: String,
    pub source_agent: AgentProvider,
    pub session_id: String,
    pub title: String,
    pub project_name: String,
    pub git_included: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub conversation_records: usize,
    pub importable_session: bool,
    pub asset_count: usize,
    pub untracked_file_count: usize,
    pub diagnostics: Vec<RelaypackDiagnosticPreview>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelaypackDiagnosticPreview {
    pub code: String,
    pub severity: String,
    pub scope: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RestoreRelaypackRequest {
    pub package_path: String,
    pub key: String,
    pub repository_path: Option<String>,
    pub target_path: String,
    pub branch_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreRelaypackResult {
    pub worktree_path: String,
    pub branch_name: Option<String>,
    pub head: Option<String>,
    pub handoff_directory: String,
    pub handoff_markdown_path: String,
    pub handoff_json_path: String,
    pub staged_applied: bool,
    pub unstaged_applied: bool,
    pub untracked_files_restored: usize,
    pub preview: RelaypackPreview,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImportNativeSessionRequest {
    pub agent: AgentProvider,
    pub worktree_path: String,
    pub handoff_json_path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImportNativeSessionResult {
    pub status: String,
    pub target: String,
    pub session_id: String,
    pub title: String,
    pub target_home: String,
    pub target_cwd: String,
    pub session_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_dir: Option<String>,
    pub writes: Vec<String>,
    #[serde(default)]
    pub created_files: Vec<String>,
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub continue_command: String,
    pub verification: NativeImportVerification,
    #[serde(default)]
    pub open_status: String,
    #[serde(default)]
    pub catalog_refresh_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_refresh_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_refresh_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NativeImportVerification {
    pub session_file: bool,
    pub index: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UploadShareRequest {
    pub package_path: String,
    pub key: String,
    pub service_base_url: String,
    #[serde(default)]
    pub project_title: Option<String>,
    #[serde(default)]
    pub project_name: Option<String>,
    #[serde(default)]
    pub expires_in_seconds: Option<u64>,
    #[serde(default)]
    pub upload_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UploadShareResult {
    pub share_id: String,
    pub share_url: String,
    pub expires_at: String,
    #[serde(skip_serializing)]
    pub(crate) revoke_token: String,
    #[serde(skip_serializing)]
    pub(crate) upload_token: String,
    pub ciphertext_sha256: String,
    pub ciphertext_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareHistoryStatus {
    PendingUpload,
    Active,
    Revoked,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShareHistoryRecord {
    pub share_id: String,
    pub share_url: String,
    pub service_base_url: String,
    pub created_at: String,
    pub expires_at: String,
    pub package_path: String,
    pub package_exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    pub ciphertext_sha256: String,
    pub ciphertext_bytes: u64,
    pub status: ShareHistoryStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListShareHistoryResult {
    pub records: Vec<ShareHistoryRecord>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RevokeSavedShareRequest {
    pub share_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RevokeSavedShareResult {
    pub record: ShareHistoryRecord,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResumeSavedShareUploadRequest {
    pub share_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResumeSavedShareUploadResult {
    pub record: ShareHistoryRecord,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DownloadShareRequest {
    pub share_url: String,
    pub service_base_url: String,
    pub output_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadShareResult {
    pub package_path: String,
    pub key: String,
    pub share_id: String,
    pub ciphertext_sha256: String,
    pub ciphertext_bytes: u64,
    pub preview: RelaypackPreview,
    pub content_preview: Value,
    pub warnings: Vec<RelaypackDiagnosticPreview>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RevokeShareRequest {
    pub share_id: String,
    pub revoke_token: String,
    pub service_base_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RevokeShareResult {
    pub share_id: String,
    pub revoked: bool,
}
