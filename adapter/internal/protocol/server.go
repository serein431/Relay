package protocol

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"strings"
	"time"

	"relay.local/agent-adapter/internal/relay"
)

type Request struct {
	ID     string          `json:"id"`
	Method string          `json:"method"`
	Params json.RawMessage `json:"params,omitempty"`
}

type Response struct {
	ID     string         `json:"id"`
	OK     bool           `json:"ok"`
	Result any            `json:"result,omitempty"`
	Error  *ResponseError `json:"error,omitempty"`
}

type ResponseError struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

type Clock func() time.Time

type Server struct {
	now Clock
}

func NewServer(now Clock) *Server {
	if now == nil {
		now = time.Now
	}
	return &Server{now: now}
}

func (s *Server) Serve(input io.Reader, output io.Writer) error {
	scanner := bufio.NewScanner(input)
	scanner.Buffer(make([]byte, 64*1024), 4*1024*1024)
	encoder := json.NewEncoder(output)
	encoder.SetEscapeHTML(false)
	for scanner.Scan() {
		line := bytes.TrimSpace(scanner.Bytes())
		if len(line) == 0 {
			continue
		}
		response := s.handleLine(line)
		if err := encoder.Encode(response); err != nil {
			return err
		}
	}
	return scanner.Err()
}

func (s *Server) handleLine(line []byte) (response Response) {
	defer func() {
		if recovered := recover(); recovered != nil {
			response = Response{ID: response.ID, OK: false, Error: &ResponseError{Code: "internal_error", Message: "adapter request failed unexpectedly"}}
		}
	}()

	var request Request
	if err := json.Unmarshal(line, &request); err != nil {
		return failure("", "invalid_request", "request must be one JSON object: "+err.Error())
	}
	response.ID = request.ID
	if strings.TrimSpace(request.ID) == "" {
		return failure(request.ID, "invalid_request", "id is required")
	}
	if strings.TrimSpace(request.Method) == "" {
		return failure(request.ID, "invalid_request", "method is required")
	}

	params := request.Params
	if len(bytes.TrimSpace(params)) == 0 || bytes.Equal(bytes.TrimSpace(params), []byte("null")) {
		params = json.RawMessage(`{}`)
	}

	var result any
	var err error
	switch request.Method {
	case "health":
		result = map[string]any{
			"protocol":               relay.ProtocolSchema,
			"schema":                 relay.ProtocolSchema,
			"version":                relay.AdapterVersion,
			"adapter_version":        relay.AdapterVersion,
			"handoff_preview_schema": relay.PreviewSchema,
			"read_only":              true,
			"supported_agents":       []string{relay.AgentClaude, relay.AgentCodex},
			"supported_methods":      []string{"health", "discover_sessions", "inspect_session", "export_session"},
			"limits": map[string]any{
				"session_file_bytes": relay.MaxSessionFileBytes,
				"jsonl_line_bytes":   relay.MaxSessionLineBytes,
				"json_depth":         relay.MaxJSONDepth,
			},
		}
	case "discover_sessions":
		var options relay.DiscoverOptions
		if decodeErr := json.Unmarshal(params, &options); decodeErr != nil {
			return failure(request.ID, "invalid_params", decodeErr.Error())
		}
		result, err = relay.Discover(options, s.now())
	case "inspect_session":
		var options relay.SessionOptions
		if decodeErr := json.Unmarshal(params, &options); decodeErr != nil {
			return failure(request.ID, "invalid_params", decodeErr.Error())
		}
		result, err = relay.Inspect(options)
	case "export_session":
		var options relay.SessionOptions
		if decodeErr := json.Unmarshal(params, &options); decodeErr != nil {
			return failure(request.ID, "invalid_params", decodeErr.Error())
		}
		result, err = relay.Export(options, s.now())
	default:
		return failure(request.ID, "method_not_found", fmt.Sprintf("unknown method %q", request.Method))
	}
	if err != nil {
		return failure(request.ID, classifyError(err), err.Error())
	}
	return Response{ID: request.ID, OK: true, Result: result}
}

func failure(id, code, message string) Response {
	return Response{ID: id, OK: false, Error: &ResponseError{Code: code, Message: message}}
}

func classifyError(err error) string {
	if errors.Is(err, relay.ErrSessionTooLarge) {
		return "session_too_large"
	}
	message := strings.ToLower(err.Error())
	switch {
	case strings.Contains(message, "not found") || strings.Contains(message, "was not found"):
		return "session_not_found"
	case strings.Contains(message, "required") || strings.Contains(message, "must be") || strings.Contains(message, "invalid") || strings.Contains(message, "unsupported"):
		return "invalid_params"
	default:
		return "adapter_error"
	}
}
