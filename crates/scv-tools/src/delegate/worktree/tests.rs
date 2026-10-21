//! Unit tests for `src/delegate/worktree.rs`. They run `git` on a
//! temporary repository, and skip where git is not installed.

use super::*;
use crate::delegate::request::resolve_call_cwd;

fn git_installed() -> bool {
    which::which("git").is_ok()
}

/// A repository with one commit and a `shop` directory, inside a
/// workspace, and a separate worktrees root.
struct Repo {
    _dir: tempfile::TempDir,
    workspace: PathBuf,
    repo: PathBuf,
    root: PathBuf,
}

fn git_in(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn repo() -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(dir.path()).unwrap().join("workspace");
    let repo = workspace.join("project");
    std::fs::create_dir_all(repo.join("shop")).unwrap();
    std::fs::write(repo.join("shop/cart.rs"), "fn main() {}\n").unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "first",
        ],
    ] {
        git_in(&repo, args);
    }
    let root = dir.path().join("home/worktrees");
    Repo {
        _dir: dir,
        workspace,
        repo,
        root,
    }
}

#[tokio::test]
async fn a_worktree_gets_its_own_branch_and_the_same_directory_inside_it() {
    if !git_installed() {
        return;
    }
    let repo = repo();
    let never = CancellationToken::new();
    let created = create(
        &repo.root,
        &repo.repo.join("shop"),
        &repo.workspace,
        "codex",
        &never,
    )
    .await
    .unwrap();
    let path = PathBuf::from(&created.worktree.path);
    assert!(path.starts_with(&repo.root), "{}", path.display());
    assert!(created.worktree.branch.starts_with("scv/codex-"));
    assert_eq!(created.worktree.repo, repo.repo.display().to_string());
    assert_eq!(created.cwd, path.join("shop"));
    assert!(created.cwd.join("cart.rs").is_file());
    assert_eq!(
        worktree_repo(&repo.root, &created.cwd),
        Some(repo.repo.clone())
    );
    // A directory inside it names the whole worktree.
    assert_eq!(worktree_path(&repo.root, &created.cwd), Some(path.clone()));
    assert_eq!(worktree_path(&repo.root, &repo.repo), None);
    assert!(in_workspace(&repo.root, &path, &repo.workspace));
    assert!(!in_workspace(&repo.root, &path, &repo.repo.join("shop")));
    // A call may name it as its cwd, though it is outside the workspace.
    let cwd = created.cwd.display().to_string();
    assert_eq!(
        resolve_call_cwd(&repo.workspace, Some(&repo.root), Some(&cwd), None).unwrap(),
        created.cwd
    );
    assert!(resolve_call_cwd(&repo.workspace, None, Some(&cwd), None).is_err());
    let elsewhere = tempfile::tempdir().unwrap();
    assert!(
        resolve_call_cwd(elsewhere.path(), Some(&repo.root), Some(&cwd), None).is_err(),
        "only a worktree of a repository in the workspace counts"
    );

    let listed = list(&repo.root).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].worktree.branch, created.worktree.branch);
    assert_eq!(listed[0].dirty, Some(false));
    std::fs::write(created.cwd.join("new.rs"), "x").unwrap();
    assert_eq!(list(&repo.root).await[0].dirty, Some(true));
    let refused = remove(&repo.root, &path, false).await.unwrap_err();
    assert!(refused.contains("only with force"), "{refused}");
    let removed = remove(&repo.root, &path, true).await.unwrap();
    assert!(removed.contains(&created.worktree.branch), "{removed}");
    assert!(!path.exists());
    assert_eq!(list(&repo.root).await, []);
}

#[tokio::test]
async fn only_a_git_repository_gets_a_worktree_and_only_scvs_are_removed() {
    if !git_installed() {
        return;
    }
    let repo = repo();
    let plain = tempfile::tempdir().unwrap();
    let plain_path = std::fs::canonicalize(plain.path()).unwrap();
    let error = create(
        &repo.root,
        &plain_path,
        &plain_path,
        "codex",
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, scv_core::ToolFailure::InvalidArguments);
    assert!(error.message.contains("git repository"), "{error}");
    assert!(remove(&repo.root, &repo.repo, true).await.is_err());
    assert_eq!(worktree_repo(&repo.root, &repo.repo), None);
}

#[test]
fn group_names_are_stable_slugs_of_the_repository() {
    let group = group_name(Path::new("/srv/my repo!"));
    assert!(group.starts_with("myrepo-"), "{group}");
    assert_eq!(group, group_name(Path::new("/srv/my repo!")));
    assert_ne!(group, group_name(Path::new("/other/my repo!")));
}

#[tokio::test]
async fn a_workspace_that_is_a_linked_worktree_gets_worktrees_too() {
    if !git_installed() {
        return;
    }
    // The workspace is a linked worktree of a repository outside it, as
    // when the owner works in `../project-feature`: git records the new
    // worktree under the main repository, yet it belongs to this workspace.
    let repo = repo();
    let linked = repo.workspace.parent().unwrap().join("linked");
    git_in(
        &repo.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            &linked.display().to_string(),
            "HEAD",
        ],
    );
    let linked = std::fs::canonicalize(&linked).unwrap();
    let created = create(
        &repo.root,
        &linked.join("shop"),
        &linked,
        "codex",
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(created.worktree.repo, linked.display().to_string());
    assert!(in_workspace(&repo.root, &created.cwd, &linked));
    let cwd = created.cwd.display().to_string();
    assert_eq!(
        resolve_call_cwd(&linked, Some(&repo.root), Some(&cwd), None).unwrap(),
        created.cwd
    );
    assert_eq!(list(&repo.root).await.len(), 1);
    remove(&repo.root, Path::new(&created.worktree.path), false)
        .await
        .unwrap();
    assert_eq!(list(&repo.root).await, []);
}

#[tokio::test]
async fn a_repository_reaching_outside_the_workspace_gets_no_worktree() {
    if !git_installed() {
        return;
    }
    // The workspace is a directory of a larger repository: a worktree would
    // hand the agent the whole repository, so nothing is created.
    let repo = repo();
    let shop = repo.repo.join("shop");
    let error = create(&repo.root, &shop, &shop, "codex", &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, scv_core::ToolFailure::InvalidArguments);
    assert!(error.message.contains("inside the workspace"), "{error}");
    assert!(!repo.root.exists() || list(&repo.root).await.is_empty());
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo.repo)
        .args(["branch", "--list", "scv/*"])
        .output()
        .unwrap();
    assert!(output.stdout.is_empty(), "no branch was created");
}

#[tokio::test]
async fn creating_a_worktree_runs_none_of_the_repositorys_hooks() {
    if !git_installed() {
        return;
    }
    let repo = repo();
    let marker = repo.workspace.join("hook-ran");
    let hooks = repo.repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-checkout");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    create(
        &repo.root,
        &repo.repo,
        &repo.workspace,
        "codex",
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!marker.exists(), "post-checkout ran");
}

#[tokio::test]
async fn force_removes_a_worktree_a_cut_off_creation_left_locked() {
    if !git_installed() {
        return;
    }
    let repo = repo();
    let created = create(
        &repo.root,
        &repo.repo,
        &repo.workspace,
        "codex",
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let path = created.worktree.path.clone();
    git_in(
        &repo.repo,
        &["worktree", "lock", "--reason", "initializing", &path],
    );
    let refused = remove(&repo.root, Path::new(&path), false)
        .await
        .unwrap_err();
    assert!(refused.contains("only with force"), "{refused}");
    remove(&repo.root, Path::new(&path), true).await.unwrap();
    assert!(!Path::new(&path).exists());
}
