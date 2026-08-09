package relay

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"time"
)

func parseClaudeSession(home, path string) (ParsedSession, error) {
	file, err := os.Open(path)
	if err != nil {
		return ParsedSession{}, err
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		return ParsedSession{}, err
	}
	if err := validateSessionFileSize(info); err != nil {
		return ParsedSession{}, err
	}

	parsed := ParsedSession{}
	stats := &parsed.Completeness
	var created, updated time.Time
	var firstUserText, customTitle, aiTitle string
	sessionID := strings.TrimSuffix(filepath.Base(path), filepath.Ext(path))
	cwd := ""
	nativeVersion := ""
	pendingCalls := map[string]bool{}

	err = scanBoundedLines(file, func(scanned boundedLine) {
		stats.TotalLines++
		lineNumber := scanned.Number
		if scanned.TooLarge {
			stats.DamagedLines++
			parsed.Warnings = append(parsed.Warnings, Warning{Code: "line_too_large", Message: "Claude record exceeded the per-line safety limit", Line: lineNumber})
			return
		}
		if !scanned.Terminated {
			stats.DamagedLines++
			parsed.Warnings = append(parsed.Warnings, Warning{Code: "truncated_final_line", Message: "Claude session ended with an incomplete JSONL record", Line: lineNumber})
			return
		}
		line := bytes.TrimSpace(scanned.Data)
		if len(line) == 0 {
			return
		}
		if jsonDepthExceeds(line, MaxJSONDepth) {
			stats.DamagedLines++
			parsed.Warnings = append(parsed.Warnings, Warning{Code: "json_too_deep", Message: "Claude record exceeded the JSON nesting safety limit", Line: lineNumber})
			return
		}
		var record map[string]any
		if err := json.Unmarshal(line, &record); err != nil {
			stats.DamagedLines++
			parsed.Warnings = append(parsed.Warnings, Warning{Code: "invalid_json", Message: "Claude record is not valid JSON", Line: lineNumber})
			return
		}
		stats.ParsedLines++
		recordType := stringValue(record["type"])
		timestamp := safeTimestamp(stringValue(record["timestamp"]))

		switch recordType {
		case "custom-title":
			sessionID = firstString(record["sessionId"], sessionID)
			customTitle = stringValue(record["customTitle"])
			stats.HiddenRecords++
		case "ai-title":
			sessionID = firstString(record["sessionId"], sessionID)
			aiTitle = stringValue(record["aiTitle"])
			stats.HiddenRecords++
		case "user", "assistant":
			if boolValue(record["isMeta"]) || boolValue(record["isSidechain"]) {
				stats.HiddenRecords++
				return
			}
			message := mapValue(record["message"])
			role := firstString(message["role"], recordType)
			if role != "user" && role != "assistant" {
				parsed.Warnings = append(parsed.Warnings, Warning{Code: "unsupported_role", Message: "Claude message role was preserved as an unsupported summary", Line: lineNumber, RecordType: safeNativeType(role)})
				stats.UnsupportedBlocks++
				parsed.Messages = append(parsed.Messages, Message{
					ID:   makeMessageID(AgentClaude, sessionID, lineNumber, ""),
					Role: "system",
					Blocks: []Block{unsupportedBlock("message_role:"+safeNativeType(role),
						"A Claude message with an unsupported role was present. Its raw payload was not exported.")},
				})
				return
			}
			blocks, hidden := claudeBlocks(message["content"], pendingCalls, stats, &parsed.Warnings, lineNumber)
			if hidden || len(blocks) == 0 {
				stats.HiddenRecords++
				return
			}
			updateClaudeVisibleMetadata(record, timestamp, &sessionID, &cwd, &nativeVersion, &created, &updated)
			if role == "user" && firstUserText == "" {
				for _, block := range blocks {
					if block.Kind == "text" && !providerInternalText(block.Text) {
						firstUserText = block.Text
						break
					}
				}
			}
			parsed.Messages = append(parsed.Messages, Message{
				ID:        makeMessageID(AgentClaude, sessionID, lineNumber, stringValue(record["uuid"])),
				ParentID:  stringValue(record["parentUuid"]),
				BranchID:  stringValue(record["branchId"]),
				Timestamp: timestamp,
				Role:      role,
				Blocks:    blocks,
			})
		case "attachment":
			if boolValue(record["isSidechain"]) {
				stats.HiddenRecords++
				return
			}
			attachment := mapValue(record["attachment"])
			nativeType := firstString(attachment["type"], "attachment")
			if !claudeProjectAttachment(nativeType) {
				stats.HiddenRecords++
				return
			}
			updateClaudeVisibleMetadata(record, timestamp, &sessionID, &cwd, &nativeVersion, &created, &updated)
			block := Block{
				Kind:           "source_context",
				Classification: "project_owned",
				NativeType:     nativeType,
				Source:         sanitizedValue(attachment),
			}
			parsed.Messages = append(parsed.Messages, Message{
				ID:        makeMessageID(AgentClaude, sessionID, lineNumber, stringValue(record["uuid"])),
				ParentID:  stringValue(record["parentUuid"]),
				Timestamp: timestamp,
				Role:      "system",
				Blocks:    []Block{block},
			})
		case "file-history-snapshot":
			stats.HiddenRecords++
		case "mode", "permission-mode", "last-prompt", "system", "queue-operation":
			stats.HiddenRecords++
		case "":
			stats.UnknownRecords++
			parsed.Warnings = append(parsed.Warnings, Warning{Code: "unknown_record", Message: "Claude record without a type was preserved as an unsupported summary", Line: lineNumber})
			parsed.Messages = append(parsed.Messages, claudeUnsupportedRecord(sessionID, lineNumber, "unknown"))
		default:
			stats.UnknownRecords++
			safeType := safeNativeType(recordType)
			parsed.Warnings = append(parsed.Warnings, Warning{Code: "unknown_record", Message: "Claude record type was preserved as an unsupported summary", Line: lineNumber, RecordType: safeType})
			parsed.Messages = append(parsed.Messages, claudeUnsupportedRecord(sessionID, lineNumber, safeType))
		}
	})
	if err != nil {
		return ParsedSession{}, err
	}
	for callID := range pendingCalls {
		if pendingCalls[callID] {
			stats.UnmatchedToolCalls++
		}
	}
	if stats.UnmatchedToolCalls > 0 {
		parsed.Warnings = append(parsed.Warnings, Warning{Code: "unmatched_tool_calls", Message: "Claude session ended with tool calls that have no result"})
	}

	title := firstString(customTitle, aiTitle, titleFromText(firstUserText), sessionID)
	projectKey, projectName, projectRoot := projectIdentity(cwd, sessionID)
	messageCount, toolCalls, toolResults := countBlocks(parsed.Messages)
	markCompleteness(stats, messageCount)
	parsed.Summary = SessionSummary{
		Agent:           AgentClaude,
		SessionID:       sessionID,
		Title:           title,
		Preview:         previewFromMessages(parsed.Messages),
		CWD:             cwd,
		ProjectKey:      projectKey,
		ProjectName:     projectName,
		ProjectRoot:     projectRoot,
		CreatedAt:       formatTime(created),
		UpdatedAt:       formatTime(updated),
		NativeVersion:   nativeVersion,
		SourcePath:      path,
		SizeBytes:       info.Size(),
		MessageCount:    messageCount,
		ToolCallCount:   toolCalls,
		ToolResultCount: toolResults,
		WarningCount:    len(parsed.Warnings),
		Completeness:    stats.Status,
	}
	return parsed, nil
}

func updateClaudeVisibleMetadata(record map[string]any, timestamp string, sessionID, cwd, nativeVersion *string, created, updated *time.Time) {
	*sessionID = firstString(record["sessionId"], *sessionID)
	*cwd = firstString(*cwd, record["cwd"])
	*nativeVersion = firstString(*nativeVersion, record["version"])
	updateTimeBounds(created, updated, timestamp)
}

func claudeUnsupportedRecord(sessionID string, line int, nativeType string) Message {
	return Message{
		ID:   makeMessageID(AgentClaude, sessionID, line, ""),
		Role: "system",
		Blocks: []Block{unsupportedBlock(nativeType,
			"An unsupported Claude historical record was present. Its raw payload was not exported.")},
	}
}

func claudeProjectAttachment(nativeType string) bool {
	switch nativeType {
	case "edited_text_file", "file", "image", "document", "user_attachment":
		return true
	default:
		return false
	}
}

func claudeBlocks(content any, pending map[string]bool, stats *Completeness, warnings *[]Warning, line int) ([]Block, bool) {
	if text, ok := content.(string); ok {
		if providerInternalText(text) {
			return nil, true
		}
		if strings.TrimSpace(text) == "" {
			return nil, true
		}
		return []Block{{Kind: "text", Classification: "user_visible", Text: text}}, false
	}
	items, ok := content.([]any)
	if !ok {
		*warnings = append(*warnings, Warning{Code: "unsupported_content", Message: "Claude message content was preserved as an unsupported summary", Line: line})
		stats.UnsupportedBlocks++
		return []Block{unsupportedBlock("message_content", "Unsupported Claude message content was present. Its raw payload was not exported.")}, false
	}
	blocks := make([]Block, 0, len(items))
	for _, item := range items {
		block := mapValue(item)
		kind := stringValue(block["type"])
		switch kind {
		case "text", "input_text", "output_text":
			text := firstString(block["text"], block["content"])
			if text == "" || providerInternalText(text) {
				continue
			}
			blocks = append(blocks, Block{Kind: "text", Classification: "user_visible", Text: text})
		case "tool_use":
			callID := stringValue(block["id"])
			if callID != "" {
				pending[callID] = true
			}
			blocks = append(blocks, Block{
				Kind:           "tool_call",
				Classification: "user_visible",
				CallID:         callID,
				Name:           stringValue(block["name"]),
				Input:          sanitizedValue(block["input"]),
				NativeType:     kind,
				ReplayPolicy:   "never",
			})
		case "tool_result":
			callID := stringValue(block["tool_use_id"])
			if callID == "" || !pending[callID] {
				stats.OrphanToolResults++
				*warnings = append(*warnings, Warning{Code: "orphan_tool_result", Message: "Claude tool result has no matching call", Line: line})
			} else {
				pending[callID] = false
			}
			isError := boolValue(block["is_error"])
			blocks = append(blocks, Block{
				Kind:           "tool_result",
				Classification: "user_visible",
				CallID:         callID,
				Output:         sanitizedValue(block["content"]),
				IsError:        &isError,
				NativeType:     kind,
				ReplayPolicy:   "never",
			})
		case "image", "document":
			blocks = append(blocks, Block{Kind: "asset_ref", Classification: "user_visible", NativeType: kind, Source: sanitizedValue(block)})
		case "thinking", "redacted_thinking":
			stats.HiddenRecords++
		case "":
			stats.UnsupportedBlocks++
			*warnings = append(*warnings, Warning{Code: "unsupported_block", Message: "Claude content block without a type was preserved as an unsupported summary", Line: line})
			blocks = append(blocks, unsupportedBlock("unknown", "An unsupported Claude content block was present. Its raw payload was not exported."))
		default:
			stats.UnsupportedBlocks++
			safeType := safeNativeType(kind)
			*warnings = append(*warnings, Warning{Code: "unsupported_block", Message: "Claude content block was preserved as an unsupported summary", Line: line, RecordType: safeType})
			blocks = append(blocks, unsupportedBlock(safeType, "An unsupported Claude content block was present. Its raw payload was not exported."))
		}
	}
	return blocks, len(blocks) == 0
}

func countBlocks(messages []Message) (messagesWithContent, calls, results int) {
	for _, message := range messages {
		if len(message.Blocks) > 0 {
			messagesWithContent++
		}
		for _, block := range message.Blocks {
			switch block.Kind {
			case "tool_call":
				calls++
			case "tool_result":
				results++
			}
		}
	}
	return
}
