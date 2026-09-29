//! Bounded, static project discovery; never run Cargo metadata or build scripts.

use std::{
    io::Read as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Version(pub(super) [u64; 3]);

impl Version {
    pub(super) fn parse(value: &str) -> Result<Self> {
        let mut version = [0; 3];
        let parts: Vec<_> = value.split('.').collect();
        if parts.is_empty() || parts.len() > 3 {
            bail!("invalid Rust version {value:?}");
        }
        for (index, part) in parts.iter().enumerate() {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                bail!("invalid Rust version {value:?}");
            }
            version[index] = part.parse()?;
        }
        Ok(Self(version))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0[0], self.0[1], self.0[2])
    }
}

#[derive(Debug, Default)]
pub(super) struct Project {
    pub(super) toolchain: Option<PathBuf>,
    pub(super) manifest: Option<PathBuf>,
    pub(super) minimum: Version,
}

impl Project {
    pub(super) fn detect(cwd: &Path) -> Result<Self> {
        let mut project = Self::default();
        // Match rustup's ancestor search and legacy-file precedence. Rustup,
        // not SCV, interprets channels, paths, components and future options.
        for dir in cwd.ancestors() {
            if project.toolchain.is_none() {
                for name in ["rust-toolchain", "rust-toolchain.toml"] {
                    let path = dir.join(name);
                    if path
                        .try_exists()
                        .with_context(|| format!("inspect {}", path.display()))?
                    {
                        project.toolchain = Some(path);
                        break;
                    }
                }
            }
            if project.manifest.is_none() {
                let path = dir.join("Cargo.toml");
                if path
                    .try_exists()
                    .with_context(|| format!("inspect {}", path.display()))?
                {
                    project.manifest = Some(path);
                }
            }
        }
        if let Some(manifest) = &project.manifest {
            project.minimum = requirements(manifest)?;
        }
        Ok(project)
    }

    pub(super) fn is_rust(&self) -> bool {
        self.toolchain.is_some() || self.manifest.is_some()
    }
}

fn read_manifest(path: &Path) -> Result<toml::Value> {
    const LIMIT: u64 = 1024 * 1024;
    let mut text = String::new();
    std::fs::File::open(path)
        .with_context(|| format!("read {}", path.display()))?
        .take(LIMIT + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > LIMIT {
        bail!("{} exceeds the 1 MiB manifest limit", path.display());
    }
    toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn requirements(manifest: &Path) -> Result<Version> {
    let value = read_manifest(manifest)?;
    let package = value.get("package");
    let workspace = workspace_manifest(manifest, &value)?;
    let inherited = workspace
        .as_ref()
        .and_then(|value| value.get("workspace"))
        .and_then(|value| value.get("package"));
    let mut minimum = Version::default();
    // A virtual workspace's declared common requirements also apply when
    // delegating at its root, before selecting a particular member.
    for key in ["rust-version", "edition"] {
        let field = package.and_then(|package| package.get(key));
        let field = if field
            .and_then(|field| field.get("workspace"))
            .and_then(toml::Value::as_bool)
            == Some(true)
        {
            Some(
                inherited
                    .and_then(|package| package.get(key))
                    .with_context(|| {
                        format!(
                            "{} inherits {key}, but workspace.package.{key} is missing",
                            manifest.display()
                        )
                    })?,
            )
        } else if package.is_none() {
            inherited.and_then(|package| package.get(key))
        } else {
            field
        };
        if let Some(field) = field {
            let text = field.as_str().with_context(|| {
                format!(
                    "{}: {key} must be a string or workspace inheritance",
                    manifest.display()
                )
            })?;
            let version = if key == "rust-version" {
                Version::parse(text)?
            } else {
                match text {
                    "2015" => Version([1, 0, 0]),
                    "2018" => Version([1, 31, 0]),
                    "2021" => Version([1, 56, 0]),
                    "2024" => Version([1, 85, 0]),
                    _ => bail!(
                        "{}: unsupported Rust edition {text:?}; update SCV's project resolver",
                        manifest.display()
                    ),
                }
            };
            minimum = minimum.max(version);
        }
    }
    Ok(minimum)
}

fn workspace_manifest(manifest: &Path, value: &toml::Value) -> Result<Option<toml::Value>> {
    if value.get("workspace").is_some() {
        return Ok(Some(value.clone()));
    }
    let dir = manifest.parent().context("manifest has no parent")?;
    if let Some(workspace) = value
        .get("package")
        .and_then(|package| package.get("workspace"))
    {
        let path = workspace
            .as_str()
            .context("package.workspace must be a path")?;
        return read_manifest(&dir.join(path).join("Cargo.toml")).map(Some);
    }
    // Only seek a workspace if a package actually inherits a field.
    let inherits = ["rust-version", "edition"].iter().any(|key| {
        value
            .get("package")
            .and_then(|package| package.get(key))
            .and_then(|field| field.get("workspace"))
            .and_then(toml::Value::as_bool)
            == Some(true)
    });
    if inherits {
        for parent in dir.ancestors().skip(1) {
            let path = parent.join("Cargo.toml");
            if path.try_exists()? {
                let value = read_manifest(&path)?;
                if value.get("workspace").is_some() {
                    return Ok(Some(value));
                }
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
