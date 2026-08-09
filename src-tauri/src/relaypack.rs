use crate::adapter;
use crate::git;
use crate::process::{
    bytes_to_trimmed_string, canonical_existing_directory, find_executable_on_path,
    run_process_with_removed_environment, ProcessOutput, ProcessRunError,
};
use crate::sensitive;
use crate::types::{
    AgentProvider, CommandError, ExportRelaypackRequest, ExportRelaypackResult, GitFileChange,
    InspectRelaypackResult, RelaypackDiagnosticPreview, RelaypackPreview, RestoreRelaypackRequest,
    RestoreRelaypackResult, SessionStateInput,
};
use aes_gcm::aead::{Aead, KeyInit, Payload as AeadPayload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD};
use base64::Engine;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tempfile::{Builder as TempBuilder, TempDir};
use unicode_normalization::UnicodeNormalization;
use url::Url;
use uuid::Uuid;

const PACKAGE_MAGIC: &[u8; 8] = b"RELAYPK1";
const PACKAGE_SCHEMA: &str = "relay.package.v1";
const HANDOFF_SCHEMA: &str = "relay.handoff.v1";
const ADAPTER_PREVIEW_SCHEMA: &str = "relay.adapter.handoff-preview.v1";
const NONCE_LENGTH: usize = 12;
const KEY_LENGTH: usize = 32;
const MAX_PLAINTEXT_BYTES: usize = 32 * 1024 * 1024;
const MAX_CIPHERTEXT_BYTES: usize = 32 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: usize = 20 * 1024 * 1024;
const MAX_SINGLE_FILE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_UNTRACKED_FILES: usize = 500;
const MAX_GIT_OUTPUT: usize = 24 * 1024 * 1024;
const MAX_GIT_STDERR: usize = 256 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const HANDOFF_ID_BYTES: usize = 32;
const HANDOFF_DIRECTORY_ATTEMPTS: usize = 16;
const MAX_GIT_EXCLUDE_BYTES: usize = 4 * 1024 * 1024;

#[cfg(windows)]
const GIT_NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const GIT_NULL_DEVICE: &str = "/dev/null";

const GIT_CONTEXT_ENVIRONMENT: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_SYSTEM",
    "GIT_DIR",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PackageEnvelope {
    schema: String,
    package_id: String,
    handoff: Value,
    payloads: Vec<PackagePayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PackagePayload {
    asset_id: String,
    archive_path: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    byte_length: u64,
    sha256: String,
    data_base64: String,
}

impl PackagePayload {
    fn from_bytes(
        asset_id: impl Into<String>,
        archive_path: impl Into<String>,
        kind: impl Into<String>,
        mode: Option<String>,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            asset_id: asset_id.into(),
            archive_path: archive_path.into(),
            kind: kind.into(),
            mode,
            byte_length: bytes.len() as u64,
            sha256: sha256_hex(&bytes),
            data_base64: BASE64.encode(bytes),
        }
    }

    fn decode(&self) -> Result<Vec<u8>, CommandError> {
        let bytes = BASE64.decode(&self.data_base64).map_err(|_| {
            CommandError::new(
                "relaypack_invalid",
                format!("payload '{}' is not valid base64", self.asset_id),
            )
        })?;
        if bytes.len() as u64 != self.byte_length {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!(
                    "payload '{}' length does not match its manifest",
                    self.asset_id
                ),
            ));
        }
        if sha256_hex(&bytes) != self.sha256 {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!("payload '{}' SHA-256 does not match", self.asset_id),
            ));
        }
        Ok(bytes)
    }
}

#[derive(Debug)]
struct LoadedRelaypack {
    path: PathBuf,
    ciphertext_sha256: String,
    ciphertext_bytes: u64,
    envelope: PackageEnvelope,
    preview: RelaypackPreview,
}

#[derive(Debug, Default)]
struct GitCapture {
    included: bool,
    root: Option<PathBuf>,
    branch: Option<String>,
    head: Option<String>,
    upstream: Option<String>,
    base: Option<String>,
    canonical_remote: Option<String>,
    remote_fingerprint: Option<String>,
    object_format: String,
    bundle_asset_id: Option<String>,
    staged_asset_id: Option<String>,
    unstaged_asset_id: Option<String>,
    untracked: Vec<CapturedUntracked>,
    omitted_staged_files: usize,
    omitted_unstaged_files: usize,
    omitted_untracked_files: usize,
    payloads: Vec<PackagePayload>,
    diagnostics: Vec<Value>,
    local_commits_status: String,
    local_commits_note: Option<String>,
    staged_status: String,
    unstaged_status: String,
    lfs_status: String,
}

#[derive(Debug, Clone)]
struct CapturedUntracked {
    logical_path: String,
    asset_id: String,
    mode: String,
}

pub fn export_relaypack(
    request: ExportRelaypackRequest,
) -> Result<ExportRelaypackResult, CommandError> {
    let adapter_preview = adapter::export_session(
        request.agent,
        &request.session_id,
        request.claude_home.clone(),
        request.codex_home.clone(),
    )?;
    export_relaypack_with_preview(request, adapter_preview)
}

fn export_relaypack_with_preview(
    request: ExportRelaypackRequest,
    adapter_preview: Value,
) -> Result<ExportRelaypackResult, CommandError> {
    verify_session_preview_unchanged(&request.preview_sha256, &adapter_preview)?;
    let output_path = validate_new_relaypack_path(&request.output_path)?;
    export_relaypack_from_preview(request, adapter_preview, output_path)
}

fn verify_session_preview_unchanged(
    expected_sha256: &str,
    adapter_preview: &Value,
) -> Result<(), CommandError> {
    let actual_sha256 = adapter::session_preview_sha256(adapter_preview)?;
    if expected_sha256 != actual_sha256 {
        return Err(CommandError::new(
            "session_preview_changed",
            "the session changed after it was previewed; review the share contents again before exporting",
        )
        .with_details(json!({
            "expected_preview_sha256": expected_sha256,
            "actual_preview_sha256": actual_sha256
        })));
    }
    Ok(())
}

fn export_relaypack_from_preview(
    request: ExportRelaypackRequest,
    adapter_preview: Value,
    output_path: PathBuf,
) -> Result<ExportRelaypackResult, CommandError> {
    let package_id = format!("pkg.{}", Uuid::new_v4().simple());
    let created_at = now_rfc3339();
    let mut capture = if request.include_git {
        capture_git(&request)?
    } else {
        GitCapture {
            local_commits_status: "omitted".into(),
            local_commits_note: Some("Git content was not selected.".into()),
            staged_status: "omitted".into(),
            unstaged_status: "omitted".into(),
            diagnostics: vec![diagnostic(
                "GIT_EXCLUDED",
                "info",
                "git",
                "Git content was not selected for this package.",
            )],
            ..GitCapture::default()
        }
    };

    let mut handoff = build_handoff(
        &adapter_preview,
        &request,
        &capture,
        &package_id,
        &created_at,
    )?;
    let sensitive_findings = scan_selected_sensitive_content(&handoff, &capture)?;
    if !sensitive_findings.is_empty() {
        if !request.allow_sensitive_content {
            return Err(CommandError::new(
                "sensitive_content_confirmation_required",
                "selected content may contain sensitive information and requires explicit confirmation",
            )
            .with_details(json!({"findings": sensitive_findings})));
        }
        append_sensitive_diagnostics(&mut handoff, &sensitive_findings)?;
    }
    let handoff_markdown = render_handoff_markdown(&handoff);
    let handoff_payload = PackagePayload::from_bytes(
        "asset.handoff",
        "handoff/HANDOFF.md",
        "handoff_document",
        Some("100444".into()),
        handoff_markdown.into_bytes(),
    );
    append_asset_manifest(
        &mut handoff,
        &handoff_payload,
        "handoff_document",
        None,
        Some("HANDOFF.md"),
    )?;
    capture.payloads.push(handoff_payload);

    let envelope = PackageEnvelope {
        schema: PACKAGE_SCHEMA.into(),
        package_id: package_id.clone(),
        handoff,
        payloads: capture.payloads,
    };
    validate_envelope(&envelope)?;
    let plaintext = serde_json::to_vec(&envelope).map_err(|error| {
        CommandError::new(
            "relaypack_encode_failed",
            format!("cannot encode Relay package: {error}"),
        )
    })?;
    if plaintext.len() > MAX_PLAINTEXT_BYTES {
        return Err(CommandError::new(
            "relaypack_too_large",
            format!(
                "Relay package plaintext is {} bytes; the limit is {MAX_PLAINTEXT_BYTES}",
                plaintext.len()
            ),
        ));
    }

    let compressed = zstd::stream::encode_all(Cursor::new(plaintext), 3).map_err(|error| {
        CommandError::new(
            "relaypack_compression_failed",
            format!("cannot compress Relay package: {error}"),
        )
    })?;
    let mut key = [0_u8; KEY_LENGTH];
    let mut nonce_bytes = [0_u8; NONCE_LENGTH];
    getrandom::fill(&mut key).map_err(|error| {
        CommandError::new(
            "relaypack_random_failed",
            format!("cannot generate package key: {error}"),
        )
    })?;
    getrandom::fill(&mut nonce_bytes).map_err(|error| {
        CommandError::new(
            "relaypack_random_failed",
            format!("cannot generate package nonce: {error}"),
        )
    })?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|_| CommandError::new("relaypack_encrypt_failed", "invalid AES key"))?;
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            AeadPayload {
                msg: &compressed,
                aad: PACKAGE_MAGIC,
            },
        )
        .map_err(|_| CommandError::new("relaypack_encrypt_failed", "AES-GCM encryption failed"))?;
    let mut package_bytes =
        Vec::with_capacity(PACKAGE_MAGIC.len() + NONCE_LENGTH + ciphertext.len());
    package_bytes.extend_from_slice(PACKAGE_MAGIC);
    package_bytes.extend_from_slice(&nonce_bytes);
    package_bytes.extend_from_slice(&ciphertext);
    if package_bytes.len() > MAX_CIPHERTEXT_BYTES {
        return Err(CommandError::new(
            "relaypack_too_large",
            "encrypted Relay package exceeds the ciphertext size limit",
        ));
    }

    write_new_private_file(&output_path, &package_bytes)?;
    let preview = preview_from_handoff(&envelope.handoff)?;
    let warnings = warning_previews(&preview);
    Ok(ExportRelaypackResult {
        package_path: output_path.to_string_lossy().into_owned(),
        key_fragment: URL_SAFE_NO_PAD.encode(key),
        ciphertext_sha256: sha256_hex(&package_bytes),
        ciphertext_bytes: package_bytes.len() as u64,
        preview,
        warnings,
    })
}

#[cfg(test)]
pub(crate) fn export_test_relaypack(
    output_path: &Path,
) -> Result<ExportRelaypackResult, CommandError> {
    let request = ExportRelaypackRequest {
        agent: AgentProvider::Codex,
        session_id: "share-smoke-session".into(),
        preview_sha256: String::new(),
        output_path: output_path.to_string_lossy().into_owned(),
        claude_home: None,
        codex_home: None,
        repository_path: None,
        include_git: false,
        include_local_commits: None,
        include_staged: None,
        include_unstaged: None,
        selected_staged: Vec::new(),
        selected_unstaged: Vec::new(),
        selected_untracked: Vec::new(),
        excluded_message_ids: Vec::new(),
        excluded_blocks: Vec::new(),
        allow_sensitive_content: false,
        session_state: None,
        include_conversation: true,
        include_tool_evidence: true,
        include_project_instructions: true,
        include_environment: true,
    };
    let preview = json!({
        "schema": ADAPTER_PREVIEW_SCHEMA,
        "exported_at": "2026-08-08T00:00:00Z",
        "source": {
            "agent": "codex",
            "session_id": "share-smoke-session",
            "source_path": "/private/relay-test/session.jsonl",
            "read_only": true
        },
        "session": {"title": "Relay local share smoke test"},
        "environment": {"cwd": "/private/relay-test/project"},
        "project": {"key": "relay-smoke", "name": "Relay Smoke"},
        "conversation": {
            "messages": [{
                "id": "message-1",
                "role": "assistant",
                "blocks": [
                    {
                        "kind": "text",
                        "classification": "user_visible",
                        "text": "Local encrypted sharing smoke test"
                    },
                    {
                        "kind": "tool_call",
                        "classification": "project_owned",
                        "call_id": "call-smoke-1",
                        "name": "exec_command",
                        "input": {"cmd": "cargo test"},
                        "status": "completed",
                        "replay_policy": "never"
                    },
                    {
                        "kind": "tool_result",
                        "classification": "project_owned",
                        "call_id": "call-smoke-1",
                        "output": "test result: ok",
                        "status": "success",
                        "replay_policy": "never"
                    }
                ]
            }]
        },
        "assets": [],
        "diagnostics": {
            "warnings": [],
            "completeness": {
                "status": "complete",
                "total_lines": 1,
                "parsed_lines": 1,
                "damaged_lines": 0,
                "unknown_records": 0,
                "hidden_records": 0,
                "unsupported_blocks": 0,
                "orphan_tool_results": 0,
                "unmatched_tool_calls": 0
            }
        },
        "export": {
            "adapter_version": "0.1.0",
            "protocol": "relay.adapter.v1",
            "native_history": false
        }
    });
    let mut request = request;
    request.preview_sha256 = adapter::session_preview_sha256(&preview)?;
    export_relaypack_with_preview(request, preview)
}

pub fn inspect_relaypack(path: &str, key: &str) -> Result<InspectRelaypackResult, CommandError> {
    let loaded = load_relaypack(path, key)?;
    let warnings = warning_previews(&loaded.preview);
    Ok(InspectRelaypackResult {
        package_path: loaded.path.to_string_lossy().into_owned(),
        ciphertext_sha256: loaded.ciphertext_sha256,
        ciphertext_bytes: loaded.ciphertext_bytes,
        preview: loaded.preview,
        warnings,
    })
}

pub fn restore_relaypack(
    request: RestoreRelaypackRequest,
) -> Result<RestoreRelaypackResult, CommandError> {
    let loaded = load_relaypack(&request.package_path, &request.key)?;
    restore_loaded_relaypack(loaded, request)
}

fn load_relaypack(path: &str, key: &str) -> Result<LoadedRelaypack, CommandError> {
    let path = canonical_regular_file(Path::new(path), "Relay package")?;
    let metadata = fs::metadata(&path).map_err(|error| {
        CommandError::new(
            "relaypack_read_failed",
            format!("cannot inspect Relay package: {error}"),
        )
    })?;
    if metadata.len() > MAX_CIPHERTEXT_BYTES as u64 {
        return Err(CommandError::new(
            "relaypack_too_large",
            "encrypted Relay package exceeds the ciphertext size limit",
        ));
    }
    let package_bytes = read_file_limited(&path, MAX_CIPHERTEXT_BYTES)?;
    if package_bytes.len() < PACKAGE_MAGIC.len() + NONCE_LENGTH + 16
        || &package_bytes[..PACKAGE_MAGIC.len()] != PACKAGE_MAGIC
    {
        return Err(CommandError::new(
            "relaypack_invalid",
            "file is not a supported Relay package",
        ));
    }
    let key = URL_SAFE_NO_PAD.decode(key.trim()).map_err(|_| {
        CommandError::new(
            "relaypack_key_invalid",
            "key must be an unpadded base64url Relay key",
        )
    })?;
    if key.len() != KEY_LENGTH {
        return Err(CommandError::new(
            "relaypack_key_invalid",
            "Relay key must decode to 32 bytes",
        ));
    }
    let nonce_start = PACKAGE_MAGIC.len();
    let cipher_start = nonce_start + NONCE_LENGTH;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|_| CommandError::new("relaypack_key_invalid", "invalid AES key"))?;
    let compressed = cipher
        .decrypt(
            Nonce::from_slice(&package_bytes[nonce_start..cipher_start]),
            AeadPayload {
                msg: &package_bytes[cipher_start..],
                aad: PACKAGE_MAGIC,
            },
        )
        .map_err(|_| {
            CommandError::new(
                "relaypack_auth_failed",
                "Relay package authentication failed; the key is wrong or the ciphertext was changed",
            )
        })?;

    let decoder = zstd::stream::read::Decoder::new(Cursor::new(compressed)).map_err(|_| {
        CommandError::new(
            "relaypack_invalid",
            "Relay package contains an invalid compressed envelope",
        )
    })?;
    let mut plaintext = Vec::new();
    decoder
        .take((MAX_PLAINTEXT_BYTES + 1) as u64)
        .read_to_end(&mut plaintext)
        .map_err(|_| {
            CommandError::new(
                "relaypack_invalid",
                "Relay package could not be decompressed safely",
            )
        })?;
    if plaintext.len() > MAX_PLAINTEXT_BYTES {
        return Err(CommandError::new(
            "relaypack_too_large",
            "decompressed Relay package exceeds the plaintext size limit",
        ));
    }
    let envelope: PackageEnvelope = serde_json::from_slice(&plaintext).map_err(|error| {
        CommandError::new(
            "relaypack_invalid",
            format!("Relay package envelope is invalid JSON: {error}"),
        )
    })?;
    validate_envelope(&envelope)?;
    let preview = preview_from_handoff(&envelope.handoff)?;

    Ok(LoadedRelaypack {
        path,
        ciphertext_sha256: sha256_hex(&package_bytes),
        ciphertext_bytes: package_bytes.len() as u64,
        envelope,
        preview,
    })
}

fn validate_envelope(envelope: &PackageEnvelope) -> Result<(), CommandError> {
    if envelope.schema != PACKAGE_SCHEMA {
        return Err(CommandError::new(
            "relaypack_invalid",
            format!("unsupported package schema '{}'", envelope.schema),
        ));
    }
    if envelope.package_id.is_empty()
        || envelope.handoff.get("package_id").and_then(Value::as_str)
            != Some(envelope.package_id.as_str())
    {
        return Err(CommandError::new(
            "relaypack_invalid",
            "package_id is missing or inconsistent",
        ));
    }

    validate_handoff_schema(&envelope.handoff)?;

    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    let mut path_collisions = HashSet::new();
    let mut declared_payload = 0_u64;
    let mut decoded_payload = 0_usize;
    for payload in &envelope.payloads {
        if !ids.insert(payload.asset_id.clone()) {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!("duplicate payload asset id '{}'", payload.asset_id),
            ));
        }
        validate_archive_path(&payload.archive_path)?;
        if !paths.insert(payload.archive_path.clone())
            || !path_collisions.insert(collision_key(&payload.archive_path))
        {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!(
                    "duplicate or conflicting archive path '{}'",
                    payload.archive_path
                ),
            ));
        }
        validate_payload_metadata(payload)?;
        declared_payload = declared_payload
            .checked_add(payload.byte_length)
            .ok_or_else(|| {
                CommandError::new("relaypack_too_large", "payload byte total overflowed")
            })?;
        if declared_payload > MAX_PAYLOAD_BYTES as u64 {
            return Err(CommandError::new(
                "relaypack_too_large",
                "declared payloads exceed the package payload limit",
            ));
        }
        let expected_base64_length =
            payload
                .byte_length
                .div_ceil(3)
                .checked_mul(4)
                .ok_or_else(|| {
                    CommandError::new("relaypack_too_large", "payload base64 length overflowed")
                })?;
        if payload.data_base64.len() as u64 != expected_base64_length {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!(
                    "payload '{}' base64 length does not match its manifest",
                    payload.asset_id
                ),
            ));
        }
        let bytes = payload.decode()?;
        decoded_payload = decoded_payload.checked_add(bytes.len()).ok_or_else(|| {
            CommandError::new("relaypack_too_large", "payload byte total overflowed")
        })?;
        if decoded_payload > MAX_PAYLOAD_BYTES {
            return Err(CommandError::new(
                "relaypack_too_large",
                "decoded payloads exceed the package payload limit",
            ));
        }
    }

    validate_handoff_relations(&envelope.handoff, &envelope.payloads)?;
    if envelope
        .handoff
        .pointer("/git/included")
        .and_then(Value::as_bool)
        == Some(true)
    {
        restore_material(envelope)?;
    }
    Ok(())
}

fn validate_payload_metadata(payload: &PackagePayload) -> Result<(), CommandError> {
    match payload.kind.as_str() {
        "untracked_file" => {
            if payload.byte_length > MAX_SINGLE_FILE_BYTES {
                return Err(CommandError::new(
                    "relaypack_too_large",
                    format!("untracked payload '{}' exceeds 5 MiB", payload.asset_id),
                ));
            }
            if !matches!(payload.mode.as_deref(), Some("100644" | "100755")) {
                return Err(CommandError::new(
                    "relaypack_invalid",
                    format!(
                        "untracked payload '{}' has an invalid mode",
                        payload.asset_id
                    ),
                ));
            }
        }
        "git_bundle" | "git_patch" | "handoff_document" => {
            if payload.mode.as_deref() != Some("100444") {
                return Err(CommandError::new(
                    "relaypack_invalid",
                    format!("payload '{}' must be read-only", payload.asset_id),
                ));
            }
        }
        "conversation_attachment" | "tool_output" | "source_snapshot" | "other" => {
            if payload.mode.is_some() {
                return Err(CommandError::new(
                    "relaypack_invalid",
                    format!("payload '{}' has an unexpected mode", payload.asset_id),
                ));
            }
        }
        _ => {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!("payload '{}' has an unsupported kind", payload.asset_id),
            ))
        }
    }
    Ok(())
}

fn validate_handoff_schema(handoff: &Value) -> Result<(), CommandError> {
    let schema: Value = serde_json::from_str(include_str!(
        "../../schemas/relay-handoff-v1.schema.json"
    ))
    .map_err(|error| {
        CommandError::new(
            "relaypack_validator_failed",
            format!("bundled handoff schema is invalid: {error}"),
        )
    })?;
    let validator = jsonschema::validator_for(&schema).map_err(|error| {
        CommandError::new(
            "relaypack_validator_failed",
            format!("cannot compile bundled handoff schema: {error}"),
        )
    })?;
    validator.validate(handoff).map_err(|error| {
        CommandError::new(
            "relaypack_invalid",
            format!("handoff does not match relay.handoff.v1: {error}"),
        )
    })
}

fn validate_handoff_relations(
    handoff: &Value,
    payloads: &[PackagePayload],
) -> Result<(), CommandError> {
    if handoff.get("schema").and_then(Value::as_str) != Some(HANDOFF_SCHEMA) {
        return Err(CommandError::new(
            "relaypack_invalid",
            "handoff schema must be relay.handoff.v1",
        ));
    }
    if contains_forbidden_key(handoff, "source_path") || contains_forbidden_key(handoff, "cwd") {
        return Err(CommandError::new(
            "relaypack_invalid",
            "formal handoff contains a local source_path or cwd",
        ));
    }

    let assets = handoff
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| CommandError::new("relaypack_invalid", "handoff assets are missing"))?;
    let payload_by_id: HashMap<&str, &PackagePayload> = payloads
        .iter()
        .map(|payload| (payload.asset_id.as_str(), payload))
        .collect();
    let mut asset_by_id = HashMap::new();
    let mut handoff_manifest_count = 0_usize;
    for asset in assets {
        let id = required_string(asset, "id", "asset")?;
        if asset_by_id.insert(id, asset).is_some() {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!("duplicate handoff asset id '{id}'"),
            ));
        }
        let status = asset.get("status").and_then(Value::as_str);
        if status == Some("included") {
            let payload = payload_by_id.get(id).ok_or_else(|| {
                CommandError::new(
                    "relaypack_invalid",
                    format!("included asset '{id}' has no payload"),
                )
            })?;
            if asset.get("kind").and_then(Value::as_str) != Some(payload.kind.as_str()) {
                return Err(CommandError::new(
                    "relaypack_invalid",
                    format!("asset '{id}' kind does not match its payload"),
                ));
            }
            if asset.get("archive_path").and_then(Value::as_str)
                != Some(payload.archive_path.as_str())
                || asset.get("sha256").and_then(Value::as_str) != Some(payload.sha256.as_str())
                || asset.get("byte_length").and_then(Value::as_u64) != Some(payload.byte_length)
            {
                return Err(CommandError::new(
                    "relaypack_invalid",
                    format!("asset '{id}' does not match its payload manifest"),
                ));
            }
            if payload.kind == "handoff_document" {
                handoff_manifest_count += 1;
            }
        } else if payload_by_id.contains_key(id) {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!("non-included asset '{id}' unexpectedly has a payload"),
            ));
        }
    }
    if payloads
        .iter()
        .any(|payload| !asset_by_id.contains_key(payload.asset_id.as_str()))
    {
        return Err(CommandError::new(
            "relaypack_invalid",
            "package contains a payload not declared by handoff assets",
        ));
    }
    let handoff_payload_count = payloads
        .iter()
        .filter(|payload| payload.kind == "handoff_document")
        .count();
    if handoff_manifest_count != 1 || handoff_payload_count != 1 {
        return Err(CommandError::new(
            "relaypack_invalid",
            "package must contain exactly one included HANDOFF document",
        ));
    }

    let records = handoff
        .pointer("/conversation/records")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CommandError::new("relaypack_invalid", "conversation records are missing")
        })?;
    let mut record_ids = HashSet::new();
    let mut block_ids = HashSet::new();
    let mut call_ids = HashSet::new();
    let mut result_call_ids = HashSet::new();
    let mut parents = HashMap::new();
    let mut branch_ids = HashSet::new();
    for record in records {
        let record_id = required_string(record, "id", "conversation record")?;
        if !record_ids.insert(record_id.to_owned()) {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!("duplicate conversation record id '{record_id}'"),
            ));
        }
        if let Some(branch_id) = record.get("branch_id").and_then(Value::as_str) {
            branch_ids.insert(branch_id.to_owned());
        }
        if let Some(blocks) = record.get("blocks").and_then(Value::as_array) {
            for block in blocks {
                validate_block(
                    block,
                    &asset_by_id,
                    &mut block_ids,
                    &mut call_ids,
                    &mut result_call_ids,
                )?;
            }
        }
        parents.insert(
            record_id.to_owned(),
            record
                .get("parent_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        );
    }
    for (record_id, parent) in &parents {
        if let Some(parent) = parent {
            if !record_ids.contains(parent) {
                return Err(CommandError::new(
                    "relaypack_invalid",
                    format!("record '{record_id}' parent '{parent}' does not exist"),
                ));
            }
        }
    }
    validate_record_roots_and_cycles(handoff, &parents)?;
    validate_branch_relations(handoff, &branch_ids)?;
    if call_ids != result_call_ids {
        return Err(CommandError::new(
            "relaypack_invalid",
            "tool call and tool result ids are not paired",
        ));
    }

    validate_export_statistics(handoff, records.len())?;
    validate_git_asset_relations(handoff, &asset_by_id)?;
    for diagnostic in handoff
        .get("diagnostics")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(asset_id) = diagnostic.get("asset_id").and_then(Value::as_str) {
            referenced_asset(asset_id, "diagnostic", &asset_by_id)?;
        }
    }

    let mut logical_paths = HashMap::new();
    collect_repo_uris(handoff, &mut logical_paths)?;
    Ok(())
}

fn validate_export_statistics(handoff: &Value, record_count: usize) -> Result<(), CommandError> {
    let completeness = handoff
        .pointer("/export/completeness")
        .ok_or_else(|| CommandError::new("relaypack_invalid", "export completeness is missing"))?;
    let exported_records = completeness
        .get("exported_records")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            CommandError::new(
                "relaypack_invalid",
                "export completeness is missing exported_records",
            )
        })?;
    if exported_records != record_count as u64 {
        return Err(CommandError::new(
            "relaypack_invalid",
            "exported_records does not match the conversation record count",
        ));
    }
    let omitted_records = completeness
        .get("omitted_records")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            CommandError::new(
                "relaypack_invalid",
                "export completeness is missing omitted_records",
            )
        })?;
    if let Some(source_records) = completeness.get("source_records").and_then(Value::as_u64) {
        let expected_omitted = source_records
            .checked_sub(exported_records)
            .ok_or_else(|| {
                CommandError::new(
                    "relaypack_invalid",
                    "source_records is smaller than exported_records",
                )
            })?;
        if expected_omitted != omitted_records {
            return Err(CommandError::new(
                "relaypack_invalid",
                "source_records, exported_records, and omitted_records are inconsistent",
            ));
        }
    }
    let unknown_records = completeness
        .get("unknown_records")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if unknown_records > omitted_records {
        return Err(CommandError::new(
            "relaypack_invalid",
            "unknown_records cannot exceed omitted_records",
        ));
    }
    if completeness.get("status").and_then(Value::as_str) == Some("complete")
        && (omitted_records > 0 || unknown_records > 0)
    {
        return Err(CommandError::new(
            "relaypack_invalid",
            "a complete export cannot report omitted or unknown records",
        ));
    }
    if handoff.pointer("/export/mode").and_then(Value::as_str) == Some("full")
        && handoff
            .pointer("/export/omissions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|omission| {
                matches!(
                    omission.get("reason").and_then(Value::as_str),
                    Some("redacted_by_user" | "git_excluded")
                )
            })
    {
        return Err(CommandError::new(
            "relaypack_invalid",
            "a full export cannot contain sender-selected omissions",
        ));
    }
    Ok(())
}

fn validate_branch_relations(
    handoff: &Value,
    branch_ids: &HashSet<String>,
) -> Result<(), CommandError> {
    let active_branch = handoff
        .pointer("/conversation/active_branch_id")
        .and_then(Value::as_str);
    match (branch_ids.is_empty(), active_branch) {
        (true, Some(active_branch)) => Err(CommandError::new(
            "relaypack_invalid",
            format!("active branch '{active_branch}' is not used by any conversation record"),
        )),
        (false, None) => Err(CommandError::new(
            "relaypack_invalid",
            "conversation records use branch ids but active_branch_id is missing",
        )),
        (false, Some(active_branch)) if !branch_ids.contains(active_branch) => {
            Err(CommandError::new(
                "relaypack_invalid",
                format!("active branch '{active_branch}' is not used by any conversation record"),
            ))
        }
        _ => Ok(()),
    }
}

fn validate_git_asset_relations(
    handoff: &Value,
    assets: &HashMap<&str, &Value>,
) -> Result<(), CommandError> {
    let git_included = handoff
        .pointer("/git/included")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    for (pointer, label, expected_kind) in [
        (
            "/git/capture/local_commits",
            "local commit capture",
            "git_bundle",
        ),
        (
            "/git/capture/staged_patch",
            "staged patch capture",
            "git_patch",
        ),
        (
            "/git/capture/unstaged_patch",
            "unstaged patch capture",
            "git_patch",
        ),
    ] {
        let capture = handoff.pointer(pointer).ok_or_else(|| {
            CommandError::new("relaypack_invalid", format!("{label} manifest is missing"))
        })?;
        let status = capture.get("status").and_then(Value::as_str);
        match (status, capture.get("asset_id").and_then(Value::as_str)) {
            (Some("included"), Some(asset_id)) if git_included => {
                require_included_asset(asset_id, label, assets, Some(expected_kind))?;
            }
            (Some("included"), Some(_)) => {
                return Err(CommandError::new(
                    "relaypack_invalid",
                    format!("{label} cannot be included when Git content is excluded"),
                ));
            }
            (Some("included"), None) => {
                return Err(CommandError::new(
                    "relaypack_invalid",
                    format!("{label} is included but has no asset_id"),
                ));
            }
            (_, Some(_)) => {
                return Err(CommandError::new(
                    "relaypack_invalid",
                    format!("{label} has an asset_id without included status"),
                ));
            }
            _ => {}
        }
    }

    let untracked = handoff
        .pointer("/git/capture/untracked_files")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CommandError::new("relaypack_invalid", "untracked file manifest is missing")
        })?;
    if untracked.len() > MAX_UNTRACKED_FILES {
        return Err(CommandError::new(
            "relaypack_invalid",
            "untracked file manifest exceeds the file count limit",
        ));
    }
    if !git_included && !untracked.is_empty() {
        return Err(CommandError::new(
            "relaypack_invalid",
            "untracked files cannot be included when Git content is excluded",
        ));
    }
    let mut asset_ids = HashSet::new();
    for entry in untracked {
        let asset_id = required_string(entry, "asset_id", "untracked file")?;
        if !asset_ids.insert(asset_id) {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!("untracked asset '{asset_id}' is referenced more than once"),
            ));
        }
        let asset =
            require_included_asset(asset_id, "untracked file", assets, Some("untracked_file"))?;
        let logical_path = required_string(entry, "logical_path", "untracked file")?;
        if asset.get("logical_path").and_then(Value::as_str) != Some(logical_path) {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!("untracked asset '{asset_id}' has an inconsistent logical path"),
            ));
        }
    }
    Ok(())
}

fn referenced_asset<'a>(
    asset_id: &str,
    label: &str,
    assets: &'a HashMap<&str, &Value>,
) -> Result<&'a Value, CommandError> {
    assets.get(asset_id).copied().ok_or_else(|| {
        CommandError::new(
            "relaypack_invalid",
            format!("{label} references missing asset '{asset_id}'"),
        )
    })
}

fn require_included_asset<'a>(
    asset_id: &str,
    label: &str,
    assets: &'a HashMap<&str, &Value>,
    expected_kind: Option<&str>,
) -> Result<&'a Value, CommandError> {
    let asset = referenced_asset(asset_id, label, assets)?;
    if asset.get("status").and_then(Value::as_str) != Some("included") {
        return Err(CommandError::new(
            "relaypack_invalid",
            format!("{label} references asset '{asset_id}' that is not included"),
        ));
    }
    if expected_kind.is_some_and(|kind| asset.get("kind").and_then(Value::as_str) != Some(kind)) {
        return Err(CommandError::new(
            "relaypack_invalid",
            format!("{label} references asset '{asset_id}' with the wrong kind"),
        ));
    }
    Ok(asset)
}

fn validate_record_roots_and_cycles(
    handoff: &Value,
    parents: &HashMap<String, Option<String>>,
) -> Result<(), CommandError> {
    let roots = handoff
        .pointer("/conversation/root_record_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CommandError::new(
                "relaypack_invalid",
                "conversation root_record_ids are missing",
            )
        })?;
    let mut declared_roots = HashSet::new();
    for root in roots {
        let root = root.as_str().ok_or_else(|| {
            CommandError::new(
                "relaypack_invalid",
                "conversation root_record_ids contains a non-string value",
            )
        })?;
        let parent = parents.get(root).ok_or_else(|| {
            CommandError::new(
                "relaypack_invalid",
                format!("conversation root record '{root}' does not exist"),
            )
        })?;
        if parent.is_some() {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!("conversation root record '{root}' has a parent"),
            ));
        }
        if !declared_roots.insert(root.to_owned()) {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!("conversation root record '{root}' is duplicated"),
            ));
        }
    }
    let actual_roots: HashSet<String> = parents
        .iter()
        .filter_map(|(record_id, parent)| parent.is_none().then_some(record_id.clone()))
        .collect();
    if declared_roots != actual_roots {
        return Err(CommandError::new(
            "relaypack_invalid",
            "conversation root_record_ids do not match the parent graph",
        ));
    }

    for start in parents.keys() {
        let mut path = HashSet::new();
        let mut current = Some(start.as_str());
        while let Some(record_id) = current {
            if !path.insert(record_id) {
                return Err(CommandError::new(
                    "relaypack_invalid",
                    format!("conversation parent cycle includes record '{record_id}'"),
                ));
            }
            current = parents.get(record_id).and_then(|parent| parent.as_deref());
        }
    }
    Ok(())
}

fn validate_block(
    block: &Value,
    assets: &HashMap<&str, &Value>,
    block_ids: &mut HashSet<String>,
    call_ids: &mut HashSet<String>,
    result_call_ids: &mut HashSet<String>,
) -> Result<(), CommandError> {
    let id = required_string(block, "id", "content block")?;
    if !block_ids.insert(id.to_owned()) {
        return Err(CommandError::new(
            "relaypack_invalid",
            format!("duplicate content block id '{id}'"),
        ));
    }
    match block.get("classification").and_then(Value::as_str) {
        Some("user_visible" | "project_owned") => {}
        _ => {
            return Err(CommandError::new(
                "relaypack_invalid",
                "handoff contains a non-shareable classification",
            ))
        }
    }
    match block.get("kind").and_then(Value::as_str) {
        Some("tool_call") => {
            if block.get("replay_policy").and_then(Value::as_str) != Some("never") {
                return Err(CommandError::new(
                    "relaypack_invalid",
                    "tool_call replay_policy must be never",
                ));
            }
            let call_id = required_string(block, "call_id", "tool_call")?;
            if !call_ids.insert(call_id.to_owned()) {
                return Err(CommandError::new(
                    "relaypack_invalid",
                    format!("duplicate tool call id '{call_id}'"),
                ));
            }
        }
        Some("tool_result") => {
            if block.get("replay_policy").and_then(Value::as_str) != Some("never") {
                return Err(CommandError::new(
                    "relaypack_invalid",
                    "tool_result replay_policy must be never",
                ));
            }
            let call_id = required_string(block, "call_id", "tool_result")?;
            if !result_call_ids.insert(call_id.to_owned()) {
                return Err(CommandError::new(
                    "relaypack_invalid",
                    format!("duplicate tool result id '{call_id}'"),
                ));
            }
            if let Some(content) = block.get("content").and_then(Value::as_array) {
                for child in content {
                    validate_block(child, assets, block_ids, call_ids, result_call_ids)?;
                }
            }
        }
        Some("asset_ref") => {
            let asset_id = required_string(block, "asset_id", "asset_ref")?;
            referenced_asset(asset_id, "asset_ref", assets)?;
        }
        Some("source_context") => {
            if let Some(asset_id) = block.get("asset_id").and_then(Value::as_str) {
                referenced_asset(asset_id, "source_context", assets)?;
            }
            if let Some(line_range) = block.get("line_range") {
                let start = line_range
                    .get("start")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        CommandError::new(
                            "relaypack_invalid",
                            "source_context line_range is missing start",
                        )
                    })?;
                if line_range
                    .get("end")
                    .and_then(Value::as_u64)
                    .is_some_and(|end| end < start)
                {
                    return Err(CommandError::new(
                        "relaypack_invalid",
                        "source_context line_range end is before start",
                    ));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn collect_repo_uris(
    value: &Value,
    seen: &mut HashMap<String, String>,
) -> Result<(), CommandError> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if matches!(key.as_str(), "logical_path" | "path") {
                    if let Some(uri) = child.as_str() {
                        if uri.starts_with("repo://") {
                            record_repo_uri(uri, seen)?;
                        }
                    }
                } else if matches!(key.as_str(), "important_files" | "paths") {
                    for uri in child
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .filter(|uri| uri.starts_with("repo://"))
                    {
                        record_repo_uri(uri, seen)?;
                    }
                }
                collect_repo_uris(child, seen)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_repo_uris(item, seen)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn record_repo_uri(uri: &str, seen: &mut HashMap<String, String>) -> Result<(), CommandError> {
    validate_repo_uri(uri)?;
    let collision = collision_key(uri);
    if let Some(existing) = seen.get(&collision) {
        if existing != uri {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!(
                    "logical paths '{existing}' and '{uri}' conflict by case or Unicode normalization"
                ),
            ));
        }
    } else {
        seen.insert(collision, uri.to_owned());
    }
    Ok(())
}

fn contains_forbidden_key(value: &Value, forbidden: &str) -> bool {
    match value {
        Value::Object(object) => object
            .iter()
            .any(|(key, child)| key == forbidden || contains_forbidden_key(child, forbidden)),
        Value::Array(items) => items
            .iter()
            .any(|child| contains_forbidden_key(child, forbidden)),
        _ => false,
    }
}

fn preview_from_handoff(handoff: &Value) -> Result<RelaypackPreview, CommandError> {
    let agent = match handoff.pointer("/source/agent").and_then(Value::as_str) {
        Some("claude_code") => AgentProvider::ClaudeCode,
        Some("codex") => AgentProvider::Codex,
        _ => AgentProvider::Unknown,
    };
    let diagnostics = handoff
        .get("diagnostics")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|diagnostic| RelaypackDiagnosticPreview {
            code: diagnostic
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("UNKNOWN")
                .into(),
            severity: diagnostic
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or("warning")
                .into(),
            scope: diagnostic
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("other")
                .into(),
            message: diagnostic
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Relay package diagnostic")
                .into(),
        })
        .collect();
    Ok(RelaypackPreview {
        package_id: required_string(handoff, "package_id", "handoff")?.into(),
        created_at: required_string(handoff, "created_at", "handoff")?.into(),
        source_agent: agent,
        session_id: handoff
            .pointer("/source/session_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        title: handoff
            .pointer("/source/title")
            .and_then(Value::as_str)
            .unwrap_or("Untitled session")
            .into(),
        project_name: handoff
            .pointer("/project/display_name")
            .and_then(Value::as_str)
            .unwrap_or("Unknown project")
            .into(),
        git_included: handoff
            .pointer("/git/included")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        branch: handoff
            .pointer("/git/repository/branch")
            .and_then(Value::as_str)
            .map(str::to_owned),
        head: handoff
            .pointer("/git/repository/head")
            .and_then(Value::as_str)
            .map(str::to_owned),
        conversation_records: handoff
            .pointer("/conversation/records")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        asset_count: handoff
            .get("assets")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        untracked_file_count: handoff
            .pointer("/git/capture/untracked_files")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        diagnostics,
    })
}

fn warning_previews(preview: &RelaypackPreview) -> Vec<RelaypackDiagnosticPreview> {
    preview
        .diagnostics
        .iter()
        .filter(|item| item.severity != "info")
        .cloned()
        .collect()
}

fn validate_new_relaypack_path(raw: &str) -> Result<PathBuf, CommandError> {
    if raw.trim().is_empty() {
        return Err(CommandError::new(
            "invalid_output_path",
            "output_path cannot be empty",
        ));
    }
    let path = Path::new(raw);
    let file_name = path.file_name().ok_or_else(|| {
        CommandError::new(
            "invalid_output_path",
            "output_path must include a file name",
        )
    })?;
    if Path::new(file_name).extension() != Some(OsStr::new("relaypack")) {
        return Err(CommandError::new(
            "invalid_output_path",
            "output_path must end in .relaypack",
        ));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let parent = canonical_existing_directory(parent, "output directory")?;
    let candidate = parent.join(file_name);
    match fs::symlink_metadata(&candidate) {
        Ok(_) => Err(CommandError::new(
            "output_exists",
            format!(
                "Relay will not overwrite '{}'; choose a new path",
                candidate.display()
            ),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(candidate),
        Err(error) => Err(CommandError::new(
            "invalid_output_path",
            format!("cannot inspect output path: {error}"),
        )),
    }
}

fn canonical_regular_file(path: &Path, label: &str) -> Result<PathBuf, CommandError> {
    let link_metadata = fs::symlink_metadata(path).map_err(|error| {
        CommandError::new(
            "invalid_path",
            format!("{label} does not exist or cannot be inspected: {error}"),
        )
    })?;
    if link_metadata.file_type().is_symlink() || !link_metadata.is_file() {
        return Err(CommandError::new(
            "invalid_path",
            format!("{label} must be a non-symlink ordinary file"),
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        CommandError::new("invalid_path", format!("cannot resolve {label}: {error}"))
    })?;
    if !fs::metadata(&canonical).is_ok_and(|metadata| metadata.is_file()) {
        return Err(CommandError::new(
            "invalid_path",
            format!("{label} must resolve to an ordinary file"),
        ));
    }
    Ok(canonical)
}

fn write_new_private_file(path: &Path, bytes: &[u8]) -> Result<(), CommandError> {
    let parent = path.parent().ok_or_else(|| {
        CommandError::new("invalid_output_path", "output path has no parent directory")
    })?;
    let mut temporary = TempBuilder::new()
        .prefix(".relaypack-")
        .tempfile_in(parent)
        .map_err(|error| {
            CommandError::new(
                "relaypack_write_failed",
                format!("cannot create temporary package: {error}"),
            )
        })?;
    temporary.write_all(bytes).map_err(|error| {
        CommandError::new(
            "relaypack_write_failed",
            format!("cannot write encrypted package: {error}"),
        )
    })?;
    temporary.as_file().sync_all().map_err(|error| {
        CommandError::new(
            "relaypack_write_failed",
            format!("cannot sync encrypted package: {error}"),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                CommandError::new(
                    "relaypack_write_failed",
                    format!("cannot protect encrypted package: {error}"),
                )
            })?;
    }
    temporary.persist_noclobber(path).map_err(|error| {
        CommandError::new(
            "relaypack_write_failed",
            format!(
                "cannot create final package without overwriting: {}",
                error.error
            ),
        )
    })?;
    Ok(())
}

fn read_file_limited(path: &Path, limit: usize) -> Result<Vec<u8>, CommandError> {
    let file = File::open(path).map_err(|error| {
        CommandError::new(
            "file_read_failed",
            format!("cannot open '{}': {error}", path.display()),
        )
    })?;
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            CommandError::new(
                "file_read_failed",
                format!("cannot read '{}': {error}", path.display()),
            )
        })?;
    if bytes.len() > limit {
        return Err(CommandError::new(
            "file_too_large",
            format!("'{}' exceeds the {limit} byte limit", path.display()),
        ));
    }
    Ok(bytes)
}

fn validate_archive_path(path: &str) -> Result<(), CommandError> {
    validate_relative_path(path, false)
}

fn validate_repo_uri(uri: &str) -> Result<(), CommandError> {
    let relative = uri.strip_prefix("repo://").ok_or_else(|| {
        CommandError::new(
            "relaypack_invalid",
            format!("logical path '{uri}' is not a repo:// URI"),
        )
    })?;
    if relative.is_empty() {
        return Ok(());
    }
    validate_relative_path(relative, true)
}

fn validate_relative_path(path: &str, reject_git: bool) -> Result<(), CommandError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || Path::new(path).is_absolute()
    {
        return Err(CommandError::new(
            "unsafe_path",
            format!("unsafe relative path '{path}'"),
        ));
    }
    let mut first = true;
    for component in Path::new(path).components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_string_lossy();
                if value.is_empty()
                    || value == "."
                    || value == ".."
                    || (reject_git && first && value.eq_ignore_ascii_case(".git"))
                {
                    return Err(CommandError::new(
                        "unsafe_path",
                        format!("unsafe path component in '{path}'"),
                    ));
                }
                first = false;
            }
            _ => {
                return Err(CommandError::new(
                    "unsafe_path",
                    format!("unsafe path component in '{path}'"),
                ))
            }
        }
    }
    Ok(())
}

fn collision_key(path: &str) -> String {
    path.nfc().collect::<String>().to_lowercase()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn required_string<'a>(value: &'a Value, key: &str, label: &str) -> Result<&'a str, CommandError> {
    value.get(key).and_then(Value::as_str).ok_or_else(|| {
        CommandError::new(
            "relaypack_invalid",
            format!("{label} is missing string field '{key}'"),
        )
    })
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn diagnostic(code: &str, severity: &str, scope: &str, message: &str) -> Value {
    json!({
        "code": code,
        "severity": severity,
        "scope": scope,
        "message": message
    })
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct SensitiveContentFinding {
    code: &'static str,
    label: &'static str,
    scope: &'static str,
    count: usize,
}

fn scan_selected_sensitive_content(
    handoff: &Value,
    capture: &GitCapture,
) -> Result<Vec<SensitiveContentFinding>, CommandError> {
    let mut findings = Vec::new();
    collect_sensitive_findings(
        &mut findings,
        "conversation",
        sensitive::scan_json(handoff.get("conversation").unwrap_or(&Value::Null)),
    );
    collect_sensitive_findings(
        &mut findings,
        "session_state",
        sensitive::scan_json(handoff.get("session_state").unwrap_or(&Value::Null)),
    );

    for payload in &capture.payloads {
        let scope = match payload.kind.as_str() {
            "git_patch" => "git_patch",
            "untracked_file" => "untracked_file",
            _ => continue,
        };
        collect_sensitive_findings(
            &mut findings,
            scope,
            sensitive::scan_bytes(&payload.decode()?),
        );
    }
    Ok(findings)
}

fn collect_sensitive_findings(
    summaries: &mut Vec<SensitiveContentFinding>,
    scope: &'static str,
    findings: Vec<sensitive::SensitiveFinding>,
) {
    for finding in findings {
        if let Some(summary) = summaries
            .iter_mut()
            .find(|summary| summary.scope == scope && summary.code == finding.code)
        {
            summary.count += 1;
        } else {
            summaries.push(SensitiveContentFinding {
                code: finding.code,
                label: finding.label,
                scope,
                count: 1,
            });
        }
    }
}

fn append_sensitive_diagnostics(
    handoff: &mut Value,
    findings: &[SensitiveContentFinding],
) -> Result<(), CommandError> {
    let diagnostics = handoff
        .get_mut("diagnostics")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| CommandError::new("relaypack_invalid", "handoff diagnostics are missing"))?;
    for finding in findings {
        diagnostics.push(diagnostic(
            &format!("SENSITIVE_CONTENT_INCLUDED_{}", finding.code),
            "warning",
            "security",
            &format!(
                "{}; sender confirmed inclusion of {} selected item(s) in {}.",
                finding.label, finding.count, finding.scope
            ),
        ));
    }
    Ok(())
}

fn capture_git(request: &ExportRelaypackRequest) -> Result<GitCapture, CommandError> {
    let repository_path = request.repository_path.as_deref().ok_or_else(|| {
        CommandError::new(
            "repository_required",
            "repository_path is required when include_git is true",
        )
    })?;
    let requested_root = canonical_existing_directory(Path::new(repository_path), "repository")?;
    let safety_root = git_root_without_worktree_filters(&requested_root)?;
    if safety_root != requested_root {
        return Err(CommandError::new(
            "repository_root_required",
            format!(
                "repository_path must be the worktree root '{}', not a subdirectory",
                safety_root.display()
            ),
        ));
    }
    ensure_worktree_attributes_safe(&safety_root)?;
    ensure_receiver_attributes_safe(&safety_root)?;
    if let Some(head) = git_head_without_worktree_filters(&safety_root)? {
        ensure_commit_attributes_safe(&safety_root, &head)?;
    }
    let inspection = git::inspect_repository(repository_path)?;
    let root = PathBuf::from(&inspection.root);
    if root != requested_root {
        return Err(CommandError::new(
            "repository_root_required",
            format!(
                "repository_path must be the worktree root '{}', not a subdirectory",
                root.display()
            ),
        ));
    }
    let head = inspection.head.clone().ok_or_else(|| {
        CommandError::new(
            "git_capture_blocked",
            "Git capture requires a repository with an existing HEAD commit",
        )
    })?;
    if inspection.operation.any() {
        return Err(CommandError::new(
            "git_capture_blocked",
            "Git capture is blocked while merge, rebase, cherry-pick, revert, or bisect is in progress",
        ));
    }
    if inspection
        .submodules
        .iter()
        .any(|submodule| submodule.state != "clean")
        || inspection
            .staged
            .iter()
            .chain(inspection.unstaged.iter())
            .any(|change| change.submodule.is_some())
    {
        return Err(CommandError::new(
            "git_capture_blocked",
            "Git capture is blocked because a submodule is uninitialized, mismatched, conflicted, or dirty",
        ));
    }
    if request.selected_untracked.len() > MAX_UNTRACKED_FILES {
        return Err(CommandError::new(
            "too_many_untracked_files",
            format!("at most {MAX_UNTRACKED_FILES} untracked files may be selected"),
        ));
    }
    if !request.wants_staged() && !request.selected_staged.is_empty() {
        return Err(CommandError::new(
            "invalid_git_selection",
            "selected_staged must be empty when staged changes are excluded",
        ));
    }
    if !request.wants_unstaged() && !request.selected_unstaged.is_empty() {
        return Err(CommandError::new(
            "invalid_git_selection",
            "selected_unstaged must be empty when unstaged changes are excluded",
        ));
    }
    let selected_staged =
        validate_selected_changes("staged", &inspection.staged, &request.selected_staged)?;
    let selected_unstaged =
        validate_selected_changes("unstaged", &inspection.unstaged, &request.selected_unstaged)?;

    let mut capture = GitCapture {
        included: true,
        root: Some(root.clone()),
        branch: inspection.branch.clone(),
        head: Some(head.clone()),
        object_format: if head.len() == 64 { "sha256" } else { "sha1" }.into(),
        local_commits_status: if request.wants_local_commits() {
            "unknown".into()
        } else {
            "omitted".into()
        },
        local_commits_note: (!request.wants_local_commits())
            .then_some("Local commits were not selected.".into()),
        staged_status: if !request.wants_staged()
            || (!inspection.staged.is_empty() && request.selected_staged.is_empty())
        {
            "omitted".into()
        } else {
            "none".into()
        },
        unstaged_status: if !request.wants_unstaged()
            || (!inspection.unstaged.is_empty() && request.selected_unstaged.is_empty())
        {
            "omitted".into()
        } else {
            "none".into()
        },
        omitted_staged_files: if request.wants_staged() {
            inspection
                .staged
                .len()
                .saturating_sub(request.selected_staged.len())
        } else {
            inspection.staged.len()
        },
        omitted_unstaged_files: if request.wants_unstaged() {
            inspection
                .unstaged
                .len()
                .saturating_sub(request.selected_unstaged.len())
        } else {
            inspection.unstaged.len()
        },
        omitted_untracked_files: inspection
            .untracked
            .len()
            .saturating_sub(request.selected_untracked.len()),
        lfs_status: match inspection.lfs.status.as_str() {
            "not_present" => "not_present",
            "rules_only" => "resolved",
            _ => "unknown",
        }
        .into(),
        ..GitCapture::default()
    };

    if let Some(remote) = inspection.primary_remote.as_deref() {
        let canonical = canonical_remote(remote)?;
        capture.remote_fingerprint = Some(sha256_hex(canonical.as_bytes()));
        capture.canonical_remote = Some(canonical);
    } else {
        capture.diagnostics.push(diagnostic(
            "GIT_REMOTE_MISSING",
            "warning",
            "git",
            "No fetch remote was found. The package can only be restored into a repository that already has the required commits.",
        ));
    }

    if request.wants_local_commits() {
        capture_local_commits(&root, &head, &mut capture)?;
    }
    if request.wants_staged() {
        let patch = capture_selected_patch(&root, true, &selected_staged)?;
        if !patch.is_empty() {
            validate_patch_payload(&patch)?;
            let asset_id = "asset.git.staged".to_owned();
            capture.payloads.push(PackagePayload::from_bytes(
                &asset_id,
                "payload/git/staged.patch",
                "git_patch",
                Some("100444".into()),
                patch,
            ));
            capture.staged_asset_id = Some(asset_id);
            capture.staged_status = "included".into();
        }
    }
    if request.wants_unstaged() {
        let patch = capture_selected_patch(&root, false, &selected_unstaged)?;
        if !patch.is_empty() {
            validate_patch_payload(&patch)?;
            let asset_id = "asset.git.unstaged".to_owned();
            capture.payloads.push(PackagePayload::from_bytes(
                &asset_id,
                "payload/git/unstaged.patch",
                "git_patch",
                Some("100444".into()),
                patch,
            ));
            capture.unstaged_asset_id = Some(asset_id);
            capture.unstaged_status = "included".into();
        }
    }
    capture_untracked_files(
        &root,
        &inspection.untracked,
        &request.selected_untracked,
        &mut capture,
    )?;

    for warning in &inspection.warnings {
        if matches!(
            warning.code.as_str(),
            "git_operation_in_progress"
                | "submodule_state"
                | "submodule_worktree_changes"
                | "lfs_unavailable"
        ) {
            continue;
        }
        capture.diagnostics.push(diagnostic(
            &diagnostic_code(&warning.code),
            "warning",
            "git",
            &redact_absolute_paths(&warning.message, Some(&root), None),
        ));
    }
    Ok(capture)
}

fn capture_local_commits(
    root: &Path,
    head: &str,
    capture: &mut GitCapture,
) -> Result<(), CommandError> {
    let upstream_output = git_raw_strings(
        root,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
        true,
        1024 * 1024,
    )?;
    if !upstream_output.status.success() {
        capture.local_commits_status = "unknown".into();
        capture.local_commits_note = Some(
            "No reliable upstream was configured, so no local commit bundle was included.".into(),
        );
        capture.diagnostics.push(diagnostic(
            "GIT_UPSTREAM_UNKNOWN",
            "warning",
            "git",
            "No reliable upstream was configured. Local commits were omitted; restoration requires the receiver repository to already contain the package HEAD.",
        ));
        return Ok(());
    }
    let upstream = bytes_to_trimmed_string(&upstream_output.stdout);
    if upstream.is_empty() {
        return Err(CommandError::new(
            "git_protocol_error",
            "Git returned an empty upstream name",
        ));
    }
    capture.upstream = Some(upstream.clone());
    let ahead_text = git_text(
        root,
        &["rev-list", "--count", &format!("{upstream}..HEAD")],
        true,
    )?;
    let ahead: u64 = ahead_text.parse().map_err(|_| {
        CommandError::new(
            "git_protocol_error",
            format!("Git returned invalid ahead count '{ahead_text}'"),
        )
    })?;
    if ahead == 0 {
        capture.local_commits_status = "none".into();
        capture.local_commits_note = None;
        return Ok(());
    }
    let base = git_text(root, &["merge-base", "HEAD", &upstream], true)?;
    if !is_commit_id(&base) {
        return Err(CommandError::new(
            "git_protocol_error",
            "Git returned an invalid merge base",
        ));
    }

    let temp = TempBuilder::new()
        .prefix("relay-bundle-")
        .tempdir()
        .map_err(|error| {
            CommandError::new(
                "git_capture_failed",
                format!("cannot create bundle workspace: {error}"),
            )
        })?;
    let bundle_path = temp.path().join("local-commits.bundle");
    let exclusion = format!("^{base}");
    git_checked_os(
        root,
        &[
            OsString::from("bundle"),
            OsString::from("create"),
            bundle_path.as_os_str().to_owned(),
            OsString::from("HEAD"),
            OsString::from(exclusion),
        ],
        false,
        MAX_GIT_OUTPUT,
    )?;
    git_checked_os(
        root,
        &[
            OsString::from("bundle"),
            OsString::from("verify"),
            bundle_path.as_os_str().to_owned(),
        ],
        true,
        MAX_GIT_OUTPUT,
    )?;
    let bytes = read_file_limited(&bundle_path, MAX_PAYLOAD_BYTES)?;
    let asset_id = "asset.git.local_commits".to_owned();
    capture.payloads.push(PackagePayload::from_bytes(
        &asset_id,
        "payload/git/local-commits.bundle",
        "git_bundle",
        Some("100444".into()),
        bytes,
    ));
    capture.bundle_asset_id = Some(asset_id);
    capture.local_commits_status = "included".into();
    capture.local_commits_note = Some(format!(
        "Included {ahead} commit(s) reachable from HEAD and not from the configured upstream."
    ));
    capture.base = Some(base);
    capture.head = Some(head.into());
    Ok(())
}

fn capture_untracked_files(
    root: &Path,
    available: &[String],
    selected: &[String],
    capture: &mut GitCapture,
) -> Result<(), CommandError> {
    let available: HashSet<&str> = available.iter().map(String::as_str).collect();
    let mut exact_paths = HashSet::new();
    let mut collision_paths = HashSet::new();
    for path in selected {
        validate_relative_path(path, true)?;
        if !exact_paths.insert(path.clone()) || !collision_paths.insert(collision_key(path)) {
            return Err(CommandError::new(
                "unsafe_path",
                format!("duplicate or conflicting untracked path '{path}'"),
            ));
        }
        if !available.contains(path.as_str()) {
            return Err(CommandError::new(
                "untracked_file_not_found",
                format!("selected untracked file '{path}' is not an untracked ordinary path"),
            ));
        }
        let joined = root.join(path);
        let link_metadata = fs::symlink_metadata(&joined).map_err(|error| {
            CommandError::new(
                "untracked_file_invalid",
                format!("cannot inspect selected untracked file '{path}': {error}"),
            )
        })?;
        if link_metadata.file_type().is_symlink() || !link_metadata.is_file() {
            return Err(CommandError::new(
                "untracked_file_invalid",
                format!("selected untracked path '{path}' must be a non-symlink ordinary file"),
            ));
        }
        if link_metadata.len() > MAX_SINGLE_FILE_BYTES {
            return Err(CommandError::new(
                "untracked_file_too_large",
                format!("selected untracked file '{path}' exceeds 5 MiB"),
            ));
        }
        let canonical = fs::canonicalize(&joined).map_err(|error| {
            CommandError::new(
                "untracked_file_invalid",
                format!("cannot resolve selected untracked file '{path}': {error}"),
            )
        })?;
        if !canonical.starts_with(root) {
            return Err(CommandError::new(
                "untracked_file_invalid",
                format!("selected untracked file '{path}' escapes the repository"),
            ));
        }
        let bytes = read_file_limited(&canonical, MAX_SINGLE_FILE_BYTES as usize)?;
        let mode = regular_file_mode(&link_metadata);
        let asset_id = format!("asset.untracked.{}", &sha256_hex(path.as_bytes())[..20]);
        let archive_path = format!("payload/git/untracked/{path}");
        capture.payloads.push(PackagePayload::from_bytes(
            &asset_id,
            archive_path,
            "untracked_file",
            Some(mode.clone()),
            bytes,
        ));
        capture.untracked.push(CapturedUntracked {
            logical_path: format!("repo://{path}"),
            asset_id,
            mode,
        });
    }
    Ok(())
}

fn validate_selected_changes(
    category: &str,
    available: &[GitFileChange],
    selected: &[String],
) -> Result<Vec<String>, CommandError> {
    let mut by_path: HashMap<&str, &GitFileChange> = HashMap::new();
    for change in available {
        if by_path.insert(change.path.as_str(), change).is_some() {
            return Err(CommandError::new(
                "git_protocol_error",
                format!("Git reported duplicate {category} path '{}'", change.path),
            ));
        }
    }

    let mut selected_paths = HashSet::new();
    let mut collision_paths = HashSet::new();
    let mut pathspecs = Vec::new();
    for path in selected {
        validate_relative_path(path, true)?;
        if !selected_paths.insert(path.clone()) || !collision_paths.insert(collision_key(path)) {
            return Err(CommandError::new(
                "unsafe_path",
                format!("duplicate or conflicting selected {category} path '{path}'"),
            ));
        }
        let change = by_path.get(path.as_str()).ok_or_else(|| {
            CommandError::new(
                "git_change_not_found",
                format!("selected {category} path '{path}' is not present in Git status"),
            )
        })?;
        add_patch_pathspec(&mut pathspecs, path)?;
        if let Some(original_path) = change.original_path.as_deref() {
            add_patch_pathspec(&mut pathspecs, original_path)?;
        }
    }
    Ok(pathspecs)
}

fn add_patch_pathspec(pathspecs: &mut Vec<String>, path: &str) -> Result<(), CommandError> {
    validate_relative_path(path, true)?;
    let literal = format!(":(top,literal){path}");
    if !pathspecs.contains(&literal) {
        pathspecs.push(literal);
    }
    Ok(())
}

fn capture_selected_patch(
    root: &Path,
    cached: bool,
    pathspecs: &[String],
) -> Result<Vec<u8>, CommandError> {
    if pathspecs.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec![OsString::from("diff")];
    if cached {
        args.push(OsString::from("--cached"));
    }
    args.extend([
        OsString::from("--binary"),
        OsString::from("--full-index"),
        OsString::from("--no-ext-diff"),
        OsString::from("--no-textconv"),
        OsString::from("--src-prefix=a/"),
        OsString::from("--dst-prefix=b/"),
        OsString::from("--"),
    ]);
    args.extend(pathspecs.iter().map(OsString::from));
    let patch = git_checked_os(root, &args, true, MAX_GIT_OUTPUT)?.stdout;
    if !patch.is_empty() {
        validate_patch_payload(&patch)?;
    }
    Ok(patch)
}

#[cfg(unix)]
fn regular_file_mode(metadata: &fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 != 0 {
        "100755".into()
    } else {
        "100644".into()
    }
}

#[cfg(not(unix))]
fn regular_file_mode(_metadata: &fs::Metadata) -> String {
    "100644".into()
}

fn canonical_remote(remote: &str) -> Result<String, CommandError> {
    let remote = remote.trim();
    if remote.is_empty() {
        return Err(CommandError::new(
            "git_remote_invalid",
            "Git remote URL is empty",
        ));
    }
    if remote.contains("\0") || remote.contains('\n') || remote.contains('\r') {
        return Err(CommandError::new(
            "git_capture_blocked",
            "Git remote contains control characters",
        ));
    }
    match Url::parse(remote) {
        Ok(mut url) => {
            let http_user = matches!(url.scheme(), "http" | "https") && !url.username().is_empty();
            if http_user
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(CommandError::new(
                    "git_capture_blocked",
                    "Git remote contains user information, a password, a query, or a fragment; remove credentials before sharing code",
                ));
            }
            if !url.username().is_empty() && !matches!(url.scheme(), "ssh") {
                url.set_username("").map_err(|_| {
                    CommandError::new("git_remote_invalid", "cannot sanitize Git remote")
                })?;
            }
            Ok(url.to_string())
        }
        Err(_) if remote.contains("://") => Err(CommandError::new(
            "git_remote_invalid",
            "Git remote URL is malformed",
        )),
        Err(_) => Ok(remote.to_owned()),
    }
}

fn git_stdout(
    repository: &Path,
    args: &[&str],
    read_only: bool,
    max_stdout: usize,
) -> Result<Vec<u8>, CommandError> {
    let output = git_checked_strings(repository, args, read_only, max_stdout)?;
    Ok(output.stdout)
}

fn git_text(repository: &Path, args: &[&str], read_only: bool) -> Result<String, CommandError> {
    let output = git_checked_strings(repository, args, read_only, 4 * 1024 * 1024)?;
    let text = bytes_to_trimmed_string(&output.stdout);
    if text.is_empty() {
        return Err(CommandError::new(
            "git_protocol_error",
            format!("git {} returned empty output", args.join(" ")),
        ));
    }
    Ok(text)
}

fn git_checked_strings(
    repository: &Path,
    args: &[&str],
    read_only: bool,
    max_stdout: usize,
) -> Result<ProcessOutput, CommandError> {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    git_checked_os(repository, &args, read_only, max_stdout)
}

fn git_raw_strings(
    repository: &Path,
    args: &[&str],
    read_only: bool,
    max_stdout: usize,
) -> Result<ProcessOutput, CommandError> {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    git_raw_os(repository, &args, read_only, max_stdout)
}

fn git_checked_os(
    repository: &Path,
    args: &[OsString],
    read_only: bool,
    max_stdout: usize,
) -> Result<ProcessOutput, CommandError> {
    let output = git_raw_os(repository, args, read_only, max_stdout)?;
    if output.status.success() {
        if output.stdout_truncated {
            Err(CommandError::new(
                "git_output_too_large",
                "Git output exceeded the configured safety limit",
            ))
        } else {
            Ok(output)
        }
    } else {
        Err(git_output_error(args, &output))
    }
}

fn git_raw_os(
    repository: &Path,
    args: &[OsString],
    read_only: bool,
    max_stdout: usize,
) -> Result<ProcessOutput, CommandError> {
    let git = find_executable_on_path("git").ok_or_else(|| {
        CommandError::new("git_not_found", "git executable was not found on PATH")
    })?;
    let mut all_args = vec![OsString::from("-C"), repository.as_os_str().to_owned()];
    all_args.extend(safe_git_prefix());
    all_args.extend_from_slice(args);
    let read_only_environment = [
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GCM_INTERACTIVE", "Never"),
        ("GIT_ATTR_NOSYSTEM", "1"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", GIT_NULL_DEVICE),
        ("GIT_CONFIG_COUNT", "0"),
        ("GIT_PROTOCOL_FROM_USER", "0"),
    ];
    let write_environment = [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GCM_INTERACTIVE", "Never"),
        ("GIT_ATTR_NOSYSTEM", "1"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", GIT_NULL_DEVICE),
        ("GIT_CONFIG_COUNT", "0"),
        ("GIT_PROTOCOL_FROM_USER", "0"),
    ];
    run_process_with_removed_environment(
        &git,
        &all_args,
        None,
        GIT_TIMEOUT,
        max_stdout,
        MAX_GIT_STDERR,
        if read_only {
            &read_only_environment
        } else {
            &write_environment
        },
        GIT_CONTEXT_ENVIRONMENT,
    )
    .map_err(map_git_process_error)
}

fn map_git_process_error(error: ProcessRunError) -> CommandError {
    match error {
        ProcessRunError::Timeout { .. } => CommandError::new("git_timeout", error.to_string()),
        ProcessRunError::Spawn(_) => CommandError::new("git_start_error", error.to_string()),
        _ => CommandError::new("git_io_error", error.to_string()),
    }
}

fn git_output_error(args: &[OsString], output: &ProcessOutput) -> CommandError {
    let command = args
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let stderr = bytes_to_trimmed_string(&output.stderr);
    let code = output
        .status
        .code()
        .map_or_else(|| "signal".into(), |code| code.to_string());
    CommandError::new(
        "git_command_failed",
        if stderr.is_empty() {
            format!("git {command} exited with {code}")
        } else {
            format!("git {command} exited with {code}: {stderr}")
        },
    )
}

fn validate_patch_payload(bytes: &[u8]) -> Result<(), CommandError> {
    if bytes.contains(&0) {
        return Err(CommandError::new(
            "unsafe_patch",
            "Git patch contains a NUL byte",
        ));
    }
    let text = String::from_utf8_lossy(bytes);
    for line in text.lines() {
        if line.starts_with("diff --git ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("rename from ")
            || line.starts_with("rename to ")
        {
            let lower = line.to_ascii_lowercase();
            if lower.contains("../")
                || lower.contains("\\")
                || lower.contains(" a/.git")
                || lower.contains(" b/.git")
                || lower.contains("/.git/")
            {
                return Err(CommandError::new(
                    "unsafe_patch",
                    "Git patch contains an unsafe path",
                ));
            }
        }
    }
    Ok(())
}

fn is_commit_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn diagnostic_code(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_uppercase());
        } else if !output.ends_with('_') {
            output.push('_');
        }
    }
    let output = output.trim_matches('_');
    if output.is_empty() {
        "RELAY_WARNING".into()
    } else if output.as_bytes()[0].is_ascii_alphabetic() {
        output.into()
    } else {
        format!("RELAY_{output}")
    }
}

struct ConversationBuild {
    value: Value,
    exported_records: usize,
    omitted_records: usize,
    unsupported_blocks: usize,
    redacted_conversation_blocks: usize,
    redacted_tool_blocks: usize,
    redacted_project_instruction_blocks: usize,
    diagnostics: Vec<Value>,
}

struct ContentSelection {
    excluded_messages: HashSet<String>,
    excluded_blocks: HashMap<String, HashSet<usize>>,
}

impl ContentSelection {
    fn excludes(&self, message_id: &str, block_index: usize) -> bool {
        self.excluded_messages.contains(message_id)
            || self
                .excluded_blocks
                .get(message_id)
                .is_some_and(|indices| indices.contains(&block_index))
    }
}

fn validate_content_selection(
    messages: &[Value],
    request: &ExportRelaypackRequest,
) -> Result<ContentSelection, CommandError> {
    let mut message_blocks = HashMap::new();
    for message in messages {
        let message_id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("missing-message-id");
        let block_count = message
            .get("blocks")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if message_blocks.insert(message_id, block_count).is_some() {
            return Err(CommandError::new(
                "adapter_protocol_error",
                format!("adapter returned duplicate message id '{message_id}'"),
            ));
        }
    }

    let mut excluded_messages = HashSet::new();
    for message_id in &request.excluded_message_ids {
        if !message_blocks.contains_key(message_id.as_str()) {
            return Err(CommandError::new(
                "invalid_content_selection",
                "excluded_message_ids contains an unknown message id",
            ));
        }
        if !excluded_messages.insert(message_id.clone()) {
            return Err(CommandError::new(
                "invalid_content_selection",
                "excluded_message_ids contains a duplicate message id",
            ));
        }
    }

    let mut excluded_blocks: HashMap<String, HashSet<usize>> = HashMap::new();
    for excluded in &request.excluded_blocks {
        let Some(block_count) = message_blocks.get(excluded.message_id.as_str()) else {
            return Err(CommandError::new(
                "invalid_content_selection",
                "excluded_blocks contains an unknown message id",
            ));
        };
        if excluded.block_index >= *block_count {
            return Err(CommandError::new(
                "invalid_content_selection",
                "excluded_blocks contains an out-of-range block index",
            ));
        }
        if !excluded_blocks
            .entry(excluded.message_id.clone())
            .or_default()
            .insert(excluded.block_index)
        {
            return Err(CommandError::new(
                "invalid_content_selection",
                "excluded_blocks contains a duplicate block reference",
            ));
        }
    }

    let selection = ContentSelection {
        excluded_messages,
        excluded_blocks,
    };
    if request.include_tool_evidence {
        validate_tool_pair_selection(messages, &selection)?;
    }
    Ok(selection)
}

fn validate_tool_pair_selection(
    messages: &[Value],
    selection: &ContentSelection,
) -> Result<(), CommandError> {
    let mut pairs: HashMap<String, (Vec<bool>, Vec<bool>)> = HashMap::new();
    for message in messages {
        let message_id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("missing-message-id");
        for (block_index, block) in message
            .get("blocks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if !is_shareable_classification(block.get("classification").and_then(Value::as_str)) {
                continue;
            }
            let Some(call_id) = block.get("call_id").and_then(Value::as_str) else {
                continue;
            };
            let selected = !selection.excludes(message_id, block_index);
            let pair = pairs.entry(call_id.to_owned()).or_default();
            match block.get("kind").and_then(Value::as_str) {
                Some("tool_call") => pair.0.push(selected),
                Some("tool_result") => pair.1.push(selected),
                _ => {}
            }
        }
    }
    for (calls, results) in pairs.values() {
        if calls.len() == 1 && results.len() == 1 && calls[0] != results[0] {
            return Err(CommandError::new(
                "invalid_content_selection",
                "a tool call and its matching result must be selected or excluded together",
            ));
        }
    }
    Ok(())
}

fn build_handoff(
    adapter_preview: &Value,
    request: &ExportRelaypackRequest,
    capture: &GitCapture,
    package_id: &str,
    created_at: &str,
) -> Result<Value, CommandError> {
    if adapter_preview.get("schema").and_then(Value::as_str) != Some(ADAPTER_PREVIEW_SCHEMA) {
        return Err(CommandError::new(
            "adapter_protocol_error",
            "adapter export is not relay.adapter.handoff-preview.v1",
        ));
    }
    let source = adapter_preview
        .get("source")
        .and_then(Value::as_object)
        .ok_or_else(|| CommandError::new("adapter_protocol_error", "adapter source is missing"))?;
    let source_agent = source
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if source_agent != request.agent.as_str() {
        return Err(CommandError::new(
            "adapter_protocol_error",
            "adapter source agent does not match the export request",
        ));
    }
    let source_session_id = source
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            CommandError::new("adapter_protocol_error", "adapter session_id is missing")
        })?;
    if source_session_id != request.session_id.trim() {
        return Err(CommandError::new(
            "adapter_protocol_error",
            "adapter session_id does not match the export request",
        ));
    }
    if source.get("read_only").and_then(Value::as_bool) != Some(true) {
        return Err(CommandError::new(
            "adapter_unsafe",
            "adapter export must explicitly report read_only=true",
        ));
    }

    let session = adapter_preview
        .get("session")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let title = session
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("Untitled session")
        .to_owned();
    let adapter_cwd = adapter_preview
        .pointer("/environment/cwd")
        .and_then(Value::as_str);
    let source_path = source.get("source_path").and_then(Value::as_str);
    let sanitize = SanitizeContext {
        repository_root: capture.root.as_deref(),
        adapter_cwd,
        source_path,
    };

    let conversation = build_conversation(adapter_preview, &sanitize, request)?;
    let adapter_diagnostics = adapter_preview
        .pointer("/diagnostics/warnings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let completeness = adapter_preview
        .pointer("/diagnostics/completeness")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let reported_source_records = completeness.get("total_lines").and_then(Value::as_u64);
    let hidden_records = completeness
        .get("hidden_records")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let unknown_records = completeness
        .get("unknown_records")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let damaged_lines = completeness
        .get("damaged_lines")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut diagnostics = Vec::new();
    for warning in adapter_diagnostics {
        let code = warning
            .get("code")
            .and_then(Value::as_str)
            .map(diagnostic_code)
            .unwrap_or_else(|| "ADAPTER_WARNING".into());
        let message = warning
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Adapter reported an incomplete record");
        diagnostics.push(diagnostic(
            &code,
            "warning",
            "conversation",
            &redact_absolute_paths(message, capture.root.as_deref(), adapter_cwd),
        ));
    }
    diagnostics.extend(conversation.diagnostics.clone());
    diagnostics.extend(capture.diagnostics.clone());

    let session_state = build_session_state(request.session_state.as_ref(), &title, &sanitize)?;
    if request.session_state.is_none() {
        diagnostics.push(diagnostic(
            "SESSION_STATE_NOT_PROVIDED",
            "warning",
            "conversation",
            "No structured task state was supplied. Relay used only the session title as the objective and left the other task fields empty.",
        ));
    }

    let adapter_version = adapter_preview
        .pointer("/export/adapter_version")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let mut source_value = Map::new();
    source_value.insert("agent".into(), Value::String(source_agent.into()));
    source_value.insert("session_id".into(), Value::String(source_session_id.into()));
    source_value.insert("read_only".into(), Value::Bool(true));
    source_value.insert("title".into(), Value::String(title.clone()));
    for key in ["created_at", "updated_at", "native_version"] {
        if let Some(value) = session.get(key).and_then(Value::as_str) {
            source_value.insert(key.into(), Value::String(value.into()));
        }
    }
    source_value.insert(
        "adapter".into(),
        json!({
            "name": "relay-agent-adapter",
            "version": adapter_version,
            "mapping_status": if completeness.get("status").and_then(Value::as_str) == Some("complete") { "normalized" } else { "lossy" }
        }),
    );

    let project_name = adapter_preview
        .pointer("/project/name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("Unknown project");
    let project_seed = capture
        .remote_fingerprint
        .as_deref()
        .unwrap_or(source_session_id);
    let mut project = json!({
        "project_id": format!("project.{}", &sha256_hex(project_seed.as_bytes())[..24]),
        "display_name": project_name,
        "logical_root": "repo://",
        "workspace": {
            "kind": if capture.included { "primary" } else { "unknown" },
            "display_name": capture.branch.as_deref().unwrap_or("unknown"),
            "detached": capture.included && capture.branch.is_none()
        }
    });
    if let Some(branch) = capture.branch.as_deref() {
        project["workspace"]["branch"] = Value::String(branch.into());
    }
    if let Some(head) = capture.head.as_deref() {
        project["workspace"]["head"] = Value::String(head.into());
    }
    if let (Some(remote), Some(fingerprint)) = (
        capture.canonical_remote.as_deref(),
        capture.remote_fingerprint.as_deref(),
    ) {
        project["repository_identity"] = json!({
            "canonical_remote": remote,
            "remote_fingerprint": fingerprint
        });
    }

    let mut assets = Vec::new();
    for payload in &capture.payloads {
        let logical_path = capture
            .untracked
            .iter()
            .find(|file| file.asset_id == payload.asset_id)
            .map(|file| file.logical_path.as_str());
        assets.push(asset_manifest(payload, &payload.kind, logical_path, None));
    }

    let git_value = build_git_value(capture);
    let minimum_omitted_records = hidden_records
        .saturating_add(unknown_records)
        .saturating_add(damaged_lines)
        .saturating_add(conversation.omitted_records as u64);
    let minimum_source_records =
        (conversation.exported_records as u64).saturating_add(minimum_omitted_records);
    let source_records = reported_source_records
        .unwrap_or(minimum_source_records)
        .max(minimum_source_records);
    let omitted_records = source_records - conversation.exported_records as u64;
    let omitted_git_items = if capture.included {
        let local_commit_omissions = usize::from(!request.wants_local_commits());
        let staged_omissions = if request.wants_staged() {
            capture.omitted_staged_files
        } else {
            capture.omitted_staged_files.max(1)
        };
        let unstaged_omissions = if request.wants_unstaged() {
            capture.omitted_unstaged_files
        } else {
            capture.omitted_unstaged_files.max(1)
        };
        local_commit_omissions
            .saturating_add(staged_omissions)
            .saturating_add(unstaged_omissions)
            .saturating_add(capture.omitted_untracked_files)
    } else {
        0
    };
    let partial = completeness.get("status").and_then(Value::as_str) != Some("complete")
        || omitted_records > 0
        || conversation.omitted_records > 0
        || conversation.unsupported_blocks > 0
        || conversation.redacted_conversation_blocks > 0
        || conversation.redacted_tool_blocks > 0
        || conversation.redacted_project_instruction_blocks > 0
        || !request.include_environment
        || !capture.included
        || omitted_git_items > 0;
    let mut omissions = Vec::new();
    if hidden_records > 0 {
        omissions.push(json!({
            "reason": "provider_internal",
            "count": hidden_records,
            "note": "Provider-internal records and private model data are never exported."
        }));
    }
    if unknown_records + damaged_lines + conversation.unsupported_blocks as u64 > 0 {
        omissions.push(json!({
            "reason": "unsupported_by_adapter",
            "count": unknown_records + damaged_lines + conversation.unsupported_blocks as u64,
            "note": "Unknown, damaged, or unsafe records were omitted with diagnostics."
        }));
    }
    if !capture.included {
        omissions.push(json!({
            "reason": "git_excluded",
            "count": 1,
            "note": "Git content was not included."
        }));
    } else {
        if omitted_git_items > 0 {
            omissions.push(json!({
                "reason": "git_excluded",
                "count": omitted_git_items,
                "note": "One or more Git capture categories or changed files were excluded by the sender."
            }));
        }
    }
    for (count, note) in [
        (
            conversation.redacted_conversation_blocks,
            "Conversation blocks were excluded by the sender.",
        ),
        (
            conversation.redacted_tool_blocks,
            "Historical tool evidence was excluded by the sender.",
        ),
        (
            conversation.redacted_project_instruction_blocks,
            "Project instruction blocks were excluded by the sender.",
        ),
    ] {
        if count > 0 {
            omissions.push(json!({
                "reason": "redacted_by_user",
                "count": count,
                "note": note
            }));
        }
    }
    if !request.include_environment {
        omissions.push(json!({
            "reason": "redacted_by_user",
            "count": 1,
            "note": "Environment details were excluded by the sender. Required placeholder values remain so the handoff format stays valid."
        }));
        diagnostics.push(diagnostic(
            "ENVIRONMENT_REDACTED_BY_USER",
            "info",
            "other",
            "Environment details were excluded by the sender. Required placeholder values remain in the handoff.",
        ));
    }

    let environment = if request.include_environment {
        json!({
            "os": match std::env::consts::OS { "macos" => "macos", "linux" => "linux", "windows" => "windows", _ => "unknown" },
            "arch": match std::env::consts::ARCH { "aarch64" => "arm64", "x86_64" => "x86_64", _ => "unknown" },
            "tools": [
                {"name": "git"},
                {"name": source_agent}
            ],
            "notes": ["Environment variables and local absolute paths are not included."]
        })
    } else {
        json!({
            "os": "unknown",
            "arch": "unknown",
            "tools": [],
            "notes": ["Environment details were excluded by the sender."]
        })
    };

    let selected_export = !request.include_conversation
        || !request.include_tool_evidence
        || !request.include_project_instructions
        || !request.include_environment
        || !request.include_git
        || !request.wants_local_commits()
        || !request.wants_staged()
        || !request.wants_unstaged()
        || !request.excluded_message_ids.is_empty()
        || !request.excluded_blocks.is_empty()
        || capture.omitted_staged_files > 0
        || capture.omitted_unstaged_files > 0
        || capture.omitted_untracked_files > 0;

    Ok(json!({
        "schema": HANDOFF_SCHEMA,
        "package_id": package_id,
        "created_at": created_at,
        "export": {
            "mode": if selected_export { "selected" } else { "full" },
            "native_history_included": false,
            "completeness": {
                "status": if partial { "partial" } else { "complete" },
                "source_records": source_records,
                "exported_records": conversation.exported_records,
                "omitted_records": omitted_records,
                "unknown_records": unknown_records,
                "notes": []
            },
            "omissions": omissions
        },
        "source": Value::Object(source_value),
        "session_state": session_state,
        "environment": environment,
        "project": project,
        "conversation": conversation.value,
        "assets": assets,
        "git": git_value,
        "diagnostics": diagnostics
    }))
}

fn build_session_state(
    input: Option<&SessionStateInput>,
    title: &str,
    sanitize: &SanitizeContext<'_>,
) -> Result<Value, CommandError> {
    let empty = SessionStateInput::default();
    let input = input.unwrap_or(&empty);
    let objective = input.objective.as_deref().unwrap_or(title);
    let mut important_files = Vec::new();
    let mut collisions = HashSet::new();
    for path in &input.important_files {
        let relative = path.strip_prefix("repo://").unwrap_or(path);
        validate_relative_path(relative, true)?;
        let uri = format!("repo://{relative}");
        if !collisions.insert(collision_key(&uri)) {
            return Err(CommandError::new(
                "unsafe_path",
                format!("duplicate or conflicting important file '{path}'"),
            ));
        }
        important_files.push(uri);
    }
    let next_steps: Vec<Value> = input
        .next_steps
        .iter()
        .map(|step| {
            json!({
                "text": sanitize_text(&step.text, sanitize),
                "status": normalize_next_step_status(step.status.as_deref())
            })
        })
        .collect();
    let tests: Vec<Value> = input
        .tests
        .iter()
        .map(|test| {
            let mut value = json!({
                "name": sanitize_text(&test.name, sanitize),
                "status": normalize_test_status(test.status.as_deref())
            });
            if let Some(command) = test.command.as_deref() {
                value["command"] = Value::String(sanitize_text(command, sanitize));
            }
            if let Some(note) = test.note.as_deref() {
                value["note"] = Value::String(sanitize_text(note, sanitize));
            }
            value
        })
        .collect();
    Ok(json!({
        "objective": sanitize_text(objective, sanitize),
        "summary": sanitize_text(input.summary.as_deref().unwrap_or(""), sanitize),
        "current_status": sanitize_text(input.current_status.as_deref().unwrap_or(""), sanitize),
        "next_steps": next_steps,
        "tests": tests,
        "important_files": important_files,
        "constraints": input.constraints.iter().map(|value| sanitize_text(value, sanitize)).collect::<Vec<_>>(),
        "open_questions": input.open_questions.iter().map(|value| sanitize_text(value, sanitize)).collect::<Vec<_>>()
    }))
}

fn normalize_next_step_status(status: Option<&str>) -> &'static str {
    match status {
        Some("pending") => "pending",
        Some("in_progress") => "in_progress",
        Some("blocked") => "blocked",
        Some("done") => "done",
        _ => "unknown",
    }
}

fn normalize_test_status(status: Option<&str>) -> &'static str {
    match status {
        Some("passed") => "passed",
        Some("failed") => "failed",
        Some("not_run") => "not_run",
        _ => "unknown",
    }
}

fn build_git_value(capture: &GitCapture) -> Value {
    let lfs_status = if capture.lfs_status.is_empty() {
        "not_present"
    } else {
        capture.lfs_status.as_str()
    };
    let local_commits = if let Some(asset_id) = capture.bundle_asset_id.as_deref() {
        let mut value = json!({
            "status": "included",
            "asset_id": asset_id,
            "base": capture.base.as_deref().unwrap_or_default(),
            "tips": capture.head.iter().collect::<Vec<_>>()
        });
        if let Some(note) = capture.local_commits_note.as_deref() {
            value["note"] = Value::String(note.into());
        }
        value
    } else {
        let mut value = json!({"status": capture.local_commits_status});
        if let Some(note) = capture.local_commits_note.as_deref() {
            value["note"] = Value::String(note.into());
        }
        value
    };
    let staged = capture.staged_asset_id.as_ref().map_or_else(
        || json!({"status": capture.staged_status}),
        |asset_id| json!({"status": "included", "asset_id": asset_id}),
    );
    let unstaged = capture.unstaged_asset_id.as_ref().map_or_else(
        || json!({"status": capture.unstaged_status}),
        |asset_id| json!({"status": "included", "asset_id": asset_id}),
    );
    let untracked: Vec<Value> = capture
        .untracked
        .iter()
        .map(|file| {
            json!({
                "logical_path": file.logical_path,
                "asset_id": file.asset_id,
                "mode": file.mode
            })
        })
        .collect();
    if !capture.included {
        return json!({
            "included": false,
            "capture": {
                "local_commits": local_commits,
                "staged_patch": staged,
                "unstaged_patch": unstaged,
                "untracked_files": untracked,
                "ignored_files": {"status": "excluded"},
                "submodules": {"status": "not_present"},
                "lfs": {"status": "not_present", "objects_included": false}
            },
            "completeness": {
                "status": "complete",
                "notes": ["Git content was intentionally omitted."]
            }
        });
    }

    let mut repository = json!({"object_format": capture.object_format});
    if let Some(head) = capture.head.as_deref() {
        repository["head"] = Value::String(head.into());
    }
    if let Some(branch) = capture.branch.as_deref() {
        repository["branch"] = Value::String(branch.into());
    }
    if let Some(base) = capture.base.as_deref() {
        repository["base"] = Value::String(base.into());
    }
    if let Some(upstream) = capture.upstream.as_deref() {
        repository["upstream"] = Value::String(upstream.into());
    }
    if let Some(remote) = capture.canonical_remote.as_deref() {
        repository["canonical_remote"] = Value::String(remote.into());
    }
    if let Some(fingerprint) = capture.remote_fingerprint.as_deref() {
        repository["remote_fingerprint"] = Value::String(fingerprint.into());
    }
    json!({
        "included": true,
        "repository": repository,
        "capture": {
            "local_commits": local_commits,
            "staged_patch": staged,
            "unstaged_patch": unstaged,
            "untracked_files": untracked,
            "ignored_files": {"status": "excluded"},
            "submodules": {"status": "not_present"},
            "lfs": {"status": lfs_status, "objects_included": false}
        },
        "completeness": {
            "status": if capture.local_commits_status == "unknown" { "partial" } else { "complete" },
            "notes": capture.local_commits_note.iter().collect::<Vec<_>>()
        }
    })
}

fn asset_manifest(
    payload: &PackagePayload,
    kind: &str,
    logical_path: Option<&str>,
    filename: Option<&str>,
) -> Value {
    let mut value = json!({
        "id": payload.asset_id,
        "kind": kind,
        "classification": if kind == "handoff_document" { "user_visible" } else { "project_owned" },
        "status": "included",
        "archive_path": payload.archive_path,
        "byte_length": payload.byte_length,
        "sha256": payload.sha256,
        "execution_policy": "never"
    });
    if let Some(logical_path) = logical_path {
        value["logical_path"] = Value::String(logical_path.into());
    }
    if let Some(filename) = filename {
        value["filename"] = Value::String(filename.into());
    }
    value
}

fn append_asset_manifest(
    handoff: &mut Value,
    payload: &PackagePayload,
    kind: &str,
    logical_path: Option<&str>,
    filename: Option<&str>,
) -> Result<(), CommandError> {
    let assets = handoff
        .get_mut("assets")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| CommandError::new("relaypack_invalid", "handoff assets are missing"))?;
    assets.push(asset_manifest(payload, kind, logical_path, filename));
    Ok(())
}

fn render_handoff_markdown(handoff: &Value) -> String {
    let title = markdown_inline(
        handoff
            .pointer("/source/title")
            .and_then(Value::as_str)
            .unwrap_or("Untitled session"),
    );
    let project = markdown_inline(
        handoff
            .pointer("/project/display_name")
            .and_then(Value::as_str)
            .unwrap_or("Unknown project"),
    );
    let objective = handoff
        .pointer("/session_state/objective")
        .and_then(Value::as_str)
        .unwrap_or("");
    let summary = handoff
        .pointer("/session_state/summary")
        .and_then(Value::as_str)
        .unwrap_or("");
    let current = handoff
        .pointer("/session_state/current_status")
        .and_then(Value::as_str)
        .unwrap_or("");
    let package_id = markdown_inline(
        handoff
            .get("package_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
    );
    let mut markdown = format!(
        "# Relay Handoff\n\n- Project: {project}\n- Session: {title}\n- Package: {}\n\n## Objective\n\n{}\n\n## Summary\n\n{}\n\n## Current status\n\n{}\n",
        package_id,
        markdown_inline(nonempty_or(objective, "No objective was supplied.")),
        markdown_inline(nonempty_or(summary, "No summary was supplied.")),
        markdown_inline(nonempty_or(current, "No current status was supplied."))
    );
    markdown.push_str("\n## Next steps\n\n");
    let next_steps = handoff
        .pointer("/session_state/next_steps")
        .and_then(Value::as_array);
    if next_steps.map_or(true, Vec::is_empty) {
        markdown.push_str("No next steps were supplied.\n");
    } else if let Some(next_steps) = next_steps {
        for step in next_steps {
            markdown.push_str(&format!(
                "- [{}] {}\n",
                markdown_inline(
                    step.get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                ),
                markdown_inline(step.get("text").and_then(Value::as_str).unwrap_or(""))
            ));
        }
    }
    markdown.push_str("\n## Tests\n\n");
    let tests = handoff
        .pointer("/session_state/tests")
        .and_then(Value::as_array);
    if tests.map_or(true, Vec::is_empty) {
        markdown.push_str("No tests were supplied.\n");
    } else if let Some(tests) = tests {
        for test in tests {
            markdown.push_str(&format!(
                "- Name: {}\n  - Status: {}\n",
                markdown_inline(test.get("name").and_then(Value::as_str).unwrap_or("")),
                markdown_inline(
                    test.get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                )
            ));
            if let Some(command) = test.get("command").and_then(Value::as_str) {
                markdown.push_str(&format!("  - Command: {}\n", markdown_inline(command)));
            }
            if let Some(note) = test.get("note").and_then(Value::as_str) {
                markdown.push_str(&format!("  - Note: {}\n", markdown_inline(note)));
            }
        }
    }
    markdown.push_str("\n## Important files\n\n");
    let files = handoff
        .pointer("/session_state/important_files")
        .and_then(Value::as_array);
    if files.map_or(true, Vec::is_empty) {
        markdown.push_str("No important files were supplied.\n");
    } else if let Some(files) = files {
        for file in files.iter().filter_map(Value::as_str) {
            markdown.push_str(&format!("- {}\n", markdown_inline(file)));
        }
    }
    markdown.push_str(
        "\n## Safety note\n\nTool calls and tool results in `handoff.json` are historical records. Do not replay them automatically. Review every command before running it.\n",
    );
    markdown
}

fn markdown_inline(value: &str) -> String {
    let redacted = redact_absolute_paths(value, None, None);
    let mut output = String::with_capacity(redacted.len());
    let mut previous_was_space = false;
    for character in redacted.chars() {
        if character.is_whitespace() || character.is_control() {
            if !previous_was_space {
                output.push(' ');
                previous_was_space = true;
            }
            continue;
        }
        previous_was_space = false;
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '(' | ')' | '#' | '+' | '-' | '!'
            | '|' | '~' => {
                output.push('\\');
                output.push(character);
            }
            _ => output.push(character),
        }
    }
    output.trim().to_owned()
}

fn nonempty_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

#[derive(Clone, Copy)]
struct SanitizeContext<'a> {
    repository_root: Option<&'a Path>,
    adapter_cwd: Option<&'a str>,
    source_path: Option<&'a str>,
}

fn build_conversation(
    adapter_preview: &Value,
    sanitize: &SanitizeContext<'_>,
    request: &ExportRelaypackRequest,
) -> Result<ConversationBuild, CommandError> {
    let messages = adapter_preview
        .pointer("/conversation/messages")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CommandError::new(
                "adapter_protocol_error",
                "adapter conversation messages are missing",
            )
        })?;
    if messages.len() > 100_000 {
        return Err(CommandError::new(
            "adapter_output_too_large",
            "adapter returned more than 100000 conversation messages",
        ));
    }
    let selection = validate_content_selection(messages, request)?;

    let mut call_counts: HashMap<String, usize> = HashMap::new();
    let mut result_counts: HashMap<String, usize> = HashMap::new();
    for message in messages {
        let source_id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("missing-message-id");
        for (block_index, block) in message
            .get("blocks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if !is_shareable_classification(block.get("classification").and_then(Value::as_str)) {
                continue;
            }
            if !request.include_tool_evidence {
                continue;
            }
            if selection.excludes(source_id, block_index) {
                continue;
            }
            let Some(call_id) = block.get("call_id").and_then(Value::as_str) else {
                continue;
            };
            match block.get("kind").and_then(Value::as_str) {
                Some("tool_call") => *call_counts.entry(call_id.into()).or_default() += 1,
                Some("tool_result") => *result_counts.entry(call_id.into()).or_default() += 1,
                _ => {}
            }
        }
    }
    let paired_calls: HashSet<String> = call_counts
        .iter()
        .filter_map(|(call_id, count)| {
            (*count == 1 && result_counts.get(call_id) == Some(&1)).then_some(call_id.clone())
        })
        .collect();

    let mut source_ids = HashSet::new();
    let mut id_map = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        let source_id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("missing-message-id");
        if !source_ids.insert(source_id.to_owned()) {
            return Err(CommandError::new(
                "adapter_protocol_error",
                format!("adapter returned duplicate message id '{source_id}'"),
            ));
        }
        id_map.insert(source_id.to_owned(), stable_id("record", source_id, index));
    }

    let mut records = Vec::new();
    let mut record_source_ids = Vec::new();
    let mut parent_source_ids = Vec::new();
    let mut diagnostics = Vec::new();
    let mut omitted_records = 0_usize;
    let mut unsupported_blocks = 0_usize;
    let mut redacted_conversation_blocks = 0_usize;
    let mut redacted_tool_blocks = 0_usize;
    let mut redacted_project_instruction_blocks = 0_usize;
    let mut seen_calls = HashSet::new();
    let mut seen_results = HashSet::new();
    let mut active_branch = None;

    for (message_index, message) in messages.iter().enumerate() {
        let source_id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("missing-message-id");
        let source_blocks = message
            .get("blocks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut blocks = Vec::new();
        let mut record_classification = "project_owned";
        for (block_index, block) in source_blocks.iter().enumerate() {
            let classification = block
                .get("classification")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if !is_shareable_classification(Some(classification)) {
                unsupported_blocks += 1;
                continue;
            }
            let kind = block
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if selection.excludes(source_id, block_index) {
                match kind {
                    "tool_call" | "tool_result" => redacted_tool_blocks += 1,
                    "source_context"
                        if block
                            .pointer("/source/filename")
                            .and_then(Value::as_str)
                            .is_some_and(is_project_instruction_file) =>
                    {
                        redacted_project_instruction_blocks += 1;
                    }
                    _ => redacted_conversation_blocks += 1,
                }
                continue;
            }
            match kind {
                "tool_call" | "tool_result" => {
                    if !request.include_tool_evidence {
                        redacted_tool_blocks += 1;
                        continue;
                    }
                }
                "source_context"
                    if block
                        .pointer("/source/filename")
                        .and_then(Value::as_str)
                        .is_some_and(is_project_instruction_file) =>
                {
                    if !request.include_project_instructions {
                        redacted_project_instruction_blocks += 1;
                        continue;
                    }
                }
                _ if !request.include_conversation => {
                    redacted_conversation_blocks += 1;
                    continue;
                }
                _ => {}
            }
            match convert_adapter_block(
                block,
                source_id,
                message_index,
                block_index,
                &paired_calls,
                &mut seen_calls,
                &mut seen_results,
                sanitize,
            )? {
                Some(converted) => {
                    if classification == "user_visible" {
                        record_classification = "user_visible";
                    }
                    blocks.push(converted);
                }
                None => unsupported_blocks += 1,
            }
        }
        if blocks.is_empty() {
            omitted_records += 1;
            continue;
        }

        let record_id = id_map
            .get(source_id)
            .cloned()
            .expect("message id map was built above");
        let role = normalize_role(message.get("role").and_then(Value::as_str));
        let mut record = json!({
            "id": record_id,
            "kind": "message",
            "classification": record_classification,
            "mapping": {
                "status": "normalized",
                "source_type": "adapter_message",
                "source_id": truncate_string(source_id, 512)
            },
            "completeness": {
                "status": "complete"
            },
            "role": role,
            "blocks": blocks
        });
        if let Some(timestamp) = message.get("timestamp").and_then(Value::as_str) {
            if chrono::DateTime::parse_from_rfc3339(timestamp).is_ok() {
                record["timestamp"] = Value::String(timestamp.into());
            }
        }
        if let Some(turn_id) = message.get("turn_id").and_then(Value::as_str) {
            record["turn_id"] = Value::String(stable_id("turn", turn_id, 0));
        }
        if let Some(branch_id) = message.get("branch_id").and_then(Value::as_str) {
            let branch_id = stable_id("branch", branch_id, 0);
            active_branch = Some(branch_id.clone());
            record["branch_id"] = Value::String(branch_id);
        }
        records.push(record);
        record_source_ids.push(source_id.to_owned());
        parent_source_ids.push(
            message
                .get("parent_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        );
    }

    let included_ids: HashSet<&str> = record_source_ids.iter().map(String::as_str).collect();
    let mut roots = Vec::new();
    for (index, record) in records.iter_mut().enumerate() {
        match parent_source_ids[index].as_deref() {
            Some(parent) if included_ids.contains(parent) => {
                if let Some(parent_id) = id_map.get(parent) {
                    record["parent_id"] = Value::String(parent_id.clone());
                }
            }
            _ => {
                roots.push(
                    record
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
        }
    }
    let unpaired_count = call_counts
        .keys()
        .chain(result_counts.keys())
        .collect::<HashSet<_>>()
        .len()
        .saturating_sub(paired_calls.len());
    if unpaired_count > 0 {
        diagnostics.push(diagnostic(
            "UNPAIRED_TOOL_HISTORY_OMITTED",
            "warning",
            "conversation",
            &format!(
                "{unpaired_count} unpaired or duplicate tool call/result id(s) were omitted from the formal handoff."
            ),
        ));
    }
    if redacted_conversation_blocks > 0 {
        diagnostics.push(diagnostic(
            "CONVERSATION_REDACTED_BY_USER",
            "info",
            "conversation",
            &format!(
                "{redacted_conversation_blocks} conversation block(s) were excluded by the sender."
            ),
        ));
    }
    if redacted_tool_blocks > 0 {
        diagnostics.push(diagnostic(
            "TOOL_EVIDENCE_REDACTED_BY_USER",
            "info",
            "conversation",
            &format!(
                "{redacted_tool_blocks} historical tool call/result block(s) were excluded by the sender."
            ),
        ));
    }
    if redacted_project_instruction_blocks > 0 {
        diagnostics.push(diagnostic(
            "PROJECT_INSTRUCTIONS_REDACTED_BY_USER",
            "info",
            "conversation",
            &format!(
                "{redacted_project_instruction_blocks} project instruction block(s) were excluded by the sender."
            ),
        ));
    }

    let mut conversation = json!({
        "root_record_ids": roots,
        "completeness": {
            "status": if omitted_records > 0 || unsupported_blocks > 0 || unpaired_count > 0 || redacted_conversation_blocks > 0 || redacted_tool_blocks > 0 || redacted_project_instruction_blocks > 0 { "partial" } else { "complete" },
            "notes": if unsupported_blocks > 0 || unpaired_count > 0 || redacted_conversation_blocks > 0 || redacted_tool_blocks > 0 || redacted_project_instruction_blocks > 0 { vec!["Some adapter blocks were omitted because they were unsafe, incomplete, or deselected by the sender."] } else { Vec::<&str>::new() }
        },
        "records": records
    });
    if let Some(active_branch) = active_branch {
        conversation["active_branch_id"] = Value::String(active_branch);
    }
    Ok(ConversationBuild {
        exported_records: conversation["records"].as_array().map_or(0, Vec::len),
        value: conversation,
        omitted_records,
        unsupported_blocks: unsupported_blocks + unpaired_count,
        redacted_conversation_blocks,
        redacted_tool_blocks,
        redacted_project_instruction_blocks,
        diagnostics,
    })
}

#[allow(clippy::too_many_arguments)]
fn convert_adapter_block(
    block: &Value,
    message_source_id: &str,
    message_index: usize,
    block_index: usize,
    paired_calls: &HashSet<String>,
    seen_calls: &mut HashSet<String>,
    seen_results: &mut HashSet<String>,
    sanitize: &SanitizeContext<'_>,
) -> Result<Option<Value>, CommandError> {
    let classification = block
        .get("classification")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if !is_shareable_classification(Some(classification)) {
        return Ok(None);
    }
    let kind = block
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let block_id = stable_id(
        "block",
        &format!("{message_source_id}:{kind}:{block_index}"),
        message_index,
    );
    let mapping = json!({
        "status": if kind == "text" { "exact" } else { "normalized" },
        "source_type": block.get("native_type").and_then(Value::as_str).unwrap_or(kind)
    });
    match kind {
        "text" => Ok(Some(json!({
            "id": block_id,
            "kind": "text",
            "classification": classification,
            "mapping": mapping,
            "text": sanitize_text(block.get("text").and_then(Value::as_str).unwrap_or(""), sanitize),
            "format": "plain"
        }))),
        "tool_call" => {
            let call_id = match block.get("call_id").and_then(Value::as_str) {
                Some(call_id)
                    if paired_calls.contains(call_id) && seen_calls.insert(call_id.into()) =>
                {
                    call_id
                }
                _ => return Ok(None),
            };
            if block.get("replay_policy").and_then(Value::as_str) != Some("never") {
                return Err(CommandError::new(
                    "adapter_unsafe",
                    "adapter tool_call replay_policy must be never",
                ));
            }
            let arguments =
                sanitize_json_value(block.get("input").unwrap_or(&Value::Null), sanitize, 0);
            Ok(Some(json!({
                "id": block_id,
                "kind": "tool_call",
                "classification": classification,
                "mapping": mapping,
                "call_id": stable_id("call", call_id, 0),
                "tool_name": block.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()).unwrap_or("unknown_tool"),
                "arguments": arguments,
                "status": normalize_tool_call_status(block.get("status").and_then(Value::as_str)),
                "semantics": "historical_record",
                "replay_policy": "never"
            })))
        }
        "tool_result" => {
            let call_id = match block.get("call_id").and_then(Value::as_str) {
                Some(call_id)
                    if paired_calls.contains(call_id) && seen_results.insert(call_id.into()) =>
                {
                    call_id
                }
                _ => return Ok(None),
            };
            if block.get("replay_policy").and_then(Value::as_str) != Some("never") {
                return Err(CommandError::new(
                    "adapter_unsafe",
                    "adapter tool_result replay_policy must be never",
                ));
            }
            let output =
                sanitize_json_value(block.get("output").unwrap_or(&Value::Null), sanitize, 0);
            let output_text = match output {
                Value::String(text) => text,
                other => serde_json::to_string_pretty(&other).unwrap_or_else(|_| "null".into()),
            };
            let child_id = stable_id(
                "block",
                &format!("{message_source_id}:tool-result-content:{block_index}"),
                message_index,
            );
            let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true)
                || matches!(
                    block.get("status").and_then(Value::as_str),
                    Some("error" | "failed")
                );
            Ok(Some(json!({
                "id": block_id,
                "kind": "tool_result",
                "classification": classification,
                "mapping": mapping,
                "call_id": stable_id("call", call_id, 0),
                "status": if is_error { "error" } else { "success" },
                "content": [{
                    "id": child_id,
                    "kind": "text",
                    "classification": classification,
                    "mapping": {"status": "normalized"},
                    "text": output_text,
                    "format": "plain"
                }],
                "semantics": "historical_record",
                "replay_policy": "never"
            })))
        }
        "source_context" => {
            let source = block.get("source").and_then(Value::as_object);
            let filename = source
                .and_then(|source| source.get("filename"))
                .and_then(Value::as_str);
            let text = source
                .and_then(|source| source.get("snippet").or_else(|| source.get("text")))
                .and_then(Value::as_str)
                .map(|text| sanitize_text(text, sanitize));
            let mut converted = json!({
                "id": block_id,
                "kind": "source_context",
                "classification": classification,
                "mapping": mapping,
                "source_kind": if filename.is_some_and(is_project_instruction_file) { "project_instruction" } else { "other" }
            });
            if let Some(text) = text {
                converted["text"] = Value::String(text);
            }
            if let Some(filename) = filename {
                if validate_relative_path(filename, true).is_ok() {
                    converted["logical_path"] = Value::String(format!("repo://{filename}"));
                }
            }
            Ok(Some(converted))
        }
        "unsupported" => Ok(Some(json!({
            "id": block_id,
            "kind": "unsupported",
            "classification": classification,
            "mapping": {"status": "unmapped", "source_type": block.get("native_type").and_then(Value::as_str).unwrap_or("unknown")},
            "original_type": block.get("native_type").and_then(Value::as_str).unwrap_or("unknown_adapter_block"),
            "safe_summary": "An unsupported historical block was present. Its raw payload was not exported.",
            "preservation": "historical_record_only"
        }))),
        // Adapter assets are not copied from arbitrary source paths in this
        // version. Missing attachments remain visible through diagnostics.
        "asset_ref" => Ok(None),
        _ => Ok(None),
    }
}

fn is_shareable_classification(value: Option<&str>) -> bool {
    matches!(value, Some("user_visible" | "project_owned"))
}

fn normalize_role(role: Option<&str>) -> &'static str {
    match role {
        Some("user") => "user",
        Some("assistant") => "assistant",
        Some("developer") => "developer",
        Some("system") => "system",
        Some("tool") => "tool",
        _ => "unknown",
    }
}

fn normalize_tool_call_status(status: Option<&str>) -> &'static str {
    match status {
        Some("completed") => "completed",
        Some("failed" | "error") => "failed",
        Some("cancelled") => "cancelled",
        Some("observed" | "started" | "in_progress") => "observed",
        _ => "unknown",
    }
}

fn stable_id(prefix: &str, source: &str, index: usize) -> String {
    let digest = sha256_hex(format!("{prefix}\0{source}\0{index}").as_bytes());
    format!("{prefix}.{}", &digest[..24])
}

fn truncate_string(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn is_project_instruction_file(filename: &str) -> bool {
    Path::new(filename)
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "agents.md" | "claude.md" | "codex.md"
            )
        })
}

fn sanitize_json_value(value: &Value, context: &SanitizeContext<'_>, depth: usize) -> Value {
    if depth > 64 {
        return Value::String("[nested value omitted]".into());
    }
    match value {
        Value::String(text) => Value::String(sanitize_text(text, context)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| sanitize_json_value(item, context, depth + 1))
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    let value = if is_secret_key(key) {
                        Value::String("[redacted]".into())
                    } else {
                        sanitize_json_value(value, context, depth + 1)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn is_secret_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '.'], "_");
    [
        "token",
        "api_key",
        "apikey",
        "authorization",
        "password",
        "passwd",
        "secret",
        "cookie",
        "private_key",
        "access_key",
    ]
    .iter()
    .any(|candidate| normalized == *candidate || normalized.ends_with(&format!("_{candidate}")))
}

fn sanitize_text(text: &str, context: &SanitizeContext<'_>) -> String {
    let mut output = text.to_owned();
    if let Some(root) = context.repository_root {
        let root = root.to_string_lossy();
        output = replace_path_prefix(&output, &root, "repo://");
    }
    if let Some(cwd) = context.adapter_cwd {
        output = replace_path_prefix(&output, cwd, "repo://");
    }
    if let Some(source_path) = context.source_path {
        output = output.replace(source_path, "[session-source]");
    }
    redact_absolute_paths(&output, None, None)
}

fn replace_path_prefix(text: &str, path: &str, replacement: &str) -> String {
    if path.is_empty() {
        return text.into();
    }
    let with_separator = format!("{}/", path.trim_end_matches('/'));
    text.replace(&with_separator, replacement)
        .replace(path, replacement)
}

fn redact_absolute_paths(
    text: &str,
    repository_root: Option<&Path>,
    adapter_cwd: Option<&str>,
) -> String {
    let mut output = text.to_owned();
    if let Some(root) = repository_root {
        output = replace_path_prefix(&output, &root.to_string_lossy(), "repo://");
    }
    if let Some(cwd) = adapter_cwd {
        output = replace_path_prefix(&output, cwd, "repo://");
    }
    for marker in ["/Users/", "/home/", "/tmp/", "/private/var/"] {
        loop {
            let Some(start) = output.find(marker) else {
                break;
            };
            let end = output[start..]
                .char_indices()
                .skip(1)
                .find_map(|(offset, character)| {
                    character
                        .is_whitespace()
                        .then_some(start + offset)
                        .or_else(|| {
                            matches!(character, '"' | '\'' | '`' | ')' | ']' | '}' | ',' | ';')
                                .then_some(start + offset)
                        })
                })
                .unwrap_or(output.len());
            output.replace_range(start..end, "[local-path]");
        }
    }
    output
}

struct RestoreMaterial {
    head: String,
    base: Option<String>,
    remote_fingerprint: Option<String>,
    bundle: Option<Vec<u8>>,
    staged_patch: Option<Vec<u8>>,
    unstaged_patch: Option<Vec<u8>>,
    untracked: Vec<RestoreUntracked>,
    handoff_markdown: Vec<u8>,
}

struct RestoreUntracked {
    relative_path: String,
    mode: String,
    bytes: Vec<u8>,
}

struct MaterializedRestore {
    _directory: TempDir,
    bundle_path: Option<PathBuf>,
    staged_path: Option<PathBuf>,
    unstaged_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilesystemIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    canonical_path: PathBuf,
}

#[derive(Debug)]
struct OwnedHandoffFile {
    path: PathBuf,
    identity: FilesystemIdentity,
}

#[derive(Debug)]
struct OwnedHandoffDirectory {
    path: PathBuf,
    identity: FilesystemIdentity,
    markdown: Option<OwnedHandoffFile>,
    json: Option<OwnedHandoffFile>,
}

#[derive(Debug)]
struct GitExcludeSnapshot {
    bytes: Vec<u8>,
    identity: FilesystemIdentity,
    permissions: fs::Permissions,
}

#[derive(Debug)]
struct GitExcludeMutation {
    path: PathBuf,
    previous: Option<GitExcludeSnapshot>,
    installed: Vec<u8>,
    installed_identity: FilesystemIdentity,
}

#[derive(Debug)]
struct HandoffInstallation {
    directory: OwnedHandoffDirectory,
    exclude: GitExcludeMutation,
}

#[derive(Debug, Default)]
struct FailedRestoreCleanup {
    incomplete: bool,
    preserved_worktree_path: Option<String>,
    preserved_branch_ref: Option<String>,
    diagnostics: Vec<Value>,
}

static GIT_EXCLUDE_MUTATION_LOCK: Mutex<()> = Mutex::new(());

fn restore_loaded_relaypack(
    loaded: LoadedRelaypack,
    request: RestoreRelaypackRequest,
) -> Result<RestoreRelaypackResult, CommandError> {
    if !loaded
        .envelope
        .handoff
        .pointer("/git/included")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return restore_conversation_only_relaypack(loaded, request);
    }
    let material = restore_material(&loaded.envelope)?;
    let repository_path = request
        .repository_path
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            CommandError::new(
                "repository_path_required",
                "a receiver Git repository is required for a package that contains Git changes",
            )
        })?;
    let branch_name = request
        .branch_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            CommandError::new(
                "branch_name_required",
                "a new branch name is required for a package that contains Git changes",
            )
        })?
        .to_owned();
    let requested_repository =
        canonical_existing_directory(Path::new(repository_path), "receiver repository")?;
    let safety_root = git_root_without_worktree_filters(&requested_repository)?;
    if safety_root != requested_repository {
        return Err(CommandError::new(
            "repository_root_required",
            format!(
                "repository_path must be the receiver worktree root '{}'",
                safety_root.display()
            ),
        ));
    }
    ensure_receiver_attributes_safe(&safety_root)?;
    ensure_worktree_attributes_safe(&safety_root)?;
    if let Some(head) = git_head_without_worktree_filters(&safety_root)? {
        ensure_commit_attributes_safe(&safety_root, &head)?;
    }
    let repository_inspection = git::inspect_repository(repository_path)?;
    let repository = PathBuf::from(&repository_inspection.root);
    if repository != requested_repository {
        return Err(CommandError::new(
            "repository_root_required",
            format!(
                "repository_path must be the receiver worktree root '{}'",
                repository.display()
            ),
        ));
    }
    validate_receiver_remote(
        &repository_inspection.remotes,
        material.remote_fingerprint.as_deref(),
    )?;
    validate_branch_name(&repository, &branch_name)?;
    ensure_branch_absent(&repository, &branch_name)?;
    let target = validate_new_directory_path(&request.target_path, "target worktree")?;
    if target == repository || target.starts_with(&repository) {
        // A nested worktree can be valid in Git, but it makes cleanup and
        // symlink boundaries needlessly risky for a received package.
        return Err(CommandError::new(
            "unsafe_target_path",
            "target_path must be outside the receiver repository",
        ));
    }

    let materialized = materialize_restore_payloads(&material)?;
    preflight_restore(
        &repository,
        &material,
        &materialized,
        &loaded.envelope.handoff,
    )?;

    let mut created_branch_oid = None;
    let mut handoff_installation = None;
    let restore_result = (|| {
        if let Some(bundle_path) = materialized.bundle_path.as_deref() {
            git_safe_checked_os(
                &repository,
                &[
                    OsString::from("bundle"),
                    OsString::from("unbundle"),
                    bundle_path.as_os_str().to_owned(),
                ],
                false,
                MAX_GIT_OUTPUT,
            )?;
        }
        ensure_commit_exists(&repository, &material.head)?;
        let mut worktree_args = safe_git_prefix();
        worktree_args.extend([
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("-b"),
            OsString::from(&branch_name),
            target.as_os_str().to_owned(),
            OsString::from(&material.head),
        ]);
        git_checked_os(&repository, &worktree_args, false, MAX_GIT_OUTPUT)?;
        created_branch_oid = Some(material.head.clone());

        apply_restore_payloads(&target, &material, &materialized)?;
        verify_restored_state(&target, &material)?;
        handoff_installation = Some(install_handoff_directory(
            &target,
            &material.handoff_markdown,
            &loaded.envelope.handoff,
        )?);
        Ok::<(), CommandError>(())
    })();

    if let Err(error) = restore_result {
        let cleanup = cleanup_failed_restore(
            &repository,
            &target,
            &branch_name,
            created_branch_oid.as_deref(),
            handoff_installation.as_ref(),
        );
        return Err(with_failed_restore_cleanup(error, cleanup));
    }

    let handoff_installation = handoff_installation.ok_or_else(|| {
        CommandError::new(
            "handoff_write_failed",
            "handoff installation did not return a directory",
        )
    })?;
    let handoff_directory = handoff_installation.directory.path.clone();

    Ok(RestoreRelaypackResult {
        worktree_path: target.to_string_lossy().into_owned(),
        branch_name: Some(branch_name),
        head: Some(material.head),
        handoff_directory: handoff_directory.to_string_lossy().into_owned(),
        handoff_markdown_path: handoff_directory
            .join("HANDOFF.md")
            .to_string_lossy()
            .into_owned(),
        handoff_json_path: handoff_directory
            .join("handoff.json")
            .to_string_lossy()
            .into_owned(),
        staged_applied: material.staged_patch.is_some(),
        unstaged_applied: material.unstaged_patch.is_some(),
        untracked_files_restored: material.untracked.len(),
        preview: loaded.preview,
    })
}

fn restore_conversation_only_relaypack(
    loaded: LoadedRelaypack,
    request: RestoreRelaypackRequest,
) -> Result<RestoreRelaypackResult, CommandError> {
    let handoff_markdown = handoff_markdown_payload(&loaded.envelope)?;
    let target = validate_new_directory_path(&request.target_path, "handoff folder")?;
    let handoff_directory =
        install_plain_handoff_directory(&target, &handoff_markdown, &loaded.envelope.handoff)?.path;

    Ok(RestoreRelaypackResult {
        worktree_path: target.to_string_lossy().into_owned(),
        branch_name: None,
        head: None,
        handoff_directory: handoff_directory.to_string_lossy().into_owned(),
        handoff_markdown_path: handoff_directory
            .join("HANDOFF.md")
            .to_string_lossy()
            .into_owned(),
        handoff_json_path: handoff_directory
            .join("handoff.json")
            .to_string_lossy()
            .into_owned(),
        staged_applied: false,
        unstaged_applied: false,
        untracked_files_restored: 0,
        preview: loaded.preview,
    })
}

fn restore_material(envelope: &PackageEnvelope) -> Result<RestoreMaterial, CommandError> {
    let handoff = &envelope.handoff;
    let head = handoff
        .pointer("/git/repository/head")
        .and_then(Value::as_str)
        .filter(|head| is_commit_id(head))
        .ok_or_else(|| {
            CommandError::new(
                "relaypack_invalid",
                "Git package is missing a valid HEAD commit",
            )
        })?
        .to_owned();
    let base = handoff
        .pointer("/git/repository/base")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let remote_fingerprint = handoff
        .pointer("/git/repository/remote_fingerprint")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let payloads: HashMap<&str, &PackagePayload> = envelope
        .payloads
        .iter()
        .map(|payload| (payload.asset_id.as_str(), payload))
        .collect();

    let bundle = payload_from_capture(
        handoff.pointer("/git/capture/local_commits"),
        &payloads,
        "git_bundle",
    )?;
    let staged_patch = payload_from_capture(
        handoff.pointer("/git/capture/staged_patch"),
        &payloads,
        "git_patch",
    )?;
    let unstaged_patch = payload_from_capture(
        handoff.pointer("/git/capture/unstaged_patch"),
        &payloads,
        "git_patch",
    )?;

    let untracked_entries = handoff
        .pointer("/git/capture/untracked_files")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CommandError::new("relaypack_invalid", "untracked file manifest is missing")
        })?;
    if untracked_entries.len() > MAX_UNTRACKED_FILES {
        return Err(CommandError::new(
            "relaypack_invalid",
            "untracked file manifest exceeds the file count limit",
        ));
    }
    let mut untracked = Vec::new();
    let mut paths = HashSet::new();
    for entry in untracked_entries {
        let logical_path = required_string(entry, "logical_path", "untracked file")?;
        validate_repo_uri(logical_path)?;
        let relative_path = logical_path
            .strip_prefix("repo://")
            .expect("validated repo URI")
            .to_owned();
        if !paths.insert(collision_key(&relative_path)) {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!("duplicate or conflicting untracked path '{logical_path}'"),
            ));
        }
        let asset_id = required_string(entry, "asset_id", "untracked file")?;
        let mode = required_string(entry, "mode", "untracked file")?;
        if !matches!(mode, "100644" | "100755") {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!("unsupported untracked file mode '{mode}'"),
            ));
        }
        let payload = payloads.get(asset_id).ok_or_else(|| {
            CommandError::new(
                "relaypack_invalid",
                format!("untracked asset '{asset_id}' is missing"),
            )
        })?;
        if payload.kind != "untracked_file" || payload.mode.as_deref() != Some(mode) {
            return Err(CommandError::new(
                "relaypack_invalid",
                format!("untracked asset '{asset_id}' has inconsistent type or mode"),
            ));
        }
        untracked.push(RestoreUntracked {
            relative_path,
            mode: mode.into(),
            bytes: payload.decode()?,
        });
    }

    Ok(RestoreMaterial {
        head,
        base,
        remote_fingerprint,
        bundle,
        staged_patch,
        unstaged_patch,
        untracked,
        handoff_markdown: handoff_markdown_payload(envelope)?,
    })
}

fn handoff_markdown_payload(envelope: &PackageEnvelope) -> Result<Vec<u8>, CommandError> {
    let handoff_asset = envelope
        .payloads
        .iter()
        .find(|payload| payload.kind == "handoff_document")
        .ok_or_else(|| CommandError::new("relaypack_invalid", "HANDOFF.md payload is missing"))?;
    handoff_asset.decode()
}

fn payload_from_capture(
    capture: Option<&Value>,
    payloads: &HashMap<&str, &PackagePayload>,
    expected_kind: &str,
) -> Result<Option<Vec<u8>>, CommandError> {
    let Some(capture) = capture else {
        return Err(CommandError::new(
            "relaypack_invalid",
            "Git capture manifest is incomplete",
        ));
    };
    if capture.get("status").and_then(Value::as_str) != Some("included") {
        return Ok(None);
    }
    let asset_id = required_string(capture, "asset_id", "Git capture item")?;
    let payload = payloads.get(asset_id).ok_or_else(|| {
        CommandError::new(
            "relaypack_invalid",
            format!("Git capture asset '{asset_id}' is missing"),
        )
    })?;
    if payload.kind != expected_kind {
        return Err(CommandError::new(
            "relaypack_invalid",
            format!("Git capture asset '{asset_id}' has the wrong kind"),
        ));
    }
    let bytes = payload.decode()?;
    if expected_kind == "git_patch" {
        validate_patch_payload(&bytes)?;
    }
    Ok(Some(bytes))
}

fn materialize_restore_payloads(
    material: &RestoreMaterial,
) -> Result<MaterializedRestore, CommandError> {
    let directory = TempBuilder::new()
        .prefix("relay-restore-")
        .tempdir()
        .map_err(|error| {
            CommandError::new(
                "restore_preflight_failed",
                format!("cannot create restore workspace: {error}"),
            )
        })?;
    let bundle_path = material
        .bundle
        .as_deref()
        .map(|bytes| write_temp_payload(directory.path(), "local-commits.bundle", bytes))
        .transpose()?;
    let staged_path = material
        .staged_patch
        .as_deref()
        .map(|bytes| write_temp_payload(directory.path(), "staged.patch", bytes))
        .transpose()?;
    let unstaged_path = material
        .unstaged_patch
        .as_deref()
        .map(|bytes| write_temp_payload(directory.path(), "unstaged.patch", bytes))
        .transpose()?;
    Ok(MaterializedRestore {
        _directory: directory,
        bundle_path,
        staged_path,
        unstaged_path,
    })
}

fn write_temp_payload(parent: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, CommandError> {
    let path = parent.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            CommandError::new(
                "restore_preflight_failed",
                format!("cannot create temporary payload: {error}"),
            )
        })?;
    file.write_all(bytes).map_err(|error| {
        CommandError::new(
            "restore_preflight_failed",
            format!("cannot write temporary payload: {error}"),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                CommandError::new(
                    "restore_preflight_failed",
                    format!("cannot protect temporary payload: {error}"),
                )
            })?;
    }
    Ok(path)
}

fn preflight_restore(
    receiver: &Path,
    material: &RestoreMaterial,
    materialized: &MaterializedRestore,
    _handoff: &Value,
) -> Result<(), CommandError> {
    let preflight_parent = materialized._directory.path().join("preflight-parent");
    fs::create_dir(&preflight_parent).map_err(|error| {
        CommandError::new(
            "restore_preflight_failed",
            format!("cannot create preflight directory: {error}"),
        )
    })?;
    let preflight = preflight_parent.join("repository");
    let mut clone_args = safe_git_prefix();
    clone_args.extend([
        OsString::from("-c"),
        OsString::from("protocol.file.allow=always"),
        OsString::from("clone"),
        OsString::from("--shared"),
        OsString::from("--no-checkout"),
        OsString::from("--no-tags"),
        receiver.as_os_str().to_owned(),
        preflight.as_os_str().to_owned(),
    ]);
    git_checked_os(receiver, &clone_args, false, MAX_GIT_OUTPUT)?;

    if let Some(bundle_path) = materialized.bundle_path.as_deref() {
        if let Some(base) = material.base.as_deref() {
            ensure_commit_exists(&preflight, base)?;
        }
        git_safe_checked_os(
            &preflight,
            &[
                OsString::from("bundle"),
                OsString::from("verify"),
                bundle_path.as_os_str().to_owned(),
            ],
            true,
            MAX_GIT_OUTPUT,
        )?;
        let heads = git_safe_checked_os(
            &preflight,
            &[
                OsString::from("bundle"),
                OsString::from("list-heads"),
                bundle_path.as_os_str().to_owned(),
            ],
            true,
            MAX_GIT_OUTPUT,
        )?;
        if !String::from_utf8_lossy(&heads.stdout)
            .lines()
            .any(|line| line.split_whitespace().next() == Some(material.head.as_str()))
        {
            return Err(CommandError::new(
                "relaypack_invalid",
                "local commit bundle does not advertise the package HEAD",
            ));
        }
        git_safe_checked_os(
            &preflight,
            &[
                OsString::from("bundle"),
                OsString::from("unbundle"),
                bundle_path.as_os_str().to_owned(),
            ],
            false,
            MAX_GIT_OUTPUT,
        )?;
    }
    ensure_commit_exists(&preflight, &material.head)?;
    ensure_commit_attributes_safe(&preflight, &material.head)?;

    let mut checkout_args = safe_git_prefix();
    checkout_args.extend([
        OsString::from("checkout"),
        OsString::from("--detach"),
        OsString::from(&material.head),
    ]);
    git_checked_os(&preflight, &checkout_args, false, MAX_GIT_OUTPUT)?;
    apply_restore_payloads(&preflight, material, materialized)?;
    verify_restored_state(&preflight, material)?;
    Ok(())
}

fn apply_restore_payloads(
    worktree: &Path,
    material: &RestoreMaterial,
    materialized: &MaterializedRestore,
) -> Result<(), CommandError> {
    if let Some(path) = materialized.staged_path.as_deref() {
        git_apply(worktree, path, true, true)?;
        git_apply(worktree, path, true, false)?;
    }
    if let Some(path) = materialized.unstaged_path.as_deref() {
        git_apply(worktree, path, false, true)?;
        git_apply(worktree, path, false, false)?;
    }
    for file in &material.untracked {
        write_restored_untracked(worktree, file)?;
    }
    Ok(())
}

fn git_apply(worktree: &Path, patch: &Path, staged: bool, check: bool) -> Result<(), CommandError> {
    let mut args = safe_git_prefix();
    args.push(OsString::from("apply"));
    if check {
        args.push(OsString::from("--check"));
    }
    if staged {
        args.push(OsString::from("--index"));
    }
    args.extend([
        OsString::from("--binary"),
        OsString::from("--whitespace=nowarn"),
        patch.as_os_str().to_owned(),
    ]);
    git_checked_os(worktree, &args, false, MAX_GIT_OUTPUT)?;
    Ok(())
}

fn verify_restored_state(worktree: &Path, material: &RestoreMaterial) -> Result<(), CommandError> {
    if let Some(expected) = material.staged_patch.as_deref() {
        let actual = git_stdout(
            worktree,
            &[
                "diff",
                "--cached",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                "--no-textconv",
                "--src-prefix=a/",
                "--dst-prefix=b/",
            ],
            true,
            MAX_GIT_OUTPUT,
        )?;
        if actual != expected {
            return Err(CommandError::new(
                "restore_verification_failed",
                "restored staged patch differs from the package",
            ));
        }
    }
    if let Some(expected) = material.unstaged_patch.as_deref() {
        let actual = git_stdout(
            worktree,
            &[
                "diff",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                "--no-textconv",
                "--src-prefix=a/",
                "--dst-prefix=b/",
            ],
            true,
            MAX_GIT_OUTPUT,
        )?;
        if actual != expected {
            return Err(CommandError::new(
                "restore_verification_failed",
                "restored unstaged patch differs from the package",
            ));
        }
    }
    for file in &material.untracked {
        let path = worktree.join(&file.relative_path);
        let actual = read_file_limited(&path, MAX_SINGLE_FILE_BYTES as usize)?;
        if actual != file.bytes {
            return Err(CommandError::new(
                "restore_verification_failed",
                format!("restored untracked file '{}' differs", file.relative_path),
            ));
        }
    }
    Ok(())
}

fn write_restored_untracked(
    worktree: &Path,
    restore: &RestoreUntracked,
) -> Result<(), CommandError> {
    validate_relative_path(&restore.relative_path, true)?;
    let mut current = worktree.to_path_buf();
    let components: Vec<&OsStr> = Path::new(&restore.relative_path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect();
    let (file_name, parents) = components
        .split_last()
        .ok_or_else(|| CommandError::new("unsafe_path", "untracked file path is empty"))?;
    for component in parents {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(CommandError::new(
                    "unsafe_path",
                    format!(
                        "untracked file parent '{}' is not a normal directory",
                        current.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|error| {
                    CommandError::new(
                        "restore_write_failed",
                        format!("cannot create '{}': {error}", current.display()),
                    )
                })?;
            }
            Err(error) => {
                return Err(CommandError::new(
                    "restore_write_failed",
                    format!("cannot inspect '{}': {error}", current.display()),
                ))
            }
        }
    }
    let destination = current.join(file_name);
    if fs::symlink_metadata(&destination).is_ok() {
        return Err(CommandError::new(
            "restore_path_conflict",
            format!(
                "untracked file '{}' conflicts with an existing restored path",
                restore.relative_path
            ),
        ));
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
        .map_err(|error| {
            CommandError::new(
                "restore_write_failed",
                format!("cannot create '{}': {error}", destination.display()),
            )
        })?;
    output.write_all(&restore.bytes).map_err(|error| {
        CommandError::new(
            "restore_write_failed",
            format!("cannot write '{}': {error}", destination.display()),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if restore.mode == "100755" {
            0o755
        } else {
            0o644
        };
        output
            .set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|error| {
                CommandError::new(
                    "restore_write_failed",
                    format!("cannot set mode on '{}': {error}", destination.display()),
                )
            })?;
    }
    Ok(())
}

fn install_handoff_directory(
    worktree: &Path,
    markdown: &[u8],
    handoff: &Value,
) -> Result<HandoffInstallation, CommandError> {
    install_handoff_directory_with_id_source(worktree, markdown, handoff, random_handoff_id)
}

fn install_plain_handoff_directory(
    target: &Path,
    markdown: &[u8],
    handoff: &Value,
) -> Result<OwnedHandoffDirectory, CommandError> {
    let handoff_json = serde_json::to_vec_pretty(handoff).map_err(|error| {
        CommandError::new(
            "handoff_write_failed",
            format!("cannot encode handoff.json: {error}"),
        )
    })?;
    fs::create_dir(target).map_err(|error| {
        CommandError::new(
            "handoff_write_failed",
            format!(
                "cannot create handoff folder '{}': {error}",
                target.display()
            ),
        )
    })?;
    let metadata = fs::symlink_metadata(target).map_err(|error| {
        CommandError::new(
            "handoff_write_failed",
            format!("cannot inspect new handoff folder: {error}"),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CommandError::new(
            "handoff_write_failed",
            "new handoff path is not an ordinary directory",
        ));
    }
    let mut owned = OwnedHandoffDirectory {
        path: target.to_path_buf(),
        identity: filesystem_identity(&metadata, target)?,
        markdown: None,
        json: None,
    };
    let install_result = (|| {
        owned.markdown = Some(write_new_regular_file(
            &owned.path.join("HANDOFF.md"),
            markdown,
            0o444,
        )?);
        owned.json = Some(write_new_regular_file(
            &owned.path.join("handoff.json"),
            &handoff_json,
            0o444,
        )?);
        let directory = File::open(&owned.path).map_err(|error| {
            CommandError::new(
                "handoff_write_failed",
                format!("cannot open new handoff folder for sync: {error}"),
            )
        })?;
        directory.sync_all().map_err(|error| {
            CommandError::new(
                "handoff_write_failed",
                format!("cannot sync new handoff folder: {error}"),
            )
        })?;
        Ok::<(), CommandError>(())
    })();
    if let Err(error) = install_result {
        cleanup_owned_handoff_directory(&owned);
        return Err(error);
    }
    Ok(owned)
}

fn install_handoff_directory_with_id_source<F>(
    worktree: &Path,
    markdown: &[u8],
    handoff: &Value,
    mut next_id: F,
) -> Result<HandoffInstallation, CommandError>
where
    F: FnMut() -> Result<String, CommandError>,
{
    let worktree = canonical_handoff_worktree(worktree)?;
    let handoff_json = serde_json::to_vec_pretty(handoff).map_err(|error| {
        CommandError::new(
            "handoff_write_failed",
            format!("cannot encode handoff.json: {error}"),
        )
    })?;

    for _ in 0..HANDOFF_DIRECTORY_ATTEMPTS {
        let id = next_id()?;
        validate_handoff_id(&id)?;
        let directory_name = format!(".relay-handoff-{id}");
        if handoff_path_conflicts_with_git(&worktree, &directory_name)? {
            continue;
        }
        let directory_path = worktree.join(&directory_name);
        match fs::symlink_metadata(&directory_path) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(CommandError::new(
                    "handoff_write_failed",
                    format!("cannot inspect handoff directory path: {error}"),
                ))
            }
        }
        match fs::create_dir(&directory_path) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotFound
                ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(CommandError::new(
                    "handoff_write_failed",
                    format!("cannot create handoff directory: {error}"),
                ))
            }
        }

        let directory_metadata = fs::symlink_metadata(&directory_path).map_err(|error| {
            CommandError::new(
                "handoff_write_failed",
                format!("cannot inspect new handoff directory: {error}"),
            )
        })?;
        if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
            return Err(CommandError::new(
                "handoff_write_failed",
                "new handoff path is not an ordinary directory",
            ));
        }
        let mut owned = OwnedHandoffDirectory {
            identity: filesystem_identity(&directory_metadata, &directory_path)?,
            path: directory_path,
            markdown: None,
            json: None,
        };

        let install_result = (|| {
            owned.markdown = Some(write_new_regular_file(
                &owned.path.join("HANDOFF.md"),
                markdown,
                0o444,
            )?);
            owned.json = Some(write_new_regular_file(
                &owned.path.join("handoff.json"),
                &handoff_json,
                0o444,
            )?);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&owned.path, fs::Permissions::from_mode(0o555)).map_err(
                    |error| {
                        CommandError::new(
                            "handoff_write_failed",
                            format!("cannot make handoff directory read-only: {error}"),
                        )
                    },
                )?;
            }
            add_handoff_git_exclude(&worktree, &directory_name)
        })();

        match install_result {
            Ok(exclude) => {
                return Ok(HandoffInstallation {
                    directory: owned,
                    exclude,
                })
            }
            Err(error) => {
                cleanup_owned_handoff_directory(&owned);
                return Err(error);
            }
        }
    }

    Err(CommandError::new(
        "handoff_target_collision",
        format!(
            "could not reserve a handoff directory after {HANDOFF_DIRECTORY_ATTEMPTS} attempts"
        ),
    ))
}

fn random_handoff_id() -> Result<String, CommandError> {
    let mut bytes = [0_u8; HANDOFF_ID_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| {
        CommandError::new(
            "handoff_random_failed",
            format!("cannot create a random handoff id: {error}"),
        )
    })?;
    Ok(hex::encode(bytes))
}

fn validate_handoff_id(id: &str) -> Result<(), CommandError> {
    if id.len() < 32
        || id.len() % 2 != 0
        || !id
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(CommandError::new(
            "handoff_random_failed",
            "handoff id must contain at least 128 bits encoded as lowercase hexadecimal",
        ));
    }
    Ok(())
}

fn canonical_handoff_worktree(worktree: &Path) -> Result<PathBuf, CommandError> {
    let metadata = fs::symlink_metadata(worktree).map_err(|error| {
        CommandError::new(
            "handoff_write_failed",
            format!("cannot inspect restored worktree: {error}"),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CommandError::new(
            "handoff_write_failed",
            "restored worktree must be a non-symlink directory",
        ));
    }
    let canonical = fs::canonicalize(worktree).map_err(|error| {
        CommandError::new(
            "handoff_write_failed",
            format!("cannot resolve restored worktree: {error}"),
        )
    })?;
    if canonical != worktree {
        return Err(CommandError::new(
            "handoff_write_failed",
            "restored worktree path traverses a symbolic link",
        ));
    }
    Ok(canonical)
}

fn handoff_path_conflicts_with_git(
    worktree: &Path,
    directory_name: &str,
) -> Result<bool, CommandError> {
    let case_insensitive_literal = format!(":(icase,literal){directory_name}");
    let index = git_safe_checked_os(
        worktree,
        &[
            OsString::from("ls-files"),
            OsString::from("-z"),
            OsString::from("--cached"),
            OsString::from("--stage"),
            OsString::from("--"),
            OsString::from(case_insensitive_literal),
        ],
        true,
        1024 * 1024,
    )?;
    if !index.stdout.is_empty() {
        return Ok(true);
    }
    let head = git_safe_checked_os(
        worktree,
        &[
            OsString::from("ls-tree"),
            OsString::from("-z"),
            OsString::from("--name-only"),
            OsString::from("HEAD"),
        ],
        true,
        1024 * 1024,
    )?;
    Ok(head
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .any(|entry| entry.eq_ignore_ascii_case(directory_name.as_bytes())))
}

fn write_new_regular_file(
    path: &Path,
    bytes: &[u8],
    unix_mode: u32,
) -> Result<OwnedHandoffFile, CommandError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(|error| {
        CommandError::new(
            "handoff_write_failed",
            format!("cannot create '{}': {error}", path.display()),
        )
    })?;
    let metadata = file.metadata().map_err(|error| {
        CommandError::new(
            "handoff_write_failed",
            format!("cannot inspect '{}': {error}", path.display()),
        )
    })?;
    if !metadata.is_file() {
        return Err(CommandError::new(
            "handoff_write_failed",
            format!("'{}' is not an ordinary file", path.display()),
        ));
    }
    let owned = OwnedHandoffFile {
        path: path.to_path_buf(),
        identity: filesystem_identity(&metadata, path)?,
    };
    let write_result = (|| {
        file.write_all(bytes).map_err(|error| {
            CommandError::new(
                "handoff_write_failed",
                format!("cannot write '{}': {error}", path.display()),
            )
        })?;
        file.sync_all().map_err(|error| {
            CommandError::new(
                "handoff_write_failed",
                format!("cannot sync '{}': {error}", path.display()),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(unix_mode))
                .map_err(|error| {
                    CommandError::new(
                        "handoff_write_failed",
                        format!("cannot protect '{}': {error}", path.display()),
                    )
                })?;
        }
        Ok::<(), CommandError>(())
    })();
    drop(file);
    if let Err(error) = write_result {
        cleanup_owned_file(&owned);
        return Err(error);
    }
    Ok(owned)
}

fn add_handoff_git_exclude(
    worktree: &Path,
    directory_name: &str,
) -> Result<GitExcludeMutation, CommandError> {
    let path = git_info_exclude_path(worktree)?;
    let _guard = lock_git_exclude_mutation()?;
    let previous = read_git_exclude_snapshot(&path)?;
    let mut installed = previous
        .as_ref()
        .map_or_else(Vec::new, |snapshot| snapshot.bytes.clone());
    if !installed.is_empty() && !installed.ends_with(b"\n") {
        installed.push(b'\n');
    }
    installed.extend_from_slice(format!("/{directory_name}/\n").as_bytes());
    if installed.len() > MAX_GIT_EXCLUDE_BYTES {
        return Err(CommandError::new(
            "git_exclude_write_failed",
            "Git info/exclude is too large to update safely",
        ));
    }
    let mode = previous.as_ref().map_or(0o644, git_exclude_snapshot_mode);
    let installed_identity =
        atomic_replace_git_exclude(&path, previous.as_ref(), &installed, mode)?;
    Ok(GitExcludeMutation {
        path,
        previous,
        installed,
        installed_identity,
    })
}

fn git_info_exclude_path(worktree: &Path) -> Result<PathBuf, CommandError> {
    let common_output = git_safe_checked_os(
        worktree,
        &[
            OsString::from("rev-parse"),
            OsString::from("--git-common-dir"),
        ],
        true,
        1024 * 1024,
    )?;
    let common_raw = bytes_to_trimmed_string(&common_output.stdout);
    let common_raw = absolute_git_path(worktree, &common_raw, "Git common directory")?;
    let common = canonical_existing_directory(&common_raw, "Git common directory")?;
    if common_raw != common {
        return Err(CommandError::new(
            "git_exclude_write_failed",
            "Git common directory path traverses a symbolic link",
        ));
    }

    let exclude_output = git_safe_checked_os(
        worktree,
        &[
            OsString::from("rev-parse"),
            OsString::from("--git-path"),
            OsString::from("info/exclude"),
        ],
        true,
        1024 * 1024,
    )?;
    let exclude_raw = bytes_to_trimmed_string(&exclude_output.stdout);
    let exclude_raw = absolute_git_path(worktree, &exclude_raw, "Git info/exclude")?;
    let info_raw = exclude_raw.parent().ok_or_else(|| {
        CommandError::new(
            "git_exclude_write_failed",
            "Git info/exclude path has no parent directory",
        )
    })?;
    let info = canonical_existing_directory(info_raw, "Git info directory")?;
    let expected_info = common.join("info");
    if info_raw != info || info != expected_info || exclude_raw != info.join("exclude") {
        return Err(CommandError::new(
            "git_exclude_write_failed",
            "Git info/exclude does not resolve directly inside the common Git directory",
        ));
    }
    Ok(exclude_raw)
}

fn absolute_git_path(worktree: &Path, raw: &str, label: &str) -> Result<PathBuf, CommandError> {
    if raw.is_empty() || raw.contains('\0') || raw.contains('\n') || raw.contains('\r') {
        return Err(CommandError::new(
            "git_protocol_error",
            format!("Git returned an invalid {label} path"),
        ));
    }
    let path = PathBuf::from(raw);
    Ok(if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    })
}

fn lock_git_exclude_mutation() -> Result<MutexGuard<'static, ()>, CommandError> {
    GIT_EXCLUDE_MUTATION_LOCK.lock().map_err(|_| {
        CommandError::new(
            "git_exclude_write_failed",
            "Git info/exclude update lock is unavailable",
        )
    })
}

fn read_git_exclude_snapshot(path: &Path) -> Result<Option<GitExcludeSnapshot>, CommandError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(CommandError::new(
                "git_exclude_write_failed",
                "Git info/exclude must be a non-symlink ordinary file",
            ))
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(CommandError::new(
                "git_exclude_write_failed",
                format!("cannot inspect Git info/exclude: {error}"),
            ))
        }
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(|error| {
        CommandError::new(
            "git_exclude_write_failed",
            format!("cannot open Git info/exclude safely: {error}"),
        )
    })?;
    let metadata = file.metadata().map_err(|error| {
        CommandError::new(
            "git_exclude_write_failed",
            format!("cannot inspect open Git info/exclude: {error}"),
        )
    })?;
    if !metadata.is_file() || metadata.len() > MAX_GIT_EXCLUDE_BYTES as u64 {
        return Err(CommandError::new(
            "git_exclude_write_failed",
            "Git info/exclude is not an ordinary file within the size limit",
        ));
    }
    let identity = filesystem_identity(&metadata, path)?;
    let path_metadata = fs::symlink_metadata(path).map_err(|error| {
        CommandError::new(
            "git_exclude_write_failed",
            format!("cannot re-check Git info/exclude: {error}"),
        )
    })?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || !filesystem_identity_matches(&path_metadata, path, &identity)
    {
        return Err(CommandError::new(
            "git_exclude_write_failed",
            "Git info/exclude changed while it was being inspected",
        ));
    }
    let initial_modified = metadata.modified().ok();
    let initial_permissions = metadata.permissions();
    let mut limited = std::io::Read::take(&mut file, MAX_GIT_EXCLUDE_BYTES as u64 + 1);
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    limited.read_to_end(&mut bytes).map_err(|error| {
        CommandError::new(
            "git_exclude_write_failed",
            format!("cannot read Git info/exclude: {error}"),
        )
    })?;
    if bytes.len() > MAX_GIT_EXCLUDE_BYTES {
        return Err(CommandError::new(
            "git_exclude_write_failed",
            "Git info/exclude grew beyond the size limit while it was being read",
        ));
    }
    let open_metadata = file.metadata().map_err(|error| {
        CommandError::new(
            "git_exclude_write_failed",
            format!("cannot re-check open Git info/exclude: {error}"),
        )
    })?;
    let path_metadata = fs::symlink_metadata(path).map_err(|error| {
        CommandError::new(
            "git_exclude_write_failed",
            format!("cannot re-check Git info/exclude after reading: {error}"),
        )
    })?;
    let metadata_changed = bytes.len() as u64 != metadata.len()
        || open_metadata.len() != metadata.len()
        || open_metadata.modified().ok() != initial_modified
        || path_metadata.modified().ok() != initial_modified
        || !permissions_match(&open_metadata.permissions(), &initial_permissions)
        || !permissions_match(&path_metadata.permissions(), &initial_permissions)
        || !filesystem_identity_matches(&open_metadata, path, &identity)
        || path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || !filesystem_identity_matches(&path_metadata, path, &identity);
    if metadata_changed {
        return Err(CommandError::new(
            "git_exclude_changed",
            "Git info/exclude changed while it was being read",
        ));
    }
    Ok(Some(GitExcludeSnapshot {
        bytes,
        identity,
        permissions: initial_permissions,
    }))
}

fn atomic_replace_git_exclude(
    path: &Path,
    expected: Option<&GitExcludeSnapshot>,
    bytes: &[u8],
    mode: u32,
) -> Result<FilesystemIdentity, CommandError> {
    let parent = path.parent().ok_or_else(|| {
        CommandError::new(
            "git_exclude_write_failed",
            "Git info/exclude path has no parent directory",
        )
    })?;
    let temp_path = reserve_git_exclude_temp_path(parent)?;
    let temporary = write_new_regular_file(&temp_path, bytes, mode)
        .map_err(|error| CommandError::new("git_exclude_write_failed", error.message))?;

    let current = match read_git_exclude_snapshot(path) {
        Ok(current) => current,
        Err(error) => {
            cleanup_owned_file(&temporary);
            return Err(error);
        }
    };
    if !git_exclude_snapshot_matches(current.as_ref(), expected) {
        cleanup_owned_file(&temporary);
        return Err(CommandError::new(
            "git_exclude_changed",
            "Git info/exclude changed while Relay was preparing its update",
        ));
    }
    if let Err(error) = fs::rename(&temp_path, path) {
        cleanup_owned_file(&temporary);
        return Err(CommandError::new(
            "git_exclude_write_failed",
            format!("cannot atomically update Git info/exclude: {error}"),
        ));
    }
    let _ = sync_directory(parent);
    Ok(temporary.identity)
}

fn reserve_git_exclude_temp_path(parent: &Path) -> Result<PathBuf, CommandError> {
    for _ in 0..HANDOFF_DIRECTORY_ATTEMPTS {
        let path = parent.join(format!(".relay-exclude-{}.tmp", random_handoff_id()?));
        match fs::symlink_metadata(&path) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(path),
            Err(error) => {
                return Err(CommandError::new(
                    "git_exclude_write_failed",
                    format!("cannot inspect temporary Git exclude path: {error}"),
                ))
            }
        }
    }
    Err(CommandError::new(
        "git_exclude_write_failed",
        "cannot reserve a temporary Git exclude path",
    ))
}

#[cfg(unix)]
fn git_exclude_snapshot_mode(snapshot: &GitExcludeSnapshot) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    snapshot.permissions.mode() & 0o777
}

#[cfg(not(unix))]
fn git_exclude_snapshot_mode(_snapshot: &GitExcludeSnapshot) -> u32 {
    0o644
}

fn git_exclude_snapshot_matches(
    actual: Option<&GitExcludeSnapshot>,
    expected: Option<&GitExcludeSnapshot>,
) -> bool {
    match (actual, expected) {
        (None, None) => true,
        (Some(actual), Some(expected)) => {
            actual.bytes == expected.bytes
                && actual.identity == expected.identity
                && permissions_match(&actual.permissions, &expected.permissions)
        }
        _ => false,
    }
}

#[cfg(unix)]
fn permissions_match(left: &fs::Permissions, right: &fs::Permissions) -> bool {
    use std::os::unix::fs::PermissionsExt;
    left.mode() == right.mode()
}

#[cfg(not(unix))]
fn permissions_match(left: &fs::Permissions, right: &fs::Permissions) -> bool {
    left.readonly() == right.readonly()
}

fn sync_directory(path: &Path) -> Result<(), CommandError> {
    let directory = File::open(path).map_err(|error| {
        CommandError::new(
            "git_exclude_write_failed",
            format!("cannot open Git info directory for sync: {error}"),
        )
    })?;
    directory.sync_all().map_err(|error| {
        CommandError::new(
            "git_exclude_write_failed",
            format!("cannot sync Git info directory: {error}"),
        )
    })
}

fn rollback_git_exclude(mutation: &GitExcludeMutation) -> Result<(), CommandError> {
    let _guard = lock_git_exclude_mutation()?;
    let current = read_git_exclude_snapshot(&mutation.path)?.ok_or_else(|| {
        CommandError::new(
            "git_exclude_changed",
            "Git info/exclude disappeared before Relay could restore it",
        )
    })?;
    if current.identity != mutation.installed_identity || current.bytes != mutation.installed {
        return Err(CommandError::new(
            "git_exclude_changed",
            "Git info/exclude changed after Relay installed its ignore rule",
        ));
    }
    let Some(previous) = mutation.previous.as_ref() else {
        return Err(CommandError::new(
            "git_exclude_rollback_incomplete",
            "Git info/exclude did not exist before Relay updated it; deleting it safely requires a path-level compare-and-swap",
        ));
    };
    atomic_replace_git_exclude(
        &mutation.path,
        Some(&current),
        &previous.bytes,
        git_exclude_snapshot_mode(previous),
    )?;
    Ok(())
}

#[cfg(unix)]
fn filesystem_identity(
    metadata: &fs::Metadata,
    _path: &Path,
) -> Result<FilesystemIdentity, CommandError> {
    use std::os::unix::fs::MetadataExt;
    Ok(FilesystemIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn filesystem_identity(
    _metadata: &fs::Metadata,
    path: &Path,
) -> Result<FilesystemIdentity, CommandError> {
    let canonical_path = fs::canonicalize(path).map_err(|error| {
        CommandError::new(
            "handoff_write_failed",
            format!("cannot resolve '{}': {error}", path.display()),
        )
    })?;
    Ok(FilesystemIdentity { canonical_path })
}

fn filesystem_identity_matches(
    metadata: &fs::Metadata,
    path: &Path,
    expected: &FilesystemIdentity,
) -> bool {
    filesystem_identity(metadata, path).is_ok_and(|actual| actual == *expected)
}

fn cleanup_owned_file(owned: &OwnedHandoffFile) {
    let Ok(metadata) = fs::symlink_metadata(&owned.path) else {
        return;
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || !filesystem_identity_matches(&metadata, &owned.path, &owned.identity)
    {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&owned.path, fs::Permissions::from_mode(0o600));
    }
    let _ = fs::remove_file(&owned.path);
}

fn cleanup_owned_handoff_directory(owned: &OwnedHandoffDirectory) {
    let Ok(metadata) = fs::symlink_metadata(&owned.path) else {
        return;
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || !filesystem_identity_matches(&metadata, &owned.path, &owned.identity)
    {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&owned.path, fs::Permissions::from_mode(0o700));
    }
    if let Some(markdown) = owned.markdown.as_ref() {
        cleanup_owned_file(markdown);
    }
    if let Some(json) = owned.json.as_ref() {
        cleanup_owned_file(json);
    }
    let _ = fs::remove_dir(&owned.path);
}

fn cleanup_failed_restore(
    _repository: &Path,
    target: &Path,
    branch: &str,
    created_branch_oid: Option<&str>,
    handoff: Option<&HandoffInstallation>,
) -> FailedRestoreCleanup {
    let mut cleanup = FailedRestoreCleanup::default();
    if let Some(handoff) = handoff {
        if let Err(error) = rollback_git_exclude(&handoff.exclude) {
            cleanup.preserve_worktree(target, branch, "git_exclude_rollback", &error);
            return cleanup;
        }
        cleanup_owned_handoff_directory(&handoff.directory);
    }

    if created_branch_oid.is_none() {
        return cleanup;
    }
    cleanup.preserve_worktree(
        target,
        branch,
        "worktree_preserved",
        &CommandError::new(
            "restore_cleanup_preserved",
            "Relay preserved the failed restore worktree and branch so it cannot delete files created concurrently by another process",
        ),
    );
    cleanup
}

impl FailedRestoreCleanup {
    fn preserve_worktree(
        &mut self,
        target: &Path,
        branch: &str,
        stage: &str,
        error: &CommandError,
    ) {
        self.incomplete = true;
        self.preserved_worktree_path = Some(target.to_string_lossy().into_owned());
        self.preserved_branch_ref = Some(format!("refs/heads/{branch}"));
        self.diagnostics.push(cleanup_diagnostic(stage, error));
    }
}

fn cleanup_diagnostic(stage: &str, error: &CommandError) -> Value {
    json!({
        "code": error.code,
        "stage": stage,
        "message": error.message,
    })
}

fn with_failed_restore_cleanup(
    mut error: CommandError,
    cleanup: FailedRestoreCleanup,
) -> CommandError {
    if !cleanup.incomplete {
        return error;
    }
    let mut details = match error.details.take() {
        Some(Value::Object(details)) => details,
        Some(other) => {
            let mut details = Map::new();
            details.insert("restore_error_details".into(), other);
            details
        }
        None => Map::new(),
    };
    details.insert("cleanup_incomplete".into(), Value::Bool(true));
    if let Some(path) = cleanup.preserved_worktree_path {
        details.insert("preserved_worktree_path".into(), Value::String(path));
    }
    if let Some(reference) = cleanup.preserved_branch_ref {
        details.insert("preserved_branch_ref".into(), Value::String(reference));
    }
    details.insert(
        "cleanup_diagnostics".into(),
        Value::Array(cleanup.diagnostics),
    );
    error.details = Some(Value::Object(details));
    error
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use std::process::Command;

    fn default_request(output_path: &Path) -> ExportRelaypackRequest {
        ExportRelaypackRequest {
            agent: AgentProvider::Codex,
            session_id: "session-1".into(),
            preview_sha256: String::new(),
            output_path: output_path.to_string_lossy().into_owned(),
            claude_home: None,
            codex_home: None,
            repository_path: None,
            include_git: false,
            include_local_commits: None,
            include_staged: None,
            include_unstaged: None,
            selected_staged: Vec::new(),
            selected_unstaged: Vec::new(),
            selected_untracked: Vec::new(),
            excluded_message_ids: Vec::new(),
            excluded_blocks: Vec::new(),
            allow_sensitive_content: false,
            session_state: None,
            include_conversation: true,
            include_tool_evidence: true,
            include_project_instructions: true,
            include_environment: true,
        }
    }

    fn adapter_preview(messages: Vec<Value>, status: &str) -> Value {
        json!({
            "schema": ADAPTER_PREVIEW_SCHEMA,
            "exported_at": "2026-08-08T00:00:00Z",
            "source": {
                "agent": "codex",
                "session_id": "session-1",
                "source_path": "/private/agent/history/session-1.jsonl",
                "read_only": true
            },
            "session": {"title": "Continue Relay"},
            "environment": {"cwd": "/private/project"},
            "project": {"key": "project-key", "name": "Relay"},
            "conversation": {"messages": messages},
            "assets": [],
            "diagnostics": {
                "warnings": [],
                "completeness": {
                    "status": status,
                    "total_lines": 1,
                    "parsed_lines": 1,
                    "damaged_lines": 0,
                    "unknown_records": 0,
                    "hidden_records": 0,
                    "unsupported_blocks": 0,
                    "orphan_tool_results": 0,
                    "unmatched_tool_calls": 0
                }
            },
            "export": {
                "adapter_version": "0.1.0",
                "protocol": "relay.adapter.v1",
                "native_history": false
            }
        })
    }

    fn sample_messages() -> Vec<Value> {
        vec![json!({
            "id": "message-1",
            "role": "assistant",
            "blocks": [
                {
                    "kind": "text",
                    "classification": "user_visible",
                    "text": "Visible answer"
                },
                {
                    "kind": "tool_call",
                    "classification": "project_owned",
                    "call_id": "call-1",
                    "name": "exec_command",
                    "input": {"cmd": "cargo test"},
                    "status": "completed",
                    "replay_policy": "never"
                },
                {
                    "kind": "tool_result",
                    "classification": "project_owned",
                    "call_id": "call-1",
                    "output": "ok",
                    "status": "success",
                    "replay_policy": "never"
                },
                {
                    "kind": "source_context",
                    "classification": "project_owned",
                    "source": {
                        "filename": "AGENTS.md",
                        "snippet": "Project instructions"
                    }
                }
            ]
        })]
    }

    fn export_from_preview(
        mut request: ExportRelaypackRequest,
        preview: Value,
    ) -> ExportRelaypackResult {
        request.preview_sha256 = adapter::session_preview_sha256(&preview).unwrap();
        export_relaypack_with_preview(request, preview).unwrap()
    }

    #[test]
    fn changed_session_preview_stops_before_git_or_package_creation() {
        let directory = tempfile::tempdir().unwrap();
        let output_path = directory.path().join("changed-session.relaypack");
        let mut request = default_request(&output_path);
        request.preview_sha256 = "0".repeat(64);
        request.include_git = true;
        request.repository_path = Some(
            directory
                .path()
                .join("repository-that-does-not-exist")
                .to_string_lossy()
                .into_owned(),
        );

        let error =
            export_relaypack_with_preview(request, adapter_preview(sample_messages(), "complete"))
                .expect_err("a stale preview digest must stop export");

        assert_eq!(error.code, "session_preview_changed");
        assert!(!output_path.exists());
    }

    fn read_handoff(result: &ExportRelaypackResult) -> Value {
        let loaded = load_relaypack(&result.package_path, &result.key_fragment).unwrap();
        loaded.envelope.handoff
    }

    fn run_git(directory: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn git_succeeds(directory: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .status()
            .is_ok_and(|status| status.success())
    }

    fn init_repository(path: &Path) {
        fs::create_dir(path).unwrap();
        run_git(path, &["init"]);
        run_git(path, &["config", "user.name", "Relay Tests"]);
        run_git(path, &["config", "user.email", "relay@example.invalid"]);
    }

    fn branch_exists(repository: &Path, branch: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(repository)
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(unix)]
    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    #[cfg(unix)]
    fn write_executable_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[derive(Debug, PartialEq, Eq)]
    struct RepositorySnapshot {
        head: String,
        status: String,
        refs: String,
        worktrees: String,
    }

    fn repository_snapshot(repository: &Path) -> RepositorySnapshot {
        RepositorySnapshot {
            head: run_git(repository, &["rev-parse", "HEAD"]),
            status: run_git(
                repository,
                &["status", "--porcelain=v1", "--untracked-files=all"],
            ),
            refs: run_git(
                repository,
                &[
                    "for-each-ref",
                    "--format=%(refname):%(objectname)",
                    "refs/heads",
                ],
            ),
            worktrees: run_git(repository, &["worktree", "list", "--porcelain"]),
        }
    }

    fn test_envelope(result: &ExportRelaypackResult) -> PackageEnvelope {
        load_relaypack(&result.package_path, &result.key_fragment)
            .unwrap()
            .envelope
    }

    fn write_encrypted_test_bytes(path: &Path, compressed: &[u8]) -> String {
        let key = [0x5a_u8; KEY_LENGTH];
        let digest = Sha256::digest(path.to_string_lossy().as_bytes());
        let nonce = &digest[..NONCE_LENGTH];
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(nonce),
                AeadPayload {
                    msg: compressed,
                    aad: PACKAGE_MAGIC,
                },
            )
            .unwrap();
        let mut package = Vec::with_capacity(PACKAGE_MAGIC.len() + NONCE_LENGTH + ciphertext.len());
        package.extend_from_slice(PACKAGE_MAGIC);
        package.extend_from_slice(nonce);
        package.extend_from_slice(&ciphertext);
        fs::write(path, package).unwrap();
        URL_SAFE_NO_PAD.encode(key)
    }

    fn write_encrypted_test_envelope(path: &Path, envelope: &PackageEnvelope) -> String {
        let plaintext = serde_json::to_vec(envelope).unwrap();
        let compressed = zstd::stream::encode_all(Cursor::new(plaintext), 3).unwrap();
        write_encrypted_test_bytes(path, &compressed)
    }

    fn assert_package_rejected_before_worktree(
        package: &Path,
        key: &str,
        receiver: &Path,
        target: &Path,
        branch: &str,
        expected_code: &str,
    ) -> CommandError {
        let before = repository_snapshot(receiver);
        let error = restore_relaypack(RestoreRelaypackRequest {
            package_path: package.to_string_lossy().into_owned(),
            key: key.into(),
            repository_path: Some(receiver.to_string_lossy().into_owned()),
            target_path: target.to_string_lossy().into_owned(),
            branch_name: Some(branch.into()),
        })
        .unwrap_err();
        assert_eq!(error.code, expected_code, "unexpected error: {error:?}");
        assert!(!target.exists(), "malicious package created a worktree");
        assert!(!branch_exists(receiver, branch));
        assert_eq!(repository_snapshot(receiver), before);
        error
    }

    fn assert_envelope_rejected_before_worktree(
        directory: &Path,
        receiver: &Path,
        name: &str,
        envelope: &PackageEnvelope,
        expected_code: &str,
    ) -> CommandError {
        let package = directory.join(format!("{name}.relaypack"));
        let key = write_encrypted_test_envelope(&package, envelope);
        let target = directory.join(format!("{name}-worktree"));
        let branch = format!("relay/malicious-{name}");
        assert_package_rejected_before_worktree(
            &package,
            &key,
            receiver,
            &target,
            &branch,
            expected_code,
        )
    }

    fn git_handoff(head: &str, untracked_files: Vec<Value>) -> Value {
        json!({
            "included": true,
            "repository": {
                "object_format": if head.len() == 64 { "sha256" } else { "sha1" },
                "head": head
            },
            "capture": {
                "local_commits": {"status": "none"},
                "staged_patch": {"status": "none"},
                "unstaged_patch": {"status": "none"},
                "untracked_files": untracked_files,
                "ignored_files": {"status": "excluded"},
                "submodules": {"status": "not_present"},
                "lfs": {"status": "not_present", "objects_included": false}
            },
            "completeness": {"status": "complete", "notes": []}
        })
    }

    #[test]
    fn partial_tool_history_is_omitted_with_a_diagnostic() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("partial.relaypack");
        let messages = vec![json!({
            "id": "message-1",
            "role": "assistant",
            "blocks": [
                {"kind": "text", "classification": "user_visible", "text": "Before tool"},
                {
                    "kind": "tool_call",
                    "classification": "project_owned",
                    "call_id": "unfinished-call",
                    "name": "exec_command",
                    "input": {"cmd": "cargo test"},
                    "status": "running",
                    "replay_policy": "never"
                }
            ]
        })];
        let result = export_from_preview(
            default_request(&output),
            adapter_preview(messages, "partial"),
        );
        let handoff = read_handoff(&result);

        assert_eq!(
            handoff.pointer("/conversation/records/0/blocks/0/kind"),
            Some(&Value::String("text".into()))
        );
        assert_eq!(
            handoff
                .pointer("/conversation/records/0/blocks")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        assert!(handoff
            .get("diagnostics")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .any(|item| item.get("code").and_then(Value::as_str)
                == Some("UNPAIRED_TOOL_HISTORY_OMITTED")));
    }

    #[test]
    fn content_selection_rejects_unknown_duplicate_and_out_of_range_references() {
        let temp = tempfile::tempdir().unwrap();
        let cases = [
            (
                vec!["unknown-message".into()],
                Vec::new(),
                "unknown-message.relaypack",
            ),
            (
                vec!["message-1".into(), "message-1".into()],
                Vec::new(),
                "duplicate-message.relaypack",
            ),
            (
                Vec::new(),
                vec![crate::types::ExcludedContentBlock {
                    message_id: "unknown-message".into(),
                    block_index: 0,
                }],
                "unknown-block-message.relaypack",
            ),
            (
                Vec::new(),
                vec![crate::types::ExcludedContentBlock {
                    message_id: "message-1".into(),
                    block_index: 99,
                }],
                "out-of-range-block.relaypack",
            ),
            (
                Vec::new(),
                vec![
                    crate::types::ExcludedContentBlock {
                        message_id: "message-1".into(),
                        block_index: 0,
                    },
                    crate::types::ExcludedContentBlock {
                        message_id: "message-1".into(),
                        block_index: 0,
                    },
                ],
                "duplicate-block.relaypack",
            ),
        ];

        for (excluded_message_ids, excluded_blocks, filename) in cases {
            let output = temp.path().join(filename);
            let mut request = default_request(&output);
            request.excluded_message_ids = excluded_message_ids;
            request.excluded_blocks = excluded_blocks;
            let validated_path = validate_new_relaypack_path(&request.output_path).unwrap();
            let error = export_relaypack_from_preview(
                request,
                adapter_preview(sample_messages(), "complete"),
                validated_path,
            )
            .unwrap_err();
            assert_eq!(error.code, "invalid_content_selection");
            assert!(!output.exists());
        }
    }

    #[test]
    fn content_selection_removes_blocks_and_requires_complete_tool_pairs() {
        let temp = tempfile::tempdir().unwrap();
        let mismatched_output = temp.path().join("mismatched-tool.relaypack");
        let mut mismatched = default_request(&mismatched_output);
        mismatched.excluded_blocks = vec![crate::types::ExcludedContentBlock {
            message_id: "message-1".into(),
            block_index: 1,
        }];
        let output_path = validate_new_relaypack_path(&mismatched.output_path).unwrap();
        let error = export_relaypack_from_preview(
            mismatched,
            adapter_preview(sample_messages(), "complete"),
            output_path,
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid_content_selection");
        assert!(!mismatched_output.exists());

        let selected_output = temp.path().join("selected-blocks.relaypack");
        let mut selected = default_request(&selected_output);
        selected.excluded_blocks = vec![
            crate::types::ExcludedContentBlock {
                message_id: "message-1".into(),
                block_index: 0,
            },
            crate::types::ExcludedContentBlock {
                message_id: "message-1".into(),
                block_index: 1,
            },
            crate::types::ExcludedContentBlock {
                message_id: "message-1".into(),
                block_index: 2,
            },
        ];
        let result = export_from_preview(selected, adapter_preview(sample_messages(), "complete"));
        let handoff = read_handoff(&result);
        let blocks = handoff
            .pointer("/conversation/records/0/blocks")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].get("kind").and_then(Value::as_str),
            Some("source_context")
        );
        assert_eq!(
            handoff.pointer("/export/mode").and_then(Value::as_str),
            Some("selected")
        );
        let redacted_count: u64 = handoff
            .pointer("/export/omissions")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter(|omission| {
                omission.get("reason").and_then(Value::as_str) == Some("redacted_by_user")
            })
            .filter_map(|omission| omission.get("count").and_then(Value::as_u64))
            .sum();
        assert_eq!(redacted_count, 3);
    }

    #[test]
    fn privacy_switches_remove_selected_sections_and_report_omissions() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("privacy.relaypack");
        let mut request = default_request(&output);
        request.include_conversation = false;
        request.include_tool_evidence = false;
        request.include_project_instructions = false;
        request.include_environment = false;
        let result = export_from_preview(request, adapter_preview(sample_messages(), "complete"));
        let handoff = read_handoff(&result);

        assert!(handoff
            .pointer("/conversation/records")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty));
        assert_eq!(
            handoff.pointer("/environment/os").and_then(Value::as_str),
            Some("unknown")
        );
        assert_eq!(
            handoff.pointer("/export/mode").and_then(Value::as_str),
            Some("selected")
        );
        assert_eq!(
            handoff
                .pointer("/export/completeness/source_records")
                .and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(
            handoff
                .pointer("/export/completeness/exported_records")
                .and_then(Value::as_u64),
            Some(0)
        );
        assert_eq!(
            handoff
                .pointer("/export/completeness/omitted_records")
                .and_then(Value::as_u64),
            Some(1)
        );
        let redacted_count: u64 = handoff
            .pointer("/export/omissions")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter(|omission| {
                omission.get("reason").and_then(Value::as_str) == Some("redacted_by_user")
            })
            .filter_map(|omission| omission.get("count").and_then(Value::as_u64))
            .sum();
        assert_eq!(redacted_count, 5);
        let diagnostics = handoff
            .get("diagnostics")
            .and_then(Value::as_array)
            .unwrap();
        for code in [
            "CONVERSATION_REDACTED_BY_USER",
            "TOOL_EVIDENCE_REDACTED_BY_USER",
            "PROJECT_INSTRUCTIONS_REDACTED_BY_USER",
            "ENVIRONMENT_REDACTED_BY_USER",
        ] {
            assert!(diagnostics
                .iter()
                .any(|item| item.get("code").and_then(Value::as_str) == Some(code)));
        }
    }

    #[test]
    fn sensitive_content_requires_confirmation_and_reports_only_safe_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let token = "sk-abcdefghijklmnopqrstuvwxyz1234567890";
        let connection = "postgres://relay:private-password@example.test/relay";
        let mut messages = sample_messages();
        messages[0]["blocks"][0]["text"] = Value::String(format!("Use {token}"));

        let blocked_output = temp.path().join("sensitive-blocked.relaypack");
        let mut blocked = default_request(&blocked_output);
        blocked.session_state = Some(SessionStateInput {
            summary: Some(connection.into()),
            ..SessionStateInput::default()
        });
        let output_path = validate_new_relaypack_path(&blocked.output_path).unwrap();
        let error = export_relaypack_from_preview(
            blocked,
            adapter_preview(messages.clone(), "complete"),
            output_path,
        )
        .unwrap_err();
        assert_eq!(error.code, "sensitive_content_confirmation_required");
        assert!(!blocked_output.exists());
        let rendered_error = serde_json::to_string(&error).unwrap();
        assert!(!rendered_error.contains(token));
        assert!(!rendered_error.contains("private-password"));
        let findings = error
            .details
            .as_ref()
            .and_then(|details| details.get("findings"))
            .and_then(Value::as_array)
            .unwrap();
        assert!(findings
            .iter()
            .any(|finding| finding.get("scope").and_then(Value::as_str) == Some("conversation")));
        assert!(findings
            .iter()
            .any(|finding| finding.get("scope").and_then(Value::as_str) == Some("session_state")));
        assert!(findings.iter().all(|finding| {
            finding.as_object().is_some_and(|object| {
                object.len() == 4
                    && object.contains_key("code")
                    && object.contains_key("label")
                    && object.contains_key("scope")
                    && object.contains_key("count")
            })
        }));

        let allowed_output = temp.path().join("sensitive-allowed.relaypack");
        let mut allowed = default_request(&allowed_output);
        allowed.allow_sensitive_content = true;
        allowed.session_state = Some(SessionStateInput {
            summary: Some(connection.into()),
            ..SessionStateInput::default()
        });
        let result = export_from_preview(allowed, adapter_preview(messages, "complete"));
        assert!(result.warnings.iter().any(|warning| {
            warning.code.starts_with("SENSITIVE_CONTENT_INCLUDED_") && warning.scope == "security"
        }));
        let rendered_warnings = serde_json::to_string(&result.warnings).unwrap();
        assert!(!rendered_warnings.contains(token));
        assert!(!rendered_warnings.contains("private-password"));
    }

    #[test]
    fn export_mode_is_full_only_when_every_share_option_is_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("full.relaypack");
        let mut request = default_request(&output);
        request.include_git = true;
        let capture = GitCapture {
            included: true,
            object_format: "sha1".into(),
            local_commits_status: "none".into(),
            staged_status: "none".into(),
            unstaged_status: "none".into(),
            ..GitCapture::default()
        };
        let preview = adapter_preview(sample_messages(), "complete");
        let handoff = build_handoff(
            &preview,
            &request,
            &capture,
            "pkg.full-selection-test",
            "2026-08-08T00:00:00Z",
        )
        .unwrap();
        assert_eq!(
            handoff.pointer("/export/mode").and_then(Value::as_str),
            Some("full")
        );

        request.include_staged = Some(false);
        let handoff = build_handoff(
            &preview,
            &request,
            &capture,
            "pkg.selected-test",
            "2026-08-08T00:00:00Z",
        )
        .unwrap();
        assert_eq!(
            handoff.pointer("/export/mode").and_then(Value::as_str),
            Some("selected")
        );
        let git_omission = handoff
            .pointer("/export/omissions")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .find(|omission| omission.get("reason").and_then(Value::as_str) == Some("git_excluded"))
            .unwrap();
        assert_eq!(git_omission.get("count").and_then(Value::as_u64), Some(1));

        request.include_git = false;
        request.include_staged = Some(true);
        let capture = GitCapture {
            local_commits_status: "omitted".into(),
            staged_status: "omitted".into(),
            unstaged_status: "omitted".into(),
            ..GitCapture::default()
        };
        let handoff = build_handoff(
            &preview,
            &request,
            &capture,
            "pkg.no-git-test",
            "2026-08-08T00:00:00Z",
        )
        .unwrap();
        assert_eq!(
            handoff.pointer("/export/mode").and_then(Value::as_str),
            Some("selected")
        );
    }

    #[test]
    fn handoff_markdown_renders_tests_without_markdown_or_path_injection() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("tests-markdown.relaypack");
        let mut request = default_request(&output);
        request.session_state = Some(SessionStateInput {
            tests: vec![crate::types::SessionTestInput {
                name: "unit test\n## injected [link](javascript:alert(1))".into(),
                command: Some("cargo test /Users/alice/private\n```\n# injected".into()),
                status: Some("passed".into()),
                note: Some("<script>alert(1)</script> from /tmp/relay-secret".into()),
            }],
            ..SessionStateInput::default()
        });
        let result = export_from_preview(request, adapter_preview(sample_messages(), "complete"));
        let loaded = load_relaypack(&result.package_path, &result.key_fragment).unwrap();
        let markdown =
            String::from_utf8(handoff_markdown_payload(&loaded.envelope).unwrap()).unwrap();

        assert!(markdown.contains("## Tests"));
        assert!(markdown.contains("- Name: unit test"));
        assert!(markdown.contains("- Status: passed"));
        assert!(markdown.contains("- Command: cargo test"));
        assert!(markdown.contains("- Note:"));
        assert!(!markdown.contains("\n## injected"));
        assert!(!markdown.contains("javascript:alert(1)"));
        assert!(!markdown.contains("/Users/"));
        assert!(!markdown.contains("/tmp/"));
        assert!(!markdown.contains("<script>"));
        assert!(!markdown.contains("```"));
    }

    #[test]
    fn conversation_switch_keeps_separately_selected_tool_and_instruction_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("without-conversation.relaypack");
        let mut request = default_request(&output);
        request.include_conversation = false;
        let result = export_from_preview(request, adapter_preview(sample_messages(), "complete"));
        let handoff = read_handoff(&result);
        let kinds: Vec<&str> = handoff
            .pointer("/conversation/records/0/blocks")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|block| block.get("kind").and_then(Value::as_str))
            .collect();
        assert_eq!(kinds, ["tool_call", "tool_result", "source_context"]);
        assert!(!serde_json::to_string(&handoff)
            .unwrap()
            .contains("Visible answer"));

        let output = temp.path().join("conversation-only.relaypack");
        let mut request = default_request(&output);
        request.include_tool_evidence = false;
        request.include_project_instructions = false;
        let result = export_from_preview(request, adapter_preview(sample_messages(), "complete"));
        let handoff = read_handoff(&result);
        let kinds: Vec<&str> = handoff
            .pointer("/conversation/records/0/blocks")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|block| block.get("kind").and_then(Value::as_str))
            .collect();
        assert_eq!(kinds, ["text"]);
    }

    #[test]
    fn wrong_key_and_ciphertext_change_fail_authentication() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("auth.relaypack");
        let result = export_from_preview(
            default_request(&output),
            adapter_preview(sample_messages(), "complete"),
        );
        assert_eq!(
            inspect_relaypack(&result.package_path, "not-a-relay-key")
                .unwrap_err()
                .code,
            "relaypack_key_invalid"
        );
        let wrong_key = URL_SAFE_NO_PAD.encode([7_u8; KEY_LENGTH]);
        assert_eq!(
            inspect_relaypack(&result.package_path, &wrong_key)
                .unwrap_err()
                .code,
            "relaypack_auth_failed"
        );

        let mut bytes = fs::read(&result.package_path).unwrap();
        let last = bytes.last_mut().unwrap();
        *last ^= 1;
        fs::write(&result.package_path, bytes).unwrap();
        assert_eq!(
            inspect_relaypack(&result.package_path, &result.key_fragment)
                .unwrap_err()
                .code,
            "relaypack_auth_failed"
        );
    }

    #[test]
    fn export_does_not_overwrite_existing_package() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("existing.relaypack");
        fs::write(&output, b"keep-me").unwrap();
        let error = validate_new_relaypack_path(output.to_str().unwrap()).unwrap_err();
        assert_eq!(error.code, "output_exists");
        assert_eq!(fs::read(output).unwrap(), b"keep-me");
    }

    #[test]
    fn unsafe_archive_and_untracked_paths_are_rejected() {
        assert!(validate_archive_path("../escape").is_err());
        assert!(validate_repo_uri("repo://../escape").is_err());
        assert!(validate_repo_uri("repo://.git/config").is_err());
    }

    #[test]
    fn encrypted_malicious_handoff_relations_are_rejected_before_worktree_creation() {
        let temp = tempfile::tempdir().unwrap();
        let receiver = temp.path().join("receiver");
        init_repository(&receiver);
        fs::write(receiver.join("base.txt"), b"base\n").unwrap();
        run_git(&receiver, &["add", "base.txt"]);
        run_git(&receiver, &["commit", "-m", "base"]);

        let valid_path = temp.path().join("valid.relaypack");
        let result = export_from_preview(
            default_request(&valid_path),
            adapter_preview(sample_messages(), "complete"),
        );
        let baseline = test_envelope(&result);

        let mut missing_asset_ref = baseline.clone();
        missing_asset_ref
            .handoff
            .pointer_mut("/conversation/records/0/blocks")
            .and_then(Value::as_array_mut)
            .unwrap()
            .push(json!({
                "id": "block.missing-asset",
                "kind": "asset_ref",
                "classification": "user_visible",
                "mapping": {"status": "normalized"},
                "asset_id": "asset.does-not-exist"
            }));
        assert_envelope_rejected_before_worktree(
            temp.path(),
            &receiver,
            "missing-asset-ref",
            &missing_asset_ref,
            "relaypack_invalid",
        );

        let mut digest_mismatch = baseline.clone();
        digest_mismatch.handoff["assets"][0]["sha256"] = Value::String("0".repeat(64));
        assert_envelope_rejected_before_worktree(
            temp.path(),
            &receiver,
            "asset-digest-mismatch",
            &digest_mismatch,
            "relaypack_invalid",
        );

        let mut duplicate_record = baseline.clone();
        let record = duplicate_record.handoff["conversation"]["records"][0].clone();
        duplicate_record.handoff["conversation"]["records"]
            .as_array_mut()
            .unwrap()
            .push(record);
        duplicate_record.handoff["export"]["completeness"]["source_records"] = json!(2);
        duplicate_record.handoff["export"]["completeness"]["exported_records"] = json!(2);
        assert_envelope_rejected_before_worktree(
            temp.path(),
            &receiver,
            "duplicate-record-id",
            &duplicate_record,
            "relaypack_invalid",
        );

        let mut duplicate_block = baseline.clone();
        let block = duplicate_block.handoff["conversation"]["records"][0]["blocks"][0].clone();
        duplicate_block.handoff["conversation"]["records"][0]["blocks"]
            .as_array_mut()
            .unwrap()
            .push(block);
        assert_envelope_rejected_before_worktree(
            temp.path(),
            &receiver,
            "duplicate-block-id",
            &duplicate_block,
            "relaypack_invalid",
        );

        let mut duplicate_asset = baseline.clone();
        let asset = duplicate_asset.handoff["assets"][0].clone();
        duplicate_asset.handoff["assets"]
            .as_array_mut()
            .unwrap()
            .push(asset);
        assert_envelope_rejected_before_worktree(
            temp.path(),
            &receiver,
            "duplicate-asset-id",
            &duplicate_asset,
            "relaypack_invalid",
        );

        let mut invalid_branch = baseline.clone();
        invalid_branch.handoff["conversation"]["records"][0]["branch_id"] = json!("branch.present");
        invalid_branch.handoff["conversation"]["active_branch_id"] = json!("branch.absent");
        assert_envelope_rejected_before_worktree(
            temp.path(),
            &receiver,
            "invalid-branch-ref",
            &invalid_branch,
            "relaypack_invalid",
        );

        let mut invalid_line_range = baseline.clone();
        let source_context = invalid_line_range
            .handoff
            .pointer_mut("/conversation/records/0/blocks")
            .and_then(Value::as_array_mut)
            .unwrap()
            .iter_mut()
            .find(|block| block.get("kind").and_then(Value::as_str) == Some("source_context"))
            .unwrap();
        source_context["line_range"] = json!({"start": 9, "end": 4});
        assert_envelope_rejected_before_worktree(
            temp.path(),
            &receiver,
            "invalid-line-range",
            &invalid_line_range,
            "relaypack_invalid",
        );

        let mut invalid_statistics = baseline.clone();
        invalid_statistics.handoff["export"]["completeness"]["exported_records"] = json!(9);
        assert_envelope_rejected_before_worktree(
            temp.path(),
            &receiver,
            "invalid-statistics",
            &invalid_statistics,
            "relaypack_invalid",
        );

        for (name, paths) in [
            (
                "case-path-conflict",
                vec![json!("repo://Readme.md"), json!("repo://README.md")],
            ),
            (
                "unicode-path-conflict",
                vec![json!("repo://café.txt"), json!("repo://café.txt")],
            ),
        ] {
            let mut conflicting_paths = baseline.clone();
            conflicting_paths.handoff["session_state"]["important_files"] = Value::Array(paths);
            assert_envelope_rejected_before_worktree(
                temp.path(),
                &receiver,
                name,
                &conflicting_paths,
                "relaypack_invalid",
            );
        }
    }

    #[test]
    fn valid_branch_line_range_and_missing_asset_reference_remain_supported() {
        let temp = tempfile::tempdir().unwrap();
        let valid_path = temp.path().join("valid.relaypack");
        let result = export_from_preview(
            default_request(&valid_path),
            adapter_preview(sample_messages(), "complete"),
        );
        let mut envelope = test_envelope(&result);
        envelope.handoff["assets"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "id": "asset.missing-attachment",
                "kind": "other",
                "classification": "user_visible",
                "status": "missing",
                "execution_policy": "never",
                "note": "The source attachment was not available."
            }));
        envelope.handoff["conversation"]["records"][0]["branch_id"] = json!("branch.current");
        envelope.handoff["conversation"]["active_branch_id"] = json!("branch.current");
        let blocks = envelope.handoff["conversation"]["records"][0]["blocks"]
            .as_array_mut()
            .unwrap();
        blocks
            .iter_mut()
            .find(|block| block.get("kind").and_then(Value::as_str) == Some("source_context"))
            .unwrap()["line_range"] = json!({"start": 1, "end": 3});
        blocks.push(json!({
            "id": "block.missing-attachment",
            "kind": "asset_ref",
            "classification": "user_visible",
            "mapping": {"status": "normalized"},
            "asset_id": "asset.missing-attachment",
            "caption": "Attachment was unavailable at export time."
        }));
        let package = temp.path().join("valid-relations.relaypack");
        let key = write_encrypted_test_envelope(&package, &envelope);
        inspect_relaypack(package.to_str().unwrap(), &key).unwrap();
    }

    #[test]
    fn encrypted_malicious_git_material_and_limits_are_rejected_before_worktree_creation() {
        let temp = tempfile::tempdir().unwrap();
        let receiver = temp.path().join("receiver");
        init_repository(&receiver);
        fs::write(receiver.join("base.txt"), b"base\n").unwrap();
        run_git(&receiver, &["add", "base.txt"]);
        run_git(&receiver, &["commit", "-m", "base"]);
        let head = run_git(&receiver, &["rev-parse", "HEAD"]);

        let valid_path = temp.path().join("valid.relaypack");
        let result = export_from_preview(
            default_request(&valid_path),
            adapter_preview(sample_messages(), "complete"),
        );
        let baseline = test_envelope(&result);

        let mut special_file = baseline.clone();
        let special_payload = PackagePayload::from_bytes(
            "asset.special-file",
            "payload/git/untracked/link.txt",
            "untracked_file",
            Some("120000".into()),
            b"target.txt".to_vec(),
        );
        special_file.handoff["assets"]
            .as_array_mut()
            .unwrap()
            .push(asset_manifest(
                &special_payload,
                "untracked_file",
                Some("repo://link.txt"),
                None,
            ));
        special_file.payloads.push(special_payload);
        special_file.handoff["git"] = git_handoff(
            &head,
            vec![json!({
                "logical_path": "repo://link.txt",
                "asset_id": "asset.special-file",
                "mode": "120000"
            })],
        );
        assert_envelope_rejected_before_worktree(
            temp.path(),
            &receiver,
            "special-file-mode",
            &special_file,
            "relaypack_invalid",
        );

        let mut git_metadata_patch = baseline.clone();
        let patch_payload = PackagePayload::from_bytes(
            "asset.git-metadata-patch",
            "payload/git/staged.patch",
            "git_patch",
            Some("100444".into()),
            b"diff --git a/.git/config b/.git/config\n--- a/.git/config\n+++ b/.git/config\n@@ -0,0 +1 @@\n+malicious=true\n".to_vec(),
        );
        git_metadata_patch.handoff["assets"]
            .as_array_mut()
            .unwrap()
            .push(asset_manifest(&patch_payload, "git_patch", None, None));
        git_metadata_patch.payloads.push(patch_payload);
        git_metadata_patch.handoff["git"] = git_handoff(&head, Vec::new());
        git_metadata_patch.handoff["git"]["capture"]["staged_patch"] = json!({
            "status": "included",
            "asset_id": "asset.git-metadata-patch"
        });
        assert_envelope_rejected_before_worktree(
            temp.path(),
            &receiver,
            "git-metadata-patch",
            &git_metadata_patch,
            "unsafe_patch",
        );

        let mut too_many_files = baseline.clone();
        let untracked = (0..=MAX_UNTRACKED_FILES)
            .map(|index| {
                json!({
                    "logical_path": format!("repo://generated/file-{index}.txt"),
                    "asset_id": format!("asset.generated-{index}"),
                    "mode": "100644"
                })
            })
            .collect();
        too_many_files.handoff["git"] = git_handoff(&head, untracked);
        assert_envelope_rejected_before_worktree(
            temp.path(),
            &receiver,
            "untracked-count-limit",
            &too_many_files,
            "relaypack_invalid",
        );

        let mut oversized_payload = baseline.clone();
        let declared_size = MAX_PAYLOAD_BYTES as u64 + 1;
        oversized_payload.payloads[0].byte_length = declared_size;
        oversized_payload.handoff["assets"][0]["byte_length"] = json!(declared_size);
        assert_envelope_rejected_before_worktree(
            temp.path(),
            &receiver,
            "payload-size-limit",
            &oversized_payload,
            "relaypack_too_large",
        );
    }

    #[test]
    fn authenticated_compression_bomb_is_rejected_before_worktree_creation() {
        let temp = tempfile::tempdir().unwrap();
        let receiver = temp.path().join("receiver");
        init_repository(&receiver);
        fs::write(receiver.join("base.txt"), b"base\n").unwrap();
        run_git(&receiver, &["add", "base.txt"]);
        run_git(&receiver, &["commit", "-m", "base"]);

        let plaintext = vec![b' '; MAX_PLAINTEXT_BYTES + 1];
        let compressed = zstd::stream::encode_all(Cursor::new(plaintext), 3).unwrap();
        let package = temp.path().join("compression-bomb.relaypack");
        let key = write_encrypted_test_bytes(&package, &compressed);
        assert_package_rejected_before_worktree(
            &package,
            &key,
            &receiver,
            &temp.path().join("compression-bomb-worktree"),
            "relay/malicious-compression-bomb",
            "relaypack_too_large",
        );
    }

    #[test]
    fn parent_cycles_and_invalid_roots_are_rejected() {
        let cyclic = HashMap::from([
            ("record.a".into(), Some("record.b".into())),
            ("record.b".into(), Some("record.a".into())),
        ]);
        let handoff = json!({"conversation": {"root_record_ids": []}});
        assert_eq!(
            validate_record_roots_and_cycles(&handoff, &cyclic)
                .unwrap_err()
                .code,
            "relaypack_invalid"
        );

        let roots = HashMap::from([("record.a".into(), None)]);
        let handoff = json!({"conversation": {"root_record_ids": ["record.missing"]}});
        assert_eq!(
            validate_record_roots_and_cycles(&handoff, &roots)
                .unwrap_err()
                .code,
            "relaypack_invalid"
        );
    }

    #[test]
    fn external_git_filter_is_detected() {
        assert!(attributes_define_filter(b"*.dat filter=evil\n"));
        assert!(!attributes_define_filter(b"*.bin filter=lfs -text\n"));
        assert!(!attributes_define_filter(b"*.txt text eol=lf\n"));

        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        init_repository(&repository);
        let info = repository.join(".git/info");
        fs::create_dir_all(&info).unwrap();
        fs::write(info.join("attributes"), b"*.dat filter=evil\n").unwrap();
        assert_eq!(
            ensure_receiver_attributes_safe(&repository)
                .unwrap_err()
                .code,
            "git_filter_blocked"
        );
    }

    #[test]
    fn unused_lfs_rules_are_recorded_without_blocking_export() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        init_repository(&repository);
        fs::write(
            repository.join(".gitattributes"),
            b"*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        fs::write(repository.join("base.txt"), b"base\n").unwrap();
        run_git(&repository, &["add", ".gitattributes", "base.txt"]);
        run_git(&repository, &["commit", "-m", "base"]);

        let package = temp.path().join("lfs-rules-only.relaypack");
        let mut request = default_request(&package);
        request.include_git = true;
        request.include_local_commits = Some(false);
        request.repository_path = Some(repository.to_string_lossy().into_owned());
        let result = export_from_preview(request, adapter_preview(sample_messages(), "complete"));
        let handoff = read_handoff(&result);
        assert_eq!(
            handoff
                .pointer("/git/capture/lfs/status")
                .and_then(Value::as_str),
            Some("resolved")
        );
        assert_eq!(
            handoff
                .pointer("/git/capture/lfs/objects_included")
                .and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn a_head_path_using_lfs_is_blocked_before_package_creation() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        init_repository(&repository);
        fs::write(
            repository.join(".gitattributes"),
            b"*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        fs::write(
            repository.join("asset.bin"),
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 1\n",
                "0".repeat(64)
            ),
        )
        .unwrap();
        let safe_lfs = [
            "-c",
            "filter.lfs.clean=",
            "-c",
            "filter.lfs.smudge=",
            "-c",
            "filter.lfs.process=",
            "-c",
            "filter.lfs.required=false",
        ];
        let mut add = safe_lfs.to_vec();
        add.extend(["add", ".gitattributes", "asset.bin"]);
        run_git(&repository, &add);
        let mut commit = safe_lfs.to_vec();
        commit.extend(["commit", "-m", "lfs pointer"]);
        run_git(&repository, &commit);

        let package = temp.path().join("lfs-blocked.relaypack");
        let mut request = default_request(&package);
        request.include_git = true;
        request.repository_path = Some(repository.to_string_lossy().into_owned());
        let output_path = validate_new_relaypack_path(&request.output_path).unwrap();
        let error = export_relaypack_from_preview(
            request,
            adapter_preview(sample_messages(), "complete"),
            output_path,
        )
        .unwrap_err();
        assert!(matches!(
            error.code.as_str(),
            "lfs_unavailable" | "lfs_object_missing"
        ));
        assert!(!package.exists());
        let serialized = serde_json::to_string(&error).unwrap();
        assert!(!serialized.contains("asset.bin"));
        assert!(!serialized.contains("version https://"));
    }

    #[test]
    fn handoff_is_inside_linked_worktree_and_only_its_exact_root_is_excluded() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        init_repository(&repository);
        fs::write(repository.join("base.txt"), b"base\n").unwrap();
        run_git(&repository, &["add", "base.txt"]);
        run_git(&repository, &["commit", "-m", "base"]);

        let linked = temp.path().join("linked");
        run_git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "relay/handoff-test",
                linked.to_str().unwrap(),
            ],
        );
        let linked = fs::canonicalize(linked).unwrap();
        let exclude_path = repository.join(".git/info/exclude");
        let original_exclude = b"# existing content\nlocal-only\nno-final-newline".to_vec();
        fs::write(&exclude_path, &original_exclude).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&exclude_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let status_before = run_git(
            &linked,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        );

        let id = "11".repeat(HANDOFF_ID_BYTES);
        let mut ids = [id.clone()].into_iter();
        let installation = install_handoff_directory_with_id_source(
            &linked,
            b"# handoff\n",
            &json!({"schema": HANDOFF_SCHEMA}),
            || Ok(ids.next().unwrap()),
        )
        .unwrap();

        let directory = fs::canonicalize(&installation.directory.path).unwrap();
        assert_eq!(directory.parent(), Some(linked.as_path()));
        let directory_name = directory.file_name().unwrap().to_str().unwrap();
        assert_eq!(directory_name, format!(".relay-handoff-{id}"));
        validate_handoff_id(directory_name.trim_start_matches(".relay-handoff-")).unwrap();
        assert_eq!(
            git_info_exclude_path(&linked).unwrap(),
            fs::canonicalize(&exclude_path).unwrap()
        );
        assert_eq!(
            run_git(
                &linked,
                &["status", "--porcelain=v1", "--untracked-files=all"]
            ),
            status_before
        );

        let mut expected_exclude = original_exclude.clone();
        expected_exclude.extend_from_slice(format!("\n/{directory_name}/\n").as_bytes());
        assert_eq!(fs::read(&exclude_path).unwrap(), expected_exclude);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&exclude_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let other_name = format!(".relay-handoff-{}", "22".repeat(HANDOFF_ID_BYTES));
        let other = linked.join(&other_name);
        fs::create_dir(&other).unwrap();
        fs::write(other.join("user.txt"), b"not ignored\n").unwrap();
        assert!(!git_succeeds(
            &linked,
            &[
                "check-ignore",
                "--quiet",
                "--no-index",
                "--",
                &format!("{other_name}/user.txt"),
            ],
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(directory.join("user-owned.txt"), b"keep\n").unwrap();
        let expected_oid = run_git(&linked, &["rev-parse", "HEAD"]);
        let cleanup = cleanup_failed_restore(
            &fs::canonicalize(&repository).unwrap(),
            &linked,
            "relay/handoff-test",
            Some(&expected_oid),
            Some(&installation),
        );
        assert!(cleanup.incomplete);
        assert_eq!(fs::read(&exclude_path).unwrap(), original_exclude);
        assert_eq!(
            fs::read(directory.join("user-owned.txt")).unwrap(),
            b"keep\n"
        );
        assert!(linked.is_dir());
        assert!(branch_exists(&repository, "relay/handoff-test"));
        assert!(!directory.join("HANDOFF.md").exists());
        assert!(!directory.join("handoff.json").exists());
    }

    #[test]
    fn failed_restore_cleanup_preserves_even_a_clean_worktree_and_branch() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        init_repository(&repository);
        fs::write(repository.join("base.txt"), b"base\n").unwrap();
        run_git(&repository, &["add", "base.txt"]);
        run_git(&repository, &["commit", "-m", "base"]);

        let target = temp.path().join("linked");
        let branch = "relay/cleanup-clean";
        run_git(
            &repository,
            &["worktree", "add", "-b", branch, target.to_str().unwrap()],
        );
        let target = fs::canonicalize(target).unwrap();
        let expected_oid = run_git(&target, &["rev-parse", "HEAD"]);

        let cleanup = cleanup_failed_restore(
            &fs::canonicalize(&repository).unwrap(),
            &target,
            branch,
            Some(&expected_oid),
            None,
        );

        assert!(cleanup.incomplete);
        assert!(target.is_dir());
        assert!(branch_exists(&repository, branch));
    }

    #[test]
    fn failed_restore_cleanup_preserves_ignored_content_and_its_branch() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        init_repository(&repository);
        fs::write(repository.join("base.txt"), b"base\n").unwrap();
        run_git(&repository, &["add", "base.txt"]);
        run_git(&repository, &["commit", "-m", "base"]);

        let target = temp.path().join("linked");
        let branch = "relay/cleanup-preserve";
        run_git(
            &repository,
            &["worktree", "add", "-b", branch, target.to_str().unwrap()],
        );
        let target = fs::canonicalize(target).unwrap();
        let expected_oid = run_git(&target, &["rev-parse", "HEAD"]);
        fs::write(repository.join(".git/info/exclude"), b"/ignored.txt\n").unwrap();
        fs::write(target.join("ignored.txt"), b"keep\n").unwrap();

        let cleanup = cleanup_failed_restore(
            &fs::canonicalize(&repository).unwrap(),
            &target,
            branch,
            Some(&expected_oid),
            None,
        );

        assert!(cleanup.incomplete);
        assert_eq!(fs::read(target.join("ignored.txt")).unwrap(), b"keep\n");
        assert!(branch_exists(&repository, branch));
        let target_text = target.to_string_lossy().into_owned();
        assert_eq!(
            cleanup.preserved_worktree_path.as_deref(),
            Some(target_text.as_str())
        );
        let error = with_failed_restore_cleanup(
            CommandError::new("restore_write_failed", "original restore failure"),
            cleanup,
        );
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|details| details.get("cleanup_incomplete"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|details| details.get("preserved_worktree_path"))
                .and_then(Value::as_str),
            Some(target_text.as_str())
        );
    }

    #[test]
    fn failed_restore_cleanup_never_deletes_a_replaced_branch() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        init_repository(&repository);
        fs::write(repository.join("base.txt"), b"base\n").unwrap();
        run_git(&repository, &["add", "base.txt"]);
        run_git(&repository, &["commit", "-m", "base"]);
        let expected_oid = run_git(&repository, &["rev-parse", "HEAD"]);
        let branch = "relay/cleanup-cas";
        let target = temp.path().join("linked");
        run_git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                target.to_str().unwrap(),
                &expected_oid,
            ],
        );
        let target = fs::canonicalize(target).unwrap();
        fs::write(repository.join("base.txt"), b"new\n").unwrap();
        run_git(&repository, &["commit", "-am", "new"]);
        let replacement_oid = run_git(&repository, &["rev-parse", "HEAD"]);
        run_git(
            &repository,
            &[
                "update-ref",
                &format!("refs/heads/{branch}"),
                &replacement_oid,
                &expected_oid,
            ],
        );

        let cleanup = cleanup_failed_restore(
            &fs::canonicalize(&repository).unwrap(),
            &target,
            branch,
            Some(&expected_oid),
            None,
        );

        assert!(cleanup.incomplete);
        assert!(target.is_dir());
        assert!(branch_exists(&repository, branch));
        assert_eq!(
            run_git(&repository, &["rev-parse", &format!("refs/heads/{branch}")]),
            replacement_oid
        );
    }

    #[test]
    fn failed_restore_cleanup_stops_when_git_exclude_rollback_detects_a_change() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        init_repository(&repository);
        fs::write(repository.join("base.txt"), b"base\n").unwrap();
        run_git(&repository, &["add", "base.txt"]);
        run_git(&repository, &["commit", "-m", "base"]);

        let target = temp.path().join("linked");
        let branch = "relay/cleanup-rollback";
        run_git(
            &repository,
            &["worktree", "add", "-b", branch, target.to_str().unwrap()],
        );
        let target = fs::canonicalize(target).unwrap();
        let installation =
            install_handoff_directory(&target, b"handoff\n", &json!({"schema": HANDOFF_SCHEMA}))
                .unwrap();
        let expected_oid = run_git(&target, &["rev-parse", "HEAD"]);
        let exclude_path = repository.join(".git/info/exclude");
        let mut concurrent = fs::read(&exclude_path).unwrap();
        concurrent.extend_from_slice(b"# concurrent user edit\n");
        fs::write(&exclude_path, &concurrent).unwrap();

        let cleanup = cleanup_failed_restore(
            &fs::canonicalize(&repository).unwrap(),
            &target,
            branch,
            Some(&expected_oid),
            Some(&installation),
        );

        assert!(cleanup.incomplete);
        assert!(target.is_dir());
        assert!(branch_exists(&repository, branch));
        assert_eq!(fs::read(&exclude_path).unwrap(), concurrent);
        assert!(installation.directory.path.join("HANDOFF.md").exists());
        assert_eq!(
            cleanup.diagnostics[0].get("stage").and_then(Value::as_str),
            Some("git_exclude_rollback")
        );
    }

    #[test]
    fn handoff_directory_retries_head_index_and_disk_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        init_repository(&repository);
        let head_id = "33".repeat(HANDOFF_ID_BYTES);
        let index_id = "44".repeat(HANDOFF_ID_BYTES);
        let disk_id = "55".repeat(HANDOFF_ID_BYTES);
        let free_id = "66".repeat(HANDOFF_ID_BYTES);
        let head_name = format!(".relay-handoff-{head_id}");
        let head_name_on_disk = format!(".RELAY-HANDOFF-{head_id}");
        let index_name = format!(".relay-handoff-{index_id}");
        let disk_name = format!(".relay-handoff-{disk_id}");

        fs::create_dir(repository.join(&head_name_on_disk)).unwrap();
        fs::write(
            repository.join(&head_name_on_disk).join("tracked.txt"),
            b"head\n",
        )
        .unwrap();
        run_git(&repository, &["add", &head_name_on_disk]);
        run_git(&repository, &["commit", "-m", "head collision"]);
        fs::create_dir(repository.join(&index_name)).unwrap();
        fs::write(repository.join(&index_name).join("staged.txt"), b"index\n").unwrap();
        run_git(&repository, &["add", &index_name]);
        fs::create_dir(repository.join(&disk_name)).unwrap();
        fs::write(repository.join(&disk_name).join("user.txt"), b"disk\n").unwrap();

        let repository = fs::canonicalize(repository).unwrap();
        assert!(handoff_path_conflicts_with_git(&repository, &head_name).unwrap());
        assert!(handoff_path_conflicts_with_git(&repository, &index_name).unwrap());
        let status_before = run_git(
            &repository,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        );
        let mut ids = [head_id, index_id, disk_id, free_id.clone()].into_iter();
        let installation = install_handoff_directory_with_id_source(
            &repository,
            b"handoff\n",
            &json!({"schema": HANDOFF_SCHEMA}),
            || Ok(ids.next().unwrap()),
        )
        .unwrap();
        assert_eq!(
            installation.directory.path.file_name().unwrap(),
            OsStr::new(&format!(".relay-handoff-{free_id}"))
        );
        assert_eq!(
            fs::read(repository.join(&disk_name).join("user.txt")).unwrap(),
            b"disk\n"
        );
        assert_eq!(
            run_git(
                &repository,
                &["status", "--porcelain=v1", "--untracked-files=all"]
            ),
            status_before
        );
    }

    #[test]
    fn conversation_only_package_creates_a_plain_handoff_folder_without_git() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("conversation-only.relaypack");
        let result = export_from_preview(
            default_request(&output),
            adapter_preview(sample_messages(), "complete"),
        );
        let package_path = result.package_path.clone();
        let package_key = result.key_fragment.clone();
        let target = temp.path().join("conversation-handoff");
        let restored = restore_relaypack(RestoreRelaypackRequest {
            package_path: result.package_path,
            key: result.key_fragment,
            repository_path: None,
            target_path: target.to_string_lossy().into_owned(),
            branch_name: None,
        })
        .unwrap();
        assert_eq!(restored.head, None);
        assert_eq!(restored.branch_name, None);
        assert!(!restored.staged_applied);
        assert!(!restored.unstaged_applied);
        assert_eq!(restored.untracked_files_restored, 0);
        assert!(!restored.preview.git_included);
        assert!(!git_succeeds(
            &target,
            &["rev-parse", "--is-inside-work-tree"]
        ));
        let handoff_directory = fs::canonicalize(&restored.handoff_directory).unwrap();
        let target_canonical = fs::canonicalize(&target).unwrap();
        assert_eq!(handoff_directory, target_canonical);
        assert_eq!(
            Path::new(&restored.handoff_markdown_path),
            target_canonical.join("HANDOFF.md")
        );
        assert_eq!(
            Path::new(&restored.handoff_json_path),
            target_canonical.join("handoff.json")
        );
        assert!(Path::new(&restored.handoff_markdown_path).is_file());
        assert!(Path::new(&restored.handoff_json_path).is_file());
        assert!(fs::metadata(&restored.handoff_markdown_path)
            .unwrap()
            .permissions()
            .readonly());
        assert!(fs::metadata(&restored.handoff_json_path)
            .unwrap()
            .permissions()
            .readonly());

        let existing_target = temp.path().join("existing-handoff");
        fs::create_dir(&existing_target).unwrap();
        let error = restore_relaypack(RestoreRelaypackRequest {
            package_path,
            key: package_key,
            repository_path: None,
            target_path: existing_target.to_string_lossy().into_owned(),
            branch_name: None,
        })
        .unwrap_err();
        assert_eq!(error.code, "target_exists");
    }

    #[cfg(unix)]
    #[test]
    fn git_capture_never_executes_fsmonitor_textconv_or_clean_filters() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        init_repository(&repository);
        fs::write(repository.join(".gitattributes"), b"*.txt diff=canary\n").unwrap();
        fs::write(repository.join("tracked.txt"), b"base\n").unwrap();
        run_git(&repository, &["add", ".gitattributes", "tracked.txt"]);
        run_git(&repository, &["commit", "-m", "base"]);

        let fsmonitor_marker = temp.path().join("fsmonitor.marker");
        let textconv_marker = temp.path().join("textconv.marker");
        let clean_marker = temp.path().join("clean.marker");
        let fsmonitor = temp.path().join("fsmonitor.sh");
        let textconv = temp.path().join("textconv.sh");
        let clean = temp.path().join("clean.sh");
        write_executable_script(
            &fsmonitor,
            &format!(
                "printf fsmonitor > {}\nprintf '0\\n'",
                shell_quote(&fsmonitor_marker)
            ),
        );
        write_executable_script(
            &textconv,
            &format!(
                "printf textconv > {}\ncat \"$1\"",
                shell_quote(&textconv_marker)
            ),
        );
        write_executable_script(
            &clean,
            &format!("printf clean > {}\ncat", shell_quote(&clean_marker)),
        );
        run_git(
            &repository,
            &["config", "core.fsmonitor", fsmonitor.to_str().unwrap()],
        );
        run_git(
            &repository,
            &["config", "diff.canary.textconv", textconv.to_str().unwrap()],
        );
        run_git(
            &repository,
            &["config", "filter.canary.clean", clean.to_str().unwrap()],
        );
        for marker in [&fsmonitor_marker, &textconv_marker, &clean_marker] {
            let _ = fs::remove_file(marker);
        }
        fs::write(repository.join("tracked.txt"), b"changed\n").unwrap();

        let package = temp.path().join("safe-diff.relaypack");
        let mut request = default_request(&package);
        request.include_git = true;
        request.include_local_commits = Some(false);
        request.repository_path = Some(repository.to_string_lossy().into_owned());
        export_from_preview(request, adapter_preview(sample_messages(), "complete"));
        assert!(!fsmonitor_marker.exists());
        assert!(!textconv_marker.exists());
        assert!(!clean_marker.exists());

        fs::write(
            repository.join(".gitattributes"),
            b"*.txt diff=canary filter=canary\n",
        )
        .unwrap();
        let blocked_package = temp.path().join("blocked-filter.relaypack");
        let mut request = default_request(&blocked_package);
        request.include_git = true;
        request.include_local_commits = Some(false);
        request.repository_path = Some(repository.to_string_lossy().into_owned());
        let output_path = validate_new_relaypack_path(&request.output_path).unwrap();
        let error = export_relaypack_from_preview(
            request,
            adapter_preview(sample_messages(), "complete"),
            output_path,
        )
        .unwrap_err();
        assert_eq!(error.code, "git_filter_blocked");
        assert!(!blocked_package.exists());
        assert!(!fsmonitor_marker.exists());
        assert!(!textconv_marker.exists());
        assert!(!clean_marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn restore_disables_checkout_hooks_and_blocks_filters_before_execution() {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        fs::create_dir(&remote).unwrap();
        run_git(&remote, &["init", "--bare"]);

        let sender = temp.path().join("sender");
        init_repository(&sender);
        run_git(
            &sender,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        fs::write(sender.join("tracked.txt"), b"base\n").unwrap();
        run_git(&sender, &["add", "tracked.txt"]);
        run_git(&sender, &["commit", "-m", "base"]);
        let branch = run_git(&sender, &["branch", "--show-current"]);
        run_git(&sender, &["push", "-u", "origin", &branch]);

        let receiver = temp.path().join("receiver");
        let clone_output = Command::new("git")
            .args([
                "clone",
                remote.to_str().unwrap(),
                receiver.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(clone_output.status.success());

        let hook_marker = temp.path().join("post-checkout.marker");
        let filter_marker = temp.path().join("filter.marker");
        let hooks = temp.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        write_executable_script(
            &hooks.join("post-checkout"),
            &format!("printf hook > {}", shell_quote(&hook_marker)),
        );
        let filter = temp.path().join("filter.sh");
        write_executable_script(
            &filter,
            &format!("printf filter > {}\nexit 1", shell_quote(&filter_marker)),
        );
        run_git(
            &receiver,
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );
        run_git(
            &receiver,
            &["config", "filter.canary.process", filter.to_str().unwrap()],
        );
        run_git(&receiver, &["config", "filter.canary.required", "true"]);

        let package = temp.path().join("restore-canary.relaypack");
        let mut request = default_request(&package);
        request.include_git = true;
        request.repository_path = Some(sender.to_string_lossy().into_owned());
        let result = export_from_preview(request, adapter_preview(sample_messages(), "complete"));

        let safe_target = temp.path().join("safe-target");
        restore_relaypack(RestoreRelaypackRequest {
            package_path: result.package_path.clone(),
            key: result.key_fragment.clone(),
            repository_path: Some(receiver.to_string_lossy().into_owned()),
            target_path: safe_target.to_string_lossy().into_owned(),
            branch_name: Some("relay/hook-canary".into()),
        })
        .unwrap();
        assert!(safe_target.exists());
        assert!(!hook_marker.exists());
        assert!(!filter_marker.exists());

        fs::write(
            receiver.join(".git/info/attributes"),
            b"*.txt filter=canary\n",
        )
        .unwrap();
        let blocked_target = temp.path().join("blocked-target");
        let blocked_branch = "relay/filter-canary";
        let error = restore_relaypack(RestoreRelaypackRequest {
            package_path: result.package_path,
            key: result.key_fragment,
            repository_path: Some(receiver.to_string_lossy().into_owned()),
            target_path: blocked_target.to_string_lossy().into_owned(),
            branch_name: Some(blocked_branch.into()),
        })
        .unwrap_err();
        assert_eq!(error.code, "git_filter_blocked");
        assert!(!hook_marker.exists());
        assert!(!filter_marker.exists());
        assert!(!blocked_target.exists());
        assert!(!branch_exists(&receiver, blocked_branch));
    }

    #[test]
    fn selected_git_paths_are_literal_and_rename_sources_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        init_repository(&repository);
        for path in [
            "staged-keep.txt",
            "staged-drop.txt",
            ":(glob)literal.txt",
            "old-name.txt",
            "unstaged-keep.txt",
            "unstaged-drop.txt",
        ] {
            fs::write(repository.join(path), b"base\n").unwrap();
        }
        run_git(&repository, &["add", "."]);
        run_git(&repository, &["commit", "-m", "base"]);

        fs::write(repository.join("staged-keep.txt"), b"selected staged\n").unwrap();
        fs::write(repository.join("staged-drop.txt"), b"unselected staged\n").unwrap();
        fs::write(repository.join(":(glob)literal.txt"), b"literal selected\n").unwrap();
        run_git(
            &repository,
            &[
                "add",
                "staged-keep.txt",
                "staged-drop.txt",
                ":(literal):(glob)literal.txt",
            ],
        );
        run_git(&repository, &["mv", "old-name.txt", "new-name.txt"]);
        fs::write(repository.join("unstaged-keep.txt"), b"selected unstaged\n").unwrap();
        fs::write(
            repository.join("unstaged-drop.txt"),
            b"unselected unstaged\n",
        )
        .unwrap();

        let package = temp.path().join("selected-git.relaypack");
        let mut request = default_request(&package);
        request.include_git = true;
        request.include_local_commits = Some(false);
        request.repository_path = Some(repository.to_string_lossy().into_owned());
        request.selected_staged = vec![
            "staged-keep.txt".into(),
            ":(glob)literal.txt".into(),
            "new-name.txt".into(),
        ];
        request.selected_unstaged = vec!["unstaged-keep.txt".into()];
        let result = export_from_preview(request, adapter_preview(sample_messages(), "complete"));
        let loaded = load_relaypack(&result.package_path, &result.key_fragment).unwrap();
        let staged_id = loaded
            .envelope
            .handoff
            .pointer("/git/capture/staged_patch/asset_id")
            .and_then(Value::as_str)
            .unwrap();
        let unstaged_id = loaded
            .envelope
            .handoff
            .pointer("/git/capture/unstaged_patch/asset_id")
            .and_then(Value::as_str)
            .unwrap();
        let staged_patch = loaded
            .envelope
            .payloads
            .iter()
            .find(|payload| payload.asset_id == staged_id)
            .unwrap()
            .decode()
            .unwrap();
        let unstaged_patch = loaded
            .envelope
            .payloads
            .iter()
            .find(|payload| payload.asset_id == unstaged_id)
            .unwrap()
            .decode()
            .unwrap();
        let staged_text = String::from_utf8_lossy(&staged_patch);
        let unstaged_text = String::from_utf8_lossy(&unstaged_patch);
        assert!(staged_text.contains("staged-keep.txt"));
        assert!(staged_text.contains("literal.txt"));
        assert!(staged_text.contains("old-name.txt"));
        assert!(staged_text.contains("new-name.txt"));
        assert!(!staged_text.contains("staged-drop.txt"));
        assert!(unstaged_text.contains("unstaged-keep.txt"));
        assert!(!unstaged_text.contains("unstaged-drop.txt"));
        let git_omission = loaded
            .envelope
            .handoff
            .pointer("/export/omissions")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .find(|omission| omission.get("reason").and_then(Value::as_str) == Some("git_excluded"))
            .unwrap();
        assert_eq!(git_omission.get("count").and_then(Value::as_u64), Some(3));

        let omitted_package = temp.path().join("omitted-git.relaypack");
        let mut omitted = default_request(&omitted_package);
        omitted.include_git = true;
        omitted.include_local_commits = Some(false);
        omitted.repository_path = Some(repository.to_string_lossy().into_owned());
        let omitted_result =
            export_from_preview(omitted, adapter_preview(sample_messages(), "complete"));
        let omitted_handoff = read_handoff(&omitted_result);
        assert_eq!(
            omitted_handoff
                .pointer("/git/capture/staged_patch/status")
                .and_then(Value::as_str),
            Some("omitted")
        );
        assert_eq!(
            omitted_handoff
                .pointer("/git/capture/unstaged_patch/status")
                .and_then(Value::as_str),
            Some("omitted")
        );
        assert!(omitted_handoff
            .pointer("/git/capture/staged_patch/asset_id")
            .is_none());
        assert!(omitted_handoff
            .pointer("/git/capture/unstaged_patch/asset_id")
            .is_none());

        let unknown_package = temp.path().join("unknown-git.relaypack");
        let mut unknown = default_request(&unknown_package);
        unknown.include_git = true;
        unknown.include_local_commits = Some(false);
        unknown.repository_path = Some(repository.to_string_lossy().into_owned());
        unknown.selected_staged = vec!["not-present.txt".into()];
        let output_path = validate_new_relaypack_path(&unknown.output_path).unwrap();
        let error = export_relaypack_from_preview(
            unknown,
            adapter_preview(sample_messages(), "complete"),
            output_path,
        )
        .unwrap_err();
        assert_eq!(error.code, "git_change_not_found");
        assert!(!unknown_package.exists());
    }

    #[test]
    fn selected_git_payloads_are_scanned_before_the_package_is_written() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        init_repository(&repository);
        fs::write(repository.join("tracked.txt"), b"base\n").unwrap();
        run_git(&repository, &["add", "tracked.txt"]);
        run_git(&repository, &["commit", "-m", "base"]);
        let patch_secret = "sk-abcdefghijklmnopqrstuvwxyz1234567890";
        let file_secret = "postgres://relay:private-password@example.test/relay";
        fs::write(
            repository.join("tracked.txt"),
            format!("selected {patch_secret}\n"),
        )
        .unwrap();
        fs::write(
            repository.join("new.txt"),
            format!("selected {file_secret}\n"),
        )
        .unwrap();

        let package = temp.path().join("sensitive-git.relaypack");
        let mut request = default_request(&package);
        request.include_git = true;
        request.include_local_commits = Some(false);
        request.repository_path = Some(repository.to_string_lossy().into_owned());
        request.selected_unstaged = vec!["tracked.txt".into()];
        request.selected_untracked = vec!["new.txt".into()];
        let output_path = validate_new_relaypack_path(&request.output_path).unwrap();
        let error = export_relaypack_from_preview(
            request,
            adapter_preview(sample_messages(), "complete"),
            output_path,
        )
        .unwrap_err();
        assert_eq!(error.code, "sensitive_content_confirmation_required");
        assert!(!package.exists());
        let rendered = serde_json::to_string(&error).unwrap();
        assert!(!rendered.contains(patch_secret));
        assert!(!rendered.contains("private-password"));
        let findings = error
            .details
            .as_ref()
            .and_then(|details| details.get("findings"))
            .and_then(Value::as_array)
            .unwrap();
        assert!(findings
            .iter()
            .any(|finding| finding.get("scope").and_then(Value::as_str) == Some("git_patch")));
        assert!(findings.iter().any(|finding| {
            finding.get("scope").and_then(Value::as_str) == Some("untracked_file")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn selected_untracked_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        init_repository(&repository);
        fs::write(repository.join("target.txt"), b"target").unwrap();
        symlink("target.txt", repository.join("link.txt")).unwrap();
        let mut capture = GitCapture::default();
        let error = capture_untracked_files(
            &fs::canonicalize(&repository).unwrap(),
            &["link.txt".into()],
            &["link.txt".into()],
            &mut capture,
        )
        .unwrap_err();
        assert_eq!(error.code, "untracked_file_invalid");
    }

    #[test]
    fn git_export_inspect_and_restore_preserve_sender_state() {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        fs::create_dir(&remote).unwrap();
        run_git(&remote, &["init", "--bare"]);

        let sender = temp.path().join("sender");
        init_repository(&sender);
        run_git(
            &sender,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        fs::write(sender.join("tracked.txt"), b"base\n").unwrap();
        run_git(&sender, &["add", "tracked.txt"]);
        run_git(&sender, &["commit", "-m", "base"]);
        let branch = run_git(&sender, &["branch", "--show-current"]);
        run_git(&sender, &["push", "-u", "origin", &branch]);

        fs::write(sender.join("committed.txt"), b"local commit\n").unwrap();
        run_git(&sender, &["add", "committed.txt"]);
        run_git(&sender, &["commit", "-m", "local"]);
        fs::write(sender.join("staged.txt"), b"staged\n").unwrap();
        run_git(&sender, &["add", "staged.txt"]);
        fs::write(sender.join("tracked.txt"), b"base\nunstaged\n").unwrap();
        fs::write(sender.join("notes.txt"), b"untracked\n").unwrap();
        fs::write(sender.join("unshared.txt"), b"not selected\n").unwrap();

        let receiver = temp.path().join("receiver");
        let clone_output = Command::new("git")
            .args([
                "clone",
                remote.to_str().unwrap(),
                receiver.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(clone_output.status.success());

        let package = temp.path().join("git.relaypack");
        let mut request = default_request(&package);
        request.include_git = true;
        request.repository_path = Some(sender.to_string_lossy().into_owned());
        request.selected_staged = vec!["staged.txt".into()];
        request.selected_unstaged = vec!["tracked.txt".into()];
        request.selected_untracked = vec!["notes.txt".into()];
        let result = export_from_preview(request, adapter_preview(sample_messages(), "complete"));
        let inspected = inspect_relaypack(&result.package_path, &result.key_fragment).unwrap();
        assert!(inspected.preview.git_included);
        let loaded = load_relaypack(&result.package_path, &result.key_fragment).unwrap();
        assert_eq!(
            loaded
                .envelope
                .handoff
                .pointer("/export/mode")
                .and_then(Value::as_str),
            Some("selected")
        );
        let git_omission = loaded
            .envelope
            .handoff
            .pointer("/export/omissions")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .find(|omission| omission.get("reason").and_then(Value::as_str) == Some("git_excluded"))
            .unwrap();
        assert_eq!(git_omission.get("count").and_then(Value::as_u64), Some(1));

        let target = temp.path().join("restored-worktree");
        let restored = restore_relaypack(RestoreRelaypackRequest {
            package_path: result.package_path.clone(),
            key: result.key_fragment.clone(),
            repository_path: Some(receiver.to_string_lossy().into_owned()),
            target_path: target.to_string_lossy().into_owned(),
            branch_name: Some("relay/restored".into()),
        })
        .unwrap();

        let restored_head = restored.head.as_deref().unwrap();
        assert_eq!(run_git(&sender, &["rev-parse", "HEAD"]), restored_head);
        assert_eq!(run_git(&target, &["rev-parse", "HEAD"]), restored_head);
        assert_eq!(
            run_git(&sender, &["diff", "--cached", "--binary", "--full-index"]),
            run_git(&target, &["diff", "--cached", "--binary", "--full-index"])
        );
        assert_eq!(
            run_git(&sender, &["diff", "--binary", "--full-index"]),
            run_git(&target, &["diff", "--binary", "--full-index"])
        );
        assert_eq!(fs::read(target.join("notes.txt")).unwrap(), b"untracked\n");
        assert!(!target.join("unshared.txt").exists());
        assert!(Path::new(&restored.handoff_json_path).exists());

        let existing_target = temp.path().join("already-exists");
        fs::create_dir(&existing_target).unwrap();
        let error = restore_relaypack(RestoreRelaypackRequest {
            package_path: result.package_path,
            key: result.key_fragment,
            repository_path: Some(receiver.to_string_lossy().into_owned()),
            target_path: existing_target.to_string_lossy().into_owned(),
            branch_name: Some("relay/unused".into()),
        })
        .unwrap_err();
        assert_eq!(error.code, "target_exists");
    }
}

fn validate_receiver_remote(
    remotes: &[crate::types::GitRemote],
    expected_fingerprint: Option<&str>,
) -> Result<(), CommandError> {
    let Some(expected) = expected_fingerprint else {
        return Ok(());
    };
    let matches = remotes
        .iter()
        .filter(|remote| remote.kind == "fetch")
        .filter_map(|remote| canonical_remote(&remote.url).ok())
        .any(|remote| sha256_hex(remote.as_bytes()) == expected);
    if !matches {
        return Err(CommandError::new(
            "repository_identity_mismatch",
            "receiver repository does not have a fetch remote matching the Relay package",
        ));
    }
    Ok(())
}

fn validate_branch_name(repository: &Path, branch: &str) -> Result<(), CommandError> {
    if branch.trim().is_empty() || branch != branch.trim() {
        return Err(CommandError::new(
            "invalid_branch_name",
            "branch_name cannot be empty or have surrounding whitespace",
        ));
    }
    git_safe_checked_os(
        repository,
        &[
            OsString::from("check-ref-format"),
            OsString::from("--branch"),
            OsString::from(branch),
        ],
        true,
        1024 * 1024,
    )?;
    Ok(())
}

fn ensure_branch_absent(repository: &Path, branch: &str) -> Result<(), CommandError> {
    let mut args = safe_git_prefix();
    args.extend([
        OsString::from("show-ref"),
        OsString::from("--verify"),
        OsString::from("--quiet"),
        OsString::from(format!("refs/heads/{branch}")),
    ]);
    let output = git_raw_os(repository, &args, true, 1024 * 1024)?;
    match output.status.code() {
        Some(0) => Err(CommandError::new(
            "branch_exists",
            format!("branch '{branch}' already exists"),
        )),
        Some(1) => Ok(()),
        _ => Err(git_output_error(&args, &output)),
    }
}

fn validate_new_directory_path(raw: &str, label: &str) -> Result<PathBuf, CommandError> {
    if raw.trim().is_empty() {
        return Err(CommandError::new(
            "invalid_target_path",
            format!("{label} path cannot be empty"),
        ));
    }
    let path = Path::new(raw);
    let name = path.file_name().ok_or_else(|| {
        CommandError::new(
            "invalid_target_path",
            format!("{label} path must have a final directory name"),
        )
    })?;
    let parent = canonical_existing_directory(
        path.parent().unwrap_or_else(|| Path::new(".")),
        &format!("{label} parent"),
    )?;
    let candidate = parent.join(name);
    match fs::symlink_metadata(&candidate) {
        Ok(_) => Err(CommandError::new(
            "target_exists",
            format!("{label} '{}' already exists", candidate.display()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(candidate),
        Err(error) => Err(CommandError::new(
            "invalid_target_path",
            format!("cannot inspect {label} path: {error}"),
        )),
    }
}

fn ensure_commit_exists(repository: &Path, commit: &str) -> Result<(), CommandError> {
    if !is_commit_id(commit) {
        return Err(CommandError::new(
            "relaypack_invalid",
            "package contains an invalid commit id",
        ));
    }
    git_safe_checked_os(
        repository,
        &[
            OsString::from("cat-file"),
            OsString::from("-e"),
            OsString::from(format!("{commit}^{{commit}}")),
        ],
        true,
        1024 * 1024,
    )?;
    Ok(())
}

fn git_root_without_worktree_filters(repository: &Path) -> Result<PathBuf, CommandError> {
    let output = git_safe_checked_os(
        repository,
        &[
            OsString::from("rev-parse"),
            OsString::from("--show-toplevel"),
        ],
        true,
        1024 * 1024,
    )?;
    let root = bytes_to_trimmed_string(&output.stdout);
    canonical_existing_directory(Path::new(&root), "Git worktree root")
}

fn git_head_without_worktree_filters(repository: &Path) -> Result<Option<String>, CommandError> {
    let mut args = safe_git_prefix();
    args.extend([
        OsString::from("rev-parse"),
        OsString::from("--verify"),
        OsString::from("HEAD"),
    ]);
    let output = git_raw_os(repository, &args, true, 1024 * 1024)?;
    if !output.status.success() {
        return Ok(None);
    }
    let head = bytes_to_trimmed_string(&output.stdout);
    if !is_commit_id(&head) {
        return Err(CommandError::new(
            "git_protocol_error",
            "Git returned an invalid HEAD commit while checking attributes",
        ));
    }
    Ok(Some(head))
}

fn ensure_worktree_attributes_safe(repository: &Path) -> Result<(), CommandError> {
    let paths = git_safe_checked_os(
        repository,
        &[
            OsString::from("ls-files"),
            OsString::from("-z"),
            OsString::from("--cached"),
            OsString::from("--others"),
            OsString::from("--"),
            OsString::from(".gitattributes"),
            OsString::from(":(glob)**/.gitattributes"),
        ],
        true,
        MAX_GIT_OUTPUT,
    )?;
    for raw_path in paths
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(raw_path).map_err(|_| {
            CommandError::new(
                "git_filter_blocked",
                "a .gitattributes path is not valid UTF-8",
            )
        })?;
        validate_relative_path(path, true)?;
        let full_path = repository.join(path);
        match fs::symlink_metadata(&full_path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(CommandError::new(
                    "git_filter_blocked",
                    "a repository attribute path is not an ordinary file",
                ));
            }
            Ok(_) => {
                let bytes = read_file_limited(&full_path, 1024 * 1024)?;
                reject_filter_attributes(path, &bytes)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(CommandError::new(
                    "git_filter_blocked",
                    "cannot safely inspect a repository attribute file",
                ))
            }
        }
    }

    let index = git_safe_checked_os(
        repository,
        &[
            OsString::from("ls-files"),
            OsString::from("--stage"),
            OsString::from("-z"),
            OsString::from("--"),
            OsString::from(".gitattributes"),
            OsString::from(":(glob)**/.gitattributes"),
        ],
        true,
        MAX_GIT_OUTPUT,
    )?;
    for record in index
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let record = std::str::from_utf8(record).map_err(|_| {
            CommandError::new(
                "git_filter_blocked",
                "a .gitattributes index record is not valid UTF-8",
            )
        })?;
        let (metadata, path) = record.split_once('\t').ok_or_else(|| {
            CommandError::new(
                "git_protocol_error",
                "Git returned a malformed .gitattributes index record",
            )
        })?;
        validate_relative_path(path, true)?;
        let mut fields = metadata.split_whitespace();
        let mode = fields.next().unwrap_or_default();
        let object = fields.next().unwrap_or_default();
        let stage = fields.next().unwrap_or_default();
        if !matches!(mode, "100644" | "100755")
            || !is_commit_id(object)
            || !matches!(stage, "0" | "1" | "2" | "3")
            || fields.next().is_some()
        {
            return Err(CommandError::new(
                "git_filter_blocked",
                "a repository attribute file has an unsafe index entry",
            ));
        }
        let blob = git_safe_checked_os(
            repository,
            &[
                OsString::from("cat-file"),
                OsString::from("blob"),
                OsString::from(object),
            ],
            true,
            1024 * 1024,
        )?;
        reject_filter_attributes(path, &blob.stdout)?;
    }
    Ok(())
}

fn reject_filter_attributes(_path: &str, bytes: &[u8]) -> Result<(), CommandError> {
    if attributes_define_filter(bytes) {
        return Err(CommandError::new(
            "git_filter_blocked",
            "repository attributes define a non-LFS Git filter",
        ));
    }
    Ok(())
}

fn ensure_receiver_attributes_safe(repository: &Path) -> Result<(), CommandError> {
    let output = git_safe_checked_os(
        repository,
        &[
            OsString::from("rev-parse"),
            OsString::from("--git-path"),
            OsString::from("info/attributes"),
        ],
        true,
        1024 * 1024,
    )?;
    let raw_path = bytes_to_trimmed_string(&output.stdout);
    if raw_path.is_empty() {
        return Ok(());
    }
    let path = PathBuf::from(raw_path);
    let path = if path.is_absolute() {
        path
    } else {
        repository.join(path)
    };
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(CommandError::new(
                "git_filter_blocked",
                "receiver info/attributes is not an ordinary file",
            ));
        }
        Ok(_) => {
            let bytes = read_file_limited(&path, 1024 * 1024)?;
            if attributes_define_filter(&bytes) {
                return Err(CommandError::new(
                    "git_filter_blocked",
                    "receiver info/attributes defines an external Git filter",
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(CommandError::new(
                "git_filter_blocked",
                format!("cannot inspect receiver info/attributes: {error}"),
            ))
        }
    }
    Ok(())
}

fn ensure_commit_attributes_safe(repository: &Path, head: &str) -> Result<(), CommandError> {
    let output = git_safe_checked_os(
        repository,
        &[
            OsString::from("ls-tree"),
            OsString::from("-r"),
            OsString::from("--name-only"),
            OsString::from("-z"),
            OsString::from(head),
        ],
        true,
        MAX_GIT_OUTPUT,
    )?;
    for path in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = String::from_utf8_lossy(path);
        if Path::new(path.as_ref()).file_name() != Some(OsStr::new(".gitattributes")) {
            continue;
        }
        validate_relative_path(&path, true)?;
        let spec = format!("{head}:{path}");
        let blob = git_safe_checked_os(
            repository,
            &[OsString::from("show"), OsString::from(spec)],
            true,
            1024 * 1024,
        )?;
        if attributes_define_filter(&blob.stdout) {
            return Err(CommandError::new(
                "git_filter_blocked",
                "repository attributes define a non-LFS Git filter",
            ));
        }
    }
    git::ensure_lfs_commit_safe(repository, head)
}

fn attributes_define_filter(bytes: &[u8]) -> bool {
    git::attribute_filter_definitions(bytes).non_lfs
}

fn safe_git_prefix() -> Vec<OsString> {
    vec![
        OsString::from("-c"),
        OsString::from(format!("core.hooksPath={GIT_NULL_DEVICE}")),
        OsString::from("-c"),
        OsString::from("core.fsmonitor=false"),
        OsString::from("-c"),
        OsString::from(format!("core.attributesFile={GIT_NULL_DEVICE}")),
        OsString::from("-c"),
        OsString::from("filter.lfs.clean="),
        OsString::from("-c"),
        OsString::from("filter.lfs.smudge="),
        OsString::from("-c"),
        OsString::from("filter.lfs.process="),
        OsString::from("-c"),
        OsString::from("filter.lfs.required=false"),
    ]
}

fn git_safe_checked_os(
    repository: &Path,
    args: &[OsString],
    read_only: bool,
    max_stdout: usize,
) -> Result<ProcessOutput, CommandError> {
    let mut safe = safe_git_prefix();
    safe.extend_from_slice(args);
    git_checked_os(repository, &safe, read_only, max_stdout)
}
