use crate::process::{canonical_executable, canonical_existing_directory, run_process};
use crate::types::{
    AgentProvider, CommandError, ImportNativeSessionRequest, ImportNativeSessionResult,
};
use serde::Deserialize;
use serde_json::json;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const IMPORTER_ENV: &str = "RELAY_SESSION_IMPORTER";
const IMPORT_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_IMPORTER_STDOUT: usize = 2 * 1024 * 1024;
const MAX_IMPORTER_STDERR: usize = 128 * 1024;
const MAX_HANDOFF_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct ImporterResponse {
    ok: bool,
    #[serde(default)]
    result: Option<ImportNativeSessionResult>,
    #[serde(default)]
    error: Option<ImporterError>,
}

#[derive(Debug, Deserialize)]
struct ImporterError {
    code: String,
    message: String,
    #[serde(default)]
    backup_dir: String,
    #[serde(default)]
    steps: Vec<String>,
}

pub fn import_native_session(
    request: ImportNativeSessionRequest,
) -> Result<ImportNativeSessionResult, CommandError> {
    if request.agent == AgentProvider::Unknown {
        return Err(CommandError::new(
            "invalid_agent",
            "agent must be claude_code or codex",
        ));
    }
    let worktree = canonical_existing_directory(Path::new(&request.worktree_path), "worktree")?;
    let handoff = canonical_handoff_json(Path::new(&request.handoff_json_path), &worktree)?;
    let importer = resolve_importer()?;
    let input = serde_json::to_vec(&json!({
        "handoff_path": handoff,
        "target": request.agent.as_str(),
        "target_cwd": worktree,
        "execute": true
    }))
    .map_err(|error| {
        CommandError::new(
            "native_import_failed",
            format!("cannot encode the native import request: {error}"),
        )
    })?;
    let output = run_process(
        &importer,
        &[] as &[OsString],
        Some(&input),
        IMPORT_TIMEOUT,
        MAX_IMPORTER_STDOUT,
        MAX_IMPORTER_STDERR,
        &[],
    )
    .map_err(|error| {
        CommandError::new(
            "native_import_failed",
            format!("the native session importer could not finish: {error}"),
        )
    })?;
    if !output.status.success() {
        return Err(CommandError::new(
            "native_import_failed",
            format!(
                "the native session importer exited with status {}",
                output.status
            ),
        ));
    }
    if output.stdout_truncated {
        return Err(CommandError::new(
            "native_import_failed",
            "the native session importer returned too much output",
        ));
    }
    let response: ImporterResponse = serde_json::from_slice(&output.stdout).map_err(|error| {
        CommandError::new(
            "native_import_failed",
            format!("the native session importer returned invalid JSON: {error}"),
        )
    })?;
    if !response.ok {
        let error = response.error.unwrap_or(ImporterError {
            code: "native_import_failed".into(),
            message: "the native session importer did not provide an error message".into(),
            backup_dir: String::new(),
            steps: Vec::new(),
        });
        let mut details = serde_json::Map::new();
        if !error.backup_dir.is_empty() {
            details.insert("backup_dir".into(), json!(error.backup_dir));
        }
        if !error.steps.is_empty() {
            details.insert("steps".into(), json!(error.steps));
        }
        let command_error = CommandError::new(&error.code, error.message);
        return Err(if details.is_empty() {
            command_error
        } else {
            command_error.with_details(serde_json::Value::Object(details))
        });
    }
    let mut result = response.result.ok_or_else(|| {
        CommandError::new(
            "native_import_failed",
            "the native session importer returned no result",
        )
    })?;
    if result.status != "ok" || result.dry_run {
        return Err(CommandError::new(
            "native_import_unverified",
            "the native session importer did not confirm a completed import",
        ));
    }
    if result.target != request.agent.as_str() || result.target_cwd != worktree.to_string_lossy() {
        return Err(CommandError::new(
            "native_import_unverified",
            "the native session importer returned a different target than requested",
        ));
    }
    let session_path = fs::canonicalize(&result.session_path).map_err(|error| {
        CommandError::new(
            "native_import_unverified",
            format!("the imported native history cannot be found: {error}"),
        )
    })?;
    let metadata = fs::metadata(&session_path).map_err(|error| {
        CommandError::new(
            "native_import_unverified",
            format!("the imported native history cannot be inspected: {error}"),
        )
    })?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(CommandError::new(
            "native_import_unverified",
            "the imported native history is missing or empty",
        ));
    }
    let target_verification_failed = if request.agent == AgentProvider::Codex {
        result.verification.state != Some(true) || result.verification.pinned != Some(true)
    } else {
        result.verification.state == Some(false) || result.verification.pinned == Some(false)
    };
    if !result.verification.session_file || !result.verification.index || target_verification_failed
    {
        return Err(CommandError::new(
            "native_import_unverified",
            "the native session importer did not verify every required record",
        ));
    }
    if request.agent == AgentProvider::Codex {
        match crate::chatgpt::refresh_and_show_task(
            Path::new(&result.target_home),
            &result.session_id,
        ) {
            Ok((refresh_status, refresh_error)) => {
                match refresh_error {
                    Some(error) => {
                        result.catalog_refresh_status = "failed".into();
                        result.catalog_refresh_error_code = Some(error.code);
                        result.catalog_refresh_error = Some(error.message);
                    }
                    None => {
                        result.catalog_refresh_status = match refresh_status {
                            crate::chatgpt::CatalogRefreshStatus::Sent => "sent",
                            crate::chatgpt::CatalogRefreshStatus::NotRunning => "not_running",
                        }
                        .into();
                        result.catalog_refresh_error_code = None;
                        result.catalog_refresh_error = None;
                    }
                }
                result.open_status = "opened".into();
                result.open_error_code = None;
                result.open_error = None;
            }
            Err(error) => {
                result.open_status = chatgpt_open_status(&error.code).into();
                result.open_error_code = Some(error.code);
                result.open_error = Some(error.message);
            }
        }
    } else {
        result.catalog_refresh_status.clear();
        result.catalog_refresh_error_code = None;
        result.catalog_refresh_error = None;
        result.open_status = "manual".into();
        result.open_error_code = None;
        result.open_error = None;
    }
    Ok(result)
}

fn chatgpt_open_status(error_code: &str) -> &'static str {
    match error_code {
        "chatgpt_handler_not_found" | "unsupported_platform" => "manual",
        _ => "failed",
    }
}

fn resolve_importer() -> Result<PathBuf, CommandError> {
    if let Some(configured) = std::env::var_os(IMPORTER_ENV) {
        if configured.is_empty() {
            return Err(CommandError::new(
                "session_importer_not_found",
                format!("{IMPORTER_ENV} is set but empty"),
            ));
        }
        return canonical_executable(Path::new(&configured), "Relay Session Importer");
    }

    let mut candidates = Vec::new();
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(directory) = current_exe.parent() {
            candidates.push(directory.join("relay-session-importer"));
            candidates.push(directory.join("binaries/relay-session-importer"));
            if let Some(contents) = directory.parent() {
                candidates.push(contents.join("Resources/relay-session-importer"));
            }
        }
    }
    if cfg!(debug_assertions) {
        candidates.push(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../adapter/bin/relay-session-importer"),
        );
        candidates
            .push(Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries/relay-session-importer"));
    }
    for candidate in candidates {
        if candidate.exists() {
            return canonical_executable(&candidate, "Relay Session Importer");
        }
    }
    Err(CommandError::new(
        "session_importer_not_found",
        "the bundled Relay Session Importer was not found; reinstall Relay",
    ))
}

fn canonical_handoff_json(path: &Path, worktree: &Path) -> Result<PathBuf, CommandError> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        CommandError::new(
            "invalid_handoff_path",
            format!("handoff.json cannot be resolved: {error}"),
        )
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| {
        CommandError::new(
            "invalid_handoff_path",
            format!("cannot inspect handoff.json: {error}"),
        )
    })?;
    if !metadata.is_file() || metadata.len() > MAX_HANDOFF_BYTES {
        return Err(CommandError::new(
            "invalid_handoff_path",
            "handoff.json must be an ordinary file no larger than 1 GiB",
        ));
    }
    if canonical.file_name().and_then(|value| value.to_str()) != Some("handoff.json") {
        return Err(CommandError::new(
            "invalid_handoff_path",
            "the handoff data file must be named handoff.json",
        ));
    }
    if !canonical.starts_with(worktree) {
        return Err(CommandError::new(
            "invalid_handoff_path",
            "handoff.json must be located inside the restored worktree",
        ));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_json_must_be_inside_the_restored_directory() {
        let directory = tempfile::tempdir().unwrap();
        let worktree = directory.path().join("worktree");
        let outside = directory.path().join("outside");
        fs::create_dir(&worktree).unwrap();
        fs::create_dir(&outside).unwrap();
        let worktree = fs::canonicalize(worktree).unwrap();

        let inside = worktree.join("handoff.json");
        fs::write(&inside, "{}").unwrap();
        assert_eq!(
            canonical_handoff_json(&inside, &worktree).unwrap(),
            fs::canonicalize(&inside).unwrap()
        );

        let outside_file = outside.join("handoff.json");
        fs::write(&outside_file, "{}").unwrap();
        assert_eq!(
            canonical_handoff_json(&outside_file, &worktree)
                .unwrap_err()
                .code,
            "invalid_handoff_path"
        );
    }

    #[test]
    fn distinguishes_manual_opening_from_an_open_failure() {
        assert_eq!(chatgpt_open_status("chatgpt_handler_not_found"), "manual");
        assert_eq!(chatgpt_open_status("unsupported_platform"), "manual");
        assert_eq!(chatgpt_open_status("chatgpt_identity_unverified"), "failed");
        assert_eq!(chatgpt_open_status("chatgpt_open_failed"), "failed");
    }
}
