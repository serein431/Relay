#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
project_dir="$(cd "$script_dir/.." && pwd)"
artifact_path="${1:-$project_dir/src-tauri/target/release/bundle/macos/Relay.app}"

if [[ ! -e "$artifact_path" && -d "$project_dir/src-tauri/target/debug/bundle/macos/Relay.app" ]]; then
  artifact_path="$project_dir/src-tauri/target/debug/bundle/macos/Relay.app"
fi

verify_app() {
  local app_path="$1"
  local main_binary="$app_path/Contents/MacOS/relay"
  local adapter_binary="$app_path/Contents/MacOS/relay-agent-adapter"

  [[ -d "$app_path" ]] || { printf 'Relay.app not found: %s\n' "$app_path" >&2; exit 1; }
  [[ -x "$main_binary" ]] || { printf 'Relay main executable is missing: %s\n' "$main_binary" >&2; exit 1; }
  [[ -x "$adapter_binary" ]] || { printf 'Bundled Relay Adapter is missing or not executable: %s\n' "$adapter_binary" >&2; exit 1; }

  local main_arches
  local adapter_arches
  main_arches="$(lipo -archs "$main_binary")"
  adapter_arches="$(lipo -archs "$adapter_binary")"
  [[ "$main_arches" == "$adapter_arches" ]] || {
    printf 'Relay and Adapter architectures differ: app=%s adapter=%s\n' "$main_arches" "$adapter_arches" >&2
    exit 1
  }

  printf '%s\n' '{"id":"bundle-check","method":"health","params":{}}' \
    | "$adapter_binary" \
    | node -e '
      let input = "";
      process.stdin.setEncoding("utf8");
      process.stdin.on("data", chunk => { input += chunk; });
      process.stdin.on("end", () => {
        const response = JSON.parse(input);
        if (!response.ok || response.result?.protocol !== "relay.adapter.v1" || response.result?.read_only !== true) {
          throw new Error("bundled Adapter returned an invalid health response");
        }
      });
    '

  printf 'Verified %s (%s) with bundled read-only Adapter.\n' "$app_path" "$main_arches"
}

if [[ -d "$artifact_path" ]]; then
  verify_app "$artifact_path"
  exit 0
fi

if [[ "$artifact_path" != *.dmg || ! -f "$artifact_path" ]]; then
  printf 'Relay app or DMG not found: %s\n' "$artifact_path" >&2
  exit 1
fi

mount_dir="$(mktemp -d "${TMPDIR:-/tmp}/relay-dmg.XXXXXX")"
cleanup() {
  hdiutil detach "$mount_dir" -quiet >/dev/null 2>&1 || true
  rmdir "$mount_dir" >/dev/null 2>&1 || true
}
trap cleanup EXIT

hdiutil attach "$artifact_path" -readonly -nobrowse -noautoopen -mountpoint "$mount_dir" -quiet
mounted_app="$(find "$mount_dir" -maxdepth 2 -type d -name 'Relay.app' -print -quit)"
[[ -n "$mounted_app" ]] || { printf 'Relay.app was not found inside DMG: %s\n' "$artifact_path" >&2; exit 1; }
verify_app "$mounted_app"
printf 'Verified DMG: %s\n' "$artifact_path"
