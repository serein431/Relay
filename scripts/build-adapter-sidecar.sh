#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
project_dir="$(cd "$script_dir/.." && pwd)"
adapter_dir="$project_dir/adapter"
output_dir="$adapter_dir/bin"
platform="${TAURI_ENV_PLATFORM:-darwin}"
arch="${TAURI_ENV_ARCH:-$(uname -m)}"
target_triple="${TAURI_ENV_TARGET_TRIPLE:-}"

if [[ "$platform" != "darwin" && "$platform" != "macos" ]]; then
  printf 'Relay v0.1 sidecar packaging currently supports macOS only.\n' >&2
  exit 1
fi

mkdir -p "$output_dir"

build_one() {
  local goarch="$1"
  local target="$2"
  local adapter_output="$output_dir/relay-agent-adapter-$target"
  local importer_output="$output_dir/relay-session-importer-$target"
  (
    cd "$adapter_dir"
    CGO_ENABLED=0 GOOS=darwin GOARCH="$goarch" \
      go build -trimpath -ldflags='-s -w' -o "$adapter_output" ./cmd/relay-agent-adapter
    CGO_ENABLED=0 GOOS=darwin GOARCH="$goarch" \
      go build -trimpath -ldflags='-s -w' -o "$importer_output" ./cmd/relay-session-importer
  )
  chmod 0755 "$adapter_output" "$importer_output"
}

case "$target_triple" in
  aarch64-apple-darwin)
    build_one arm64 aarch64-apple-darwin
    ;;
  x86_64-apple-darwin)
    build_one amd64 x86_64-apple-darwin
    ;;
  universal-apple-darwin)
    build_one arm64 aarch64-apple-darwin
    build_one amd64 x86_64-apple-darwin
    lipo -create \
      "$output_dir/relay-agent-adapter-aarch64-apple-darwin" \
      "$output_dir/relay-agent-adapter-x86_64-apple-darwin" \
      -output "$output_dir/relay-agent-adapter-universal-apple-darwin"
    lipo -create \
      "$output_dir/relay-session-importer-aarch64-apple-darwin" \
      "$output_dir/relay-session-importer-x86_64-apple-darwin" \
      -output "$output_dir/relay-session-importer-universal-apple-darwin"
    chmod 0755 \
      "$output_dir/relay-agent-adapter-universal-apple-darwin" \
      "$output_dir/relay-session-importer-universal-apple-darwin"
    ;;
  "")
    case "$arch" in
      arm64|aarch64)
        build_one arm64 aarch64-apple-darwin
        ;;
      x86_64|amd64)
        build_one amd64 x86_64-apple-darwin
        ;;
      universal|universal2)
        build_one arm64 aarch64-apple-darwin
        build_one amd64 x86_64-apple-darwin
        lipo -create \
          "$output_dir/relay-agent-adapter-aarch64-apple-darwin" \
          "$output_dir/relay-agent-adapter-x86_64-apple-darwin" \
          -output "$output_dir/relay-agent-adapter-universal-apple-darwin"
        lipo -create \
          "$output_dir/relay-session-importer-aarch64-apple-darwin" \
          "$output_dir/relay-session-importer-x86_64-apple-darwin" \
          -output "$output_dir/relay-session-importer-universal-apple-darwin"
        chmod 0755 \
          "$output_dir/relay-agent-adapter-universal-apple-darwin" \
          "$output_dir/relay-session-importer-universal-apple-darwin"
        ;;
      *)
        printf 'Unsupported Tauri architecture for Relay Adapter: %s\n' "$arch" >&2
        exit 1
        ;;
    esac
    ;;
  *)
    printf 'Unsupported Tauri target for Relay Adapter: %s\n' "$target_triple" >&2
    exit 1
    ;;
esac
