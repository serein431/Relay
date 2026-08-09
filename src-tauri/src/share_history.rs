use crate::share::SavedUploadCredentials;
use crate::types::{
    CommandError, ListShareHistoryResult, ShareHistoryRecord, ShareHistoryStatus,
    UploadShareRequest, UploadShareResult,
};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use url::Url;
use uuid::Uuid;

const RECORD_SCHEMA: &str = "relay.share-history-record.v1";
const HISTORY_DIRECTORY: &str = "share-history-v1";
const CORRUPT_DIRECTORY: &str = "corrupt";
const LEGACY_HISTORY_FILENAME: &str = "share-history.v1.json";
const LEGACY_BACKUP_FILENAME: &str = "legacy-share-history.v1.json.migration-backup";
const HISTORY_LOCK_FILENAME: &str = ".share-history.lock";
const UPLOAD_ATTEMPT_SCHEMA: &str = "relay.share-upload-attempt.v1";
const MAX_HISTORY_RECORDS: usize = 500;
const MAX_RECORD_BYTES: usize = 64 * 1024;
const MAX_CIPHERTEXT_BYTES: u64 = 32 * 1024 * 1024;
const SHARE_ID_LENGTH: usize = 32;
const CAPABILITY_LENGTH: usize = 43;
const SHA256_HEX_LENGTH: usize = 64;

static HISTORY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredShareFile {
    schema: String,
    record: StoredShareRecord,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredShareRecord {
    share_id: String,
    share_url: String,
    service_base_url: String,
    revoke_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    upload_token: Option<String>,
    created_at: String,
    expires_at: String,
    package_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_name: Option<String>,
    ciphertext_sha256: String,
    ciphertext_bytes: u64,
    status: ShareHistoryStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    revoked_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredUploadAttempt {
    schema: String,
    attempt_id: String,
    started_at: String,
}

#[derive(Debug)]
struct ScannedRecord {
    path: PathBuf,
    record: StoredShareRecord,
}

#[derive(Debug, Clone)]
pub struct RevokeCredentials {
    pub share_id: String,
    pub service_base_url: String,
    pub revoke_token: String,
    pub already_revoked: bool,
}

#[derive(Debug)]
pub struct UploadAttempt {
    id: String,
    path: PathBuf,
    file: File,
}

#[derive(Debug)]
struct TemporaryRecord {
    path: PathBuf,
    file: File,
}

pub fn prepare_for_upload(app_data_dir: &Path) -> Result<UploadAttempt, CommandError> {
    with_history_lock(app_data_dir, || {
        let directory = ensure_storage_directory(app_data_dir)?;
        let interrupted = detect_interrupted_uploads(&directory)?;
        if interrupted > 0 {
            return Err(CommandError::new(
                "share_upload_state_uncertain",
                "Relay found an interrupted upload that may have reached the share service; its local recovery marker was preserved",
            )
            .with_details(serde_json::json!({ "interrupted_uploads": interrupted })));
        }
        ensure_capacity(&directory)?;
        create_upload_attempt(&directory)
    })
}

#[cfg(test)]
pub fn record_upload(
    app_data_dir: &Path,
    request: &UploadShareRequest,
    result: &UploadShareResult,
    attempt: &mut UploadAttempt,
) -> Result<ShareHistoryRecord, CommandError> {
    with_history_lock(app_data_dir, || {
        let directory = ensure_storage_directory(app_data_dir)?;
        verify_upload_attempt(&directory, attempt)?;
        let target = record_path(&directory, &result.share_id)?;
        if fs::symlink_metadata(&target).is_ok() {
            return Err(CommandError::new(
                "share_history_conflict",
                "a Relay share record with this ID already exists",
            ));
        }
        ensure_capacity(&directory)?;

        let package_path = canonical_package_path(&request.package_path)?;
        let service_base_url = canonical_service_base_url(&request.service_base_url)?;
        let record = StoredShareRecord {
            share_id: result.share_id.clone(),
            share_url: result.share_url.clone(),
            service_base_url,
            revoke_token: result.revoke_token.clone(),
            upload_token: None,
            created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            expires_at: result.expires_at.clone(),
            package_path,
            project_title: normalize_label(request.project_title.as_deref()),
            project_name: normalize_label(request.project_name.as_deref()),
            ciphertext_sha256: result.ciphertext_sha256.to_ascii_lowercase(),
            ciphertext_bytes: result.ciphertext_bytes,
            status: ShareHistoryStatus::Active,
            revoked_at: None,
        };
        validate_record(&record)?;
        write_new_record(&directory, &target, &record, attempt)?;
        Ok(public_record(&record))
    })
}

pub fn record_pending_upload(
    app_data_dir: &Path,
    request: &UploadShareRequest,
    result: &UploadShareResult,
    attempt: &mut UploadAttempt,
) -> Result<ShareHistoryRecord, CommandError> {
    with_history_lock(app_data_dir, || {
        let directory = ensure_storage_directory(app_data_dir)?;
        verify_upload_attempt(&directory, attempt)?;
        let target = record_path(&directory, &result.share_id)?;
        if fs::symlink_metadata(&target).is_ok() {
            return Err(CommandError::new(
                "share_history_conflict",
                "a Relay share record with this ID already exists",
            ));
        }
        ensure_capacity(&directory)?;

        let record = StoredShareRecord {
            share_id: result.share_id.clone(),
            share_url: result.share_url.clone(),
            service_base_url: canonical_service_base_url(&request.service_base_url)?,
            revoke_token: result.revoke_token.clone(),
            upload_token: Some(result.upload_token.clone()),
            created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            expires_at: result.expires_at.clone(),
            package_path: canonical_package_path(&request.package_path)?,
            project_title: normalize_label(request.project_title.as_deref()),
            project_name: normalize_label(request.project_name.as_deref()),
            ciphertext_sha256: result.ciphertext_sha256.to_ascii_lowercase(),
            ciphertext_bytes: result.ciphertext_bytes,
            status: ShareHistoryStatus::PendingUpload,
            revoked_at: None,
        };
        validate_record(&record)?;
        write_new_record(&directory, &target, &record, attempt)?;
        Ok(public_record(&record))
    })
}

pub fn pending_upload_credentials(
    app_data_dir: &Path,
    share_id: &str,
) -> Result<SavedUploadCredentials, CommandError> {
    validate_share_id(share_id)?;
    with_history_lock(app_data_dir, || {
        let directory = ensure_storage_directory(app_data_dir)?;
        let path = record_path(&directory, share_id)?;
        let record = read_required_record(&directory, &path, share_id)?;
        if record.status != ShareHistoryStatus::PendingUpload {
            return Err(CommandError::new(
                "share_upload_not_pending",
                "the saved share is not waiting for a ciphertext upload",
            ));
        }
        let upload_token = record.upload_token.ok_or_else(|| {
            CommandError::new(
                "share_history_corrupt",
                "the pending Relay share is missing its upload capability",
            )
        })?;
        Ok(SavedUploadCredentials {
            share_id: record.share_id,
            service_base_url: record.service_base_url,
            upload_token,
            package_path: record.package_path,
            ciphertext_sha256: record.ciphertext_sha256,
            ciphertext_bytes: record.ciphertext_bytes,
        })
    })
}

pub fn mark_upload_active(
    app_data_dir: &Path,
    share_id: &str,
) -> Result<ShareHistoryRecord, CommandError> {
    validate_share_id(share_id)?;
    with_history_lock(app_data_dir, || {
        let directory = ensure_storage_directory(app_data_dir)?;
        let path = record_path(&directory, share_id)?;
        let mut record = read_required_record(&directory, &path, share_id)?;
        match record.status {
            ShareHistoryStatus::PendingUpload => {
                record.status = ShareHistoryStatus::Active;
                record.upload_token = None;
                validate_record(&record)?;
                replace_record_state(&directory, &path, &record)?;
            }
            ShareHistoryStatus::Active => {}
            ShareHistoryStatus::Revoked => {
                return Err(CommandError::new(
                    "share_upload_revoked",
                    "the saved share was revoked before its upload could be marked complete",
                ))
            }
        }
        Ok(public_record(&record))
    })
}

pub fn cancel_upload_attempt(
    app_data_dir: &Path,
    attempt: &UploadAttempt,
) -> Result<(), CommandError> {
    with_history_lock(app_data_dir, || {
        let directory = ensure_storage_directory(app_data_dir)?;
        verify_upload_attempt(&directory, attempt)?;
        match fs::remove_file(&attempt.path) {
            Ok(()) => sync_directory(&directory),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(CommandError::new(
                "share_history_write_failed",
                format!("cannot remove a Relay upload recovery marker: {error}"),
            )),
        }
    })
}

pub fn list_history(app_data_dir: &Path) -> Result<ListShareHistoryResult, CommandError> {
    with_history_lock(app_data_dir, || {
        let directory = ensure_storage_directory(app_data_dir)?;
        let interrupted = detect_interrupted_uploads(&directory)?;
        if interrupted > 0 {
            return Err(CommandError::new(
                "share_upload_state_uncertain",
                "Relay found an interrupted upload that may have reached the share service; its local recovery marker was preserved",
            )
            .with_details(serde_json::json!({ "interrupted_uploads": interrupted })));
        }
        let (mut records, _, isolated_count) = scan_records(&directory)?;
        if records.is_empty() && isolated_count > 0 {
            return Err(CommandError::new(
                "share_history_corrupt",
                "Relay isolated invalid share records and found no readable history",
            ));
        }
        records.sort_by(|left, right| right.record.created_at.cmp(&left.record.created_at));
        Ok(ListShareHistoryResult {
            records: records
                .iter()
                .map(|entry| public_record(&entry.record))
                .collect(),
        })
    })
}

pub fn revoke_credentials(
    app_data_dir: &Path,
    share_id: &str,
) -> Result<RevokeCredentials, CommandError> {
    validate_share_id(share_id)?;
    with_history_lock(app_data_dir, || {
        let directory = ensure_storage_directory(app_data_dir)?;
        let path = record_path(&directory, share_id)?;
        let record = read_required_record(&directory, &path, share_id)?;
        Ok(RevokeCredentials {
            share_id: record.share_id,
            service_base_url: record.service_base_url,
            revoke_token: record.revoke_token,
            already_revoked: record.status == ShareHistoryStatus::Revoked,
        })
    })
}

pub fn mark_revoked(
    app_data_dir: &Path,
    share_id: &str,
) -> Result<ShareHistoryRecord, CommandError> {
    validate_share_id(share_id)?;
    with_history_lock(app_data_dir, || {
        let directory = ensure_storage_directory(app_data_dir)?;
        let path = record_path(&directory, share_id)?;
        let mut record = read_required_record(&directory, &path, share_id)?;
        if record.status != ShareHistoryStatus::Revoked {
            record.status = ShareHistoryStatus::Revoked;
            record.upload_token = None;
            record.revoked_at = Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
            validate_record(&record)?;
            replace_record_state(&directory, &path, &record)?;
        }
        Ok(public_record(&record))
    })
}

fn with_history_lock<T>(
    app_data_dir: &Path,
    operation: impl FnOnce() -> Result<T, CommandError>,
) -> Result<T, CommandError> {
    let lock = HISTORY_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().map_err(|_| {
        CommandError::new(
            "share_history_lock_failed",
            "Relay share history is temporarily unavailable",
        )
    })?;
    ensure_private_directory(app_data_dir, "Relay app data")?;
    let _process_guard = acquire_process_lock(app_data_dir)?;
    operation()
}

fn acquire_process_lock(app_data_dir: &Path) -> Result<File, CommandError> {
    let path = app_data_dir.join(HISTORY_LOCK_FILENAME);
    let existed = match fs::symlink_metadata(&path) {
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => {
            return Err(CommandError::new(
                "share_history_lock_failed",
                format!("cannot inspect the Relay share history lock: {error}"),
            ))
        }
    };
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&path).map_err(|error| {
        CommandError::new(
            "share_history_lock_failed",
            format!("cannot open the Relay share history lock safely: {error}"),
        )
    })?;
    let opened = file.metadata().map_err(|error| {
        CommandError::new(
            "share_history_lock_failed",
            format!("cannot inspect the open Relay share history lock: {error}"),
        )
    })?;
    let current = fs::symlink_metadata(&path).map_err(|error| {
        CommandError::new(
            "share_history_lock_failed",
            format!("cannot verify the Relay share history lock: {error}"),
        )
    })?;
    if current.file_type().is_symlink()
        || !current.is_file()
        || !opened.is_file()
        || !same_file(&current, &opened)
    {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            "the Relay share history lock must be a non-symlink ordinary file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                CommandError::new(
                    "share_history_lock_failed",
                    format!("cannot protect the Relay share history lock: {error}"),
                )
            })?;
    }
    lock_file_exclusive(&file)?;
    let locked_current = fs::symlink_metadata(&path).map_err(|error| {
        CommandError::new(
            "share_history_lock_failed",
            format!("cannot re-check the locked Relay share history file: {error}"),
        )
    })?;
    if locked_current.file_type().is_symlink()
        || !locked_current.is_file()
        || !same_file(&locked_current, &opened)
    {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            "the Relay share history lock changed while it was being acquired",
        ));
    }
    if !existed {
        sync_directory(app_data_dir)?;
    }
    Ok(file)
}

#[cfg(unix)]
fn lock_file_exclusive(file: &File) -> Result<(), CommandError> {
    use std::os::fd::AsRawFd;

    loop {
        // SAFETY: the descriptor remains valid for the duration of the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(CommandError::new(
                "share_history_lock_failed",
                format!("cannot lock Relay share history: {error}"),
            ));
        }
    }
}

#[cfg(not(unix))]
fn lock_file_exclusive(_file: &File) -> Result<(), CommandError> {
    Ok(())
}

#[cfg(unix)]
fn try_lock_file_exclusive(file: &File) -> Result<bool, CommandError> {
    use std::os::fd::AsRawFd;

    loop {
        // SAFETY: the descriptor remains valid for the duration of the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == ErrorKind::Interrupted {
            continue;
        }
        if error.kind() == ErrorKind::WouldBlock {
            return Ok(false);
        }
        return Err(CommandError::new(
            "share_history_lock_failed",
            format!("cannot inspect a Relay upload recovery marker lock: {error}"),
        ));
    }
}

#[cfg(not(unix))]
fn try_lock_file_exclusive(_file: &File) -> Result<bool, CommandError> {
    Ok(true)
}

fn history_not_found() -> CommandError {
    CommandError::new(
        "share_history_not_found",
        "the requested share is not present in Relay history",
    )
}

fn history_directory(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(HISTORY_DIRECTORY)
}

fn record_path(directory: &Path, share_id: &str) -> Result<PathBuf, CommandError> {
    validate_share_id(share_id)?;
    Ok(directory.join(format!("{share_id}.json")))
}

fn validate_share_id(share_id: &str) -> Result<(), CommandError> {
    if !valid_capability(share_id, SHARE_ID_LENGTH) {
        return Err(CommandError::new(
            "invalid_share_id",
            "share ID must be a 32-character base64url token",
        ));
    }
    Ok(())
}

fn ensure_storage_directory(app_data_dir: &Path) -> Result<PathBuf, CommandError> {
    ensure_private_directory(app_data_dir, "Relay app data")?;
    let directory = history_directory(app_data_dir);
    ensure_private_directory(&directory, "Relay share history")?;
    exclude_from_system_backup(&directory)?;
    ensure_private_directory(&directory.join(CORRUPT_DIRECTORY), "Relay corrupt history")?;
    migrate_legacy_history(app_data_dir, &directory)?;
    recover_temporary_records(&directory)?;
    Ok(directory)
}

fn ensure_private_directory(path: &Path, label: &str) -> Result<(), CommandError> {
    let mut created = false;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(CommandError::new(
                    "share_history_unsafe_path",
                    format!("{label} path must be a real directory, not a symbolic link"),
                ));
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            match fs::create_dir(path) {
                Ok(()) => created = true,
                Err(create_error) if create_error.kind() == ErrorKind::AlreadyExists => {}
                Err(create_error) => {
                    return Err(CommandError::new(
                        "share_history_write_failed",
                        format!("cannot create {label} directory: {create_error}"),
                    ))
                }
            }
            let metadata = fs::symlink_metadata(path).map_err(|inspect_error| {
                CommandError::new(
                    "share_history_write_failed",
                    format!("cannot inspect {label} directory: {inspect_error}"),
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(CommandError::new(
                    "share_history_unsafe_path",
                    format!("{label} path must be a real directory, not a symbolic link"),
                ));
            }
        }
        Err(error) => {
            return Err(CommandError::new(
                "share_history_write_failed",
                format!("cannot inspect {label} directory: {error}"),
            ))
        }
    }
    protect_directory(path, label)?;
    if created {
        let parent = path.parent().ok_or_else(|| {
            CommandError::new(
                "share_history_write_failed",
                format!("cannot find the parent directory for {label}"),
            )
        })?;
        sync_directory(parent)?;
    }
    Ok(())
}

fn migrate_legacy_history(app_data_dir: &Path, directory: &Path) -> Result<(), CommandError> {
    let source = app_data_dir.join(LEGACY_HISTORY_FILENAME);
    let metadata = match fs::symlink_metadata(&source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(CommandError::new(
                "share_history_read_failed",
                format!("cannot inspect legacy Relay share history: {error}"),
            ))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            "the legacy Relay share history must be a non-symlink ordinary file",
        ));
    }

    let preferred = directory.join(LEGACY_BACKUP_FILENAME);
    let target = if fs::symlink_metadata(&preferred).is_err() {
        preferred
    } else {
        directory.join(format!("{LEGACY_BACKUP_FILENAME}.{}", Uuid::new_v4()))
    };
    rename_exclusive(&source, &target).map_err(|error| {
        CommandError::new(
            "share_history_write_failed",
            format!("cannot preserve legacy Relay share history for migration: {error}"),
        )
    })?;
    protect_regular_file(&target)?;
    sync_directory(app_data_dir)?;
    sync_directory(directory)?;
    Ok(())
}

#[cfg(unix)]
fn protect_directory(path: &Path, label: &str) -> Result<(), CommandError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let expected = fs::symlink_metadata(path).map_err(|error| {
        CommandError::new(
            "share_history_write_failed",
            format!("cannot inspect {label} directory: {error}"),
        )
    })?;
    if expected.file_type().is_symlink() || !expected.is_dir() {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            format!("{label} path must be a real directory, not a symbolic link"),
        ));
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    let directory = options.open(path).map_err(|error| {
        CommandError::new(
            "share_history_unsafe_path",
            format!("cannot open {label} directory safely: {error}"),
        )
    })?;
    let opened = directory.metadata().map_err(|error| {
        CommandError::new(
            "share_history_unsafe_path",
            format!("cannot verify {label} directory: {error}"),
        )
    })?;
    if !same_file(&expected, &opened) {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            format!("{label} directory changed while it was being checked"),
        ));
    }
    directory
        .set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(|error| {
            CommandError::new(
                "share_history_write_failed",
                format!("cannot protect {label} directory: {error}"),
            )
        })
}

#[cfg(not(unix))]
fn protect_directory(_path: &Path, _label: &str) -> Result<(), CommandError> {
    Ok(())
}

#[cfg(target_os = "macos")]
const BACKUP_EXCLUSION_XATTR: &[u8] = b"com.apple.metadata:com_apple_backup_excludeItem\0";
#[cfg(target_os = "macos")]
const BACKUP_EXCLUSION_VALUE: &[u8] = b"com.apple.backupd";

#[cfg(target_os = "macos")]
fn exclude_from_system_backup(path: &Path) -> Result<(), CommandError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let expected = fs::symlink_metadata(path).map_err(|error| {
        CommandError::new(
            "share_history_write_failed",
            format!("cannot inspect Relay share history for backup exclusion: {error}"),
        )
    })?;
    if expected.file_type().is_symlink() || !expected.is_dir() {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            "Relay share history must be a real directory before backup exclusion",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            CommandError::new(
                "share_history_write_failed",
                format!("cannot open Relay share history for backup exclusion: {error}"),
            )
        })?;
    let opened = file.metadata().map_err(|error| {
        CommandError::new(
            "share_history_write_failed",
            format!("cannot verify Relay share history for backup exclusion: {error}"),
        )
    })?;
    if !same_file(&expected, &opened) {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            "Relay share history changed while backup exclusion was being set",
        ));
    }

    // This is the documented Time Machine exclusion attribute used by `tmutil isexcluded`.
    // SAFETY: the file descriptor and both byte buffers remain valid for the call, and the
    // attribute name is NUL-terminated.
    let set_result = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            BACKUP_EXCLUSION_XATTR.as_ptr().cast(),
            BACKUP_EXCLUSION_VALUE.as_ptr().cast(),
            BACKUP_EXCLUSION_VALUE.len(),
            0,
            0,
        )
    };
    if set_result != 0 {
        let error = std::io::Error::last_os_error();
        return Err(CommandError::new(
            "share_history_write_failed",
            format!("cannot exclude Relay share history from system backup: {error}"),
        ));
    }
    let actual = backup_exclusion_value(&file)?;
    if actual != BACKUP_EXCLUSION_VALUE {
        return Err(CommandError::new(
            "share_history_write_failed",
            "Relay could not verify the system-backup exclusion on share history",
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn backup_exclusion_value(file: &File) -> Result<Vec<u8>, CommandError> {
    use std::os::fd::AsRawFd;

    // SAFETY: the descriptor is valid and the attribute name is NUL-terminated.
    let size = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            BACKUP_EXCLUSION_XATTR.as_ptr().cast(),
            std::ptr::null_mut(),
            0,
            0,
            0,
        )
    };
    if size < 0 {
        let error = std::io::Error::last_os_error();
        return Err(CommandError::new(
            "share_history_write_failed",
            format!("cannot read Relay share-history backup exclusion: {error}"),
        ));
    }
    let mut value = vec![0u8; size as usize];
    // SAFETY: the output buffer has exactly the size reported by the first call.
    let read = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            BACKUP_EXCLUSION_XATTR.as_ptr().cast(),
            value.as_mut_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    if read < 0 {
        let error = std::io::Error::last_os_error();
        return Err(CommandError::new(
            "share_history_write_failed",
            format!("cannot verify Relay share-history backup exclusion: {error}"),
        ));
    }
    value.truncate(read as usize);
    Ok(value)
}

#[cfg(not(target_os = "macos"))]
fn exclude_from_system_backup(_path: &Path) -> Result<(), CommandError> {
    Ok(())
}

fn recover_temporary_records(directory: &Path) -> Result<(), CommandError> {
    let paths = fs::read_dir(directory)
        .map_err(|error| {
            CommandError::new(
                "share_history_read_failed",
                format!("cannot inspect Relay temporary share records: {error}"),
            )
        })?
        .filter_map(Result::ok)
        .filter(|entry| is_temporary_record_name(&entry.file_name()))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    for path in paths {
        recover_temporary_record(directory, &path)?;
    }
    Ok(())
}

fn is_temporary_record_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with(".record.") && name.ends_with(".tmp"))
}

fn temporary_record_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let id = name.strip_prefix(".record.")?.strip_suffix(".tmp")?;
    Uuid::parse_str(id).ok().map(|value| value.to_string())
}

fn recover_temporary_record(directory: &Path, path: &Path) -> Result<(), CommandError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(CommandError::new(
                "share_history_read_failed",
                format!("cannot inspect a Relay temporary share record: {error}"),
            ))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        isolate_temporary_path(directory, path)?;
        return Ok(());
    }
    let mut file = open_existing_private_file(path, "Relay temporary share record")?;
    if !try_lock_file_exclusive(&file)? {
        return Ok(());
    }
    let Some(record) = read_record_from_open_file(&mut file, None)? else {
        let source_metadata = file.metadata().map_err(|error| {
            CommandError::new(
                "share_history_read_failed",
                format!("cannot verify an invalid Relay temporary record: {error}"),
            )
        })?;
        if !path_still_matches(path, &source_metadata)? {
            return Ok(());
        }
        drop(file);
        remove_temporary_file(directory, path)?;
        return Ok(());
    };
    let source_metadata = file.metadata().map_err(|error| {
        CommandError::new(
            "share_history_read_failed",
            format!("cannot verify a Relay temporary share record: {error}"),
        )
    })?;
    if !path_still_matches(path, &source_metadata)? {
        return Ok(());
    }

    if let Some(attempt_id) = temporary_record_id(path) {
        let pending = upload_attempt_path(directory, &attempt_id)?;
        if fs::symlink_metadata(&pending).is_ok() {
            fs::rename(path, &pending).map_err(|error| {
                CommandError::new(
                    "share_history_write_failed",
                    format!("cannot recover a staged Relay upload record: {error}"),
                )
            })?;
            return publish_recovered_record(directory, &pending, &record);
        }
    }
    publish_recovered_record(directory, path, &record)
}

fn publish_recovered_record(
    directory: &Path,
    source: &Path,
    record: &StoredShareRecord,
) -> Result<(), CommandError> {
    let target = record_path(directory, &record.share_id)?;
    for _ in 0..3 {
        match read_record_file(&target, &record.share_id)? {
            Some(current) => {
                if recoverable_state_update(&current, record) {
                    fs::rename(source, &target).map_err(|error| {
                        CommandError::new(
                            "share_history_write_failed",
                            format!("cannot recover a Relay share state update: {error}"),
                        )
                    })?;
                } else if current == *record {
                    remove_temporary_file(directory, source)?;
                    return Ok(());
                } else {
                    preserve_recovery_conflict(directory, source, &record.share_id)?;
                    return Err(CommandError::new(
                        "share_history_conflict",
                        "Relay preserved a valid staged share record because its ID conflicts with different saved history",
                    ));
                }
                sync_directory(directory)?;
                return Ok(());
            }
            None => match fs::symlink_metadata(&target) {
                Ok(_) => {
                    if !quarantine_record(directory, &target, &record.share_id)? {
                        continue;
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(CommandError::new(
                        "share_history_read_failed",
                        format!("cannot inspect a recovered Relay share target: {error}"),
                    ))
                }
            },
        }
        match rename_exclusive(source, &target) {
            Ok(()) => {
                sync_directory(directory)?;
                return Ok(());
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(CommandError::new(
                    "share_history_write_failed",
                    format!("cannot publish a recovered Relay share record: {error}"),
                ))
            }
        }
    }
    Err(CommandError::new(
        "share_history_conflict",
        "a Relay share record kept changing while temporary state was being recovered",
    ))
}

fn recoverable_state_update(current: &StoredShareRecord, staged: &StoredShareRecord) -> bool {
    let valid_transition = matches!(
        (current.status, staged.status),
        (
            ShareHistoryStatus::PendingUpload,
            ShareHistoryStatus::Active
        ) | (
            ShareHistoryStatus::PendingUpload | ShareHistoryStatus::Active,
            ShareHistoryStatus::Revoked
        )
    );
    valid_transition
        && current.revoked_at.is_none()
        && match staged.status {
            ShareHistoryStatus::Active => staged.revoked_at.is_none(),
            ShareHistoryStatus::Revoked => staged.revoked_at.is_some(),
            ShareHistoryStatus::PendingUpload => false,
        }
        && current.upload_token.is_some() == (current.status == ShareHistoryStatus::PendingUpload)
        && staged.upload_token.is_none()
        && current.share_id == staged.share_id
        && current.share_url == staged.share_url
        && current.service_base_url == staged.service_base_url
        && current.revoke_token == staged.revoke_token
        && current.created_at == staged.created_at
        && current.expires_at == staged.expires_at
        && current.package_path == staged.package_path
        && current.project_title == staged.project_title
        && current.project_name == staged.project_name
        && current.ciphertext_sha256 == staged.ciphertext_sha256
        && current.ciphertext_bytes == staged.ciphertext_bytes
}

fn remove_temporary_file(directory: &Path, path: &Path) -> Result<(), CommandError> {
    match fs::remove_file(path) {
        Ok(()) => sync_directory(directory),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CommandError::new(
            "share_history_write_failed",
            format!("cannot remove a Relay temporary share record: {error}"),
        )),
    }
}

fn isolate_temporary_path(directory: &Path, path: &Path) -> Result<(), CommandError> {
    let corrupt_directory = directory.join(CORRUPT_DIRECTORY);
    let target = corrupt_directory.join(format!("temporary.{}.corrupt", Uuid::new_v4()));
    rename_exclusive(path, &target).map_err(|error| {
        CommandError::new(
            "share_history_quarantine_failed",
            format!("cannot isolate an unsafe Relay temporary record: {error}"),
        )
    })?;
    sync_directory(directory)?;
    sync_directory(&corrupt_directory)?;
    Ok(())
}

fn preserve_recovery_conflict(
    directory: &Path,
    source: &Path,
    share_id: &str,
) -> Result<(), CommandError> {
    let target = directory.join(format!(
        ".record-conflict.{share_id}.{}.recovery",
        Uuid::new_v4()
    ));
    rename_exclusive(source, &target).map_err(|error| {
        CommandError::new(
            "share_history_write_failed",
            format!("cannot preserve a conflicting Relay recovery record: {error}"),
        )
    })?;
    protect_regular_file(&target)?;
    sync_directory(directory)?;
    Ok(())
}

fn open_existing_private_file(path: &Path, label: &str) -> Result<File, CommandError> {
    let expected = fs::symlink_metadata(path).map_err(|error| {
        CommandError::new(
            "share_history_read_failed",
            format!("cannot inspect {label}: {error}"),
        )
    })?;
    if expected.file_type().is_symlink() || !expected.is_file() {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            format!("{label} must be a non-symlink ordinary file"),
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|error| {
        CommandError::new(
            "share_history_read_failed",
            format!("cannot open {label} safely: {error}"),
        )
    })?;
    let opened = file.metadata().map_err(|error| {
        CommandError::new(
            "share_history_read_failed",
            format!("cannot verify {label}: {error}"),
        )
    })?;
    if !same_file(&expected, &opened) {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            format!("{label} changed while it was being opened"),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                CommandError::new(
                    "share_history_write_failed",
                    format!("cannot protect {label}: {error}"),
                )
            })?;
    }
    Ok(file)
}

fn read_record_from_open_file(
    file: &mut File,
    expected_share_id: Option<&str>,
) -> Result<Option<StoredShareRecord>, CommandError> {
    let metadata = file.metadata().map_err(|error| {
        CommandError::new(
            "share_history_read_failed",
            format!("cannot inspect an open Relay share record: {error}"),
        )
    })?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_RECORD_BYTES as u64 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(0)).map_err(|error| {
        CommandError::new(
            "share_history_read_failed",
            format!("cannot rewind a Relay share record: {error}"),
        )
    })?;
    let mut bytes = Vec::new();
    Read::by_ref(file)
        .take((MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            CommandError::new(
                "share_history_read_failed",
                format!("cannot read a Relay share record: {error}"),
            )
        })?;
    if bytes.is_empty() || bytes.len() > MAX_RECORD_BYTES {
        return Ok(None);
    }
    let stored: StoredShareFile = match serde_json::from_slice(&bytes) {
        Ok(stored) => stored,
        Err(_) => return Ok(None),
    };
    if stored.schema != RECORD_SCHEMA
        || expected_share_id.is_some_and(|expected| stored.record.share_id != expected)
        || validate_record(&stored.record).is_err()
    {
        return Ok(None);
    }
    Ok(Some(stored.record))
}

fn create_upload_attempt(directory: &Path) -> Result<UploadAttempt, CommandError> {
    for _ in 0..8 {
        let id = Uuid::new_v4().to_string();
        let path = upload_attempt_path(directory, &id)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(CommandError::new(
                    "share_history_write_failed",
                    format!("cannot create a Relay upload recovery marker: {error}"),
                ))
            }
        };
        if let Err(error) = lock_file_exclusive(&file) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        let stored = StoredUploadAttempt {
            schema: UPLOAD_ATTEMPT_SCHEMA.to_owned(),
            attempt_id: id.clone(),
            started_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        };
        let bytes = match serde_json::to_vec_pretty(&stored) {
            Ok(bytes) => bytes,
            Err(error) => {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(CommandError::new(
                    "share_history_write_failed",
                    format!("cannot encode a Relay upload recovery marker: {error}"),
                ));
            }
        };
        let save_result = (|| {
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()
        })();
        if let Err(error) = save_result {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(CommandError::new(
                "share_history_write_failed",
                format!("cannot save a Relay upload recovery marker: {error}"),
            ));
        }
        if let Err(error) = sync_directory(directory) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        return Ok(UploadAttempt { id, path, file });
    }
    Err(CommandError::new(
        "share_history_write_failed",
        "cannot reserve a unique Relay upload recovery marker",
    ))
}

fn upload_attempt_path(directory: &Path, attempt_id: &str) -> Result<PathBuf, CommandError> {
    let parsed = Uuid::parse_str(attempt_id).map_err(|_| {
        CommandError::new(
            "share_history_unsafe_path",
            "a Relay upload recovery marker has an invalid identifier",
        )
    })?;
    if parsed.to_string() != attempt_id {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            "a Relay upload recovery marker has a non-canonical identifier",
        ));
    }
    Ok(directory.join(format!(".upload-attempt.{attempt_id}.pending")))
}

fn verify_upload_attempt(directory: &Path, attempt: &UploadAttempt) -> Result<(), CommandError> {
    if attempt.path != upload_attempt_path(directory, &attempt.id)? {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            "the Relay upload recovery marker is outside share history",
        ));
    }
    let current = fs::symlink_metadata(&attempt.path).map_err(|error| {
        CommandError::new(
            "share_history_write_failed",
            format!("cannot inspect the Relay upload recovery marker: {error}"),
        )
    })?;
    let opened = attempt.file.metadata().map_err(|error| {
        CommandError::new(
            "share_history_write_failed",
            format!("cannot verify the open Relay upload recovery marker: {error}"),
        )
    })?;
    if current.file_type().is_symlink()
        || !current.is_file()
        || !opened.is_file()
        || !same_file(&current, &opened)
    {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            "the Relay upload recovery marker changed unexpectedly",
        ));
    }
    Ok(())
}

fn detect_interrupted_uploads(directory: &Path) -> Result<usize, CommandError> {
    let paths = fs::read_dir(directory)
        .map_err(|error| {
            CommandError::new(
                "share_history_read_failed",
                format!("cannot inspect Relay upload recovery markers: {error}"),
            )
        })?
        .filter_map(Result::ok)
        .filter(|entry| is_pending_upload_name(&entry.file_name()))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    let mut interrupted = 0usize;
    for path in paths {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(CommandError::new(
                    "share_history_read_failed",
                    format!("cannot inspect a Relay upload recovery marker: {error}"),
                ))
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            preserve_uncertain_upload(directory, &path)?;
            interrupted = interrupted.saturating_add(1);
            continue;
        }
        let mut file = open_existing_private_file(&path, "Relay upload recovery marker")?;
        if !try_lock_file_exclusive(&file)? {
            continue;
        }
        if let Some(record) = read_record_from_open_file(&mut file, None)? {
            let source_metadata = file.metadata().map_err(|error| {
                CommandError::new(
                    "share_history_read_failed",
                    format!("cannot verify a staged Relay upload record: {error}"),
                )
            })?;
            if !path_still_matches(&path, &source_metadata)? {
                continue;
            }
            drop(file);
            publish_recovered_record(directory, &path, &record)?;
            continue;
        }
        let _ = read_upload_attempt_from_open_file(&mut file);
        let source_metadata = file.metadata().map_err(|error| {
            CommandError::new(
                "share_history_read_failed",
                format!("cannot verify a Relay upload recovery marker: {error}"),
            )
        })?;
        if !path_still_matches(&path, &source_metadata)? {
            continue;
        }
        drop(file);
        preserve_uncertain_upload(directory, &path)?;
        interrupted = interrupted.saturating_add(1);
    }
    Ok(interrupted)
}

fn path_still_matches(path: &Path, opened: &fs::Metadata) -> Result<bool, CommandError> {
    match fs::symlink_metadata(path) {
        Ok(current) => Ok(!current.file_type().is_symlink()
            && current.is_file()
            && same_file(&current, opened)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(CommandError::new(
            "share_history_read_failed",
            format!("cannot re-check a Relay recovery file: {error}"),
        )),
    }
}

fn is_pending_upload_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with(".upload-attempt.") && name.ends_with(".pending"))
}

fn read_upload_attempt_from_open_file(file: &mut File) -> Option<StoredUploadAttempt> {
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_RECORD_BYTES as u64 {
        return None;
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    Read::by_ref(file)
        .take((MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    let stored: StoredUploadAttempt = serde_json::from_slice(&bytes).ok()?;
    if stored.schema != UPLOAD_ATTEMPT_SCHEMA
        || !Uuid::parse_str(&stored.attempt_id)
            .ok()
            .is_some_and(|id| id.to_string() == stored.attempt_id)
        || chrono::DateTime::parse_from_rfc3339(&stored.started_at).is_err()
    {
        return None;
    }
    Some(stored)
}

fn preserve_uncertain_upload(directory: &Path, source: &Path) -> Result<(), CommandError> {
    let id = source
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .and_then(|name| name.strip_prefix(".upload-attempt."))
        .and_then(|name| name.strip_suffix(".pending"))
        .filter(|id| Uuid::parse_str(id).is_ok())
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let target = directory.join(format!(".upload-attempt.{id}.uncertain"));
    let target = if fs::symlink_metadata(&target).is_err() {
        target
    } else {
        directory.join(format!(".upload-attempt.{}.uncertain", Uuid::new_v4()))
    };
    rename_exclusive(source, &target).map_err(|error| {
        CommandError::new(
            "share_history_write_failed",
            format!("cannot preserve an interrupted Relay upload marker: {error}"),
        )
    })?;
    if fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.is_file()) {
        protect_regular_file(&target)?;
    }
    sync_directory(directory)?;
    Ok(())
}

fn scan_records(directory: &Path) -> Result<(Vec<ScannedRecord>, usize, usize), CommandError> {
    let entries = fs::read_dir(directory).map_err(|error| {
        CommandError::new(
            "share_history_read_failed",
            format!("cannot list Relay share history: {error}"),
        )
    })?;
    let mut records = Vec::new();
    let mut record_file_count = 0usize;
    let mut isolated_count = 0usize;
    for entry in entries {
        let entry = entry.map_err(|error| {
            CommandError::new(
                "share_history_read_failed",
                format!("cannot inspect a Relay share history entry: {error}"),
            )
        })?;
        let Some(share_id) = share_id_from_filename(&entry.file_name()) else {
            continue;
        };
        let path = entry.path();
        match read_record_file(&path, &share_id)? {
            Some(record) => {
                record_file_count = record_file_count.saturating_add(1);
                records.push(ScannedRecord { path, record });
            }
            None => {
                if quarantine_record(directory, &path, &share_id)? {
                    isolated_count = isolated_count.saturating_add(1);
                } else if let Some(record) = read_record_file(&path, &share_id)? {
                    record_file_count = record_file_count.saturating_add(1);
                    records.push(ScannedRecord { path, record });
                }
            }
        }
    }
    Ok((records, record_file_count, isolated_count))
}

fn share_id_from_filename(name: &std::ffi::OsStr) -> Option<String> {
    let name = name.to_str()?;
    let share_id = name.strip_suffix(".json")?;
    valid_capability(share_id, SHARE_ID_LENGTH).then(|| share_id.to_owned())
}

fn read_record_file(
    path: &Path,
    expected_share_id: &str,
) -> Result<Option<StoredShareRecord>, CommandError> {
    read_record_file_with_expected_id(path, Some(expected_share_id))
}

fn read_record_file_with_expected_id(
    path: &Path,
    expected_share_id: Option<&str>,
) -> Result<Option<StoredShareRecord>, CommandError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(None),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_RECORD_BYTES as u64
    {
        return Ok(None);
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let Ok(opened_metadata) = file.metadata() else {
        return Ok(None);
    };
    if !same_file(&metadata, &opened_metadata) {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .is_err()
        {
            return Ok(None);
        }
    }

    let mut bytes = Vec::new();
    if Read::by_ref(&mut file)
        .take((MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.is_empty()
        || bytes.len() > MAX_RECORD_BYTES
    {
        return Ok(None);
    }
    let stored: StoredShareFile = match serde_json::from_slice(&bytes) {
        Ok(stored) => stored,
        Err(_) => return Ok(None),
    };
    if stored.schema != RECORD_SCHEMA
        || expected_share_id.is_some_and(|expected| stored.record.share_id != expected)
        || validate_record(&stored.record).is_err()
    {
        return Ok(None);
    }
    Ok(Some(stored.record))
}

fn read_required_record(
    directory: &Path,
    path: &Path,
    share_id: &str,
) -> Result<StoredShareRecord, CommandError> {
    match read_record_file(path, share_id)? {
        Some(record) => Ok(record),
        None if fs::symlink_metadata(path).is_err() => Err(history_not_found()),
        None => {
            if quarantine_record(directory, path, share_id)? {
                Err(CommandError::new(
                    "share_history_corrupt",
                    "the requested Relay share record is invalid and was isolated",
                ))
            } else {
                read_record_file(path, share_id)?.ok_or_else(history_not_found)
            }
        }
    }
}

fn quarantine_record(directory: &Path, path: &Path, share_id: &str) -> Result<bool, CommandError> {
    if read_record_file(path, share_id)?.is_some() {
        return Ok(false);
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(CommandError::new(
                "share_history_quarantine_failed",
                format!("cannot inspect an invalid Relay share record: {error}"),
            ))
        }
    };
    let corrupt_directory = directory.join(CORRUPT_DIRECTORY);
    ensure_private_directory(&corrupt_directory, "Relay corrupt history")?;
    let target = corrupt_directory.join(format!("{share_id}.{}.corrupt", Uuid::new_v4()));
    rename_exclusive(path, &target).map_err(|error| {
        CommandError::new(
            "share_history_quarantine_failed",
            format!("cannot isolate an invalid Relay share record: {error}"),
        )
    })?;
    if read_record_file(&target, share_id)?.is_some() {
        match rename_exclusive(&target, path) {
            Ok(()) => {
                sync_directory(directory)?;
                sync_directory(&corrupt_directory)?;
                return Ok(false);
            }
            Err(error) => {
                return Err(CommandError::new(
                    "share_history_quarantine_failed",
                    format!(
                        "a valid Relay share record changed during isolation and could not be restored: {error}"
                    ),
                ))
            }
        }
    }
    if metadata.is_file() {
        protect_regular_file(&target)?;
    }
    sync_directory(directory)?;
    sync_directory(&corrupt_directory)?;
    Ok(true)
}

#[cfg(unix)]
fn same_file(first: &fs::Metadata, second: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    first.dev() == second.dev() && first.ino() == second.ino()
}

#[cfg(unix)]
fn protect_regular_file(path: &Path) -> Result<(), CommandError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let expected = fs::symlink_metadata(path).map_err(|error| {
        CommandError::new(
            "share_history_quarantine_failed",
            format!("cannot inspect an isolated Relay share record: {error}"),
        )
    })?;
    if expected.file_type().is_symlink() || !expected.is_file() {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            "an isolated Relay share record is not an ordinary file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path).map_err(|error| {
        CommandError::new(
            "share_history_quarantine_failed",
            format!("cannot open an isolated Relay share record safely: {error}"),
        )
    })?;
    let opened = file.metadata().map_err(|error| {
        CommandError::new(
            "share_history_quarantine_failed",
            format!("cannot verify an isolated Relay share record: {error}"),
        )
    })?;
    if !same_file(&expected, &opened) {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            "an isolated Relay share record changed while it was being checked",
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| {
            CommandError::new(
                "share_history_quarantine_failed",
                format!("cannot protect an isolated Relay share record: {error}"),
            )
        })
}

#[cfg(not(unix))]
fn protect_regular_file(_path: &Path) -> Result<(), CommandError> {
    Ok(())
}

#[cfg(not(unix))]
fn same_file(first: &fs::Metadata, second: &fs::Metadata) -> bool {
    first.len() == second.len() && first.is_file() == second.is_file()
}

fn write_new_record(
    directory: &Path,
    target: &Path,
    record: &StoredShareRecord,
    attempt: &mut UploadAttempt,
) -> Result<(), CommandError> {
    if fs::symlink_metadata(target).is_ok() {
        return Err(CommandError::new(
            "share_history_conflict",
            "a Relay share record with this ID already exists",
        ));
    }
    let temporary = write_temporary_record(directory, record, Some(&attempt.id))?;
    if let Err(error) = verify_upload_attempt(directory, attempt) {
        drop(temporary.file);
        let _ = fs::remove_file(&temporary.path);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary.path, &attempt.path) {
        drop(temporary.file);
        let _ = fs::remove_file(&temporary.path);
        return Err(CommandError::new(
            "share_history_write_failed",
            format!("cannot stage Relay upload credentials for recovery: {error}"),
        ));
    }
    let previous = std::mem::replace(&mut attempt.file, temporary.file);
    drop(previous);
    match rename_exclusive(&attempt.path, target) {
        Ok(()) => {}
        Err(error) => {
            if error.kind() == ErrorKind::AlreadyExists || fs::symlink_metadata(target).is_ok() {
                return Err(CommandError::new(
                    "share_history_conflict",
                    "a Relay share record with this ID already exists",
                ));
            }
            return Err(CommandError::new(
                "share_history_write_failed",
                format!("cannot publish a Relay share record: {error}"),
            ));
        }
    }
    if let Err(error) = sync_directory(directory) {
        if fs::rename(target, &attempt.path).is_ok() {
            let _ = sync_directory(directory);
        }
        return Err(error);
    }
    Ok(())
}

fn replace_record_state(
    directory: &Path,
    target: &Path,
    record: &StoredShareRecord,
) -> Result<(), CommandError> {
    let metadata = fs::symlink_metadata(target).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            history_not_found()
        } else {
            CommandError::new(
                "share_history_read_failed",
                format!("cannot inspect the Relay share record before updating it: {error}"),
            )
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CommandError::new(
            "share_history_unsafe_path",
            "the Relay share record is not an ordinary file",
        ));
    }
    let temporary = write_temporary_record(directory, record, None)?;
    if let Err(error) = fs::rename(&temporary.path, target) {
        drop(temporary.file);
        let _ = fs::remove_file(&temporary.path);
        return Err(CommandError::new(
            "share_history_write_failed",
            format!("cannot save the Relay share state: {error}"),
        ));
    }
    if let Err(error) = sync_directory(directory) {
        return Err(error.with_details(serde_json::json!({
            "share_state_may_have_been_saved": true,
        })));
    }
    Ok(())
}

fn write_temporary_record(
    directory: &Path,
    record: &StoredShareRecord,
    attempt_id: Option<&str>,
) -> Result<TemporaryRecord, CommandError> {
    validate_record(record)?;
    let stored = StoredShareFile {
        schema: RECORD_SCHEMA.to_owned(),
        record: record.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&stored).map_err(|error| {
        CommandError::new(
            "share_history_write_failed",
            format!("cannot encode a Relay share record: {error}"),
        )
    })?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(CommandError::new(
            "share_history_write_failed",
            "a Relay share record exceeds its size limit",
        ));
    }

    let id = attempt_id
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let temporary = directory.join(format!(".record.{id}.tmp"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temporary).map_err(|error| {
        CommandError::new(
            "share_history_write_failed",
            format!("cannot create a private Relay share record: {error}"),
        )
    })?;
    if let Err(error) = lock_file_exclusive(&file) {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    let write_result = (|| {
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(CommandError::new(
            "share_history_write_failed",
            format!("cannot save a Relay share record: {error}"),
        ));
    }
    Ok(TemporaryRecord {
        path: temporary,
        file,
    })
}

#[cfg(target_os = "macos")]
fn rename_exclusive(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "source path contains NUL"))?;
    let target = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "target path contains NUL"))?;
    // SAFETY: both C strings are NUL-terminated and remain alive for this call.
    let result = unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn rename_exclusive(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::hard_link(source, target)?;
    fs::remove_file(source)
}

fn sync_directory(directory: &Path) -> Result<(), CommandError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    }
    let file = options.open(directory).map_err(|error| {
        CommandError::new(
            "share_history_write_failed",
            format!("cannot open a Relay history directory for syncing: {error}"),
        )
    })?;
    file.sync_all().map_err(|error| {
        CommandError::new(
            "share_history_write_failed",
            format!("cannot sync a Relay history directory: {error}"),
        )
    })
}

fn ensure_capacity(directory: &Path) -> Result<(), CommandError> {
    let (mut records, mut count, _) = scan_records(directory)?;
    if count < MAX_HISTORY_RECORDS {
        return Ok(());
    }
    records.sort_by(|left, right| left.record.created_at.cmp(&right.record.created_at));
    let mut removed_any = false;
    for entry in records {
        if count < MAX_HISTORY_RECORDS {
            break;
        }
        if entry.record.status != ShareHistoryStatus::Revoked
            && !is_expired(&entry.record.expires_at)
        {
            continue;
        }
        if read_record_file(&entry.path, &entry.record.share_id)?.is_some_and(|current| {
            current.status == ShareHistoryStatus::Revoked || is_expired(&current.expires_at)
        }) && fs::remove_file(&entry.path).is_ok()
        {
            count = count.saturating_sub(1);
            removed_any = true;
        }
    }
    if removed_any {
        sync_directory(directory)?;
    }
    if count >= MAX_HISTORY_RECORDS {
        return Err(CommandError::new(
            "share_history_full",
            "Relay already has 500 share record files; revoke, remove, or wait for one to expire",
        ));
    }
    Ok(())
}

fn validate_record(record: &StoredShareRecord) -> Result<(), CommandError> {
    if !valid_capability(&record.share_id, SHARE_ID_LENGTH)
        || !valid_capability(&record.revoke_token, CAPABILITY_LENGTH)
        || record
            .upload_token
            .as_deref()
            .is_some_and(|token| !valid_capability(token, CAPABILITY_LENGTH))
    {
        return corrupt_record("contains an invalid share ID or revoke token");
    }
    if record.ciphertext_sha256.len() != SHA256_HEX_LENGTH
        || !record
            .ciphertext_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || record.ciphertext_bytes == 0
        || record.ciphertext_bytes > MAX_CIPHERTEXT_BYTES
    {
        return corrupt_record("contains invalid ciphertext metadata");
    }
    let created_at = chrono::DateTime::parse_from_rfc3339(&record.created_at)
        .map_err(|_| corrupt_record_error("contains an invalid creation time"))?;
    let expires_at = chrono::DateTime::parse_from_rfc3339(&record.expires_at)
        .map_err(|_| corrupt_record_error("contains an invalid expiration time"))?;
    if expires_at <= created_at {
        return corrupt_record("contains an expiration time before its creation time");
    }
    match (record.status, record.revoked_at.as_deref()) {
        (ShareHistoryStatus::PendingUpload, None) if record.upload_token.is_some() => {}
        (ShareHistoryStatus::Active, None) if record.upload_token.is_none() => {}
        (ShareHistoryStatus::Revoked, Some(value)) => {
            if record.upload_token.is_some() {
                return corrupt_record("contains an upload token after revocation");
            }
            chrono::DateTime::parse_from_rfc3339(value)
                .map_err(|_| corrupt_record_error("contains an invalid revocation time"))?;
        }
        _ => return corrupt_record("contains an inconsistent revocation state"),
    }
    if record.package_path.len() > 8192
        || record.package_path.chars().any(char::is_control)
        || !Path::new(&record.package_path).is_absolute()
    {
        return corrupt_record("contains an invalid package path");
    }
    validate_optional_label(record.project_title.as_deref())?;
    validate_optional_label(record.project_name.as_deref())?;

    let canonical_base = canonical_service_base_url(&record.service_base_url)
        .map_err(|_| corrupt_record_error("contains an invalid service URL"))?;
    if canonical_base != record.service_base_url {
        return corrupt_record("contains a non-canonical service URL");
    }
    validate_share_url(&record.share_url, &record.share_id, &canonical_base)?;
    Ok(())
}

fn validate_share_url(raw: &str, share_id: &str, base: &str) -> Result<(), CommandError> {
    let url = Url::parse(raw).map_err(|_| corrupt_record_error("contains an invalid share URL"))?;
    let origin =
        Url::parse(base).map_err(|_| corrupt_record_error("contains an invalid service URL"))?;
    if url.scheme() != origin.scheme()
        || url.host_str() != origin.host_str()
        || url.port_or_known_default() != origin.port_or_known_default()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.path() != format!("/s/v1/{share_id}")
    {
        return corrupt_record("contains a share URL outside its recorded service");
    }
    let key = url.fragment().and_then(|value| value.strip_prefix("k="));
    if !key.is_some_and(|value| valid_capability(value, CAPABILITY_LENGTH)) {
        return corrupt_record("contains an invalid share-link key");
    }
    Ok(())
}

fn canonical_service_base_url(raw: &str) -> Result<String, CommandError> {
    let mut url = Url::parse(raw.trim()).map_err(|_| {
        CommandError::new(
            "invalid_share_service",
            "share service must be an absolute HTTPS origin",
        )
    })?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(CommandError::new(
            "invalid_share_service",
            "share service must contain only scheme, host, and optional port",
        ));
    }
    if url.scheme() != "https" && !(url.scheme() == "http" && is_loopback_host(&url)) {
        return Err(CommandError::new(
            "invalid_share_service",
            "share service must use HTTPS; HTTP is allowed only for loopback development",
        ));
    }
    url.set_path("/");
    Ok(url.into())
}

fn is_loopback_host(url: &Url) -> bool {
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback()),
        None => false,
    }
}

fn valid_capability(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn canonical_package_path(raw: &str) -> Result<String, CommandError> {
    let canonical = fs::canonicalize(raw.trim()).map_err(|error| {
        CommandError::new(
            "share_history_write_failed",
            format!("cannot resolve the shared Relay package: {error}"),
        )
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| {
        CommandError::new(
            "share_history_write_failed",
            format!("cannot inspect the shared Relay package: {error}"),
        )
    })?;
    if !metadata.is_file() {
        return Err(CommandError::new(
            "share_history_write_failed",
            "the shared Relay package is not an ordinary file",
        ));
    }
    let value = canonical.to_string_lossy().into_owned();
    if value.len() > 8192 || value.chars().any(char::is_control) {
        return Err(CommandError::new(
            "share_history_write_failed",
            "the shared Relay package path is too long or contains invalid characters",
        ));
    }
    Ok(value)
}

fn normalize_label(value: Option<&str>) -> Option<String> {
    let normalized = value?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect::<String>();
    (!normalized.is_empty()).then_some(normalized)
}

fn validate_optional_label(value: Option<&str>) -> Result<(), CommandError> {
    if value.is_some_and(|label| {
        label.is_empty() || label.len() > 1024 || label.chars().any(char::is_control)
    }) {
        return corrupt_record("contains an invalid project label");
    }
    Ok(())
}

fn is_expired(value: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(value)
        .is_ok_and(|time| time.with_timezone(&Utc) <= Utc::now())
}

fn public_record(record: &StoredShareRecord) -> ShareHistoryRecord {
    ShareHistoryRecord {
        share_id: record.share_id.clone(),
        share_url: record.share_url.clone(),
        service_base_url: record.service_base_url.clone(),
        created_at: record.created_at.clone(),
        expires_at: record.expires_at.clone(),
        package_exists: Path::new(&record.package_path).is_file(),
        package_path: record.package_path.clone(),
        project_title: record.project_title.clone(),
        project_name: record.project_name.clone(),
        ciphertext_sha256: record.ciphertext_sha256.clone(),
        ciphertext_bytes: record.ciphertext_bytes,
        status: record.status,
        revoked_at: record.revoked_at.clone(),
    }
}

fn corrupt_record<T>(message: &str) -> Result<T, CommandError> {
    Err(corrupt_record_error(message))
}

fn corrupt_record_error(message: &str) -> CommandError {
    CommandError::new(
        "share_history_corrupt",
        format!("Relay share history {message}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn request(package_path: &Path) -> UploadShareRequest {
        UploadShareRequest {
            package_path: package_path.to_string_lossy().into_owned(),
            key: "k".repeat(CAPABILITY_LENGTH),
            service_base_url: "https://share.example".into(),
            project_title: Some("  Repair   login flow  ".into()),
            project_name: Some("Relay".into()),
            expires_in_seconds: Some(3600),
            upload_token: None,
        }
    }

    fn result(id_byte: char) -> UploadShareResult {
        let id = id_byte.to_string().repeat(SHARE_ID_LENGTH);
        let key = "k".repeat(CAPABILITY_LENGTH);
        UploadShareResult {
            share_id: id.clone(),
            share_url: format!("https://share.example/s/v1/{id}#k={key}"),
            expires_at: "2099-01-01T00:00:00Z".into(),
            revoke_token: "r".repeat(CAPABILITY_LENGTH),
            upload_token: "u".repeat(CAPABILITY_LENGTH),
            ciphertext_sha256: "a".repeat(SHA256_HEX_LENGTH),
            ciphertext_bytes: 128,
        }
    }

    fn stored_record(share_id: String, status: ShareHistoryStatus) -> StoredShareRecord {
        StoredShareRecord {
            share_url: format!(
                "https://share.example/s/v1/{share_id}#k={}",
                "k".repeat(CAPABILITY_LENGTH)
            ),
            share_id,
            service_base_url: "https://share.example/".into(),
            revoke_token: "r".repeat(CAPABILITY_LENGTH),
            upload_token: (status == ShareHistoryStatus::PendingUpload)
                .then(|| "u".repeat(CAPABILITY_LENGTH)),
            created_at: "2026-01-01T00:00:00Z".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            package_path: "/tmp/sample.relaypack".into(),
            project_title: None,
            project_name: Some("Relay".into()),
            ciphertext_sha256: "a".repeat(SHA256_HEX_LENGTH),
            ciphertext_bytes: 128,
            status,
            revoked_at: (status == ShareHistoryStatus::Revoked)
                .then(|| "2026-01-02T00:00:00Z".into()),
        }
    }

    fn write_fixture(directory: &Path, record: StoredShareRecord) {
        let path = record_path(directory, &record.share_id).unwrap();
        let stored = StoredShareFile {
            schema: RECORD_SCHEMA.into(),
            record,
        };
        fs::write(path, serde_json::to_vec(&stored).unwrap()).unwrap();
    }

    fn save_upload(
        app_data: &Path,
        request: &UploadShareRequest,
        result: &UploadShareResult,
    ) -> Result<ShareHistoryRecord, CommandError> {
        let mut attempt = prepare_for_upload(app_data)?;
        match record_upload(app_data, request, result, &mut attempt) {
            Ok(record) => Ok(record),
            Err(error) => {
                cancel_upload_attempt(app_data, &attempt)?;
                Err(error)
            }
        }
    }

    fn save_pending_upload(
        app_data: &Path,
        request: &UploadShareRequest,
        result: &UploadShareResult,
    ) -> ShareHistoryRecord {
        let mut attempt = prepare_for_upload(app_data).unwrap();
        record_pending_upload(app_data, request, result, &mut attempt).unwrap()
    }

    #[test]
    fn stores_one_private_file_per_share_and_never_returns_the_revoke_token() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let package = temp.path().join("sample.relaypack");
        fs::write(&package, b"ciphertext").unwrap();

        let public = save_upload(&app_data, &request(&package), &result('A')).unwrap();
        assert_eq!(public.project_title.as_deref(), Some("Repair login flow"));
        let directory = history_directory(&app_data);
        let path = record_path(&directory, &"A".repeat(SHARE_ID_LENGTH)).unwrap();
        assert!(path.is_file());
        assert_eq!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                .count(),
            0
        );

        let listed = list_history(&app_data).unwrap();
        let public_json = serde_json::to_string(&listed.records).unwrap();
        let upload_json = serde_json::to_string(&result('A')).unwrap();
        assert!(!public_json.contains(&"r".repeat(CAPABILITY_LENGTH)));
        assert!(!upload_json.contains("revoke_token"));
        assert!(!upload_json.contains(&"r".repeat(CAPABILITY_LENGTH)));
        assert!(!upload_json.contains("upload_token"));
        assert!(!upload_json.contains(&"u".repeat(CAPABILITY_LENGTH)));
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains(&"r".repeat(CAPABILITY_LENGTH)));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(directory.join(CORRUPT_DIRECTORY))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn pending_upload_keeps_private_capabilities_and_activation_erases_upload_token() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let package = temp.path().join("sample.relaypack");
        fs::write(&package, b"ciphertext").unwrap();
        let pending = save_pending_upload(&app_data, &request(&package), &result('P'));
        assert_eq!(pending.status, ShareHistoryStatus::PendingUpload);

        let directory = history_directory(&app_data);
        let path = record_path(&directory, &"P".repeat(SHARE_ID_LENGTH)).unwrap();
        let pending_json = fs::read_to_string(&path).unwrap();
        assert!(pending_json.contains(&"u".repeat(CAPABILITY_LENGTH)));
        assert!(pending_json.contains(&"r".repeat(CAPABILITY_LENGTH)));
        let public_json = serde_json::to_string(&pending).unwrap();
        assert!(!public_json.contains(&"u".repeat(CAPABILITY_LENGTH)));
        assert!(!public_json.contains(&"r".repeat(CAPABILITY_LENGTH)));

        let credentials =
            pending_upload_credentials(&app_data, &"P".repeat(SHARE_ID_LENGTH)).unwrap();
        assert_eq!(credentials.upload_token, "u".repeat(CAPABILITY_LENGTH));
        let active = mark_upload_active(&app_data, &credentials.share_id).unwrap();
        assert_eq!(active.status, ShareHistoryStatus::Active);
        let active_json = fs::read_to_string(path).unwrap();
        assert!(!active_json.contains("upload_token"));
        assert!(!active_json.contains(&"u".repeat(CAPABILITY_LENGTH)));
        assert_eq!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.ends_with(".pending") || name.ends_with(".tmp")
                })
                .count(),
            0
        );
    }

    #[test]
    fn pending_upload_can_be_revoked_and_upload_token_is_erased() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let package = temp.path().join("sample.relaypack");
        fs::write(&package, b"ciphertext").unwrap();
        save_pending_upload(&app_data, &request(&package), &result('Q'));

        let share_id = "Q".repeat(SHARE_ID_LENGTH);
        let revoked = mark_revoked(&app_data, &share_id).unwrap();
        assert_eq!(revoked.status, ShareHistoryStatus::Revoked);
        let path = record_path(&history_directory(&app_data), &share_id).unwrap();
        let stored = fs::read_to_string(path).unwrap();
        assert!(!stored.contains("upload_token"));
        assert!(!stored.contains(&"u".repeat(CAPABILITY_LENGTH)));
    }

    #[test]
    fn interrupted_pending_to_active_update_is_recovered_without_upload_token() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let package = temp.path().join("sample.relaypack");
        fs::write(&package, b"ciphertext").unwrap();
        let share_id = "T".repeat(SHARE_ID_LENGTH);
        save_pending_upload(&app_data, &request(&package), &result('T'));
        let directory = history_directory(&app_data);
        let path = record_path(&directory, &share_id).unwrap();
        let mut staged = read_record_file(&path, &share_id).unwrap().unwrap();
        staged.status = ShareHistoryStatus::Active;
        staged.upload_token = None;
        let temporary = write_temporary_record(&directory, &staged, None).unwrap();
        drop(temporary);

        let listed = list_history(&app_data).unwrap();
        assert_eq!(listed.records.len(), 1);
        assert_eq!(listed.records[0].status, ShareHistoryStatus::Active);
        let stored = fs::read_to_string(path).unwrap();
        assert!(!stored.contains("upload_token"));
        assert!(!stored.contains(&"u".repeat(CAPABILITY_LENGTH)));
    }

    #[test]
    fn a_corrupt_record_does_not_hide_other_share_records() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let package = temp.path().join("sample.relaypack");
        fs::write(&package, b"ciphertext").unwrap();
        save_upload(&app_data, &request(&package), &result('A')).unwrap();
        save_upload(&app_data, &request(&package), &result('B')).unwrap();

        let directory = history_directory(&app_data);
        let first = record_path(&directory, &"A".repeat(SHARE_ID_LENGTH)).unwrap();
        fs::write(&first, b"not json").unwrap();
        let listed = list_history(&app_data).unwrap();
        assert_eq!(listed.records.len(), 1);
        assert_eq!(listed.records[0].share_id, "B".repeat(SHARE_ID_LENGTH));
        assert!(!first.exists());
        assert_eq!(
            fs::read_dir(directory.join(CORRUPT_DIRECTORY))
                .unwrap()
                .count(),
            1
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let isolated = fs::read_dir(directory.join(CORRUPT_DIRECTORY))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            assert_eq!(
                fs::metadata(isolated).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            revoke_credentials(&app_data, &"B".repeat(SHARE_ID_LENGTH))
                .unwrap()
                .revoke_token,
            "r".repeat(CAPABILITY_LENGTH)
        );
    }

    #[test]
    fn legacy_single_file_is_preserved_without_blocking_new_history() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        fs::create_dir(&app_data).unwrap();
        fs::write(app_data.join(LEGACY_HISTORY_FILENAME), b"legacy sentinel").unwrap();

        assert!(list_history(&app_data).unwrap().records.is_empty());
        assert!(!app_data.join(LEGACY_HISTORY_FILENAME).exists());
        let backup = history_directory(&app_data).join(LEGACY_BACKUP_FILENAME);
        assert_eq!(fs::read(&backup).unwrap(), b"legacy sentinel");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(backup).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn revoked_state_is_saved_in_only_that_share_file() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let package = temp.path().join("sample.relaypack");
        fs::write(&package, b"ciphertext").unwrap();
        save_upload(&app_data, &request(&package), &result('A')).unwrap();
        save_upload(&app_data, &request(&package), &result('B')).unwrap();
        let directory = history_directory(&app_data);
        let untouched = record_path(&directory, &"B".repeat(SHARE_ID_LENGTH)).unwrap();
        let untouched_before = fs::read(&untouched).unwrap();

        let revoked = mark_revoked(&app_data, &"A".repeat(SHARE_ID_LENGTH)).unwrap();
        assert_eq!(revoked.status, ShareHistoryStatus::Revoked);
        assert!(revoked.revoked_at.is_some());
        assert_eq!(fs::read(&untouched).unwrap(), untouched_before);
        assert!(
            revoke_credentials(&app_data, &"A".repeat(SHARE_ID_LENGTH))
                .unwrap()
                .already_revoked
        );
    }

    #[test]
    fn capacity_removes_revoked_files_and_blocks_a_full_active_directory() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let directory = ensure_storage_directory(&app_data).unwrap();
        for index in 0..MAX_HISTORY_RECORDS {
            let status = if index == 0 {
                ShareHistoryStatus::Revoked
            } else {
                ShareHistoryStatus::Active
            };
            write_fixture(&directory, stored_record(format!("{index:032}"), status));
        }

        ensure_capacity(&directory).unwrap();
        assert_eq!(scan_records(&directory).unwrap().1, MAX_HISTORY_RECORDS - 1);
        write_fixture(
            &directory,
            stored_record(
                format!("{:032}", MAX_HISTORY_RECORDS),
                ShareHistoryStatus::Active,
            ),
        );
        let error = ensure_capacity(&directory).unwrap_err();
        assert_eq!(error.code, "share_history_full");
        assert_eq!(scan_records(&directory).unwrap().1, MAX_HISTORY_RECORDS);
    }

    #[test]
    fn existing_record_is_never_truncated_or_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let package = temp.path().join("sample.relaypack");
        fs::write(&package, b"ciphertext").unwrap();
        let directory = ensure_storage_directory(&app_data).unwrap();
        let target = record_path(&directory, &"A".repeat(SHARE_ID_LENGTH)).unwrap();
        fs::write(&target, b"sentinel").unwrap();

        let mut attempt = create_upload_attempt(&directory).unwrap();
        let error =
            record_upload(&app_data, &request(&package), &result('A'), &mut attempt).unwrap_err();
        assert_eq!(error.code, "share_history_conflict");
        assert_eq!(fs::read(&target).unwrap(), b"sentinel");
        assert!(!error.message.contains(&"r".repeat(CAPABILITY_LENGTH)));
        cancel_upload_attempt(&app_data, &attempt).unwrap();
    }

    #[test]
    fn concurrent_creators_publish_exactly_one_record() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let package = temp.path().join("sample.relaypack");
        fs::write(&package, b"ciphertext").unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let app_data = app_data.clone();
            let package = package.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                save_upload(&app_data, &request(&package), &result('A'))
            }));
        }
        barrier.wait();
        let outcomes: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter_map(|result| result.as_ref().err())
                .filter(|error| error.code == "share_history_conflict")
                .count(),
            1
        );
        assert_eq!(list_history(&app_data).unwrap().records.len(), 1);
    }

    #[test]
    fn concurrent_distinct_records_are_all_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let package = temp.path().join("sample.relaypack");
        fs::write(&package, b"ciphertext").unwrap();
        let barrier = Arc::new(Barrier::new(9));
        let mut threads = Vec::new();
        for id_byte in ['A', 'B', 'C', 'D', 'E', 'F', 'G', 'H'] {
            let app_data = app_data.clone();
            let package = package.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                save_upload(&app_data, &request(&package), &result(id_byte))
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        assert_eq!(list_history(&app_data).unwrap().records.len(), 8);
    }

    #[test]
    fn stale_valid_temporary_record_is_recovered() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let directory = ensure_storage_directory(&app_data).unwrap();
        let record = stored_record("A".repeat(SHARE_ID_LENGTH), ShareHistoryStatus::Active);
        let temporary = write_temporary_record(&directory, &record, None).unwrap();
        let temporary_path = temporary.path.clone();
        drop(temporary);

        let listed = list_history(&app_data).unwrap();
        assert_eq!(listed.records.len(), 1);
        assert_eq!(listed.records[0].share_id, record.share_id);
        assert!(!temporary_path.exists());
    }

    #[test]
    fn stale_revocation_temporary_record_replaces_only_its_active_record() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let directory = ensure_storage_directory(&app_data).unwrap();
        let share_id = "A".repeat(SHARE_ID_LENGTH);
        write_fixture(
            &directory,
            stored_record(share_id.clone(), ShareHistoryStatus::Active),
        );
        let revoked = stored_record(share_id.clone(), ShareHistoryStatus::Revoked);
        let temporary = write_temporary_record(&directory, &revoked, None).unwrap();
        drop(temporary);

        let listed = list_history(&app_data).unwrap();
        assert_eq!(listed.records.len(), 1);
        assert_eq!(listed.records[0].status, ShareHistoryStatus::Revoked);
        assert!(listed.records[0].revoked_at.is_some());
    }

    #[test]
    fn conflicting_valid_temporary_record_is_preserved_for_manual_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let directory = ensure_storage_directory(&app_data).unwrap();
        let share_id = "A".repeat(SHARE_ID_LENGTH);
        let current = stored_record(share_id.clone(), ShareHistoryStatus::Active);
        write_fixture(&directory, current.clone());
        let mut conflicting = current.clone();
        conflicting.project_name = Some("Different Relay project".into());
        let temporary = write_temporary_record(&directory, &conflicting, None).unwrap();
        let temporary_path = temporary.path.clone();
        drop(temporary);

        let error = list_history(&app_data).unwrap_err();
        assert_eq!(error.code, "share_history_conflict");
        assert!(!temporary_path.exists());
        assert_eq!(
            read_record_file(
                &record_path(&directory, &share_id).unwrap(),
                &current.share_id
            )
            .unwrap()
            .unwrap(),
            current
        );
        assert_eq!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".recovery"))
                .count(),
            1
        );
    }

    #[test]
    fn invalid_temporary_record_is_removed_on_the_next_history_open() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let directory = ensure_storage_directory(&app_data).unwrap();
        let path = directory.join(format!(".record.{}.tmp", Uuid::new_v4()));
        fs::write(&path, b"not a Relay record").unwrap();

        assert!(list_history(&app_data).unwrap().records.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn locked_live_temporary_record_is_not_cleaned_as_stale() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let directory = ensure_storage_directory(&app_data).unwrap();
        let record = stored_record("A".repeat(SHARE_ID_LENGTH), ShareHistoryStatus::Active);
        let temporary = write_temporary_record(&directory, &record, None).unwrap();
        let temporary_path = temporary.path.clone();

        assert!(list_history(&app_data).unwrap().records.is_empty());
        assert!(temporary_path.exists());
        drop(temporary);
        assert_eq!(list_history(&app_data).unwrap().records.len(), 1);
        assert!(!temporary_path.exists());
    }

    #[test]
    fn interrupted_upload_marker_is_reported_once_and_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let attempt = prepare_for_upload(&app_data).unwrap();
        let directory = history_directory(&app_data);
        let pending = attempt.path.clone();
        drop(attempt);

        let error = list_history(&app_data).unwrap_err();
        assert_eq!(error.code, "share_upload_state_uncertain");
        assert!(!pending.exists());
        assert_eq!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".uncertain"))
                .count(),
            1
        );
        assert!(list_history(&app_data).unwrap().records.is_empty());
    }

    #[test]
    fn live_upload_marker_is_ignored_until_it_is_cancelled() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let attempt = prepare_for_upload(&app_data).unwrap();

        assert!(list_history(&app_data).unwrap().records.is_empty());
        cancel_upload_attempt(&app_data, &attempt).unwrap();
        assert!(!attempt.path.exists());
    }

    #[test]
    fn staged_upload_credentials_are_recovered_after_interruption() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let attempt = prepare_for_upload(&app_data).unwrap();
        let directory = history_directory(&app_data);
        let record = stored_record("A".repeat(SHARE_ID_LENGTH), ShareHistoryStatus::Active);
        let temporary = write_temporary_record(&directory, &record, Some(&attempt.id)).unwrap();
        drop(temporary);
        drop(attempt);

        let listed = list_history(&app_data).unwrap();
        assert_eq!(listed.records.len(), 1);
        assert_eq!(listed.records[0].share_id, record.share_id);
        assert_eq!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.ends_with(".tmp") || name.ends_with(".pending")
                })
                .count(),
            0
        );
    }

    #[test]
    fn quarantine_rechecks_a_record_that_became_valid() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let directory = ensure_storage_directory(&app_data).unwrap();
        let record = stored_record("A".repeat(SHARE_ID_LENGTH), ShareHistoryStatus::Active);
        let path = record_path(&directory, &record.share_id).unwrap();
        fs::write(&path, b"invalid before a competing replacement").unwrap();
        write_fixture(&directory, record.clone());

        assert!(!quarantine_record(&directory, &path, &record.share_id).unwrap());
        assert_eq!(
            read_record_file(&path, &record.share_id)
                .unwrap()
                .unwrap()
                .share_id,
            record.share_id
        );
        assert_eq!(
            fs::read_dir(directory.join(CORRUPT_DIRECTORY))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn process_lock_is_private_and_excludes_a_second_open_file_description() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        ensure_private_directory(&app_data, "Relay app data").unwrap();
        let first = acquire_process_lock(&app_data).unwrap();
        let path = app_data.join(HISTORY_LOCK_FILENAME);
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .unwrap();

        assert!(!try_lock_file_exclusive(&second).unwrap());
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(first);
        assert!(try_lock_file_exclusive(&second).unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn history_directory_has_verified_time_machine_exclusion() {
        use std::os::unix::fs::OpenOptionsExt;

        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let directory = ensure_storage_directory(&app_data).unwrap();
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(directory)
            .unwrap();
        assert_eq!(
            backup_exclusion_value(&file).unwrap(),
            BACKUP_EXCLUSION_VALUE
        );
    }

    #[cfg(unix)]
    #[test]
    fn temporary_record_symlink_is_moved_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let outside = temp.path().join("outside");
        fs::write(&outside, b"outside sentinel").unwrap();
        let directory = ensure_storage_directory(&app_data).unwrap();
        let temporary = directory.join(format!(".record.{}.tmp", Uuid::new_v4()));
        symlink(&outside, &temporary).unwrap();

        assert!(list_history(&app_data).unwrap().records.is_empty());
        assert!(fs::symlink_metadata(&temporary).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside sentinel");
        assert_eq!(
            fs::read_dir(directory.join(CORRUPT_DIRECTORY))
                .unwrap()
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn process_lock_symlink_is_never_followed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let outside = temp.path().join("outside");
        fs::create_dir(&app_data).unwrap();
        fs::write(&outside, b"outside sentinel").unwrap();
        symlink(&outside, app_data.join(HISTORY_LOCK_FILENAME)).unwrap();

        let error = list_history(&app_data).unwrap_err();
        assert_eq!(error.code, "share_history_lock_failed");
        assert_eq!(fs::read(&outside).unwrap(), b"outside sentinel");
        assert!(!history_directory(&app_data).exists());
    }

    #[cfg(unix)]
    #[test]
    fn record_symlink_is_not_followed_or_overwritten() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        let package = temp.path().join("sample.relaypack");
        let outside = temp.path().join("outside");
        fs::write(&package, b"ciphertext").unwrap();
        fs::write(&outside, b"outside sentinel").unwrap();
        let directory = ensure_storage_directory(&app_data).unwrap();
        let target = record_path(&directory, &"A".repeat(SHARE_ID_LENGTH)).unwrap();
        symlink(&outside, &target).unwrap();

        let error = list_history(&app_data).unwrap_err();
        assert_eq!(error.code, "share_history_corrupt");
        assert!(!target.exists());
        assert_eq!(
            fs::read_dir(directory.join(CORRUPT_DIRECTORY))
                .unwrap()
                .count(),
            1
        );
        save_upload(&app_data, &request(&package), &result('A')).unwrap();
        assert_eq!(fs::read(&outside).unwrap(), b"outside sentinel");
    }
}
