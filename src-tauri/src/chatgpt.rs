use crate::types::CommandError;
use serde_json::json;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use url::Url;

const CHATGPT_HANDLER_PROBE_URL: &str = "codex://threads/new";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogRefreshStatus {
    Sent,
    NotRunning,
}

trait ChatGptHandlerRegistry {
    fn handlers_for_probe(&self, probe: &Url) -> Result<Vec<PathBuf>, CommandError>;
}

trait ChatGptHandlerVerifier {
    fn verify_handler(&self, application_path: &Path) -> Result<(), CommandError>;
}

trait ChatGptApplicationLauncher {
    fn launch_application(&self, application_path: &Path) -> Result<(), CommandError>;
}

pub fn show_task_list() -> Result<(), CommandError> {
    show_task_list_for_platform()
}

pub fn refresh_and_show_task_list(codex_home: &Path) -> Result<(), CommandError> {
    refresh_running_catalog(codex_home)?;
    show_task_list()
}

pub fn refresh_running_catalog(codex_home: &Path) -> Result<CatalogRefreshStatus, CommandError> {
    refresh_running_catalog_for_platform(codex_home)
}

#[cfg(target_os = "macos")]
fn refresh_running_catalog_for_platform(
    codex_home: &Path,
) -> Result<CatalogRefreshStatus, CommandError> {
    macos_chatgpt::refresh_running_catalog(codex_home)
}

#[cfg(not(target_os = "macos"))]
fn refresh_running_catalog_for_platform(
    _codex_home: &Path,
) -> Result<CatalogRefreshStatus, CommandError> {
    Ok(CatalogRefreshStatus::NotRunning)
}

#[cfg(target_os = "macos")]
fn show_task_list_for_platform() -> Result<(), CommandError> {
    let registry = macos_chatgpt::MacChatGptRegistry;
    let verifier = macos_chatgpt::PinnedChatGptVerifier;
    let launcher = macos_chatgpt::MacChatGptLauncher;
    show_task_list_with_services(&registry, &verifier, &launcher)
}

#[cfg(not(target_os = "macos"))]
fn show_task_list_for_platform() -> Result<(), CommandError> {
    Err(CommandError::new(
        "unsupported_platform",
        "Relay can only show the ChatGPT task list automatically on macOS",
    ))
}

fn show_task_list_with_services<R, V, L>(
    registry: &R,
    verifier: &V,
    launcher: &L,
) -> Result<(), CommandError>
where
    R: ChatGptHandlerRegistry,
    V: ChatGptHandlerVerifier,
    L: ChatGptApplicationLauncher,
{
    let probe = Url::parse(CHATGPT_HANDLER_PROBE_URL).map_err(|_| {
        CommandError::new(
            "chatgpt_open_failed",
            "cannot construct the ChatGPT handler probe",
        )
    })?;
    let handlers = registry.handlers_for_probe(&probe)?;
    let application_path = select_verified_handler(handlers, verifier)?;
    launcher.launch_application(&application_path)
}

fn select_verified_handler<V>(handlers: Vec<PathBuf>, verifier: &V) -> Result<PathBuf, CommandError>
where
    V: ChatGptHandlerVerifier,
{
    if handlers.is_empty() {
        return Err(CommandError::new(
            "chatgpt_handler_not_found",
            "no ChatGPT application is registered to open codex:// links",
        ));
    }

    let candidate_count = handlers.len();
    let mut seen = HashSet::new();
    let mut failures = Vec::new();
    for handler in handlers {
        let canonical = match fs::canonicalize(&handler) {
            Ok(path) => path,
            Err(_) => {
                failures.push("handler_path_unavailable".to_owned());
                continue;
            }
        };
        if !seen.insert(canonical.clone()) {
            continue;
        }
        if !fs::metadata(&canonical)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
        {
            failures.push("handler_path_invalid".to_owned());
            continue;
        }
        match verifier.verify_handler(&canonical) {
            Ok(()) => return Ok(canonical),
            Err(error) => failures.push(error.code),
        }
    }

    Err(CommandError::new(
        "chatgpt_identity_unverified",
        "no registered ChatGPT application passed the OpenAI signature check",
    )
    .with_details(json!({
        "candidate_count": candidate_count,
        "verification_failures": failures,
    })))
}

#[cfg(target_os = "macos")]
mod macos_chatgpt {
    use super::{
        CatalogRefreshStatus, ChatGptApplicationLauncher, ChatGptHandlerRegistry,
        ChatGptHandlerVerifier, CommandError, Path, PathBuf, Url,
    };
    use block2::RcBlock;
    use core_foundation::url::CFURL;
    use objc2_app_kit::{NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration};
    use objc2_foundation::{NSError, NSString, NSURL};
    use security_framework::os::macos::code_signing::{Flags, SecRequirement, SecStaticCode};
    use serde_json::{json, Value};
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Duration;

    const CHATGPT_PINNED_REQUIREMENT: &str = concat!(
        "identifier \"com.openai.codex\" and anchor apple generic ",
        "and certificate 1[field.1.2.840.113635.100.6.2.6] exists ",
        "and certificate leaf[field.1.2.840.113635.100.6.1.13] exists ",
        "and certificate leaf[subject.OU] = \"2DC432GLL2\""
    );
    const OPEN_COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);
    const IPC_TIMEOUT: Duration = Duration::from_secs(2);
    const MAX_IPC_FRAME_BYTES: usize = 1024 * 1024;
    const QUERY_CACHE_INVALIDATE_VERSION: u8 = 1;

    pub struct MacChatGptRegistry;
    pub struct PinnedChatGptVerifier;
    pub struct MacChatGptLauncher;

    pub fn refresh_running_catalog(
        codex_home: &Path,
    ) -> Result<CatalogRefreshStatus, CommandError> {
        let socket_path = codex_home.join("ipc").join("ipc.sock");
        if !socket_path.exists() {
            return Ok(CatalogRefreshStatus::NotRunning);
        }
        verify_ipc_socket(&socket_path)?;
        let mut stream = match UnixStream::connect(&socket_path) {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(CatalogRefreshStatus::NotRunning);
            }
            Err(error) => {
                return Err(CommandError::new(
                    "chatgpt_catalog_refresh_failed",
                    format!("cannot connect to the running ChatGPT task catalog: {error}"),
                ));
            }
        };
        stream
            .set_read_timeout(Some(IPC_TIMEOUT))
            .map_err(ipc_refresh_error)?;
        stream
            .set_write_timeout(Some(IPC_TIMEOUT))
            .map_err(ipc_refresh_error)?;

        let request_id = uuid::Uuid::new_v4().to_string();
        write_json_frame(
            &mut stream,
            &json!({
                "type": "request",
                "requestId": request_id,
                "method": "initialize",
                "params": { "clientType": "relay" }
            }),
        )?;
        let client_id = loop {
            let message = read_json_frame(&mut stream)?;
            if message.get("type").and_then(Value::as_str) != Some("response")
                || message.get("requestId").and_then(Value::as_str) != Some(&request_id)
            {
                continue;
            }
            if message.get("resultType").and_then(Value::as_str) != Some("success") {
                return Err(CommandError::new(
                    "chatgpt_catalog_refresh_failed",
                    "ChatGPT rejected the task catalog refresh connection",
                ));
            }
            break message
                .pointer("/result/clientId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    CommandError::new(
                        "chatgpt_catalog_refresh_failed",
                        "ChatGPT did not identify the task catalog refresh connection",
                    )
                })?
                .to_owned();
        };

        write_json_frame(
            &mut stream,
            &json!({
                "type": "broadcast",
                "method": "query-cache-invalidate",
                "sourceClientId": client_id,
                "params": { "queryKey": [] },
                "version": QUERY_CACHE_INVALIDATE_VERSION
            }),
        )?;
        stream.flush().map_err(ipc_refresh_error)?;
        Ok(CatalogRefreshStatus::Sent)
    }

    fn verify_ipc_socket(socket_path: &Path) -> Result<(), CommandError> {
        let current_user = unsafe { libc::geteuid() };
        let directory = socket_path.parent().ok_or_else(|| {
            CommandError::new(
                "chatgpt_ipc_untrusted",
                "the ChatGPT task catalog socket has no parent directory",
            )
        })?;
        let directory_metadata = fs::symlink_metadata(directory).map_err(ipc_refresh_error)?;
        let directory_mode = directory_metadata.permissions().mode();
        if !directory_metadata.is_dir()
            || directory_metadata.uid() != current_user
            || directory_mode & 0o022 != 0
        {
            return Err(CommandError::new(
                "chatgpt_ipc_untrusted",
                "the ChatGPT task catalog directory is not private to the current user",
            ));
        }
        let socket_metadata = fs::symlink_metadata(socket_path).map_err(ipc_refresh_error)?;
        if !socket_metadata.file_type().is_socket() || socket_metadata.uid() != current_user {
            return Err(CommandError::new(
                "chatgpt_ipc_untrusted",
                "the ChatGPT task catalog socket is not owned by the current user",
            ));
        }
        Ok(())
    }

    fn write_json_frame(stream: &mut UnixStream, message: &Value) -> Result<(), CommandError> {
        let body = serde_json::to_vec(message).map_err(|error| {
            CommandError::new(
                "chatgpt_catalog_refresh_failed",
                format!("cannot encode the ChatGPT task catalog refresh: {error}"),
            )
        })?;
        if body.is_empty() || body.len() > MAX_IPC_FRAME_BYTES {
            return Err(CommandError::new(
                "chatgpt_catalog_refresh_failed",
                "the ChatGPT task catalog refresh message has an invalid size",
            ));
        }
        let length = u32::try_from(body.len()).map_err(|_| {
            CommandError::new(
                "chatgpt_catalog_refresh_failed",
                "the ChatGPT task catalog refresh message is too large",
            )
        })?;
        stream
            .write_all(&length.to_le_bytes())
            .and_then(|()| stream.write_all(&body))
            .map_err(ipc_refresh_error)
    }

    fn read_json_frame(stream: &mut UnixStream) -> Result<Value, CommandError> {
        let mut length = [0_u8; 4];
        stream.read_exact(&mut length).map_err(ipc_refresh_error)?;
        let length = u32::from_le_bytes(length) as usize;
        if length == 0 || length > MAX_IPC_FRAME_BYTES {
            return Err(CommandError::new(
                "chatgpt_catalog_refresh_failed",
                "ChatGPT returned an invalid task catalog refresh frame",
            ));
        }
        let mut body = vec![0_u8; length];
        stream.read_exact(&mut body).map_err(ipc_refresh_error)?;
        serde_json::from_slice(&body).map_err(|error| {
            CommandError::new(
                "chatgpt_catalog_refresh_failed",
                format!("ChatGPT returned an invalid task catalog refresh response: {error}"),
            )
        })
    }

    fn ipc_refresh_error(error: io::Error) -> CommandError {
        CommandError::new(
            "chatgpt_catalog_refresh_failed",
            format!("ChatGPT task catalog refresh failed: {error}"),
        )
    }

    impl ChatGptHandlerRegistry for MacChatGptRegistry {
        fn handlers_for_probe(&self, probe: &Url) -> Result<Vec<PathBuf>, CommandError> {
            let probe = ns_url(probe, "ChatGPT handler probe")?;
            let applications = NSWorkspace::sharedWorkspace().URLsForApplicationsToOpenURL(&probe);
            Ok(applications
                .iter()
                .filter(|application| application.isFileURL())
                .filter_map(|application| application.path())
                .map(|path| PathBuf::from(path.to_string()))
                .collect())
        }
    }

    impl ChatGptHandlerVerifier for PinnedChatGptVerifier {
        fn verify_handler(&self, application_path: &Path) -> Result<(), CommandError> {
            let application_url = CFURL::from_path(application_path, true).ok_or_else(|| {
                CommandError::new(
                    "chatgpt_signature_check_failed",
                    "cannot create a Security.framework URL for the ChatGPT application",
                )
            })?;
            let code =
                SecStaticCode::from_path(&application_url, Flags::NONE).map_err(|error| {
                    CommandError::new(
                        "chatgpt_signature_check_failed",
                        format!("cannot inspect the ChatGPT application signature: {error}"),
                    )
                })?;
            let requirement: SecRequirement =
                CHATGPT_PINNED_REQUIREMENT.parse().map_err(|error| {
                    CommandError::new(
                        "chatgpt_signature_check_failed",
                        format!("cannot compile the pinned ChatGPT signature requirement: {error}"),
                    )
                })?;
            let flags =
                Flags::CHECK_ALL_ARCHITECTURES | Flags::CHECK_NESTED_CODE | Flags::STRICT_VALIDATE;
            code.check_validity(flags, &requirement).map_err(|error| {
                CommandError::new(
                    "chatgpt_signature_untrusted",
                    format!("ChatGPT application signature did not match OpenAI: {error}"),
                )
            })
        }
    }

    impl ChatGptApplicationLauncher for MacChatGptLauncher {
        fn launch_application(&self, application_path: &Path) -> Result<(), CommandError> {
            let application_path = NSString::from_str(&application_path.to_string_lossy());
            let application_url = NSURL::fileURLWithPath_isDirectory(&application_path, true);
            let configuration = NSWorkspaceOpenConfiguration::configuration();
            configuration.setActivates(true);
            configuration.setCreatesNewApplicationInstance(false);
            configuration.setAllowsRunningApplicationSubstitution(true);
            let (sender, receiver) = mpsc::sync_channel(1);
            let completion: RcBlock<dyn Fn(*mut NSRunningApplication, *mut NSError)> = RcBlock::new(
                move |application: *mut NSRunningApplication, error: *mut NSError| {
                    let _ = sender.try_send((!application.is_null(), error.is_null()));
                },
            );
            NSWorkspace::sharedWorkspace().openApplicationAtURL_configuration_completionHandler(
                &application_url,
                &configuration,
                Some(&completion),
            );
            match receiver.recv_timeout(OPEN_COMPLETION_TIMEOUT) {
                Ok((true, true)) => Ok(()),
                Ok(_) => Err(CommandError::new(
                    "chatgpt_open_failed",
                    "macOS could not open ChatGPT",
                )),
                Err(mpsc::RecvTimeoutError::Timeout) => Err(CommandError::new(
                    "chatgpt_open_failed",
                    "macOS did not confirm the ChatGPT launch request in time",
                )),
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(CommandError::new(
                    "chatgpt_open_failed",
                    "macOS ended the ChatGPT launch request before reporting a result",
                )),
            }
        }
    }

    fn ns_url(url: &Url, label: &str) -> Result<objc2::rc::Retained<NSURL>, CommandError> {
        let value = NSString::from_str(url.as_str());
        NSURL::URLWithString(&value).ok_or_else(|| {
            CommandError::new(
                "chatgpt_open_failed",
                format!("macOS could not construct the {label}"),
            )
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;
        use std::sync::mpsc;
        use std::thread;

        #[test]
        fn reports_chatgpt_as_not_running_without_an_ipc_socket() {
            let home = tempfile::tempdir().unwrap();
            assert_eq!(
                refresh_running_catalog(home.path()).unwrap(),
                CatalogRefreshStatus::NotRunning
            );
        }

        #[test]
        fn rejects_an_ipc_directory_writable_by_other_users() {
            let home = tempfile::tempdir().unwrap();
            let ipc_directory = home.path().join("ipc");
            fs::create_dir(&ipc_directory).unwrap();
            fs::set_permissions(&ipc_directory, fs::Permissions::from_mode(0o777)).unwrap();
            let listener = UnixListener::bind(ipc_directory.join("ipc.sock")).unwrap();

            let error = refresh_running_catalog(home.path()).unwrap_err();

            drop(listener);
            assert_eq!(error.code, "chatgpt_ipc_untrusted");
        }

        #[test]
        fn initializes_ipc_and_invalidates_the_running_task_catalog() {
            let home = tempfile::tempdir().unwrap();
            let ipc_directory = home.path().join("ipc");
            fs::create_dir(&ipc_directory).unwrap();
            fs::set_permissions(&ipc_directory, fs::Permissions::from_mode(0o700)).unwrap();
            let listener = UnixListener::bind(ipc_directory.join("ipc.sock")).unwrap();
            let (sender, receiver) = mpsc::sync_channel(1);

            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let initialize = read_json_frame(&mut stream).unwrap();
                let request_id = initialize
                    .get("requestId")
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_owned();
                write_json_frame(
                    &mut stream,
                    &json!({
                        "type": "response",
                        "requestId": request_id,
                        "resultType": "success",
                        "result": { "clientId": "relay-test-client" }
                    }),
                )
                .unwrap();
                let broadcast = read_json_frame(&mut stream).unwrap();
                sender.send((initialize, broadcast)).unwrap();
            });

            assert_eq!(
                refresh_running_catalog(home.path()).unwrap(),
                CatalogRefreshStatus::Sent
            );
            let (initialize, broadcast) = receiver.recv_timeout(IPC_TIMEOUT).unwrap();
            server.join().unwrap();

            assert_eq!(
                initialize.pointer("/params/clientType"),
                Some(&json!("relay"))
            );
            assert_eq!(initialize.get("method"), Some(&json!("initialize")));
            assert_eq!(broadcast.get("type"), Some(&json!("broadcast")));
            assert_eq!(
                broadcast.get("method"),
                Some(&json!("query-cache-invalidate"))
            );
            assert_eq!(
                broadcast.get("sourceClientId"),
                Some(&json!("relay-test-client"))
            );
            assert_eq!(broadcast.pointer("/params/queryKey"), Some(&json!([])));
            assert_eq!(broadcast.get("version"), Some(&json!(1)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;

    #[derive(Default)]
    struct FakeRegistry {
        handlers: Vec<PathBuf>,
    }

    impl ChatGptHandlerRegistry for FakeRegistry {
        fn handlers_for_probe(&self, _probe: &Url) -> Result<Vec<PathBuf>, CommandError> {
            Ok(self.handlers.clone())
        }
    }

    #[derive(Default)]
    struct FakeVerifier {
        trusted: HashSet<PathBuf>,
    }

    impl ChatGptHandlerVerifier for FakeVerifier {
        fn verify_handler(&self, application_path: &Path) -> Result<(), CommandError> {
            if self.trusted.contains(application_path) {
                Ok(())
            } else {
                Err(CommandError::new(
                    "chatgpt_signature_untrusted",
                    "fake signature did not match",
                ))
            }
        }
    }

    #[derive(Default)]
    struct FakeLauncher {
        calls: RefCell<Vec<PathBuf>>,
    }

    impl ChatGptApplicationLauncher for FakeLauncher {
        fn launch_application(&self, application_path: &Path) -> Result<(), CommandError> {
            self.calls.borrow_mut().push(application_path.to_path_buf());
            Ok(())
        }
    }

    #[test]
    fn shows_the_verified_chatgpt_application_without_a_task_link() {
        let directory = tempfile::tempdir().unwrap();
        let application = directory.path().join("ChatGPT.app");
        fs::create_dir(&application).unwrap();
        let application = fs::canonicalize(application).unwrap();
        let registry = FakeRegistry {
            handlers: vec![application.clone()],
        };
        let verifier = FakeVerifier {
            trusted: HashSet::from([application.clone()]),
        };
        let launcher = FakeLauncher::default();

        show_task_list_with_services(&registry, &verifier, &launcher).unwrap();

        assert_eq!(launcher.calls.borrow().as_slice(), [application]);
    }

    #[test]
    fn rejects_an_untrusted_handler() {
        let directory = tempfile::tempdir().unwrap();
        let application = directory.path().join("ChatGPT.app");
        fs::create_dir(&application).unwrap();
        let registry = FakeRegistry {
            handlers: vec![application],
        };
        let error = show_task_list_with_services(
            &registry,
            &FakeVerifier::default(),
            &FakeLauncher::default(),
        )
        .unwrap_err();
        assert_eq!(error.code, "chatgpt_identity_unverified");
    }
}
