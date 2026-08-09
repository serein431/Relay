package main

import (
	"fmt"
	"os"

	"relay.local/agent-adapter/internal/protocol"
)

func main() {
	if err := protocol.NewServer(nil).Serve(os.Stdin, os.Stdout); err != nil {
		fmt.Fprintln(os.Stderr, "relay-agent-adapter:", err)
		os.Exit(1)
	}
}
