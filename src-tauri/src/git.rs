use crate::process::{
    bytes_to_trimmed_string, canonical_existing_directory, find_executable_on_path,
    run_process_with_removed_environment, ProcessOutput, ProcessRunError,
};
use crate::types::{
    CommandError, GitFileChange, GitLfsStatus, GitOperationState, GitRemote, GitSubmodule,
    GitSubmoduleWorktreeState, RepositoryInspection, RepositoryWarning,
};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const GIT_SLOW_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_GIT_STDOUT: usize = 64 * 1024 * 1024;
const MAX_GIT_STDERR: usize = 256 * 1024;
const MAX_IGNORED_STDOUT: usize = 8 * 1024 * 1024;
const MAX_SENSITIVE_HINTS: usize = 100;
const MAX_ATTRIBUTES_BYTES: usize = 1024 * 1024;
const MAX_LFS_POINTER_BYTES: usize = 4096;

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

#[derive(Debug, Default, PartialEq, Eq)]
struct ParsedGitStatus {
    staged: Vec<GitFileChange>,
    unstaged: Vec<GitFileChange>,
    untracked: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AttributeFilterDefinitions {
    pub lfs: bool,
    pub non_lfs: bool,
}

#[derive(Debug, Clone)]
struct GitObjectEntry {
    path: String,
    oid: String,
    stage: Option<u8>,
}

#[derive(Debug, Default)]
struct MatchedLfsPath {
    head_oid: Option<String>,
    index_oid: Option<String>,
    worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LfsPointer {
    oid: String,
    size: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct LfsObjectSummary {
    matching_paths: usize,
    pointers_missing: usize,
    objects_missing: usize,
    objects_present: usize,
}

pub fn inspect_repository(path: &str) -> Result<RepositoryInspection, CommandError> {
    if path.trim().is_empty() {
        return Err(CommandError::new(
            "invalid_path",
            "repository path cannot be empty",
        ));
    }
    let requested = canonical_existing_directory(Path::new(path), "repository path")?;
    let git = find_executable_on_path("git").ok_or_else(|| {
        CommandError::new("git_not_found", "git executable was not found on PATH")
    })?;

    let root_text = git_checked_text(
        &git,
        &requested,
        &["rev-parse", "--show-toplevel"],
        GIT_TIMEOUT,
    )
    .map_err(|error| {
        if error.code == "git_command_failed" {
            CommandError::new(
                "not_a_git_repository",
                format!("'{}' is not inside a Git worktree", requested.display()),
            )
            .with_details(error.details.unwrap_or(json!({})))
        } else {
            error
        }
    })?;
    let root = canonical_existing_directory(Path::new(&root_text), "Git worktree root")?;

    let branch_output = git_raw(
        &git,
        &root,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        GIT_TIMEOUT,
        MAX_GIT_STDOUT,
    )?;
    let branch = if branch_output.status.success() {
        nonempty_string(&branch_output.stdout)
    } else if branch_output.status.code() == Some(1) {
        None
    } else {
        return Err(git_exit_error(
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            &branch_output,
        ));
    };

    let head_output = git_raw(
        &git,
        &root,
        &["rev-parse", "--verify", "HEAD"],
        GIT_TIMEOUT,
        MAX_GIT_STDOUT,
    )?;
    let head = if head_output.status.success() {
        nonempty_string(&head_output.stdout)
    } else {
        // An unborn branch has no HEAD commit and is still a valid repository.
        None
    };

    // Attribute inspection must happen before `git status`. A matching filter
    // can otherwise be executed while Git compares the worktree with the
    // index. Repositories that only declare unused LFS rules remain safe.
    let lfs = inspect_lfs(&git, &root, head.as_deref())?;

    let remote_output = git_checked(&git, &root, &["remote", "-v"], GIT_TIMEOUT, MAX_GIT_STDOUT)?;
    let remotes = parse_remotes(&remote_output.stdout);
    let primary_remote = remotes
        .iter()
        .find(|remote| remote.name == "origin" && remote.kind == "fetch")
        .or_else(|| remotes.iter().find(|remote| remote.kind == "fetch"))
        .map(|remote| remote.url.clone());

    let status_output = git_checked(
        &git,
        &root,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
        GIT_SLOW_TIMEOUT,
        MAX_GIT_STDOUT,
    )?;
    if status_output.stdout_truncated {
        return Err(CommandError::new(
            "git_output_too_large",
            "Git status output exceeded the safety limit; Relay will not use an incomplete status",
        ));
    }
    let status = parse_porcelain_v2(&status_output.stdout)?;

    let git_dir_text = git_checked_text(
        &git,
        &root,
        &["rev-parse", "--absolute-git-dir"],
        GIT_TIMEOUT,
    )?;
    let git_dir = canonical_existing_directory(Path::new(&git_dir_text), "Git metadata directory")?;
    let operation = inspect_operation_state(&git_dir);

    let mut warnings = Vec::new();
    if operation.any() {
        warnings.push(RepositoryWarning {
            code: "git_operation_in_progress".into(),
            message: "A merge, rebase, cherry-pick, revert, or bisect is in progress; code sharing should be blocked until it finishes".into(),
        });
    }
    if branch.is_none() && head.is_some() {
        warnings.push(RepositoryWarning {
            code: "detached_head".into(),
            message: "The repository is on a detached HEAD".into(),
        });
    }
    if primary_remote.is_none() {
        warnings.push(RepositoryWarning {
            code: "remote_missing".into(),
            message: "No fetch remote was found; another computer cannot clone this repository automatically".into(),
        });
    }

    let submodules = inspect_submodules(&git, &root, &mut warnings)?;
    let ignored_sensitive_files = inspect_ignored_sensitive_files(&git, &root, &mut warnings)?;
    if !ignored_sensitive_files.is_empty() {
        warnings.push(RepositoryWarning {
            code: "ignored_sensitive_files".into(),
            message: format!(
                "{} ignored file(s) look sensitive and should stay excluded unless the sender explicitly reviews them",
                ignored_sensitive_files.len()
            ),
        });
    }

    let submodule_worktree_changes = status
        .staged
        .iter()
        .chain(status.unstaged.iter())
        .any(|change| change.submodule.is_some());
    if submodule_worktree_changes {
        warnings.push(RepositoryWarning {
            code: "submodule_worktree_changes".into(),
            message: "At least one submodule has commit, tracked-file, or untracked-file changes; code sharing should be blocked until reviewed".into(),
        });
    }

    Ok(RepositoryInspection {
        requested_path: requested.to_string_lossy().into_owned(),
        root: root.to_string_lossy().into_owned(),
        branch: branch.clone(),
        detached: branch.is_none() && head.is_some(),
        head,
        primary_remote,
        remotes,
        staged: status.staged,
        unstaged: status.unstaged,
        untracked: status.untracked,
        operation,
        submodules,
        lfs,
        ignored_sensitive_files,
        warnings,
    })
}

fn git_raw(
    git: &Path,
    repository: &Path,
    args: &[&str],
    timeout: Duration,
    max_stdout: usize,
) -> Result<ProcessOutput, CommandError> {
    git_raw_with_input(git, repository, args, None, timeout, max_stdout)
}

fn git_raw_with_input(
    git: &Path,
    repository: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    timeout: Duration,
    max_stdout: usize,
) -> Result<ProcessOutput, CommandError> {
    let mut all_args = vec![OsString::from("-C"), repository.as_os_str().to_owned()];
    all_args.extend(safe_git_prefix());
    all_args.extend(args.iter().map(OsString::from));
    run_process_with_removed_environment(
        git,
        &all_args,
        stdin,
        timeout,
        max_stdout,
        MAX_GIT_STDERR,
        &[
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GCM_INTERACTIVE", "Never"),
            ("GIT_ATTR_NOSYSTEM", "1"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", GIT_NULL_DEVICE),
            ("GIT_CONFIG_COUNT", "0"),
            ("GIT_PROTOCOL_FROM_USER", "0"),
        ],
        GIT_CONTEXT_ENVIRONMENT,
    )
    .map_err(map_git_process_error)
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

fn git_checked(
    git: &Path,
    repository: &Path,
    args: &[&str],
    timeout: Duration,
    max_stdout: usize,
) -> Result<ProcessOutput, CommandError> {
    let output = git_raw(git, repository, args, timeout, max_stdout)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(git_exit_error(args, &output))
    }
}

fn git_checked_text(
    git: &Path,
    repository: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<String, CommandError> {
    let output = git_checked(git, repository, args, timeout, MAX_GIT_STDOUT)?;
    if output.stdout_truncated {
        return Err(CommandError::new(
            "git_output_too_large",
            "Git command output exceeded the safety limit",
        ));
    }
    nonempty_string(&output.stdout).ok_or_else(|| {
        CommandError::new(
            "git_protocol_error",
            format!("git {} returned empty output", args.join(" ")),
        )
    })
}

fn map_git_process_error(error: ProcessRunError) -> CommandError {
    match error {
        ProcessRunError::Timeout { .. } => CommandError::new("git_timeout", error.to_string()),
        ProcessRunError::Spawn(_) => CommandError::new("git_start_error", error.to_string()),
        _ => CommandError::new("git_io_error", error.to_string()),
    }
}

fn git_exit_error(args: &[&str], output: &ProcessOutput) -> CommandError {
    let stderr = bytes_to_trimmed_string(&output.stderr);
    let exit = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".into());
    let message = if stderr.is_empty() {
        format!("git {} exited with {exit}", args.join(" "))
    } else {
        format!("git {} exited with {exit}: {stderr}", args.join(" "))
    };
    CommandError::new("git_command_failed", message).with_details(json!({
        "exit": exit,
        "stderr_truncated": output.stderr_truncated
    }))
}

fn nonempty_string(bytes: &[u8]) -> Option<String> {
    let value = bytes_to_trimmed_string(bytes);
    (!value.is_empty()).then_some(value)
}

fn parse_remotes(bytes: &[u8]) -> Vec<GitRemote> {
    let mut seen = HashSet::new();
    let mut remotes = Vec::new();
    for line in String::from_utf8_lossy(bytes).lines() {
        let Some((name, remainder)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let remainder = remainder.trim_start();
        let (url, kind) = if let Some(url) = remainder.strip_suffix(" (fetch)") {
            (url, "fetch")
        } else if let Some(url) = remainder.strip_suffix(" (push)") {
            (url, "push")
        } else {
            continue;
        };
        let key = (name.to_owned(), url.to_owned(), kind.to_owned());
        if seen.insert(key.clone()) {
            remotes.push(GitRemote {
                name: key.0,
                url: key.1,
                kind: key.2,
            });
        }
    }
    remotes
}

fn parse_porcelain_v2(bytes: &[u8]) -> Result<ParsedGitStatus, CommandError> {
    let records: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    let mut parsed = ParsedGitStatus::default();
    let mut index = 0;

    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.is_empty() {
            continue;
        }
        match record[0] {
            b'#' => continue,
            b'?' => {
                let path = record
                    .strip_prefix(b"? ")
                    .ok_or_else(|| invalid_status_record(record, "invalid untracked record"))?;
                parsed.untracked.push(path_string(path));
            }
            b'!' => continue,
            b'1' => {
                let fields = split_record(record, 9, "ordinary")?;
                let change = change_from_fields(&fields, "ordinary", None)?;
                add_change(&mut parsed, fields[1], change)?;
            }
            b'2' => {
                let fields = split_record(record, 10, "renamed or copied")?;
                let original = records.get(index).ok_or_else(|| {
                    invalid_status_record(record, "rename/copy record is missing original path")
                })?;
                index += 1;
                let change =
                    change_from_fields(&fields, "renamed_or_copied", Some(path_string(original)))?;
                add_change(&mut parsed, fields[1], change)?;
            }
            b'u' => {
                let fields = split_record(record, 11, "unmerged")?;
                let change = change_from_fields(&fields, "unmerged", None)?;
                add_change(&mut parsed, fields[1], change)?;
            }
            _ => {
                return Err(invalid_status_record(
                    record,
                    "unknown porcelain v2 record type",
                ))
            }
        }
    }

    Ok(parsed)
}

fn split_record<'a>(
    record: &'a [u8],
    field_count: usize,
    kind: &str,
) -> Result<Vec<&'a [u8]>, CommandError> {
    let fields: Vec<&[u8]> = record.splitn(field_count, |byte| *byte == b' ').collect();
    if fields.len() != field_count {
        return Err(invalid_status_record(
            record,
            &format!(
                "{kind} record has {} fields, expected {field_count}",
                fields.len()
            ),
        ));
    }
    Ok(fields)
}

fn change_from_fields(
    fields: &[&[u8]],
    kind: &str,
    original_path: Option<String>,
) -> Result<GitFileChange, CommandError> {
    let path_index = match fields.first().copied() {
        Some(b"1") => 8,
        Some(b"2") => 9,
        Some(b"u") => 10,
        _ => {
            return Err(CommandError::new(
                "git_protocol_error",
                "unexpected Git status record type",
            ))
        }
    };
    let status = std::str::from_utf8(fields[1])
        .map_err(|_| CommandError::new("git_protocol_error", "Git status code is not UTF-8"))?;
    if status.len() != 2 {
        return Err(CommandError::new(
            "git_protocol_error",
            format!("invalid Git status code '{status}'"),
        ));
    }
    Ok(GitFileChange {
        path: path_string(fields[path_index]),
        status: status.to_owned(),
        kind: kind.to_owned(),
        original_path,
        submodule: parse_submodule_worktree_state(fields[2]),
    })
}

fn add_change(
    parsed: &mut ParsedGitStatus,
    status: &[u8],
    change: GitFileChange,
) -> Result<(), CommandError> {
    if status.len() != 2 {
        return Err(CommandError::new(
            "git_protocol_error",
            "Git status code must have two bytes",
        ));
    }
    if status[0] != b'.' {
        parsed.staged.push(change.clone());
    }
    if status[1] != b'.' {
        parsed.unstaged.push(change);
    }
    Ok(())
}

fn parse_submodule_worktree_state(field: &[u8]) -> Option<GitSubmoduleWorktreeState> {
    if field.len() != 4 || field[0] != b'S' {
        return None;
    }
    Some(GitSubmoduleWorktreeState {
        commit_changed: field[1] == b'C',
        tracked_changes: field[2] == b'M',
        untracked_changes: field[3] == b'U',
    })
}

fn invalid_status_record(record: &[u8], reason: &str) -> CommandError {
    CommandError::new(
        "git_protocol_error",
        format!("{reason}: {}", String::from_utf8_lossy(record)),
    )
}

fn path_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn inspect_operation_state(git_dir: &Path) -> GitOperationState {
    GitOperationState {
        merge: git_dir.join("MERGE_HEAD").is_file(),
        rebase: git_dir.join("rebase-merge").is_dir() || git_dir.join("rebase-apply").is_dir(),
        cherry_pick: git_dir.join("CHERRY_PICK_HEAD").is_file(),
        revert: git_dir.join("REVERT_HEAD").is_file(),
        bisect: git_dir.join("BISECT_LOG").is_file(),
    }
}

fn inspect_submodules(
    git: &Path,
    root: &Path,
    warnings: &mut Vec<RepositoryWarning>,
) -> Result<Vec<GitSubmodule>, CommandError> {
    let output = git_raw(
        git,
        root,
        &["submodule", "status", "--recursive"],
        GIT_SLOW_TIMEOUT,
        MAX_GIT_STDOUT,
    )?;
    if !output.status.success() {
        warnings.push(RepositoryWarning {
            code: "submodule_inspection_failed".into(),
            message: format!(
                "Git could not inspect submodules: {}",
                bytes_to_trimmed_string(&output.stderr)
            ),
        });
        return Ok(Vec::new());
    }
    if output.stdout_truncated {
        return Err(CommandError::new(
            "git_output_too_large",
            "Git submodule output exceeded the safety limit",
        ));
    }

    let submodules: Vec<GitSubmodule> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_submodule_line)
        .collect();
    if submodules
        .iter()
        .any(|submodule| submodule.state != "clean")
    {
        warnings.push(RepositoryWarning {
            code: "submodule_state".into(),
            message: "At least one submodule is uninitialized, conflicted, or checked out at a different commit; code sharing should be blocked until reviewed".into(),
        });
    }
    Ok(submodules)
}

fn parse_submodule_line(line: &str) -> Option<GitSubmodule> {
    let marker = line.chars().next()?;
    let state = match marker {
        ' ' => "clean",
        '-' => "uninitialized",
        '+' => "commit_mismatch",
        'U' => "conflict",
        _ => return None,
    };
    let body = line.get(marker.len_utf8()..)?;
    let (commit, remainder) = body.split_once(' ')?;
    let remainder = remainder.trim_start();
    let (path, description) = if remainder.ends_with(')') {
        if let Some(index) = remainder.rfind(" (") {
            (
                remainder[..index].to_owned(),
                Some(remainder[index + 2..remainder.len() - 1].to_owned()),
            )
        } else {
            (remainder.to_owned(), None)
        }
    } else {
        (remainder.to_owned(), None)
    };
    Some(GitSubmodule {
        path,
        commit: commit.to_owned(),
        state: state.into(),
        description,
    })
}

fn inspect_lfs(git: &Path, root: &Path, head: Option<&str>) -> Result<GitLfsStatus, CommandError> {
    let head_entries = match head {
        Some(head) => list_tree_entries(git, root, head)?,
        None => Vec::new(),
    };
    let index_entries = list_index_entries(git, root)?;
    let worktree_paths = list_worktree_paths(git, root)?;
    let definitions =
        inspect_current_attribute_definitions(git, root, &head_entries, &index_entries)?;
    reject_non_lfs_filter(definitions)?;

    let (available, version) = git_lfs_version();
    if !definitions.lfs {
        return Ok(GitLfsStatus {
            status: "not_present".into(),
            available,
            configured: false,
            version,
            tracked_file_count: Some(0),
            matching_path_count: 0,
        });
    }

    let matches = collect_lfs_matches(
        git,
        root,
        head,
        &head_entries,
        &index_entries,
        &worktree_paths,
    )?;
    if matches.is_empty() {
        return Ok(GitLfsStatus {
            status: "rules_only".into(),
            available,
            configured: true,
            version,
            tracked_file_count: Some(0),
            matching_path_count: 0,
        });
    }

    let summary = if available {
        inspect_lfs_objects(git, root, &matches)?
    } else {
        LfsObjectSummary {
            matching_paths: matches.len(),
            ..LfsObjectSummary::default()
        }
    };
    Err(lfs_blocking_error(available, &summary))
}

pub(crate) fn ensure_lfs_commit_safe(repository: &Path, commit: &str) -> Result<(), CommandError> {
    if !is_object_id(commit) {
        return Err(lfs_inspection_uncertain("commit"));
    }
    let git = find_executable_on_path("git").ok_or_else(|| {
        CommandError::new("git_not_found", "git executable was not found on PATH")
    })?;
    let entries = list_tree_entries(&git, repository, commit)?;
    let mut definitions = inspect_entry_attribute_definitions(&git, repository, &entries)?;
    definitions = merge_filter_definitions(
        definitions,
        inspect_info_attribute_definitions(&git, repository)?,
    );
    reject_non_lfs_filter(definitions)?;
    if !definitions.lfs {
        return Ok(());
    }

    let matches = collect_lfs_matches(&git, repository, Some(commit), &entries, &[], &[])?;
    if matches.is_empty() {
        return Ok(());
    }
    let (available, _) = git_lfs_version();
    let summary = if available {
        inspect_lfs_objects(&git, repository, &matches)?
    } else {
        LfsObjectSummary {
            matching_paths: matches.len(),
            ..LfsObjectSummary::default()
        }
    };
    Err(lfs_blocking_error(available, &summary))
}

pub(crate) fn attribute_filter_definitions(bytes: &[u8]) -> AttributeFilterDefinitions {
    let mut definitions = AttributeFilterDefinitions::default();
    for line in String::from_utf8_lossy(bytes).lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        for attribute in line.split_whitespace().skip(1) {
            let lower = attribute.to_ascii_lowercase();
            let is_filter = lower == "filter"
                || lower == "-filter"
                || lower == "!filter"
                || lower.starts_with("filter=");
            if !is_filter {
                continue;
            }
            if attribute == "filter=lfs" {
                definitions.lfs = true;
            } else {
                definitions.non_lfs = true;
            }
        }
    }
    definitions
}

fn merge_filter_definitions(
    left: AttributeFilterDefinitions,
    right: AttributeFilterDefinitions,
) -> AttributeFilterDefinitions {
    AttributeFilterDefinitions {
        lfs: left.lfs || right.lfs,
        non_lfs: left.non_lfs || right.non_lfs,
    }
}

fn reject_non_lfs_filter(definitions: AttributeFilterDefinitions) -> Result<(), CommandError> {
    if definitions.non_lfs {
        return Err(CommandError::new(
            "git_filter_blocked",
            "repository attributes define a non-LFS Git filter; Relay will not run external filters",
        )
        .with_details(json!({"status": "non_lfs_filter"})));
    }
    Ok(())
}

fn list_tree_entries(
    git: &Path,
    root: &Path,
    commit: &str,
) -> Result<Vec<GitObjectEntry>, CommandError> {
    let output = git_raw(
        git,
        root,
        &["ls-tree", "-r", "-z", "--full-tree", commit],
        GIT_SLOW_TIMEOUT,
        MAX_GIT_STDOUT,
    )?;
    if !output.status.success() || output.stdout_truncated {
        return Err(lfs_inspection_uncertain("head_tree"));
    }
    let mut entries = Vec::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            return Err(lfs_inspection_uncertain("head_tree"));
        };
        let metadata = std::str::from_utf8(&record[..tab])
            .map_err(|_| lfs_inspection_uncertain("head_tree"))?;
        let mut fields = metadata.split_whitespace();
        let _mode = fields.next();
        let kind = fields.next();
        let oid = fields.next();
        if fields.next().is_some() || kind != Some("blob") || !oid.is_some_and(is_object_id) {
            continue;
        }
        let path = String::from_utf8(record[tab + 1..].to_vec())
            .map_err(|_| lfs_inspection_uncertain("head_path_encoding"))?;
        validate_git_relative_path(&path)?;
        entries.push(GitObjectEntry {
            path,
            oid: oid.unwrap_or_default().into(),
            stage: None,
        });
    }
    Ok(entries)
}

fn list_index_entries(git: &Path, root: &Path) -> Result<Vec<GitObjectEntry>, CommandError> {
    let output = git_raw(
        git,
        root,
        &["ls-files", "--stage", "-z"],
        GIT_TIMEOUT,
        MAX_GIT_STDOUT,
    )?;
    if !output.status.success() || output.stdout_truncated {
        return Err(lfs_inspection_uncertain("index"));
    }
    let mut entries = Vec::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            return Err(lfs_inspection_uncertain("index"));
        };
        let metadata =
            std::str::from_utf8(&record[..tab]).map_err(|_| lfs_inspection_uncertain("index"))?;
        let mut fields = metadata.split_whitespace();
        let _mode = fields.next();
        let oid = fields.next();
        let stage = fields.next().and_then(|value| value.parse::<u8>().ok());
        if fields.next().is_some() || !oid.is_some_and(is_object_id) || stage.is_none() {
            return Err(lfs_inspection_uncertain("index"));
        }
        let path = String::from_utf8(record[tab + 1..].to_vec())
            .map_err(|_| lfs_inspection_uncertain("index_path_encoding"))?;
        validate_git_relative_path(&path)?;
        entries.push(GitObjectEntry {
            path,
            oid: oid.unwrap_or_default().into(),
            stage,
        });
    }
    Ok(entries)
}

fn list_worktree_paths(git: &Path, root: &Path) -> Result<Vec<String>, CommandError> {
    let output = git_raw(
        git,
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        GIT_TIMEOUT,
        MAX_GIT_STDOUT,
    )?;
    if !output.status.success() || output.stdout_truncated {
        return Err(lfs_inspection_uncertain("worktree_paths"));
    }
    parse_nul_paths(&output.stdout, "worktree_path_encoding")
}

fn parse_nul_paths(bytes: &[u8], stage: &'static str) -> Result<Vec<String>, CommandError> {
    let mut paths = Vec::new();
    for path in bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = String::from_utf8(path.to_vec()).map_err(|_| lfs_inspection_uncertain(stage))?;
        validate_git_relative_path(&path)?;
        paths.push(path);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn inspect_current_attribute_definitions(
    git: &Path,
    root: &Path,
    head_entries: &[GitObjectEntry],
    index_entries: &[GitObjectEntry],
) -> Result<AttributeFilterDefinitions, CommandError> {
    let mut definitions = inspect_entry_attribute_definitions(git, root, head_entries)?;
    definitions = merge_filter_definitions(
        definitions,
        inspect_entry_attribute_definitions(git, root, index_entries)?,
    );

    let output = git_raw(
        git,
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--",
            ".gitattributes",
            ":(glob)**/.gitattributes",
        ],
        GIT_TIMEOUT,
        MAX_GIT_STDOUT,
    )?;
    if !output.status.success() || output.stdout_truncated {
        return Err(lfs_inspection_uncertain("worktree_attributes"));
    }
    for path in parse_nul_paths(&output.stdout, "attribute_path_encoding")? {
        if let Some(bytes) = read_worktree_attribute_file(root, &path)? {
            definitions =
                merge_filter_definitions(definitions, attribute_filter_definitions(&bytes));
        }
    }
    Ok(merge_filter_definitions(
        definitions,
        inspect_info_attribute_definitions(git, root)?,
    ))
}

fn inspect_entry_attribute_definitions(
    git: &Path,
    root: &Path,
    entries: &[GitObjectEntry],
) -> Result<AttributeFilterDefinitions, CommandError> {
    let mut definitions = AttributeFilterDefinitions::default();
    let mut seen = HashSet::new();
    for entry in entries {
        if Path::new(&entry.path).file_name() != Some(OsStr::new(".gitattributes"))
            || !seen.insert(entry.oid.clone())
        {
            continue;
        }
        let bytes = read_git_blob(git, root, &entry.oid, MAX_ATTRIBUTES_BYTES)?;
        definitions = merge_filter_definitions(definitions, attribute_filter_definitions(&bytes));
    }
    Ok(definitions)
}

fn inspect_info_attribute_definitions(
    git: &Path,
    root: &Path,
) -> Result<AttributeFilterDefinitions, CommandError> {
    let output = git_raw(
        git,
        root,
        &["rev-parse", "--git-path", "info/attributes"],
        GIT_TIMEOUT,
        MAX_ATTRIBUTES_BYTES,
    )?;
    if !output.status.success() || output.stdout_truncated {
        return Err(lfs_inspection_uncertain("info_attributes"));
    }
    let raw = std::str::from_utf8(&output.stdout)
        .map_err(|_| lfs_inspection_uncertain("info_attributes"))?
        .trim();
    if raw.is_empty() {
        return Ok(AttributeFilterDefinitions::default());
    }
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(lfs_inspection_uncertain("info_attributes"))
        }
        Ok(metadata) if metadata.len() > MAX_ATTRIBUTES_BYTES as u64 => {
            Err(lfs_inspection_uncertain("info_attributes"))
        }
        Ok(_) => fs::read(path)
            .map(|bytes| attribute_filter_definitions(&bytes))
            .map_err(|_| lfs_inspection_uncertain("info_attributes")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(AttributeFilterDefinitions::default())
        }
        Err(_) => Err(lfs_inspection_uncertain("info_attributes")),
    }
}

fn read_worktree_attribute_file(root: &Path, path: &str) -> Result<Option<Vec<u8>>, CommandError> {
    validate_git_relative_path(path)?;
    let full_path = root.join(path);
    match fs::symlink_metadata(&full_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(lfs_inspection_uncertain("worktree_attributes"))
        }
        Ok(metadata) if metadata.len() > MAX_ATTRIBUTES_BYTES as u64 => {
            Err(lfs_inspection_uncertain("worktree_attributes"))
        }
        Ok(_) => {
            let canonical = fs::canonicalize(&full_path)
                .map_err(|_| lfs_inspection_uncertain("worktree_attributes"))?;
            if !canonical.starts_with(root) {
                return Err(lfs_inspection_uncertain("worktree_attributes"));
            }
            fs::read(canonical)
                .map(Some)
                .map_err(|_| lfs_inspection_uncertain("worktree_attributes"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(lfs_inspection_uncertain("worktree_attributes")),
    }
}

fn validate_git_relative_path(path: &str) -> Result<(), CommandError> {
    if path.is_empty() {
        return Err(lfs_inspection_uncertain("path"));
    }
    let mut first = true;
    for component in Path::new(path).components() {
        match component {
            Component::Normal(value) => {
                if value.is_empty()
                    || value == OsStr::new(".")
                    || value == OsStr::new("..")
                    || (first && value.eq_ignore_ascii_case(OsStr::new(".git")))
                {
                    return Err(lfs_inspection_uncertain("path"));
                }
                first = false;
            }
            _ => return Err(lfs_inspection_uncertain("path")),
        }
    }
    Ok(())
}

fn collect_lfs_matches(
    git: &Path,
    root: &Path,
    head: Option<&str>,
    head_entries: &[GitObjectEntry],
    index_entries: &[GitObjectEntry],
    worktree_paths: &[String],
) -> Result<HashMap<String, MatchedLfsPath>, CommandError> {
    let mut matches = HashMap::<String, MatchedLfsPath>::new();
    if let Some(head) = head {
        let paths: Vec<String> = head_entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        let matched = check_lfs_attributes(
            git,
            root,
            &["check-attr", "-z", "--stdin", "--source", head, "filter"],
            &paths,
            "head_attributes",
        )?;
        let by_path: HashMap<&str, &GitObjectEntry> = head_entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry))
            .collect();
        for path in matched {
            let entry = by_path
                .get(path.as_str())
                .ok_or_else(|| lfs_inspection_uncertain("head_attributes"))?;
            matches.entry(path).or_default().head_oid = Some(entry.oid.clone());
        }
    }

    let index_paths: Vec<String> = index_entries
        .iter()
        .filter(|entry| entry.stage == Some(0))
        .map(|entry| entry.path.clone())
        .collect();
    if index_entries.iter().any(|entry| entry.stage != Some(0)) {
        return Err(lfs_inspection_uncertain("unmerged_index"));
    }
    let matched = check_lfs_attributes(
        git,
        root,
        &["check-attr", "-z", "--stdin", "--cached", "filter"],
        &index_paths,
        "index_attributes",
    )?;
    let by_path: HashMap<&str, &GitObjectEntry> = index_entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    for path in matched {
        let entry = by_path
            .get(path.as_str())
            .ok_or_else(|| lfs_inspection_uncertain("index_attributes"))?;
        matches.entry(path).or_default().index_oid = Some(entry.oid.clone());
    }

    for path in check_lfs_attributes(
        git,
        root,
        &["check-attr", "-z", "--stdin", "filter"],
        worktree_paths,
        "worktree_attributes",
    )? {
        matches.entry(path).or_default().worktree = true;
    }
    Ok(matches)
}

fn check_lfs_attributes(
    git: &Path,
    root: &Path,
    args: &[&str],
    paths: &[String],
    stage: &'static str,
) -> Result<HashSet<String>, CommandError> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    let output = git_raw_with_input(
        git,
        root,
        args,
        Some(&input),
        GIT_SLOW_TIMEOUT,
        MAX_GIT_STDOUT,
    )?;
    if !output.status.success() || output.stdout_truncated {
        return Err(lfs_inspection_uncertain(stage));
    }
    let records: Vec<&[u8]> = output.stdout.split(|byte| *byte == 0).collect();
    let mut matched = HashSet::new();
    let known: HashSet<&str> = paths.iter().map(String::as_str).collect();
    let mut reported = 0_usize;
    for record in records.chunks_exact(3) {
        if record.iter().all(|part| part.is_empty()) {
            continue;
        }
        reported += 1;
        let path = std::str::from_utf8(record[0]).map_err(|_| lfs_inspection_uncertain(stage))?;
        let attribute =
            std::str::from_utf8(record[1]).map_err(|_| lfs_inspection_uncertain(stage))?;
        let value = std::str::from_utf8(record[2]).map_err(|_| lfs_inspection_uncertain(stage))?;
        if !known.contains(path) || attribute != "filter" {
            return Err(lfs_inspection_uncertain(stage));
        }
        match value {
            "lfs" => {
                matched.insert(path.to_owned());
            }
            "unspecified" | "unset" => {}
            _ => {
                return Err(CommandError::new(
                    "git_filter_blocked",
                    "a repository path resolves to a non-LFS Git filter; Relay will not run external filters",
                )
                .with_details(json!({"status": "non_lfs_filter"})))
            }
        }
    }
    if reported != paths.len()
        || records.len() % 3 != 1
        || records.last().is_some_and(|record| !record.is_empty())
    {
        return Err(lfs_inspection_uncertain(stage));
    }
    Ok(matched)
}

fn inspect_lfs_objects(
    git: &Path,
    root: &Path,
    matches: &HashMap<String, MatchedLfsPath>,
) -> Result<LfsObjectSummary, CommandError> {
    let mut summary = LfsObjectSummary {
        matching_paths: matches.len(),
        ..LfsObjectSummary::default()
    };
    let mut pointers = Vec::new();
    for matched in matches.values() {
        let Some(oid) = matched.index_oid.as_deref().or(matched.head_oid.as_deref()) else {
            summary.pointers_missing += 1;
            continue;
        };
        let blob = read_git_blob(git, root, oid, MAX_LFS_POINTER_BYTES)?;
        let Some(pointer) = parse_lfs_pointer(&blob) else {
            summary.pointers_missing += 1;
            continue;
        };
        pointers.push(pointer);
    }
    if pointers.is_empty() {
        return Ok(summary);
    }
    let object_root = lfs_object_root(git, root)?;
    for pointer in pointers {
        let object = object_root
            .join(&pointer.oid[..2])
            .join(&pointer.oid[2..4])
            .join(&pointer.oid);
        match fs::metadata(object) {
            Ok(metadata) if metadata.is_file() && metadata.len() == pointer.size => {
                summary.objects_present += 1;
            }
            Ok(_) => summary.objects_missing += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                summary.objects_missing += 1;
            }
            Err(_) => return Err(lfs_inspection_uncertain("lfs_object_store")),
        }
    }
    Ok(summary)
}

fn read_git_blob(
    git: &Path,
    root: &Path,
    oid: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, CommandError> {
    if !is_object_id(oid) {
        return Err(lfs_inspection_uncertain("git_object"));
    }
    let output = git_raw(
        git,
        root,
        &["cat-file", "blob", oid],
        GIT_TIMEOUT,
        max_bytes,
    )?;
    if !output.status.success() || output.stdout_truncated {
        return Err(lfs_inspection_uncertain("git_object"));
    }
    Ok(output.stdout)
}

fn lfs_object_root(git: &Path, root: &Path) -> Result<PathBuf, CommandError> {
    let common = git_raw(
        git,
        root,
        &["rev-parse", "--git-common-dir"],
        GIT_TIMEOUT,
        MAX_GIT_STDOUT,
    )?;
    if !common.status.success() || common.stdout_truncated {
        return Err(lfs_inspection_uncertain("lfs_storage"));
    }
    let common = std::str::from_utf8(&common.stdout)
        .map_err(|_| lfs_inspection_uncertain("lfs_storage"))?
        .trim();
    if common.is_empty() {
        return Err(lfs_inspection_uncertain("lfs_storage"));
    }
    let common = PathBuf::from(common);
    let common = if common.is_absolute() {
        common
    } else {
        root.join(common)
    };

    let configured = git_raw(
        git,
        root,
        &["config", "--path", "--get", "lfs.storage"],
        GIT_TIMEOUT,
        MAX_GIT_STDOUT,
    )?;
    let storage = if configured.status.success() {
        if configured.stdout_truncated {
            return Err(lfs_inspection_uncertain("lfs_storage"));
        }
        let value = std::str::from_utf8(&configured.stdout)
            .map_err(|_| lfs_inspection_uncertain("lfs_storage"))?
            .trim();
        if value.is_empty() {
            return Err(lfs_inspection_uncertain("lfs_storage"));
        }
        let value = PathBuf::from(value);
        if value.is_absolute() {
            value
        } else {
            common.join(value)
        }
    } else if configured.status.code() == Some(1) {
        common.join("lfs")
    } else {
        return Err(lfs_inspection_uncertain("lfs_storage"));
    };
    Ok(storage.join("objects"))
}

fn parse_lfs_pointer(bytes: &[u8]) -> Option<LfsPointer> {
    if bytes.len() > MAX_LFS_POINTER_BYTES {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    if lines.next()? != "version https://git-lfs.github.com/spec/v1" {
        return None;
    }
    let mut oid = None;
    let mut size = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some(value) = line.strip_prefix("oid sha256:") {
            if oid.is_some()
                || value.len() != 64
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return None;
            }
            oid = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("size ") {
            if size.is_some()
                || value.is_empty()
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return None;
            }
            size = value.parse::<u64>().ok();
            size?;
        } else if !line.starts_with("ext-") {
            return None;
        }
    }
    Some(LfsPointer {
        oid: oid?,
        size: size?,
    })
}

fn git_lfs_version() -> (bool, Option<String>) {
    let Some(executable) = find_executable_on_path("git-lfs") else {
        return (false, None);
    };
    let output = run_process_with_removed_environment(
        &executable,
        &[OsString::from("version")],
        None,
        GIT_TIMEOUT,
        64 * 1024,
        MAX_GIT_STDERR,
        &[("GIT_TERMINAL_PROMPT", "0")],
        GIT_CONTEXT_ENVIRONMENT,
    );
    match output {
        Ok(output) if output.status.success() && !output.stdout_truncated => {
            (true, nonempty_string(&output.stdout))
        }
        _ => (false, None),
    }
}

fn lfs_blocking_error(available: bool, summary: &LfsObjectSummary) -> CommandError {
    let (code, status, message) = if !available {
        (
            "lfs_unavailable",
            "git_lfs_unavailable",
            "Git LFS paths are present, but git-lfs is unavailable; Relay cannot verify a safe portable package",
        )
    } else if summary.pointers_missing > 0 {
        (
            "lfs_pointer_missing",
            "pointer_missing",
            "At least one path uses LFS attributes without a valid stored LFS pointer; Relay will not capture it",
        )
    } else if summary.objects_missing > 0 {
        (
            "lfs_object_missing",
            "object_missing",
            "At least one LFS pointer has no complete local object; Relay will not create an incomplete package",
        )
    } else {
        (
            "lfs_objects_not_included",
            "objects_present_not_included",
            "LFS objects are present locally, but Relay packages do not include LFS objects yet",
        )
    };
    CommandError::new(code, message).with_details(json!({
        "status": status,
        "matching_path_count": summary.matching_paths,
        "pointer_missing_count": summary.pointers_missing,
        "missing_object_count": summary.objects_missing,
        "present_object_count": summary.objects_present
    }))
}

fn lfs_inspection_uncertain(stage: &'static str) -> CommandError {
    CommandError::new(
        "lfs_inspection_uncertain",
        "Relay could not safely determine whether repository paths use Git LFS",
    )
    .with_details(json!({"status": "inspection_uncertain", "stage": stage}))
}

fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn inspect_ignored_sensitive_files(
    git: &Path,
    root: &Path,
    warnings: &mut Vec<RepositoryWarning>,
) -> Result<Vec<String>, CommandError> {
    let output = git_raw(
        git,
        root,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
        GIT_SLOW_TIMEOUT,
        MAX_IGNORED_STDOUT,
    )?;
    if !output.status.success() {
        warnings.push(RepositoryWarning {
            code: "ignored_file_inspection_failed".into(),
            message: format!(
                "Git could not inspect ignored files: {}",
                bytes_to_trimmed_string(&output.stderr)
            ),
        });
        return Ok(Vec::new());
    }
    if output.stdout_truncated {
        warnings.push(RepositoryWarning {
            code: "ignored_file_list_truncated".into(),
            message: "The ignored-file list exceeded 8 MiB; sensitive-file hints may be incomplete"
                .into(),
        });
    }

    let mut files: Vec<String> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty() && is_sensitive_path_hint(path))
        .take(MAX_SENSITIVE_HINTS)
        .map(path_string)
        .collect();
    files.sort();
    files.dedup();
    if files.len() == MAX_SENSITIVE_HINTS {
        warnings.push(RepositoryWarning {
            code: "sensitive_file_hint_limit".into(),
            message: format!(
                "Sensitive ignored-file hints were limited to {MAX_SENSITIVE_HINTS} paths"
            ),
        });
    }
    Ok(files)
}

fn is_sensitive_path_hint(path: &[u8]) -> bool {
    let path = String::from_utf8_lossy(path).to_ascii_lowercase();
    let name = path.rsplit('/').next().unwrap_or(&path);
    let obvious_template = ["example", "sample", "template", "placeholder"]
        .iter()
        .any(|word| name.contains(word));

    if (name == ".env" || name.starts_with(".env.")) && !obvious_template {
        return true;
    }
    if matches!(
        name,
        ".npmrc"
            | ".pypirc"
            | ".netrc"
            | "credentials"
            | "credentials.json"
            | "secrets.json"
            | "id_rsa"
            | "id_ed25519"
            | "known_hosts"
    ) {
        return true;
    }
    if [".pem", ".key", ".p12", ".pfx", ".jks"]
        .iter()
        .any(|extension| name.ends_with(extension))
    {
        return true;
    }
    !obvious_template
        && (name.contains("service-account")
            || name.contains("service_account")
            || name.contains("credential")
            || name.contains("secret"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    struct TemporaryRepository {
        path: std::path::PathBuf,
        _directory: tempfile::TempDir,
    }

    impl TemporaryRepository {
        fn create() -> Self {
            let directory = tempfile::Builder::new()
                .prefix("relay-rust-git-test-")
                .tempdir()
                .expect("temporary repository directory should be created");
            let path = directory.path().to_path_buf();
            Self {
                path,
                _directory: directory,
            }
        }

        fn git(&self, args: &[&str]) {
            let status = Command::new("git")
                .arg("-C")
                .arg(&self.path)
                .args(args)
                .status()
                .expect("git test command should start");
            assert!(status.success(), "git test command failed: {args:?}");
        }
    }

    #[test]
    fn parses_porcelain_v2_status_records() {
        let input = concat!(
            "1 M. N... 100644 100644 100644 aaaaaaa bbbbbbb staged.rs\0",
            "1 .M N... 100644 100644 100644 aaaaaaa bbbbbbb modified.rs\0",
            "1 MM N... 100644 100644 100644 aaaaaaa bbbbbbb both.rs\0",
            "2 R. N... 100644 100644 100644 aaaaaaa bbbbbbb R100 renamed.rs\0old.rs\0",
            "? untracked file.txt\0",
            "1 .M S.MU 160000 160000 160000 aaaaaaa bbbbbbb vendor/submodule\0"
        );
        let status = parse_porcelain_v2(input.as_bytes()).expect("status should parse");

        assert_eq!(status.staged.len(), 3);
        assert_eq!(status.unstaged.len(), 3);
        assert_eq!(status.untracked, vec!["untracked file.txt"]);
        let renamed = status
            .staged
            .iter()
            .find(|change| change.path == "renamed.rs")
            .expect("rename should be staged");
        assert_eq!(renamed.original_path.as_deref(), Some("old.rs"));
        let submodule = status
            .unstaged
            .iter()
            .find(|change| change.path == "vendor/submodule")
            .and_then(|change| change.submodule.as_ref())
            .expect("submodule state should be present");
        assert!(!submodule.commit_changed);
        assert!(submodule.tracked_changes);
        assert!(submodule.untracked_changes);
    }

    #[test]
    fn rejects_malformed_porcelain_record() {
        let error =
            parse_porcelain_v2(b"1 M. too-short\0").expect_err("malformed status must fail");
        assert_eq!(error.code, "git_protocol_error");
    }

    #[test]
    fn parses_submodule_status_lines() {
        let clean =
            parse_submodule_line(" 0123456789012345678901234567890123456789 deps/lib (heads/main)")
                .expect("clean line should parse");
        assert_eq!(clean.state, "clean");
        assert_eq!(clean.path, "deps/lib");
        assert_eq!(clean.description.as_deref(), Some("heads/main"));

        let changed = parse_submodule_line(
            "+0123456789012345678901234567890123456789 deps/lib (v1.0-2-gabc)",
        )
        .expect("changed line should parse");
        assert_eq!(changed.state, "commit_mismatch");
    }

    #[test]
    fn identifies_sensitive_ignored_paths_without_flagging_templates() {
        assert!(is_sensitive_path_hint(b".env"));
        assert!(is_sensitive_path_hint(b"config/service-account.json"));
        assert!(is_sensitive_path_hint(b"certs/client.key"));
        assert!(!is_sensitive_path_hint(b".env.example"));
        assert!(!is_sensitive_path_hint(b"docs/secret-template.json"));
    }

    #[test]
    fn parses_fetch_and_push_remotes() {
        let remotes = parse_remotes(
            b"origin\tgit@github.com:example/repo.git (fetch)\norigin\tgit@github.com:example/repo.git (push)\n",
        );
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].kind, "fetch");
        assert_eq!(remotes[1].kind, "push");
    }

    #[test]
    fn parses_standard_lfs_pointers_without_accepting_lookalikes() {
        let oid = "0123456789abcdef".repeat(4);
        let pointer =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 42\n");
        assert_eq!(
            parse_lfs_pointer(pointer.as_bytes()),
            Some(LfsPointer { oid, size: 42 })
        );
        assert!(parse_lfs_pointer(
            b"version https://git-lfs.github.com/spec/v1\noid sha256:ABCDEF\nsize 1\n"
        )
        .is_none());
        assert!(parse_lfs_pointer(
            b"version https://git-lfs.github.com/spec/v1\noid sha256:0000000000000000000000000000000000000000000000000000000000000000\nsize 1\nsecret value\n"
        )
        .is_none());
    }

    #[test]
    fn lfs_block_reasons_are_distinct_and_do_not_include_repository_data() {
        let unavailable = lfs_blocking_error(
            false,
            &LfsObjectSummary {
                matching_paths: 1,
                ..LfsObjectSummary::default()
            },
        );
        assert_eq!(unavailable.code, "lfs_unavailable");

        let pointer_missing = lfs_blocking_error(
            true,
            &LfsObjectSummary {
                matching_paths: 1,
                pointers_missing: 1,
                ..LfsObjectSummary::default()
            },
        );
        assert_eq!(pointer_missing.code, "lfs_pointer_missing");

        let object_missing = lfs_blocking_error(
            true,
            &LfsObjectSummary {
                matching_paths: 1,
                objects_missing: 1,
                ..LfsObjectSummary::default()
            },
        );
        assert_eq!(object_missing.code, "lfs_object_missing");

        let object_present = lfs_blocking_error(
            true,
            &LfsObjectSummary {
                matching_paths: 1,
                objects_present: 1,
                ..LfsObjectSummary::default()
            },
        );
        assert_eq!(object_present.code, "lfs_objects_not_included");

        for error in [unavailable, pointer_missing, object_missing, object_present] {
            let serialized = serde_json::to_string(&error).unwrap();
            assert!(!serialized.contains("TOKEN="));
            assert!(!serialized.contains("version https://"));
        }
        assert_eq!(
            lfs_inspection_uncertain("test").code,
            "lfs_inspection_uncertain"
        );
    }

    #[test]
    fn unused_lfs_attribute_rules_are_allowed() {
        let repository = TemporaryRepository::create();
        repository.git(&["init", "--quiet"]);
        std::fs::write(
            repository.path.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        repository.git(&["add", ".gitattributes"]);

        let inspection = inspect_repository(repository.path.to_str().unwrap()).unwrap();
        assert!(inspection.lfs.configured);
        assert_eq!(inspection.lfs.status, "rules_only");
        assert_eq!(inspection.lfs.matching_path_count, 0);
        assert_eq!(inspection.lfs.tracked_file_count, Some(0));
    }

    #[test]
    fn an_actual_lfs_path_is_blocked_before_status() {
        let repository = TemporaryRepository::create();
        repository.git(&["init", "--quiet"]);
        std::fs::write(
            repository.path.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        std::fs::write(
            repository.path.join("asset.bin"),
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 1\n",
                "0".repeat(64)
            ),
        )
        .unwrap();
        repository.git(&[
            "-c",
            "filter.lfs.clean=",
            "-c",
            "filter.lfs.smudge=",
            "-c",
            "filter.lfs.process=",
            "-c",
            "filter.lfs.required=false",
            "add",
            ".gitattributes",
            "asset.bin",
        ]);

        let error = inspect_repository(repository.path.to_str().unwrap()).unwrap_err();
        assert!(matches!(
            error.code.as_str(),
            "lfs_unavailable" | "lfs_object_missing"
        ));
        let serialized = serde_json::to_string(&error).unwrap();
        assert!(!serialized.contains("asset.bin"));
        assert!(!serialized.contains("version https://"));
    }

    #[cfg(unix)]
    #[test]
    fn lfs_detection_never_executes_the_configured_filter_process() {
        use std::os::unix::fs::PermissionsExt;

        let repository = TemporaryRepository::create();
        repository.git(&["init", "--quiet"]);
        std::fs::write(
            repository.path.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        std::fs::write(
            repository.path.join("asset.bin"),
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 1\n",
                "0".repeat(64)
            ),
        )
        .unwrap();
        repository.git(&[
            "-c",
            "filter.lfs.clean=",
            "-c",
            "filter.lfs.smudge=",
            "-c",
            "filter.lfs.process=",
            "-c",
            "filter.lfs.required=false",
            "add",
            ".gitattributes",
            "asset.bin",
        ]);

        let marker = repository.path.join("filter.marker");
        let filter = repository.path.join("filter.sh");
        std::fs::write(
            &filter,
            format!(
                "#!/bin/sh\nprintf invoked > '{}'\nexit 1\n",
                marker.to_string_lossy().replace('\'', "'\\''")
            ),
        )
        .unwrap();
        std::fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o755)).unwrap();
        repository.git(&["config", "filter.lfs.process", filter.to_str().unwrap()]);
        repository.git(&["config", "filter.lfs.required", "true"]);

        let error = inspect_repository(repository.path.to_str().unwrap()).unwrap_err();
        assert!(error.code.starts_with("lfs_"));
        assert!(!marker.exists());
    }

    #[test]
    fn non_lfs_filter_rules_remain_blocked_even_without_matching_paths() {
        let repository = TemporaryRepository::create();
        repository.git(&["init", "--quiet"]);
        std::fs::write(
            repository.path.join(".gitattributes"),
            "*.dat filter=external-driver\n",
        )
        .unwrap();

        let error = inspect_repository(repository.path.to_str().unwrap()).unwrap_err();
        assert_eq!(error.code, "git_filter_blocked");
    }

    #[test]
    fn a_present_lfs_object_is_still_not_packaged() {
        let repository = TemporaryRepository::create();
        repository.git(&["init", "--quiet"]);
        let oid = "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881";
        std::fs::write(
            repository.path.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        std::fs::write(
            repository.path.join("asset.bin"),
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 1\n"),
        )
        .unwrap();
        repository.git(&[
            "-c",
            "filter.lfs.clean=",
            "-c",
            "filter.lfs.smudge=",
            "-c",
            "filter.lfs.process=",
            "-c",
            "filter.lfs.required=false",
            "add",
            ".gitattributes",
            "asset.bin",
        ]);
        let object = repository
            .path
            .join(".git/lfs/objects")
            .join(&oid[..2])
            .join(&oid[2..4])
            .join(oid);
        std::fs::create_dir_all(object.parent().unwrap()).unwrap();
        std::fs::write(&object, b"x").unwrap();

        let git = find_executable_on_path("git").unwrap();
        let indexed_oid = bytes_to_trimmed_string(
            &git_checked(
                &git,
                &repository.path,
                &["rev-parse", ":asset.bin"],
                GIT_TIMEOUT,
                MAX_GIT_STDOUT,
            )
            .unwrap()
            .stdout,
        );
        let matches = HashMap::from([(
            "asset.bin".into(),
            MatchedLfsPath {
                index_oid: Some(indexed_oid),
                worktree: true,
                ..MatchedLfsPath::default()
            },
        )]);
        let summary = inspect_lfs_objects(&git, &repository.path, &matches).unwrap();
        assert_eq!(summary.objects_present, 1);
        assert_eq!(
            lfs_blocking_error(true, &summary).code,
            "lfs_objects_not_included"
        );
    }

    #[test]
    fn inspects_a_real_repository_read_only() {
        let repository = TemporaryRepository::create();
        repository.git(&["init", "--quiet"]);
        repository.git(&["config", "user.name", "Relay Test"]);
        repository.git(&["config", "user.email", "relay-test@example.invalid"]);

        std::fs::write(repository.path.join("tracked.txt"), "initial\n")
            .expect("tracked file should be written");
        std::fs::write(repository.path.join(".gitignore"), ".env\n")
            .expect("gitignore should be written");
        repository.git(&["add", "tracked.txt", ".gitignore"]);
        repository.git(&["commit", "--quiet", "-m", "initial"]);
        repository.git(&[
            "remote",
            "add",
            "origin",
            "https://example.invalid/relay.git",
        ]);

        std::fs::write(repository.path.join("tracked.txt"), "modified\n")
            .expect("tracked file should be modified");
        std::fs::write(repository.path.join("staged.txt"), "staged\n")
            .expect("staged file should be written");
        repository.git(&["add", "staged.txt"]);
        std::fs::write(repository.path.join("untracked.txt"), "untracked\n")
            .expect("untracked file should be written");
        std::fs::write(repository.path.join(".env"), "TOKEN=test-only\n")
            .expect("ignored file should be written");

        let before = repository
            .path
            .join(".git/index")
            .metadata()
            .and_then(|metadata| metadata.modified())
            .expect("index timestamp should be readable");
        let inspection = inspect_repository(repository.path.to_str().expect("UTF-8 temp path"))
            .expect("repository should be inspected");
        let after = repository
            .path
            .join(".git/index")
            .metadata()
            .and_then(|metadata| metadata.modified())
            .expect("index timestamp should still be readable");

        assert!(matches!(
            inspection.branch.as_deref(),
            Some("main") | Some("master")
        ));
        assert!(inspection
            .staged
            .iter()
            .any(|change| change.path == "staged.txt"));
        assert!(inspection
            .unstaged
            .iter()
            .any(|change| change.path == "tracked.txt"));
        assert!(inspection
            .untracked
            .iter()
            .any(|path| path == "untracked.txt"));
        assert!(inspection
            .ignored_sensitive_files
            .iter()
            .any(|path| path == ".env"));
        assert_eq!(
            inspection.primary_remote.as_deref(),
            Some("https://example.invalid/relay.git")
        );
        assert_eq!(
            before, after,
            "repository inspection must not update the index"
        );
    }
}
