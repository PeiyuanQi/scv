//! The feature-flow skill's `publish.sh`, run against a local git remote
//! with fake `curl`, `scv`, and `cargo`, so nothing reaches crates.io, a
//! daemon, or the network: an agent SCV delegated to asks the owner before
//! its first `cargo publish` and stops unless the answer is yes.

use std::{
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const CRATES: &str = "scv-core, scv-protocol, scv-client, scv-provider-openai, scv-tools, \
                      scv-channels, scv-server, scv-tui, scv-cli";

struct Checkout {
    root: tempfile::TempDir,
}

impl Checkout {
    /// A clone at `origin/main` of a repository whose workspace version is
    /// 9.9.9, beside a bin directory of fakes: `curl` reports every crate
    /// unpublished, `scv` records its arguments and exits with `answer`,
    /// and `cargo` records each call.
    fn new(answer: i32) -> Self {
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
                    "printf '%s\\n' \"$@\" > '{}/scv'\nexit {answer}\n",
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

const PARENT: &str = "0a1b2c3d/session/codex-3f9a2c";

#[test]
fn a_delegated_publish_asks_the_owner_first_and_publishes_on_yes() {
    let checkout = Checkout::new(0);
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
fn a_delegated_publish_stops_unless_the_owner_says_yes() {
    for (answer, why) in [(1, "did not say yes"), (2, "could not ask the owner")] {
        let checkout = Checkout::new(answer);
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
fn a_terminal_publish_and_a_check_never_ask() {
    let checkout = Checkout::new(1);
    let output = checkout.publish(&["--check"], Some(PARENT));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(checkout.logged("scv"), None);
    assert_eq!(checkout.logged("cargo"), None);
    let output = checkout.publish(&[], None);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(checkout.logged("scv"), None);
    assert_eq!(checkout.logged("cargo").unwrap().lines().count(), 9);
}
