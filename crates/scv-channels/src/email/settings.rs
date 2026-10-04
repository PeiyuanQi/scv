//! An email account's `mail` table, read strictly.
//!
//! The daemon's configuration keeps `[channels.email.<account>.mail]`
//! opaque, so a mistake here fails only that account: it is parsed and
//! checked when the account starts, and `scv config show` reports the same
//! error. Every limit has a default and bounds.
//!
//! Without a `mail.actions` table the account only reads. With one, each
//! kind of action is `"off"` until the owner sets it to `"approve"`, the
//! only other value: no setting carries an action out by itself.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The longest standing instruction text, in bytes.
const MAX_INSTRUCTIONS_BYTES: usize = 4 * 1024;
/// Rules one account may have, and entries in one rule's list.
const MAX_RULES: usize = 64;
const MAX_RULE_ENTRIES: usize = 64;
/// Mail chats one account reports to, tried in order.
const MAX_ROUTES: usize = 4;
/// Domains `mail.actions.recipient_domains` may list.
const MAX_DOMAINS: usize = 64;
/// The most recipients one outgoing message may have, `to` and `cc` together.
pub(crate) const MAX_RECIPIENTS: usize = 10;
/// The largest preview in the state file, in KiB, for the state budget.
const PREVIEW_KIB: u64 = 12;

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
    /// What the owner may have SCV do with mail, each kind only after the
    /// owner approves it. Absent, the account only reads.
    pub(crate) actions: Option<ActionSettings>,
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
            actions: None,
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
    /// How long a finished action's code is remembered, so a late command
    /// is answered and no code is used twice.
    pub(crate) tombstone_days: u64,
    /// How long an action SCV lost track of is kept for a later check.
    pub(crate) unknown_keep_days: u64,
    /// The pending actions' content may not grow past this, in MiB.
    pub(crate) max_actions_mib: u64,
    /// How long the action audit log keeps a line.
    pub(crate) audit_days: u64,
    /// The action audit log may not grow past this, in KiB.
    pub(crate) max_audit_kib: u64,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        Self {
            max_state_kib: 2048,
            min_free_mib: 64,
            sweep_minutes: 60,
            tombstone_days: 30,
            unknown_keep_days: 3,
            max_actions_mib: 8,
            audit_days: 90,
            max_audit_kib: 4096,
        }
    }
}

/// Whether one kind of action may happen: never, or once the owner
/// approves that very action. There is no third value; serde refuses any
/// other, so no setting carries an action out unapproved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionMode {
    #[default]
    Off,
    Approve,
}

/// A move triage may suggest by itself, for the owner to approve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Suggestion {
    Archive,
    Trash,
    Spam,
}

/// Where a reply goes when the original names a `Reply-To`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReplyTo {
    /// To the `Reply-To` address, as mail clients do (the preview points
    /// it out when it differs from the sender).
    #[default]
    Honor,
    /// Always to the sender.
    Ignore,
}

/// How a sent message reaches the Sent folder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SentCopy {
    /// The provider files it (the Gmail API and Graph always do; some SMTP
    /// servers do).
    #[default]
    Provider,
    /// SCV appends a copy to the Sent folder over IMAP after sending.
    Append,
}

/// `[channels.email.<account>.mail.actions]`: what SCV may do with mail
/// once the owner approves each action, and the limits around it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ActionSettings {
    /// Save a reply, a forward, or new mail in the Drafts folder.
    pub(crate) draft: ActionMode,
    /// Send a reply, a forward, or new mail.
    pub(crate) send: ActionMode,
    /// Forward a reported message (as a draft or sent, which `draft` and
    /// `send` gate too).
    pub(crate) forward: ActionMode,
    /// Move a message out of the inbox to the archive.
    pub(crate) archive: ActionMode,
    /// Mark a message read.
    pub(crate) mark_read: ActionMode,
    /// Move a message to Trash.
    pub(crate) trash: ActionMode,
    /// Move a message to Spam (Junk).
    pub(crate) spam: ActionMode,
    /// Moves triage may suggest on its own, each only when its kind is on.
    pub(crate) propose: Vec<Suggestion>,
    /// The owner's standing guidance for replies and new mail, ≤ 4 KiB.
    pub(crate) reply_instructions: String,
    /// The model that drafts replies and new mail; empty uses the daemon's
    /// default model.
    pub(crate) compose_model: String,
    /// Drafting turns per local day.
    pub(crate) max_compose_per_day: u64,
    pub(crate) reply_to: ReplyTo,
    /// Replies also go to the original's other recipients.
    pub(crate) reply_all: bool,
    /// When not empty, every recipient must be in one of these domains
    /// or their subdomains.
    pub(crate) recipient_domains: Vec<String>,
    /// Recipients of one message, `to` and `cc` together.
    pub(crate) max_recipients: usize,
    /// How long an approval code works once it reached the owner.
    pub(crate) approval_hours: u64,
    /// How long an action may wait for its approval at all.
    pub(crate) max_pending_hours: u64,
    /// How long after its approval an action may still start.
    pub(crate) execute_minutes: u64,
    /// Actions waiting or under way at once.
    pub(crate) max_open: usize,
    /// Approvals per rolling day, by what they do.
    pub(crate) max_sends_per_day: u64,
    pub(crate) max_drafts_per_day: u64,
    pub(crate) max_moves_per_day: u64,
    pub(crate) max_flags_per_day: u64,
    /// Folder names for providers that do not mark them (IMAP without
    /// SPECIAL-USE or XLIST), as UTF-8; empty finds them by their marks.
    pub(crate) drafts_folder: String,
    pub(crate) sent_folder: String,
    pub(crate) trash_folder: String,
    pub(crate) spam_folder: String,
    pub(crate) archive_folder: String,
    pub(crate) sent_copy: SentCopy,
    /// The display name on outgoing mail; empty for the address alone.
    pub(crate) from_name: String,
    /// How long a report's `#` handle works in commands.
    pub(crate) handle_days: u64,
}

impl Default for ActionSettings {
    fn default() -> Self {
        Self {
            draft: ActionMode::Off,
            send: ActionMode::Off,
            forward: ActionMode::Off,
            archive: ActionMode::Off,
            mark_read: ActionMode::Off,
            trash: ActionMode::Off,
            spam: ActionMode::Off,
            propose: Vec::new(),
            reply_instructions: String::new(),
            compose_model: String::new(),
            max_compose_per_day: 20,
            reply_to: ReplyTo::Honor,
            reply_all: false,
            recipient_domains: Vec::new(),
            max_recipients: MAX_RECIPIENTS,
            approval_hours: 24,
            max_pending_hours: 72,
            execute_minutes: 15,
            max_open: 32,
            max_sends_per_day: 20,
            max_drafts_per_day: 50,
            max_moves_per_day: 100,
            max_flags_per_day: 200,
            drafts_folder: String::new(),
            sent_folder: String::new(),
            trash_folder: String::new(),
            spam_folder: String::new(),
            archive_folder: String::new(),
            sent_copy: SentCopy::Provider,
            from_name: String::new(),
            handle_days: 14,
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
        within(
            "mail.retention.tombstone_days",
            retention.tombstone_days,
            7,
            365,
        )?;
        within(
            "mail.retention.unknown_keep_days",
            retention.unknown_keep_days,
            1,
            30,
        )?;
        within(
            "mail.retention.max_actions_mib",
            retention.max_actions_mib,
            1,
            256,
        )?;
        within("mail.retention.audit_days", retention.audit_days, 7, 730)?;
        within(
            "mail.retention.max_audit_kib",
            retention.max_audit_kib,
            64,
            65_536,
        )?;
        // The queue's text must fit in half the state file.
        if (self.notify.max_queue as u64) * 3 > retention.max_state_kib {
            bail!(
                "mail.notify.max_queue × 1.5 KiB must fit in half of mail.retention.max_state_kib"
            );
        }
        if let Some(actions) = &mut self.actions {
            actions.validate()?;
            // Previews wait in the queue too: all of them at once must still
            // leave a quarter of the state file free.
            let needed =
                (self.notify.max_queue as u64) * 3 / 2 + actions.max_open as u64 * PREVIEW_KIB;
            if needed * 4 > retention.max_state_kib * 3 {
                bail!(
                    "mail.notify.max_queue × 1.5 KiB and mail.actions.max_open × 12 KiB must fit \
                     in three quarters of mail.retention.max_state_kib"
                );
            }
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

impl ActionSettings {
    fn validate(&mut self) -> Result<()> {
        if self.reply_instructions.len() > MAX_INSTRUCTIONS_BYTES {
            bail!("mail.actions.reply_instructions must be at most 4 KiB");
        }
        if self.compose_model.len() > 128 || self.compose_model.chars().any(char::is_control) {
            bail!("mail.actions.compose_model must be a model name");
        }
        within(
            "mail.actions.max_compose_per_day",
            self.max_compose_per_day,
            0,
            200,
        )?;
        if self.recipient_domains.len() > MAX_DOMAINS {
            bail!("mail.actions.recipient_domains may hold at most {MAX_DOMAINS} domains");
        }
        for domain in &mut self.recipient_domains {
            let lowered = domain.trim().trim_start_matches('@').to_ascii_lowercase();
            if !valid_domain(&lowered) {
                bail!("mail.actions.recipient_domains entries must be domains, not {domain:?}");
            }
            *domain = lowered;
        }
        within(
            "mail.actions.max_recipients",
            self.max_recipients as u64,
            1,
            MAX_RECIPIENTS as u64,
        )?;
        within("mail.actions.approval_hours", self.approval_hours, 1, 168)?;
        within(
            "mail.actions.max_pending_hours",
            self.max_pending_hours,
            self.approval_hours,
            336,
        )?;
        within("mail.actions.execute_minutes", self.execute_minutes, 1, 120)?;
        within("mail.actions.max_open", self.max_open as u64, 1, 128)?;
        within(
            "mail.actions.max_sends_per_day",
            self.max_sends_per_day,
            0,
            500,
        )?;
        within(
            "mail.actions.max_drafts_per_day",
            self.max_drafts_per_day,
            0,
            1000,
        )?;
        within(
            "mail.actions.max_moves_per_day",
            self.max_moves_per_day,
            0,
            1000,
        )?;
        within(
            "mail.actions.max_flags_per_day",
            self.max_flags_per_day,
            0,
            1000,
        )?;
        within("mail.actions.handle_days", self.handle_days, 1, 60)?;
        for (name, folder) in [
            ("drafts_folder", &mut self.drafts_folder),
            ("sent_folder", &mut self.sent_folder),
            ("trash_folder", &mut self.trash_folder),
            ("spam_folder", &mut self.spam_folder),
            ("archive_folder", &mut self.archive_folder),
        ] {
            let trimmed = folder.trim().to_owned();
            if trimmed.len() > 255 || trimmed.chars().any(char::is_control) {
                bail!("mail.actions.{name} must be a folder name without control characters");
            }
            *folder = trimmed;
        }
        let name = self.from_name.trim().to_owned();
        if name.chars().count() > 64
            || name
                .chars()
                .any(|c| c.is_control() || matches!(c, '"' | '<' | '>' | '\\'))
        {
            bail!(
                "mail.actions.from_name must be one line of at most 64 characters without \
                 quotes or angle brackets"
            );
        }
        self.from_name = name;
        let mut seen = Vec::new();
        for suggestion in &self.propose {
            if seen.contains(suggestion) {
                bail!("mail.actions.propose names a move twice");
            }
            seen.push(*suggestion);
            let (name, mode) = match suggestion {
                Suggestion::Archive => ("archive", self.archive),
                Suggestion::Trash => ("trash", self.trash),
                Suggestion::Spam => ("spam", self.spam),
            };
            if mode != ActionMode::Approve {
                bail!(
                    "mail.actions.propose names {name}, but mail.actions.{name} is \"off\"; set \
                     it to \"approve\" or leave it out of propose"
                );
            }
        }
        Ok(())
    }

    /// Whether any kind of action may happen at all.
    pub(crate) fn any(&self) -> bool {
        [
            self.draft,
            self.send,
            self.archive,
            self.mark_read,
            self.trash,
            self.spam,
        ]
        .contains(&ActionMode::Approve)
    }

    /// Whether the mailbox itself may change (drafts saved, messages moved
    /// or marked), which needs a credential that can write.
    pub(crate) fn changes_mailbox(&self) -> bool {
        [
            self.draft,
            self.archive,
            self.mark_read,
            self.trash,
            self.spam,
        ]
        .contains(&ActionMode::Approve)
    }
}

/// A lowercase ASCII domain of at least two labels of letters, digits, and
/// inner hyphens.
pub(crate) fn valid_domain(domain: &str) -> bool {
    let labels: Vec<&str> = domain.split('.').collect();
    domain.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
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
