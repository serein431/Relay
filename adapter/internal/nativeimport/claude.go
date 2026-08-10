package nativeimport

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"time"

	"github.com/google/uuid"
)

type claudeProjectIndex struct {
	Version      int              `json:"version"`
	Entries      []map[string]any `json:"entries"`
	OriginalPath string           `json:"originalPath"`
}

var (
	updateClaudeIndexForImport  = updateClaudeIndex
	writeClaudeSessionForImport = writeFileExclusive
)

func importClaude(loaded loadedRequest) (Result, *ImportError) {
	now := time.Now().UTC()
	sessionID, sessionPath, indexPath, allocationErr := allocateClaudeTarget(loaded.home, loaded.targetCWD)
	if allocationErr != nil {
		return Result{}, fail("session_id_failed", "cannot allocate a new Claude Code session: %v", allocationErr)
	}
	title := importedTitle(loaded.transcript.Title, now, sessionID)
	indexExisted := regularFileExists(indexPath)
	createdFiles := []string{sessionPath}
	if !indexExisted {
		createdFiles = append(createdFiles, indexPath)
	}
	result := Result{
		Status: "planned", Target: "claude_code", SessionID: sessionID, Title: title,
		TargetHome: loaded.home, TargetCWD: loaded.targetCWD, SessionPath: sessionPath,
		Writes: []string{sessionPath, indexPath}, CreatedFiles: createdFiles,
		DryRun:          !loaded.request.Execute,
		ContinueCommand: "claude --resume " + sessionID,
	}
	if !loaded.request.Execute {
		return result, nil
	}
	backupDir, backupErr := backupFiles(loaded.home, "claude", sessionID, []string{indexPath})
	if backupErr != nil {
		return Result{}, failAfter("backup_failed", backupDir, nil, "cannot back up Claude Code state: %v", backupErr)
	}
	result.BackupDir = backupDir
	steps := []string{"backup_created"}
	sessionBytes, buildErr := synthesizeClaudeSession(loaded.transcript, sessionID, title, loaded.targetCWD, now)
	if buildErr != nil {
		return Result{}, failAfter("session_build_failed", backupDir, steps, "cannot build Claude Code history: %v", buildErr)
	}
	if err := writeClaudeSessionForImport(sessionPath, sessionBytes, 0o600); err != nil {
		return Result{}, failAfter("session_write_failed", backupDir, steps, "cannot write Claude Code history: %v", err)
	}
	steps = append(steps, "session_created")
	if err := updateClaudeIndexForImport(indexPath, loaded.targetCWD, sessionID, sessionPath, title, sessionBytes, now); err != nil {
		if rollbackErr := rollbackClaudeImport(sessionPath, indexPath, indexExisted, sessionID); rollbackErr != nil {
			steps = append(steps, "rollback_incomplete")
			return Result{}, failAfter("rollback_incomplete", backupDir, steps, "cannot update the Claude Code session index: %v; automatic rollback also failed: %v", err, rollbackErr)
		}
		steps = append(steps, "session_and_partial_index_rolled_back")
		return Result{}, failAfter("index_write_failed", backupDir, steps, "cannot update the Claude Code session index: %v", err)
	}
	steps = append(steps, "index_updated")
	verification, verifyErr := verifyClaudeImport(sessionPath, indexPath, sessionID)
	if verifyErr != nil {
		if rollbackErr := rollbackClaudeImport(sessionPath, indexPath, indexExisted, sessionID); rollbackErr != nil {
			steps = append(steps, "rollback_incomplete")
			return Result{}, failAfter("rollback_incomplete", backupDir, steps, "cannot verify the imported Claude Code session: %v; automatic rollback also failed: %v", verifyErr, rollbackErr)
		}
		steps = append(steps, "verification_failed_and_changes_rolled_back")
		return Result{}, failAfter("native_import_unverified", backupDir, steps, "cannot verify the imported Claude Code session: %v", verifyErr)
	}
	result.Status = "ok"
	result.DryRun = false
	result.Verification = verification
	return result, nil
}

func synthesizeClaudeSession(source transcript, sessionID, title, cwd string, now time.Time) ([]byte, error) {
	items := []map[string]any{
		{"type": "queue-operation", "operation": "enqueue", "timestamp": now.Format(time.RFC3339Nano), "sessionId": sessionID},
		{"type": "queue-operation", "operation": "dequeue", "timestamp": now.Format(time.RFC3339Nano), "sessionId": sessionID},
		{"type": "custom-title", "customTitle": title, "timestamp": now.Format(time.RFC3339Nano), "sessionId": sessionID},
	}
	parent := ""
	for index, item := range source.Entries {
		id := uuid.NewString()
		record := map[string]any{
			"parentUuid": parent, "isSidechain": false, "uuid": id,
			"timestamp": timestampOr(item.Timestamp, now.Add(time.Duration(index+1)*time.Millisecond)),
			"userType":  "external", "entrypoint": "relay", "cwd": cwd,
			"sessionId": sessionID, "version": "relay-0.1.0", "gitBranch": currentGitBranch(cwd),
		}
		switch item.Kind {
		case "context":
			record["type"] = "user"
			record["message"] = map[string]any{"role": "user", "content": item.Text}
		case "message":
			role := item.Role
			if role != "assistant" {
				role = "user"
			}
			record["type"] = role
			if role == "assistant" {
				record["message"] = map[string]any{
					"role": "assistant", "model": "<relay-import>",
					"content": []map[string]string{{"type": "text", "text": item.Text}},
					"usage":   map[string]any{},
				}
			} else {
				record["message"] = map[string]any{"role": "user", "content": item.Text}
			}
		case "tool_call":
			record["type"] = "assistant"
			record["message"] = map[string]any{
				"role": "assistant", "model": "<relay-import>",
				"content": []map[string]string{{"type": "text", "text": claudeToolCallEvidence(item)}},
				"usage":   map[string]any{},
			}
		case "tool_result":
			record["type"] = "assistant"
			record["message"] = map[string]any{
				"role": "assistant", "model": "<relay-import>",
				"content": []map[string]string{{"type": "text", "text": claudeToolResultEvidence(item)}},
				"usage":   map[string]any{},
			}
		default:
			continue
		}
		items = append(items, record)
		parent = id
	}
	items = append(items, map[string]any{
		"type": "last-prompt", "lastPrompt": previewText(source),
		"leafUuid": parent, "sessionId": sessionID,
	})
	return jsonLines(items)
}

func claudeToolCallEvidence(item entry) string {
	var parts []string
	parts = append(parts, fmt.Sprintf("[Relay 历史工具调用：%s]", strings.TrimSpace(item.Tool)))
	if item.Status != "" {
		parts = append(parts, "状态："+item.Status)
	}
	if item.Input != "" {
		parts = append(parts, "输入："+item.Input)
	}
	parts = append(parts, "[这只是历史记录，不得自动重新执行。]")
	return strings.Join(parts, "\n")
}

func claudeToolResultEvidence(item entry) string {
	label := strings.TrimSpace(item.Tool)
	if label == "" {
		label = "工具结果"
	}
	parts := []string{fmt.Sprintf("[Relay 历史工具结果：%s]", label)}
	if item.Status != "" {
		parts = append(parts, "状态："+item.Status)
	}
	if item.Output != "" {
		parts = append(parts, "结果："+item.Output)
	}
	parts = append(parts, "[这只是历史记录，不得自动重新执行。]")
	return strings.Join(parts, "\n")
}

func allocateClaudeTarget(home, cwd string) (string, string, string, error) {
	projectDirectory := filepath.Join(home, "projects", claudeProjectDirName(cwd))
	indexPath := filepath.Join(projectDirectory, "sessions-index.json")
	for range 10 {
		id := uuid.NewString()
		sessionPath := filepath.Join(projectDirectory, id+".jsonl")
		if regularFileExists(sessionPath) {
			continue
		}
		found, err := claudeIndexContains(indexPath, id)
		if err != nil {
			return "", "", "", err
		}
		if found {
			continue
		}
		return id, sessionPath, indexPath, nil
	}
	return "", "", "", errors.New("could not allocate a unique Claude Code session id")
}

func readClaudeIndex(path string) (claudeProjectIndex, error) {
	content, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return claudeProjectIndex{Version: 1}, nil
	}
	if err != nil {
		return claudeProjectIndex{}, err
	}
	return decodeClaudeIndex(content)
}

func decodeClaudeIndex(content []byte) (claudeProjectIndex, error) {
	index := claudeProjectIndex{Version: 1}
	if err := json.Unmarshal(content, &index); err != nil {
		return index, err
	}
	if index.Version == 0 {
		index.Version = 1
	}
	return index, nil
}

func claudeIndexContains(path, sessionID string) (bool, error) {
	index, err := readClaudeIndex(path)
	if err != nil {
		return false, err
	}
	for _, item := range index.Entries {
		if fmt.Sprint(item["sessionId"]) == sessionID {
			return true, nil
		}
	}
	return false, nil
}

func updateClaudeIndex(path, cwd, sessionID, sessionPath, title string, session []byte, now time.Time) error {
	for range 3 {
		original, err := os.ReadFile(path)
		existed := err == nil
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
		index := claudeProjectIndex{Version: 1}
		if existed {
			index, err = decodeClaudeIndex(original)
			if err != nil {
				return err
			}
		}
		index.OriginalPath = cwd
		entry := map[string]any{
			"sessionId": sessionID, "fullPath": sessionPath, "fileMtime": now.UnixMilli(),
			"firstPrompt": clip(title, 240), "messageCount": countClaudeMessages(session),
			"created": now.Format(time.RFC3339Nano), "modified": now.Format(time.RFC3339Nano),
			"gitBranch": currentGitBranch(cwd), "projectPath": cwd, "isSidechain": false,
		}
		index.Entries = append([]map[string]any{entry}, index.Entries...)
		payload, err := json.MarshalIndent(index, "", "  ")
		if err != nil {
			return err
		}
		content := append(payload, '\n')
		if !existed {
			if err := writeFileExclusive(path, content, 0o600); errors.Is(err, os.ErrExist) {
				continue
			} else if err != nil {
				return err
			}
			return nil
		}
		info, err := os.Stat(path)
		if err != nil {
			return err
		}
		mode := info.Mode().Perm()
		if mode == 0 {
			mode = 0o600
		}
		if err := writeFileAtomicIfUnchanged(path, original, content, mode); errors.Is(err, errFileChanged) {
			continue
		} else if err != nil {
			return err
		}
		return nil
	}
	return errors.New("Claude Code session index kept changing during import")
}

func removeClaudeIndexEntry(path, sessionID string, existedBefore bool) error {
	for range 3 {
		original, err := os.ReadFile(path)
		if errors.Is(err, os.ErrNotExist) {
			return nil
		}
		if err != nil {
			return err
		}
		index, err := decodeClaudeIndex(original)
		if err != nil {
			return err
		}
		filtered := index.Entries[:0]
		for _, item := range index.Entries {
			if fmt.Sprint(item["sessionId"]) != sessionID {
				filtered = append(filtered, item)
			}
		}
		index.Entries = filtered
		if !existedBefore && len(filtered) == 0 {
			if err := removeFileIfUnchanged(path, original); errors.Is(err, errFileChanged) {
				continue
			} else if err != nil && !errors.Is(err, os.ErrNotExist) {
				return err
			}
			return nil
		}
		payload, err := json.MarshalIndent(index, "", "  ")
		if err != nil {
			return err
		}
		info, err := os.Stat(path)
		if err != nil {
			return err
		}
		mode := info.Mode().Perm()
		if mode == 0 {
			mode = 0o600
		}
		if err := writeFileAtomicIfUnchanged(path, original, append(payload, '\n'), mode); errors.Is(err, errFileChanged) {
			continue
		} else if err != nil {
			return err
		}
		return nil
	}
	return errors.New("Claude Code session index kept changing during rollback")
}

func rollbackClaudeImport(sessionPath, indexPath string, indexExisted bool, sessionID string) error {
	var rollbackErrors []error
	if err := removeClaudeIndexEntry(indexPath, sessionID, indexExisted); err != nil {
		rollbackErrors = append(rollbackErrors, fmt.Errorf("remove session index entry: %w", err))
	}
	if err := os.Remove(sessionPath); err != nil && !errors.Is(err, os.ErrNotExist) {
		rollbackErrors = append(rollbackErrors, fmt.Errorf("remove session file: %w", err))
	}
	return errors.Join(rollbackErrors...)
}

func verifyClaudeImport(sessionPath, indexPath, sessionID string) (Verification, error) {
	verification := Verification{SessionFile: regularFileExists(sessionPath)}
	var err error
	verification.Index, err = claudeIndexContains(indexPath, sessionID)
	if err != nil {
		return verification, err
	}
	if !verification.SessionFile || !verification.Index {
		return verification, errors.New("one or more Claude Code session records are missing")
	}
	return verification, nil
}

func countClaudeMessages(content []byte) int {
	count := 0
	for _, line := range strings.Split(string(content), "\n") {
		var item map[string]any
		if json.Unmarshal([]byte(line), &item) != nil {
			continue
		}
		if item["type"] == "user" || item["type"] == "assistant" {
			count++
		}
	}
	return count
}

var claudeProjectCharacters = regexp.MustCompile(`[^A-Za-z0-9_-]`)

func claudeProjectDirName(cwd string) string {
	abs, err := filepath.Abs(cwd)
	if err == nil {
		cwd = abs
	}
	return claudeProjectCharacters.ReplaceAllString(cwd, "-")
}

func currentGitBranch(cwd string) string {
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	command := exec.CommandContext(ctx, "git", "-C", cwd, "symbolic-ref", "--quiet", "--short", "HEAD")
	command.Env = append(os.Environ(), "GIT_OPTIONAL_LOCKS=0", "GIT_TERMINAL_PROMPT=0")
	output, err := command.Output()
	if err != nil || ctx.Err() != nil {
		return ""
	}
	return strings.TrimSpace(string(output))
}
