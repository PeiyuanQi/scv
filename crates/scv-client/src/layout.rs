//! Where an SCV instance keeps everything, in one place.
//!
//! An instance home (`SCV_HOME`, default `~/.scv`) holds exactly:
//!
//! - `config.toml`: every setting a person edits, including channel accounts;
//! - `credentials/`: sign-ins SCV writes itself (channel logins);
//! - `agents/<name>/`: the private homes of delegated agent CLIs, which keep
//!   their own sign-ins and configuration there;
//! - `skills/`: the user's SCV skills;
//! - `state/`: runtime data SCV writes: the daemon socket and lock, delegated
//!   run records, conversation markers, import records, and channel delivery
//!   state and locks.
//!
//! Anything else in the home is not read by SCV; [`Layout::strays`] lists it.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Top-level entries of an instance home, in display order.
pub const ENTRIES: [&str; 5] = ["config.toml", "credentials", "agents", "skills", "state"];

/// Paths earlier releases used, which SCV no longer reads.
const LEGACY: [&str; 7] = [
    "adapters",
    "channels",
    "run",
    "server.sock",
    "server.lock",
    "clawbot",
    "clawbot.toml",
];

/// The paths of one SCV instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    home: PathBuf,
}

/// Something in an instance home that SCV does not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stray {
    pub path: PathBuf,
    /// A path an earlier SCV release used, rather than an unknown file.
    pub legacy: bool,
}

impl Layout {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// The instance selected by `SCV_HOME`, or `~/.scv`.
    pub fn from_env() -> Result<Self> {
        std::env::var_os("SCV_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|path| path.join(".scv")))
            .map(Self::new)
            .context("cannot determine SCV_HOME")
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The settings file a person edits.
    pub fn config(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    /// Sign-ins SCV writes itself.
    pub fn credentials(&self) -> PathBuf {
        self.home.join("credentials")
    }

    /// One channel's account credentials, `<account>.json` each.
    pub fn channel_credentials(&self, channel: &str) -> PathBuf {
        self.credentials().join(channel)
    }

    /// The private homes of delegated agent CLIs.
    pub fn agents(&self) -> PathBuf {
        self.home.join("agents")
    }

    pub fn agent_home(&self, agent: &str) -> PathBuf {
        self.agents().join(agent)
    }

    pub fn skills(&self) -> PathBuf {
        self.home.join("skills")
    }

    /// Runtime data SCV writes and reads back; never edited by hand.
    pub fn state(&self) -> PathBuf {
        self.home.join("state")
    }

    pub fn socket(&self) -> PathBuf {
        self.state().join("server.sock")
    }

    /// Records of running delegated agents.
    pub fn delegations(&self) -> PathBuf {
        self.state().join("delegations")
    }

    /// Markers of live delegated conversations, for `scv agents gc`.
    pub fn conversations(&self) -> PathBuf {
        self.state().join("conversations")
    }

    /// What each `scv agents import` copied, and from where.
    pub fn imports(&self) -> PathBuf {
        self.state().join("imports")
    }

    /// One channel's delivery state and account locks.
    pub fn channel_state(&self, channel: &str) -> PathBuf {
        self.state().join("channels").join(channel)
    }

    /// Serializes SCV's own edits of `config.toml`.
    pub fn config_lock(&self) -> PathBuf {
        self.state().join("config.lock")
    }

    /// Entries of the home that SCV does not read, sorted by name.
    pub fn strays(&self) -> Result<Vec<Stray>> {
        let entries = match std::fs::read_dir(&self.home) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", self.home.display()));
            }
        };
        let mut strays = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            let Some(text) = name.to_str() else {
                strays.push(Stray {
                    path: self.home.join(&name),
                    legacy: false,
                });
                continue;
            };
            if ENTRIES.contains(&text) {
                continue;
            }
            strays.push(Stray {
                path: self.home.join(text),
                legacy: LEGACY.contains(&text),
            });
        }
        strays.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(strays)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_path_lives_under_one_of_the_top_level_entries() {
        let layout = Layout::new("/h");
        for path in [
            layout.config(),
            layout.channel_credentials("wechat"),
            layout.agent_home("codex"),
            layout.skills(),
            layout.socket(),
            layout.delegations(),
            layout.conversations(),
            layout.imports(),
            layout.channel_state("feishu"),
            layout.config_lock(),
        ] {
            let top = path
                .strip_prefix("/h")
                .unwrap()
                .components()
                .next()
                .unwrap();
            let top = top.as_os_str().to_str().unwrap();
            assert!(ENTRIES.contains(&top), "{}", path.display());
        }
        assert_eq!(layout.socket(), Path::new("/h/state/server.sock"));
    }

    #[test]
    fn strays_name_old_layout_paths_and_unknown_files() {
        let home = tempfile::tempdir().unwrap();
        for directory in ["state", "agents", "adapters", "notes"] {
            std::fs::create_dir(home.path().join(directory)).unwrap();
        }
        for file in ["config.toml", "server.sock", "config.toml.bak"] {
            std::fs::write(home.path().join(file), "").unwrap();
        }
        let strays = Layout::new(home.path()).strays().unwrap();
        let named: Vec<(String, bool)> = strays
            .iter()
            .map(|stray| {
                (
                    stray
                        .path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    stray.legacy,
                )
            })
            .collect();
        assert_eq!(
            named,
            [
                ("adapters".to_owned(), true),
                ("config.toml.bak".to_owned(), false),
                ("notes".to_owned(), false),
                ("server.sock".to_owned(), true),
            ]
        );
        assert!(
            Layout::new(home.path().join("missing"))
                .strays()
                .unwrap()
                .is_empty()
        );
    }
}
