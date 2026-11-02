//! Approving tool calls: the policy's decision ([`decide`]), requests the
//! client answers, questions only the owner may answer, and the unattended
//! answers background jobs get.
//!
//! A built-in command gate decides first: a `git push` that forces `main` is
//! denied, and a direct `cargo publish` asks the owner (the counted path is
//! `feature-flow`'s publish script). The gates match the exact command a
//! call runs ([`ApprovalRequest::command`]), so they catch the plain
//! spelling, not a script or `bash -c` that hides it: a guardrail against
//! mistakes, not a security boundary. After them the mode decides.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use scv_core::{AgentError, ApprovalGate, ApprovalRequest, ToolRisk};
use scv_protocol::ServerEvent;
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    config::PolicyMode,
    confirm::Confirmer,
    outbound::{OutboundSender, send_turn_event},
    session::{TurnMeta, next_seq},
};

#[derive(Default)]
pub(crate) struct ApprovalBroker {
    pub(crate) pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

impl ApprovalBroker {
    pub(crate) async fn insert(&self, id: String, sender: oneshot::Sender<bool>) {
        self.pending.lock().await.insert(id, sender);
    }

    pub(crate) async fn remove(&self, id: &str) {
        self.pending.lock().await.remove(id);
    }

    pub(crate) async fn resolve(&self, id: &str, approved: bool) -> bool {
        let sender = self.pending.lock().await.remove(id);
        sender.is_some_and(|sender| sender.send(approved).is_ok())
    }
}

/// The policy SCV's own tool calls and the requests relayed to it are
/// decided by: `[policy] mode`, under the built-in command gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Policy {
    pub(crate) mode: PolicyMode,
}

/// What the policy does with one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow,
    Deny,
    /// Today's question: the client's prompt, or for a background job the
    /// client's blanket answer, then the owner's workbench, then denial.
    AskClient,
    /// Only the owner's explicit yes allows it. No blanket answer counts:
    /// in the TUI it is the usual prompt, elsewhere a question in chat
    /// through [`Confirmer`], where no answer by the deadline is no.
    AskOwner,
}

/// Why `git push` forcing `main` is denied.
const FORCE_PUSH_MAIN: &str = "Force-pushing main is never allowed";
/// Why a direct `cargo publish` asks the owner.
const CARGO_PUBLISH: &str = "A direct cargo publish needs the owner's yes (feature-flow's publish \
     script is the path that counts crates.io's daily limit)";

/// The policy's decision for `request`: a matching command gate first, then
/// the mode.
pub(crate) fn decide(policy: Policy, request: &ApprovalRequest) -> Decision {
    if let Some((decision, _)) = gate(request) {
        return decision;
    }
    let read_only = request.risk == ToolRisk::ReadOnly;
    match policy.mode {
        PolicyMode::Auto => Decision::Allow,
        PolicyMode::OnRisk | PolicyMode::ReadOnly if read_only => Decision::Allow,
        PolicyMode::ReadOnly => Decision::Deny,
        PolicyMode::OnRisk | PolicyMode::Always => Decision::AskClient,
    }
}

/// The built-in command gate `request`'s command matches, with why.
fn gate(request: &ApprovalRequest) -> Option<(Decision, &'static str)> {
    let commands = simple_commands(request.command.as_deref()?);
    if commands.iter().any(|words| forces_main(words)) {
        return Some((Decision::Deny, FORCE_PUSH_MAIN));
    }
    commands
        .iter()
        .any(|words| cargo_publish(words))
        .then_some((Decision::AskOwner, CARGO_PUBLISH))
}

/// The words of each simple command in a shell command line, split at
/// `;`, `&`, `|`, parentheses, and line breaks, with quotes and backslashes
/// removed. No expansion: this reads the plain spelling only.
fn simple_commands(command: &str) -> Vec<Vec<String>> {
    let mut commands = vec![Vec::new()];
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = command.chars();
    let end_word = |commands: &mut Vec<Vec<String>>, word: &mut String, in_word: &mut bool| {
        if std::mem::take(in_word) {
            commands
                .last_mut()
                .expect("one command at least")
                .push(std::mem::take(word));
        }
    };
    while let Some(character) = chars.next() {
        match (quote, character) {
            (Some(open), _) if character == open => quote = None,
            (Some('"'), '\\') => word.extend(chars.next()),
            (Some(_), _) => word.push(character),
            (None, '\'' | '"') => {
                quote = Some(character);
                in_word = true;
            }
            (None, '\\') => {
                word.extend(chars.next());
                in_word = true;
            }
            (None, ';' | '&' | '|' | '(' | ')' | '\n') => {
                end_word(&mut commands, &mut word, &mut in_word);
                commands.push(Vec::new());
            }
            (None, _) if character.is_whitespace() => {
                end_word(&mut commands, &mut word, &mut in_word);
            }
            (None, _) => {
                word.push(character);
                in_word = true;
            }
        }
    }
    end_word(&mut commands, &mut word, &mut in_word);
    commands.retain(|words| !words.is_empty());
    commands
}

/// The arguments after the first word naming `program` (as a path's last
/// part too), so wrappers such as `env`, `sudo`, or `host.sh` before it do
/// not hide it.
fn arguments_of<'a>(words: &'a [String], program: &str) -> Option<&'a [String]> {
    let at = words
        .iter()
        .position(|word| word.rsplit('/').next() == Some(program))?;
    Some(&words[at + 1..])
}

/// Whether `words` is a `git push` that forces `main`: a force option or a
/// `+` refspec, with a refspec whose destination is `main`.
fn forces_main(words: &[String]) -> bool {
    let Some(arguments) = arguments_of(words, "git") else {
        return false;
    };
    // Git's own options before the subcommand, some with a value.
    let mut rest = arguments.iter();
    let subcommand = loop {
        match rest.next().map(String::as_str) {
            Some("-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace") => {
                rest.next();
            }
            Some(option) if option.starts_with('-') => {}
            Some(subcommand) => break subcommand,
            None => return false,
        }
    };
    if subcommand != "push" {
        return false;
    }
    let mut force = false;
    let mut positional = Vec::new();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "--" => {
                positional.extend(rest.by_ref());
                break;
            }
            "-f" | "--force" => force = true,
            option if option.starts_with("--force-with-lease") => force = true,
            "-o" | "--push-option" | "--repo" | "--receive-pack" | "--exec" => {
                rest.next();
            }
            option if option.starts_with("--") => {}
            option if option.starts_with('-') => {
                force |= option.contains('f');
                // `-o` takes a value, also at the end of a cluster.
                if option.ends_with('o') {
                    rest.next();
                }
            }
            _ => positional.push(argument),
        }
    }
    // The first is the remote; the rest are refspecs.
    positional.iter().skip(1).any(|refspec| {
        let (forced, refspec) = match refspec.strip_prefix('+') {
            Some(refspec) => (true, refspec),
            None => (force, refspec.as_str()),
        };
        let destination = refspec.rsplit(':').next().unwrap_or(refspec);
        forced && matches!(destination, "main" | "refs/heads/main")
    })
}

/// Whether `words` is a `cargo publish`, with or without a toolchain.
fn cargo_publish(words: &[String]) -> bool {
    let Some(arguments) = arguments_of(words, "cargo") else {
        return false;
    };
    let mut rest = arguments.iter();
    loop {
        match rest.next().map(String::as_str) {
            Some("-Z" | "--config" | "-C" | "--color") => {
                rest.next();
            }
            Some(option) if option.starts_with('-') || option.starts_with('+') => {}
            Some(subcommand) => return subcommand == "publish",
            None => return false,
        }
    }
}

/// The question a request only the owner may answer puts to the owner.
fn owner_question(request: &ApprovalRequest) -> String {
    let reason = gate(request).map_or("This needs the owner's yes", |(_, reason)| reason);
    let summary: String = request.summary.chars().take(3000).collect();
    format!("{reason}. An agent asks to run this:\n\n{summary}")
}

/// Puts a request only the owner may answer ([`Decision::AskOwner`]) to the
/// owner in chat through the [`Confirmer`], in the chat the session's work
/// came from or else the `[notify]` owner chat. A server without one (stdio)
/// cannot ask, and a question nobody asked is a no.
#[derive(Clone)]
pub(crate) struct OwnerQuestions {
    pub(crate) confirmer: Option<Arc<Confirmer>>,
    /// The daemon session whose work asks.
    pub(crate) session: String,
}

impl OwnerQuestions {
    /// The owner's answer to `request`: yes only on an explicit yes before
    /// the deadline.
    pub(crate) async fn ask(
        &self,
        request: &ApprovalRequest,
        cancellation: &CancellationToken,
    ) -> bool {
        let Some(confirmer) = &self.confirmer else {
            tracing::warn!(
                "{} needs the owner's yes, which this server cannot ask for; denied",
                request.name
            );
            return false;
        };
        confirmer
            .ask_owner(&owner_question(request), &self.session, cancellation)
            .await
    }
}

pub(crate) struct ProtocolApprovalGate {
    pub(crate) policy: Policy,
    pub(crate) broker: Arc<ApprovalBroker>,
    pub(crate) meta: TurnMeta,
    pub(crate) output: OutboundSender,
    /// The session's place in the workbench, which shows it waiting.
    pub(crate) workbench: Option<Arc<scv_tools::workbench::SessionHandle>>,
    /// The person at this client is the owner (the TUI), so a request only
    /// the owner may answer is its usual prompt.
    pub(crate) owner_at_client: bool,
    pub(crate) owner: OwnerQuestions,
}

/// Decides a background job's nested approval requests, which outlive the
/// turn that could carry them to the client. Each gets the answer the
/// session would give without asking a person: the policy's own decision,
/// else the client's declared blanket answer (`auto_approve`), else a denial.
/// It never grants more than the same request would get in the foreground.
/// A request only the owner may answer goes to the owner in chat, never to
/// the blanket answer.
pub(crate) struct UnattendedGate {
    pub(crate) policy: Policy,
    pub(crate) client_approves_all: bool,
    pub(crate) owner: OwnerQuestions,
}

#[async_trait]
impl ApprovalGate for UnattendedGate {
    async fn approve(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        Ok(match decide(self.policy, &request) {
            Decision::Allow => true,
            Decision::Deny => false,
            Decision::AskClient => self.client_approves_all,
            Decision::AskOwner => self.owner.ask(&request, &cancellation).await,
        })
    }
}

/// A background job's gate while the owner may answer from the workbench:
/// what the policy decides on its own stands, so only a request the
/// foreground would have put to a person reaches `attended`, which holds it
/// for the owner when the unattended answer would deny it. A request only
/// the owner may answer goes to the owner in chat instead, since the
/// unattended answer could be a blanket yes.
pub(crate) struct PolicyFirstGate {
    pub(crate) policy: Policy,
    pub(crate) attended: Arc<dyn ApprovalGate>,
    pub(crate) owner: OwnerQuestions,
}

#[async_trait]
impl ApprovalGate for PolicyFirstGate {
    async fn approve(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        match decide(self.policy, &request) {
            Decision::Allow => Ok(true),
            Decision::Deny => Ok(false),
            Decision::AskClient => self.attended.approve(request, cancellation).await,
            Decision::AskOwner => Ok(self.owner.ask(&request, &cancellation).await),
        }
    }
}

#[async_trait]
impl ApprovalGate for ProtocolApprovalGate {
    async fn approve(
        &self,
        mut request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> Result<bool, AgentError> {
        match decide(self.policy, &request) {
            Decision::Allow => return Ok(true),
            Decision::Deny => return Ok(false),
            Decision::AskClient => {}
            Decision::AskOwner if self.owner_at_client => {
                if let Some((_, reason)) = gate(&request) {
                    request.summary = format!("{reason}. {}", request.summary);
                }
            }
            Decision::AskOwner => return Ok(self.owner.ask(&request, &cancellation).await),
        }
        let approval_id = Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        self.broker.insert(approval_id.clone(), sender).await;
        let waiting = Waiting::new(self.workbench.as_ref(), &request.summary);
        let event = ServerEvent::ApprovalRequested {
            request_id: self.meta.request_id.clone(),
            session_id: self.meta.session_id.clone(),
            turn_id: self.meta.turn_id.clone(),
            seq: next_seq(&self.meta.seq),
            approval_id: approval_id.clone(),
            call_id: request.call_id,
            name: request.name,
            risk: request.risk.as_str().into(),
            cwd: request.cwd.display().to_string(),
            summary: request.summary,
        };
        if let Err(error) = send_turn_event(
            &self.output,
            event,
            self.meta.max_server_frame,
            &cancellation,
        )
        .await
        {
            self.broker.remove(&approval_id).await;
            return Err(error);
        }
        let answer = tokio::select! {
            result = receiver => result.map_err(|_| AgentError::Cancelled),
            () = cancellation.cancelled() => {
                self.broker.remove(&approval_id).await;
                Err(AgentError::Cancelled)
            }
        };
        drop(waiting);
        answer
    }
}

/// The session shown waiting for its client's approval while this lives.
struct Waiting<'a>(Option<&'a Arc<scv_tools::workbench::SessionHandle>>);

impl<'a> Waiting<'a> {
    fn new(
        registration: Option<&'a Arc<scv_tools::workbench::SessionHandle>>,
        summary: &str,
    ) -> Self {
        if let Some(registration) = registration {
            let summary: String = summary.chars().take(200).collect();
            registration.set_state(
                scv_protocol::AgentState::AwaitingInput,
                Some(format!("waiting for approval: {summary}")),
            );
        }
        Self(registration)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Some(registration) = self.0 {
            registration.set_state(scv_protocol::AgentState::Working, None);
        }
    }
}

#[cfg(test)]
mod tests;
