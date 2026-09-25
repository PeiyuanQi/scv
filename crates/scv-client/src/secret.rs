//! A string that must never reach a log or an error message.

use serde::{Deserialize, Serialize};
use std::fmt;

/// An API key, token, or app secret.
///
/// It serializes as the plain string, so settings and credential files keep
/// their format, but its `Debug` output is `<redacted>`: a struct holding one
/// can derive `Debug` without leaking it. Read the value with
/// [`expose`](Self::expose), or through `Deref<Target = str>`, only where it
/// is sent to the service it belongs to.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The secret itself, owned.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl std::ops::Deref for Secret {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[cfg(test)]
mod tests;
