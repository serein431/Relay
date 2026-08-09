package relay

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

type DiscoverOptions struct {
	Agents     []string `json:"agents,omitempty"`
	ClaudeHome string   `json:"claude_home,omitempty"`
	CodexHome  string   `json:"codex_home,omitempty"`
	Limit      int      `json:"limit,omitempty"`
}

type SessionOptions struct {
	Agent        string `json:"agent"`
	SessionID    string `json:"session_id"`
	ClaudeHome   string `json:"claude_home,omitempty"`
	CodexHome    string `json:"codex_home,omitempty"`
	PreviewLimit int    `json:"preview_limit,omitempty"`
}

type candidate struct {
	agent string
	home  string
	path  string
	mod   time.Time
	name  string
}

func DefaultClaudeHome() (string, error) {
	if configured := strings.TrimSpace(os.Getenv("CLAUDE_CONFIG_DIR")); configured != "" {
		return expandHome(configured)
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(home, ".claude"), nil
}

func DefaultCodexHome() (string, error) {
	if configured := strings.TrimSpace(os.Getenv("CODEX_HOME")); configured != "" {
		return expandHome(configured)
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(home, ".codex"), nil
}

func resolveHomes(claudeHome, codexHome string) (string, string, error) {
	var err error
	if strings.TrimSpace(claudeHome) == "" {
		claudeHome, err = DefaultClaudeHome()
	} else {
		claudeHome, err = expandHome(claudeHome)
	}
	if err != nil {
		return "", "", fmt.Errorf("resolve Claude home: %w", err)
	}
	if strings.TrimSpace(codexHome) == "" {
		codexHome, err = DefaultCodexHome()
	} else {
		codexHome, err = expandHome(codexHome)
	}
	if err != nil {
		return "", "", fmt.Errorf("resolve Codex home: %w", err)
	}
	return claudeHome, codexHome, nil
}

func Discover(options DiscoverOptions, now time.Time) (DiscoverResult, error) {
	claudeHome, codexHome, err := resolveHomes(options.ClaudeHome, options.CodexHome)
	if err != nil {
		return DiscoverResult{}, err
	}
	agents, err := selectedAgents(options.Agents)
	if err != nil {
		return DiscoverResult{}, err
	}

	limit := options.Limit
	if limit <= 0 {
		limit = 200
	}
	if limit > 5000 {
		limit = 5000
	}

	var candidates []candidate
	warnings := make([]Warning, 0)
	if agents[AgentClaude] {
		found, warning := claudeCandidates(claudeHome)
		candidates = append(candidates, found...)
		if warning != nil {
			warnings = append(warnings, *warning)
		}
	}
	if agents[AgentCodex] {
		found, foundWarnings := codexCandidates(codexHome)
		candidates = append(candidates, found...)
		warnings = append(warnings, foundWarnings...)
	}
	sort.SliceStable(candidates, func(i, j int) bool {
		if candidates[i].mod.Equal(candidates[j].mod) {
			return candidates[i].path < candidates[j].path
		}
		return candidates[i].mod.After(candidates[j].mod)
	})
	if len(candidates) > limit {
		candidates = candidates[:limit]
	}

	codexTitles := readCodexTitles(codexHome)
	sessions := make([]SessionSummary, 0, len(candidates))
	for _, item := range candidates {
		parsed, parseErr := parseCandidate(item, codexTitles)
		if parseErr != nil {
			if errors.Is(parseErr, ErrSessionTooLarge) {
				warnings = append(warnings, Warning{
					Code:    "session_too_large",
					Message: "A session file exceeded the configured safety limit",
				})
				continue
			}
			warnings = append(warnings, Warning{
				Code:    "session_parse_failed",
				Message: "A session file could not be parsed safely",
			})
			continue
		}
		sessions = append(sessions, parsed.Summary)
	}

	return DiscoverResult{
		Schema:    ProtocolSchema,
		ScannedAt: now.UTC().Format(time.RFC3339Nano),
		Sessions:  sessions,
		Warnings:  warnings,
	}, nil
}

func Inspect(options SessionOptions) (InspectResult, error) {
	parsed, err := ParseSession(options)
	if err != nil {
		return InspectResult{}, err
	}
	limit := options.PreviewLimit
	if limit <= 0 {
		limit = 40
	}
	if limit > 500 {
		limit = 500
	}
	return parsed.Inspect(limit), nil
}

func Export(options SessionOptions, now time.Time) (Handoff, error) {
	parsed, err := ParseSession(options)
	if err != nil {
		return Handoff{}, err
	}
	return parsed.Handoff(now), nil
}

func ParseSession(options SessionOptions) (ParsedSession, error) {
	if err := validateAgent(options.Agent); err != nil {
		return ParsedSession{}, err
	}
	if err := validateSessionID(options.SessionID); err != nil {
		return ParsedSession{}, err
	}
	claudeHome, codexHome, err := resolveHomes(options.ClaudeHome, options.CodexHome)
	if err != nil {
		return ParsedSession{}, err
	}

	var item candidate
	switch options.Agent {
	case AgentClaude:
		item, err = findClaudeCandidate(claudeHome, options.SessionID)
	case AgentCodex:
		item, err = findCodexCandidate(codexHome, options.SessionID)
	}
	if err != nil {
		return ParsedSession{}, err
	}
	return parseCandidate(item, readCodexTitles(codexHome))
}

func parseCandidate(item candidate, codexTitles map[string]string) (ParsedSession, error) {
	switch item.agent {
	case AgentClaude:
		return parseClaudeSession(item.home, item.path)
	case AgentCodex:
		return parseCodexSession(item.home, item.path, codexTitles)
	default:
		return ParsedSession{}, fmt.Errorf("unsupported agent %q", item.agent)
	}
}

func selectedAgents(requested []string) (map[string]bool, error) {
	if len(requested) == 0 {
		return map[string]bool{AgentClaude: true, AgentCodex: true}, nil
	}
	out := map[string]bool{}
	for _, agent := range requested {
		if agent == "claude" {
			agent = AgentClaude
		}
		if err := validateAgent(agent); err != nil {
			return nil, err
		}
		out[agent] = true
	}
	return out, nil
}

func validateAgent(agent string) error {
	if agent == AgentClaude || agent == AgentCodex {
		return nil
	}
	return fmt.Errorf("agent must be %q or %q", AgentClaude, AgentCodex)
}

func validateSessionID(id string) error {
	id = strings.TrimSpace(id)
	if id == "" {
		return errors.New("session_id is required")
	}
	if len(id) > 256 || id == "." || id == ".." || strings.ContainsAny(id, "/\\") || strings.ContainsRune(id, '\x00') {
		return errors.New("session_id contains invalid path characters")
	}
	return nil
}

func claudeCandidates(home string) ([]candidate, *Warning) {
	root := filepath.Join(home, "projects")
	projects, err := os.ReadDir(root)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return nil, &Warning{Code: "claude_home_missing", Message: "Claude projects directory does not exist: " + root}
		}
		return nil, &Warning{Code: "claude_scan_failed", Message: err.Error()}
	}
	var out []candidate
	for _, project := range projects {
		if !project.IsDir() || project.Type()&os.ModeSymlink != 0 {
			continue
		}
		dir := filepath.Join(root, project.Name())
		entries, err := os.ReadDir(dir)
		if err != nil {
			continue
		}
		for _, entry := range entries {
			if entry.IsDir() || entry.Type()&os.ModeSymlink != 0 || !strings.HasSuffix(strings.ToLower(entry.Name()), ".jsonl") {
				continue
			}
			path := filepath.Join(dir, entry.Name())
			info, err := entry.Info()
			if err != nil || !info.Mode().IsRegular() {
				continue
			}
			out = append(out, candidate{agent: AgentClaude, home: home, path: path, mod: info.ModTime(), name: strings.TrimSuffix(entry.Name(), filepath.Ext(entry.Name()))})
		}
	}
	return out, nil
}

func codexCandidates(home string) ([]candidate, []Warning) {
	var out []candidate
	var warnings []Warning
	for _, name := range []string{"sessions", "archived_sessions"} {
		root := filepath.Join(home, name)
		info, err := os.Stat(root)
		if err != nil {
			if !errors.Is(err, fs.ErrNotExist) {
				warnings = append(warnings, Warning{Code: "codex_scan_failed", Message: err.Error()})
			}
			continue
		}
		if !info.IsDir() {
			continue
		}
		err = filepath.WalkDir(root, func(path string, entry fs.DirEntry, walkErr error) error {
			if walkErr != nil {
				return nil
			}
			if entry.Type()&os.ModeSymlink != 0 {
				if entry.IsDir() {
					return filepath.SkipDir
				}
				return nil
			}
			if entry.IsDir() || !strings.HasSuffix(strings.ToLower(entry.Name()), ".jsonl") {
				return nil
			}
			fileInfo, err := entry.Info()
			if err != nil || !fileInfo.Mode().IsRegular() {
				return nil
			}
			out = append(out, candidate{agent: AgentCodex, home: home, path: path, mod: fileInfo.ModTime(), name: strings.TrimSuffix(entry.Name(), filepath.Ext(entry.Name()))})
			return nil
		})
		if err != nil {
			warnings = append(warnings, Warning{Code: "codex_scan_failed", Message: err.Error()})
		}
	}
	if len(out) == 0 {
		warnings = append(warnings, Warning{Code: "codex_home_missing", Message: "No Codex sessions found under: " + home})
	}
	return out, warnings
}

func findClaudeCandidate(home, sessionID string) (candidate, error) {
	items, _ := claudeCandidates(home)
	return findCandidate(items, sessionID)
}

func findCodexCandidate(home, sessionID string) (candidate, error) {
	items, _ := codexCandidates(home)
	return findCandidate(items, sessionID)
}

func findCandidate(items []candidate, sessionID string) (candidate, error) {
	for _, item := range items {
		if item.name == sessionID || strings.HasSuffix(item.name, "-"+sessionID) {
			return item, nil
		}
	}
	// A renamed or legacy file can still be found by a bounded metadata scan.
	for _, item := range items {
		if fileContainsSessionID(item, sessionID) {
			return item, nil
		}
	}
	return candidate{}, fmt.Errorf("session %q was not found", sessionID)
}

func fileContainsSessionID(item candidate, sessionID string) bool {
	file, err := os.Open(item.path)
	if err != nil {
		return false
	}
	defer file.Close()
	scanner := bufio.NewScanner(file)
	scanner.Buffer(make([]byte, 64*1024), 4*1024*1024)
	for line := 0; line < 64 && scanner.Scan(); line++ {
		var record map[string]any
		if json.Unmarshal(scanner.Bytes(), &record) != nil {
			continue
		}
		if item.agent == AgentClaude && stringValue(record["sessionId"]) == sessionID {
			return true
		}
		if item.agent == AgentCodex {
			identity := ""
			recordType := stringValue(record["type"])
			if recordType == "session_meta" {
				payload := mapValue(record["payload"])
				identity = firstString(payload["id"], payload["session_id"])
			} else if recordType == "" {
				identity = firstString(record["id"], record["session_id"])
			}
			if identity != "" {
				return identity == sessionID
			}
		}
	}
	return false
}

func readCodexTitles(home string) map[string]string {
	out := map[string]string{}
	file, err := os.Open(filepath.Join(home, "session_index.jsonl"))
	if err != nil {
		return out
	}
	defer file.Close()
	scanner := bufio.NewScanner(file)
	scanner.Buffer(make([]byte, 64*1024), 4*1024*1024)
	for scanner.Scan() {
		var record map[string]any
		if json.Unmarshal(scanner.Bytes(), &record) != nil {
			continue
		}
		id := firstString(record["id"], record["thread_id"])
		title := firstString(record["thread_name"], record["title"])
		if id != "" && title != "" {
			out[id] = title
		}
	}
	return out
}
