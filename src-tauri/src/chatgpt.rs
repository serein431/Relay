use crate::types::CommandError;
use serde_json::json;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use url::Url;

const CHATGPT_HANDLER_PROBE_URL: &str = "codex://threads/new";

trait ChatGptHandlerRegistry {
    fn handlers_for_probe(&self, probe: &Url) -> Result<Vec<PathBuf>, CommandError>;
}

trait ChatGptHandlerVerifier {
    fn verify_handler(&self, application_path: &Path) -> Result<(), CommandError>;
}

trait ChatGptUrlOpener {
    fn open_with_application(
        &self,
        application_path: &Path,
        deep_link: &Url,
    ) -> Result<(), CommandError>;
}

pub fn open_imported_task(session_id: &str) -> Result<(), CommandError> {
    let session_id = validate_session_id(session_id)?;
    open_imported_task_for_platform(session_id)
}

#[cfg(target_os = "macos")]
fn open_imported_task_for_platform(session_id: &str) -> Result<(), CommandError> {
    let registry = macos_chatgpt::MacChatGptRegistry;
    let verifier = macos_chatgpt::PinnedChatGptVerifier;
    let opener = macos_chatgpt::MacChatGptOpener;
    open_imported_task_with_services(session_id, &registry, &verifier, &opener)
}

#[cfg(not(target_os = "macos"))]
fn open_imported_task_for_platform(_session_id: &str) -> Result<(), CommandError> {
    Err(CommandError::new(
        "unsupported_platform",
        "Relay can only open an imported ChatGPT task automatically on macOS",
    ))
}

fn open_imported_task_with_services<R, V, O>(
    session_id: &str,
    registry: &R,
    verifier: &V,
    opener: &O,
) -> Result<(), CommandError>
where
    R: ChatGptHandlerRegistry,
    V: ChatGptHandlerVerifier,
    O: ChatGptUrlOpener,
{
    let probe = Url::parse(CHATGPT_HANDLER_PROBE_URL).map_err(|_| {
        CommandError::new(
            "chatgpt_open_failed",
            "cannot construct the ChatGPT handler probe",
        )
    })?;
    let handlers = registry.handlers_for_probe(&probe)?;
    let application_path = select_verified_handler(handlers, verifier)?;
    let deep_link = imported_task_url(session_id)?;
    opener.open_with_application(&application_path, &deep_link)
}

fn validate_session_id(session_id: &str) -> Result<&str, CommandError> {
    let value = session_id.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(CommandError::new(
            "invalid_session_id",
            "the imported ChatGPT task id is invalid",
        ));
    }
    Ok(value)
}

fn imported_task_url(session_id: &str) -> Result<Url, CommandError> {
    Url::parse(&format!("codex://threads/{session_id}")).map_err(|_| {
        CommandError::new(
            "chatgpt_open_failed",
            "cannot construct the imported ChatGPT task link",
        )
    })
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
        ChatGptHandlerRegistry, ChatGptHandlerVerifier, ChatGptUrlOpener, CommandError, Path,
        PathBuf, Url,
    };
    use block2::RcBlock;
    use core_foundation::url::CFURL;
    use objc2_app_kit::{NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration};
    use objc2_foundation::{NSArray, NSError, NSString, NSURL};
    use security_framework::os::macos::code_signing::{Flags, SecRequirement, SecStaticCode};
    use std::sync::mpsc;
    use std::time::Duration;

    const CHATGPT_PINNED_REQUIREMENT: &str = concat!(
        "identifier \"com.openai.codex\" and anchor apple generic ",
        "and certificate 1[field.1.2.840.113635.100.6.2.6] exists ",
        "and certificate leaf[field.1.2.840.113635.100.6.1.13] exists ",
        "and certificate leaf[subject.OU] = \"2DC432GLL2\""
    );
    const OPEN_COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);

    pub struct MacChatGptRegistry;
    pub struct PinnedChatGptVerifier;
    pub struct MacChatGptOpener;

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

    impl ChatGptUrlOpener for MacChatGptOpener {
        fn open_with_application(
            &self,
            application_path: &Path,
            deep_link: &Url,
        ) -> Result<(), CommandError> {
            let deep_link = ns_url(deep_link, "ChatGPT task link")?;
            let application_path = NSString::from_str(&application_path.to_string_lossy());
            let application_url = NSURL::fileURLWithPath_isDirectory(&application_path, true);
            let urls = NSArray::from_retained_slice(&[deep_link]);
            let configuration = NSWorkspaceOpenConfiguration::configuration();
            let (sender, receiver) = mpsc::sync_channel(1);
            let completion: RcBlock<dyn Fn(*mut NSRunningApplication, *mut NSError)> = RcBlock::new(
                move |application: *mut NSRunningApplication, error: *mut NSError| {
                    let _ = sender.try_send((!application.is_null(), error.is_null()));
                },
            );
            NSWorkspace::sharedWorkspace()
                .openURLs_withApplicationAtURL_configuration_completionHandler(
                    &urls,
                    &application_url,
                    &configuration,
                    Some(&completion),
                );
            match receiver.recv_timeout(OPEN_COMPLETION_TIMEOUT) {
                Ok((true, true)) => Ok(()),
                Ok(_) => Err(CommandError::new(
                    "chatgpt_open_failed",
                    "macOS could not open the imported task with ChatGPT",
                )),
                Err(mpsc::RecvTimeoutError::Timeout) => Err(CommandError::new(
                    "chatgpt_open_failed",
                    "macOS did not confirm the ChatGPT open request in time",
                )),
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(CommandError::new(
                    "chatgpt_open_failed",
                    "macOS ended the ChatGPT open request before reporting a result",
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
    struct FakeOpener {
        calls: RefCell<Vec<String>>,
    }

    impl ChatGptUrlOpener for FakeOpener {
        fn open_with_application(
            &self,
            _application_path: &Path,
            deep_link: &Url,
        ) -> Result<(), CommandError> {
            self.calls.borrow_mut().push(deep_link.as_str().to_owned());
            Ok(())
        }
    }

    #[test]
    fn opens_the_exact_imported_task_with_a_verified_application() {
        let directory = tempfile::tempdir().unwrap();
        let application = directory.path().join("ChatGPT.app");
        fs::create_dir(&application).unwrap();
        let application = fs::canonicalize(application).unwrap();
        let registry = FakeRegistry {
            handlers: vec![application.clone()],
        };
        let verifier = FakeVerifier {
            trusted: HashSet::from([application]),
        };
        let opener = FakeOpener::default();

        open_imported_task_with_services(
            "01912345-6789-7abc-8def-0123456789ab",
            &registry,
            &verifier,
            &opener,
        )
        .unwrap();

        assert_eq!(
            opener.calls.borrow().as_slice(),
            ["codex://threads/01912345-6789-7abc-8def-0123456789ab"]
        );
    }

    #[test]
    fn rejects_an_untrusted_handler() {
        let directory = tempfile::tempdir().unwrap();
        let application = directory.path().join("ChatGPT.app");
        fs::create_dir(&application).unwrap();
        let registry = FakeRegistry {
            handlers: vec![application],
        };
        let error = open_imported_task_with_services(
            "01912345-6789-7abc-8def-0123456789ab",
            &registry,
            &FakeVerifier::default(),
            &FakeOpener::default(),
        )
        .unwrap_err();
        assert_eq!(error.code, "chatgpt_identity_unverified");
    }
}
