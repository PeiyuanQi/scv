---
name: feature-flow
description: Standard landing flow for SCV changes on this machine. Develop in a sibling worktree, pass the AGENTS.md gates, rebase onto origin/main (gh PR when gh is authenticated, plain git otherwise), publish every crate to crates.io, install the release locally, and restart the SCV daemon. Use when asked to land, ship, release, deploy, or finish a feature or fix in the SCV repository.
---

# SCV feature flow

`AGENTS.md` is the authority on how to develop. This skill fixes the order of
operations for delivering on a machine that runs the SCV daemon.

Landing pushes to `main` and publishes to crates.io, and neither can be
undone. Run stages 5-8 only when the user asked to land, ship, or release.
Asking only for the feature authorizes stages 1-4, not delivery.

Scripts live in `scripts/` next to this file. `publish.sh` and `deploy.sh` wrap
themselves in `host.sh`, so they work in both environments described in
stage 0.

## 0. Know where you run

- **Delegated by SCV:** `$SCV_HOME` ends in `/adapters/<agent>`. `HOME` and XDG
  then point at SCV's private adapter home, which has no git identity, SSH
  keys, `gh` login, rustup toolchain, crates.io token, or daemon socket. Run
  your own `git`, `gh`, `cargo`, `scv`, and `systemctl` commands as
  `scripts/host.sh <command...>`, which restores the real home. Outside SCV it
  passes straight through.
- **SCV delegating this flow:** call `agent_codex` or `agent_claude` with `cwd`
  set to this repository (`scv` in a `~/projects` workspace) so the agent loads
  this skill and `AGENTS.md`. The agent default of `tools.agent_timeout_seconds`
  (3600) covers a normal landing (gates, CI, and publishing take 20-50
  minutes); pass a larger `timeout_seconds`, up to `tools.max_timeout_seconds`
  (14400), when CI reruns are likely. The agent needs `permissions = "full"`
  in its `[agents.<name>]` user config to run commands and edit files unprompted.
- **Secrets:** never print `~/.scv/config.toml`, `~/.scv/channels/*/accounts/`,
  `~/.scv/adapters/*/auth.json`, or `~/.cargo/credentials.toml`.

## 1. Develop in a sibling worktree

- Read `AGENTS.md`, `README.md`, `docs/architecture.md`, and the `docs/` files
  for the area you touch.
- The main checkout (`~/projects/scv`) may hold the user's uncommitted work.
  Never edit, stash, reset, or pull there.
- Create an isolated worktree from the remote tip and work only inside it:
  ```sh
  git fetch origin
  git worktree add ../scv-<topic> -b <type>/<topic> origin/main
  ```
- Ship code, tests, and the final-state `docs/` as one end-to-end change.

## 2. Version

- **Shipped change** (anything under `crates/`, `src/`, or packaged files such
  as `README.md`): bump the patch version (`0.1.N` to `0.1.N+1`) everywhere
  together:
  - `[workspace.package] version` and every exact `=X.Y.Z` pin in
    `[workspace.dependencies]`;
  - every current-version mention in the docs, found with
    `grep -rn '<old>' --exclude-dir=target --exclude=Cargo.lock .`. Leave
    historical notes such as "saved by `0.1.16` or newer" alone.

  Then refresh the lockfile with `cargo build --offline`; `--locked` refuses
  the change. Commit the manifest and lockfile together.
- **Repo-only change** (skills, CI, unpackaged docs): no bump and no release.
  Stop after stage 5.
- **Batch small fixes:** crates.io accepts at most 20 new versions of a crate
  per 24 hours. Merge small fixes to `main` as they land, without a bump, and
  publish them together in one release, keeping under 20 versions a day.

## 3. Gates

All must pass. Report any you could not run.

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check advisories bans licenses sources  # when installed; CI always runs it
cargo build --release --locked
git diff --check
```

Treat every test failure as real, and never weaken or skip a failing test.
Process tests must wait for an observable condition under a generous ceiling,
never a fixed budget that includes process startup: `bash -l` sources the
host's login profile, which takes seconds on loaded CI runners.

## 4. Commit

- One cohesive commit per requested outcome (`AGENTS.md`).
- Use a conventional subject (`feat:`, `fix:`, `docs:`, `chore:`), and say why
  in the body.
- End the message with the attribution trailer your harness requires.

## 5. Land on origin/main by rebase

Never create merge commits, and never force-push `main`. Use the `gh` path when
`gh auth status --hostname github.com` succeeds (through `host.sh` when
delegated). Otherwise use plain git.

**With gh:**

```sh
git push -u origin HEAD
gh pr create --base main --fill
gh pr checks --watch --fail-fast
gh pr merge --rebase
git push origin --delete <type>/<topic>
```

Do not use `--delete-branch` here. After merging, gh tries to check out `main`
locally, which fails because the main checkout already holds `main`, and the
remote branch is then left behind.

- If checks fail, read `gh run view <run-id> --log-failed`.
  - A runner or network fault outside the tests: run
    `gh run rerun <run-id> --failed` once, then watch again.
  - A test failure: find the cause, fix it in the worktree, push, and repeat.
- If `main` moved or the PR conflicts: rebase locally as below, then run
  `git push --force-with-lease` on the task branch only.

**Without gh** (not installed, or not signed in):

```sh
git fetch origin
git rebase origin/main   # resolve conflicts; rerun stage 3 if upstream moved
git push origin HEAD:main
```

That push is a fast-forward. A rejection means `main` moved: fetch, rebase,
rerun the gates, and push again.

**After landing:** a rebase merge on GitHub rewrites commit IDs, so release
from the remote tip itself. Confirm your change is on it:

```sh
git fetch origin
git switch --detach origin/main
```

## 6. Publish to crates.io

```sh
scripts/publish.sh --check  # show which crates still need this version
scripts/publish.sh          # publish in AGENTS.md dependency order
```

Before publishing, the script requires a clean tree at `origin/main` and one
shared version. It publishes `scv-core`, `scv-protocol`, `scv-client`,
`scv-provider-openai`, `scv-tools`, `scv-channels`, `scv-clawbot`, `scv-feishu`,
`scv-server`, `scv-tui`, and `scv-cli` in that order, and skips any crate already on
crates.io, so a partial run can be resumed. It needs the `cargo login` token in the real home.

## 7. Install and restart

```sh
scripts/deploy.sh <version>
```

- Installs `scv-cli@<version>` from crates.io, retrying while the index catches
  up.
- Restarts `scv.service` with `systemctl --user`. Do not use `scv restart`: it
  demands sudo verification. Never pass `--allow-sudo` without the user's
  consent. Set `SCV_UNIT` for a custom-profile unit.
- Waits for the daemon to report the new version and for enabled channel
  accounts to reconnect, then prints recent journal warnings.
- **Delegated by SCV:** restarting the daemon would kill this agent mid-turn.
  The script instead schedules the restart 60 seconds out, via a transient
  `systemd-run` timer outside the daemon, and skips verification. Tell the
  user to check `scv status` and `scv channels status` afterwards.

## 8. Clean up and report

- Run `git worktree remove ../scv-<topic>`.
- Delete the local branch with `git branch -D <branch>` once its change is on
  `origin/main`. Also make sure the remote task branch is gone:
  `git ls-remote --heads origin <branch>` should print nothing.
- Prune stale remote branches. For each branch in
  `git branch -r | grep -v -e 'origin/main' -e HEAD`, delete it with
  `git push origin --delete <branch>` when its PR is merged or closed
  (`gh pr list --state all --head <branch>`) or it has none, and
  `git rev-list --count origin/main..origin/<branch>` prints 0 or
  `git cherry origin/main origin/<branch>` shows no `+` lines. Never delete
  `main`, a branch with an open PR, or one with commits not on `main`: report
  those instead. Finish with `git fetch --prune origin`.
- Report:
  - the commit(s) now on `main`;
  - the version published and the version installed;
  - daemon and channel status;
  - gates run or skipped;
  - the CI result;
  - remote branches pruned, and any kept with the reason.
