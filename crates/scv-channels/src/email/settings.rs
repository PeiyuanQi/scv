//! An email account's `mail` table, read strictly.
//!
//! The daemon's configuration keeps `[channels.email.<account>.mail]`
//! opaque, so a mistake here fails only that account: it is parsed and
//! checked when the account starts, and `scv config show` reports the same
//! error. Every limit has a default and bounds.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The longest standing instruction text, in bytes.
const MAX_INSTRUCTIONS_BYTES: usize = 4 * 1024;
/// Rules one account may have, and entries in one rule's list.
const MAX_RULES: usize = 64;
const MAX_RULE_ENTRIES: usize = 64;
/// Mail chats one account reports to, tried in order.
const MAX_ROUTES: usize = 4;

/// `[channels.email.<account>.mail]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct MailSettings {
    /// The folder watched, as UTF-8.
    pub(crate) mailbox: String,
    /// How often the mailbox is checked.
    pub(crate) poll_seconds: u64,
    /// The owner's standing instructions for triage, shown to the model.
    pub(crate) instructions: String,
    /// The model triage uses; empty uses the daemon's default model.
    pub(crate) triage_model: String,
    /// Whether the triage model may see a cleaned body (`false`: headers
    /// and attachment names only).
    pub(crate) send_body: bool,
    /// The most cleaned body text sent to the model, in KiB.
    pub(crate) max_body_kib: u64,
    /// The most bytes of one text part fetched, in KiB.
    pub(crate) max_fetch_kib: u64,
    /// Triage turns per rolling hour; beyond it mail is reported by header.
    pub(crate) max_triage_per_hour: u32,
    /// Model tokens per local day; 0 turns the model off.
    pub(crate) max_tokens_per_day: u64,
    /// Mail first seen more than this long after it arrived is counted,
    /// not triaged: after an outage or a mailbox reset.
    pub(crate) catchup_hours: u64,
    /// Count a message decided within the last week and listed again (the
    /// same stored message, as after a mailbox reset; see
    /// [`super::parse::identity`]) instead of reporting it again.
    pub(crate) dedupe_message_id: bool,
    /// The owner's rules, first match wins, before the built-in ones.
    pub(crate) rules: Vec<Rule>,
    pub(crate) notify: NotifySettings,
    pub(crate) retention: RetentionSettings,
}

impl Default for MailSettings {
    fn default() -> Self {
        Self {
            mailbox: "INBOX".into(),
            poll_seconds: 60,
            instructions: String::new(),
            triage_model: String::new(),
            send_body: true,
            max_body_kib: 8,
            max_fetch_kib: 64,
            max_triage_per_hour: 30,
            max_tokens_per_day: 150_000,
            catchup_hours: 24,
            dedupe_message_id: true,
            rules: Vec::new(),
            notify: NotifySettings::default(),
            retention: RetentionSettings::default(),
        }
    }
}

/// One deterministic rule: every condition it names must hold, and a rule
/// that names none matches every message.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rule {
    /// Sender addresses (`name@example.com`) or domains (`@example.com`,
    /// which also matches its subdomains), case-insensitively.
    #[serde(default)]
    pub(crate) from: Vec<String>,
    /// Text a `List-Id` header contains, case-insensitively.
    #[serde(default)]
    pub(crate) list_id: Vec<String>,
    /// Whether the message is bulk or automated (a list, `Precedence`,
    /// `Auto-Submitted`).
    #[serde(default)]
    pub(crate) bulk: Option<bool>,
    /// Whether the sender looks like a no-reply address.
    #[serde(default)]
    pub(crate) noreply: Option<bool>,
    pub(crate) action: RuleAction,
    /// Report the message as urgent, within the urgent budget.
    #[serde(default)]
    pub(crate) urgent: bool,
}

/// What a rule does with a message, from cheapest to dearest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuleAction {
    /// Count it in the next digest's header; no report, no model.
    Count,
    /// Report its headers; no model.
    Header,
    /// A model reads its headers and attachment names, not its body.
    TriageMeta,
    /// A model reads its headers and a cleaned, bounded body.
    Triage,
}

/// `[channels.email.<account>.mail.notify]`: when and where reports go.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct NotifySettings {
    /// Mail chats (`<channel>:<account>` with `purpose = "mail"`), tried in
    /// order. Required.
    pub(crate) route: Vec<String>,
    /// Send once no new item arrived for this long.
    pub(crate) settle_seconds: u64,
    /// Send once the oldest item waited this long.
    pub(crate) max_delay_seconds: u64,
    /// Send at this many items; also the most items one message holds.
    pub(crate) max_items: usize,
    /// The most bytes of one message, in KiB.
    pub(crate) max_message_kib: usize,
    /// Whether the model's `urgent` counts.
    pub(crate) urgent_by_model: bool,
    pub(crate) max_urgent_per_hour: usize,
    pub(crate) max_messages_per_hour: usize,
    pub(crate) max_messages_per_day: usize,
    pub(crate) max_responses_per_hour: usize,
    /// `HH:MM-HH:MM` local time, possibly across midnight; empty for none.
    pub(crate) quiet_hours: String,
    /// `+HH:MM` or `-HH:MM`: the offset for quiet hours, displayed times,
    /// and local days. Empty uses the host's time zone.
    pub(crate) utc_offset: String,
    pub(crate) quiet_urgent: QuietUrgent,
    pub(crate) skipped: Skipped,
    /// The most items waiting; beyond it the oldest collapse into a count.
    pub(crate) max_queue: usize,
    /// A message no mail chat could store in this time is given up.
    pub(crate) give_up_hours: u64,
}

impl Default for NotifySettings {
    fn default() -> Self {
        Self {
            route: Vec::new(),
            settle_seconds: 120,
            max_delay_seconds: 900,
            max_items: 10,
            max_message_kib: 12,
            urgent_by_model: true,
            max_urgent_per_hour: 4,
            max_messages_per_hour: 6,
            max_messages_per_day: 48,
            max_responses_per_hour: 60,
            quiet_hours: String::new(),
            utc_offset: String::new(),
            quiet_urgent: QuietUrgent::Deliver,
            skipped: Skipped::Count,
            max_queue: 256,
            give_up_hours: 72,
        }
    }
}

/// Whether urgent mail is sent during quiet hours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QuietUrgent {
    Deliver,
    Hold,
}

/// Whether digests say how much mail was counted without a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Skipped {
    Count,
    Off,
}

/// `[channels.email.<account>.mail.retention]`: local disk bounds.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RetentionSettings {
    /// The account's state file may not grow past this, in KiB.
    pub(crate) max_state_kib: u64,
    /// New reports are refused while the state disk has less free, in MiB.
    pub(crate) min_free_mib: u64,
    /// How often the janitor runs.
    pub(crate) sweep_minutes: u64,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        Self {
            max_state_kib: 2048,
            min_free_mib: 64,
            sweep_minutes: 60,
        }
    }
}

/// Quiet hours as minutes after local midnight, `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QuietHours {
    pub(crate) start: u32,
    pub(crate) end: u32,
}

impl MailSettings {
    /// The account's `mail` table, or the defaults when it has none (which
    /// then fail for lack of a route).
    pub(crate) fn parse(table: Option<&toml::Table>) -> Result<Self> {
        let table = table.cloned().unwrap_or_default();
        if table.contains_key("actions") {
            bail!(
                "mail.actions: saving drafts, sending, and moving mail are not available in \
                 this release; remove the table"
            );
        }
        let settings: Self = toml::Value::Table(table)
            .try_into()
            .map_err(|error: toml::de::Error| anyhow::anyhow!("mail: {}", error.message()))?;
        settings.validated()
    }

    fn validated(mut self) -> Result<Self> {
        let mailbox = self.mailbox.trim();
        if mailbox.is_empty() || mailbox.len() > 255 || mailbox.chars().any(char::is_control) {
            bail!("mail.mailbox must be a folder name without control characters");
        }
        self.mailbox = mailbox.to_owned();
        within("mail.poll_seconds", self.poll_seconds, 15, 3600)?;
        if self.instructions.len() > MAX_INSTRUCTIONS_BYTES {
            bail!("mail.instructions must be at most 4 KiB");
        }
        if self.triage_model.len() > 128 || self.triage_model.chars().any(char::is_control) {
            bail!("mail.triage_model must be a model name");
        }
        within("mail.max_body_kib", self.max_body_kib, 1, 64)?;
        within("mail.max_fetch_kib", self.max_fetch_kib, 16, 1024)?;
        within(
            "mail.max_triage_per_hour",
            u64::from(self.max_triage_per_hour),
            1,
            600,
        )?;
        within(
            "mail.max_tokens_per_day",
            self.max_tokens_per_day,
            0,
            10_000_000,
        )?;
        within("mail.catchup_hours", self.catchup_hours, 1, 168)?;
        if self.rules.len() > MAX_RULES {
            bail!("mail.rules may hold at most {MAX_RULES} rules");
        }
        for (index, rule) in self.rules.iter_mut().enumerate() {
            rule.validate()
                .with_context(|| format!("mail.rules[{}]", index + 1))?;
        }
        self.notify.validate()?;
        let retention = &self.retention;
        within(
            "mail.retention.max_state_kib",
            retention.max_state_kib,
            512,
            16_384,
        )?;
        within(
            "mail.retention.min_free_mib",
            retention.min_free_mib,
            16,
            4096,
        )?;
        within(
            "mail.retention.sweep_minutes",
            retention.sweep_minutes,
            5,
            1440,
        )?;
        // The queue's text must fit in half the state file.
        if (self.notify.max_queue as u64) * 3 > retention.max_state_kib {
            bail!(
                "mail.notify.max_queue × 1.5 KiB must fit in half of mail.retention.max_state_kib"
            );
        }
        Ok(self)
    }

    /// The most cleaned body text sent to the model, in bytes.
    pub(crate) fn max_body_bytes(&self) -> usize {
        usize::try_from(self.max_body_kib * 1024).unwrap_or(usize::MAX)
    }

    /// The most bytes of one text part fetched.
    pub(crate) fn max_fetch_bytes(&self) -> usize {
        usize::try_from(self.max_fetch_kib * 1024).unwrap_or(usize::MAX)
    }
}

impl Rule {
    fn validate(&mut self) -> Result<()> {
        for (name, entries) in [("from", &mut self.from), ("list_id", &mut self.list_id)] {
            if entries.len() > MAX_RULE_ENTRIES {
                bail!("{name} may hold at most {MAX_RULE_ENTRIES} entries");
            }
            for entry in entries.iter_mut() {
                let trimmed = entry.trim().to_lowercase();
                if trimmed.is_empty()
                    || trimmed.len() > 254
                    || trimmed.chars().any(char::is_control)
                {
                    bail!("{name} entries must be short non-empty text");
                }
                *entry = trimmed;
            }
        }
        if let Some(bad) = self.from.iter().find(|entry| {
            let Some((local, domain)) = entry.rsplit_once('@') else {
                return true;
            };
            domain.is_empty() || domain.contains(char::is_whitespace) || local.contains('@')
        }) {
            bail!("from entries must be addresses or @domains, not {bad:?}");
        }
        Ok(())
    }
}

impl NotifySettings {
    fn validate(&self) -> Result<()> {
        if self.route.is_empty() || self.route.len() > MAX_ROUTES {
            bail!(
                "mail.notify.route must name 1 to {MAX_ROUTES} mail chats, such as \
                 [\"feishu:mail\"]"
            );
        }
        for route in &self.route {
            let valid = route.split_once(':').is_some_and(|(channel, account)| {
                !channel.is_empty()
                    && channel != super::CHANNEL
                    && crate::state::validate_name(account).is_ok()
            });
            if !valid {
                bail!(
                    "mail.notify.route entries must be chat accounts like \"feishu:mail\", not \
                     {route:?}"
                );
            }
        }
        within("mail.notify.settle_seconds", self.settle_seconds, 0, 3600)?;
        within(
            "mail.notify.max_delay_seconds",
            self.max_delay_seconds,
            self.settle_seconds.max(1),
            86_400,
        )?;
        within("mail.notify.max_items", self.max_items as u64, 1, 50)?;
        within(
            "mail.notify.max_message_kib",
            self.max_message_kib as u64,
            4,
            15,
        )?;
        within(
            "mail.notify.max_urgent_per_hour",
            self.max_urgent_per_hour as u64,
            0,
            60,
        )?;
        within(
            "mail.notify.max_messages_per_hour",
            self.max_messages_per_hour as u64,
            1,
            60,
        )?;
        within(
            "mail.notify.max_messages_per_day",
            self.max_messages_per_day as u64,
            1,
            500,
        )?;
        within(
            "mail.notify.max_responses_per_hour",
            self.max_responses_per_hour as u64,
            1,
            600,
        )?;
        self.quiet()?;
        self.fixed_offset()?;
        within("mail.notify.max_queue", self.max_queue as u64, 16, 1024)?;
        within("mail.notify.give_up_hours", self.give_up_hours, 1, 168)?;
        Ok(())
    }

    /// Quiet hours, when set.
    pub(crate) fn quiet(&self) -> Result<Option<QuietHours>> {
        let text = self.quiet_hours.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let parsed = text.split_once('-').and_then(|(start, end)| {
            let start = clock_minutes(start.trim())?;
            let end = clock_minutes(end.trim())?;
            (start != end).then_some(QuietHours { start, end })
        });
        parsed
            .map(Some)
            .context("mail.notify.quiet_hours must look like \"23:00-07:30\" (start ≠ end)")
    }

    /// The fixed offset in seconds east of UTC, when one is set.
    pub(crate) fn fixed_offset(&self) -> Result<Option<i32>> {
        let text = self.utc_offset.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let parsed = (|| {
            let (sign, rest) = match text.as_bytes().first()? {
                b'+' => (1, &text[1..]),
                b'-' => (-1, &text[1..]),
                _ => return None,
            };
            let (hours, minutes) = rest.split_once(':')?;
            if hours.len() != 2 || minutes.len() != 2 {
                return None;
            }
            let hours: i32 = hours.parse().ok()?;
            let minutes: i32 = minutes.parse().ok()?;
            (hours <= 14 && minutes < 60 && hours * 60 + minutes <= 14 * 60)
                .then_some(sign * (hours * 3600 + minutes * 60))
        })();
        parsed
            .map(Some)
            .context("mail.notify.utc_offset must look like \"+08:00\", between -14:00 and +14:00")
    }
}

/// `HH:MM` as minutes after midnight.
fn clock_minutes(text: &str) -> Option<u32> {
    let (hours, minutes) = text.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let hours: u32 = hours.parse().ok()?;
    let minutes: u32 = minutes.parse().ok()?;
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

fn within(name: &str, value: u64, min: u64, max: u64) -> Result<()> {
    if !(min..=max).contains(&value) {
        bail!("{name} must be between {min} and {max}");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
