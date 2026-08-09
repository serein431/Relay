use crate::types::CommandError;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;
use wait_timeout::ChildExt;

#[derive(Debug)]
pub struct ProcessOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug)]
pub enum ProcessRunError {
    Spawn(String),
    Stdin(String),
    Wait(String),
    Read(String),
    Timeout { timeout_ms: u128, stderr: String },
}

impl std::fmt::Display for ProcessRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(message) => write!(f, "cannot start process: {message}"),
            Self::Stdin(message) => write!(f, "cannot write process input: {message}"),
            Self::Wait(message) => write!(f, "cannot wait for process: {message}"),
            Self::Read(message) => write!(f, "cannot read process output: {message}"),
            Self::Timeout { timeout_ms, stderr } if stderr.is_empty() => {
                write!(f, "process timed out after {timeout_ms} ms")
            }
            Self::Timeout { timeout_ms, stderr } => {
                write!(
                    f,
                    "process timed out after {timeout_ms} ms; stderr: {stderr}"
                )
            }
        }
    }
}

struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
}

pub fn run_process(
    executable: &Path,
    args: &[OsString],
    stdin_bytes: Option<&[u8]>,
    timeout: Duration,
    max_stdout: usize,
    max_stderr: usize,
    environment: &[(&str, &str)],
) -> Result<ProcessOutput, ProcessRunError> {
    run_process_with_removed_environment(
        executable,
        args,
        stdin_bytes,
        timeout,
        max_stdout,
        max_stderr,
        environment,
        &[],
    )
}

#[allow(clippy::too_many_arguments)]
pub fn run_process_with_removed_environment(
    executable: &Path,
    args: &[OsString],
    stdin_bytes: Option<&[u8]>,
    timeout: Duration,
    max_stdout: usize,
    max_stderr: usize,
    environment: &[(&str, &str)],
    removed_environment: &[&str],
) -> Result<ProcessOutput, ProcessRunError> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(environment.iter().copied());
    for name in removed_environment {
        command.env_remove(name);
    }

    // Give the child its own process group so a timeout can also stop helpers
    // such as git-lfs instead of leaving inherited output pipes open.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|error| ProcessRunError::Spawn(error.to_string()))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProcessRunError::Read("stdout pipe was not created".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProcessRunError::Read("stderr pipe was not created".into()))?;

    let stdout_thread = thread::spawn(move || read_limited(stdout, max_stdout));
    let stderr_thread = thread::spawn(move || read_limited(stderr, max_stderr));

    if let Some(bytes) = stdin_bytes {
        let write_result = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "stdin pipe was not created"))
            .and_then(|mut stdin| {
                stdin.write_all(bytes)?;
                stdin.flush()
            });
        if let Err(error) = write_result {
            terminate_child_tree(&mut child);
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(ProcessRunError::Stdin(error.to_string()));
        }
    }

    let wait_result = child.wait_timeout(timeout);
    let status = match wait_result {
        Err(error) => {
            terminate_child_tree(&mut child);
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(ProcessRunError::Wait(error.to_string()));
        }
        Ok(None) => {
            terminate_child_tree(&mut child);
            let _ = stdout_thread.join();
            let stderr = stderr_thread
                .join()
                .ok()
                .and_then(Result::ok)
                .map(|capture| String::from_utf8_lossy(&capture.bytes).trim().to_owned())
                .unwrap_or_default();
            return Err(ProcessRunError::Timeout {
                timeout_ms: timeout.as_millis(),
                stderr,
            });
        }
        Ok(Some(status)) => status,
    };

    let stdout = stdout_thread
        .join()
        .map_err(|_| ProcessRunError::Read("stdout reader thread panicked".into()))?
        .map_err(|error| ProcessRunError::Read(format!("stdout: {error}")))?;
    let stderr = stderr_thread
        .join()
        .map_err(|_| ProcessRunError::Read("stderr reader thread panicked".into()))?
        .map_err(|error| ProcessRunError::Read(format!("stderr: {error}")))?;

    Ok(ProcessOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
    })
}

fn terminate_child_tree(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        // The child was put in a new process group immediately before spawn.
        // A failed kill is harmless here; child.kill below remains a fallback.
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn read_limited(mut reader: impl Read, limit: usize) -> io::Result<CapturedStream> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0_u8; 16 * 1024];
    let mut truncated = false;

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let keep = remaining.min(read);
        bytes.extend_from_slice(&buffer[..keep]);
        truncated |= keep < read;
    }

    Ok(CapturedStream { bytes, truncated })
}

pub fn canonical_existing_directory(path: &Path, label: &str) -> Result<PathBuf, CommandError> {
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
    if !metadata.is_dir() {
        return Err(CommandError::new(
            "invalid_path",
            format!("{label} must be a directory: {}", canonical.display()),
        ));
    }
    Ok(canonical)
}

pub fn canonical_executable(path: &Path, label: &str) -> Result<PathBuf, CommandError> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        CommandError::new(
            "executable_not_found",
            format!(
                "{label} cannot be resolved at '{}': {error}",
                path.display()
            ),
        )
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| {
        CommandError::new(
            "invalid_executable",
            format!("cannot inspect {label} '{}': {error}", canonical.display()),
        )
    })?;
    if !metadata.is_file() {
        return Err(CommandError::new(
            "invalid_executable",
            format!("{label} must be an ordinary file: {}", canonical.display()),
        ));
    }
    if !is_executable(&metadata) {
        return Err(CommandError::new(
            "invalid_executable",
            format!("{label} is not executable: {}", canonical.display()),
        ));
    }
    Ok(canonical)
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    true
}

pub fn find_executable_on_path(name: &str) -> Option<PathBuf> {
    if name.contains(std::path::MAIN_SEPARATOR) {
        return canonical_executable(Path::new(name), name).ok();
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find_map(|candidate| canonical_executable(&candidate, name).ok())
}

pub fn bytes_to_trimmed_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}
