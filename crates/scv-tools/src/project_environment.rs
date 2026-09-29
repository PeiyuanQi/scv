//! Project-aware Rust preflight shared by delegated agents and the CLI doctor.
//! Resolves only installed tools; does not install, update, or edit rustup state.

mod project;

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::process::{ProcessSpec, execute_process};
use project::{Project, Version};

/// The environment additions and human-readable result of a launch preflight.
#[derive(Debug, Default)]
pub struct ProjectEnvironment {
    pub(crate) environment: Vec<(OsString, OsString)>,
    /// Paths and versions only; no credentials or arbitrary environment dump.
    pub diagnostics: Vec<String>,
}

/// Resolve the Rust tools for `cwd` before relocating an agent's home.
/// `environment` is its adapter environment (including its private HOME).
/// Non-Rust projects return no changes and do not need Rust installed.
pub async fn resolve(
    cwd: &Path,
    environment: &[(OsString, OsString)],
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<ProjectEnvironment> {
    let cancellation = CancellationToken::new();
    let resolution = resolve_from(
        cwd,
        environment,
        std::env::vars_os().collect(),
        cancellation.clone(),
    );
    tokio::pin!(resolution);
    let result = tokio::select! {
        result = &mut resolution => result,
        () = cancelled => {
            cancellation.cancel();
            // Join the probe and its process group before returning.
            resolution.await
        }
    };
    result
        .with_context(|| format!("Rust environment preflight for {}; run `scv agents doctor --workspace <project>` in the launch environment", cwd.display()))
}

async fn resolve_from(
    cwd: &Path,
    adapter: &[(OsString, OsString)],
    inherited: Vec<(OsString, OsString)>,
    cancellation: CancellationToken,
) -> Result<ProjectEnvironment> {
    let cwd = cwd.canonicalize().context("resolve project directory")?;
    if !cwd.is_dir() {
        bail!("project path is not a directory");
    }
    let project = Project::detect(&cwd)?;
    if !project.is_rust() {
        return Ok(ProjectEnvironment {
            diagnostics: vec!["No Rust project detected; environment unchanged.".into()],
            ..Default::default()
        });
    }
    let get = |key: &str| {
        adapter
            .iter()
            .rev()
            .chain(inherited.iter().rev())
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    };
    let host = |key: &str| {
        inherited
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    };
    // Tool installations belong to the launching user. Cargo's mutable state
    // and registry credentials still belong to the private agent home.
    let cargo_installs = [
        get("CARGO_HOME").map(PathBuf::from),
        host("HOME").map(|home| PathBuf::from(home).join(".cargo")),
    ];
    let rustup_home = get("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| host("HOME").map(|home| PathBuf::from(home).join(".rustup")));
    let path = get("PATH").unwrap_or_default();
    let rustup = cargo_installs
        .into_iter()
        .flatten()
        .map(|home| absolute(&cwd, &home).join("bin/rustup"))
        .find(|path| executable(path))
        .or_else(|| which::which_in("rustup", Some(&path), &cwd).ok());
    let mut environment = Vec::new();
    if let Some(home) = rustup_home {
        environment.push(("RUSTUP_HOME".into(), absolute(&cwd, &home).into()));
    }
    if let Some(home) = get("HOME") {
        environment.push((
            "CARGO_HOME".into(),
            absolute(&cwd, &PathBuf::from(home)).join(".cargo").into(),
        ));
    }
    environment.push(("RUSTUP_AUTO_INSTALL".into(), "0".into()));
    let mut probe_environment: Vec<_> = inherited
        .into_iter()
        .filter(|(key, _)| !crate::adapters::is_removed_agent_variable(key))
        .collect();
    probe_environment.extend(adapter.iter().cloned());
    probe_environment.extend(environment.iter().cloned());
    let probe = Probe {
        cwd: &cwd,
        environment: probe_environment,
        cancellation,
        deadline: Instant::now() + Duration::from_secs(20),
    };
    let mut diagnostics = Vec::new();
    if let Some(file) = &project.toolchain {
        diagnostics.push(format!("Toolchain file: {}", file.display()));
    }
    if let Some(manifest) = &project.manifest {
        diagnostics.push(format!(
            "Manifest: {}; minimum Rust {}",
            manifest.display(),
            project.minimum
        ));
    }
    let (rustc, cargo, selection) = if let Some(rustup) = &rustup {
        // `which` is intentionally used without --install. Rustup owns its
        // override precedence and toolchain-file format, including path tools.
        let rustc = probe.which(rustup, None, "rustc").await?;
        let cargo = probe.which(rustup, None, "cargo").await?;
        let active = probe.run(rustup, &["show", "active-toolchain"]).await?;
        let version = probe.version(&rustc, "rustc").await?;
        if version >= project.minimum {
            (rustc, cargo, format!("rustup: {}", active.trim()))
        } else if project.toolchain.is_some()
            || get_toolchain(&probe.environment).is_some()
            || !active.contains("(default)")
        {
            bail!(
                "selected toolchain {active} has Rust {version}, but the project requires {}; install/fix the explicit toolchain selection",
                project.minimum
            );
        } else {
            // A default is not a project pin. Consider installed stable tools,
            // by measured version, never by lexicographic channel/directory name.
            let listed = probe.run(rustup, &["toolchain", "list"]).await?;
            let mut compatible = Vec::new();
            for line in listed.lines().take(128) {
                let Some(candidate) = line.split_whitespace().next() else {
                    continue;
                };
                if Some(candidate) == active.strip_suffix(" (default)")
                    || !(candidate.starts_with("stable")
                        || candidate.starts_with(|character: char| character.is_ascii_digit()))
                {
                    continue;
                }
                let Ok(candidate_rustc) = probe.which(rustup, Some(candidate), "rustc").await
                else {
                    continue;
                };
                let Ok(version) = probe.version(&candidate_rustc, "rustc").await else {
                    continue;
                };
                if version >= project.minimum {
                    compatible.push((version, candidate.to_owned(), candidate_rustc));
                }
            }
            compatible.sort_by(|left, right| right.cmp(left));
            let mut selected = None;
            for (_, name, rustc) in compatible {
                if let Ok(cargo) = probe.which(rustup, Some(&name), "cargo").await {
                    selected = Some((
                        rustc,
                        cargo,
                        format!(
                            "installed compatible rustup toolchain: {name} (default {active} is too old)"
                        ),
                    ));
                    break;
                }
            }
            selected.with_context(|| format!("default Rust {version} is older than required {}; install a compatible toolchain with `rustup toolchain install stable` or the project's pinned version", project.minimum))?
        }
    } else {
        if project.toolchain.is_some() || get_toolchain(&probe.environment).is_some() {
            bail!(
                "project/inherited toolchain selection needs rustup; install rustup or expose its bin directory to SCV"
            );
        }
        let rustc = which::which_in("rustc", Some(&path), &cwd)
            .context("Rust project needs rustc; install a compatible Rust toolchain")?;
        let cargo = which::which_in("cargo", Some(&path), &cwd)
            .context("Rust project needs cargo; install a compatible Rust toolchain")?;
        (rustc, cargo, "system PATH (rustup unavailable)".into())
    };
    let rust_version = probe.version(&rustc, "rustc").await?;
    let cargo_version = probe.version(&cargo, "cargo").await?;
    if rust_version < project.minimum || cargo_version < project.minimum {
        bail!(
            "{selection}: rustc {rust_version}, cargo {cargo_version}; project requires Rust {} or newer. Install a compatible toolchain; SCV will not launch with these obsolete tools",
            project.minimum
        );
    }
    let mut paths = Vec::new();
    if let Some(rustup) = &rustup {
        paths.push(
            rustc
                .parent()
                .context("rustc has no bin directory")?
                .to_owned(),
        );
        paths.push(
            cargo
                .parent()
                .context("cargo has no bin directory")?
                .to_owned(),
        );
        paths.push(
            rustup
                .parent()
                .context("rustup has no bin directory")?
                .to_owned(),
        );
    }
    // Without rustup, retain PATH ordering: the validated system compiler
    // and Cargo may come from different directories with shadowed tools.
    paths.extend(std::env::split_paths(&path));
    let mut unique = Vec::new();
    for path in paths {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    environment.push(("PATH".into(), std::env::join_paths(unique)?));
    environment.push(("RUSTC".into(), rustc.clone().into()));
    let rustdoc = rustc.with_file_name("rustdoc");
    if executable(&rustdoc) {
        environment.push(("RUSTDOC".into(), rustdoc.into()));
    }
    diagnostics.push(format!("Selection: {selection}"));
    diagnostics.push(format!("rustc {rust_version}: {}", rustc.display()));
    diagnostics.push(format!("cargo {cargo_version}: {}", cargo.display()));
    for (key, value) in &environment {
        diagnostics.push(format!(
            "{}={}",
            key.to_string_lossy(),
            value.to_string_lossy()
        ));
    }
    Ok(ProjectEnvironment {
        environment,
        diagnostics,
    })
}

fn get_toolchain(environment: &[(OsString, OsString)]) -> Option<&OsStr> {
    environment
        .iter()
        .rev()
        .find(|(key, _)| key == "RUSTUP_TOOLCHAIN")
        .map(|(_, value)| value.as_os_str())
        .filter(|value| !value.is_empty())
}

fn absolute(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    }
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

struct Probe<'a> {
    cwd: &'a Path,
    environment: Vec<(OsString, OsString)>,
    cancellation: CancellationToken,
    deadline: Instant,
}

impl Probe<'_> {
    async fn run(&self, executable: &Path, args: &[&str]) -> Result<String> {
        if self.cancellation.is_cancelled() {
            bail!("Rust preflight cancelled");
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("Rust preflight exceeded 20 seconds");
        }
        let output = execute_process(
            ProcessSpec {
                executable: executable.into(),
                args: args.iter().map(OsString::from).collect(),
                cwd: self.cwd.to_owned(),
                environment: self.environment.clone(),
                clear_environment: true,
                sanitize_scv_environment: true,
                timeout: remaining.min(Duration::from_secs(5)),
                output_limit: 16 * 1024,
            },
            self.cancellation.clone(),
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        let value: serde_json::Value = serde_json::from_str(&output.content)?;
        let text = value["output"].as_str().unwrap_or_default().trim();
        if output.is_error() || output.truncated {
            bail!(
                "{} {args:?} failed (exit {}, timed out {}): {text}. Install the required toolchain/components explicitly; preflight never downloads them",
                executable.display(),
                value["exit_code"],
                value["timed_out"]
            );
        }
        Ok(text.to_owned())
    }

    async fn which(&self, rustup: &Path, toolchain: Option<&str>, binary: &str) -> Result<PathBuf> {
        let args = match toolchain {
            Some(toolchain) => vec!["which", "--toolchain", toolchain, binary],
            None => vec!["which", binary],
        };
        let path = PathBuf::from(self.run(rustup, &args).await?);
        if !path.is_absolute() || !executable(&path) {
            bail!(
                "rustup returned an unusable {binary} path: {}",
                path.display()
            );
        }
        Ok(path)
    }

    async fn version(&self, executable: &Path, name: &str) -> Result<Version> {
        let text = self.run(executable, &["--version"]).await?;
        let mut words = text.split_whitespace();
        if words.next() != Some(name) {
            bail!(
                "{} returned an invalid {name} version",
                executable.display()
            );
        }
        let version = words
            .next()
            .context("missing tool version")?
            .split('-')
            .next()
            .context("missing version number")?;
        Version::parse(version)
    }
}

#[cfg(test)]
mod tests;
