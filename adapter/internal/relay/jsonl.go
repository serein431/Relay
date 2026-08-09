package relay

import (
	"bufio"
	"bytes"
	"errors"
	"fmt"
	"io"
	"os"
)

const (
	// MaxSessionFileBytes bounds the amount of untrusted provider history that
	// one parse may consume. The adapter is a preview/export helper, not an
	// archival reader for arbitrarily large files.
	MaxSessionFileBytes int64 = 256 * 1024 * 1024
	// MaxSessionLineBytes permits large tool results while preventing one JSONL
	// record from forcing an unbounded allocation.
	MaxSessionLineBytes = 16 * 1024 * 1024
	// MaxJSONDepth is intentionally aligned with the desktop-side sanitizer.
	MaxJSONDepth = 64
)

var ErrSessionTooLarge = errors.New("session file exceeds the safety limit")

type boundedLine struct {
	Number     int
	Data       []byte
	Terminated bool
	TooLarge   bool
}

func validateSessionFileSize(info os.FileInfo) error {
	if info.Size() > MaxSessionFileBytes {
		return fmt.Errorf("%w of %d bytes", ErrSessionTooLarge, MaxSessionFileBytes)
	}
	return nil
}

// scanBoundedLines reads JSONL without retaining an oversized record. It can
// discard one oversized line and resume at the next newline, unlike
// bufio.Scanner, whose ErrTooLong terminates the whole scan.
func scanBoundedLines(reader io.Reader, visit func(boundedLine)) error {
	buffered := bufio.NewReaderSize(reader, 64*1024)
	var line []byte
	var lineNumber int
	var totalBytes int64
	tooLarge := false

	for {
		fragment, err := buffered.ReadSlice('\n')
		totalBytes += int64(len(fragment))
		if totalBytes > MaxSessionFileBytes {
			return fmt.Errorf("%w of %d bytes", ErrSessionTooLarge, MaxSessionFileBytes)
		}

		terminated := err == nil && len(fragment) > 0 && fragment[len(fragment)-1] == '\n'
		part := fragment
		if terminated {
			part = part[:len(part)-1]
		}
		if !tooLarge {
			if len(part) > MaxSessionLineBytes-len(line) {
				tooLarge = true
				line = nil
			} else {
				line = append(line, part...)
			}
		}

		switch {
		case err == nil:
			lineNumber++
			visit(boundedLine{Number: lineNumber, Data: trimJSONLCarriageReturn(line), Terminated: true, TooLarge: tooLarge})
			line = nil
			tooLarge = false
		case errors.Is(err, bufio.ErrBufferFull):
			continue
		case errors.Is(err, io.EOF):
			if len(fragment) > 0 || len(line) > 0 || tooLarge {
				lineNumber++
				visit(boundedLine{Number: lineNumber, Data: line, Terminated: false, TooLarge: tooLarge})
			}
			return nil
		default:
			return errors.New("session history could not be read")
		}
	}
}

func trimJSONLCarriageReturn(line []byte) []byte {
	return bytes.TrimSuffix(line, []byte{'\r'})
}

// jsonDepthExceeds performs a small lexical pass before json.Unmarshal. Braces
// inside strings are ignored. Invalid JSON is still reported by json.Unmarshal.
func jsonDepthExceeds(data []byte, maximum int) bool {
	depth := 0
	inString := false
	escaped := false
	for _, value := range data {
		if inString {
			if escaped {
				escaped = false
				continue
			}
			switch value {
			case '\\':
				escaped = true
			case '"':
				inString = false
			}
			continue
		}
		switch value {
		case '"':
			inString = true
		case '{', '[':
			depth++
			if depth > maximum {
				return true
			}
		case '}', ']':
			if depth > 0 {
				depth--
			}
		}
	}
	return false
}
