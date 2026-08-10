package relay

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"time"
)

type codexState struct {
	sessionID          string
	sessionIdentitySet bool
	cwd                string
	nativeVersion      string
	created            time.Time
	updated            time.Time
	firstUserText      string
	currentTurnID      string
	pendingCalls       map[string]bool
	seenMessageID      map[string]bool
}

func parseCodexSession(home, path string, titles map[string]string) (ParsedSession, error) {
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
	state := codexState{
		sessionID:     inferCodexIDFromFilename(path),
		pendingCalls:  map[string]bool{},
		seenMessageID: map[string]bool{},
	}
	err = scanBoundedLines(file, func(scanned boundedLine) {
		stats.TotalLines++
		lineNumber := scanned.Number
		if scanned.TooLarge {
			stats.DamagedLines++
			parsed.Warnings = append(parsed.Warnings, Warning{Code: "line_too_large", Message: "Codex record exceeded the per-line safety limit", Line: lineNumber})
			return
		}
		if !scanned.Terminated {
			stats.DamagedLines++
			parsed.Warnings = append(parsed.Warnings, Warning{Code: "truncated_final_line", Message: "Codex session ended with an incomplete JSONL record", Line: lineNumber})
			return
		}
		line := bytes.TrimSpace(scanned.Data)
		if len(line) == 0 {
			return
		}
		if jsonDepthExceeds(line, MaxJSONDepth) {
			stats.DamagedLines++
			parsed.Warnings = append(parsed.Warnings, Warning{Code: "json_too_deep", Message: "Codex record exceeded the JSON nesting safety limit", Line: lineNumber})
			return
		}
		var record map[string]any
		if err := json.Unmarshal(line, &record); err != nil {
			stats.DamagedLines++
			parsed.Warnings = append(parsed.Warnings, Warning{Code: "invalid_json", Message: "Codex record is not valid JSON", Line: lineNumber})
			return
		}
		stats.ParsedLines++
		codexRecord(&parsed, &state, record, lineNumber)
	})
	if err != nil {
		return ParsedSession{}, err
	}
	for callID := range state.pendingCalls {
		if state.pendingCalls[callID] {
			stats.UnmatchedToolCalls++
		}
	}
	if stats.UnmatchedToolCalls > 0 {
		parsed.Warnings = append(parsed.Warnings, Warning{Code: "unmatched_tool_calls", Message: "Codex session ended with tool calls that have no result"})
	}

	title := firstString(titles[state.sessionID], titleFromText(state.firstUserText), state.sessionID)
	projectKey, projectName, projectRoot := projectIdentity(state.cwd, state.sessionID)
	messageCount, toolCalls, toolResults := countBlocks(parsed.Messages)
	markCompleteness(stats, messageCount)
	parsed.Summary = SessionSummary{
		Agent:           AgentCodex,
		SessionID:       state.sessionID,
		Title:           title,
		Preview:         previewFromMessages(parsed.Messages),
		CWD:             state.cwd,
		ProjectKey:      projectKey,
		ProjectName:     projectName,
		ProjectRoot:     projectRoot,
		CreatedAt:       formatTime(state.created),
		UpdatedAt:       formatTime(state.updated),
		NativeVersion:   state.nativeVersion,
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

func codexRecord(parsed *ParsedSession, state *codexState, record map[string]any, line int) {
	timestamp := safeTimestamp(stringValue(record["timestamp"]))
	updateTimeBounds(&state.created, &state.updated, timestamp)
	recordType := stringValue(record["type"])
	payload := mapValue(record["payload"])

	// Legacy Codex JSONL (before the rollout envelope) stores items directly.
	if recordType == "" && firstString(record["id"], record["session_id"]) != "" {
		if !state.sessionIdentitySet {
			state.sessionID = firstString(record["id"], record["session_id"], state.sessionID)
			state.sessionIdentitySet = true
		}
		state.cwd = firstString(state.cwd, record["cwd"], mapValue(record["environment"])["cwd"])
		state.nativeVersion = firstString(state.nativeVersion, record["cli_version"], record["version"])
		parsed.Completeness.HiddenRecords++
		return
	}

	switch recordType {
	case "session_meta":
		if !state.sessionIdentitySet {
			if identity := firstString(payload["id"], payload["session_id"]); identity != "" {
				state.sessionID = identity
				state.sessionIdentitySet = true
			}
		}
		state.cwd = firstString(state.cwd, payload["cwd"])
		state.nativeVersion = firstString(state.nativeVersion, payload["cli_version"])
		updateTimeBounds(&state.created, &state.updated, stringValue(payload["timestamp"]))
		parsed.Completeness.HiddenRecords++
	case "turn_context":
		if cwd := stringValue(payload["cwd"]); cwd != "" {
			state.cwd = cwd
		}
		state.currentTurnID = firstString(payload["turn_id"], state.currentTurnID)
		parsed.Completeness.HiddenRecords++
	case "response_item":
		codexItem(parsed, state, payload, timestamp, line)
	case "message", "function_call", "function_call_output", "custom_tool_call", "custom_tool_call_output":
		codexItem(parsed, state, record, timestamp, line)
	case "event_msg":
		codexEvent(parsed, state, payload, timestamp, line)
	case "world_state", "record_type", "compacted", "inter_agent_communication_metadata":
		parsed.Completeness.HiddenRecords++
	case "":
		if stringValue(record["record_type"]) != "" {
			parsed.Completeness.HiddenRecords++
			return
		}
		parsed.Completeness.UnknownRecords++
		parsed.Warnings = append(parsed.Warnings, Warning{Code: "unknown_record", Message: "Codex record without a type was preserved as an unsupported summary", Line: line})
		appendCodexUnsupported(parsed, state, line, "unknown", "An unsupported Codex historical record was present. Its raw payload was not exported.")
	default:
		parsed.Completeness.UnknownRecords++
		safeType := safeNativeType(recordType)
		parsed.Warnings = append(parsed.Warnings, Warning{Code: "unknown_record", Message: "Codex record type was preserved as an unsupported summary", Line: line, RecordType: safeType})
		appendCodexUnsupported(parsed, state, line, safeType, "An unsupported Codex historical record was present. Its raw payload was not exported.")
	}
}

func codexItem(parsed *ParsedSession, state *codexState, item map[string]any, timestamp string, line int) {
	kind := stringValue(item["type"])
	id := makeMessageID(AgentCodex, state.sessionID, line, stringValue(item["id"]))

	switch kind {
	case "message":
		role := stringValue(item["role"])
		if role != "user" && role != "assistant" {
			parsed.Completeness.HiddenRecords++
			return
		}
		turnID := codexTurnID(item, state.currentTurnID)
		blocks, hidden := codexMessageBlocks(item["content"], state, parsed, line)
		if hidden || len(blocks) == 0 {
			parsed.Completeness.HiddenRecords++
			return
		}
		if turnID != "" {
			state.currentTurnID = turnID
		}
		if role == "user" && state.firstUserText == "" {
			for _, block := range blocks {
				if block.Kind == "text" && !providerInternalText(block.Text) {
					state.firstUserText = block.Text
					break
				}
			}
		}
		appendCodexMessage(parsed, state, Message{ID: id, TurnID: turnID, BranchID: codexBranchID(item), Timestamp: timestamp, Role: role, Phase: stringValue(item["phase"]), Blocks: blocks})
	case "agent_message":
		// Collaboration messages can contain only a visible envelope while the
		// actual payload is encrypted provider state. They are intentionally not
		// exported as conversation messages.
		parsed.Completeness.HiddenRecords++
	case "function_call", "custom_tool_call", "tool_search_call":
		turnID := updateCodexTurnID(state, item)
		callID := stringValue(item["call_id"])
		if callID != "" {
			state.pendingCalls[callID] = true
		}
		input := item["arguments"]
		if input == nil {
			input = item["input"]
		}
		name := stringValue(item["name"])
		if namespace := stringValue(item["namespace"]); namespace != "" {
			name = namespace + "." + name
		}
		arguments, argumentsTooDeep := jsonArgument(input)
		if argumentsTooDeep {
			parsed.Completeness.UnsupportedBlocks++
			parsed.Warnings = append(parsed.Warnings, Warning{Code: "embedded_json_too_deep", Message: "Codex tool arguments exceeded the JSON nesting safety limit", Line: line})
		}
		appendCodexMessage(parsed, state, Message{
			ID: id, TurnID: turnID, BranchID: codexBranchID(item), Timestamp: timestamp, Role: "assistant",
			Blocks: []Block{{Kind: "tool_call", Classification: "user_visible", CallID: callID, Name: name, Status: stringValue(item["status"]), Input: arguments, NativeType: kind, ReplayPolicy: "never"}},
		})
	case "function_call_output", "custom_tool_call_output", "tool_search_output":
		turnID := updateCodexTurnID(state, item)
		callID := stringValue(item["call_id"])
		if callID == "" || !state.pendingCalls[callID] {
			parsed.Completeness.OrphanToolResults++
			parsed.Warnings = append(parsed.Warnings, Warning{Code: "orphan_tool_result", Message: "Codex tool result has no matching call", Line: line})
		} else {
			state.pendingCalls[callID] = false
		}
		appendCodexMessage(parsed, state, Message{
			ID: id, TurnID: turnID, BranchID: codexBranchID(item), Timestamp: timestamp, Role: "tool",
			Blocks: []Block{{Kind: "tool_result", Classification: "user_visible", CallID: callID, Status: stringValue(item["status"]), Output: sanitizedValue(item["output"]), NativeType: kind, ReplayPolicy: "never"}},
		})
	case "reasoning":
		parsed.Completeness.HiddenRecords++
	case "":
		parsed.Completeness.UnknownRecords++
		parsed.Warnings = append(parsed.Warnings, Warning{Code: "unknown_response_item", Message: "Codex response item without a type was preserved as an unsupported summary", Line: line})
		appendCodexUnsupported(parsed, state, line, "unknown", "An unsupported Codex response item was present. Its raw payload was not exported.")
	default:
		parsed.Completeness.UnsupportedBlocks++
		safeType := safeNativeType(kind)
		parsed.Warnings = append(parsed.Warnings, Warning{Code: "unsupported_response_item", Message: "Codex response item was preserved as an unsupported summary", Line: line, RecordType: safeType})
		appendCodexUnsupported(parsed, state, line, safeType, "An unsupported Codex response item was present. Its raw payload was not exported.")
	}
}

func appendCodexUnsupported(parsed *ParsedSession, state *codexState, line int, nativeType, summary string) {
	appendCodexMessage(parsed, state, Message{
		ID:     makeMessageID(AgentCodex, state.sessionID, line, ""),
		Role:   "system",
		Blocks: []Block{unsupportedBlock(nativeType, summary)},
	})
}

func codexMessageBlocks(content any, state *codexState, parsed *ParsedSession, line int) ([]Block, bool) {
	items, ok := content.([]any)
	if !ok {
		if text, isString := content.(string); isString && strings.TrimSpace(text) != "" {
			if providerInternalText(text) {
				if cwd := cwdFromEnvironmentText(text); cwd != "" {
					state.cwd = firstString(state.cwd, cwd)
				}
				return nil, true
			}
			return []Block{{Kind: "text", Classification: "user_visible", Text: text}}, false
		}
		parsed.Completeness.UnsupportedBlocks++
		parsed.Warnings = append(parsed.Warnings, Warning{Code: "unsupported_content", Message: "Codex message content was preserved as an unsupported summary", Line: line})
		return []Block{unsupportedBlock("message_content", "Unsupported Codex message content was present. Its raw payload was not exported.")}, false
	}
	blocks := make([]Block, 0, len(items))
	for _, value := range items {
		item := mapValue(value)
		kind := stringValue(item["type"])
		switch kind {
		case "input_text", "output_text", "text":
			text := firstString(item["text"], item["output_text"])
			if text == "" {
				continue
			}
			if providerInternalText(text) {
				if cwd := cwdFromEnvironmentText(text); cwd != "" {
					state.cwd = firstString(state.cwd, cwd)
				}
				continue
			}
			blocks = append(blocks, Block{Kind: "text", Classification: "user_visible", Text: text})
		case "input_image", "image_url", "local_image", "audio", "input_audio", "file":
			blocks = append(blocks, Block{Kind: "asset_ref", Classification: "user_visible", NativeType: kind, Source: sanitizedValue(item)})
		case "encrypted_content":
			parsed.Completeness.HiddenRecords++
		case "":
			parsed.Completeness.UnsupportedBlocks++
			parsed.Warnings = append(parsed.Warnings, Warning{Code: "unsupported_block", Message: "Codex content block without a type was preserved as an unsupported summary", Line: line})
			blocks = append(blocks, unsupportedBlock("unknown", "An unsupported Codex content block was present. Its raw payload was not exported."))
		default:
			parsed.Completeness.UnsupportedBlocks++
			safeType := safeNativeType(kind)
			parsed.Warnings = append(parsed.Warnings, Warning{Code: "unsupported_block", Message: "Codex content block was preserved as an unsupported summary", Line: line, RecordType: safeType})
			blocks = append(blocks, unsupportedBlock(safeType, "An unsupported Codex content block was present. Its raw payload was not exported."))
		}
	}
	return blocks, len(blocks) == 0
}

func codexEvent(parsed *ParsedSession, state *codexState, payload map[string]any, timestamp string, line int) {
	switch stringValue(payload["type"]) {
	case "user_message":
		text := stringValue(payload["message"])
		if text == "" || providerInternalText(text) || state.firstUserText != "" {
			parsed.Completeness.HiddenRecords++
			return
		}
		// Event messages duplicate response_item messages in current Codex. Keep
		// only metadata here; the response item is the canonical transcript.
		state.firstUserText = text
		parsed.Completeness.HiddenRecords++
	case "agent_message":
		parsed.Completeness.HiddenRecords++
	case "context_compacted":
		appendCodexMessage(parsed, state, Message{
			ID:        makeMessageID(AgentCodex, state.sessionID, line, ""),
			TurnID:    state.currentTurnID,
			Timestamp: timestamp,
			Role:      "system",
			Blocks: []Block{{
				Kind:           "context_compacted",
				Classification: "user_visible",
				NativeType:     "context_compacted",
			}},
		})
	default:
		parsed.Completeness.HiddenRecords++
	}
	_ = timestamp
	_ = line
}

func appendCodexMessage(parsed *ParsedSession, state *codexState, message Message) {
	if state.seenMessageID[message.ID] {
		return
	}
	state.seenMessageID[message.ID] = true
	parsed.Messages = append(parsed.Messages, message)
}

func codexTurnID(item map[string]any, fallback string) string {
	metadata := mapValue(item["internal_chat_message_metadata_passthrough"])
	return firstString(metadata["turn_id"], item["turn_id"], fallback)
}

func updateCodexTurnID(state *codexState, item map[string]any) string {
	turnID := codexTurnID(item, state.currentTurnID)
	if turnID != "" {
		state.currentTurnID = turnID
	}
	return turnID
}

func codexBranchID(item map[string]any) string {
	metadata := mapValue(item["internal_chat_message_metadata_passthrough"])
	return firstString(metadata["branch_id"], item["branch_id"])
}

func inferCodexIDFromFilename(path string) string {
	name := strings.TrimSuffix(filepath.Base(path), filepath.Ext(path))
	if len(name) >= 36 {
		candidate := name[len(name)-36:]
		if strings.Count(candidate, "-") == 4 {
			return candidate
		}
	}
	return name
}
