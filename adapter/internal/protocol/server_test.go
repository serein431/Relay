package protocol

import (
	"bufio"
	"bytes"
	"encoding/json"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	"relay.local/agent-adapter/internal/relay"
)

func TestJSONLProtocolSupportsAllMethodsAndKeepsStdoutMachineReadable(t *testing.T) {
	claudeHome, codexHome := homes(t)
	requests := []map[string]any{
		{"id": "health-1", "method": "health", "params": map[string]any{}},
		{"id": "discover-1", "method": "discover_sessions", "params": map[string]any{"claude_home": claudeHome, "codex_home": codexHome, "limit": 20}},
		{"id": "inspect-1", "method": "inspect_session", "params": map[string]any{"agent": "claude_code", "session_id": "claude-session-1", "claude_home": claudeHome, "codex_home": codexHome}},
		{"id": "export-1", "method": "export_session", "params": map[string]any{"agent": "codex", "session_id": "codex-session-1", "claude_home": claudeHome, "codex_home": codexHome}},
		{"id": "missing-1", "method": "does_not_exist", "params": map[string]any{}},
	}
	var input bytes.Buffer
	encoder := json.NewEncoder(&input)
	for _, request := range requests {
		if err := encoder.Encode(request); err != nil {
			t.Fatal(err)
		}
	}
	var output bytes.Buffer
	now := time.Date(2026, 8, 7, 5, 0, 0, 0, time.UTC)
	if err := NewServer(func() time.Time { return now }).Serve(&input, &output); err != nil {
		t.Fatal(err)
	}

	var responses []map[string]any
	scanner := bufio.NewScanner(&output)
	for scanner.Scan() {
		var response map[string]any
		if err := json.Unmarshal(scanner.Bytes(), &response); err != nil {
			t.Fatalf("stdout contained non-JSON data: %q: %v", scanner.Text(), err)
		}
		responses = append(responses, response)
	}
	if err := scanner.Err(); err != nil {
		t.Fatal(err)
	}
	if len(responses) != len(requests) {
		t.Fatalf("got %d responses, want %d", len(responses), len(requests))
	}
	health := responses[0]["result"].(map[string]any)
	if health["protocol"] != "relay.adapter.v1" || health["read_only"] != true {
		t.Fatalf("unexpected health result: %+v", health)
	}
	limits := health["limits"].(map[string]any)
	if limits["session_file_bytes"] != float64(relay.MaxSessionFileBytes) || limits["jsonl_line_bytes"] != float64(relay.MaxSessionLineBytes) || limits["json_depth"] != float64(relay.MaxJSONDepth) {
		t.Fatalf("unexpected adapter limits: %+v", limits)
	}
	discovery := responses[1]["result"].(map[string]any)
	if len(discovery["sessions"].([]any)) != 3 {
		t.Fatalf("unexpected discovery response: %+v", discovery)
	}
	exported := responses[3]["result"].(map[string]any)
	if exported["schema"] != "relay.adapter.handoff-preview.v1" {
		t.Fatalf("unexpected export schema: %+v", exported)
	}
	if responses[4]["ok"] != false || responses[4]["error"].(map[string]any)["code"] != "method_not_found" {
		t.Fatalf("unexpected method error: %+v", responses[4])
	}
	if strings.Contains(output.String(), "CODEX_ENCRYPTED_REASONING") {
		t.Fatal("private reasoning leaked through protocol output")
	}
}

func TestJSONLProtocolReturnsErrorsWithoutStoppingTheStream(t *testing.T) {
	input := strings.NewReader("not json\n{\"id\":\"health-after-error\",\"method\":\"health\"}\n")
	var output bytes.Buffer
	if err := NewServer(nil).Serve(input, &output); err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSpace(output.String()), "\n")
	if len(lines) != 2 {
		t.Fatalf("unexpected responses: %q", output.String())
	}
	for _, line := range lines {
		var response map[string]any
		if err := json.Unmarshal([]byte(line), &response); err != nil {
			t.Fatal(err)
		}
	}
}

func TestOversizedProtocolInputErrorIsSafeToPrintOnStderr(t *testing.T) {
	const token = "STDERR_REQUEST_TOKEN"
	input := strings.NewReader(token + strings.Repeat("x", 4*1024*1024))
	var output bytes.Buffer
	err := NewServer(nil).Serve(input, &output)
	if err == nil {
		t.Fatal("expected oversized protocol input to fail")
	}
	printed := "relay-agent-adapter: " + err.Error()
	if strings.Contains(printed, token) || strings.Contains(printed, strings.Repeat("x", 128)) {
		t.Fatalf("stderr-safe error leaked request content: %q", printed)
	}
}

func TestSessionTooLargeHasStableProtocolErrorCode(t *testing.T) {
	if code := classifyError(relay.ErrSessionTooLarge); code != "session_too_large" {
		t.Fatalf("unexpected error code %q", code)
	}
}

func homes(t *testing.T) (string, string) {
	t.Helper()
	_, filename, _, ok := runtime.Caller(0)
	if !ok {
		t.Fatal("runtime.Caller failed")
	}
	root := filepath.Join(filepath.Dir(filename), "..", "..", "testdata")
	return filepath.Join(root, "claude_home"), filepath.Join(root, "codex_home")
}
