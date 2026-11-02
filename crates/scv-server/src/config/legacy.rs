//! The keys `[policy] mode` replaced, accepted as deprecated aliases until a
//! later breaking release removes them: `[tools] approval_policy` and
//! `[agents.<name>] permissions`.
//!
//! Each layer is rewritten before it is merged, keeping the old meaning:
//! `on-risk` stays `on-risk`, `always` stays `always`, and `never` becomes
//! `read-only`; `permissions = "full"` becomes `mode = "auto"`, and
//! `"default"` becomes `mode = "on-risk"`, so that agent keeps starting with
//! no switches. A line equal to the old default is honored too, since
//! nothing tells a copied sample line from a decision. A layer that sets an
//! old key and its new key is refused.

use anyhow::{Result, bail};

/// One deprecated key a layer set, and what it is read as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deprecated {
    /// The key as written, such as `tools.approval_policy`.
    pub key: String,
    /// Its value as written, such as `on-risk`.
    pub value: String,
    /// The key it is read as, such as `policy.mode`.
    pub replacement: String,
    /// The value it is read as, such as `read-only`.
    pub mode: String,
    /// The layer that set it, as `scv config show` names layers.
    pub origin: String,
    /// The line pins the old default, which `auto` replaced.
    pub pins_old_default: bool,
}

impl Deprecated {
    /// The startup warning, which names the replacement line.
    pub fn warning(&self) -> String {
        let (table, field) = self.replacement.rsplit_once('.').unwrap_or_default();
        let advice = if self.pins_old_default {
            "; it pins the old default, so delete it for auto"
        } else if self.mode == "auto" {
            ", which is now the default, so it can be deleted"
        } else {
            ""
        };
        format!(
            "{}: {} = \"{}\" is deprecated and read as [{table}] {field} = \"{}\"; replace it \
             with that line{advice}",
            self.origin, self.key, self.value, self.mode
        )
    }

    /// What `scv config show` adds to the setting it became.
    pub fn note(&self) -> String {
        let pins = if self.pins_old_default {
            "; pins the old default; delete it for auto"
        } else {
            ""
        };
        format!("set as deprecated {} = \"{}\"{pins}", self.key, self.value)
    }
}

/// Rewrite the deprecated keys of the layer `origin` into the keys that
/// replace them, returning what it set.
pub(super) fn migrate(layer: &mut toml::Value, origin: &str) -> Result<Vec<Deprecated>> {
    let mut found = Vec::new();
    let Some(root) = layer.as_table_mut() else {
        return Ok(found);
    };
    let old_policy = root
        .get_mut("tools")
        .and_then(toml::Value::as_table_mut)
        .and_then(|tools| tools.remove("approval_policy"));
    if let Some(old) = old_policy {
        let (value, mode, pins) = match old.as_str() {
            Some("on-risk") => ("on-risk", "on-risk", true),
            Some("always") => ("always", "always", false),
            Some("never") => ("never", "read-only", false),
            _ => bail!(
                "{origin}: tools.approval_policy (deprecated) must be \"on-risk\", \"always\", or \
                 \"never\"; set [policy] mode instead"
            ),
        };
        let policy = root
            .entry("policy")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(policy) = policy.as_table_mut() else {
            bail!("{origin}: [policy] must be a table");
        };
        if policy.contains_key("mode") {
            bail!(
                "{origin} sets both tools.approval_policy and policy.mode; keep only [policy] mode \
                 (tools.approval_policy is deprecated)"
            );
        }
        policy.insert("mode".into(), mode.into());
        found.push(Deprecated {
            key: "tools.approval_policy".into(),
            value: value.into(),
            replacement: "policy.mode".into(),
            mode: mode.into(),
            origin: origin.into(),
            pins_old_default: pins,
        });
    }
    let agents = root
        .get_mut("agents")
        .and_then(toml::Value::as_table_mut)
        .into_iter()
        .flat_map(|agents| agents.iter_mut());
    for (agent, table) in agents {
        let Some(table) = table.as_table_mut() else {
            continue;
        };
        let Some(old) = table.remove("permissions") else {
            continue;
        };
        let (value, mode, pins) = match old.as_str() {
            Some("full") => ("full", "auto", false),
            Some("default") => ("default", "on-risk", true),
            _ => bail!(
                "{origin}: agents.{agent}.permissions (deprecated) must be \"default\" or \"full\"; \
                 set [agents.{agent}] mode instead"
            ),
        };
        if table.contains_key("mode") {
            bail!(
                "{origin} sets both agents.{agent}.permissions and agents.{agent}.mode; keep only \
                 mode (permissions is deprecated)"
            );
        }
        table.insert("mode".into(), mode.into());
        found.push(Deprecated {
            key: format!("agents.{agent}.permissions"),
            value: value.into(),
            replacement: format!("agents.{agent}.mode"),
            mode: mode.into(),
            origin: origin.into(),
            pins_old_default: pins,
        });
    }
    Ok(found)
}
