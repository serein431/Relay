package nativeimport

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"
)

const maxHandoffBytes = 1024 << 20

type Request struct {
	HandoffPath string `json:"handoff_path"`
	Target      string `json:"target"`
	TargetCWD   string `json:"target_cwd"`
	Home        string `json:"home,omitempty"`
	Execute     bool   `json:"execute"`
}

type Result struct {
	Status          string       `json:"status"`
	Target          string       `json:"target"`
	SessionID       string       `json:"session_id"`
	Title           string       `json:"title"`
	TargetHome      string       `json:"target_home"`
	TargetCWD       string       `json:"target_cwd"`
	SessionPath     string       `json:"session_path"`
	BackupDir       string       `json:"backup_dir,omitempty"`
	Writes          []string     `json:"writes"`
	CreatedFiles    []string     `json:"created_files"`
	DryRun          bool         `json:"dry_run"`
	ContinueCommand string       `json:"continue_command,omitempty"`
	Verification    Verification `json:"verification"`
}

type Verification struct {
	SessionFile bool  `json:"session_file"`
	Index       bool  `json:"index"`
	State       *bool `json:"state,omitempty"`
	Pinned      *bool `json:"pinned,omitempty"`
}

type ImportError struct {
	Code      string
	Message   string
	BackupDir string
	Steps     []string
}

func (e *ImportError) Error() string { return e.Message }

func fail(code, format string, args ...any) *ImportError {
	return &ImportError{Code: code, Message: fmt.Sprintf(format, args...)}
}

func failAfter(code, backupDir string, steps []string, format string, args ...any) *ImportError {
	return &ImportError{
		Code: code, Message: fmt.Sprintf(format, args...), BackupDir: backupDir,
		Steps: append([]string(nil), steps...),
	}
}

type handoffDocument struct {
	Schema    string `json:"schema"`
	CreatedAt string `json:"created_at"`
	Source    struct {
		Agent     string `json:"agent"`
		SessionID string `json:"session_id"`
		Title     string `json:"title"`
	} `json:"source"`
	SessionState struct {
		Objective string `json:"objective"`
	} `json:"session_state"`
	Conversation struct {
		Records []handoffRecord `json:"records"`
	} `json:"conversation"`
}

type handoffRecord struct {
	Role      string         `json:"role"`
	Timestamp string         `json:"timestamp"`
	Blocks    []handoffBlock `json:"blocks"`
}

type handoffBlock struct {
	Kind        string          `json:"kind"`
	Text        string          `json:"text"`
	CallID      string          `json:"call_id"`
	ToolName    string          `json:"tool_name"`
	Arguments   json.RawMessage `json:"arguments"`
	Status      string          `json:"status"`
	Content     []handoffBlock  `json:"content"`
	LogicalPath string          `json:"logical_path"`
	Mapping     struct {
		SourceType string `json:"source_type"`
	} `json:"mapping"`
}

type transcript struct {
	SourceAgent string
	SourceID    string
	Title       string
	CreatedAt   string
	Entries     []entry
}

type entry struct {
	Kind       string
	Timestamp  string
	Role       string
	Text       string
	Tool       string
	CallID     string
	Status     string
	NativeType string
	Input      string
	Output     string
}

func Import(request Request) (Result, *ImportError) {
	loaded, err := loadRequest(request)
	if err != nil {
		return Result{}, err
	}
	switch loaded.request.Target {
	case "codex":
		return importCodex(loaded)
	case "claude_code":
		return importClaude(loaded)
	default:
		return Result{}, fail("invalid_target", "target must be codex or claude_code")
	}
}

type loadedRequest struct {
	request    Request
	handoff    handoffDocument
	transcript transcript
	targetCWD  string
	home       string
}

func loadRequest(request Request) (loadedRequest, *ImportError) {
	request.Target = strings.ToLower(strings.TrimSpace(request.Target))
	if request.Target == "claude" {
		request.Target = "claude_code"
	}
	if request.HandoffPath == "" {
		return loadedRequest{}, fail("invalid_request", "handoff_path is required")
	}
	info, err := os.Lstat(request.HandoffPath)
	if err != nil {
		return loadedRequest{}, fail("handoff_not_found", "cannot inspect handoff.json: %v", err)
	}
	if !info.Mode().IsRegular() || info.Size() > maxHandoffBytes {
		return loadedRequest{}, fail("handoff_invalid", "handoff.json must be an ordinary file no larger than 1 GiB")
	}
	handoffPath, err := filepath.Abs(request.HandoffPath)
	if err != nil {
		return loadedRequest{}, fail("handoff_invalid", "cannot resolve handoff.json: %v", err)
	}
	file, err := os.Open(handoffPath)
	if err != nil {
		return loadedRequest{}, fail("handoff_not_found", "cannot open handoff.json: %v", err)
	}
	defer file.Close()
	decoder := json.NewDecoder(file)
	var handoff handoffDocument
	if err := decoder.Decode(&handoff); err != nil {
		return loadedRequest{}, fail("handoff_invalid", "cannot decode handoff.json: %v", err)
	}
	if handoff.Schema != "relay.handoff.v1" {
		return loadedRequest{}, fail("handoff_invalid", "unsupported handoff schema %q", handoff.Schema)
	}
	targetCWD, err := filepath.Abs(strings.TrimSpace(request.TargetCWD))
	if err != nil || targetCWD == "" {
		return loadedRequest{}, fail("invalid_target_cwd", "target_cwd must be an existing directory")
	}
	cwdInfo, err := os.Stat(targetCWD)
	if err != nil || !cwdInfo.IsDir() {
		return loadedRequest{}, fail("invalid_target_cwd", "target_cwd must be an existing directory")
	}
	home, homeErr := resolveHome(request.Target, request.Home)
	if homeErr != nil {
		return loadedRequest{}, homeErr
	}
	request.HandoffPath = handoffPath
	request.TargetCWD = targetCWD
	loaded := loadedRequest{
		request:    request,
		handoff:    handoff,
		transcript: transcriptFromHandoff(handoff),
		targetCWD:  targetCWD,
		home:       home,
	}
	if !loaded.transcript.importable() {
		return loadedRequest{}, fail(
			"no_importable_content",
			"the share contains no visible conversation or project instructions that can be imported",
		)
	}
	return loaded, nil
}

func resolveHome(target, configured string) (string, *ImportError) {
	value := strings.TrimSpace(configured)
	if value == "" {
		if target == "codex" {
			value = os.Getenv("CODEX_HOME")
		} else {
			value = os.Getenv("CLAUDE_CONFIG_DIR")
		}
	}
	if value == "" {
		userHome, err := os.UserHomeDir()
		if err != nil {
			return "", fail("home_unavailable", "cannot resolve the user home: %v", err)
		}
		if target == "codex" {
			value = filepath.Join(userHome, ".codex")
		} else {
			value = filepath.Join(userHome, ".claude")
		}
	}
	abs, err := filepath.Abs(value)
	if err != nil {
		return "", fail("home_unavailable", "cannot resolve target home: %v", err)
	}
	return abs, nil
}

func transcriptFromHandoff(handoff handoffDocument) transcript {
	title := strings.Join(strings.Fields(handoff.Source.Title), " ")
	if title == "" {
		title = strings.Join(strings.Fields(handoff.SessionState.Objective), " ")
	}
	if title == "" {
		title = "Relay 导入会话"
	}
	title = clip(title, 120)
	createdAt := handoff.CreatedAt
	if _, err := time.Parse(time.RFC3339Nano, createdAt); err != nil {
		createdAt = time.Now().UTC().Format(time.RFC3339Nano)
	}
	out := transcript{
		SourceAgent: handoff.Source.Agent,
		SourceID:    handoff.Source.SessionID,
		Title:       title,
		CreatedAt:   createdAt,
	}
	toolNames := map[string]string{}
	for _, record := range handoff.Conversation.Records {
		role := strings.ToLower(strings.TrimSpace(record.Role))
		var textParts []string
		flushText := func() {
			text := strings.TrimSpace(strings.Join(textParts, "\n\n"))
			textParts = nil
			if text == "" || (role != "user" && role != "assistant") {
				return
			}
			out.Entries = append(out.Entries, entry{
				Kind: "message", Timestamp: record.Timestamp, Role: role, Text: text,
			})
		}
		for _, block := range record.Blocks {
			switch block.Kind {
			case "text":
				if strings.TrimSpace(block.Text) != "" {
					textParts = append(textParts, strings.TrimSpace(block.Text))
				}
			case "tool_call":
				flushText()
				input := nativeToolInput(block.Arguments, block.Mapping.SourceType)
				out.Entries = append(out.Entries, entry{
					Kind: "tool_call", Timestamp: record.Timestamp, Tool: block.ToolName,
					CallID: block.CallID, Status: block.Status,
					NativeType: block.Mapping.SourceType, Input: input,
				})
				if block.CallID != "" {
					toolNames[block.CallID] = block.ToolName
				}
			case "tool_result":
				flushText()
				var resultParts []string
				for _, child := range block.Content {
					if child.Kind == "text" && strings.TrimSpace(child.Text) != "" {
						resultParts = append(resultParts, strings.TrimSpace(child.Text))
					}
				}
				output := strings.Join(resultParts, "\n")
				if output != "" || block.CallID != "" {
					out.Entries = append(out.Entries, entry{
						Kind: "tool_result", Timestamp: record.Timestamp,
						Tool:   toolNames[block.CallID],
						CallID: block.CallID, Status: block.Status,
						NativeType: block.Mapping.SourceType, Output: output,
					})
				}
			case "context_compacted":
				flushText()
				out.Entries = append(out.Entries, entry{
					Kind: "context_compacted", Timestamp: record.Timestamp,
					NativeType: block.Mapping.SourceType,
				})
			case "source_context":
				flushText()
				if strings.TrimSpace(block.Text) != "" {
					label := "项目说明"
					if block.LogicalPath != "" {
						label = block.LogicalPath
					}
					out.Entries = append(out.Entries, entry{
						Kind: "context", Timestamp: record.Timestamp,
						Text: "[Relay 导入的项目说明：" + label + "]\n" + strings.TrimSpace(block.Text),
					})
				}
			}
		}
		flushText()
	}
	return out
}

func (value transcript) importable() bool {
	for _, item := range value.Entries {
		if (item.Kind == "message" || item.Kind == "context") && strings.TrimSpace(item.Text) != "" {
			return true
		}
	}
	return false
}

func compactJSON(value json.RawMessage) string {
	if len(value) == 0 || string(value) == "null" {
		return ""
	}
	var decoded any
	if json.Unmarshal(value, &decoded) != nil {
		return string(value)
	}
	encoded, err := json.Marshal(decoded)
	if err != nil {
		return ""
	}
	return string(encoded)
}

func nativeToolInput(value json.RawMessage, nativeType string) string {
	if nativeType == "custom_tool_call" {
		var text string
		if json.Unmarshal(value, &text) == nil {
			return text
		}
	}
	return compactJSON(value)
}

func clip(value string, limit int) string {
	value = strings.TrimSpace(value)
	runes := []rune(value)
	if len(runes) <= limit {
		return value
	}
	return strings.TrimSpace(string(runes[:limit])) + "…"
}

func timestampOr(value string, fallback time.Time) string {
	if parsed, err := time.Parse(time.RFC3339Nano, value); err == nil {
		return parsed.UTC().Format(time.RFC3339Nano)
	}
	return fallback.UTC().Format(time.RFC3339Nano)
}
