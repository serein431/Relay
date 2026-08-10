package main

import (
	"encoding/json"
	"fmt"
	"os"

	"relay.local/agent-adapter/internal/nativeimport"
)

func main() {
	var request nativeimport.Request
	if err := json.NewDecoder(os.Stdin).Decode(&request); err != nil {
		writeError(&nativeimport.ImportError{
			Code: "invalid_request", Message: fmt.Sprintf("cannot decode import request: %v", err),
		})
		return
	}
	result, err := nativeimport.Import(request)
	if err != nil {
		writeError(err)
		return
	}
	_ = json.NewEncoder(os.Stdout).Encode(map[string]any{
		"ok":     true,
		"result": result,
	})
}

func writeError(err *nativeimport.ImportError) {
	_ = json.NewEncoder(os.Stdout).Encode(map[string]any{
		"ok": false,
		"error": map[string]any{
			"code":       err.Code,
			"message":    err.Message,
			"backup_dir": err.BackupDir,
			"steps":      err.Steps,
		},
	})
}
