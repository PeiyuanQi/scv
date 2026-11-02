//! The feature-flow skill's `publish.sh`, run against a local git remote
//! with fake `curl`, `scv`, and `cargo`, so nothing reaches crates.io, a
//! daemon, or the network: the whole release goes through
//! `scv policy check publish` before the first `cargo publish` and stops
//! unless it says go. An installed `scv` without the rule makes an agent SCV
//! delegated to ask the owner with `scv confirm` instead, and one too old to
//! ask says what to do.

use std::{
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const CRATES: &str = "scv-core, scv-protocol, scv-client, scv-provider-openai, scv-tools, \
                      scv-channels, scv-server, scv-tui, scv-cli";

/// What the installed `scv` offers `publish.sh`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Scv {
    /// 0.2.1: neither `policy` nor `confirm`.
    TooOld,
    /// `confirm`, but not yet `policy`.
    Confirm,
    /// `policy check publish`.
    Policy,
}

struct Checkout {
    root: tempfile::TempDir,
}

impl Checkout {
    /// A clone at `origin/main` of a repository whose workspace version is
    /// 9.9.9, beside a bin directory of fakes: `curl` reports every crate
    /// unpublished, `scv` (with `scv policy`) records its arguments and
    /// exits with `answer`, and `cargo` records each call.
    fn new(answer: i32) -> Self {
        Self::with_scv(answer, Scv::Policy)
    }

    /// [`Checkout::new`], with an `scv` that offers what `scv` says; one
    /// without a subcommand refuses it.
    fn with_scv(answer: i32, scv: Scv) -> Self {
        let root = tempfile::tempdir().unwrap();
        let checkout = Self { root };
        let bin = checkout.path("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = checkout.path("log");
        std::fs::create_dir_all(&log).unwrap();
        for (name, script) in [
            ("curl", "printf 404\n".to_owned()),
            (
                "scv",
                format!(
                    "if [ \"$1\" = --version ]; then echo 'scv 0.2.1'; exit 0; fi\n\
                     if [ \"$1\" = policy ] && [ \"$2\" = --help ]; then {}; fi\n\
                     if [ \"$1\" = confirm ] && [ \"$2\" = --help ]; then {}; fi\n\
                     printf '%s\\n' \"$@\" > '{}/scv'\nexit {answer}\n",
                    offers(scv >= Scv::Policy, "policy"),
                    offers(scv >= Scv::Confirm, "confirm"),
                    log.display()
                ),
            ),
            (
                "cargo",
                format!("printf '%s\\n' \"$*\" >> '{}/cargo'\n", log.display()),
            ),
        ] {
            let path = bin.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{script}")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let repo = checkout.path("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace.package]\nversion = \"9.9.9\"\n",
        )
        .unwrap();
        checkout.git(&["init", "--bare", "--quiet", "../origin.git"]);
        checkout.git(&["init", "--quiet"]);
        checkout.git(&["add", "Cargo.toml"]);
        checkout.git(&["commit", "--quiet", "-m", "feat: the release"]);
        checkout.git(&["remote", "add", "origin", "../origin.git"]);
        checkout.git(&["push", "--quiet", "origin", "HEAD:main"]);
        checkout.git(&["fetch", "--quiet", "origin"]);
        checkout
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        // The fakes shadow any real `scv`, `cargo`, and `curl`; nothing else
        // of the caller's environment (such as its SCV home or git
        // configuration) comes along.
        let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
        command
            .env_clear()
            .env("PATH", format!("{}:{path}", self.path("bin").display()))
            .env("HOME", self.root.path())
            .env("SCV_HOME", self.path("scv-home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .current_dir(self.path("repo"));
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self
            .command("git")
            .args(["-c", "init.defaultBranch=main"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn publish(&self, args: &[&str], parent: Option<&str>) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".agents/skills/feature-flow/scripts/publish.sh");
        let mut command = self.command("bash");
        command.arg(script).args(args);
        if let Some(parent) = parent {
            command.env("SCV_PARENT", parent);
        }
        command.output().unwrap()
    }

    fn logged(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.path("log").join(name)).ok()
    }
}

/// How a fake `scv` answers `<subcommand> --help`.
fn offers(offered: bool, subcommand: &str) -> String {
    if offered {
        "exit 0".into()
    } else {
        format!("echo \"error: unrecognized subcommand '{subcommand}'\" >&2; exit 2")
    }
}

const PARENT: &str = "0a1b2c3d/session/codex-3f9a2c";

/// The `scv policy check publish` call for the whole release.
fn policy_check() -> String {
    format!(
        "policy\ncheck\npublish\n--version\n9.9.9\n{}\n",
        CRATES.replace(", ", "\n")
    )
}

#[test]
fn a_publish_goes_through_the_publish_rule_first_and_publishes_on_go() {
    for parent in [Some(PARENT), None] {
        let checkout = Checkout::new(0);
        let output = checkout.publish(&[], parent);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            checkout.logged("scv").unwrap(),
            policy_check(),
            "{parent:?}"
        );
        let published = checkout.logged("cargo").unwrap();
        assert_eq!(published.lines().count(), 9, "{published}");
        assert_eq!(
            published.lines().next(),
            Some("publish --locked -p scv-core")
        );
    }
}

#[test]
fn a_publish_stops_unless_the_publish_rule_says_go() {
    for (answer, why) in [
        (1, "the publish rule or the owner said no"),
        (2, "could not decide or could not ask the owner"),
    ] {
        let checkout = Checkout::new(answer);
        let output = checkout.publish(&[], Some(PARENT));
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(why), "{stderr}");
        assert!(stderr.contains("nothing was published"), "{stderr}");
        assert_eq!(checkout.logged("scv").unwrap(), policy_check());
        assert_eq!(checkout.logged("cargo"), None, "nothing was published");
    }
}

#[test]
fn a_check_never_asks() {
    let checkout = Checkout::new(1);
    let output = checkout.publish(&["--check"], Some(PARENT));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(checkout.logged("scv"), None);
    assert_eq!(checkout.logged("cargo"), None);
}

#[test]
fn without_the_rule_a_delegated_publish_asks_the_owner_first_and_publishes_on_yes() {
    let checkout = Checkout::with_scv(0, Scv::Confirm);
    let output = checkout.publish(&[], Some(PARENT));
    assert!(output.status.success(), "{output:?}");
    let head = checkout.git(&["log", "-1", "--format=%h"]);
    let asked = checkout.logged("scv").unwrap();
    assert_eq!(
        asked,
        format!(
            "confirm\n--timeout\n1800\n--\nPublish SCV 9.9.9 to crates.io from origin/main {} \
             feat: the release?\nCrates: {CRATES}.\nPublishing cannot be undone.\n",
            head.trim()
        )
    );
    let published = checkout.logged("cargo").unwrap();
    assert_eq!(published.lines().count(), 9, "{published}");
    assert_eq!(
        published.lines().next(),
        Some("publish --locked -p scv-core")
    );
}

#[test]
fn without_the_rule_a_delegated_publish_stops_unless_the_owner_says_yes() {
    for (answer, why) in [
        (1, "did not say yes"),
        (2, "could not ask the owner"),
        (2, "If the running daemon is too old to ask"),
    ] {
        let checkout = Checkout::with_scv(answer, Scv::Confirm);
        let output = checkout.publish(&[], Some(PARENT));
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(why), "{stderr}");
        assert!(stderr.contains("nothing was published"), "{stderr}");
        assert!(checkout.logged("scv").is_some());
        assert_eq!(checkout.logged("cargo"), None, "nothing was published");
    }
}

#[test]
fn without_the_rule_a_terminal_publish_does_not_ask() {
    let checkout = Checkout::with_scv(1, Scv::Confirm);
    let output = checkout.publish(&[], None);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(checkout.logged("scv"), None);
    assert_eq!(checkout.logged("cargo").unwrap().lines().count(), 9);
}

#[test]
fn a_delegated_publish_with_an_scv_too_old_to_ask_says_to_publish_from_a_terminal() {
    let checkout = Checkout::with_scv(0, Scv::TooOld);
    let output = checkout.publish(&[], Some(PARENT));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    for why in [
        "the installed scv 0.2.1 cannot ask the owner in chat",
        "nothing was published",
        "Publish this release from a\nterminal",
    ] {
        assert!(stderr.contains(why), "{why:?} in {stderr}");
    }
    assert_eq!(checkout.logged("scv"), None, "no question was asked");
    assert_eq!(checkout.logged("cargo"), None, "nothing was published");
}
