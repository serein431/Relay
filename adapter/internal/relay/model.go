package relay

import "time"

const (
	ProtocolSchema = "relay.adapter.v1"
	// PreviewSchema is the adapter's local, conversation-only export. It is not
	// the encrypted/shareable Relay package assembled by the desktop core.
	PreviewSchema  = "relay.adapter.handoff-preview.v1"
	AdapterVersion = "0.1.2"

	AgentClaude = "claude_code"
	AgentCodex  = "codex"
)

// Warning describes data that could not be represented faithfully. It never
// contains the raw record, because unknown records can contain provider-only or
// encrypted data.
type Warning struct {
	Code       string `json:"code"`
	Message    string `json:"message"`
	Line       int    `json:"line,omitempty"`
	RecordType string `json:"record_type,omitempty"`
}

type Completeness struct {
	Status             string `json:"status"`
	TotalLines         int    `json:"total_lines"`
	ParsedLines        int    `json:"parsed_lines"`
	DamagedLines       int    `json:"damaged_lines"`
	UnknownRecords     int    `json:"unknown_records"`
	HiddenRecords      int    `json:"hidden_records"`
	UnsupportedBlocks  int    `json:"unsupported_blocks"`
	OrphanToolResults  int    `json:"orphan_tool_results"`
	UnmatchedToolCalls int    `json:"unmatched_tool_calls"`
}

type SessionSummary struct {
	Agent           string `json:"agent"`
	SessionID       string `json:"session_id"`
	Title           string `json:"title"`
	Preview         string `json:"preview,omitempty"`
	CWD             string `json:"cwd,omitempty"`
	ProjectKey      string `json:"project_key"`
	ProjectName     string `json:"project_name"`
	ProjectRoot     string `json:"project_root,omitempty"`
	CreatedAt       string `json:"created_at,omitempty"`
	UpdatedAt       string `json:"updated_at,omitempty"`
	NativeVersion   string `json:"native_version,omitempty"`
	SourcePath      string `json:"source_path"`
	SizeBytes       int64  `json:"size_bytes"`
	MessageCount    int    `json:"message_count"`
	ToolCallCount   int    `json:"tool_call_count"`
	ToolResultCount int    `json:"tool_result_count"`
	WarningCount    int    `json:"warning_count"`
	Completeness    string `json:"completeness"`
}

type BlockMapping struct {
	Status     string `json:"status"`
	SourceType string `json:"source_type,omitempty"`
}

type Block struct {
	Kind           string        `json:"kind"`
	Classification string        `json:"classification"`
	Mapping        *BlockMapping `json:"mapping,omitempty"`
	SafeSummary    string        `json:"safe_summary,omitempty"`
	Text           string        `json:"text,omitempty"`
	CallID         string        `json:"call_id,omitempty"`
	Name           string        `json:"name,omitempty"`
	Status         string        `json:"status,omitempty"`
	Input          any           `json:"input,omitempty"`
	Output         any           `json:"output,omitempty"`
	IsError        *bool         `json:"is_error,omitempty"`
	NativeType     string        `json:"native_type,omitempty"`
	ReplayPolicy   string        `json:"replay_policy,omitempty"`
	Source         any           `json:"source,omitempty"`
}

type Message struct {
	ID        string  `json:"id"`
	ParentID  string  `json:"parent_id,omitempty"`
	TurnID    string  `json:"turn_id,omitempty"`
	BranchID  string  `json:"branch_id,omitempty"`
	Timestamp string  `json:"timestamp,omitempty"`
	Role      string  `json:"role"`
	Phase     string  `json:"phase,omitempty"`
	Blocks    []Block `json:"blocks"`
}

type ParsedSession struct {
	Summary      SessionSummary
	Messages     []Message
	Warnings     []Warning
	Completeness Completeness
}

type DiscoverResult struct {
	Schema    string           `json:"schema"`
	ScannedAt string           `json:"scanned_at"`
	Sessions  []SessionSummary `json:"sessions"`
	Warnings  []Warning        `json:"warnings"`
}

type InspectResult struct {
	Schema       string         `json:"schema"`
	Session      SessionSummary `json:"session"`
	Preview      []Message      `json:"preview"`
	Warnings     []Warning      `json:"warnings"`
	Completeness Completeness   `json:"completeness"`
}

type Handoff struct {
	Schema       string              `json:"schema"`
	ExportedAt   string              `json:"exported_at"`
	Source       HandoffSource       `json:"source"`
	Session      HandoffSession      `json:"session"`
	Environment  HandoffEnvironment  `json:"environment"`
	Project      HandoffProject      `json:"project"`
	Conversation HandoffConversation `json:"conversation"`
	Assets       []any               `json:"assets"`
	Diagnostics  HandoffDiagnostics  `json:"diagnostics"`
	Export       HandoffExport       `json:"export"`
}

type HandoffSource struct {
	Agent      string `json:"agent"`
	SessionID  string `json:"session_id"`
	SourcePath string `json:"source_path"`
	ReadOnly   bool   `json:"read_only"`
}

type HandoffSession struct {
	Title         string `json:"title"`
	CreatedAt     string `json:"created_at,omitempty"`
	UpdatedAt     string `json:"updated_at,omitempty"`
	NativeVersion string `json:"native_version,omitempty"`
}

type HandoffEnvironment struct {
	CWD string `json:"cwd,omitempty"`
}

type HandoffProject struct {
	Key  string `json:"key"`
	Name string `json:"name"`
	Path string `json:"path,omitempty"`
}

type HandoffConversation struct {
	Messages []Message `json:"messages"`
}

type HandoffDiagnostics struct {
	Warnings     []Warning    `json:"warnings"`
	Completeness Completeness `json:"completeness"`
}

type HandoffExport struct {
	AdapterVersion string `json:"adapter_version"`
	Protocol       string `json:"protocol"`
	NativeHistory  bool   `json:"native_history"`
}

func (p ParsedSession) Inspect(previewLimit int) InspectResult {
	preview := p.Messages
	if previewLimit > 0 && len(preview) > previewLimit {
		// The most recent records are more useful in a handoff inspection.
		preview = preview[len(preview)-previewLimit:]
	}
	return InspectResult{
		Schema:       ProtocolSchema,
		Session:      p.Summary,
		Preview:      preview,
		Warnings:     p.Warnings,
		Completeness: p.Completeness,
	}
}

func (p ParsedSession) Handoff(now time.Time) Handoff {
	return Handoff{
		Schema:     PreviewSchema,
		ExportedAt: now.UTC().Format(time.RFC3339Nano),
		Source: HandoffSource{
			Agent:      p.Summary.Agent,
			SessionID:  p.Summary.SessionID,
			SourcePath: p.Summary.SourcePath,
			ReadOnly:   true,
		},
		Session: HandoffSession{
			Title:         p.Summary.Title,
			CreatedAt:     p.Summary.CreatedAt,
			UpdatedAt:     p.Summary.UpdatedAt,
			NativeVersion: p.Summary.NativeVersion,
		},
		Environment: HandoffEnvironment{CWD: p.Summary.CWD},
		Project: HandoffProject{
			Key:  p.Summary.ProjectKey,
			Name: p.Summary.ProjectName,
			Path: p.Summary.CWD,
		},
		Conversation: HandoffConversation{Messages: p.Messages},
		Assets:       []any{},
		Diagnostics: HandoffDiagnostics{
			Warnings:     p.Warnings,
			Completeness: p.Completeness,
		},
		Export: HandoffExport{
			AdapterVersion: AdapterVersion,
			Protocol:       ProtocolSchema,
			NativeHistory:  false,
		},
	}
}
