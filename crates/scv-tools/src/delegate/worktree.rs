//! Git worktrees SCV creates so a job's agent works apart from its
//! repository's own checkout (`agent` calls with `worktree: true`).
//!
//! A worktree lives under the instance's
//! [`Layout::worktrees`](scv_client::Layout::worktrees), grouped by
//! repository as `<repository>-<digest>/<agent>-<6 hex>`, on a new branch
//! `scv/<agent>-<6 hex>` from the repository's current `HEAD`, with a private
//! record beside it (`<agent>-<6 hex>.json`) naming the checkout it was made
//! from. SCV runs `git` only to create, list, and remove these worktrees,
//! with fixed arguments, the repository's hooks and file-system monitor
//! off, no `*_API_KEY` variables, and a time limit; it never commits, merges, or deletes a
//! branch. A worktree counts as part of the workspace its checkout is in, so
//! an agent call may name it as `cwd`.

use std::{
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use scv_core::ToolError;
use scv_protocol::{WorktreeInfo, WorktreeRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// How long one `git` command may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest output read from one `git` command.
const MAX_GIT_OUTPUT: usize = 64 * 1024;
/// Largest worktree record read.
const MAX_RECORD_BYTES: u64 = 16 * 1024;

/// A worktree SCV just created, and where the agent runs in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub worktree: WorktreeRef,
    /// The directory the call asked for, inside the worktree.
    pub cwd: PathBuf,
}

/// What SCV records beside each worktree it created.
#[derive(Debug, Serialize, Deserialize)]
struct Record {
    v: u32,
    /// The checkout the worktree was made from: the top of the repository
    /// (or linked worktree) holding the call's `cwd`.
    source: PathBuf,
    branch: String,
}

/// Create a worktree under `root` for an `agent` call that would run in
/// `cwd`, a directory of a git repository whose top is inside `workspace`
/// (canonical), and return where it is. A worktree whose creation fails is
/// removed again; its branch may stay.
pub(crate) async fn create(
    root: &Path,
    cwd: &Path,
    workspace: &Path,
    agent: &str,
    cancellation: &CancellationToken,
) -> Result<Created, ToolError> {
    let top = git(cwd, &["rev-parse", "--show-toplevel"], cancellation)
        .await
        .map_err(|error| {
            ToolError::invalid_arguments(format!(
                "worktree needs cwd to be inside a git repository: {error}"
            ))
        })?;
    let source = std::fs::canonicalize(top.trim())
        .map_err(|error| ToolError::failed(format!("resolve the repository: {error}")))?;
    if !source.starts_with(workspace) {
        return Err(ToolError::invalid_arguments(format!(
            "worktree needs the repository holding cwd ({}) to be inside the workspace; \
             cwd is in a repository that reaches outside it",
            source.display()
        )));
    }
    let inside = cwd
        .strip_prefix(&source)
        .unwrap_or(Path::new(""))
        .to_owned();
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..6];
    let name = format!("{}-{suffix}", slug(agent));
    let branch = format!("scv/{name}");
    let group = root.join(group_name(&source));
    private_dir(root)
        .and_then(|()| private_dir(&group))
        .map_err(|error| ToolError::failed(format!("create the worktrees directory: {error}")))?;
    let path = group.join(&name);
    let record = group.join(format!("{name}.json"));
    write_record(
        &record,
        &Record {
            v: 1,
            source: source.clone(),
            branch: branch.clone(),
        },
    )
    .map_err(|error| ToolError::failed(format!("record the worktree: {error}")))?;
    let target = path.display().to_string();
    let added = git(
        &source,
        &["worktree", "add", "-b", &branch, &target, "HEAD"],
        cancellation,
    )
    .await;
    if let Err(error) = added {
        discard(&source, &path, &record).await;
        return Err(ToolError::failed(format!(
            "git worktree add failed: {error}"
        )));
    }
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    // A directory the repository does not track has no copy in the worktree.
    let cwd = Some(&inside)
        .filter(|inside| !inside.as_os_str().is_empty())
        .map(|inside| path.join(inside))
        .filter(|candidate| candidate.is_dir())
        .unwrap_or_else(|| path.clone());
    Ok(Created {
        worktree: WorktreeRef {
            path: path.display().to_string(),
            branch,
            repo: source.display().to_string(),
        },
        cwd,
    })
}

/// Undo a worktree whose creation failed or was cut off: git may have left
/// it half made and locked.
async fn discard(source: &Path, path: &Path, record: &Path) {
    let never = CancellationToken::new();
    if path.exists() {
        let target = path.display().to_string();
        let _ = git(
            source,
            &["worktree", "remove", "--force", "--force", &target],
            &never,
        )
        .await;
    }
    let _ = git(source, &["worktree", "prune"], &never).await;
    let _ = std::fs::remove_file(record);
}

/// The top of the SCV worktree under `root` holding `path`, when `path` is
/// in one.
pub(crate) fn worktree_path(root: &Path, path: &Path) -> Option<PathBuf> {
    worktree_top(root, path).map(|(top, _)| top)
}

/// The checkout the SCV worktree under `root` holding `path` was made from,
/// when `path` is in one.
pub fn worktree_repo(root: &Path, path: &Path) -> Option<PathBuf> {
    worktree_top(root, path).map(|(_, source)| source)
}

/// The top of the SCV worktree under `root` that holds `path`, and the
/// checkout it was made from. It must be a linked worktree whose `.git`
/// file and its repository's record of it point at each other, with SCV's
/// record beside it.
fn worktree_top(root: &Path, path: &Path) -> Option<(PathBuf, PathBuf)> {
    let root = std::fs::canonicalize(root).ok()?;
    let path = std::fs::canonicalize(path).ok()?;
    let relative = path.strip_prefix(&root).ok()?;
    let mut parts = relative.components();
    let (Some(Component::Normal(group)), Some(Component::Normal(name))) =
        (parts.next(), parts.next())
    else {
        return None;
    };
    let top = root.join(group).join(name);
    let pointer = std::fs::read_to_string(top.join(".git")).ok()?;
    // Git may write either link relative to the file holding it.
    let gitdir = top.join(pointer.trim().strip_prefix("gitdir:")?.trim());
    let back = std::fs::read_to_string(gitdir.join("gitdir")).ok()?;
    let back = std::fs::canonicalize(gitdir.join(back.trim())).ok()?;
    if back.parent()? != top {
        return None;
    }
    let mut record_name = name.to_os_string();
    record_name.push(".json");
    let record = read_record(&root.join(group).join(record_name))?;
    Some((top, record.source))
}

/// Whether `path` is inside an SCV worktree under `root` made from a
/// checkout inside `workspace` (canonical).
pub(crate) fn in_workspace(root: &Path, path: &Path, workspace: &Path) -> bool {
    worktree_repo(root, path).is_some_and(|source| source.starts_with(workspace))
}

/// Every worktree SCV created under `root`, with whether it has uncommitted
/// changes.
pub async fn list(root: &Path) -> Vec<WorktreeInfo> {
    let mut found = Vec::new();
    // Canonical, as the paths jobs record are.
    let Ok(root) = std::fs::canonicalize(root) else {
        return found;
    };
    let root = root.as_path();
    let Ok(groups) = std::fs::read_dir(root) else {
        return found;
    };
    let never = CancellationToken::new();
    for group in groups.filter_map(Result::ok) {
        let Ok(names) = std::fs::read_dir(group.path()) else {
            continue;
        };
        for name in names.filter_map(Result::ok) {
            let path = name.path();
            if !name.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Some(repo) = worktree_repo(root, &path) else {
                continue;
            };
            let branch = git(&path, &["rev-parse", "--abbrev-ref", "HEAD"], &never)
                .await
                .map(|branch| branch.trim().to_owned())
                .unwrap_or_default();
            let dirty = git(&path, &["status", "--porcelain"], &never)
                .await
                .ok()
                .map(|status| !status.trim().is_empty());
            found.push(WorktreeInfo {
                worktree: WorktreeRef {
                    path: path.display().to_string(),
                    branch,
                    repo: repo.display().to_string(),
                },
                dirty,
                jobs: Vec::new(),
            });
        }
    }
    found.sort_by(|a, b| a.worktree.path.cmp(&b.worktree.path));
    found
}

/// Remove the SCV worktree at `path` under `root` (`git worktree remove`),
/// refusing one with uncommitted changes unless `force`, which also removes
/// one a cut-off creation left locked. Its branch stays.
pub async fn remove(root: &Path, path: &Path, force: bool) -> Result<String, String> {
    let (path, source) = worktree_top(root, path)
        .ok_or_else(|| format!("{} is not a worktree SCV created", path.display()))?;
    let never = CancellationToken::new();
    let branch = git(&path, &["rev-parse", "--abbrev-ref", "HEAD"], &never)
        .await
        .map(|branch| branch.trim().to_owned())
        .unwrap_or_default();
    let target = path.display().to_string();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.extend(["--force", "--force"]);
    }
    args.push(&target);
    // Git removes a worktree from inside it too, which works whether or not
    // the checkout it came from is still there.
    git(&path, &args, &never).await.map_err(|error| {
        if force {
            format!("git worktree remove failed: {error}")
        } else {
            format!(
                "git worktree remove failed: {error}. A worktree with uncommitted changes is \
                 removed only with force"
            )
        }
    })?;
    if let (Some(group), Some(name)) = (path.parent(), path.file_name()) {
        let mut record = name.to_os_string();
        record.push(".json");
        let _ = std::fs::remove_file(group.join(record));
        // The repository's group goes with its last worktree.
        let _ = std::fs::remove_dir(group);
    }
    Ok(if branch.is_empty() {
        format!("Removed worktree {target}.")
    } else {
        format!(
            "Removed worktree {target}. Its branch {branch} is kept; delete it with `git branch \
             -D {branch}` in {} when you no longer need it.",
            source.display()
        )
    })
}

/// Run `git -C <dir> <args>` and return its standard output.
async fn git(
    dir: &Path,
    args: &[&str],
    cancellation: &CancellationToken,
) -> Result<String, String> {
    let mut command = Command::new("git");
    // Hooks and a file-system monitor are commands the repository's (or an
    // agent's) configuration names, which no one asked SCV to run.
    command.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
    ]);
    // Git and what it runs (filters) need no model or provider keys.
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|name| name.ends_with("_API_KEY")) {
            command.env_remove(name);
        }
    }
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command
        .spawn()
        .map_err(|error| format!("cannot run git: {error}"))?;
    let output = tokio::select! {
        output = tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output()) => output
            .map_err(|_| format!("git took longer than {} seconds", GIT_TIMEOUT.as_secs()))?
            .map_err(|error| format!("git failed: {error}"))?,
        () = cancellation.cancelled() => return Err("cancelled".into()),
    };
    let text = |bytes: &[u8]| {
        String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_GIT_OUTPUT)]).into_owned()
    };
    if output.status.success() {
        Ok(text(&output.stdout))
    } else {
        let stderr = text(&output.stderr);
        Err(stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("git exited with an error")
            .trim()
            .to_owned())
    }
}

/// `<repository name>-<8 hex of its path>`, one directory per repository.
fn group_name(repo: &Path) -> String {
    let digest = Sha256::digest(repo.as_os_str().as_encoded_bytes());
    let hex: String = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let name = repo
        .file_name()
        .map(|name| slug(&name.to_string_lossy()))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "repo".into());
    format!("{name}-{hex}")
}

/// Letters, digits, `-`, and `_` of `text`, at most 40 of them.
fn slug(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(40)
        .collect()
}

fn write_record(path: &Path, record: &Record) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
    scv_client::fs::replace_private(path, &bytes)
}

fn read_record(path: &Path) -> Option<Record> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES).read_to_end(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests;
