//! Git worktrees SCV creates so a job's agent works apart from its
//! repository's own checkout (`agent` calls with `worktree: true`).
//!
//! A worktree lives under the instance's
//! [`Layout::worktrees`](scv_client::Layout::worktrees), grouped by
//! repository as `<repository>-<digest>/<agent>-<6 hex>`, on a new branch
//! `scv/<agent>-<6 hex>` from the repository's current `HEAD`. SCV runs
//! `git` only to create, list, and remove these worktrees, with fixed
//! arguments and a time limit; it never commits, merges, or deletes a
//! branch. A worktree counts as part of the workspace its repository is in,
//! so an agent call may name it as `cwd`.

use std::{
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use scv_core::ToolError;
use scv_protocol::{WorktreeInfo, WorktreeRef};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// How long one `git` command may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest output read from one `git` command.
const MAX_GIT_OUTPUT: usize = 64 * 1024;

/// A worktree SCV just created, and where the agent runs in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub worktree: WorktreeRef,
    /// The directory the call asked for, inside the worktree.
    pub cwd: PathBuf,
}

/// Create a worktree under `root` for an `agent` call that would run in
/// `cwd`, a directory of a git repository, and return where it is.
pub(crate) async fn create(
    root: &Path,
    cwd: &Path,
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
    let repo = std::fs::canonicalize(top.trim())
        .map_err(|error| ToolError::failed(format!("resolve the repository: {error}")))?;
    let inside = cwd.strip_prefix(&repo).unwrap_or(Path::new("")).to_owned();
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..6];
    let name = format!("{}-{suffix}", slug(agent));
    let branch = format!("scv/{name}");
    let group = root.join(group_name(&repo));
    private_dir(root)
        .and_then(|()| private_dir(&group))
        .map_err(|error| ToolError::failed(format!("create the worktrees directory: {error}")))?;
    let path = group.join(&name);
    let target = path.display().to_string();
    git(
        &repo,
        &["worktree", "add", "-b", &branch, &target, "HEAD"],
        cancellation,
    )
    .await
    .map_err(|error| ToolError::failed(format!("git worktree add failed: {error}")))?;
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
            repo: repo.display().to_string(),
        },
        cwd,
    })
}

/// The repository whose SCV worktree under `root` holds `path`, when it is
/// one: its `.git` file points into that repository's worktree records.
pub fn worktree_repo(root: &Path, path: &Path) -> Option<PathBuf> {
    worktree_top(root, path).map(|(_, repo)| repo)
}

/// The top of the SCV worktree under `root` that holds `path`, and its
/// repository.
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
    let gitdir = PathBuf::from(pointer.trim().strip_prefix("gitdir:")?.trim());
    // `<repo>/.git/worktrees/<name>`: the repository is above `.git`.
    let dot_git = gitdir
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == ".git"))?;
    let repo = std::fs::canonicalize(dot_git.parent()?).ok()?;
    // The worktree must be one this repository records.
    let recorded = std::fs::read_to_string(gitdir.join("gitdir")).ok()?;
    let recorded = std::fs::canonicalize(Path::new(recorded.trim()).parent()?).ok()?;
    (recorded == top).then_some((top, repo))
}

/// Whether `path` is inside an SCV worktree under `root` whose repository
/// is inside `workspace` (canonical).
pub(crate) fn in_workspace(root: &Path, path: &Path, workspace: &Path) -> bool {
    worktree_repo(root, path).is_some_and(|repo| repo.starts_with(workspace))
}

/// Every worktree SCV created under `root`, with whether it has uncommitted
/// changes.
pub async fn list(root: &Path) -> Vec<WorktreeInfo> {
    let mut found = Vec::new();
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
/// refusing one with uncommitted changes unless `force`. Its branch stays.
pub async fn remove(root: &Path, path: &Path, force: bool) -> Result<String, String> {
    let (path, repo) = worktree_top(root, path)
        .ok_or_else(|| format!("{} is not a worktree SCV created", path.display()))?;
    let never = CancellationToken::new();
    let branch = git(&path, &["rev-parse", "--abbrev-ref", "HEAD"], &never)
        .await
        .map(|branch| branch.trim().to_owned())
        .unwrap_or_default();
    let target = path.display().to_string();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&target);
    git(&repo, &args, &never).await.map_err(|error| {
        if force {
            format!("git worktree remove failed: {error}")
        } else {
            format!(
                "git worktree remove failed: {error}. A worktree with uncommitted changes is \
                 removed only with force"
            )
        }
    })?;
    if let Some(group) = path.parent() {
        // The repository's group goes with its last worktree.
        let _ = std::fs::remove_dir(group);
    }
    Ok(if branch.is_empty() {
        format!("Removed worktree {target}.")
    } else {
        format!(
            "Removed worktree {target}. Its branch {branch} is kept; delete it with `git branch \
             -D {branch}` in {} when you no longer need it.",
            repo.display()
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

fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests;
