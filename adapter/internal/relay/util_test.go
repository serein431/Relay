package relay

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestSanitizedValueOmitsEmbeddedMediaAndLargePayloadStrings(t *testing.T) {
	media := "data:image/png;base64," + strings.Repeat("A", 2*1024*1024)
	value := sanitizedValue([]any{
		map[string]any{"type": "image", "image_url": media, "detail": "original"},
		map[string]any{"type": "audio", "data": strings.Repeat("B", 2048)},
		strings.Repeat("C", maxPreservedPayloadStringBytes+1),
	})
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	text := string(encoded)
	for _, secret := range []string{media, strings.Repeat("B", 2048), strings.Repeat("C", 1024)} {
		if strings.Contains(text, secret) {
			t.Fatal("sanitized value retained an embedded or oversized payload")
		}
	}
	for _, marker := range []string{"image_url_omitted", "data_omitted", "embedded_media", "value_too_large"} {
		if !strings.Contains(text, marker) {
			t.Fatalf("sanitized value is missing %q: %s", marker, text)
		}
	}
}

func TestProjectIdentityGroupsNestedPathsInOneRepository(t *testing.T) {
	repository := t.TempDir()
	if err := os.Mkdir(filepath.Join(repository, ".git"), 0o700); err != nil {
		t.Fatal(err)
	}
	nested := filepath.Join(repository, "packages", "desktop")
	if err := os.MkdirAll(nested, 0o700); err != nil {
		t.Fatal(err)
	}

	rootKey, rootName, rootPath := projectIdentity(repository, "root")
	nestedKey, nestedName, nestedPath := projectIdentity(nested, "nested")
	if rootKey != nestedKey {
		t.Fatalf("same repository produced different keys: %q != %q", rootKey, nestedKey)
	}
	if rootName != filepath.Base(repository) || nestedName != rootName {
		t.Fatalf("unexpected project names: root=%q nested=%q", rootName, nestedName)
	}
	expectedRoot, err := filepath.EvalSymlinks(repository)
	if err != nil {
		t.Fatal(err)
	}
	if rootPath != expectedRoot || nestedPath != expectedRoot {
		t.Fatalf("unexpected project roots: root=%q nested=%q", rootPath, nestedPath)
	}
	if wantPrefix := "git-common-dir:"; len(rootKey) <= len(wantPrefix) || rootKey[:len(wantPrefix)] != wantPrefix {
		t.Fatalf("expected Git project key, got %q", rootKey)
	}
}

func TestProjectIdentityGroupsLinkedWorktreeWithMainRepository(t *testing.T) {
	parent := t.TempDir()
	repository := filepath.Join(parent, "relay")
	commonDir := filepath.Join(repository, ".git")
	worktreeGitDir := filepath.Join(commonDir, "worktrees", "feature")
	linkedWorktree := filepath.Join(parent, "relay-feature")
	for _, directory := range []string{commonDir, worktreeGitDir, linkedWorktree} {
		if err := os.MkdirAll(directory, 0o700); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(linkedWorktree, ".git"), []byte("gitdir: "+worktreeGitDir+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(worktreeGitDir, "commondir"), []byte("../..\n"), 0o600); err != nil {
		t.Fatal(err)
	}

	mainKey, mainName, mainRoot := projectIdentity(repository, "main")
	worktreeKey, worktreeName, worktreeRoot := projectIdentity(linkedWorktree, "feature")
	if mainKey != worktreeKey {
		t.Fatalf("linked worktree did not share project key: %q != %q", mainKey, worktreeKey)
	}
	if mainName != "relay" || worktreeName != "relay" {
		t.Fatalf("expected main repository name, got main=%q worktree=%q", mainName, worktreeName)
	}
	expectedRoot, err := filepath.EvalSymlinks(repository)
	if err != nil {
		t.Fatal(err)
	}
	if mainRoot != expectedRoot || worktreeRoot != expectedRoot {
		t.Fatalf("expected main repository root, got main=%q worktree=%q", mainRoot, worktreeRoot)
	}
}

func TestProjectIdentityFallsBackForNonGitAndMalformedMarkers(t *testing.T) {
	nonGit := t.TempDir()
	key, name, root := projectIdentity(nonGit, "plain")
	if len(key) < len("cwd:") || key[:len("cwd:")] != "cwd:" {
		t.Fatalf("expected cwd fallback, got %q", key)
	}
	if name != filepath.Base(nonGit) {
		t.Fatalf("unexpected fallback name %q", name)
	}
	if root != nonGit {
		t.Fatalf("unexpected fallback root %q", root)
	}

	malformed := t.TempDir()
	if err := os.WriteFile(filepath.Join(malformed, ".git"), []byte("not a gitdir marker\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	malformedKey, _, _ := projectIdentity(malformed, "malformed")
	if len(malformedKey) < len("cwd:") || malformedKey[:len("cwd:")] != "cwd:" {
		t.Fatalf("malformed marker should fall back, got %q", malformedKey)
	}

	unknownKey, unknownName, unknownRoot := projectIdentity("", "session-1")
	if unknownKey != "unknown:session-1" || unknownName != "Unknown project" {
		t.Fatalf("unexpected empty cwd identity: %q %q", unknownKey, unknownName)
	}
	if unknownRoot != "" {
		t.Fatalf("unexpected empty project root %q", unknownRoot)
	}
}

func TestSafeNativeTypeRejectsSecretLikeOrUnboundedValues(t *testing.T) {
	tests := map[string]string{
		"future-record":         "future-record",
		"FutureEvent.V2":        "FutureEvent.V2",
		"sk-secret-value":       "unknown",
		"Authorization:Bearer":  "unknown",
		strings.Repeat("x", 65): "unknown",
		"has whitespace":        "unknown",
		"":                      "unknown",
	}
	for input, want := range tests {
		if got := safeNativeType(input); got != want {
			t.Fatalf("safeNativeType(%q)=%q, want %q", input, got, want)
		}
	}
}

func TestJSONDepthCheckIgnoresBracketsInsideStrings(t *testing.T) {
	if jsonDepthExceeds([]byte(`{"value":"[[[[{{{{","nested":[{"ok":true}]}`), 3) {
		t.Fatal("brackets inside strings were counted as JSON nesting")
	}
	if !jsonDepthExceeds([]byte(`[[[[]]]]`), 3) {
		t.Fatal("deep JSON nesting was not detected")
	}
}
