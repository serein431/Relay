package nativeimport

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"relay.local/agent-adapter/internal/relay"
)

func writeTestHandoff(t *testing.T, directory string) string {
	t.Helper()
	handoff := map[string]any{
		"schema":     "relay.handoff.v1",
		"created_at": "2026-08-10T00:00:00Z",
		"source": map[string]any{
			"agent": "claude_code", "session_id": "source-session", "title": "旧标题",
		},
		"session_state": map[string]any{"objective": "修复 Relay 导入功能"},
		"conversation": map[string]any{
			"records": []any{
				map[string]any{
					"role": "user", "timestamp": "2026-08-10T00:00:01Z",
					"blocks": []any{map[string]any{"kind": "text", "text": "请修复导入功能"}},
				},
				map[string]any{
					"role": "assistant", "timestamp": "2026-08-10T00:00:02Z",
					"blocks": []any{
						map[string]any{"kind": "text", "text": "正在检查。"},
						map[string]any{
							"kind": "tool_call", "call_id": "call-1", "tool_name": "exec_command",
							"arguments": map[string]any{"cmd": "pnpm check"}, "status": "completed",
						},
					},
				},
				map[string]any{
					"role": "tool", "timestamp": "2026-08-10T00:00:03Z",
					"blocks": []any{map[string]any{
						"kind": "tool_result", "call_id": "call-1", "status": "success",
						"content": []any{map[string]any{"kind": "text", "text": "passed"}},
					}},
				},
			},
		},
	}
	return writeHandoff(t, directory, handoff)
}

func writeHandoff(t *testing.T, directory string, handoff map[string]any) string {
	t.Helper()
	content, err := json.Marshal(handoff)
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(directory, "handoff.json")
	if err := os.WriteFile(path, content, 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func baseHandoff(records []any) map[string]any {
	return map[string]any{
		"schema":     "relay.handoff.v1",
		"created_at": "2026-08-10T00:00:00Z",
		"source": map[string]any{
			"agent": "codex", "session_id": "source-session", "title": "Relay 导入测试",
		},
		"session_state": map[string]any{"objective": "Relay 导入测试"},
		"conversation":  map[string]any{"records": records},
	}
}

func createCodexState(t *testing.T, home string) string {
	t.Helper()
	statePath := filepath.Join(home, codexStateFile)
	if err := os.MkdirAll(home, 0o755); err != nil {
		t.Fatal(err)
	}
	if _, err := runSQLite(statePath, `CREATE TABLE threads (
        id TEXT PRIMARY KEY,
        rollout_path TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        source TEXT NOT NULL,
        model_provider TEXT NOT NULL,
        cwd TEXT NOT NULL,
        title TEXT NOT NULL,
        sandbox_policy TEXT NOT NULL,
        approval_mode TEXT NOT NULL,
        preview TEXT NOT NULL DEFAULT '',
        recency_at INTEGER NOT NULL DEFAULT 0,
        history_mode TEXT NOT NULL DEFAULT 'legacy',
        is_pinned INTEGER NOT NULL DEFAULT 0
    );`); err != nil {
		t.Fatal(err)
	}
	return statePath
}

func createCodexGlobalState(t *testing.T, home string) (string, []byte) {
	t.Helper()
	if err := os.MkdirAll(home, 0o755); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(home, codexGlobalStateFile)
	content := []byte(`{"pinned-thread-ids":["existing-task"],"large-timestamp":1786298037456,"nested":{"keep":"unchanged"}}`)
	if err := os.WriteFile(path, content, 0o600); err != nil {
		t.Fatal(err)
	}
	return path, content
}

func TestReadCodexModelProvider(t *testing.T) {
	tests := []struct {
		name        string
		config      string
		write       bool
		expected    string
		expectError bool
	}{
		{name: "missing config uses the Codex default", expected: "openai"},
		{
			name:   "reads the recipient top-level provider",
			config: "model_provider = \"custom\" # recipient setting\n\n[model_providers.custom]\nname = \"OpenAI\"\n",
			write:  true, expected: "custom",
		},
		{name: "accepts a TOML literal string", config: "model_provider = 'bedrock'\n", write: true, expected: "bedrock"},
		{
			name:   "ignores provider-shaped keys inside tables",
			config: "[model_providers.custom]\nmodel_provider = \"not-top-level\"\n",
			write:  true, expected: "openai",
		},
		{name: "rejects a non-string provider", config: "model_provider = 42\n", write: true, expectError: true},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			home := t.TempDir()
			if test.write {
				if err := os.WriteFile(filepath.Join(home, codexConfigFile), []byte(test.config), 0o600); err != nil {
					t.Fatal(err)
				}
			}
			provider, err := readCodexModelProvider(home)
			if test.expectError {
				if err == nil {
					t.Fatalf("expected an error, got provider %q", provider)
				}
				return
			}
			if err != nil || provider != test.expected {
				t.Fatalf("provider = %q, error = %v; want %q", provider, err, test.expected)
			}
		})
	}
}

func TestCodexImportCreatesNativeTaskAndIndex(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeTestHandoff(t, temp)
	statePath := createCodexState(t, home)
	if err := os.WriteFile(
		filepath.Join(home, codexConfigFile),
		[]byte("model_provider = \"custom\" # use the recipient's configured provider\n\n[model_providers.custom]\nname = \"OpenAI\"\n"),
		0o600,
	); err != nil {
		t.Fatal(err)
	}

	result, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	if result.Status != "ok" || result.DryRun || result.SessionID == "" {
		t.Fatalf("unexpected import result: %+v", result)
	}
	if len(result.CreatedFiles) != 2 || result.CreatedFiles[0] != result.SessionPath {
		t.Fatalf("new files were not reported: %+v", result.CreatedFiles)
	}
	session, err := os.ReadFile(result.SessionPath)
	if err != nil {
		t.Fatal(err)
	}
	text := string(session)
	for _, expected := range []string{"session_meta", `"model_provider":"custom"`, "请修复导入功能", "exec_command", "passed", "只是历史记录，不得重新执行"} {
		if !strings.Contains(text, expected) {
			t.Fatalf("Codex history does not contain %q", expected)
		}
	}
	if strings.Contains(text, `"namespace":"relay_source"`) ||
		!strings.Contains(text, `"status":"completed","type":"function_call"`) ||
		!strings.Contains(text, `"id":"fco_relay_`) ||
		!strings.Contains(text, `"status":"completed","type":"function_call_output"`) {
		t.Fatalf("ChatGPT historical tool records were not stored as completed native history: %s", text)
	}
	index, err := os.ReadFile(filepath.Join(home, codexIndexFile))
	if err != nil || !strings.Contains(string(index), result.SessionID) {
		t.Fatalf("Codex index was not updated: %v", err)
	}
	rows, err := runSQLite(statePath, "SELECT id,title,cwd,model_provider,is_pinned FROM threads;")
	if err != nil || !strings.Contains(string(rows), result.SessionID) ||
		!strings.Contains(string(rows), `"model_provider":"custom"`) ||
		!strings.Contains(string(rows), `"is_pinned":1`) {
		t.Fatalf("Codex SQLite row was not written: %v %s", err, rows)
	}
	if result.Verification.State == nil || !*result.Verification.State ||
		result.Verification.Pinned == nil || !*result.Verification.Pinned {
		t.Fatalf("ChatGPT database and pinned state were not verified: %+v", result.Verification)
	}
	if _, err := os.Stat(filepath.Join(result.BackupDir, codexStateFile)); err != nil {
		t.Fatalf("ChatGPT database backup is missing: %v", err)
	}
}

func TestCodexImportRequiresAnInitializedChatGPTDatabase(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeTestHandoff(t, temp)

	_, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true,
	})
	if importErr == nil || importErr.Code != "chatgpt_state_not_found" {
		t.Fatalf("unexpected error: %+v", importErr)
	}
	if _, err := os.Stat(home); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("missing ChatGPT state still caused writes: %v", err)
	}
}

func TestCodexImportUpdatesTheDesktopPinnedTaskList(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	createCodexState(t, home)
	globalStatePath, originalGlobalState := createCodexGlobalState(t, home)
	handoffPath := writeTestHandoff(t, temp)

	result, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	if result.Verification.Pinned == nil || !*result.Verification.Pinned {
		t.Fatalf("pinned task list was not verified: %+v", result.Verification)
	}
	foundWrite := false
	for _, path := range result.Writes {
		if path == globalStatePath {
			foundWrite = true
		}
	}
	if !foundWrite {
		t.Fatalf("global state write was not reported: %+v", result.Writes)
	}
	content, err := os.ReadFile(globalStatePath)
	if err != nil {
		t.Fatal(err)
	}
	var document map[string]json.RawMessage
	if err := json.Unmarshal(content, &document); err != nil {
		t.Fatal(err)
	}
	var pinned []string
	if err := json.Unmarshal(document["pinned-thread-ids"], &pinned); err != nil {
		t.Fatal(err)
	}
	if len(pinned) != 2 || pinned[0] != result.SessionID || pinned[1] != "existing-task" {
		t.Fatalf("unexpected pinned task list: %+v", pinned)
	}
	if string(document["large-timestamp"]) != "1786298037456" || !strings.Contains(string(document["nested"]), "unchanged") {
		t.Fatalf("unrelated global state changed: %s", content)
	}
	backup, err := os.ReadFile(filepath.Join(result.BackupDir, codexGlobalStateFile))
	if err != nil || string(backup) != string(originalGlobalState) {
		t.Fatalf("global state backup is missing or changed: %v %s", err, backup)
	}
}

func TestImportedTitleDistinguishesNearbyUUIDv7Sessions(t *testing.T) {
	now := time.Date(2026, 8, 10, 2, 17, 0, 0, time.Local)
	first := importedTitle("同一段会话", now, "019fe7be-3ad4-7756-b6ea-81199f76933a")
	second := importedTitle("同一段会话", now, "019fe7be-a287-7470-a25e-c357eac33499")
	if first == second {
		t.Fatalf("nearby UUID v7 sessions received the same title: %q", first)
	}
	if !strings.Contains(first, "019fe7be-9f76933a") || !strings.Contains(second, "019fe7be-eac33499") {
		t.Fatalf("titles do not contain recognizable unique session IDs: %q %q", first, second)
	}
}

func TestImportedTitleUsesTheSourceSessionTitle(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	createCodexState(t, home)
	handoffPath := writeTestHandoff(t, temp)
	result, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd,
		Home: home, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	if !strings.HasPrefix(result.Title, "旧标题 · Relay ") {
		t.Fatalf("the source session title was not preserved: %q", result.Title)
	}
}

func TestImportedTitleIsACompactSingleLine(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	createCodexState(t, home)
	handoff := baseHandoff([]any{map[string]any{
		"role": "user", "blocks": []any{map[string]any{"kind": "text", "text": "继续"}},
	}})
	handoff["source"].(map[string]any)["title"] = "第一行\n\n第二行   第三段"
	handoffPath := writeHandoff(t, temp, handoff)

	result, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	if !strings.HasPrefix(result.Title, "第一行 第二行 第三段 · Relay ") || strings.Contains(result.Title, "\n") {
		t.Fatalf("imported title was not compacted: %q", result.Title)
	}
}

func TestClaudeImportCreatesNativeSessionAndProjectIndex(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "claude-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeTestHandoff(t, temp)
	result, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "claude_code", TargetCWD: cwd, Home: home, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	if result.ContinueCommand != "claude --resume "+result.SessionID {
		t.Fatalf("unexpected continue command: %q", result.ContinueCommand)
	}
	session, err := os.ReadFile(result.SessionPath)
	if err != nil {
		t.Fatal(err)
	}
	text := string(session)
	for _, expected := range []string{"queue-operation", "请修复导入功能", "Relay 历史工具调用", "Relay 历史工具结果", "passed"} {
		if !strings.Contains(text, expected) {
			t.Fatalf("Claude history does not contain %q", expected)
		}
	}
	indexPath := filepath.Join(filepath.Dir(result.SessionPath), "sessions-index.json")
	index, err := os.ReadFile(indexPath)
	if err != nil || !strings.Contains(string(index), result.SessionID) {
		t.Fatalf("Claude index was not updated: %v", err)
	}
}

func TestClaudeImportPreservesExistingIndexEntriesAndDuplicateTitles(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "claude-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	projectDir := filepath.Join(home, "projects", claudeProjectDirName(cwd))
	if err := os.MkdirAll(projectDir, 0o755); err != nil {
		t.Fatal(err)
	}
	indexPath := filepath.Join(projectDir, "sessions-index.json")
	existingIndex := map[string]any{
		"version": 1,
		"entries": []any{map[string]any{
			"sessionId": "existing-session", "firstPrompt": "同名会话",
		}},
		"originalPath": cwd,
	}
	encoded, err := json.Marshal(existingIndex)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(indexPath, encoded, 0o600); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeTestHandoff(t, temp)

	first, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "claude_code", TargetCWD: cwd,
		Home: home, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	second, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "claude_code", TargetCWD: cwd,
		Home: home, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	if first.Title == second.Title || first.SessionID == second.SessionID {
		t.Fatalf("repeated imports must have unique titles and ids: %+v %+v", first, second)
	}
	index, err := os.ReadFile(indexPath)
	if err != nil {
		t.Fatal(err)
	}
	for _, expected := range []string{"existing-session", first.SessionID, second.SessionID} {
		if !strings.Contains(string(index), expected) {
			t.Fatalf("Claude index lost %q: %s", expected, index)
		}
	}
	if len(first.CreatedFiles) != 1 || first.CreatedFiles[0] != first.SessionPath {
		t.Fatalf("existing index was incorrectly reported as a new file: %+v", first.CreatedFiles)
	}
}

func TestImportedSessionsAreRediscoveredByBothAdapters(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	codexHome := filepath.Join(temp, "codex-home")
	claudeHome := filepath.Join(temp, "claude-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	createCodexState(t, codexHome)
	handoffPath := writeTestHandoff(t, temp)

	codexResult, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd,
		Home: codexHome, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	claudeResult, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "claude_code", TargetCWD: cwd,
		Home: claudeHome, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}

	discovered, err := relay.Discover(relay.DiscoverOptions{
		ClaudeHome: claudeHome,
		CodexHome:  codexHome,
	}, time.Now())
	if err != nil {
		t.Fatal(err)
	}
	if len(discovered.Sessions) != 2 {
		t.Fatalf("expected both imported sessions, got %+v", discovered.Sessions)
	}
	for _, summary := range discovered.Sessions {
		if summary.CWD != cwd {
			t.Fatalf("wrong imported working directory: %+v", summary)
		}
		if !strings.HasPrefix(summary.Title, "旧标题") {
			t.Fatalf("wrong imported title: %+v", summary)
		}
	}

	codexParsed, err := relay.ParseSession(relay.SessionOptions{
		Agent: relay.AgentCodex, SessionID: codexResult.SessionID,
		ClaudeHome: claudeHome, CodexHome: codexHome,
	})
	if err != nil {
		t.Fatal(err)
	}
	if codexParsed.Summary.ToolCallCount != 1 || codexParsed.Summary.ToolResultCount != 1 {
		t.Fatalf("ChatGPT tool history was not rediscovered: %+v", codexParsed.Summary)
	}
	if codexParsed.Summary.MessageCount != 4 {
		t.Fatalf("ChatGPT visible messages were duplicated or omitted: %+v", codexParsed.Summary)
	}
	assertVisibleOrder(t, codexParsed, []string{"请修复导入功能", "正在检查", "pnpm check", "passed"})

	claudeParsed, err := relay.ParseSession(relay.SessionOptions{
		Agent: relay.AgentClaude, SessionID: claudeResult.SessionID,
		ClaudeHome: claudeHome, CodexHome: codexHome,
	})
	if err != nil {
		t.Fatal(err)
	}
	assertVisibleOrder(t, claudeParsed, []string{"请修复导入功能", "正在检查", "Relay 历史工具调用", "Relay 历史工具结果"})
}

func TestConfiguredAgentHomesAreUsedForImportAndRediscovery(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	codexHome := filepath.Join(temp, "configured-codex-home")
	claudeHome := filepath.Join(temp, "configured-claude-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	t.Setenv("CODEX_HOME", codexHome)
	t.Setenv("CLAUDE_CONFIG_DIR", claudeHome)
	createCodexState(t, codexHome)
	handoffPath := writeTestHandoff(t, temp)

	codexResult, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	claudeResult, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "claude_code", TargetCWD: cwd, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	if !strings.HasPrefix(codexResult.SessionPath, codexHome+string(os.PathSeparator)) {
		t.Fatalf("ChatGPT import ignored CODEX_HOME: %s", codexResult.SessionPath)
	}
	if !strings.HasPrefix(claudeResult.SessionPath, claudeHome+string(os.PathSeparator)) {
		t.Fatalf("Claude Code import ignored CLAUDE_CONFIG_DIR: %s", claudeResult.SessionPath)
	}

	discovered, err := relay.Discover(relay.DiscoverOptions{}, time.Now())
	if err != nil {
		t.Fatal(err)
	}
	if len(discovered.Sessions) != 2 {
		t.Fatalf("configured homes did not rediscover both imports: %+v", discovered.Sessions)
	}
}

func assertVisibleOrder(t *testing.T, parsed relay.ParsedSession, expected []string) {
	t.Helper()
	var visible []string
	for _, message := range parsed.Messages {
		for _, block := range message.Blocks {
			value := block.Text
			if value == "" {
				encoded, _ := json.Marshal([]any{block.Input, block.Output})
				value = string(encoded)
			}
			visible = append(visible, value)
		}
	}
	joined := strings.Join(visible, "\n")
	last := -1
	for _, value := range expected {
		position := strings.Index(joined, value)
		if position < 0 || position <= last {
			t.Fatalf("visible history order does not contain %q after position %d:\n%s", value, last, joined)
		}
		last = position
	}
}

func TestConversationOrderIsPreserved(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	createCodexState(t, home)
	handoffPath := writeHandoff(t, temp, baseHandoff([]any{
		map[string]any{
			"role": "user", "timestamp": "2026-08-10T00:00:01Z",
			"blocks": []any{map[string]any{"kind": "text", "text": "开始"}},
		},
		map[string]any{
			"role": "assistant", "timestamp": "2026-08-10T00:00:02Z",
			"blocks": []any{
				map[string]any{"kind": "text", "text": "调用前"},
				map[string]any{"kind": "tool_call", "call_id": "ordered-call", "tool_name": "exec_command", "arguments": map[string]any{"cmd": "pwd"}},
				map[string]any{"kind": "tool_result", "call_id": "ordered-call", "status": "success", "content": []any{map[string]any{"kind": "text", "text": "工具结果"}}},
				map[string]any{"kind": "text", "text": "调用后"},
			},
		},
	}))
	result, importErr := Import(Request{HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true})
	if importErr != nil {
		t.Fatal(importErr)
	}
	content, err := os.ReadFile(result.SessionPath)
	if err != nil {
		t.Fatal(err)
	}
	text := string(content)
	positions := []int{
		strings.Index(text, "调用前"),
		strings.Index(text, `"type":"function_call"`),
		strings.Index(text, `"type":"function_call_output"`),
		strings.Index(text, "调用后"),
	}
	for index, position := range positions {
		if position < 0 || (index > 0 && position <= positions[index-1]) {
			t.Fatalf("conversation order was not preserved: %v\n%s", positions, text)
		}
	}
}

func TestChatGPTImportRestoresNativeCustomToolRecordsAndCompactionMarker(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	createCodexState(t, home)
	handoffPath := writeHandoff(t, temp, baseHandoff([]any{
		map[string]any{
			"role": "user", "timestamp": "2026-08-10T00:00:01Z",
			"blocks": []any{map[string]any{"kind": "text", "text": "检查文件"}},
		},
		map[string]any{
			"role": "assistant", "timestamp": "2026-08-10T00:00:02Z",
			"blocks": []any{map[string]any{
				"kind": "tool_call", "call_id": "native-call", "tool_name": "exec",
				"arguments": "const result = await tools.exec_command({cmd: \"pwd\"});",
				"status":    "completed",
				"mapping":   map[string]any{"source_type": "custom_tool_call"},
			}},
		},
		map[string]any{
			"role": "tool", "timestamp": "2026-08-10T00:00:03Z",
			"blocks": []any{map[string]any{
				"kind": "tool_result", "call_id": "native-call", "status": "success",
				"mapping": map[string]any{"source_type": "custom_tool_call_output"},
				"content": []any{map[string]any{
					"kind": "text", "text": `[{"type":"input_text","text":"/tmp/project"}]`,
				}},
			}},
		},
		map[string]any{
			"role": "system", "timestamp": "2026-08-10T00:00:04Z",
			"blocks": []any{map[string]any{
				"kind":    "context_compacted",
				"mapping": map[string]any{"source_type": "context_compacted"},
			}},
		},
	}))

	result, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	content, err := os.ReadFile(result.SessionPath)
	if err != nil {
		t.Fatal(err)
	}
	var call map[string]any
	var output map[string]any
	var checkpoint map[string]any
	compacted := false
	for _, line := range strings.Split(string(content), "\n") {
		var record map[string]any
		if json.Unmarshal([]byte(line), &record) != nil {
			continue
		}
		if record["type"] == "compacted" {
			checkpoint, _ = record["payload"].(map[string]any)
		}
		payload, _ := record["payload"].(map[string]any)
		switch payload["type"] {
		case "custom_tool_call":
			call = payload
		case "custom_tool_call_output":
			output = payload
		case "context_compacted":
			compacted = record["type"] == "event_msg"
		}
	}
	if call == nil || call["name"] != "exec" || call["namespace"] != nil ||
		call["input"] != `const result = await tools.exec_command({cmd: "pwd"});` || call["id"] == nil {
		t.Fatalf("native custom tool call was not restored: %+v", call)
	}
	metadata, _ := call["internal_chat_message_metadata_passthrough"].(map[string]any)
	if metadata["turn_id"] == nil {
		t.Fatalf("custom tool call is missing its turn metadata: %+v", call)
	}
	if output == nil || output["id"] == nil {
		t.Fatal("native custom tool output was not restored")
	}
	if _, ok := output["output"].([]any); !ok {
		t.Fatalf("custom tool output did not recover its structured value: %+v", output)
	}
	if !compacted {
		t.Fatal("visible context compaction event was not restored")
	}
	if checkpoint == nil {
		t.Fatal("imported session is missing its resume compaction checkpoint")
	}
	replacement, err := json.Marshal(checkpoint["replacement_history"])
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(replacement), "检查文件") ||
		strings.Contains(string(replacement), `tools.exec_command`) ||
		strings.Contains(string(replacement), "/tmp/project") {
		t.Fatalf("replacement history did not keep conversation context separate from tool evidence: %s", replacement)
	}
	parsed, err := relay.ParseSession(relay.SessionOptions{
		Agent: relay.AgentCodex, SessionID: result.SessionID, CodexHome: home,
	})
	if err != nil {
		t.Fatal(err)
	}
	foundMarker := false
	for _, message := range parsed.Messages {
		for _, block := range message.Blocks {
			if block.Kind == "context_compacted" {
				foundMarker = true
			}
		}
	}
	if !foundMarker {
		t.Fatalf("imported session did not expose the context compaction marker: %+v", parsed.Messages)
	}
}

func TestProjectInstructionsCanCreateANativeSession(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeHandoff(t, temp, baseHandoff([]any{map[string]any{
		"role": "developer", "timestamp": "2026-08-10T00:00:01Z",
		"blocks": []any{map[string]any{
			"kind": "source_context", "logical_path": "AGENTS.md", "text": "只使用中文回复。",
		}},
	}}))
	result, importErr := Import(Request{HandoffPath: handoffPath, Target: "claude_code", TargetCWD: cwd, Home: filepath.Join(temp, "claude-home"), Execute: true})
	if importErr != nil {
		t.Fatal(importErr)
	}
	content, err := os.ReadFile(result.SessionPath)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(content), "Relay 导入的项目说明：AGENTS.md") || !strings.Contains(string(content), "只使用中文回复") {
		t.Fatalf("project instructions were not preserved: %s", content)
	}
}

func TestProjectInstructionsRemainTheFirstVisibleChatGPTMessage(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	statePath := createCodexState(t, home)
	if _, err := runSQLite(statePath, `ALTER TABLE threads ADD COLUMN first_user_message TEXT NOT NULL DEFAULT '';`); err != nil {
		t.Fatal(err)
	}
	if _, err := runSQLite(statePath, `ALTER TABLE threads ADD COLUMN name TEXT;`); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeHandoff(t, temp, baseHandoff([]any{
		map[string]any{
			"role": "developer", "timestamp": "2026-08-10T00:00:01Z",
			"blocks": []any{map[string]any{
				"kind": "source_context", "logical_path": "AGENTS.md", "text": "只使用中文回复。",
			}},
		},
		map[string]any{
			"role": "user", "timestamp": "2026-08-10T00:00:02Z",
			"blocks": []any{map[string]any{"kind": "text", "text": "继续处理。"}},
		},
	}))
	result, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	parsed, err := relay.ParseSession(relay.SessionOptions{
		Agent: relay.AgentCodex, SessionID: result.SessionID, CodexHome: home,
	})
	if err != nil {
		t.Fatal(err)
	}
	assertVisibleOrder(t, parsed, []string{"Relay 导入的项目说明：AGENTS.md", "只使用中文回复", "继续处理"})
	if len(parsed.Messages) == 0 || len(parsed.Messages[0].Blocks) == 0 ||
		!strings.Contains(parsed.Messages[0].Blocks[0].Text, "Relay 导入的项目说明：AGENTS.md") {
		t.Fatalf("project instructions were not the first visible ChatGPT record: %+v", parsed.Messages)
	}
	rows, err := runSQLite(statePath, "SELECT first_user_message FROM threads WHERE id="+sqliteLiteral(result.SessionID)+";")
	if err != nil || !strings.Contains(string(rows), result.Title) {
		t.Fatalf("ChatGPT database did not preserve the imported title seed: %v %s", err, rows)
	}
}

func TestAssistantOnlyShareCreatesAReadableChatGPTTask(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	createCodexState(t, home)
	handoffPath := writeHandoff(t, temp, baseHandoff([]any{map[string]any{
		"role": "assistant", "timestamp": "2026-08-10T00:00:01Z",
		"blocks": []any{map[string]any{"kind": "text", "text": "这是发送者留下的助手回复。"}},
	}}))
	result, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd,
		Home: home, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	content, err := os.ReadFile(result.SessionPath)
	if err != nil {
		t.Fatal(err)
	}
	text := string(content)
	title := strings.Index(text, "Relay 导入测试 · Relay ")
	seed := strings.Index(text, "Relay 导入说明")
	assistant := strings.Index(text, "这是发送者留下的助手回复")
	if title < 0 || seed < 0 || assistant <= seed {
		t.Fatalf("assistant-only history was not placed after a user turn: %s", text)
	}
}

func TestToolHistoryBeforeFirstUserMessageIsPreserved(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	createCodexState(t, home)
	handoffPath := writeHandoff(t, temp, baseHandoff([]any{
		map[string]any{
			"role": "assistant", "timestamp": "2026-08-10T00:00:01Z",
			"blocks": []any{
				map[string]any{"kind": "tool_call", "call_id": "early-call", "tool_name": "read_file", "arguments": map[string]any{"path": "README.md"}},
				map[string]any{"kind": "tool_result", "call_id": "early-call", "content": []any{map[string]any{"kind": "text", "text": "早期工具结果"}}},
				map[string]any{"kind": "text", "text": "工具检查完成。"},
			},
		},
		map[string]any{
			"role": "user", "timestamp": "2026-08-10T00:00:02Z",
			"blocks": []any{map[string]any{"kind": "text", "text": "继续处理。"}},
		},
	}))
	result, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd,
		Home: home, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	content, err := os.ReadFile(result.SessionPath)
	if err != nil {
		t.Fatal(err)
	}
	text := string(content)
	positions := []int{
		strings.Index(text, "Relay 导入说明"),
		strings.Index(text, `"call_id":"early-call"`),
		strings.Index(text, "早期工具结果"),
		strings.Index(text, "工具检查完成"),
		strings.Index(text, "继续处理"),
	}
	for index, position := range positions {
		if position < 0 || (index > 0 && position <= positions[index-1]) {
			t.Fatalf("early tool history order was not preserved: %v\n%s", positions, text)
		}
	}
	if !strings.Contains(text, "Relay 导入测试 · Relay ") {
		t.Fatalf("native title event was not preserved: %s", text)
	}
}

func TestChatGPTStoresTheUniqueTitleWithoutAddingAVisibleTitleMessage(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	statePath := createCodexState(t, home)
	if _, err := runSQLite(statePath, `ALTER TABLE threads ADD COLUMN first_user_message TEXT NOT NULL DEFAULT '';`); err != nil {
		t.Fatal(err)
	}
	if _, err := runSQLite(statePath, `ALTER TABLE threads ADD COLUMN name TEXT;`); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeTestHandoff(t, temp)
	result, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	content, err := os.ReadFile(result.SessionPath)
	if err != nil {
		t.Fatal(err)
	}
	var firstUser string
	var nativeTitle string
	for _, line := range strings.Split(string(content), "\n") {
		var item struct {
			Type    string `json:"type"`
			Payload struct {
				Type       string `json:"type"`
				Message    string `json:"message"`
				ThreadID   string `json:"thread_id"`
				ThreadName string `json:"thread_name"`
			} `json:"payload"`
		}
		if json.Unmarshal([]byte(line), &item) != nil || item.Type != "event_msg" {
			continue
		}
		if item.Payload.Type == "thread_name_updated" {
			if item.Payload.ThreadID != result.SessionID {
				t.Fatalf("native title event uses the wrong task id: %+v", item.Payload)
			}
			nativeTitle = item.Payload.ThreadName
		}
		if item.Payload.Type == "user_message" && firstUser == "" {
			firstUser = item.Payload.Message
		}
	}
	if nativeTitle != result.Title {
		t.Fatalf("native ChatGPT title event was not written: %q != %q", nativeTitle, result.Title)
	}
	if firstUser != "请修复导入功能" {
		t.Fatalf("the first visible ChatGPT message changed during import: %q", firstUser)
	}
	if firstUser == result.Title {
		t.Fatalf("the imported title was added as a visible user message: %q", firstUser)
	}
	rows, err := runSQLite(statePath, "SELECT name,first_user_message FROM threads WHERE id="+sqliteLiteral(result.SessionID)+";")
	if err != nil || strings.Count(string(rows), result.Title) != 2 {
		t.Fatalf("ChatGPT database did not receive the stable custom title: %v %s", err, rows)
	}
}

func TestToolOnlyShareDoesNotCreateAnEmptySession(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeHandoff(t, temp, baseHandoff([]any{map[string]any{
		"role": "assistant", "timestamp": "2026-08-10T00:00:01Z",
		"blocks": []any{map[string]any{"kind": "tool_call", "call_id": "call-only", "tool_name": "exec_command", "arguments": map[string]any{"cmd": "pwd"}}},
	}}))
	_, importErr := Import(Request{HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true})
	if importErr == nil || importErr.Code != "no_importable_content" {
		t.Fatalf("unexpected error: %+v", importErr)
	}
	if _, err := os.Stat(home); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("tool-only share wrote to the target home: %v", err)
	}
}

func TestUnpairedToolResultIsVisibleButNotExecutable(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	createCodexState(t, home)
	handoffPath := writeHandoff(t, temp, baseHandoff([]any{
		map[string]any{"role": "user", "timestamp": "2026-08-10T00:00:01Z", "blocks": []any{map[string]any{"kind": "text", "text": "检查结果"}}},
		map[string]any{
			"role": "tool", "timestamp": "2026-08-10T00:00:02Z",
			"blocks": []any{map[string]any{
				"kind": "tool_result", "call_id": "missing",
				"content": []any{map[string]any{"kind": "text", "text": "孤立结果"}},
			}},
		},
	}))
	result, importErr := Import(Request{HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true})
	if importErr != nil {
		t.Fatal(importErr)
	}
	content, err := os.ReadFile(result.SessionPath)
	if err != nil {
		t.Fatal(err)
	}
	text := string(content)
	if !strings.Contains(text, "孤立结果") || !strings.Contains(text, "不得自动重新执行") {
		t.Fatalf("unpaired result was not preserved safely: %s", text)
	}
}

func TestCodexImportedReplacementHistoryIsBounded(t *testing.T) {
	source := transcript{
		Title: "较长导入任务",
		Entries: []entry{
			{Kind: "message", Role: "user", Text: "OLD-" + strings.Repeat("旧", 24_000)},
			{Kind: "tool_call", Tool: "exec_command", Input: "TOOL-INPUT-" + strings.Repeat("x", 80_000)},
			{Kind: "tool_result", Tool: "exec_command", Output: "TOOL-OUTPUT-" + strings.Repeat("y", 120_000)},
			{Kind: "message", Role: "user", Text: "RECENT-" + strings.Repeat("新", 12_000)},
			{Kind: "message", Role: "assistant", Text: "ASSIST-" + strings.Repeat("答", 20_000)},
		},
	}

	replacement, visibleBytes := codexImportedReplacementHistory(source)
	encoded, err := json.Marshal(replacement)
	if err != nil {
		t.Fatal(err)
	}
	text := string(encoded)
	for _, forbidden := range []string{"TOOL-INPUT-", "TOOL-OUTPUT-", "OLD-"} {
		if strings.Contains(text, forbidden) {
			t.Fatalf("replacement history retained oversized historical content %q", forbidden)
		}
	}
	for _, expected := range []string{"RECENT-", "ASSIST-", codexCompactionSummaryPrefix, codexImportedHistoryInstruction} {
		if !strings.Contains(text, expected) {
			t.Fatalf("replacement history lost %q", expected)
		}
	}
	if visibleBytes <= 0 || visibleBytes > 160_000 {
		t.Fatalf("replacement history has an unsafe visible size: %d bytes", visibleBytes)
	}

	userRunes := 0
	for _, item := range replacement[1 : len(replacement)-1] {
		content, _ := item["content"].([]map[string]string)
		if len(content) == 1 {
			userRunes += len([]rune(content[0]["text"]))
		}
	}
	if userRunes > codexImportedUserHistoryMaxRunes {
		t.Fatalf("retained user history exceeded its budget: %d runes", userRunes)
	}
}

func TestLargeVisibleConversationImports(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	large := strings.Repeat("一段较长的会话内容。", 250_000)
	handoffPath := writeHandoff(t, temp, baseHandoff([]any{map[string]any{
		"role": "user", "timestamp": "2026-08-10T00:00:01Z", "blocks": []any{map[string]any{"kind": "text", "text": large}},
	}}))
	result, importErr := Import(Request{HandoffPath: handoffPath, Target: "claude_code", TargetCWD: cwd, Home: filepath.Join(temp, "claude-home"), Execute: true})
	if importErr != nil {
		t.Fatal(importErr)
	}
	info, err := os.Stat(result.SessionPath)
	if err != nil || info.Size() < int64(len(large)) {
		t.Fatalf("large session was truncated: %v size=%d", err, info.Size())
	}
}

func TestLargeToolEvidenceIsNotTruncated(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	input := strings.Repeat("输入内容", 4_000) + "INPUT-END"
	output := strings.Repeat("结果内容", 5_000) + "OUTPUT-END"
	handoffPath := writeHandoff(t, temp, baseHandoff([]any{
		map[string]any{
			"role": "user", "timestamp": "2026-08-10T00:00:01Z",
			"blocks": []any{map[string]any{"kind": "text", "text": "检查较长工具记录"}},
		},
		map[string]any{
			"role": "assistant", "timestamp": "2026-08-10T00:00:02Z",
			"blocks": []any{
				map[string]any{"kind": "tool_call", "call_id": "large-call", "tool_name": "exec_command", "arguments": map[string]any{"payload": input}},
				map[string]any{"kind": "tool_result", "call_id": "large-call", "status": "success", "content": []any{map[string]any{"kind": "text", "text": output}}},
			},
		},
	}))
	for _, target := range []string{"codex", "claude_code"} {
		home := filepath.Join(temp, target+"-home")
		if target == "codex" {
			createCodexState(t, home)
		}
		result, importErr := Import(Request{
			HandoffPath: handoffPath,
			Target:      target,
			TargetCWD:   cwd,
			Home:        home,
			Execute:     true,
		})
		if importErr != nil {
			t.Fatalf("%s import failed: %v", target, importErr)
		}
		content, err := os.ReadFile(result.SessionPath)
		if err != nil {
			t.Fatal(err)
		}
		if !strings.Contains(string(content), "INPUT-END") || !strings.Contains(string(content), "OUTPUT-END") {
			t.Fatalf("%s truncated selected tool evidence", target)
		}
	}
}

func TestCodexIndexFailureRemovesTheNewSession(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	createCodexState(t, home)
	handoffPath := writeTestHandoff(t, temp)
	original := appendCodexIndexForImport
	appendCodexIndexForImport = func(string, string, string, time.Time) error { return fmt.Errorf("injected index failure") }
	defer func() { appendCodexIndexForImport = original }()
	_, importErr := Import(Request{HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true})
	if importErr == nil || importErr.Code != "index_write_failed" {
		t.Fatalf("unexpected error: %+v", importErr)
	}
	matches, err := filepath.Glob(filepath.Join(home, "sessions", "*", "*", "*", "*.jsonl"))
	if err != nil || len(matches) != 0 {
		t.Fatalf("index failure left a session file: %v %v", matches, err)
	}
}

func TestCodexIndexFailureRemovesAPartiallyWrittenIndexEntry(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	createCodexState(t, home)
	handoffPath := writeTestHandoff(t, temp)
	indexPath := filepath.Join(home, codexIndexFile)
	original := appendCodexIndexForImport
	appendCodexIndexForImport = func(path, sessionID, title string, now time.Time) error {
		if err := appendCodexIndex(path, sessionID, title, now); err != nil {
			return err
		}
		return fmt.Errorf("injected failure after append")
	}
	defer func() { appendCodexIndexForImport = original }()
	_, importErr := Import(Request{HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true})
	if importErr == nil || importErr.Code != "index_write_failed" {
		t.Fatalf("unexpected error: %+v", importErr)
	}
	if regularFileExists(indexPath) {
		t.Fatalf("partial index failure left the newly created task index: %s", indexPath)
	}
	matches, err := filepath.Glob(filepath.Join(home, "sessions", "*", "*", "*", "*.jsonl"))
	if err != nil || len(matches) != 0 {
		t.Fatalf("partial index failure left a session file: %v %v", matches, err)
	}
}

func TestCodexSessionWriteFailureLeavesNoTaskRecords(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	createCodexState(t, home)
	handoffPath := writeTestHandoff(t, temp)
	original := writeCodexSessionForImport
	writeCodexSessionForImport = func(string, []byte, os.FileMode) error {
		return fmt.Errorf("injected session failure")
	}
	defer func() { writeCodexSessionForImport = original }()
	_, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd,
		Home: home, Execute: true,
	})
	if importErr == nil || importErr.Code != "session_write_failed" {
		t.Fatalf("unexpected error: %+v", importErr)
	}
	if regularFileExists(filepath.Join(home, codexIndexFile)) {
		t.Fatal("session write failure created a task index")
	}
	matches, err := filepath.Glob(filepath.Join(home, "sessions", "*", "*", "*", "*.jsonl"))
	if err != nil || len(matches) != 0 {
		t.Fatalf("session write failure left a task file: %v %v", matches, err)
	}
}

func TestCodexSupportsASecondSQLiteFieldVersion(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	statePath := filepath.Join(home, codexStateFile)
	if err := os.MkdirAll(home, 0o755); err != nil {
		t.Fatal(err)
	}
	if _, err := runSQLite(statePath, `CREATE TABLE threads (
		id TEXT PRIMARY KEY,
		rollout_path TEXT NOT NULL,
		created_at_ms INTEGER NOT NULL,
		updated_at_ms INTEGER NOT NULL,
		thread_source TEXT NOT NULL,
		cwd TEXT NOT NULL,
		title TEXT NOT NULL,
		first_user_message TEXT NOT NULL,
		is_pinned INTEGER NOT NULL
	);`); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeTestHandoff(t, temp)
	result, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd,
		Home: home, Execute: true,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	rows, err := runSQLite(statePath, "SELECT id,thread_source,first_user_message,is_pinned FROM threads;")
	if err != nil || !strings.Contains(string(rows), result.SessionID) || !strings.Contains(string(rows), `"is_pinned":1`) {
		t.Fatalf("second SQLite field version was not written: %v %s", err, rows)
	}
}

func TestUnknownRequiredSQLiteFieldRollsBackTheImport(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	statePath := filepath.Join(home, codexStateFile)
	if err := os.MkdirAll(home, 0o755); err != nil {
		t.Fatal(err)
	}
	if _, err := runSQLite(statePath, `CREATE TABLE threads (
		id TEXT PRIMARY KEY,
		rollout_path TEXT NOT NULL,
		future_required TEXT NOT NULL
	);`); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeTestHandoff(t, temp)
	_, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd,
		Home: home, Execute: true,
	})
	if importErr == nil || importErr.Code != "state_write_failed" {
		t.Fatalf("unexpected error: %+v", importErr)
	}
	if regularFileExists(filepath.Join(home, codexIndexFile)) {
		t.Fatal("unsupported SQLite schema left a task index")
	}
	matches, err := filepath.Glob(filepath.Join(home, "sessions", "*", "*", "*", "*.jsonl"))
	if err != nil || len(matches) != 0 {
		t.Fatalf("unsupported SQLite schema left a task file: %v %v", matches, err)
	}
}

func TestCodexStateFailureRestoresIndexAndSession(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	createCodexState(t, home)
	indexPath := filepath.Join(home, codexIndexFile)
	originalIndex := []byte("{\"id\":\"existing\",\"thread_name\":\"Existing\"}\n")
	if err := os.WriteFile(indexPath, originalIndex, 0o600); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeTestHandoff(t, temp)
	original := updateCodexStateForImport
	updateCodexStateForImport = func(string, codexRow) error { return fmt.Errorf("injected state failure") }
	defer func() { updateCodexStateForImport = original }()
	_, importErr := Import(Request{HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true})
	if importErr == nil || importErr.Code != "state_write_failed" {
		t.Fatalf("unexpected error: %+v", importErr)
	}
	index, err := os.ReadFile(indexPath)
	if err != nil || string(index) != string(originalIndex) {
		t.Fatalf("index was not restored: %v %q", err, index)
	}
	matches, err := filepath.Glob(filepath.Join(home, "sessions", "*", "*", "*", "*.jsonl"))
	if err != nil || len(matches) != 0 {
		t.Fatalf("state failure left a session file: %v %v", matches, err)
	}
}

func TestCodexStateFailureAfterInsertRemovesOnlyTheNewRow(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	statePath := createCodexState(t, home)
	if _, err := runSQLite(statePath, `INSERT INTO threads (
		id,rollout_path,created_at,updated_at,source,model_provider,cwd,title,
		sandbox_policy,approval_mode,preview,recency_at,history_mode,is_pinned
	) VALUES ('existing','/tmp/existing',1,1,'vscode','default','/tmp','Existing','{}','never','',1,'legacy',0);`); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeTestHandoff(t, temp)
	original := updateCodexStateForImport
	updateCodexStateForImport = func(path string, row codexRow) error {
		if err := updateCodexSQLite(path, row); err != nil {
			return err
		}
		return fmt.Errorf("injected failure after SQLite insert")
	}
	defer func() { updateCodexStateForImport = original }()

	_, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true,
	})
	if importErr == nil || importErr.Code != "state_write_failed" {
		t.Fatalf("unexpected error: %+v", importErr)
	}
	rows, err := runSQLite(statePath, "SELECT id FROM threads ORDER BY id;")
	if err != nil || string(rows) != "[{\"id\":\"existing\"}]\n" {
		t.Fatalf("SQLite rollback changed existing rows or kept the new row: %v %s", err, rows)
	}
	if regularFileExists(filepath.Join(home, codexIndexFile)) {
		t.Fatal("SQLite rollback left a newly created task index")
	}
	matches, err := filepath.Glob(filepath.Join(home, "sessions", "*", "*", "*", "*.jsonl"))
	if err != nil || len(matches) != 0 {
		t.Fatalf("SQLite rollback left a task file: %v %v", matches, err)
	}
}

func TestCodexPinFailureRollsBackOnlyTheNewTask(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	statePath := createCodexState(t, home)
	globalStatePath, originalGlobalState := createCodexGlobalState(t, home)
	indexPath := filepath.Join(home, codexIndexFile)
	originalIndex := []byte("{\"id\":\"existing\",\"thread_name\":\"Existing\"}\n")
	if err := os.WriteFile(indexPath, originalIndex, 0o600); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeTestHandoff(t, temp)
	original := pinCodexThreadForImport
	pinCodexThreadForImport = func(string, string) error { return fmt.Errorf("injected pin failure") }
	defer func() { pinCodexThreadForImport = original }()

	_, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: true,
	})
	if importErr == nil || importErr.Code != "pin_write_failed" {
		t.Fatalf("unexpected error: %+v", importErr)
	}
	index, err := os.ReadFile(indexPath)
	if err != nil || string(index) != string(originalIndex) {
		t.Fatalf("pin failure changed the existing task index: %v %q", err, index)
	}
	globalState, err := os.ReadFile(globalStatePath)
	if err != nil || string(globalState) != string(originalGlobalState) {
		t.Fatalf("pin failure changed the existing global state: %v %q", err, globalState)
	}
	rows, err := runSQLite(statePath, "SELECT COUNT(*) AS count FROM threads;")
	if err != nil || !strings.Contains(string(rows), `"count":0`) {
		t.Fatalf("pin failure left a SQLite task record: %v %s", err, rows)
	}
	matches, err := filepath.Glob(filepath.Join(home, "sessions", "*", "*", "*", "*.jsonl"))
	if err != nil || len(matches) != 0 {
		t.Fatalf("pin failure left a session file: %v %v", matches, err)
	}
}

func TestClaudeIndexFailureRemovesTheNewSession(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "claude-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeTestHandoff(t, temp)
	original := updateClaudeIndexForImport
	updateClaudeIndexForImport = func(string, string, string, string, string, []byte, time.Time) error {
		return fmt.Errorf("injected index failure")
	}
	defer func() { updateClaudeIndexForImport = original }()
	_, importErr := Import(Request{HandoffPath: handoffPath, Target: "claude_code", TargetCWD: cwd, Home: home, Execute: true})
	if importErr == nil || importErr.Code != "index_write_failed" {
		t.Fatalf("unexpected error: %+v", importErr)
	}
	matches, err := filepath.Glob(filepath.Join(home, "projects", "*", "*.jsonl"))
	if err != nil || len(matches) != 0 {
		t.Fatalf("index failure left a session file: %v %v", matches, err)
	}
}

func TestClaudeIndexFailureRemovesAPartiallyWrittenIndexEntry(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "claude-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeTestHandoff(t, temp)
	original := updateClaudeIndexForImport
	updateClaudeIndexForImport = func(path, cwd, sessionID, sessionPath, title string, session []byte, now time.Time) error {
		if err := updateClaudeIndex(path, cwd, sessionID, sessionPath, title, session, now); err != nil {
			return err
		}
		return fmt.Errorf("injected failure after index replacement")
	}
	defer func() { updateClaudeIndexForImport = original }()
	_, importErr := Import(Request{HandoffPath: handoffPath, Target: "claude_code", TargetCWD: cwd, Home: home, Execute: true})
	if importErr == nil || importErr.Code != "index_write_failed" {
		t.Fatalf("unexpected error: %+v", importErr)
	}
	projectDirectory := filepath.Join(home, "projects", claudeProjectDirName(cwd))
	if regularFileExists(filepath.Join(projectDirectory, "sessions-index.json")) {
		t.Fatalf("partial Claude index failure left the newly created index")
	}
	matches, err := filepath.Glob(filepath.Join(projectDirectory, "*.jsonl"))
	if err != nil || len(matches) != 0 {
		t.Fatalf("partial Claude index failure left a session file: %v %v", matches, err)
	}
}

func TestImportDryRunDoesNotWrite(t *testing.T) {
	temp := t.TempDir()
	cwd := filepath.Join(temp, "project")
	home := filepath.Join(temp, "codex-home")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	handoffPath := writeTestHandoff(t, temp)
	result, importErr := Import(Request{
		HandoffPath: handoffPath, Target: "codex", TargetCWD: cwd, Home: home, Execute: false,
	})
	if importErr != nil {
		t.Fatal(importErr)
	}
	if !result.DryRun {
		t.Fatal("dry run was not reported")
	}
	if _, err := os.Stat(result.SessionPath); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("dry run wrote a session file: %v", err)
	}
}
