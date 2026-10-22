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
//! branch. It runs `git` in a worktree only once its link checks out: its
//! git directory must be the one the repository it was made from keeps for
//! it, so a worktree's agent cannot point git at configuration it wrote. A
//! worktree counts as part of the workspace its checkout is in, so an agent
//! call may name it as `cwd`.

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
/// Names drawn for a new worktree before giving up on a free one.
const NAME_DRAWS: usize = 8;

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
    /// The common git directory of `source`'s repository (canonical),
    /// whose `worktrees/` holds the worktree's git directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    common: Option<PathBuf>,
}

/// An SCV worktree whose git link checked out.
struct Linked {
    top: PathBuf,
    /// Its git directory, which the repository it was made from keeps.
    git_dir: PathBuf,
    /// The checkout it was made from.
    source: PathBuf,
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
    let common = common_dir(&source)
        .and_then(|common| std::fs::canonicalize(common).ok())
        .ok_or_else(|| ToolError::failed("resolve the repository's git directory"))?;
    let group = root.join(group_name(&source));
    private_dir(root)
        .and_then(|()| private_dir(&group))
        .map_err(|error| ToolError::failed(format!("create the worktrees directory: {error}")))?;
    // A name already taken (by a worktree or its record) is drawn again, so
    // this call never touches a worktree it did not create.
    let mut reserved = None;
    for _ in 0..NAME_DRAWS {
        let suffix = &uuid::Uuid::new_v4().simple().to_string()[..6];
        let name = format!("{}-{suffix}", slug(agent));
        let record = Record {
            v: 1,
            source: source.clone(),
            branch: format!("scv/{name}"),
            common: Some(common.clone()),
        };
        match reserve(&group, &name, &record) {
            Ok(()) => {
                reserved = Some((name, record.branch));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(ToolError::failed(format!("record the worktree: {error}")));
            }
        }
    }
    let (name, branch) =
        reserved.ok_or_else(|| ToolError::failed("no free worktree name; try again"))?;
    let path = group.join(&name);
    let record = group.join(format!("{name}.json"));
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
    worktree_top(root, path).map(|linked| linked.top)
}

/// The checkout the SCV worktree under `root` holding `path` was made from,
/// when `path` is in one.
pub fn worktree_repo(root: &Path, path: &Path) -> Option<PathBuf> {
    worktree_top(root, path).map(|linked| linked.source)
}

/// The SCV worktree under `root` that holds `path`, when its link checks
/// out ([`verified_git_dir`]).
fn worktree_top(root: &Path, path: &Path) -> Option<Linked> {
    let (top, record) = recorded(root, path)?;
    let git_dir = verified_git_dir(&top, &record)?;
    Some(Linked {
        top,
        git_dir,
        source: record.source,
    })
}

/// The top of the directory under `root` that holds `path`, where SCV puts
/// a worktree, and SCV's record beside it; nothing about its git link is
/// checked yet.
fn recorded(root: &Path, path: &Path) -> Option<(PathBuf, Record)> {
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
    let mut record_name = name.to_os_string();
    record_name.push(".json");
    let record = read_record(&root.join(group).join(record_name))?;
    Some((top, record))
}

/// The git directory of the SCV worktree at `top` (canonical) that
/// `record` describes, when its link checks out: it must be exactly
/// `<common>/worktrees/<name>` of the repository the worktree was made
/// from, and it and the worktree's `.git` file must name each other. An
/// agent can rewrite its worktree's `.git`, but not make git read another
/// git directory's configuration (filters, for one) through it.
fn verified_git_dir(top: &Path, record: &Record) -> Option<PathBuf> {
    let common = record
        .common
        .clone()
        .or_else(|| common_dir(&record.source))?;
    let common = std::fs::canonicalize(common).ok()?;
    let git_dir = common.join("worktrees").join(top.file_name()?);
    // Git may write each link relative to the file holding it.
    let canonical = |base: &Path, link: &str| std::fs::canonicalize(base.join(link.trim())).ok();
    let pointer = read_small(&top.join(".git"))?;
    let named = canonical(top, pointer.trim().strip_prefix("gitdir:")?)?;
    let back = canonical(&git_dir, &read_small(&git_dir.join("gitdir"))?)?;
    let shared = canonical(&git_dir, &read_small(&git_dir.join("commondir"))?)?;
    (named == git_dir && back == top.join(".git") && shared == common).then_some(git_dir)
}

/// The common git directory of the repository `checkout` (the top of a
/// checkout) belongs to: its `.git` directory or, for a linked worktree,
/// the one its git directory's `commondir` names.
fn common_dir(checkout: &Path) -> Option<PathBuf> {
    let dot_git = checkout.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let pointer = read_small(&dot_git)?;
    let git_dir = checkout.join(pointer.trim().strip_prefix("gitdir:")?.trim());
    Some(match read_small(&git_dir.join("commondir")) {
        Some(common) => git_dir.join(common.trim()),
        None => git_dir,
    })
}

/// A short text file's contents.
fn read_small(path: &Path) -> Option<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(MAX_RECORD_BYTES).read_to_string(&mut text).ok()?;
    Some(text)
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
            let Some((top, record)) = recorded(root, &path) else {
                continue;
            };
            // One whose link does not check out is listed from SCV's record
            // alone, with no git run in it: its state is unknown.
            let (branch, dirty) = match verified_git_dir(&top, &record) {
                Some(git_dir) => {
                    let linked = Linked {
                        top,
                        git_dir,
                        source: record.source.clone(),
                    };
                    let branch =
                        git_in_worktree(&linked, &["rev-parse", "--abbrev-ref", "HEAD"], &never)
                            .await
                            .map(|branch| branch.trim().to_owned())
                            .unwrap_or_default();
                    let dirty = git_in_worktree(&linked, &["status", "--porcelain"], &never)
                        .await
                        .ok()
                        .map(|status| !status.trim().is_empty());
                    (branch, dirty)
                }
                None => (record.branch, None),
            };
            found.push(WorktreeInfo {
                worktree: WorktreeRef {
                    path: path.display().to_string(),
                    branch,
                    repo: record.source.display().to_string(),
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
    let (top, record) = recorded(root, path)
        .ok_or_else(|| format!("{} is not a worktree SCV created", path.display()))?;
    let git_dir = verified_git_dir(&top, &record).ok_or_else(|| {
        format!(
            "{} is not linked to the repository it was made from ({}) as SCV made it, so SCV \
             runs no git in it; check its .git file, and remove it yourself",
            top.display(),
            record.source.display()
        )
    })?;
    let linked = Linked {
        top,
        git_dir,
        source: record.source,
    };
    let never = CancellationToken::new();
    let branch = git_in_worktree(&linked, &["rev-parse", "--abbrev-ref", "HEAD"], &never)
        .await
        .map(|branch| branch.trim().to_owned())
        .unwrap_or_default();
    let path = linked.top.clone();
    let source = linked.source.clone();
    let target = path.display().to_string();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.extend(["--force", "--force"]);
    }
    args.push(&target);
    // Git removes a worktree from inside it too, which works whether or not
    // the checkout it came from is still there.
    git_in_worktree(&linked, &args, &never)
        .await
        .map_err(|error| {
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
    let mut command = git_command();
    command.arg("-C").arg(dir).args(args);
    run(command, cancellation).await
}

/// Run `git <args>` in the verified worktree `linked`, naming its git
/// directory and top outright rather than finding them again through its
/// `.git` file, which its agent may change after the check.
async fn git_in_worktree(
    linked: &Linked,
    args: &[&str],
    cancellation: &CancellationToken,
) -> Result<String, String> {
    let mut git_dir = std::ffi::OsString::from("--git-dir=");
    git_dir.push(&linked.git_dir);
    let mut work_tree = std::ffi::OsString::from("--work-tree=");
    work_tree.push(&linked.top);
    let mut command = git_command();
    command
        .arg("-C")
        .arg(&linked.top)
        .arg(git_dir)
        .arg(work_tree)
        .args(args);
    run(command, cancellation).await
}

/// `git`, with what SCV never lets it use.
fn git_command() -> Command {
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
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// Run `command`, a [`git_command`], and return its standard output.
async fn run(mut command: Command, cancellation: &CancellationToken) -> Result<String, String> {
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

/// Take the name `name` in `group` for a new worktree: write its record,
/// which must not exist yet, unless a worktree of that name does
/// (`AlreadyExists` either way).
fn reserve(group: &Path, name: &str, record: &Record) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if std::fs::symlink_metadata(group.join(name)).is_ok() {
        return Err(std::io::ErrorKind::AlreadyExists.into());
    }
    let bytes = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
    let path = group.join(format!("{name}.json"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let written = file.write_all(&bytes).and_then(|()| file.sync_all());
    if written.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    written
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
