use crate::process::{
    canonical_executable, canonical_existing_directory, find_executable_on_path, run_process,
    ProcessRunError,
};
use crate::types::{AgentProvider, CommandError, LaunchAgentRequest, LaunchAgentResult};
use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use url::Url;
use wait_timeout::ChildExt;

const MAX_HANDOFF_BYTES: u64 = 2 * 1024 * 1024;
const MAX_AGENT_LIST_BYTES: usize = 1024 * 1024;
const MAX_LAUNCH_OUTPUT_BYTES: u64 = 64 * 1024;
const CLAUDE_STARTED_AT_SLOP_SECONDS: i64 = 2;
const VERIFICATION_VERIFIED: &str = "VERIFIED";
const VERIFICATION_OPEN_REQUESTED: &str = "OPEN_REQUESTED";
const VERIFICATION_UNVERIFIED: &str = "UNVERIFIED";
const CODEX_HANDLER_PROBE_URL: &str = "codex://threads/new";

#[derive(Debug, Clone, Copy)]
struct ClaudeLaunchTiming {
    launch_timeout: Duration,
    agents_timeout: Duration,
    verification_timeout: Duration,
    poll_interval: Duration,
}

impl ClaudeLaunchTiming {
    fn production() -> Self {
        Self {
            launch_timeout: Duration::from_secs(30),
            agents_timeout: Duration::from_secs(10),
            verification_timeout: Duration::from_secs(15),
            poll_interval: Duration::from_millis(250),
        }
    }
}

#[derive(Debug, Clone)]
struct ClaudeAgentRecord {
    id: String,
    kinds: Vec<String>,
    cwds: Vec<String>,
    started_at: Vec<DateTime<Utc>>,
    state: Option<String>,
    waiting_reason: Option<String>,
}

#[derive(Debug)]
struct ClaudeLaunchOutcome {
    process_id: u32,
    status: Option<ExitStatus>,
    timed_out: bool,
    stdout: String,
    stderr: String,
}

#[derive(Debug)]
enum CandidateSelection {
    None,
    Verified(ClaudeAgentRecord),
    Ambiguous(Vec<String>),
}

trait CodexHandlerRegistry {
    fn handlers_for_probe(&self, probe: &Url) -> Result<Vec<PathBuf>, CommandError>;
}

trait CodexHandlerVerifier {
    fn verify_handler(&self, application_path: &Path) -> Result<(), CommandError>;
}

trait CodexUrlOpener {
    fn open_with_application(
        &self,
        application_path: &Path,
        deep_link: &Url,
    ) -> Result<(), CommandError>;
}

pub fn launch_agent(request: LaunchAgentRequest) -> Result<LaunchAgentResult, CommandError> {
    let worktree = canonical_existing_directory(Path::new(&request.worktree_path), "worktree")?;
    let handoff = canonical_handoff(Path::new(&request.handoff_markdown_path), &worktree)?;
    let startup_prompt = startup_prompt(&handoff);

    match request.agent {
        AgentProvider::Codex => launch_codex(worktree, startup_prompt),
        AgentProvider::ClaudeCode => launch_claude_background(worktree, startup_prompt),
        AgentProvider::Unknown => Err(CommandError::new(
            "unsupported_agent",
            "choose Claude Code or ChatGPT before launching",
        )),
    }
}

#[cfg(target_os = "macos")]
fn launch_codex(
    worktree: PathBuf,
    startup_prompt: String,
) -> Result<LaunchAgentResult, CommandError> {
    let registry = macos_codex::MacCodexRegistry;
    let verifier = macos_codex::PinnedCodexVerifier;
    let opener = macos_codex::MacCodexOpener;
    launch_codex_with_services(worktree, startup_prompt, &registry, &verifier, &opener)
}

#[cfg(not(target_os = "macos"))]
fn launch_codex(
    worktree: PathBuf,
    startup_prompt: String,
) -> Result<LaunchAgentResult, CommandError> {
    let _ = (worktree, startup_prompt);
    Err(CommandError::new(
        "unsupported_platform",
        "ChatGPT deep links can only be opened by Relay on macOS",
    ))
}

fn launch_codex_with_services<R, V, O>(
    worktree: PathBuf,
    startup_prompt: String,
    registry: &R,
    verifier: &V,
    opener: &O,
) -> Result<LaunchAgentResult, CommandError>
where
    R: CodexHandlerRegistry,
    V: CodexHandlerVerifier,
    O: CodexUrlOpener,
{
    let probe = codex_handler_probe_url()?;
    let handlers = registry.handlers_for_probe(&probe)?;
    let application_path = select_verified_codex_handler(handlers, verifier)?;

    // The real workspace path and handoff prompt are not placed in a URL until
    // an application has passed the pinned Security.framework requirement.
    let deep_link = codex_deep_link(&worktree, &startup_prompt)?;
    opener.open_with_application(&application_path, &deep_link)?;

    Ok(LaunchAgentResult {
        agent: AgentProvider::Codex,
        worktree_path: worktree.to_string_lossy().into_owned(),
        executable_path: application_path.to_string_lossy().into_owned(),
        process_id: 0,
        launch_mode: "deep_link".into(),
        startup_prompt,
        verification_status: VERIFICATION_OPEN_REQUESTED.into(),
        session_id: None,
        session_state: None,
        waiting_reason: None,
    })
}

fn codex_handler_probe_url() -> Result<Url, CommandError> {
    Url::parse(CODEX_HANDLER_PROBE_URL).map_err(|_| {
        CommandError::new(
            "agent_launch_failed",
            "cannot construct the ChatGPT handler probe",
        )
    })
}

fn select_verified_codex_handler<V>(
    handlers: Vec<PathBuf>,
    verifier: &V,
) -> Result<PathBuf, CommandError>
where
    V: CodexHandlerVerifier,
{
    if handlers.is_empty() {
        return Err(CommandError::new(
            "codex_handler_not_found",
            "no ChatGPT application is registered to open codex:// links",
        ));
    }

    let candidate_count = handlers.len();
    let mut seen = HashSet::new();
    let mut verified = Vec::new();
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
        let is_directory = fs::metadata(&canonical)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);
        if !is_directory {
            failures.push("handler_path_invalid".to_owned());
            continue;
        }
        match verifier.verify_handler(&canonical) {
            Ok(()) => verified.push(canonical),
            Err(error) => failures.push(error.code),
        }
    }

    verified.into_iter().next().ok_or_else(|| {
        CommandError::new(
            "codex_identity_unverified",
            "no registered ChatGPT handler passed the pinned OpenAI code-signing requirement",
        )
        .with_details(json!({
            "candidate_count": candidate_count,
            "verification_failures": failures,
        }))
    })
}

#[cfg(target_os = "macos")]
mod macos_codex {
    use super::{
        CodexHandlerRegistry, CodexHandlerVerifier, CodexUrlOpener, CommandError, Path, PathBuf,
        Url,
    };
    use block2::RcBlock;
    use core_foundation::url::CFURL;
    use objc2_app_kit::{NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration};
    use objc2_foundation::{NSArray, NSError, NSString, NSURL};
    use security_framework::os::macos::code_signing::{Flags, SecRequirement, SecStaticCode};
    use std::sync::mpsc;
    use std::time::Duration;

    const CODEX_PINNED_REQUIREMENT: &str = concat!(
        "identifier \"com.openai.codex\" and anchor apple generic ",
        "and certificate 1[field.1.2.840.113635.100.6.2.6] exists ",
        "and certificate leaf[field.1.2.840.113635.100.6.1.13] exists ",
        "and certificate leaf[subject.OU] = \"2DC432GLL2\""
    );
    const CODEX_OPEN_COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);

    pub struct MacCodexRegistry;
    pub struct PinnedCodexVerifier;
    pub struct MacCodexOpener;

    impl CodexHandlerRegistry for MacCodexRegistry {
        fn handlers_for_probe(&self, probe: &Url) -> Result<Vec<PathBuf>, CommandError> {
            let probe = ns_url(probe, "ChatGPT handler probe")?;
            let workspace = NSWorkspace::sharedWorkspace();
            let applications = workspace.URLsForApplicationsToOpenURL(&probe);
            Ok(applications
                .iter()
                .filter(|application| application.isFileURL())
                .filter_map(|application| application.path())
                .map(|path| PathBuf::from(path.to_string()))
                .collect())
        }
    }

    impl CodexHandlerVerifier for PinnedCodexVerifier {
        fn verify_handler(&self, application_path: &Path) -> Result<(), CommandError> {
            let application_url = CFURL::from_path(application_path, true).ok_or_else(|| {
                CommandError::new(
                    "codex_signature_check_failed",
                    "cannot create a Security.framework URL for the ChatGPT application",
                )
            })?;
            let code =
                SecStaticCode::from_path(&application_url, Flags::NONE).map_err(|error| {
                    CommandError::new(
                        "codex_signature_check_failed",
                        format!("cannot inspect the ChatGPT application signature: {error}"),
                    )
                })?;
            let requirement: SecRequirement =
                CODEX_PINNED_REQUIREMENT.parse().map_err(|error| {
                    CommandError::new(
                        "codex_signature_check_failed",
                        format!("cannot compile the pinned ChatGPT signature requirement: {error}"),
                    )
                })?;
            let flags =
                Flags::CHECK_ALL_ARCHITECTURES | Flags::CHECK_NESTED_CODE | Flags::STRICT_VALIDATE;
            code.check_validity(flags, &requirement).map_err(|error| {
                CommandError::new(
                    "codex_signature_untrusted",
                    format!("ChatGPT application signature did not match OpenAI: {error}"),
                )
            })
        }
    }

    impl CodexUrlOpener for MacCodexOpener {
        fn open_with_application(
            &self,
            application_path: &Path,
            deep_link: &Url,
        ) -> Result<(), CommandError> {
            let deep_link = ns_url(deep_link, "ChatGPT deep link")?;
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
            match receiver.recv_timeout(CODEX_OPEN_COMPLETION_TIMEOUT) {
                Ok((true, true)) => Ok(()),
                Ok(_) => Err(CommandError::new(
                    "agent_launch_failed",
                    "macOS could not open the handoff with the verified ChatGPT application",
                )),
                Err(mpsc::RecvTimeoutError::Timeout) => Err(CommandError::new(
                    "agent_launch_failed",
                    "macOS did not confirm the ChatGPT open request in time",
                )),
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(CommandError::new(
                    "agent_launch_failed",
                    "macOS ended the ChatGPT open request before reporting a result",
                )),
            }
        }
    }

    fn ns_url(url: &Url, label: &str) -> Result<objc2::rc::Retained<NSURL>, CommandError> {
        let value = NSString::from_str(url.as_str());
        NSURL::URLWithString(&value).ok_or_else(|| {
            CommandError::new(
                "agent_launch_failed",
                format!("macOS could not construct the {label}"),
            )
        })
    }
}

fn launch_claude_background(
    worktree: PathBuf,
    startup_prompt: String,
) -> Result<LaunchAgentResult, CommandError> {
    let executable = find_executable_on_path("claude").ok_or_else(|| {
        CommandError::new("claude_not_found", "Claude Code was not found on PATH")
    })?;
    launch_claude_background_with_executable(
        worktree,
        startup_prompt,
        &executable,
        ClaudeLaunchTiming::production(),
    )
}

fn launch_claude_background_with_executable(
    worktree: PathBuf,
    startup_prompt: String,
    executable: &Path,
    timing: ClaudeLaunchTiming,
) -> Result<LaunchAgentResult, CommandError> {
    let executable = canonical_executable(executable, "Claude Code")?;
    let before = list_claude_agents(
        &executable,
        &worktree,
        timing.agents_timeout,
        "before launch",
    )?;
    let launched_at = Utc::now();
    let launch = run_claude_background_command(
        &executable,
        &worktree,
        &startup_prompt,
        timing.launch_timeout,
    )?;

    let deadline = Instant::now() + timing.verification_timeout;
    let mut last_poll_error = None;
    let mut ambiguous_ids = Vec::new();

    loop {
        match list_claude_agents(
            &executable,
            &worktree,
            timing.agents_timeout,
            "after launch",
        ) {
            Ok(after) => match select_new_claude_agent(
                &before,
                &after,
                &worktree,
                launched_at,
                &launch.stdout,
            ) {
                CandidateSelection::Verified(agent) => {
                    return Ok(LaunchAgentResult {
                        agent: AgentProvider::ClaudeCode,
                        worktree_path: worktree.to_string_lossy().into_owned(),
                        executable_path: executable.to_string_lossy().into_owned(),
                        process_id: launch.process_id,
                        launch_mode: "background".into(),
                        startup_prompt,
                        verification_status: VERIFICATION_VERIFIED.into(),
                        session_id: Some(agent.id),
                        session_state: agent.state,
                        waiting_reason: agent.waiting_reason,
                    });
                }
                CandidateSelection::Ambiguous(ids) => ambiguous_ids = ids,
                CandidateSelection::None => ambiguous_ids.clear(),
            },
            Err(error) => last_poll_error = Some(error.to_string()),
        }

        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep(
            timing
                .poll_interval
                .min(deadline.saturating_duration_since(now)),
        );
    }

    Err(unverified_claude_error(
        &launch,
        ambiguous_ids,
        last_poll_error,
    ))
}

fn list_claude_agents(
    executable: &Path,
    worktree: &Path,
    timeout: Duration,
    phase: &str,
) -> Result<Vec<ClaudeAgentRecord>, CommandError> {
    let args = claude_agents_args(worktree);
    let output = run_process(
        executable,
        &args,
        None,
        timeout,
        MAX_AGENT_LIST_BYTES,
        64 * 1024,
        &[],
    )
    .map_err(|error| claude_agents_process_error(error, phase))?;

    if !output.status.success() {
        let stderr = output_excerpt(&String::from_utf8_lossy(&output.stderr));
        let detail = stderr.map(|value| format!(": {value}")).unwrap_or_default();
        return Err(CommandError::new(
            "claude_agents_failed",
            format!(
                "Claude Code could not list background sessions {phase} (status {}){detail}",
                output.status
            ),
        ));
    }
    if output.stdout_truncated {
        return Err(CommandError::new(
            "claude_agents_invalid_response",
            "Claude Code returned an oversized background session list",
        ));
    }
    parse_claude_agents(&output.stdout)
}

fn claude_agents_process_error(error: ProcessRunError, phase: &str) -> CommandError {
    CommandError::new(
        "claude_agents_failed",
        format!("Claude Code could not list background sessions {phase}: {error}"),
    )
}

fn run_claude_background_command(
    executable: &Path,
    worktree: &Path,
    prompt: &str,
    timeout: Duration,
) -> Result<ClaudeLaunchOutcome, CommandError> {
    let mut stdout_file = tempfile::tempfile().map_err(|error| {
        CommandError::new(
            "agent_launch_failed",
            format!("cannot create Claude Code stdout capture: {error}"),
        )
    })?;
    let mut stderr_file = tempfile::tempfile().map_err(|error| {
        CommandError::new(
            "agent_launch_failed",
            format!("cannot create Claude Code stderr capture: {error}"),
        )
    })?;
    let child_stdout = stdout_file.try_clone().map_err(|error| {
        CommandError::new(
            "agent_launch_failed",
            format!("cannot prepare Claude Code stdout capture: {error}"),
        )
    })?;
    let child_stderr = stderr_file.try_clone().map_err(|error| {
        CommandError::new(
            "agent_launch_failed",
            format!("cannot prepare Claude Code stderr capture: {error}"),
        )
    })?;

    let args = claude_background_args(prompt);
    let mut child = Command::new(executable)
        .args(&args)
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::from(child_stdout))
        .stderr(Stdio::from(child_stderr))
        .spawn()
        .map_err(|error| {
            CommandError::new(
                "agent_launch_failed",
                format!("cannot start a Claude Code background session: {error}"),
            )
        })?;
    let process_id = child.id();
    let (status, timed_out) = match child.wait_timeout(timeout).map_err(|error| {
        CommandError::new(
            "agent_launch_failed",
            format!("cannot wait for the Claude Code launcher: {error}"),
        )
    })? {
        Some(status) => (Some(status), false),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            (None, true)
        }
    };

    let stdout = read_capture(&mut stdout_file).map_err(|error| {
        CommandError::new(
            "agent_launch_failed",
            format!("cannot read Claude Code stdout: {error}"),
        )
    })?;
    let stderr = read_capture(&mut stderr_file).map_err(|error| {
        CommandError::new(
            "agent_launch_failed",
            format!("cannot read Claude Code stderr: {error}"),
        )
    })?;

    Ok(ClaudeLaunchOutcome {
        process_id,
        status,
        timed_out,
        stdout,
        stderr,
    })
}

fn read_capture(file: &mut File) -> std::io::Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(MAX_LAUNCH_OUTPUT_BYTES).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn unverified_claude_error(
    launch: &ClaudeLaunchOutcome,
    ambiguous_ids: Vec<String>,
    last_poll_error: Option<String>,
) -> CommandError {
    let (code, message, launcher_status) = if launch.timed_out {
        (
            "agent_launch_unverified",
            "Claude Code launcher timed out and no new background session was verified".into(),
            "timed_out".to_owned(),
        )
    } else if let Some(status) = launch.status {
        if status.success() {
            (
                "agent_launch_unverified",
                "Claude Code launcher exited successfully, but no new background session was verified"
                    .into(),
                "exited_successfully".to_owned(),
            )
        } else {
            let detail = output_excerpt(&launch.stderr)
                .or_else(|| output_excerpt(&launch.stdout))
                .map(|value| format!(": {value}"))
                .unwrap_or_default();
            (
                "agent_launch_failed",
                format!(
                    "Claude Code launcher exited with status {status}; no new background session was verified{detail}"
                ),
                format!("exit_{status}"),
            )
        }
    } else {
        (
            "agent_launch_unverified",
            "Claude Code launch ended without a verifiable background session".into(),
            "unknown".to_owned(),
        )
    };

    CommandError::new(code, message).with_details(json!({
        "verification_status": VERIFICATION_UNVERIFIED,
        "process_id": launch.process_id,
        "launcher_status": launcher_status,
        "ambiguous_session_ids": ambiguous_ids,
        "last_poll_error": last_poll_error,
    }))
}

fn output_excerpt(value: &str) -> Option<String> {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        None
    } else {
        Some(compact.chars().take(800).collect())
    }
}

fn claude_background_args(prompt: &str) -> [OsString; 2] {
    [OsString::from("--bg"), OsString::from(prompt)]
}

fn claude_agents_args(worktree: &Path) -> [OsString; 5] {
    [
        OsString::from("agents"),
        OsString::from("--json"),
        OsString::from("--all"),
        OsString::from("--cwd"),
        worktree.as_os_str().to_owned(),
    ]
}

fn parse_claude_agents(bytes: &[u8]) -> Result<Vec<ClaudeAgentRecord>, CommandError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        CommandError::new(
            "claude_agents_invalid_response",
            format!("Claude Code returned invalid agent JSON: {error}"),
        )
    })?;
    let entries = value
        .as_array()
        .or_else(|| value.get("agents").and_then(Value::as_array))
        .or_else(|| value.get("sessions").and_then(Value::as_array))
        .ok_or_else(|| {
            CommandError::new(
                "claude_agents_invalid_response",
                "Claude Code agent JSON must be an array",
            )
        })?;

    Ok(entries
        .iter()
        .filter_map(parse_claude_agent_record)
        .collect())
}

fn parse_claude_agent_record(value: &Value) -> Option<ClaudeAgentRecord> {
    let id = first_string(value, &["id", "jobId", "job_id", "sessionId", "session_id"])?;
    let kinds = all_strings(value, &["kind", "sessionKind", "session_kind"]);
    let cwds = all_strings(
        value,
        &[
            "canonicalCwd",
            "canonical_cwd",
            "cwd",
            "originCwd",
            "origin_cwd",
        ],
    );
    let started_at = all_values(
        value,
        &["startedAt", "started_at", "createdAt", "created_at"],
    )
    .into_iter()
    .filter_map(parse_timestamp)
    .collect();
    let state = first_string(value, &["state", "status"]);
    let waiting_reason = first_string(
        value,
        &[
            "waitingReason",
            "waiting_reason",
            "waitingFor",
            "waiting_for",
            "waitReason",
            "needs",
        ],
    );

    Some(ClaudeAgentRecord {
        id,
        kinds,
        cwds,
        started_at,
        state,
        waiting_reason,
    })
}

fn first_string(value: &Value, names: &[&str]) -> Option<String> {
    all_strings(value, names).into_iter().next()
}

fn all_strings(value: &Value, names: &[&str]) -> Vec<String> {
    all_values(value, names)
        .into_iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn all_values<'a>(value: &'a Value, names: &[&str]) -> Vec<&'a Value> {
    let Some(root) = value.as_object() else {
        return Vec::new();
    };
    let mut values = Vec::new();
    for name in names {
        if let Some(value) = root.get(*name) {
            values.push(value);
        }
    }
    for container_name in ["agent", "session", "metadata", "state"] {
        let Some(container) = root.get(container_name).and_then(Value::as_object) else {
            continue;
        };
        for name in names {
            if let Some(value) = container.get(*name) {
                values.push(value);
            }
        }
    }
    values
}

fn parse_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(value) = value.as_str() {
        if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
            return Some(parsed.with_timezone(&Utc));
        }
        return value.parse::<f64>().ok().and_then(timestamp_from_number);
    }
    value.as_f64().and_then(timestamp_from_number)
}

fn timestamp_from_number(value: f64) -> Option<DateTime<Utc>> {
    if !value.is_finite() {
        return None;
    }
    let milliseconds = if value.abs() >= 10_000_000_000.0 {
        value.round() as i64
    } else {
        (value * 1000.0).round() as i64
    };
    Utc.timestamp_millis_opt(milliseconds).single()
}

fn select_new_claude_agent(
    before: &[ClaudeAgentRecord],
    after: &[ClaudeAgentRecord],
    worktree: &Path,
    launched_at: DateTime<Utc>,
    launcher_stdout: &str,
) -> CandidateSelection {
    let previous_ids: HashSet<&str> = before.iter().map(|agent| agent.id.as_str()).collect();
    let earliest_start = launched_at - ChronoDuration::seconds(CLAUDE_STARTED_AT_SLOP_SECONDS);
    let mut seen_ids = HashSet::new();
    let mut candidates: Vec<_> = after
        .iter()
        .filter(|agent| !previous_ids.contains(agent.id.as_str()))
        .filter(|agent| seen_ids.insert(agent.id.as_str()))
        .filter(|agent| is_background_agent(agent))
        .filter(|agent| agent_matches_worktree(agent, worktree))
        .filter(|agent| {
            agent
                .started_at
                .iter()
                .any(|started_at| *started_at >= earliest_start)
        })
        .cloned()
        .collect();
    candidates.sort_by(|left, right| left.id.cmp(&right.id));

    match candidates.len() {
        0 => CandidateSelection::None,
        1 => CandidateSelection::Verified(candidates.remove(0)),
        _ => {
            let mut stdout_matches: Vec<_> = candidates
                .iter()
                .filter(|candidate| stdout_mentions_id(launcher_stdout, &candidate.id))
                .cloned()
                .collect();
            if stdout_matches.len() == 1 {
                CandidateSelection::Verified(stdout_matches.remove(0))
            } else {
                CandidateSelection::Ambiguous(
                    candidates
                        .into_iter()
                        .map(|candidate| candidate.id)
                        .collect(),
                )
            }
        }
    }
}

fn is_background_agent(agent: &ClaudeAgentRecord) -> bool {
    agent.kinds.iter().any(|kind| {
        matches!(
            kind.trim().to_ascii_lowercase().as_str(),
            "background" | "bg" | "background_agent" | "background-agent"
        )
    })
}

fn agent_matches_worktree(agent: &ClaudeAgentRecord, worktree: &Path) -> bool {
    agent
        .cwds
        .iter()
        .filter_map(|cwd| fs::canonicalize(cwd).ok())
        .any(|cwd| cwd == worktree)
}

fn stdout_mentions_id(stdout: &str, id: &str) -> bool {
    stdout
        .split(|character: char| {
            !character.is_ascii_alphanumeric() && character != '-' && character != '_'
        })
        .any(|token| token == id)
}

fn codex_deep_link(worktree: &Path, prompt: &str) -> Result<Url, CommandError> {
    let mut url = Url::parse("codex://new").map_err(|_| {
        CommandError::new("agent_launch_failed", "cannot construct ChatGPT deep link")
    })?;
    url.query_pairs_mut()
        .append_pair("path", &worktree.to_string_lossy())
        .append_pair("prompt", prompt);
    Ok(url)
}

fn canonical_handoff(path: &Path, worktree: &Path) -> Result<PathBuf, CommandError> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        CommandError::new(
            "invalid_handoff_path",
            format!("HANDOFF.md cannot be resolved: {error}"),
        )
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| {
        CommandError::new(
            "invalid_handoff_path",
            format!("cannot inspect HANDOFF.md: {error}"),
        )
    })?;
    if !metadata.is_file() || metadata.len() > MAX_HANDOFF_BYTES {
        return Err(CommandError::new(
            "invalid_handoff_path",
            "handoff must be an ordinary HANDOFF.md file no larger than 2 MiB",
        ));
    }
    if canonical.file_name().and_then(|value| value.to_str()) != Some("HANDOFF.md") {
        return Err(CommandError::new(
            "invalid_handoff_path",
            "handoff file must be named HANDOFF.md",
        ));
    }
    let relative = canonical.strip_prefix(worktree).map_err(|_| {
        CommandError::new(
            "invalid_handoff_path",
            "HANDOFF.md must be located inside the restored worktree",
        )
    })?;
    if relative.as_os_str().is_empty() {
        return Err(CommandError::new(
            "invalid_handoff_path",
            "HANDOFF.md must be a file inside the restored worktree",
        ));
    }
    Ok(canonical)
}

fn startup_prompt(handoff: &Path) -> String {
    format!(
        "Read the Relay handoff at '{}' before doing any work. Treat tool records as historical evidence and never replay them automatically. Continue the task in this workspace.",
        handoff.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[derive(Default)]
    struct FakeRegistry {
        handlers: Vec<PathBuf>,
        probes: RefCell<Vec<String>>,
    }

    impl CodexHandlerRegistry for FakeRegistry {
        fn handlers_for_probe(&self, probe: &Url) -> Result<Vec<PathBuf>, CommandError> {
            self.probes.borrow_mut().push(probe.as_str().to_owned());
            Ok(self.handlers.clone())
        }
    }

    #[derive(Default)]
    struct FakeVerifier {
        trusted: HashSet<PathBuf>,
        calls: RefCell<Vec<PathBuf>>,
    }

    impl CodexHandlerVerifier for FakeVerifier {
        fn verify_handler(&self, application_path: &Path) -> Result<(), CommandError> {
            self.calls.borrow_mut().push(application_path.to_path_buf());
            if self.trusted.contains(application_path) {
                Ok(())
            } else {
                Err(CommandError::new(
                    "codex_signature_untrusted",
                    "fake signature did not match",
                ))
            }
        }
    }

    #[derive(Default)]
    struct FakeOpener {
        calls: RefCell<Vec<(PathBuf, String)>>,
        fail: bool,
    }

    impl CodexUrlOpener for FakeOpener {
        fn open_with_application(
            &self,
            application_path: &Path,
            deep_link: &Url,
        ) -> Result<(), CommandError> {
            self.calls.borrow_mut().push((
                application_path.to_path_buf(),
                deep_link.as_str().to_owned(),
            ));
            if self.fail {
                Err(CommandError::new(
                    "agent_launch_failed",
                    "fake opener rejected the request",
                ))
            } else {
                Ok(())
            }
        }
    }

    fn fast_timing() -> ClaudeLaunchTiming {
        ClaudeLaunchTiming {
            launch_timeout: Duration::from_secs(2),
            agents_timeout: Duration::from_secs(2),
            verification_timeout: Duration::from_millis(500),
            poll_interval: Duration::from_millis(10),
        }
    }

    fn shell_quote(value: &Path) -> String {
        format!("'{}'", value.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    fn write_fake_claude(directory: &TempDir, script_body: &str) -> PathBuf {
        let executable = directory.path().join("claude-fake");
        fs::write(&executable, format!("#!/bin/sh\nset -eu\n{script_body}\n")).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        executable
    }

    fn fake_script(
        state_file: &Path,
        log_file: &Path,
        worktree: &Path,
        background_body: &str,
    ) -> String {
        let agent_json = serde_json::to_string(&json!([{
            "id": "relay-session-new",
            "kind": "background",
            "cwd": worktree.to_string_lossy(),
            "startedAt": "2999-01-01T00:00:00Z",
            "state": "working",
            "waitingReason": "waiting for permission"
        }]))
        .unwrap();
        format!(
            r#"state={state}
log={log}
printf '%s\n' "$*" >> "$log"
if [ "$1" = "agents" ]; then
  if [ -f "$state" ]; then
    printf '%s\n' {agent_json}
  else
    printf '%s\n' '[]'
  fi
  exit 0
fi
if [ "$1" = "--bg" ]; then
  {background_body}
fi
exit 71"#,
            state = shell_quote(state_file),
            log = shell_quote(log_file),
            agent_json = shell_quote(Path::new(&agent_json)),
        )
    }

    #[test]
    fn prompt_names_handoff_and_forbids_tool_replay() {
        let prompt = startup_prompt(Path::new("/tmp/Relay handoff/HANDOFF.md"));
        assert!(prompt.contains("/tmp/Relay handoff/HANDOFF.md"));
        assert!(prompt.contains("never replay"));
    }

    #[test]
    fn handoff_must_resolve_to_an_ordinary_file_inside_the_worktree() {
        let directory = tempfile::tempdir().unwrap();
        let worktree = directory.path().join("worktree");
        let outside = directory.path().join("outside");
        fs::create_dir(&worktree).unwrap();
        fs::create_dir(&outside).unwrap();
        let worktree = fs::canonicalize(worktree).unwrap();

        let inside_handoff = worktree.join("HANDOFF.md");
        fs::write(&inside_handoff, "inside").unwrap();
        assert_eq!(
            canonical_handoff(&inside_handoff, &worktree).unwrap(),
            fs::canonicalize(&inside_handoff).unwrap()
        );

        let outside_handoff = outside.join("HANDOFF.md");
        fs::write(&outside_handoff, "outside").unwrap();
        let outside_error = canonical_handoff(&outside_handoff, &worktree).unwrap_err();
        assert_eq!(outside_error.code, "invalid_handoff_path");

        let linked_handoff = worktree.join("nested").join("HANDOFF.md");
        fs::create_dir(linked_handoff.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside_handoff, &linked_handoff).unwrap();
        let linked_error = canonical_handoff(&linked_handoff, &worktree).unwrap_err();
        assert_eq!(linked_error.code, "invalid_handoff_path");
    }

    #[test]
    fn claude_background_uses_two_literal_arguments() {
        let prompt = "Read /tmp/$HOME; touch PWNED/HANDOFF.md";
        let args = claude_background_args(prompt);
        assert_eq!(args[0], "--bg");
        assert_eq!(args[1], prompt);
        assert!(!args.iter().any(|arg| {
            matches!(
                arg.to_str(),
                Some("-c" | "--resume" | "--continue" | "--dangerously-skip-permissions")
            )
        }));
    }

    #[test]
    fn claude_agents_uses_literal_filter_arguments() {
        let worktree = Path::new("/tmp/Relay $HOME; ' 中文");
        let args = claude_agents_args(worktree);
        assert_eq!(args[0], "agents");
        assert_eq!(args[1], "--json");
        assert_eq!(args[2], "--all");
        assert_eq!(args[3], "--cwd");
        assert_eq!(args[4], worktree.as_os_str());
    }

    #[test]
    fn candidate_requires_new_background_id_matching_canonical_cwd_and_time() {
        let directory = tempfile::tempdir().unwrap();
        let worktree = fs::canonicalize(directory.path()).unwrap();
        let launched_at = Utc::now();
        let before = parse_claude_agents(
            serde_json::to_string(&json!([{
                "id": "old",
                "kind": "background",
                "cwd": worktree,
                "startedAt": launched_at.to_rfc3339(),
            }]))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        let after = parse_claude_agents(
            serde_json::to_string(&json!([
                {
                    "id": "old",
                    "kind": "background",
                    "cwd": worktree,
                    "startedAt": launched_at.to_rfc3339(),
                },
                {
                    "id": "new",
                    "kind": "background",
                    "canonicalCwd": worktree,
                    "startedAt": (launched_at + ChronoDuration::seconds(1)).timestamp_millis(),
                    "state": "a-future-state-we-do-not-know",
                    "waitingFor": "approval required",
                },
                {
                    "id": "wrong-kind",
                    "kind": "foreground",
                    "cwd": worktree,
                    "startedAt": (launched_at + ChronoDuration::seconds(1)).to_rfc3339(),
                },
                {
                    "id": "too-old",
                    "kind": "background",
                    "cwd": worktree,
                    "startedAt": (launched_at - ChronoDuration::minutes(1)).to_rfc3339(),
                }
            ]))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();

        let CandidateSelection::Verified(candidate) =
            select_new_claude_agent(&before, &after, &worktree, launched_at, "old")
        else {
            panic!("expected one verified candidate");
        };
        assert_eq!(candidate.id, "new");
        assert_eq!(
            candidate.state.as_deref(),
            Some("a-future-state-we-do-not-know")
        );
        assert_eq!(
            candidate.waiting_reason.as_deref(),
            Some("approval required")
        );
    }

    #[test]
    fn stdout_id_only_breaks_a_real_candidate_tie() {
        let directory = tempfile::tempdir().unwrap();
        let worktree = fs::canonicalize(directory.path()).unwrap();
        let launched_at = Utc::now();
        let make_record = |id: &str| ClaudeAgentRecord {
            id: id.into(),
            kinds: vec!["background".into()],
            cwds: vec![worktree.to_string_lossy().into_owned()],
            started_at: vec![launched_at + ChronoDuration::seconds(1)],
            state: None,
            waiting_reason: None,
        };
        let after = vec![make_record("candidate-one"), make_record("candidate-two")];

        let CandidateSelection::Verified(candidate) =
            select_new_claude_agent(&[], &after, &worktree, launched_at, "created candidate-two")
        else {
            panic!("stdout should resolve the candidate tie");
        };
        assert_eq!(candidate.id, "candidate-two");

        assert!(matches!(
            select_new_claude_agent(&[], &[], &worktree, launched_at, "candidate-two"),
            CandidateSelection::None
        ));
    }

    #[test]
    fn fake_claude_verifies_new_session_after_successful_launcher_exit() {
        let directory = tempfile::tempdir().unwrap();
        let worktree = directory.path().join("Relay worktree");
        fs::create_dir(&worktree).unwrap();
        let worktree = fs::canonicalize(worktree).unwrap();
        let state_file = directory.path().join("state");
        let log_file = directory.path().join("calls.log");
        let script = fake_script(
            &state_file,
            &log_file,
            &worktree,
            &format!(
                ": > {}; printf '%s\\n' 'relay-session-new'; exit 0",
                shell_quote(&state_file)
            ),
        );
        let executable = write_fake_claude(&directory, &script);

        let result = launch_claude_background_with_executable(
            worktree.clone(),
            "Read '$HOME; no shell' exactly".into(),
            &executable,
            fast_timing(),
        )
        .unwrap();

        assert_eq!(result.verification_status, VERIFICATION_VERIFIED);
        assert_eq!(result.session_id.as_deref(), Some("relay-session-new"));
        assert_eq!(result.session_state.as_deref(), Some("working"));
        assert_eq!(
            result.waiting_reason.as_deref(),
            Some("waiting for permission")
        );
        assert_eq!(
            Path::new(&result.executable_path),
            fs::canonicalize(executable).unwrap()
        );
        let calls = fs::read_to_string(log_file).unwrap();
        assert!(calls
            .lines()
            .any(|line| line.starts_with("agents --json --all --cwd ")));
        assert!(calls.lines().any(|line| line.starts_with("--bg ")));
        assert!(
            calls
                .lines()
                .filter(|line| line.starts_with("agents "))
                .count()
                >= 2
        );
    }

    #[test]
    fn launcher_timeout_still_polls_and_can_verify_the_new_session() {
        let directory = tempfile::tempdir().unwrap();
        let worktree = directory.path().join("worktree");
        fs::create_dir(&worktree).unwrap();
        let worktree = fs::canonicalize(worktree).unwrap();
        let state_file = directory.path().join("state");
        let log_file = directory.path().join("calls.log");
        let script = fake_script(
            &state_file,
            &log_file,
            &worktree,
            &format!(": > {}; exec sleep 5", shell_quote(&state_file)),
        );
        let executable = write_fake_claude(&directory, &script);
        let mut timing = fast_timing();
        timing.launch_timeout = Duration::from_millis(30);

        let result = launch_claude_background_with_executable(
            worktree,
            "Read HANDOFF.md".into(),
            &executable,
            timing,
        )
        .unwrap();
        assert_eq!(result.verification_status, VERIFICATION_VERIFIED);
        assert_eq!(result.session_id.as_deref(), Some("relay-session-new"));
    }

    #[test]
    fn successful_launcher_without_new_id_is_unverified() {
        let directory = tempfile::tempdir().unwrap();
        let worktree = directory.path().join("worktree");
        fs::create_dir(&worktree).unwrap();
        let worktree = fs::canonicalize(worktree).unwrap();
        let state_file = directory.path().join("state");
        let log_file = directory.path().join("calls.log");
        let script = fake_script(&state_file, &log_file, &worktree, "exit 0");
        let executable = write_fake_claude(&directory, &script);
        let mut timing = fast_timing();
        timing.verification_timeout = Duration::from_millis(30);

        let error = launch_claude_background_with_executable(
            worktree,
            "Read HANDOFF.md".into(),
            &executable,
            timing,
        )
        .unwrap_err();
        assert_eq!(error.code, "agent_launch_unverified");
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|details| details.get("verification_status"))
                .and_then(Value::as_str),
            Some(VERIFICATION_UNVERIFIED)
        );
    }

    #[test]
    fn codex_verifies_every_candidate_before_opening_with_the_trusted_application() {
        let directory = tempfile::tempdir().unwrap();
        let worktree = directory.path().join("Relay worktree");
        let untrusted = directory.path().join("Untrusted.app");
        let trusted = directory.path().join("ChatGPT.app");
        fs::create_dir(&worktree).unwrap();
        fs::create_dir(&untrusted).unwrap();
        fs::create_dir(&trusted).unwrap();
        let worktree = fs::canonicalize(worktree).unwrap();
        let untrusted = fs::canonicalize(untrusted).unwrap();
        let trusted = fs::canonicalize(trusted).unwrap();
        let registry = FakeRegistry {
            handlers: vec![untrusted.clone(), trusted.clone()],
            ..FakeRegistry::default()
        };
        let verifier = FakeVerifier {
            trusted: HashSet::from([trusted.clone()]),
            ..FakeVerifier::default()
        };
        let opener = FakeOpener::default();

        let result = launch_codex_with_services(
            worktree.clone(),
            "Read HANDOFF.md?x=1&y=2".into(),
            &registry,
            &verifier,
            &opener,
        )
        .unwrap();

        assert_eq!(result.verification_status, VERIFICATION_OPEN_REQUESTED);
        assert_eq!(result.process_id, 0);
        assert!(result.session_id.is_none());
        assert_eq!(Path::new(&result.executable_path), trusted);
        assert_eq!(
            registry.probes.borrow().as_slice(),
            [CODEX_HANDLER_PROBE_URL]
        );
        assert!(!registry.probes.borrow()[0].contains(&worktree.to_string_lossy().to_string()));
        assert_eq!(
            verifier.calls.borrow().as_slice(),
            [untrusted, trusted.clone()]
        );
        let calls = opener.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, trusted);
        let opened = Url::parse(&calls[0].1).unwrap();
        let pairs: std::collections::HashMap<_, _> = opened.query_pairs().into_owned().collect();
        let expected_worktree = worktree.to_string_lossy().into_owned();
        assert_eq!(
            pairs.get("path").map(String::as_str),
            Some(expected_worktree.as_str())
        );
        assert_eq!(
            pairs.get("prompt").map(String::as_str),
            Some("Read HANDOFF.md?x=1&y=2")
        );
    }

    #[test]
    fn missing_codex_handler_fails_before_verification_or_sensitive_url_opening() {
        let directory = tempfile::tempdir().unwrap();
        let worktree = fs::canonicalize(directory.path()).unwrap();
        let registry = FakeRegistry::default();
        let verifier = FakeVerifier::default();
        let opener = FakeOpener::default();
        let error = launch_codex_with_services(
            worktree,
            "SECRET HANDOFF PATH".into(),
            &registry,
            &verifier,
            &opener,
        )
        .unwrap_err();
        assert_eq!(error.code, "codex_handler_not_found");
        assert_eq!(
            registry.probes.borrow().as_slice(),
            [CODEX_HANDLER_PROBE_URL]
        );
        assert!(verifier.calls.borrow().is_empty());
        assert!(opener.calls.borrow().is_empty());
    }

    #[test]
    fn untrusted_codex_handlers_fail_closed_without_opening_the_sensitive_url() {
        let directory = tempfile::tempdir().unwrap();
        let handler = directory.path().join("Spoofed ChatGPT.app");
        fs::create_dir(&handler).unwrap();
        let handler = fs::canonicalize(handler).unwrap();
        let registry = FakeRegistry {
            handlers: vec![handler],
            ..FakeRegistry::default()
        };
        let verifier = FakeVerifier::default();
        let opener = FakeOpener::default();

        let error = launch_codex_with_services(
            fs::canonicalize(directory.path()).unwrap(),
            "SECRET HANDOFF PATH".into(),
            &registry,
            &verifier,
            &opener,
        )
        .unwrap_err();
        assert_eq!(error.code, "codex_identity_unverified");
        assert_eq!(verifier.calls.borrow().len(), 1);
        assert!(opener.calls.borrow().is_empty());
        assert_eq!(
            registry.probes.borrow().as_slice(),
            [CODEX_HANDLER_PROBE_URL]
        );
    }

    #[test]
    fn codex_open_failure_is_not_reported_as_open_requested() {
        let directory = tempfile::tempdir().unwrap();
        let handler = directory.path().join("ChatGPT.app");
        fs::create_dir(&handler).unwrap();
        let handler = fs::canonicalize(handler).unwrap();
        let registry = FakeRegistry {
            handlers: vec![handler.clone()],
            ..FakeRegistry::default()
        };
        let verifier = FakeVerifier {
            trusted: HashSet::from([handler]),
            ..FakeVerifier::default()
        };
        let opener = FakeOpener {
            fail: true,
            ..FakeOpener::default()
        };

        let error = launch_codex_with_services(
            fs::canonicalize(directory.path()).unwrap(),
            "Read HANDOFF.md".into(),
            &registry,
            &verifier,
            &opener,
        )
        .unwrap_err();
        assert_eq!(error.code, "agent_launch_failed");
        assert_eq!(opener.calls.borrow().len(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires installed official ChatGPT app"]
    fn macos_registry_probe_contains_no_workspace_or_prompt_data() {
        let registry = macos_codex::MacCodexRegistry;
        let probe = codex_handler_probe_url().unwrap();
        let handlers = registry.handlers_for_probe(&probe).unwrap();
        assert_eq!(probe.as_str(), CODEX_HANDLER_PROBE_URL);
        assert!(!probe.as_str().contains("path="));
        assert!(!probe.as_str().contains("prompt="));
        assert!(!handlers.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires installed official ChatGPT app"]
    fn pinned_requirement_accepts_an_installed_official_chatgpt_handler() {
        let registry = macos_codex::MacCodexRegistry;
        let handlers = registry
            .handlers_for_probe(&codex_handler_probe_url().unwrap())
            .unwrap();
        let verifier = macos_codex::PinnedCodexVerifier;
        let verified = select_verified_codex_handler(handlers, &verifier).unwrap();
        assert!(verified.is_dir());
    }

    #[test]
    fn codex_deep_link_round_trips_hostile_paths_and_prompt() {
        let worktree = Path::new("/tmp/Relay $HOME; ' 中文");
        let prompt = "Read /tmp/HANDOFF.md?x=1&y=2 first";
        let url = codex_deep_link(worktree, prompt).unwrap();
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            pairs.get("path").map(String::as_str),
            Some("/tmp/Relay $HOME; ' 中文")
        );
        assert_eq!(pairs.get("prompt").map(String::as_str), Some(prompt));
        assert_eq!(pairs.len(), 2);
    }
}
