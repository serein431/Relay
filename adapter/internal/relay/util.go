package relay

import (
	"bufio"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"time"
)

var (
	cwdTagRE      = regexp.MustCompile(`(?s)<cwd>\s*([^<]+?)\s*</cwd>`)
	nativeTypeRE  = regexp.MustCompile(`^[A-Za-z][A-Za-z0-9_.:-]{0,63}$`)
	sensitiveType = regexp.MustCompile(`(?i)^(?:sk-|ghp_|github_pat_|bearer|token|secret|password|api[_-]?key|access[_-]?token|refresh[_-]?token|session[_-]?token|authorization)`)
)

const maxPreservedPayloadStringBytes = 1024 * 1024

func stringValue(v any) string {
	s, _ := v.(string)
	return strings.TrimSpace(s)
}

func boolValue(v any) bool {
	b, _ := v.(bool)
	return b
}

func mapValue(v any) map[string]any {
	m, _ := v.(map[string]any)
	return m
}

func arrayValue(v any) []any {
	a, _ := v.([]any)
	return a
}

func firstString(values ...any) string {
	for _, value := range values {
		if s := stringValue(value); s != "" {
			return s
		}
	}
	return ""
}

func parseTime(value string) (time.Time, bool) {
	if value == "" {
		return time.Time{}, false
	}
	for _, layout := range []string{time.RFC3339Nano, time.RFC3339} {
		if parsed, err := time.Parse(layout, value); err == nil {
			return parsed, true
		}
	}
	return time.Time{}, false
}

func updateTimeBounds(created, updated *time.Time, value string) {
	parsed, ok := parseTime(value)
	if !ok {
		return
	}
	if created.IsZero() || parsed.Before(*created) {
		*created = parsed
	}
	if updated.IsZero() || parsed.After(*updated) {
		*updated = parsed
	}
}

func formatTime(value time.Time) string {
	if value.IsZero() {
		return ""
	}
	return value.UTC().Format(time.RFC3339Nano)
}

func safeTimestamp(value string) string {
	parsed, ok := parseTime(value)
	if !ok {
		return ""
	}
	return formatTime(parsed)
}

func projectIdentity(cwd, sessionID string) (string, string, string) {
	cleaned := strings.TrimSpace(cwd)
	if cleaned == "" {
		return "unknown:" + sessionID, "Unknown project", ""
	}
	cleaned = filepath.Clean(cleaned)
	if commonDir, projectName, projectRoot, ok := gitProjectIdentity(cleaned); ok {
		return hashedProjectKey("git-common-dir", commonDir), projectName, projectRoot
	}
	return hashedProjectKey("cwd", cleaned), projectDisplayName(cleaned), cleaned
}

func hashedProjectKey(namespace, value string) string {
	digest := sha256.Sum256([]byte(value))
	return namespace + ":" + hex.EncodeToString(digest[:8])
}

func projectDisplayName(path string) string {
	name := filepath.Base(path)
	if name == "." || name == string(filepath.Separator) || name == "" {
		name = path
	}
	return name
}

func gitProjectIdentity(cwd string) (string, string, string, bool) {
	absolute, err := filepath.Abs(cwd)
	if err != nil {
		return "", "", "", false
	}
	resolved, err := filepath.EvalSymlinks(absolute)
	if err != nil {
		return "", "", "", false
	}
	info, err := os.Stat(resolved)
	if err != nil || !info.IsDir() {
		return "", "", "", false
	}

	for directory := resolved; ; directory = filepath.Dir(directory) {
		marker := filepath.Join(directory, ".git")
		commonDir, ok := resolveGitCommonDir(marker)
		if ok {
			projectName := projectDisplayName(directory)
			projectRoot := directory
			if filepath.Base(commonDir) == ".git" {
				projectRoot = filepath.Dir(commonDir)
				projectName = projectDisplayName(projectRoot)
			}
			return commonDir, projectName, projectRoot, true
		}
		parent := filepath.Dir(directory)
		if parent == directory {
			break
		}
	}
	return "", "", "", false
}

func resolveGitCommonDir(marker string) (string, bool) {
	info, err := os.Lstat(marker)
	if err != nil {
		return "", false
	}
	if info.IsDir() {
		resolved, err := filepath.EvalSymlinks(marker)
		return resolved, err == nil
	}
	if !info.Mode().IsRegular() {
		return "", false
	}
	markerText, err := readSmallTextFile(marker, 4096)
	if err != nil {
		return "", false
	}
	gitDirText := strings.TrimSpace(strings.TrimPrefix(markerText, "gitdir:"))
	if gitDirText == markerText || gitDirText == "" {
		return "", false
	}
	gitDir := gitDirText
	if !filepath.IsAbs(gitDir) {
		gitDir = filepath.Join(filepath.Dir(marker), gitDir)
	}
	gitDir, err = filepath.EvalSymlinks(filepath.Clean(gitDir))
	if err != nil {
		return "", false
	}
	gitDirInfo, err := os.Stat(gitDir)
	if err != nil || !gitDirInfo.IsDir() {
		return "", false
	}

	commonMarker := filepath.Join(gitDir, "commondir")
	commonInfo, err := os.Lstat(commonMarker)
	if os.IsNotExist(err) {
		return gitDir, true
	}
	if err != nil || !commonInfo.Mode().IsRegular() {
		return "", false
	}
	commonText, err := readSmallTextFile(commonMarker, 4096)
	if err != nil {
		return "", false
	}
	commonDir := strings.TrimSpace(commonText)
	if commonDir == "" {
		return "", false
	}
	if !filepath.IsAbs(commonDir) {
		commonDir = filepath.Join(gitDir, commonDir)
	}
	commonDir, err = filepath.EvalSymlinks(filepath.Clean(commonDir))
	if err != nil {
		return "", false
	}
	commonDirInfo, err := os.Stat(commonDir)
	if err != nil || !commonDirInfo.IsDir() {
		return "", false
	}
	return commonDir, true
}

func readSmallTextFile(path string, maxBytes int64) (string, error) {
	file, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer file.Close()
	reader := bufio.NewReader(io.LimitReader(file, maxBytes+1))
	bytes, err := io.ReadAll(reader)
	if err != nil {
		return "", err
	}
	if int64(len(bytes)) > maxBytes {
		return "", fmt.Errorf("file exceeds %d bytes", maxBytes)
	}
	return string(bytes), nil
}

func titleFromText(text string) string {
	text = strings.TrimSpace(text)
	if text == "" {
		return ""
	}
	if len([]rune(text)) > 96 {
		runes := []rune(text)
		text = string(runes[:93]) + "..."
	}
	return strings.Join(strings.Fields(text), " ")
}

// previewFromMessages returns the most recent user-visible text from the
// conversation. It never uses tool input/output, unsupported-record summaries,
// provider-only data, or hidden reasoning.
func previewFromMessages(messages []Message) string {
	for index := len(messages) - 1; index >= 0; index-- {
		message := messages[index]
		if message.Role != "user" && message.Role != "assistant" {
			continue
		}
		for blockIndex := len(message.Blocks) - 1; blockIndex >= 0; blockIndex-- {
			block := message.Blocks[blockIndex]
			if block.Kind != "text" || providerInternalText(block.Text) {
				continue
			}
			if preview := previewFromText(block.Text); preview != "" {
				return preview
			}
		}
	}
	return ""
}

func previewFromText(text string) string {
	text = strings.Join(strings.Fields(text), " ")
	if text == "" {
		return ""
	}
	runes := []rune(text)
	if len(runes) > 160 {
		return string(runes[:157]) + "..."
	}
	return text
}

func providerInternalText(text string) bool {
	trimmed := strings.TrimSpace(text)
	for _, prefix := range []string{
		"<environment_context>",
		"<recommended_plugins>",
		"<in-app-browser-context",
		"<codex_internal_context",
		"<local-command-caveat>",
		"<local-command-stdout>",
		"<local-command-stderr>",
		"<command-name>",
		"<system-reminder>",
		"# AGENTS.md instructions",
		"<INSTRUCTIONS>",
		"<skill>",
		"<turn_aborted>",
	} {
		if strings.HasPrefix(trimmed, prefix) {
			return true
		}
	}
	return false
}

func safeNativeType(value string) string {
	value = strings.TrimSpace(value)
	if !nativeTypeRE.MatchString(value) || sensitiveType.MatchString(value) {
		return "unknown"
	}
	return value
}

func unsupportedBlock(nativeType, summary string) Block {
	if strings.TrimSpace(summary) == "" {
		summary = "An unsupported historical record was present. Its raw payload was not exported."
	}
	safeType := safeNativeType(nativeType)
	return Block{
		Kind:           "unsupported",
		Classification: "user_visible",
		Mapping:        &BlockMapping{Status: "unmapped", SourceType: safeType},
		SafeSummary:    summary,
		NativeType:     safeType,
	}
}

func cwdFromEnvironmentText(text string) string {
	match := cwdTagRE.FindStringSubmatch(text)
	if len(match) != 2 {
		return ""
	}
	return strings.TrimSpace(match[1])
}

// sanitizedValue deliberately removes encrypted/provider-only fields from
// otherwise user-visible tool input and output. Binary base64 bodies are also
// omitted; the desktop packaging layer can collect assets separately.
func sanitizedValue(v any) any {
	return sanitizedValueAt(v, 0)
}

func sanitizedValueAt(v any, depth int) any {
	if depth > MaxJSONDepth {
		return map[string]any{"omitted": true, "reason": "json_depth_exceeded"}
	}
	switch value := v.(type) {
	case map[string]any:
		out := make(map[string]any, len(value))
		valueType := strings.ToLower(stringValue(value["type"]))
		isBase64 := valueType == "base64"
		isEmbeddedMedia := isBase64 || valueType == "image" || valueType == "audio" || valueType == "input_image" || valueType == "input_audio"
		for key, child := range value {
			switch strings.ToLower(key) {
			case "encrypted_content", "encryptedcontent", "internal_chat_message_metadata_passthrough", "reasoning_content", "thinking":
				continue
			case "data":
				if isEmbeddedMedia {
					out["data_omitted"] = omittedPayloadValue("embedded_media", valueBytes(child))
					continue
				}
			case "image_url", "audio_url":
				if text, ok := child.(string); ok && isDataURL(text) {
					out[key+"_omitted"] = omittedPayloadValue("embedded_media", len([]byte(text)))
					continue
				}
			}
			out[key] = sanitizedValueAt(child, depth+1)
		}
		return out
	case []any:
		out := make([]any, 0, len(value))
		for _, child := range value {
			out = append(out, sanitizedValueAt(child, depth+1))
		}
		return out
	case string:
		if len([]byte(value)) > maxPreservedPayloadStringBytes {
			return omittedPayloadValue("value_too_large", len([]byte(value)))
		}
		return value
	default:
		return value
	}
}

func isDataURL(value string) bool {
	return strings.HasPrefix(strings.ToLower(strings.TrimSpace(value)), "data:")
}

func valueBytes(value any) int {
	switch typed := value.(type) {
	case string:
		return len([]byte(typed))
	case []byte:
		return len(typed)
	default:
		return 0
	}
}

func omittedPayloadValue(reason string, bytes int) map[string]any {
	return map[string]any{
		"omitted": true,
		"reason":  reason,
		"bytes":   bytes,
	}
}

func jsonArgument(v any) (any, bool) {
	text, ok := v.(string)
	if !ok {
		return sanitizedValue(v), false
	}
	trimmed := strings.TrimSpace(text)
	if trimmed == "" {
		return "", false
	}
	if jsonDepthExceeds([]byte(trimmed), MaxJSONDepth) {
		return map[string]any{"omitted": true, "reason": "json_depth_exceeded"}, true
	}
	var parsed any
	if json.Unmarshal([]byte(trimmed), &parsed) == nil {
		return sanitizedValue(parsed), false
	}
	return text, false
}

func relativeOrAbsolute(root, path string) string {
	rel, err := filepath.Rel(root, path)
	if err != nil || strings.HasPrefix(rel, "..") {
		return path
	}
	return filepath.ToSlash(rel)
}

func expandHome(path string) (string, error) {
	path = strings.TrimSpace(path)
	if path == "" || path == "~" || strings.HasPrefix(path, "~/") {
		home, err := os.UserHomeDir()
		if err != nil {
			return "", err
		}
		if path == "" || path == "~" {
			return home, nil
		}
		return filepath.Join(home, strings.TrimPrefix(path, "~/")), nil
	}
	if strings.HasPrefix(path, "~") {
		return "", fmt.Errorf("another user's home shortcut is not supported")
	}
	return filepath.Abs(path)
}

func makeMessageID(agent, sessionID string, line int, nativeID string) string {
	if nativeID != "" {
		return nativeID
	}
	return fmt.Sprintf("%s:%s:%d", agent, sessionID, line)
}

func markCompleteness(stats *Completeness, messageCount int) {
	switch {
	case messageCount == 0:
		stats.Status = "metadata_only"
	case stats.DamagedLines > 0 || stats.UnknownRecords > 0 || stats.UnsupportedBlocks > 0 || stats.OrphanToolResults > 0 || stats.UnmatchedToolCalls > 0:
		stats.Status = "partial"
	default:
		stats.Status = "complete"
	}
}
