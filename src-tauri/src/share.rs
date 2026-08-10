use crate::relaypack;
use crate::types::{
    CommandError, DownloadShareRequest, DownloadShareResult, RevokeShareRequest, RevokeShareResult,
    UploadShareRequest, UploadShareResult,
};
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

const MAX_CIPHERTEXT_BYTES: usize = 32 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 32 * 1024;
const SHARE_ID_LENGTH: usize = 32;
const KEY_LENGTH: usize = 43;

#[derive(Debug, Deserialize)]
struct CreatedShareResponse {
    schema: String,
    share_id: String,
    share_url: String,
    upload_url: String,
    metadata_url: String,
    expires_at: String,
    upload_token: String,
    revoke_token: String,
}

#[derive(Debug, Serialize)]
struct CreateShareReservationRequest<'a> {
    ciphertext_bytes: u64,
    ciphertext_sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_in_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PublicShareResponse {
    schema: String,
    status: String,
    ciphertext: PublicCiphertextMetadata,
}

#[derive(Debug, Deserialize)]
struct PublicCiphertextMetadata {
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SavedUploadCredentials {
    pub share_id: String,
    pub service_base_url: String,
    pub upload_token: String,
    pub package_path: String,
    pub ciphertext_sha256: String,
    pub ciphertext_bytes: u64,
}

struct ParsedShareLink {
    share_id: String,
    key: String,
    service_origin: Url,
}

pub fn reserve_share(request: &UploadShareRequest) -> Result<UploadShareResult, CommandError> {
    let origin = parse_service_origin(&request.service_base_url)?;
    let key = validate_key(&request.key)?;
    let inspected = relaypack::inspect_relaypack(&request.package_path, &key)?;
    if inspected.ciphertext_bytes == 0 || inspected.ciphertext_bytes > MAX_CIPHERTEXT_BYTES as u64 {
        return Err(CommandError::new(
            "share_package_too_large",
            "encrypted Relay package exceeds the 32 MiB sharing limit",
        ));
    }
    let package_path = canonical_regular_file(Path::new(&request.package_path), "Relay package")?;
    let package_bytes = read_file_limited(&package_path, MAX_CIPHERTEXT_BYTES)?;
    if sha256_hex(&package_bytes) != inspected.ciphertext_sha256 {
        return Err(CommandError::new(
            "share_package_changed",
            "Relay package changed after it was inspected",
        ));
    }

    let endpoint = origin.join("/v1/shares").map_err(|_| {
        CommandError::new("invalid_share_service", "cannot build the share upload URL")
    })?;
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(token) = request
        .upload_token
        .as_deref()
        .filter(|token| !token.is_empty())
    {
        let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
            CommandError::new(
                "invalid_upload_token",
                "service upload token contains invalid characters",
            )
        })?;
        headers.insert(AUTHORIZATION, value);
    }

    let body = CreateShareReservationRequest {
        ciphertext_bytes: inspected.ciphertext_bytes,
        ciphertext_sha256: &inspected.ciphertext_sha256,
        expires_in_seconds: request.expires_in_seconds,
    };

    let response = http_client()?
        .post(endpoint)
        .headers(headers)
        .json(&body)
        .send()
        .map_err(|error| recode_error(map_http_error(error), "share_reservation_failed"))?;
    if response.status().as_u16() != 201 {
        return Err(redact_error_secrets(
            response_error("share_reservation_failed", response),
            request.upload_token.as_deref().into_iter(),
        ));
    }
    let bytes = read_response_limited(response, MAX_ERROR_BODY_BYTES)?;
    let created: CreatedShareResponse = serde_json::from_slice(&bytes).map_err(|_| {
        CommandError::new(
            "share_protocol_error",
            "share service returned an invalid creation response",
        )
    })?;
    if created.schema != "relay.share.created.v1"
        || !valid_share_id(&created.share_id)
        || !valid_capability(&created.upload_token, KEY_LENGTH)
        || !valid_capability(&created.revoke_token, KEY_LENGTH)
        || chrono::DateTime::parse_from_rfc3339(&created.expires_at).is_err()
    {
        return Err(CommandError::new(
            "share_protocol_error",
            "share service returned invalid identifiers",
        ));
    }
    let returned_url = Url::parse(&created.share_url).map_err(|_| {
        CommandError::new(
            "share_protocol_error",
            "share service returned an invalid share URL",
        )
    })?;
    ensure_same_origin(&origin, &returned_url)?;
    validate_service_endpoint(
        &returned_url,
        &origin,
        &format!("/s/v1/{}", created.share_id),
    )?;
    validate_service_endpoint(
        &Url::parse(&created.upload_url).map_err(|_| {
            CommandError::new(
                "share_protocol_error",
                "share service returned an invalid upload URL",
            )
        })?,
        &origin,
        &format!("/v1/shares/{}/blob", created.share_id),
    )?;
    validate_service_endpoint(
        &Url::parse(&created.metadata_url).map_err(|_| {
            CommandError::new(
                "share_protocol_error",
                "share service returned an invalid metadata URL",
            )
        })?,
        &origin,
        &format!("/v1/shares/{}", created.share_id),
    )?;
    let mut canonical = origin
        .join(&format!("/s/v1/{}", created.share_id))
        .map_err(|_| CommandError::new("share_protocol_error", "cannot build share URL"))?;
    canonical.set_fragment(Some(&format!("k={key}")));

    Ok(UploadShareResult {
        share_id: created.share_id,
        share_url: canonical.into(),
        expires_at: created.expires_at,
        revoke_token: created.revoke_token,
        upload_token: created.upload_token,
        ciphertext_sha256: inspected.ciphertext_sha256,
        ciphertext_bytes: inspected.ciphertext_bytes,
    })
}

pub fn upload_reserved_blob(credentials: &SavedUploadCredentials) -> Result<(), CommandError> {
    let origin = parse_service_origin(&credentials.service_base_url)?;
    if !valid_share_id(&credentials.share_id)
        || !valid_capability(&credentials.upload_token, KEY_LENGTH)
        || credentials.ciphertext_bytes == 0
        || credentials.ciphertext_bytes > MAX_CIPHERTEXT_BYTES as u64
        || !valid_sha256(&credentials.ciphertext_sha256)
    {
        return Err(CommandError::new(
            "share_history_corrupt",
            "saved upload credentials contain invalid share metadata",
        ));
    }

    let package_bytes = read_saved_upload_package(credentials)?;
    let endpoint = origin
        .join(&format!("/v1/shares/{}/blob", credentials.share_id))
        .map_err(|_| CommandError::new("invalid_share_service", "cannot build upload URL"))?;
    let response = http_client()?
        .put(endpoint)
        .header(
            AUTHORIZATION,
            format!("Bearer {}", credentials.upload_token),
        )
        .header(CONTENT_TYPE, "application/octet-stream")
        .header(CONTENT_LENGTH, package_bytes.len())
        .header("X-Relay-Ciphertext-Sha256", &credentials.ciphertext_sha256)
        .body(package_bytes)
        .send()
        .map_err(|error| recode_error(map_http_error(error), "share_upload_failed"))?;

    match response.status().as_u16() {
        200 | 201 => verify_ready_metadata_response(response, credentials),
        409 => {
            let upload_error = redact_error_secrets(
                response_error("share_upload_failed", response),
                std::iter::once(credentials.upload_token.as_str()),
            );
            match fetch_ready_metadata(&origin, credentials) {
                Ok(()) => Ok(()),
                Err(metadata_error) => Err(add_error_details(
                    upload_error,
                    serde_json::json!({
                        "metadata_verification_failed": true,
                        "metadata_error_code": metadata_error.code,
                        "metadata_error": metadata_error.message,
                    }),
                )),
            }
        }
        _ => Err(redact_error_secrets(
            response_error("share_upload_failed", response),
            std::iter::once(credentials.upload_token.as_str()),
        )),
    }
}

pub fn download_share(request: DownloadShareRequest) -> Result<DownloadShareResult, CommandError> {
    let configured_origin = parse_service_origin(&request.service_base_url)?;
    let parsed = parse_share_link(&request.share_url, &configured_origin)?;
    let endpoint = parsed
        .service_origin
        .join(&format!("/v1/shares/{}/blob", parsed.share_id))
        .map_err(|_| CommandError::new("invalid_share_link", "cannot build share download URL"))?;
    let response = http_client()?
        .get(endpoint)
        .send()
        .map_err(map_http_error)?;
    if response.status().as_u16() != 200 {
        return Err(response_error("share_download_failed", response));
    }
    if let Some(length) = response.content_length() {
        if length == 0 || length > MAX_CIPHERTEXT_BYTES as u64 {
            return Err(CommandError::new(
                "share_package_too_large",
                "share ciphertext exceeds the 32 MiB download limit",
            ));
        }
    }
    let expected_sha = response
        .headers()
        .get("X-Relay-Ciphertext-Sha256")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let package_bytes = read_response_limited(response, MAX_CIPHERTEXT_BYTES)?;
    if package_bytes.is_empty() {
        return Err(CommandError::new(
            "share_download_failed",
            "share service returned an empty ciphertext body",
        ));
    }
    let actual_sha = sha256_hex(&package_bytes);
    if expected_sha
        .as_deref()
        .is_some_and(|expected| !expected.eq_ignore_ascii_case(&actual_sha))
    {
        return Err(CommandError::new(
            "share_download_corrupt",
            "downloaded ciphertext differs from the service digest",
        ));
    }

    let output_path = validate_new_package_path(&request.output_path)?;
    write_new_private_file(&output_path, &package_bytes)?;
    let inspected = match relaypack::inspect_relaypack(&output_path.to_string_lossy(), &parsed.key)
    {
        Ok(inspected) => inspected,
        Err(error) => {
            let _ = fs::remove_file(&output_path);
            return Err(error);
        }
    };

    Ok(DownloadShareResult {
        package_path: output_path.to_string_lossy().into_owned(),
        key: parsed.key,
        share_id: parsed.share_id,
        ciphertext_sha256: inspected.ciphertext_sha256,
        ciphertext_bytes: inspected.ciphertext_bytes,
        preview: inspected.preview,
        content_preview: inspected.content_preview,
        warnings: inspected.warnings,
    })
}

pub fn revoke_share(request: RevokeShareRequest) -> Result<RevokeShareResult, CommandError> {
    let origin = parse_service_origin(&request.service_base_url)?;
    if !valid_share_id(&request.share_id) {
        return Err(CommandError::new(
            "invalid_share_id",
            "share ID must be a 32-character base64url token",
        ));
    }
    if !valid_capability(&request.revoke_token, KEY_LENGTH) {
        return Err(CommandError::new(
            "invalid_revoke_token",
            "revoke token must be a 43-character base64url token",
        ));
    }
    let endpoint = origin
        .join(&format!("/v1/shares/{}", request.share_id))
        .map_err(|_| CommandError::new("invalid_share_service", "cannot build revoke URL"))?;
    let response = http_client()?
        .delete(endpoint)
        .header(AUTHORIZATION, format!("Bearer {}", request.revoke_token))
        .send()
        .map_err(map_http_error)?;
    if !revoke_status_is_success(response.status().as_u16()) {
        return Err(redact_error_secrets(
            response_error("share_revoke_failed", response),
            std::iter::once(request.revoke_token.as_str()),
        ));
    }
    Ok(RevokeShareResult {
        share_id: request.share_id,
        revoked: true,
    })
}

fn revoke_status_is_success(status: u16) -> bool {
    matches!(status, 204 | 404 | 410)
}

fn parse_service_origin(raw: &str) -> Result<Url, CommandError> {
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
    Ok(url)
}

fn parse_share_link(raw: &str, configured_origin: &Url) -> Result<ParsedShareLink, CommandError> {
    let url = Url::parse(raw.trim())
        .map_err(|_| CommandError::new("invalid_share_link", "share link is not a valid URL"))?;
    ensure_same_origin(configured_origin, &url)?;
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() {
        return Err(CommandError::new(
            "invalid_share_link",
            "share link contains unsupported credentials or query parameters",
        ));
    }
    let prefix = "/s/v1/";
    let share_id = url
        .path()
        .strip_prefix(prefix)
        .filter(|value| !value.contains('/'));
    let share_id = share_id
        .filter(|value| valid_share_id(value))
        .ok_or_else(|| {
            CommandError::new(
                "invalid_share_link",
                "share link path must use /s/v1/{32-character-share-id}",
            )
        })?;
    let fragment = url.fragment().ok_or_else(|| {
        CommandError::new(
            "invalid_share_link",
            "share link is missing its decryption key fragment",
        )
    })?;
    let key = fragment.strip_prefix("k=").ok_or_else(|| {
        CommandError::new(
            "invalid_share_link",
            "share link fragment must use #k={Relay-key}",
        )
    })?;
    if fragment.contains('&') || fragment.contains('%') || !valid_capability(key, KEY_LENGTH) {
        return Err(CommandError::new(
            "invalid_share_link",
            "share link contains an invalid Relay key",
        ));
    }
    Ok(ParsedShareLink {
        share_id: share_id.to_owned(),
        key: key.to_owned(),
        service_origin: configured_origin.clone(),
    })
}

fn ensure_same_origin(expected: &Url, actual: &Url) -> Result<(), CommandError> {
    if expected.scheme() != actual.scheme()
        || expected.host_str() != actual.host_str()
        || expected.port_or_known_default() != actual.port_or_known_default()
    {
        return Err(CommandError::new(
            "share_origin_not_allowed",
            "share link does not belong to the configured Relay service",
        ));
    }
    Ok(())
}

fn validate_service_endpoint(
    actual: &Url,
    origin: &Url,
    expected_path: &str,
) -> Result<(), CommandError> {
    ensure_same_origin(origin, actual)?;
    if !actual.username().is_empty()
        || actual.password().is_some()
        || actual.query().is_some()
        || actual.fragment().is_some()
        || actual.path() != expected_path
    {
        return Err(CommandError::new(
            "share_protocol_error",
            "share service returned an endpoint outside the reserved share",
        ));
    }
    Ok(())
}

fn http_client() -> Result<Client, CommandError> {
    Client::builder()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .user_agent("Relay/0.1")
        .build()
        .map_err(|_| CommandError::new("http_client_failed", "cannot initialize HTTPS client"))
}

fn read_response_limited(mut response: Response, limit: usize) -> Result<Vec<u8>, CommandError> {
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            CommandError::new("share_read_failed", "cannot read share service response")
        })?;
    if bytes.len() > limit {
        return Err(CommandError::new(
            "share_package_too_large",
            "share service response exceeds the configured size limit",
        ));
    }
    Ok(bytes)
}

fn fetch_ready_metadata(
    origin: &Url,
    credentials: &SavedUploadCredentials,
) -> Result<(), CommandError> {
    let endpoint = origin
        .join(&format!("/v1/shares/{}", credentials.share_id))
        .map_err(|_| CommandError::new("invalid_share_service", "cannot build metadata URL"))?;
    let response = http_client()?
        .get(endpoint)
        .send()
        .map_err(|error| recode_error(map_http_error(error), "share_upload_failed"))?;
    if response.status().as_u16() != 200 {
        return Err(response_error("share_upload_failed", response));
    }
    verify_ready_metadata_response(response, credentials)
}

fn verify_ready_metadata_response(
    response: Response,
    credentials: &SavedUploadCredentials,
) -> Result<(), CommandError> {
    let bytes = read_response_limited(response, MAX_ERROR_BODY_BYTES)
        .map_err(|error| recode_error(error, "share_upload_failed"))?;
    verify_ready_metadata_bytes(&bytes, credentials)
}

fn verify_ready_metadata_bytes(
    bytes: &[u8],
    credentials: &SavedUploadCredentials,
) -> Result<(), CommandError> {
    let metadata: PublicShareResponse = serde_json::from_slice(bytes).map_err(|_| {
        CommandError::new(
            "share_upload_failed",
            "share service returned invalid public share metadata",
        )
    })?;
    if metadata.schema != "relay.share.public.v1"
        || metadata.status != "ready"
        || metadata.ciphertext.bytes != credentials.ciphertext_bytes
        || !metadata
            .ciphertext
            .sha256
            .eq_ignore_ascii_case(&credentials.ciphertext_sha256)
    {
        return Err(CommandError::new(
            "share_upload_failed",
            "share service metadata does not match the saved ciphertext",
        )
        .with_details(serde_json::json!({
            "remote_status": metadata.status,
            "remote_ciphertext_bytes": metadata.ciphertext.bytes,
            "remote_ciphertext_sha256": metadata.ciphertext.sha256,
        })));
    }
    Ok(())
}

fn response_error(code: &str, response: Response) -> CommandError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("Retry-After")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = read_response_limited(response, MAX_ERROR_BODY_BYTES).unwrap_or_default();
    service_response_error(code, status, retry_after, &body)
}

fn service_response_error(
    code: &str,
    status: u16,
    retry_after: Option<String>,
    body: &[u8],
) -> CommandError {
    let structured = serde_json::from_slice::<Value>(body).ok();
    let service_error_code = structured
        .as_ref()
        .and_then(|value| value.get("error"))
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let message = localized_service_error(service_error_code.as_deref(), status);
    let mut details = Map::new();
    details.insert("http_status".into(), Value::from(status));
    if let Some(service_error_code) = service_error_code {
        details.insert(
            "service_error_code".into(),
            Value::String(service_error_code),
        );
    }
    if let Some(retry_after) = retry_after {
        details.insert("retry_after".into(), Value::String(retry_after));
    }
    CommandError::new(code, message).with_details(Value::Object(details))
}

fn localized_service_error(code: Option<&str>, status: u16) -> String {
    match code {
        Some("rate_limited") => "操作过于频繁，请稍后重试。".into(),
        Some("share_not_found") => "这个分享不存在、已经到期或已经撤销。".into(),
        Some("share_not_ready") => "分享包还没有上传完成，请稍后重试。".into(),
        Some("upload_expired") => "上传时间已过，请重新生成分享链接。".into(),
        Some("request_too_large") | Some("ciphertext_too_large") => {
            "分享包超过服务允许的大小。".into()
        }
        Some("invalid_upload_token") | Some("upload_token_required") => {
            "分享服务拒绝了上传凭据，请重新生成分享链接。".into()
        }
        Some("ciphertext_size_conflict")
        | Some("ciphertext_digest_conflict")
        | Some("ciphertext_checksum_mismatch")
        | Some("stored_ciphertext_invalid") => {
            "上传内容与分享包校验信息不一致，请重新导出后再试。".into()
        }
        Some("origin_not_allowed") => "当前应用无法访问分享服务，请更新 Relay 后重试。".into(),
        _ if status >= 500 => "分享服务暂时无法处理请求，请稍后重试。".into(),
        _ => format!("分享服务没有接受这次请求（HTTP {status}）。"),
    }
}

fn add_error_details(mut error: CommandError, additions: Value) -> CommandError {
    let mut details = match error.details.take() {
        Some(Value::Object(details)) => details,
        Some(other) => {
            let mut details = Map::new();
            details.insert("original_details".into(), other);
            details
        }
        None => Map::new(),
    };
    if let Value::Object(additions) = additions {
        details.extend(additions);
    }
    error.details = Some(Value::Object(details));
    error
}

fn recode_error(mut error: CommandError, code: &str) -> CommandError {
    if error.code == code {
        return error;
    }
    let original_code = std::mem::replace(&mut error.code, code.to_owned());
    add_error_details(error, serde_json::json!({ "cause_code": original_code }))
}

fn redact_error_secrets<'a>(
    mut error: CommandError,
    secrets: impl Iterator<Item = &'a str>,
) -> CommandError {
    for secret in secrets.filter(|secret| !secret.is_empty()) {
        if error.message.contains(secret) {
            error.message = error.message.replace(secret, "[redacted]");
        }
    }
    error
}

fn map_http_error(error: reqwest::Error) -> CommandError {
    let message = if error.is_timeout() {
        "连接分享服务超时，请检查网络或代理设置后重试。"
    } else if error.is_connect() {
        "无法连接分享服务，请检查网络或代理设置后重试。"
    } else {
        "分享服务请求失败，请稍后重试。"
    };
    CommandError::new("share_network_error", message)
}

fn validate_key(raw: &str) -> Result<String, CommandError> {
    let key = raw.trim();
    if !valid_capability(key, KEY_LENGTH) {
        return Err(CommandError::new(
            "relaypack_key_invalid",
            "Relay key must be a 43-character base64url token",
        ));
    }
    Ok(key.to_owned())
}

fn valid_share_id(value: &str) -> bool {
    valid_capability(value, SHARE_ID_LENGTH)
}

fn valid_capability(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
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

fn canonical_regular_file(path: &Path, label: &str) -> Result<PathBuf, CommandError> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        CommandError::new(
            "invalid_path",
            format!("{label} does not exist or cannot be resolved: {error}"),
        )
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| {
        CommandError::new(
            "invalid_path",
            format!("cannot inspect {label} '{}': {error}", canonical.display()),
        )
    })?;
    if !metadata.is_file() {
        return Err(CommandError::new(
            "invalid_path",
            format!("{label} must be an ordinary file: {}", canonical.display()),
        ));
    }
    Ok(canonical)
}

fn read_file_limited(path: &Path, limit: usize) -> Result<Vec<u8>, CommandError> {
    let file = fs::File::open(path).map_err(|error| {
        CommandError::new(
            "share_package_read_failed",
            format!("cannot open Relay package: {error}"),
        )
    })?;
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            CommandError::new(
                "share_package_read_failed",
                format!("cannot read Relay package: {error}"),
            )
        })?;
    if bytes.len() > limit {
        return Err(CommandError::new(
            "share_package_too_large",
            "encrypted Relay package exceeds the 32 MiB sharing limit",
        ));
    }
    Ok(bytes)
}

fn read_saved_upload_package(
    credentials: &SavedUploadCredentials,
) -> Result<Vec<u8>, CommandError> {
    let path = Path::new(&credentials.package_path);
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(CommandError::new(
                "share_package_missing",
                "the saved Relay package no longer exists; the pending share can still be revoked",
            ))
        }
        Err(error) => {
            return Err(CommandError::new(
                "share_package_read_failed",
                format!("cannot inspect the saved Relay package: {error}"),
            ))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CommandError::new(
            "share_package_changed",
            "the saved Relay package path is no longer the original ordinary file",
        ));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CommandError::new(
                "share_package_missing",
                "the saved Relay package no longer exists; the pending share can still be revoked",
            )
        } else {
            CommandError::new(
                "share_package_read_failed",
                format!("cannot open the saved Relay package: {error}"),
            )
        }
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_CIPHERTEXT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            CommandError::new(
                "share_package_read_failed",
                format!("cannot read the saved Relay package: {error}"),
            )
        })?;
    if bytes.len() as u64 != credentials.ciphertext_bytes
        || !sha256_hex(&bytes).eq_ignore_ascii_case(&credentials.ciphertext_sha256)
    {
        return Err(CommandError::new(
            "share_package_changed",
            "the saved Relay package size or SHA-256 no longer matches the reserved share",
        ));
    }
    Ok(bytes)
}

fn validate_new_package_path(raw: &str) -> Result<PathBuf, CommandError> {
    let path = Path::new(raw.trim());
    if path.extension().and_then(|value| value.to_str()) != Some("relaypack") {
        return Err(CommandError::new(
            "invalid_output_path",
            "download path must end in .relaypack",
        ));
    }
    let name = path.file_name().ok_or_else(|| {
        CommandError::new("invalid_output_path", "download path must have a file name")
    })?;
    let parent =
        fs::canonicalize(path.parent().unwrap_or_else(|| Path::new("."))).map_err(|error| {
            CommandError::new(
                "invalid_output_path",
                format!("download directory cannot be resolved: {error}"),
            )
        })?;
    let candidate = parent.join(name);
    if fs::symlink_metadata(&candidate).is_ok() {
        return Err(CommandError::new(
            "output_exists",
            format!("download target '{}' already exists", candidate.display()),
        ));
    }
    Ok(candidate)
}

fn write_new_private_file(path: &Path, bytes: &[u8]) -> Result<(), CommandError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            CommandError::new(
                "share_download_write_failed",
                format!("cannot create downloaded Relay package: {error}"),
            )
        })?;
    file.write_all(bytes).map_err(|error| {
        CommandError::new(
            "share_download_write_failed",
            format!("cannot write downloaded Relay package: {error}"),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                CommandError::new(
                    "share_download_write_failed",
                    format!("cannot protect downloaded Relay package: {error}"),
                )
            })?;
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DownloadShareRequest, RevokeShareRequest, UploadShareRequest};

    fn test_credentials(package_path: &Path, bytes: &[u8]) -> SavedUploadCredentials {
        SavedUploadCredentials {
            share_id: "S".repeat(SHARE_ID_LENGTH),
            service_base_url: "http://127.0.0.1:1".into(),
            upload_token: "u".repeat(KEY_LENGTH),
            package_path: package_path.to_string_lossy().into_owned(),
            ciphertext_sha256: sha256_hex(bytes),
            ciphertext_bytes: bytes.len() as u64,
        }
    }

    #[test]
    fn strict_share_link_accepts_only_canonical_form() {
        let origin = parse_service_origin("https://share.example").unwrap();
        let id = "A".repeat(SHARE_ID_LENGTH);
        let key = "b".repeat(KEY_LENGTH);
        let valid = format!("https://share.example/s/v1/{id}#k={key}");
        let parsed = parse_share_link(&valid, &origin).unwrap();
        assert_eq!(parsed.share_id, id);
        assert_eq!(parsed.key, key);

        for invalid in [
            format!("https://share.example/s/{id}#k={key}"),
            format!("https://share.example/s/v1/{id}?x=1#k={key}"),
            format!("https://share.example/s/v1/{id}#key={key}"),
            format!("https://share.example/s/v1/{id}#k={key}&k={key}"),
            format!("https://evil.example/s/v1/{id}#k={key}"),
        ] {
            assert!(parse_share_link(&invalid, &origin).is_err(), "{invalid}");
        }
    }

    #[test]
    fn service_origin_allows_https_and_loopback_http_only() {
        assert!(parse_service_origin("https://share.example").is_ok());
        assert!(parse_service_origin("http://127.0.0.1:8787").is_ok());
        assert!(parse_service_origin("http://localhost:8787").is_ok());
        assert!(parse_service_origin("http://share.example").is_err());
        assert!(parse_service_origin("https://share.example/path").is_err());
        assert!(parse_service_origin("https://user@share.example").is_err());
    }

    #[test]
    fn reservation_body_contains_only_public_ciphertext_metadata() {
        let digest = "a".repeat(64);
        let body = serde_json::to_value(CreateShareReservationRequest {
            ciphertext_bytes: 128,
            ciphertext_sha256: &digest,
            expires_in_seconds: Some(3600),
        })
        .unwrap();
        assert_eq!(body["ciphertext_bytes"], 128);
        assert_eq!(body["ciphertext_sha256"], digest);
        assert_eq!(body["expires_in_seconds"], 3600);
        assert_eq!(body.as_object().unwrap().len(), 3);

        let share_id = "A".repeat(SHARE_ID_LENGTH);
        let reserved = UploadShareResult {
            share_id: share_id.clone(),
            share_url: format!(
                "https://share.example/s/v1/{share_id}#k={}",
                "k".repeat(KEY_LENGTH)
            ),
            expires_at: "2099-01-01T00:00:00Z".into(),
            upload_token: "u".repeat(KEY_LENGTH),
            revoke_token: "r".repeat(KEY_LENGTH),
            ciphertext_sha256: "a".repeat(64),
            ciphertext_bytes: 128,
        };
        let public = serde_json::to_string(&reserved).unwrap();
        assert!(!public.contains("upload_token"));
        assert!(!public.contains("revoke_token"));
        assert!(!public.contains(&"u".repeat(KEY_LENGTH)));
        assert!(!public.contains(&"r".repeat(KEY_LENGTH)));
    }

    #[test]
    fn saved_upload_rejects_missing_or_changed_package_before_network_access() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("saved.relaypack");
        fs::write(&package, b"original ciphertext").unwrap();
        let credentials = test_credentials(&package, b"original ciphertext");
        fs::write(&package, b"changed ciphertext").unwrap();
        let changed = upload_reserved_blob(&credentials).unwrap_err();
        assert_eq!(changed.code, "share_package_changed");

        fs::remove_file(&package).unwrap();
        let missing = upload_reserved_blob(&credentials).unwrap_err();
        assert_eq!(missing.code, "share_package_missing");
    }

    #[test]
    fn legacy_conflict_is_accepted_only_with_matching_public_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("saved.relaypack");
        let bytes = b"ciphertext";
        fs::write(&package, bytes).unwrap();
        let credentials = test_credentials(&package, bytes);
        let ready = serde_json::json!({
            "schema": "relay.share.public.v1",
            "status": "ready",
            "expires_at": "2099-01-01T00:00:00Z",
            "ciphertext": {
                "bytes": credentials.ciphertext_bytes,
                "sha256": credentials.ciphertext_sha256,
                "content_type": "application/octet-stream"
            }
        })
        .to_string();
        verify_ready_metadata_bytes(ready.as_bytes(), &credentials).unwrap();

        let mismatch = ready.replace(&credentials.ciphertext_sha256, &"f".repeat(64));
        let error = verify_ready_metadata_bytes(mismatch.as_bytes(), &credentials).unwrap_err();
        assert_eq!(error.code, "share_upload_failed");
    }

    #[test]
    fn structured_worker_error_code_is_preserved_and_message_is_localized() {
        let conflict = serde_json::json!({
            "error": {
                "code": "ciphertext_digest_conflict",
                "message": "The ciphertext digest differs from the reservation."
            }
        })
        .to_string();
        let error = service_response_error(
            "share_upload_failed",
            409,
            Some("7".into()),
            conflict.as_bytes(),
        );
        assert_eq!(error.code, "share_upload_failed");
        assert_eq!(
            error.message,
            "上传内容与分享包校验信息不一致，请重新导出后再试。"
        );
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|details| details.get("service_error_code"))
                .and_then(Value::as_str),
            Some("ciphertext_digest_conflict")
        );
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|details| details.get("http_status"))
                .and_then(Value::as_u64),
            Some(409)
        );
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|details| details.get("retry_after"))
                .and_then(Value::as_str),
            Some("7")
        );
    }

    #[test]
    fn structured_worker_error_never_echoes_a_capability_to_the_frontend() {
        let capability = "u".repeat(KEY_LENGTH);
        let body = serde_json::json!({
            "error": {
                "code": "invalid_upload_token",
                "message": format!("invalid capability: {capability}")
            }
        })
        .to_string();
        let error = redact_error_secrets(
            service_response_error("share_upload_failed", 403, None, body.as_bytes()),
            std::iter::once(capability.as_str()),
        );
        let public = serde_json::to_string(&error).unwrap();
        assert!(!public.contains(&capability));
        assert_eq!(
            error.message,
            "分享服务拒绝了上传凭据，请重新生成分享链接。"
        );
    }

    #[test]
    fn revocation_treats_missing_remote_share_as_already_gone() {
        assert!(revoke_status_is_success(204));
        assert!(revoke_status_is_success(404));
        assert!(revoke_status_is_success(410));
        assert!(!revoke_status_is_success(403));
        assert!(!revoke_status_is_success(500));
    }

    #[test]
    fn live_loopback_round_trip_when_service_is_configured() {
        let Ok(service_base_url) = std::env::var("RELAY_TEST_SHARE_SERVICE") else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("source.relaypack");
        let source = relaypack::export_test_relaypack(&source_path).unwrap();

        let request = UploadShareRequest {
            package_path: source.package_path.clone(),
            key: source.key_fragment.clone(),
            service_base_url: service_base_url.clone(),
            project_title: Some("Relay local smoke".into()),
            project_name: Some("Relay".into()),
            expires_in_seconds: Some(60),
            upload_token: None,
        };
        let uploaded = reserve_share(&request).unwrap();
        upload_reserved_blob(&SavedUploadCredentials {
            share_id: uploaded.share_id.clone(),
            service_base_url: service_base_url.clone(),
            upload_token: uploaded.upload_token.clone(),
            package_path: source.package_path.clone(),
            ciphertext_sha256: uploaded.ciphertext_sha256.clone(),
            ciphertext_bytes: uploaded.ciphertext_bytes,
        })
        .unwrap();
        assert_eq!(uploaded.ciphertext_sha256, source.ciphertext_sha256);
        assert!(uploaded.share_url.ends_with(&format!(
            "/s/v1/{}#k={}",
            uploaded.share_id, source.key_fragment
        )));

        let downloaded_path = temp.path().join("downloaded.relaypack");
        let downloaded = download_share(DownloadShareRequest {
            share_url: uploaded.share_url.clone(),
            service_base_url: service_base_url.clone(),
            output_path: downloaded_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        assert_eq!(downloaded.share_id, uploaded.share_id);
        assert_eq!(downloaded.ciphertext_sha256, source.ciphertext_sha256);
        assert_eq!(downloaded.preview.package_id, source.preview.package_id);

        let mut wrong_key_url = Url::parse(&uploaded.share_url).unwrap();
        wrong_key_url.set_fragment(Some(&format!("k={}", "A".repeat(KEY_LENGTH))));
        let wrong_key_path = temp.path().join("wrong-key.relaypack");
        let wrong_key_error = download_share(DownloadShareRequest {
            share_url: wrong_key_url.into(),
            service_base_url: service_base_url.clone(),
            output_path: wrong_key_path.to_string_lossy().into_owned(),
        })
        .unwrap_err();
        assert_eq!(wrong_key_error.code, "relaypack_auth_failed");
        assert!(!wrong_key_path.exists());

        let revoked = revoke_share(RevokeShareRequest {
            share_id: uploaded.share_id.clone(),
            revoke_token: uploaded.revoke_token,
            service_base_url: service_base_url.clone(),
        })
        .unwrap();
        assert!(revoked.revoked);

        let revoked_path = temp.path().join("revoked.relaypack");
        let revoked_error = download_share(DownloadShareRequest {
            share_url: uploaded.share_url,
            service_base_url,
            output_path: revoked_path.to_string_lossy().into_owned(),
        })
        .unwrap_err();
        assert_eq!(revoked_error.code, "share_download_failed");
        assert!(!revoked_path.exists());
    }
}
