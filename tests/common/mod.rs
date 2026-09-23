//! Helpers shared by the integration tests.
#![allow(dead_code)]

use std::path::Path;

/// SCV selectors a developer's shell may export, which would otherwise leak a
/// real configuration or delegation context into a spawned binary.
const INHERITED_SCV_ENV: &[&str] = &[
    "SCV_CONFIG",
    "SCV_PARENT",
    "SCV_DELEGATION_DEPTH",
    "SCV_MODEL",
    "SCV_PROVIDER",
    "SCV_BASE_URL",
    "SCV_API_KEY_ENV",
];

/// Confines a spawned SCV binary to a temporary instance home.
///
/// `SCV_HOME` selects the instance and `HOME` points inside it, so neither an
/// explicit nor a fallback lookup can reach the developer's real `~/.scv`.
/// Every test that spawns an SCV binary must use this; the
/// `every_spawned_scv_binary_is_isolated` guard enforces it.
pub trait Isolated {
    fn isolated(&mut self, home: &Path) -> &mut Self;
}

impl Isolated for std::process::Command {
    fn isolated(&mut self, home: &Path) -> &mut Self {
        if let Some(real) = std::env::var_os("HOME") {
            assert!(
                !home.starts_with(Path::new(&real).join(".scv")),
                "test home {} is inside the real ~/.scv",
                home.display()
            );
        }
        let user_home = home.join(".no-user-home");
        self.env("SCV_HOME", home).env("HOME", &user_home);
        for variable in INHERITED_SCV_ENV {
            self.env_remove(variable);
        }
        self
    }
}

impl Isolated for tokio::process::Command {
    fn isolated(&mut self, home: &Path) -> &mut Self {
        self.as_std_mut().isolated(home);
        self
    }
}
