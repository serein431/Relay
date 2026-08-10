mod adapter;
mod chatgpt;
mod git;
mod native_import;
mod process;
mod relaypack;
mod sensitive;
mod share;
mod share_history;
mod types;

pub use types::{
    AdapterHealth, CommandError, DiscoverSessionsRequest, DiscoverSessionsResult,
    DownloadShareRequest, DownloadShareResult, EnvironmentStatus, ExcludedContentBlock,
    ExportRelaypackRequest, ExportRelaypackResult, ImportNativeSessionRequest,
    ImportNativeSessionResult, InspectRelaypackResult, ListShareHistoryResult,
    PreviewSessionRequest, RelaypackDiagnosticPreview, RelaypackPreview, RepositoryInspection,
    RestoreRelaypackRequest, RestoreRelaypackResult, ResumeSavedShareUploadRequest,
    ResumeSavedShareUploadResult, RevokeSavedShareRequest, RevokeSavedShareResult,
    RevokeShareRequest, RevokeShareResult, ShareHistoryRecord, ShareHistoryStatus,
    UploadShareRequest, UploadShareResult,
};

use crate::process::find_executable_on_path;
use crate::types::{AdapterExecutableStatus, AgentHomes, EnvironmentTools, PathStatus, ToolStatus};
use std::path::PathBuf;
use tauri::Manager;

#[tauri::command]
fn environment_status() -> EnvironmentStatus {
    EnvironmentStatus {
        platform: std::env::consts::OS.into(),
        architecture: std::env::consts::ARCH.into(),
        tools: EnvironmentTools {
            git: tool_status("git"),
            claude: tool_status("claude"),
            codex: tool_status("codex"),
        },
        homes: AgentHomes {
            claude: path_status(agent_home("CLAUDE_CONFIG_DIR", ".claude")),
            codex: path_status(agent_home("CODEX_HOME", ".codex")),
        },
        adapter: match adapter::resolve_adapter_executable() {
            Ok(resolved) => AdapterExecutableStatus {
                available: true,
                path: Some(resolved.path.to_string_lossy().into_owned()),
                source: Some(resolved.source),
                reason: None,
            },
            Err(error) => AdapterExecutableStatus {
                available: false,
                path: None,
                source: None,
                reason: Some(error.message),
            },
        },
    }
}

#[tauri::command]
async fn adapter_health() -> Result<AdapterHealth, CommandError> {
    tauri::async_runtime::spawn_blocking(adapter::health)
        .await
        .map_err(|error| {
            CommandError::new(
                "background_task_failed",
                format!("adapter health task failed: {error}"),
            )
        })?
}

#[tauri::command]
async fn discover_sessions(
    request: Option<DiscoverSessionsRequest>,
) -> Result<DiscoverSessionsResult, CommandError> {
    tauri::async_runtime::spawn_blocking(move || adapter::discover(request))
        .await
        .map_err(|error| {
            CommandError::new(
                "background_task_failed",
                format!("session discovery task failed: {error}"),
            )
        })?
}

#[tauri::command]
async fn preview_session(
    request: PreviewSessionRequest,
) -> Result<serde_json::Value, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        let expected_agent = request.agent;
        let expected_session_id = request.session_id.trim().to_owned();
        let preview = adapter::export_session(
            request.agent,
            &request.session_id,
            request.claude_home,
            request.codex_home,
        )?;
        validate_session_preview(&preview, expected_agent, &expected_session_id)?;
        let preview_sha256 = adapter::session_preview_sha256(&preview)?;
        let mut response = preview;
        response
            .as_object_mut()
            .expect("validated adapter preview must be an object")
            .insert(
                "preview_sha256".into(),
                serde_json::Value::String(preview_sha256),
            );
        Ok(response)
    })
    .await
    .map_err(|error| {
        CommandError::new(
            "background_task_failed",
            format!("session preview task failed: {error}"),
        )
    })?
}

fn validate_session_preview(
    preview: &serde_json::Value,
    expected_agent: types::AgentProvider,
    expected_session_id: &str,
) -> Result<(), CommandError> {
    if preview.get("schema").and_then(serde_json::Value::as_str)
        != Some("relay.adapter.handoff-preview.v1")
    {
        return Err(CommandError::new(
            "adapter_protocol_error",
            "adapter export is not relay.adapter.handoff-preview.v1",
        ));
    }
    let source = preview
        .get("source")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| CommandError::new("adapter_protocol_error", "adapter source is missing"))?;
    if source.get("agent").and_then(serde_json::Value::as_str) != Some(expected_agent.as_str()) {
        return Err(CommandError::new(
            "adapter_protocol_error",
            "adapter source agent does not match the preview request",
        ));
    }
    if source.get("session_id").and_then(serde_json::Value::as_str) != Some(expected_session_id) {
        return Err(CommandError::new(
            "adapter_protocol_error",
            "adapter session_id does not match the preview request",
        ));
    }
    if source.get("read_only").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(CommandError::new(
            "adapter_unsafe",
            "adapter export must explicitly report read_only=true",
        ));
    }
    Ok(())
}

#[tauri::command]
async fn inspect_repository(path: String) -> Result<RepositoryInspection, CommandError> {
    tauri::async_runtime::spawn_blocking(move || git::inspect_repository(&path))
        .await
        .map_err(|error| {
            CommandError::new(
                "background_task_failed",
                format!("repository inspection task failed: {error}"),
            )
        })?
}

#[tauri::command]
async fn export_relaypack(
    request: ExportRelaypackRequest,
) -> Result<ExportRelaypackResult, CommandError> {
    tauri::async_runtime::spawn_blocking(move || relaypack::export_relaypack(request))
        .await
        .map_err(|error| {
            CommandError::new(
                "background_task_failed",
                format!("Relay package export task failed: {error}"),
            )
        })?
}

#[tauri::command]
async fn inspect_relaypack(
    path: String,
    key: String,
) -> Result<InspectRelaypackResult, CommandError> {
    tauri::async_runtime::spawn_blocking(move || relaypack::inspect_relaypack(&path, &key))
        .await
        .map_err(|error| {
            CommandError::new(
                "background_task_failed",
                format!("Relay package inspection task failed: {error}"),
            )
        })?
}

#[tauri::command]
async fn restore_relaypack(
    request: RestoreRelaypackRequest,
) -> Result<RestoreRelaypackResult, CommandError> {
    tauri::async_runtime::spawn_blocking(move || relaypack::restore_relaypack(request))
        .await
        .map_err(|error| {
            CommandError::new(
                "background_task_failed",
                format!("Relay package restore task failed: {error}"),
            )
        })?
}

#[tauri::command]
async fn import_native_session(
    request: ImportNativeSessionRequest,
) -> Result<ImportNativeSessionResult, CommandError> {
    tauri::async_runtime::spawn_blocking(move || native_import::import_native_session(request))
        .await
        .map_err(|error| {
            CommandError::new(
                "background_task_failed",
                format!("native session import task failed: {error}"),
            )
        })?
}

#[tauri::command]
async fn open_imported_chatgpt_task(session_id: String) -> Result<(), CommandError> {
    tauri::async_runtime::spawn_blocking(move || chatgpt::open_imported_task(&session_id))
        .await
        .map_err(|error| {
            CommandError::new(
                "background_task_failed",
                format!("ChatGPT open task failed: {error}"),
            )
        })?
}

#[tauri::command]
async fn upload_share(
    app: tauri::AppHandle,
    request: UploadShareRequest,
) -> Result<UploadShareResult, CommandError> {
    let app_data_dir = relay_app_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || perform_upload_share(&app_data_dir, request))
        .await
        .map_err(|error| {
            CommandError::new(
                "background_task_failed",
                format!("share upload task failed: {error}"),
            )
        })?
}

fn perform_upload_share(
    app_data_dir: &std::path::Path,
    request: UploadShareRequest,
) -> Result<UploadShareResult, CommandError> {
    let mut upload_attempt = share_history::prepare_for_upload(app_data_dir)?;
    let result = match share::reserve_share(&request) {
        Ok(result) => result,
        Err(reservation_error) => {
            return Err(
                match share_history::cancel_upload_attempt(app_data_dir, &upload_attempt) {
                    Ok(()) => reservation_error,
                    Err(cleanup_error) => add_command_error_details(
                        reservation_error,
                        serde_json::json!({
                            "upload_recovery_marker_cleanup_failed": true,
                            "cleanup_error": cleanup_error.message,
                        }),
                    ),
                },
            );
        }
    };

    if let Err(storage_error) =
        share_history::record_pending_upload(app_data_dir, &request, &result, &mut upload_attempt)
    {
        let rollback = share::revoke_share(RevokeShareRequest {
            share_id: result.share_id.clone(),
            revoke_token: result.revoke_token.clone(),
            service_base_url: request.service_base_url.clone(),
        });
        let cleanup = share_history::cancel_upload_attempt(app_data_dir, &upload_attempt);
        let mut details = serde_json::json!({
            "share_id": result.share_id,
            "reservation_revoked": rollback.is_ok(),
        });
        if let Err(revoke_error) = rollback {
            details["automatic_revoke_failed"] = serde_json::Value::Bool(true);
            details["revoke_error"] = serde_json::Value::String(revoke_error.message);
        }
        if let Err(cleanup_error) = cleanup {
            details["upload_recovery_marker_cleanup_failed"] = serde_json::Value::Bool(true);
            details["cleanup_error"] = serde_json::Value::String(cleanup_error.message);
        }
        return Err(add_command_error_details(storage_error, details));
    }
    drop(upload_attempt);

    let credentials = share_history::pending_upload_credentials(app_data_dir, &result.share_id)?;
    if let Err(upload_error) = share::upload_reserved_blob(&credentials) {
        return Err(add_command_error_details(
            upload_error,
            serde_json::json!({
                "share_id": result.share_id,
                "upload_pending": true,
                "can_resume": true,
            }),
        ));
    }
    if let Err(storage_error) = share_history::mark_upload_active(app_data_dir, &result.share_id) {
        return Err(add_command_error_details(
            storage_error,
            serde_json::json!({
                "share_id": result.share_id,
                "upload_completed": true,
                "upload_pending_locally": true,
                "can_resume": true,
            }),
        ));
    }
    Ok(result)
}

#[tauri::command]
async fn resume_saved_share_upload(
    app: tauri::AppHandle,
    request: ResumeSavedShareUploadRequest,
) -> Result<ResumeSavedShareUploadResult, CommandError> {
    let app_data_dir = relay_app_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let credentials =
            share_history::pending_upload_credentials(&app_data_dir, &request.share_id)?;
        if let Err(upload_error) = share::upload_reserved_blob(&credentials) {
            return Err(add_command_error_details(
                upload_error,
                serde_json::json!({
                    "share_id": request.share_id,
                    "upload_pending": true,
                    "can_resume": true,
                }),
            ));
        }
        let record = share_history::mark_upload_active(&app_data_dir, &request.share_id)?;
        Ok(ResumeSavedShareUploadResult { record })
    })
    .await
    .map_err(|error| {
        CommandError::new(
            "background_task_failed",
            format!("saved share upload task failed: {error}"),
        )
    })?
}

#[tauri::command]
async fn list_share_history(app: tauri::AppHandle) -> Result<ListShareHistoryResult, CommandError> {
    let app_data_dir = relay_app_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || share_history::list_history(&app_data_dir))
        .await
        .map_err(|error| {
            CommandError::new(
                "background_task_failed",
                format!("share history task failed: {error}"),
            )
        })?
}

#[tauri::command]
async fn revoke_saved_share(
    app: tauri::AppHandle,
    request: RevokeSavedShareRequest,
) -> Result<RevokeSavedShareResult, CommandError> {
    let app_data_dir = relay_app_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let credentials = share_history::revoke_credentials(&app_data_dir, &request.share_id)?;
        if !credentials.already_revoked {
            share::revoke_share(RevokeShareRequest {
                share_id: credentials.share_id.clone(),
                revoke_token: credentials.revoke_token,
                service_base_url: credentials.service_base_url,
            })?;
        }
        let record = share_history::mark_revoked(&app_data_dir, &credentials.share_id)?;
        Ok(RevokeSavedShareResult { record })
    })
    .await
    .map_err(|error| {
        CommandError::new(
            "background_task_failed",
            format!("saved share revoke task failed: {error}"),
        )
    })?
}

#[tauri::command]
async fn download_share(
    request: DownloadShareRequest,
) -> Result<DownloadShareResult, CommandError> {
    tauri::async_runtime::spawn_blocking(move || share::download_share(request))
        .await
        .map_err(|error| {
            CommandError::new(
                "background_task_failed",
                format!("share download task failed: {error}"),
            )
        })?
}

#[tauri::command]
async fn revoke_share(request: RevokeShareRequest) -> Result<RevokeShareResult, CommandError> {
    tauri::async_runtime::spawn_blocking(move || share::revoke_share(request))
        .await
        .map_err(|error| {
            CommandError::new(
                "background_task_failed",
                format!("share revoke task failed: {error}"),
            )
        })?
}

fn tool_status(name: &str) -> ToolStatus {
    let path = find_executable_on_path(name);
    ToolStatus {
        available: path.is_some(),
        path: path.map(|path| path.to_string_lossy().into_owned()),
    }
}

fn agent_home(environment_variable: &str, default_directory: &str) -> Option<PathBuf> {
    std::env::var_os(environment_variable)
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(default_directory))
        })
}

fn relay_app_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, CommandError> {
    app.path().app_data_dir().map_err(|error| {
        CommandError::new(
            "app_data_unavailable",
            format!("cannot resolve Relay app data directory: {error}"),
        )
    })
}

fn path_status(path: Option<PathBuf>) -> PathStatus {
    let Some(path) = path else {
        return PathStatus {
            path: None,
            exists: false,
            is_directory: false,
        };
    };
    let metadata = std::fs::metadata(&path).ok();
    let display_path = if metadata.is_some() {
        std::fs::canonicalize(&path).unwrap_or(path)
    } else {
        path
    };
    PathStatus {
        path: Some(display_path.to_string_lossy().into_owned()),
        exists: metadata.is_some(),
        is_directory: metadata.is_some_and(|metadata| metadata.is_dir()),
    }
}

fn add_command_error_details(
    mut error: CommandError,
    additions: serde_json::Value,
) -> CommandError {
    let mut details = match error.details.take() {
        Some(serde_json::Value::Object(details)) => details,
        Some(other) => {
            let mut details = serde_json::Map::new();
            details.insert("original_details".into(), other);
            details
        }
        None => serde_json::Map::new(),
    };
    if let serde_json::Value::Object(additions) = additions {
        details.extend(additions);
    }
    error.details = Some(serde_json::Value::Object(details));
    error
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            environment_status,
            adapter_health,
            discover_sessions,
            preview_session,
            inspect_repository,
            export_relaypack,
            inspect_relaypack,
            restore_relaypack,
            import_native_session,
            open_imported_chatgpt_task,
            upload_share,
            resume_saved_share_upload,
            list_share_history,
            revoke_saved_share,
            download_share,
            revoke_share
        ])
        .run(tauri::generate_context!())
        .expect("error while running Relay");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn path_status_reports_missing_directory_without_panicking() {
        let status = path_status(Some(
            std::path::Path::new("/path/that/does/not/exist/relay").into(),
        ));
        assert!(!status.exists);
        assert!(!status.is_directory);
    }

    #[test]
    fn session_preview_validation_accepts_matching_read_only_source() {
        let preview = json!({
            "schema": "relay.adapter.handoff-preview.v1",
            "source": {
                "agent": "codex",
                "session_id": "session-1",
                "read_only": true
            }
        });
        validate_session_preview(&preview, types::AgentProvider::Codex, "session-1").unwrap();
    }

    #[test]
    fn session_preview_validation_rejects_mismatched_or_writable_source() {
        let wrong_session = json!({
            "schema": "relay.adapter.handoff-preview.v1",
            "source": {
                "agent": "codex",
                "session_id": "session-2",
                "read_only": true
            }
        });
        assert_eq!(
            validate_session_preview(&wrong_session, types::AgentProvider::Codex, "session-1")
                .unwrap_err()
                .code,
            "adapter_protocol_error"
        );

        let writable = json!({
            "schema": "relay.adapter.handoff-preview.v1",
            "source": {
                "agent": "codex",
                "session_id": "session-1",
                "read_only": false
            }
        });
        assert_eq!(
            validate_session_preview(&writable, types::AgentProvider::Codex, "session-1")
                .unwrap_err()
                .code,
            "adapter_unsafe"
        );
    }
}
