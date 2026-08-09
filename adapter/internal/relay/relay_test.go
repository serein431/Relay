package relay

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
	"testing"
	"time"
)

func fixturePath(t *testing.T, parts ...string) string {
	t.Helper()
	_, filename, _, ok := runtime.Caller(0)
	if !ok {
		t.Fatal("runtime.Caller failed")
	}
	root := filepath.Join(filepath.Dir(filename), "..", "..", "testdata")
	return filepath.Join(append([]string{root}, parts...)...)
}

func TestClaudeSessionPreservesVisibleRecordsAndHidesPrivateData(t *testing.T) {
	home := fixturePath(t, "claude_home")
	parsed, err := ParseSession(SessionOptions{Agent: AgentClaude, SessionID: "claude-session-1", ClaudeHome: home, CodexHome: fixturePath(t, "codex_home")})
	if err != nil {
		t.Fatal(err)
	}
	if parsed.Summary.Title != "Relay parser fixture" {
		t.Fatalf("unexpected title: %q", parsed.Summary.Title)
	}
	if parsed.Summary.Preview != "I will inspect the files." {
		t.Fatalf("unexpected preview: %q", parsed.Summary.Preview)
	}
	if parsed.Summary.CWD != "/tmp/relay-demo" || parsed.Summary.ProjectName != "relay-demo" {
		t.Fatalf("unexpected project metadata: %+v", parsed.Summary)
	}
	if parsed.Summary.MessageCount != 7 || parsed.Summary.ToolCallCount != 1 || parsed.Summary.ToolResultCount != 2 {
		t.Fatalf("unexpected record counts: %+v", parsed.Summary)
	}
	if parsed.Completeness.Status != "partial" {
		t.Fatalf("expected partial completeness, got %+v", parsed.Completeness)
	}
	assertWarningCodes(t, parsed.Warnings, "invalid_json", "orphan_tool_result", "unknown_record", "unsupported_block")
	assertNoPrivateStrings(t, parsed, []string{
		"CLAUDE_PRIVATE_REASONING",
		"CLAUDE_ENCRYPTED_INPUT",
		"CLAUDE_UNKNOWN_BLOCK_SECRET",
		"CLAUDE_UNKNOWN_RECORD_SECRET",
		"CLAUDE_PROVIDER_INTERNAL_PROMPT",
		"CLAUDE_SKILL_LISTING_SECRET",
		"CLAUDE_SIDECHAIN_SUBAGENT_SECRET",
		"CLAUDE_EARLY_SIDECHAIN_SECRET",
		"CLAUDE_SIDECHAIN_UUID_TOKEN",
		"CLAUDE_SIDECHAIN_TIMESTAMP_TOKEN",
		"CLAUDE_SIDECHAIN_CWD_TOKEN",
		"CLAUDE_SIDECHAIN_SESSION_TOKEN",
		"CLAUDE_SIDECHAIN_VERSION_TOKEN",
		"CLAUDE_AUXILIARY_SUBAGENT_FILE_SECRET",
		"provider snapshot",
	})
	assertMessageBranch(t, parsed.Messages, "claude-user-1", "claude-branch-main")
	assertTextBlock(t, parsed.Messages, "Implement the Relay parser")
	assertTextBlock(t, parsed.Messages, "I will inspect the files.")
	assertBlockKind(t, parsed.Messages, "source_context")
	assertUnsupportedTypes(t, parsed.Messages, "future_private_block", "future-record")
	assertPairedToolCall(t, parsed.Messages, "claude-call-1")
	assertToolRecordsNeverReplay(t, parsed.Messages)

	handoff := parsed.Handoff(time.Date(2026, 8, 7, 3, 0, 0, 0, time.UTC))
	if handoff.Schema != PreviewSchema || handoff.Export.NativeHistory {
		t.Fatalf("unexpected handoff preview: %+v", handoff.Export)
	}
}

func TestCodexSessionHandlesWrappedRecordsAndHidesProviderState(t *testing.T) {
	home := fixturePath(t, "codex_home")
	parsed, err := ParseSession(SessionOptions{Agent: AgentCodex, SessionID: "codex-session-1", ClaudeHome: fixturePath(t, "claude_home"), CodexHome: home})
	if err != nil {
		t.Fatal(err)
	}
	if parsed.Summary.Title != "Codex Relay fixture" || parsed.Summary.NativeVersion != "0.142.5" {
		t.Fatalf("unexpected summary: %+v", parsed.Summary)
	}
	if parsed.Summary.Preview != "I am checking the adapter." {
		t.Fatalf("unexpected preview: %q", parsed.Summary.Preview)
	}
	if parsed.Summary.MessageCount != 7 || parsed.Summary.ToolCallCount != 1 || parsed.Summary.ToolResultCount != 2 {
		t.Fatalf("unexpected record counts: %+v", parsed.Summary)
	}
	if parsed.Completeness.Status != "partial" {
		t.Fatalf("expected partial completeness, got %+v", parsed.Completeness)
	}
	assertWarningCodes(t, parsed.Warnings, "invalid_json", "orphan_tool_result", "unknown_record", "unsupported_response_item")
	assertNoPrivateStrings(t, parsed, []string{
		"CODEX_PROVIDER_BASE_INSTRUCTIONS",
		"CODEX_WORLD_STATE_SECRET",
		"CODEX_DEVELOPER_SECRET",
		"CODEX_DEVELOPER_TURN_TOKEN",
		"CODEX_DEVELOPER_BRANCH_TOKEN",
		"CODEX_ENCRYPTED_REASONING",
		"CODEX_REASONING_SUMMARY",
		"CODEX_REASONING_TURN_TOKEN",
		"CODEX_ENCRYPTED_BLOCK",
		"CODEX_ENCRYPTED_ARGUMENT",
		"CODEX_UNKNOWN_ITEM_SECRET",
		"CODEX_UNKNOWN_RECORD_SECRET",
		"CODEX_EVENT_REASONING_SECRET",
		"CODEX_AGENT_MESSAGE_ENVELOPE",
		"CODEX_AGENT_MESSAGE_ENCRYPTED",
		"CODEX_AGENT_TURN_TOKEN",
	})
	assertMessageBranch(t, parsed.Messages, "codex-user-1", "codex-branch-main")
	assertTextBlock(t, parsed.Messages, "Continue implementing Relay")
	assertTextBlock(t, parsed.Messages, "I am checking the adapter.")
	assertBlockKind(t, parsed.Messages, "asset_ref")
	assertUnsupportedTypes(t, parsed.Messages, "future_response_item", "future_top_record")
	assertPairedToolCall(t, parsed.Messages, "codex-call-1")
	assertToolRecordsNeverReplay(t, parsed.Messages)
}

func TestCodexSessionKeepsTheFirstSessionIdentity(t *testing.T) {
	content := strings.Join([]string{
		`{"type":"session_meta","payload":{"id":"current-session","cwd":"/tmp/current"}}`,
		`{"type":"session_meta","payload":{"id":"inherited-parent","cwd":"/tmp/parent"}}`,
		`{"type":"response_item","payload":{"type":"message","id":"injected-1","role":"user","content":[{"type":"input_text","text":"<recommended_plugins>hidden provider context"}]}}`,
		`{"type":"response_item","payload":{"type":"message","id":"user-1","role":"user","content":[{"type":"input_text","text":"Current task"}]}}`,
	}, "\n") + "\n"
	options := writeTemporarySession(t, AgentCodex, "current-session", []byte(content))
	parsed, err := ParseSession(options)
	if err != nil {
		t.Fatal(err)
	}
	if parsed.Summary.SessionID != "current-session" {
		t.Fatalf("inherited metadata replaced the file identity: %+v", parsed.Summary)
	}
	if parsed.Summary.CWD != "/tmp/current" {
		t.Fatalf("inherited metadata replaced the current cwd: %+v", parsed.Summary)
	}
	if parsed.Summary.Title != "Current task" {
		t.Fatalf("provider context replaced the visible session title: %+v", parsed.Summary)
	}
	if _, err := findCodexCandidate(options.CodexHome, "inherited-parent"); err == nil {
		t.Fatal("an inherited parent id resolved to the child session file")
	}
}

func TestLegacyCodexSessionExtractsCWDWithoutExportingEnvironmentPrompt(t *testing.T) {
	home := fixturePath(t, "codex_home")
	parsed, err := ParseSession(SessionOptions{Agent: AgentCodex, SessionID: "legacy-session-1", ClaudeHome: fixturePath(t, "claude_home"), CodexHome: home})
	if err != nil {
		t.Fatal(err)
	}
	if parsed.Summary.CWD != "/tmp/legacy-relay" {
		t.Fatalf("legacy cwd was not extracted: %+v", parsed.Summary)
	}
	if parsed.Summary.Completeness != "complete" || parsed.Summary.MessageCount != 4 {
		t.Fatalf("unexpected legacy completeness: %+v", parsed.Summary)
	}
	if parsed.Summary.Preview != "I will run a command." {
		t.Fatalf("unexpected legacy preview: %q", parsed.Summary.Preview)
	}
	assertNoPrivateStrings(t, parsed, []string{"CODEX_LEGACY_INSTRUCTIONS", "CODEX_LEGACY_STATE_SECRET", "<environment_context>"})
}

func TestPreviewFromMessagesUsesRecentVisibleTextAndStaysBounded(t *testing.T) {
	long := strings.Repeat("界", 170)
	messages := []Message{
		{Role: "assistant", Blocks: []Block{{Kind: "text", Text: "earlier"}}},
		{Role: "system", Blocks: []Block{{Kind: "text", Text: "system text"}}},
		{Role: "assistant", Blocks: []Block{{Kind: "tool_result", Text: "tool output"}}},
		{Role: "user", Blocks: []Block{{Kind: "text", Text: "<environment_context>hidden"}}},
		{Role: "assistant", Blocks: []Block{{Kind: "text", Text: "  " + long + "\n  final  "}}},
	}
	preview := previewFromMessages(messages)
	if len([]rune(preview)) != 160 || !strings.HasSuffix(preview, "...") {
		t.Fatalf("preview was not normalized and bounded: %q", preview)
	}
}

func TestDiscoverGroupsSessionsByProjectMetadata(t *testing.T) {
	result, err := Discover(DiscoverOptions{
		ClaudeHome: fixturePath(t, "claude_home"),
		CodexHome:  fixturePath(t, "codex_home"),
		Limit:      20,
	}, time.Date(2026, 8, 7, 4, 0, 0, 0, time.UTC))
	if err != nil {
		t.Fatal(err)
	}
	if result.Schema != ProtocolSchema || len(result.Sessions) != 3 {
		t.Fatalf("unexpected discovery result: %+v", result)
	}
	if result.Warnings == nil {
		t.Fatal("discovery warnings must encode as an empty array, not null")
	}
	ids := make([]string, 0, len(result.Sessions))
	for _, session := range result.Sessions {
		if session.SessionID == "claude-aux-subagent" {
			t.Fatal("Claude auxiliary subagent file was exposed as a main session")
		}
		ids = append(ids, session.Agent+":"+session.SessionID+":"+session.ProjectName)
		if session.ProjectKey == "" || session.SourcePath == "" {
			t.Fatalf("missing project/source metadata: %+v", session)
		}
	}
	sort.Strings(ids)
	want := []string{
		"claude_code:claude-session-1:relay-demo",
		"codex:codex-session-1:relay-demo",
		"codex:legacy-session-1:legacy-relay",
	}
	for i := range want {
		if ids[i] != want[i] {
			t.Fatalf("sessions mismatch: got %v want %v", ids, want)
		}
	}
}

func TestInspectAndExportDoNotModifyProviderHomes(t *testing.T) {
	claudeHome := fixturePath(t, "claude_home")
	codexHome := fixturePath(t, "codex_home")
	before := treeSnapshots(t, claudeHome, codexHome)

	if _, err := Inspect(SessionOptions{Agent: AgentClaude, SessionID: "claude-session-1", ClaudeHome: claudeHome, CodexHome: codexHome}); err != nil {
		t.Fatal(err)
	}
	if _, err := Export(SessionOptions{Agent: AgentCodex, SessionID: "codex-session-1", ClaudeHome: claudeHome, CodexHome: codexHome}, time.Now()); err != nil {
		t.Fatal(err)
	}
	after := treeSnapshots(t, claudeHome, codexHome)
	if !snapshotsEqual(before, after) {
		t.Fatalf("provider fixture homes changed:\nbefore=%v\nafter=%v", before, after)
	}
}

func TestMissingAndOrphanToolRecordsProduceWarnings(t *testing.T) {
	tests := []struct {
		name    string
		agent   string
		content string
	}{
		{
			name:  "claude",
			agent: AgentClaude,
			content: strings.Join([]string{
				`{"type":"user","message":{"role":"user","content":"start"},"sessionId":"tool-bounds","uuid":"user-1"}`,
				`{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"missing-result","name":"Read","input":{}}]},"sessionId":"tool-bounds","uuid":"call-1"}`,
				`{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"orphan-result","content":"safe","is_error":false}]},"sessionId":"tool-bounds","uuid":"result-1"}`,
			}, "\n") + "\n",
		},
		{
			name:  "codex",
			agent: AgentCodex,
			content: strings.Join([]string{
				`{"type":"session_meta","payload":{"id":"tool-bounds"}}`,
				`{"type":"response_item","payload":{"type":"function_call","id":"call-1","call_id":"missing-result","name":"read","arguments":"{}"}}`,
				`{"type":"response_item","payload":{"type":"function_call_output","id":"result-1","call_id":"orphan-result","output":"safe"}}`,
			}, "\n") + "\n",
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			options := writeTemporarySession(t, test.agent, "tool-bounds", []byte(test.content))
			parsed, err := ParseSession(options)
			if err != nil {
				t.Fatal(err)
			}
			if parsed.Completeness.OrphanToolResults != 1 || parsed.Completeness.UnmatchedToolCalls != 1 {
				t.Fatalf("unexpected tool completeness: %+v", parsed.Completeness)
			}
			assertWarningCodes(t, parsed.Warnings, "orphan_tool_result", "unmatched_tool_calls")
		})
	}
}

func TestJSONLBoundariesAreBoundedAndParsingCanContinue(t *testing.T) {
	t.Run("oversized line", func(t *testing.T) {
		oversized := `{"type":"future-record","secret":"OVERSIZED_LINE_TOKEN","padding":"` + strings.Repeat("x", MaxSessionLineBytes) + `"}`
		content := strings.Join([]string{
			`{"type":"user","message":{"role":"user","content":"before"},"sessionId":"line-bounds","uuid":"before"}`,
			oversized,
			`{"type":"assistant","message":{"role":"assistant","content":"after"},"sessionId":"line-bounds","uuid":"after"}`,
		}, "\n") + "\n"
		parsed, err := ParseSession(writeTemporarySession(t, AgentClaude, "line-bounds", []byte(content)))
		if err != nil {
			t.Fatal(err)
		}
		if parsed.Summary.MessageCount != 2 || parsed.Completeness.DamagedLines != 1 {
			t.Fatalf("oversized line did not remain bounded: summary=%+v completeness=%+v", parsed.Summary, parsed.Completeness)
		}
		assertWarningCodes(t, parsed.Warnings, "line_too_large")
		assertNoPrivateStrings(t, parsed.Warnings, []string{"OVERSIZED_LINE_TOKEN", oversized})
	})

	t.Run("deep json", func(t *testing.T) {
		deep := `{"type":"future-record","payload":` + strings.Repeat("[", MaxJSONDepth) + `"DEEP_JSON_TOKEN"` + strings.Repeat("]", MaxJSONDepth) + `}`
		content := deep + "\n" + `{"type":"user","message":{"role":"user","content":"after depth warning"},"sessionId":"depth-bounds","uuid":"after"}` + "\n"
		parsed, err := ParseSession(writeTemporarySession(t, AgentClaude, "depth-bounds", []byte(content)))
		if err != nil {
			t.Fatal(err)
		}
		if parsed.Summary.MessageCount != 1 || parsed.Completeness.DamagedLines != 1 {
			t.Fatalf("deep JSON was not skipped safely: summary=%+v completeness=%+v", parsed.Summary, parsed.Completeness)
		}
		assertWarningCodes(t, parsed.Warnings, "json_too_deep")
		assertNoPrivateStrings(t, parsed.Warnings, []string{"DEEP_JSON_TOKEN", deep})
	})

	t.Run("deep embedded tool arguments", func(t *testing.T) {
		deepArguments := strings.Repeat("[", MaxJSONDepth+1) + `"EMBEDDED_JSON_TOKEN"` + strings.Repeat("]", MaxJSONDepth+1)
		callRecord, marshalErr := json.Marshal(map[string]any{
			"type": "response_item",
			"payload": map[string]any{
				"type":      "function_call",
				"id":        "deep-call-record",
				"call_id":   "deep-call",
				"name":      "example",
				"arguments": deepArguments,
			},
		})
		if marshalErr != nil {
			t.Fatal(marshalErr)
		}
		content := `{"type":"session_meta","payload":{"id":"embedded-depth"}}` + "\n" + string(callRecord) + "\n" + `{"type":"response_item","payload":{"type":"function_call_output","id":"deep-result-record","call_id":"deep-call","output":"safe"}}` + "\n"
		parsed, err := ParseSession(writeTemporarySession(t, AgentCodex, "embedded-depth", []byte(content)))
		if err != nil {
			t.Fatal(err)
		}
		assertWarningCodes(t, parsed.Warnings, "embedded_json_too_deep")
		assertPairedToolCall(t, parsed.Messages, "deep-call")
		assertNoPrivateStrings(t, parsed, []string{"EMBEDDED_JSON_TOKEN", deepArguments})
	})

	t.Run("truncated final line", func(t *testing.T) {
		partial := `{"type":"assistant","message":{"role":"assistant","content":"TRUNCATED_FINAL_TOKEN"}`
		content := `{"type":"user","message":{"role":"user","content":"complete"},"sessionId":"half-line","uuid":"complete"}` + "\n" + partial
		parsed, err := ParseSession(writeTemporarySession(t, AgentClaude, "half-line", []byte(content)))
		if err != nil {
			t.Fatal(err)
		}
		if parsed.Summary.MessageCount != 1 || parsed.Completeness.DamagedLines != 1 || parsed.Completeness.TotalLines != 2 {
			t.Fatalf("truncated line handling changed: summary=%+v completeness=%+v", parsed.Summary, parsed.Completeness)
		}
		assertWarningCodes(t, parsed.Warnings, "truncated_final_line")
		assertNoPrivateStrings(t, parsed.Warnings, []string{"TRUNCATED_FINAL_TOKEN", partial})
	})
}

func TestSessionFileSizeLimitReturnsStableSafeError(t *testing.T) {
	options := writeTemporarySession(t, AgentClaude, "oversized-file", []byte("OVERSIZED_FILE_TOKEN\n"))
	path := filepath.Join(options.ClaudeHome, "projects", "-tmp-relay", "oversized-file.jsonl")
	if err := os.Truncate(path, MaxSessionFileBytes+1); err != nil {
		t.Fatal(err)
	}
	_, err := ParseSession(options)
	if !errors.Is(err, ErrSessionTooLarge) {
		t.Fatalf("expected ErrSessionTooLarge, got %v", err)
	}
	if strings.Contains(err.Error(), "OVERSIZED_FILE_TOKEN") || strings.Contains(err.Error(), path) {
		t.Fatalf("session limit error leaked input details: %q", err)
	}
	discovered, discoverErr := Discover(DiscoverOptions{
		Agents:     []string{AgentClaude},
		ClaudeHome: options.ClaudeHome,
		CodexHome:  options.CodexHome,
	}, time.Now())
	if discoverErr != nil {
		t.Fatal(discoverErr)
	}
	if len(discovered.Sessions) != 0 {
		t.Fatalf("oversized session was returned by discovery: %+v", discovered.Sessions)
	}
	assertWarningCodes(t, discovered.Warnings, "session_too_large")
	assertNoPrivateStrings(t, discovered.Warnings, []string{"OVERSIZED_FILE_TOKEN", path})
}

func TestUnknownTypeAndWarningsNeverCopyRawPayload(t *testing.T) {
	raw := `{"type":"sk-secret-token-value","secret":"UNKNOWN_RAW_BODY_TOKEN","content":"full raw record body","sessionId":"UNKNOWN_SESSION_TOKEN","cwd":"/tmp/UNKNOWN_CWD_TOKEN","version":"UNKNOWN_VERSION_TOKEN","timestamp":"UNKNOWN_TIMESTAMP_TOKEN","uuid":"UNKNOWN_UUID_TOKEN","parentUuid":"UNKNOWN_PARENT_TOKEN","branchId":"UNKNOWN_BRANCH_TOKEN"}`
	content := `{"type":"user","message":{"role":"user","content":"safe"},"sessionId":"safe-warning","uuid":"safe"}` + "\n" + raw + "\n"
	parsed, err := ParseSession(writeTemporarySession(t, AgentClaude, "safe-warning", []byte(content)))
	if err != nil {
		t.Fatal(err)
	}
	assertNoPrivateStrings(t, parsed, []string{
		"sk-secret-token-value", "UNKNOWN_RAW_BODY_TOKEN", "full raw record body", "UNKNOWN_SESSION_TOKEN",
		"UNKNOWN_CWD_TOKEN", "UNKNOWN_VERSION_TOKEN", "UNKNOWN_TIMESTAMP_TOKEN", "UNKNOWN_UUID_TOKEN",
		"UNKNOWN_PARENT_TOKEN", "UNKNOWN_BRANCH_TOKEN", raw,
	})
	assertUnsupportedTypes(t, parsed.Messages, "unknown")
	for _, warning := range parsed.Warnings {
		if strings.Contains(warning.Message, "{") || strings.Contains(warning.Message, "\n") {
			t.Fatalf("warning contains raw record text: %+v", warning)
		}
	}
}

func TestUnknownCodexItemsKeepOnlySafeTypeAndMapping(t *testing.T) {
	raw := `{"timestamp":"UNKNOWN_CODEX_TIMESTAMP_TOKEN","type":"response_item","payload":{"type":"future_visible_item","id":"UNKNOWN_CODEX_ID_TOKEN","turn_id":"UNKNOWN_CODEX_TURN_TOKEN","branch_id":"UNKNOWN_CODEX_BRANCH_TOKEN","content":"UNKNOWN_CODEX_BODY_TOKEN","encrypted_content":"UNKNOWN_CODEX_ENCRYPTED_TOKEN"}}`
	content := `{"timestamp":"2026-08-08T00:00:00Z","type":"session_meta","payload":{"id":"safe-codex-unknown","cwd":"/tmp/safe"}}` + "\n" + raw + "\n"
	parsed, err := ParseSession(writeTemporarySession(t, AgentCodex, "safe-codex-unknown", []byte(content)))
	if err != nil {
		t.Fatal(err)
	}
	assertUnsupportedTypes(t, parsed.Messages, "future_visible_item")
	assertNoPrivateStrings(t, parsed, []string{
		"UNKNOWN_CODEX_TIMESTAMP_TOKEN", "UNKNOWN_CODEX_ID_TOKEN", "UNKNOWN_CODEX_TURN_TOKEN",
		"UNKNOWN_CODEX_BRANCH_TOKEN", "UNKNOWN_CODEX_BODY_TOKEN", "UNKNOWN_CODEX_ENCRYPTED_TOKEN", raw,
	})
}

func assertWarningCodes(t *testing.T, warnings []Warning, want ...string) {
	t.Helper()
	seen := map[string]bool{}
	for _, warning := range warnings {
		seen[warning.Code] = true
	}
	for _, code := range want {
		if !seen[code] {
			t.Fatalf("missing warning %q in %+v", code, warnings)
		}
	}
}

func assertNoPrivateStrings(t *testing.T, value any, forbidden []string) {
	t.Helper()
	payload, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	text := string(payload)
	for _, secret := range forbidden {
		if strings.Contains(text, secret) {
			t.Fatalf("private/provider data leaked: %q", secret)
		}
	}
}

func assertToolRecordsNeverReplay(t *testing.T, messages []Message) {
	t.Helper()
	for _, message := range messages {
		for _, block := range message.Blocks {
			if (block.Kind == "tool_call" || block.Kind == "tool_result") && block.ReplayPolicy != "never" {
				t.Fatalf("tool record can be replayed: %+v", block)
			}
		}
	}
}

func assertPairedToolCall(t *testing.T, messages []Message, callID string) {
	t.Helper()
	calls := 0
	results := 0
	for _, message := range messages {
		for _, block := range message.Blocks {
			if block.CallID != callID {
				continue
			}
			switch block.Kind {
			case "tool_call":
				calls++
			case "tool_result":
				results++
			}
		}
	}
	if calls != 1 || results != 1 {
		t.Fatalf("tool call %q was not paired exactly once: calls=%d results=%d", callID, calls, results)
	}
}

func assertMessageBranch(t *testing.T, messages []Message, messageID, branchID string) {
	t.Helper()
	for _, message := range messages {
		if message.ID == messageID {
			if message.BranchID != branchID {
				t.Fatalf("message %q branch mismatch: got %q want %q", messageID, message.BranchID, branchID)
			}
			return
		}
	}
	t.Fatalf("message %q was not found", messageID)
}

func assertBlockKind(t *testing.T, messages []Message, kind string) {
	t.Helper()
	for _, message := range messages {
		for _, block := range message.Blocks {
			if block.Kind == kind {
				return
			}
		}
	}
	t.Fatalf("block kind %q was not found", kind)
}

func assertTextBlock(t *testing.T, messages []Message, text string) {
	t.Helper()
	for _, message := range messages {
		for _, block := range message.Blocks {
			if block.Kind == "text" && block.Text == text {
				return
			}
		}
	}
	t.Fatalf("text block %q was not found", text)
}

func assertUnsupportedTypes(t *testing.T, messages []Message, want ...string) {
	t.Helper()
	seen := map[string]bool{}
	for _, message := range messages {
		for _, block := range message.Blocks {
			if block.Kind != "unsupported" {
				continue
			}
			if block.Mapping == nil || block.Mapping.Status != "unmapped" || block.Mapping.SourceType != block.NativeType || block.SafeSummary == "" || block.Classification != "user_visible" {
				t.Fatalf("unsafe unsupported block: %+v", block)
			}
			if block.Input != nil || block.Output != nil || block.Source != nil || block.Text != "" {
				t.Fatalf("unsupported block retained raw payload fields: %+v", block)
			}
			seen[block.NativeType] = true
		}
	}
	for _, nativeType := range want {
		if !seen[nativeType] {
			t.Fatalf("unsupported type %q was not found; seen=%v", nativeType, seen)
		}
	}
}

func writeTemporarySession(t *testing.T, agent, sessionID string, content []byte) SessionOptions {
	t.Helper()
	root := t.TempDir()
	claudeHome := filepath.Join(root, "claude")
	codexHome := filepath.Join(root, "codex")
	var path string
	switch agent {
	case AgentClaude:
		path = filepath.Join(claudeHome, "projects", "-tmp-relay", sessionID+".jsonl")
	case AgentCodex:
		path = filepath.Join(codexHome, "sessions", "2026", "08", "08", "rollout-2026-08-08T00-00-00-"+sessionID+".jsonl")
	default:
		t.Fatalf("unsupported test agent %q", agent)
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, content, 0o600); err != nil {
		t.Fatal(err)
	}
	return SessionOptions{Agent: agent, SessionID: sessionID, ClaudeHome: claudeHome, CodexHome: codexHome}
}

type fileSnapshot struct {
	Size       int64
	ModifiedNS int64
	Digest     string
}

func treeSnapshots(t *testing.T, roots ...string) map[string]fileSnapshot {
	t.Helper()
	out := map[string]fileSnapshot{}
	for _, root := range roots {
		err := filepath.WalkDir(root, func(path string, entry os.DirEntry, err error) error {
			if err != nil {
				return err
			}
			if entry.IsDir() {
				return nil
			}
			info, err := entry.Info()
			if err != nil {
				return err
			}
			data, err := os.ReadFile(path)
			if err != nil {
				return err
			}
			digest := sha256.Sum256(data)
			out[path] = fileSnapshot{
				Size:       info.Size(),
				ModifiedNS: info.ModTime().UnixNano(),
				Digest:     hex.EncodeToString(digest[:]),
			}
			return nil
		})
		if err != nil {
			t.Fatal(err)
		}
	}
	return out
}

func snapshotsEqual(left, right map[string]fileSnapshot) bool {
	if len(left) != len(right) {
		return false
	}
	for key, value := range left {
		if right[key] != value {
			return false
		}
	}
	return true
}
