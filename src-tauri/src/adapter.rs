use crate::process::{
    bytes_to_trimmed_string, canonical_executable, canonical_existing_directory,
    find_executable_on_path, run_process, ProcessRunError,
};
use crate::types::{
    AdapterHealth, AdapterWarning, AgentProvider, CommandError, DiscoverSessionsRequest,
    DiscoverSessionsResult, SessionSummary,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ADAPTER_ENV: &str = "RELAY_AGENT_ADAPTER";
const DEVELOPMENT_FALLBACK_ENV: &str = "RELAY_ALLOW_DEVELOPMENT_ADAPTER_FALLBACK";
const EXPECTED_PROTOCOL: &str = "relay.adapter.v1";
const MAX_ADAPTER_STDERR: usize = 128 * 1024;
const MAX_HEALTH_STDOUT: usize = 1024 * 1024;
const MAX_DISCOVERY_STDOUT: usize = 32 * 1024 * 1024;
const MAX_EXPORT_STDOUT: usize = 64 * 1024 * 1024;
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct ResolvedAdapter {
    pub path: PathBuf,
    pub source: String,
}

#[derive(Serialize)]
struct AdapterRequest<'a> {
    id: &'a str,
    method: &'a str,
    params: Value,
}

#[derive(Debug, Deserialize)]
struct AdapterResponse {
    id: String,
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<AdapterProtocolError>,
}

#[derive(Debug, Deserialize)]
struct AdapterProtocolError {
    code: String,
    message: String,
}

pub fn resolve_adapter_executable() -> Result<ResolvedAdapter, CommandError> {
    if let Some(configured) = std::env::var_os(ADAPTER_ENV) {
        if configured.is_empty() {
            return Err(CommandError::new(
                "adapter_not_found",
                format!("{ADAPTER_ENV} is set but empty"),
            ));
        }
        let path = canonical_executable(Path::new(&configured), "Relay agent adapter")?;
        return Ok(ResolvedAdapter {
            path,
            source: ADAPTER_ENV.into(),
        });
    }

    let allow_development_fallbacks =
        cfg!(debug_assertions) || std::env::var(DEVELOPMENT_FALLBACK_ENV).as_deref() == Ok("1");
    for (candidate, source) in adapter_candidates(allow_development_fallbacks) {
        if candidate.exists() {
            return Ok(ResolvedAdapter {
                path: canonical_executable(&candidate, "Relay agent adapter")?,
                source,
            });
        }
    }

    if allow_development_fallbacks {
        if let Some(path) = find_executable_on_path("relay-agent-adapter") {
            return Ok(ResolvedAdapter {
                path,
                source: "PATH".into(),
            });
        }
    }

    Err(CommandError::new(
        "adapter_not_found",
        format!(
            "the bundled relay-agent-adapter was not found; reinstall Relay or set {ADAPTER_ENV} for an explicit development override"
        ),
    ))
}

fn adapter_candidates(allow_development_fallbacks: bool) -> Vec<(PathBuf, String)> {
    let current_exe = std::env::current_exe().ok();
    adapter_candidates_for(
        current_exe.as_deref(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
        allow_development_fallbacks,
    )
}

fn adapter_candidates_for(
    current_exe: Option<&Path>,
    manifest: &Path,
    allow_development_fallbacks: bool,
) -> Vec<(PathBuf, String)> {
    let mut candidates = Vec::new();

    if let Some(current_exe) = current_exe {
        if let Some(directory) = current_exe.parent() {
            candidates.push((
                directory.join("relay-agent-adapter"),
                "app_binary_dir".into(),
            ));
            candidates.push((
                directory.join("binaries/relay-agent-adapter"),
                "app_binaries_dir".into(),
            ));

            // In a macOS application bundle the executable is in Contents/MacOS
            // while resources live in Contents/Resources.
            if let Some(contents) = directory.parent() {
                candidates.push((
                    contents.join("Resources/relay-agent-adapter"),
                    "app_resources".into(),
                ));
            }
        }
    }

    if !allow_development_fallbacks {
        return candidates;
    }

    if let Some(configured) = option_env!("RELAY_AGENT_ADAPTER_BUILD_PATH") {
        candidates.push((PathBuf::from(configured), "build_time".into()));
    }
    candidates.push((
        manifest.join("../adapter/bin/relay-agent-adapter"),
        "workspace_adapter_bin".into(),
    ));
    candidates.push((
        manifest.join("../adapter/relay-agent-adapter"),
        "workspace_adapter".into(),
    ));
    candidates.push((
        manifest.join("binaries/relay-agent-adapter"),
        "tauri_binaries".into(),
    ));

    candidates
}

pub fn health() -> Result<AdapterHealth, CommandError> {
    let adapter = resolve_adapter_executable()?;
    let result = call_adapter(
        &adapter,
        "health",
        json!({}),
        HEALTH_TIMEOUT,
        MAX_HEALTH_STDOUT,
    )?;
    parse_health(adapter.path, result)
}

pub fn discover(
    request: Option<DiscoverSessionsRequest>,
) -> Result<DiscoverSessionsResult, CommandError> {
    let adapter = resolve_adapter_executable()?;
    let health_result = call_adapter(
        &adapter,
        "health",
        json!({}),
        HEALTH_TIMEOUT,
        MAX_HEALTH_STDOUT,
    )?;
    parse_health(adapter.path.clone(), health_result)?;
    let params = prepare_discovery_params(request.unwrap_or_default())?;
    let result = call_adapter(
        &adapter,
        "discover_sessions",
        params,
        DISCOVERY_TIMEOUT,
        MAX_DISCOVERY_STDOUT,
    )?;
    parse_discovery_result(result)
}

pub fn export_session(
    agent: AgentProvider,
    session_id: &str,
    claude_home: Option<String>,
    codex_home: Option<String>,
) -> Result<Value, CommandError> {
    if agent == AgentProvider::Unknown {
        return Err(CommandError::new(
            "invalid_agent",
            "agent must be claude_code or codex",
        ));
    }
    if session_id.trim().is_empty() || session_id.len() > 256 {
        return Err(CommandError::new(
            "invalid_session_id",
            "session_id must contain between 1 and 256 characters",
        ));
    }
    if session_id == "." || session_id == ".." || session_id.contains(['/', '\\', '\0']) {
        return Err(CommandError::new(
            "invalid_session_id",
            "session_id contains invalid path characters",
        ));
    }

    let adapter = resolve_adapter_executable()?;
    let health_result = call_adapter(
        &adapter,
        "health",
        json!({}),
        HEALTH_TIMEOUT,
        MAX_HEALTH_STDOUT,
    )?;
    let health = parse_health(adapter.path.clone(), health_result)?;
    if !health
        .supported_methods
        .iter()
        .any(|method| method == "export_session")
    {
        return Err(CommandError::new(
            "adapter_incompatible",
            "adapter does not support required method 'export_session'",
        ));
    }

    let mut params = Map::new();
    params.insert("agent".into(), Value::String(agent.as_str().into()));
    params.insert("session_id".into(), Value::String(session_id.trim().into()));
    if let Some(path) = canonical_optional_home(claude_home, "Claude home")? {
        params.insert("claude_home".into(), Value::String(path));
    }
    if let Some(path) = canonical_optional_home(codex_home, "Codex home")? {
        params.insert("codex_home".into(), Value::String(path));
    }

    let result = call_adapter(
        &adapter,
        "export_session",
        Value::Object(params),
        DISCOVERY_TIMEOUT,
        MAX_EXPORT_STDOUT,
    )?;
    if result.get("schema").and_then(Value::as_str) != Some("relay.adapter.handoff-preview.v1") {
        return Err(CommandError::new(
            "adapter_protocol_error",
            "export_session returned an unsupported handoff preview schema",
        ));
    }
    Ok(result)
}

/// Returns a stable digest for the exact session content shown during preview.
///
/// `exported_at` changes every time the read-only adapter is invoked, so it is
/// intentionally excluded. `preview_sha256` is also excluded to keep the
/// digest independent of the response field that carries it.
pub fn session_preview_sha256(preview: &Value) -> Result<String, CommandError> {
    let mut normalized = preview.clone();
    let object = normalized.as_object_mut().ok_or_else(|| {
        CommandError::new(
            "adapter_protocol_error",
            "adapter session preview must be a JSON object",
        )
    })?;
    object.remove("exported_at");
    object.remove("preview_sha256");

    let bytes = serde_json::to_vec(&normalized).map_err(|error| {
        CommandError::new(
            "adapter_protocol_error",
            format!("cannot encode adapter session preview: {error}"),
        )
    })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn prepare_discovery_params(request: DiscoverSessionsRequest) -> Result<Value, CommandError> {
    if let Some(limit) = request.limit {
        if !(1..=1000).contains(&limit) {
            return Err(CommandError::new(
                "invalid_limit",
                "session discovery limit must be between 1 and 1000",
            ));
        }
    }

    if let Some(agents) = request.agents.as_ref() {
        if agents.contains(&AgentProvider::Unknown) {
            return Err(CommandError::new(
                "invalid_agent",
                "agents may only contain claude_code or codex",
            ));
        }
    }

    let claude_home = canonical_optional_home(request.claude_home, "Claude home")?;
    let codex_home = canonical_optional_home(request.codex_home, "Codex home")?;

    let mut params = Map::new();
    if let Some(limit) = request.limit {
        params.insert("limit".into(), json!(limit));
    }
    if let Some(agents) = request.agents {
        params.insert(
            "agents".into(),
            Value::Array(
                agents
                    .into_iter()
                    .map(|agent| Value::String(agent.as_str().into()))
                    .collect(),
            ),
        );
    }
    if let Some(path) = claude_home {
        params.insert("claude_home".into(), Value::String(path));
    }
    if let Some(path) = codex_home {
        params.insert("codex_home".into(), Value::String(path));
    }
    Ok(Value::Object(params))
}

fn canonical_optional_home(
    path: Option<String>,
    label: &str,
) -> Result<Option<String>, CommandError> {
    path.map(|path| {
        canonical_existing_directory(Path::new(&path), label)
            .map(|canonical| canonical.to_string_lossy().into_owned())
    })
    .transpose()
}

fn call_adapter(
    adapter: &ResolvedAdapter,
    method: &str,
    params: Value,
    timeout: Duration,
    max_stdout: usize,
) -> Result<Value, CommandError> {
    let id = next_request_id();
    let request = AdapterRequest {
        id: &id,
        method,
        params,
    };
    let mut input = serde_json::to_vec(&request).map_err(|error| {
        CommandError::new(
            "adapter_request_error",
            format!("cannot encode adapter request: {error}"),
        )
    })?;
    input.push(b'\n');

    let output = run_process(
        &adapter.path,
        &Vec::<OsString>::new(),
        Some(&input),
        timeout,
        max_stdout,
        MAX_ADAPTER_STDERR,
        &[],
    )
    .map_err(map_process_error)?;

    let stderr = bytes_to_trimmed_string(&output.stderr);
    if !output.status.success() {
        let exit = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "terminated by signal".into());
        let message = if stderr.is_empty() {
            format!("adapter exited with {exit}")
        } else {
            format!("adapter exited with {exit}; stderr: {stderr}")
        };
        return Err(
            CommandError::new("adapter_exit_error", message).with_details(json!({
                "stderr_truncated": output.stderr_truncated
            })),
        );
    }
    if output.stdout_truncated {
        return Err(CommandError::new(
            "adapter_protocol_error",
            format!("adapter response exceeded the {max_stdout} byte limit"),
        ));
    }

    parse_adapter_response(&output.stdout, &id)
}

fn map_process_error(error: ProcessRunError) -> CommandError {
    match error {
        ProcessRunError::Timeout { .. } => CommandError::new("adapter_timeout", error.to_string()),
        ProcessRunError::Spawn(_) => CommandError::new("adapter_start_error", error.to_string()),
        _ => CommandError::new("adapter_io_error", error.to_string()),
    }
}

fn parse_adapter_response(bytes: &[u8], expected_id: &str) -> Result<Value, CommandError> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        CommandError::new(
            "adapter_protocol_error",
            format!("adapter response is not UTF-8: {error}"),
        )
    })?;
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() != 1 {
        return Err(CommandError::new(
            "adapter_protocol_error",
            format!(
                "adapter must return exactly one JSON line, received {} non-empty lines",
                lines.len()
            ),
        ));
    }

    let response: AdapterResponse = serde_json::from_str(lines[0]).map_err(|error| {
        CommandError::new(
            "adapter_protocol_error",
            format!("adapter returned invalid JSON: {error}"),
        )
    })?;
    if response.id != expected_id {
        return Err(CommandError::new(
            "adapter_protocol_error",
            format!(
                "adapter response id '{}' does not match request id '{expected_id}'",
                response.id
            ),
        ));
    }

    if response.ok {
        if response.error.is_some() {
            return Err(CommandError::new(
                "adapter_protocol_error",
                "successful adapter response unexpectedly contains an error",
            ));
        }
        return response.result.ok_or_else(|| {
            CommandError::new(
                "adapter_protocol_error",
                "successful adapter response is missing result",
            )
        });
    }

    let error = response.error.ok_or_else(|| {
        CommandError::new(
            "adapter_protocol_error",
            "failed adapter response is missing error details",
        )
    })?;
    Err(CommandError::new(
        format!("adapter.{}", normalized_error_code(&error.code)),
        error.message,
    ))
}

fn normalized_error_code(code: &str) -> String {
    let normalized: String = code
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if normalized.is_empty() {
        "error".into()
    } else {
        normalized
    }
}

fn parse_health(path: PathBuf, result: Value) -> Result<AdapterHealth, CommandError> {
    let mut object = result.as_object().cloned().ok_or_else(|| {
        CommandError::new(
            "adapter_protocol_error",
            "health result must be a JSON object",
        )
    })?;
    let explicit_protocol = take_optional_string(&mut object, "protocol")?;
    let schema = take_optional_string(&mut object, "schema")?;
    let protocol = explicit_protocol.or_else(|| schema.clone());
    let version = take_optional_string(&mut object, "version")?
        .or(take_optional_string(&mut object, "adapter_version")?);
    let read_only = take_optional_bool(&mut object, "read_only")?;
    let supported_methods = match object.remove("supported_methods") {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => serde_json::from_value(value).map_err(|error| {
            CommandError::new(
                "adapter_protocol_error",
                format!("health supported_methods must be an array of strings: {error}"),
            )
        })?,
    };

    if protocol.as_deref() != Some(EXPECTED_PROTOCOL) {
        return Err(CommandError::new(
            "adapter_incompatible",
            format!(
                "adapter protocol must be '{EXPECTED_PROTOCOL}', received '{}'",
                protocol.as_deref().unwrap_or("missing")
            ),
        ));
    }
    if read_only != Some(true) {
        return Err(CommandError::new(
            "adapter_unsafe",
            "adapter health must explicitly report read_only=true",
        ));
    }
    for required_method in ["health", "discover_sessions"] {
        if !supported_methods
            .iter()
            .any(|method| method == required_method)
        {
            return Err(CommandError::new(
                "adapter_incompatible",
                format!("adapter does not support required method '{required_method}'"),
            ));
        }
    }

    Ok(AdapterHealth {
        executable_path: path.to_string_lossy().into_owned(),
        protocol,
        schema,
        version,
        read_only,
        supported_methods,
        details: object,
    })
}

fn take_optional_string(
    object: &mut Map<String, Value>,
    key: &str,
) -> Result<Option<String>, CommandError> {
    match object.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(CommandError::new(
            "adapter_protocol_error",
            format!("health {key} must be a string"),
        )),
    }
}

fn take_optional_bool(
    object: &mut Map<String, Value>,
    key: &str,
) -> Result<Option<bool>, CommandError> {
    match object.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(value)),
        Some(_) => Err(CommandError::new(
            "adapter_protocol_error",
            format!("health {key} must be a boolean"),
        )),
    }
}

fn parse_discovery_result(result: Value) -> Result<DiscoverSessionsResult, CommandError> {
    let parsed = match result {
        Value::Array(sessions) => DiscoverSessionsResult {
            schema: None,
            scanned_at: None,
            sessions: deserialize_sessions(sessions)?,
            warnings: Vec::new(),
            extra: Map::new(),
        },
        Value::Object(mut object) => {
            for key in ["sessions", "warnings"] {
                if object.get(key).map_or(true, Value::is_null) {
                    object.insert(key.into(), Value::Array(Vec::new()));
                }
            }
            serde_json::from_value(Value::Object(object)).map_err(|error| {
                CommandError::new(
                    "adapter_protocol_error",
                    format!("invalid discover_sessions result: {error}"),
                )
            })?
        }
        _ => {
            return Err(CommandError::new(
                "adapter_protocol_error",
                "discover_sessions result must be an object or array",
            ))
        }
    };

    validate_sessions(parsed)
}

fn deserialize_sessions(values: Vec<Value>) -> Result<Vec<SessionSummary>, CommandError> {
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            serde_json::from_value(value).map_err(|error| {
                CommandError::new(
                    "adapter_protocol_error",
                    format!("invalid session at index {index}: {error}"),
                )
            })
        })
        .collect()
}

fn validate_sessions(
    mut result: DiscoverSessionsResult,
) -> Result<DiscoverSessionsResult, CommandError> {
    for (index, session) in result.sessions.iter().enumerate() {
        if session.session_id.trim().is_empty() {
            return Err(CommandError::new(
                "adapter_protocol_error",
                format!("session at index {index} has an empty session_id"),
            ));
        }
        if session.provider == AgentProvider::Unknown {
            result.warnings.push(AdapterWarning {
                code: "unknown_agent".into(),
                message: format!(
                    "session '{}' uses an agent value this Relay version does not recognize",
                    session.session_id
                ),
                line: None,
                record_type: None,
                extra: Map::new(),
            });
        }
    }
    Ok(result)
}

fn next_request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("relay-{}-{nanos}-{sequence}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_successful_adapter_response() {
        let value = parse_adapter_response(
            br#"{"id":"request-1","ok":true,"result":{"protocol":"relay.adapter.v1"}}
"#,
            "request-1",
        )
        .expect("response should parse");
        assert_eq!(value["protocol"], "relay.adapter.v1");
    }

    #[test]
    fn rejects_mismatched_response_id() {
        let error = parse_adapter_response(
            br#"{"id":"request-2","ok":true,"result":{}}
"#,
            "request-1",
        )
        .expect_err("mismatched id must fail");
        assert_eq!(error.code, "adapter_protocol_error");
        assert!(error.message.contains("does not match"));
    }

    #[test]
    fn preserves_adapter_error_code_and_message() {
        let error = parse_adapter_response(
            br#"{"id":"request-1","ok":false,"error":{"code":"bad_input","message":"invalid home"}}
"#,
            "request-1",
        )
        .expect_err("adapter error must fail");
        assert_eq!(error.code, "adapter.bad_input");
        assert_eq!(error.message, "invalid home");
    }

    #[test]
    fn parses_current_discovery_shape() {
        let value = json!({
            "schema": "relay.adapter.v1",
            "scanned_at": "2026-08-07T00:00:00Z",
            "sessions": [{
                "agent": "claude_code",
                "session_id": "abc",
                "title": "Fix tests",
                "cwd": "/tmp/project",
                "project_root": "/tmp/project",
                "completeness": "complete"
            }],
            "warnings": []
        });
        let result = parse_discovery_result(value).expect("discovery should parse");
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].provider, AgentProvider::ClaudeCode);
        assert_eq!(result.sessions[0].session_id, "abc");
        assert_eq!(
            result.sessions[0].project_root.as_deref(),
            Some("/tmp/project")
        );
    }

    #[test]
    fn treats_null_discovery_lists_as_empty_for_older_adapters() {
        let value = json!({
            "schema": "relay.adapter.v1",
            "scanned_at": "2026-08-07T00:00:00Z",
            "sessions": null,
            "warnings": null
        });
        let result = parse_discovery_result(value).expect("null lists should be normalized");
        assert!(result.sessions.is_empty());
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn validates_health_safety_and_protocol() {
        let health = parse_health(
            PathBuf::from("/tmp/relay-agent-adapter"),
            json!({
                "schema": "relay.adapter.v1",
                "adapter_version": "0.1.0",
                "read_only": true,
                "supported_methods": ["health", "discover_sessions"]
            }),
        )
        .expect("compatible health should parse");
        assert_eq!(health.protocol.as_deref(), Some("relay.adapter.v1"));
        assert_eq!(health.version.as_deref(), Some("0.1.0"));

        let error = parse_health(
            PathBuf::from("/tmp/relay-agent-adapter"),
            json!({
                "protocol": "relay.adapter.v2",
                "read_only": true,
                "supported_methods": ["health", "discover_sessions"]
            }),
        )
        .expect_err("incompatible protocol must fail");
        assert_eq!(error.code, "adapter_incompatible");
    }

    #[test]
    fn bundled_adapter_candidates_precede_development_fallbacks() {
        let executable = Path::new("/tmp/Relay.app/Contents/MacOS/relay");
        let manifest = Path::new("/tmp/relay-workspace/src-tauri");
        let candidates = adapter_candidates_for(Some(executable), manifest, true);
        let paths = candidates
            .iter()
            .map(|(path, source)| (path.to_string_lossy().into_owned(), source.as_str()))
            .collect::<Vec<_>>();

        assert_eq!(
            paths[0],
            (
                "/tmp/Relay.app/Contents/MacOS/relay-agent-adapter".into(),
                "app_binary_dir"
            )
        );
        assert_eq!(
            paths[2],
            (
                "/tmp/Relay.app/Contents/Resources/relay-agent-adapter".into(),
                "app_resources"
            )
        );
        assert!(paths
            .iter()
            .position(|(_, source)| *source == "workspace_adapter_bin")
            .is_some_and(|index| index > 2));
    }

    #[test]
    fn release_candidates_do_not_include_workspace_paths() {
        let executable = Path::new("/tmp/Relay.app/Contents/MacOS/relay");
        let manifest = Path::new("/private/build-machine/relay/src-tauri");
        let candidates = adapter_candidates_for(Some(executable), manifest, false);

        assert_eq!(candidates.len(), 3);
        assert!(candidates
            .iter()
            .all(|(_, source)| source.starts_with("app_")));
        assert!(candidates
            .iter()
            .all(|(path, _)| !path.starts_with(manifest)));
    }

    #[test]
    fn session_preview_digest_ignores_export_time_and_own_digest() {
        let first = json!({
            "schema": "relay.adapter.handoff-preview.v1",
            "exported_at": "2026-08-08T00:00:00Z",
            "preview_sha256": "old-value",
            "conversation": {"messages": [{"id": "message-1", "blocks": []}]}
        });
        let second = json!({
            "schema": "relay.adapter.handoff-preview.v1",
            "exported_at": "2026-08-09T00:00:00Z",
            "preview_sha256": "new-value",
            "conversation": {"messages": [{"id": "message-1", "blocks": []}]}
        });

        assert_eq!(
            session_preview_sha256(&first).unwrap(),
            session_preview_sha256(&second).unwrap()
        );
    }

    #[test]
    fn session_preview_digest_changes_when_a_message_changes() {
        let first = json!({
            "schema": "relay.adapter.handoff-preview.v1",
            "exported_at": "2026-08-08T00:00:00Z",
            "conversation": {"messages": [{
                "id": "message-1",
                "blocks": [{"kind": "text", "text": "before"}]
            }]}
        });
        let second = json!({
            "schema": "relay.adapter.handoff-preview.v1",
            "exported_at": "2026-08-09T00:00:00Z",
            "conversation": {"messages": [{
                "id": "message-1",
                "blocks": [{"kind": "text", "text": "after"}]
            }]}
        });

        assert_ne!(
            session_preview_sha256(&first).unwrap(),
            session_preview_sha256(&second).unwrap()
        );
    }
}
