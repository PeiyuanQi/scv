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
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
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
    let created = create(&repo.root, &repo.repo.join("shop"), "codex", &never)
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
    let error = create(&repo.root, plain.path(), "codex", &CancellationToken::new())
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
