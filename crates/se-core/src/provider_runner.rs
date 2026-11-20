//! The runner that drives a real model provider.
//!
//! This is the Phase 2 substitution the seam was drawn for: it replaces
//! [`EchoRunner`](crate::EchoRunner) and no interface code changes,
//! because an interface only ever sees [`CoreEvent`](se_protocol::CoreEvent)s.
//!
//! What it adds beyond forwarding deltas:
//!
//! * **Time to first token is measured separately** from generation. The
//!   blueprint is explicit that a remote model must never be benchmarked
//!   by timing only the final response, and this is where that number
//!   actually comes from.
//! * **Cancellation reaches the socket.** The provider polls the same
//!   flag between frames, so interrupting stops the connection rather
//!   than politely waiting for a paid-for answer nobody will read.
//! * **Usage is forwarded only if the provider sent it.** Nothing is
//!   estimated; a provider that reports nothing produces
//!   `UsageSource::Unavailable`, which an interface shows as "not
//!   reported".

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use se_permission::{
    Approved, ExecutionContext, Gateway, NonceLedger, NullAuditSink, Request as PermRequest,
    Ruleset,
};
use se_protocol::{Outcome, Stage, Usage, UsageSource, UserTurn};
use se_provider::{
    ChatRequest, Message, ProviderError, Registry, Role, StreamEvent, TimeoutKind, ToolCallRecord,
};

use crate::sink::TurnSink;
use crate::tool_loop::{self, ToolAction};
use crate::tools::Granted;
use crate::Runner;

/// `agent_definition_hash` identifies the agent body a token was
/// approved against (`SE_TARGET_ARCHITECTURE.md` §3a). There is no
/// agent/specialist system yet (Phase 3/4, explicitly out of scope
/// here) — `ProviderRunner` itself is the only thing issuing tool
/// calls, so its own identity stands in for an agent definition until
/// one exists. A real multi-agent catalogue entry, not a provider
/// runner, will own this once it exists.
const NATIVE_LOOP_AGENT_ID: &str = "native-tool-loop";
const NATIVE_LOOP_AGENT_DEFINITION_HASH: &str = "native-tool-loop-v1";
/// Who a scheduled turn acts as while its specialist is being picked.
const AUTO_ROUTE_AGENT_ID: &str = "auto-router";
/// With auto-routing on, every turn runs as a catalogue specialist: this
/// one takes any turn the router cannot place (its own `activation` is
/// `always`), so no turn ever runs as a bare model with no SE role.
pub const FALLBACK_AGENT_ID: &str = "chief-orchestrator";

/// Hard ceiling on how many model/tool round trips one turn may make,
/// independent of the turn deadline: the deadline bounds wall time, but
/// a provider replying instantly with another tool call every time
/// would otherwise loop forever within that same budget.
const MAX_TOOL_ROUNDS: usize = 20;

/// How many tokens to allow in a reply, until this is configurable.
const MAX_TOKENS: u32 = 4096;

/// Default absolute bound on one turn, independent of the provider's
/// own idle-read timeout.
///
/// The two bounds answer different questions: the provider's idle
/// timeout detects a dead connection; this detects a connection that
/// stays alive and keeps sending *something* far longer than a turn
/// should reasonably take. Without it, a provider that trickles one
/// byte every few seconds forever would never time out at all.
const DEFAULT_TURN_DEADLINE: Duration = Duration::from_secs(600);

/// Instructions sent with every request.
///
/// Deliberately tiny. The blueprint's fast path is explicit that an
/// ordinary message must not drag in a specialist catalogue, a
/// repository tree or every tool description -- that is how a greeting
/// comes to cost ninety seconds. Phase 5 adds specialist instructions,
/// loaded only when a specialist is actually dispatched.
const SYSTEM_PROMPT: &str = "You are SE, an engineering assistant. Be concise and concrete.";

pub struct ProviderRunner {
    registry: Registry,
    provider_id: String,
    model: String,
    turn_deadline: Duration,
    /// The policy a tool call is checked against before a human is
    /// ever asked. Defaults to an empty [`Ruleset`], which (per that
    /// type's own documented default) resolves every call to `Ask`,
    /// never to a silent `Allow` — an unconfigured gateway must not
    /// grant anything on its own.
    ruleset: Ruleset,
    /// Single-use nonces for every token this runner mints, across the
    /// whole turn (and, since `Runner::run` takes `&self` and one
    /// runner is shared across task workers, across every turn it ever
    /// runs). `NonceLedger`'s own interior `Mutex` is what makes this
    /// safe to share.
    ledger: NonceLedger,
    /// Where to find the agent catalogue for a specialist-routed turn.
    /// `None` is honest for a runner no caller gave one to -- such a
    /// runner can still run ordinary turns; it just fails a
    /// specialist-routed one with a clear message rather than guessing
    /// a path. Resolved by the caller (`crates/se/src/main.rs` already
    /// owns the one true `catalogue_root()` convention for the `se
    /// agents` CLI command), never by this crate -- the same reasoning
    /// `session_store::default_sessions_dir` already follows by taking
    /// its base directory as an explicit parameter.
    catalogue_root: Option<std::path::PathBuf>,
    /// Every tool path is confined under this directory (row 2.10).
    /// Defaults to the process's working directory at construction.
    workspace_root: std::path::PathBuf,
    /// Where every gateway decision is recorded. Null unless a caller
    /// supplies a durable one (`crate::audit_log::FileAuditSink`).
    audit: std::sync::Arc<dyn se_permission::AuditSink + Send + Sync>,
    /// Pick a specialist automatically when a turn names none.
    auto_route: bool,
    /// Set by a team run: file claims and the token budget, keyed by the
    /// node a turn executes (`UserTurn::team_node`).
    team: Option<crate::team::TeamContext>,
    /// Summarize older history once it passes this many (estimated)
    /// tokens; `None` never compacts. See [`compaction_split`].
    compact_at_tokens: Option<u32>,
    /// The specialist the conversation is with. Follow-up turns keep it
    /// instead of being re-routed on their own words alone (which picked
    /// odd roles for "what's the codename?" and cost two calls a turn);
    /// naming another with `/<id>` switches it. Interactive turns only.
    sticky_specialist: Sticky,
    /// Manual, auto or plan; read at every tool call so a switch takes effect mid-turn.
    mode: std::sync::Mutex<se_protocol::WorkMode>,
    /// A cheap model that picks the specialist (`SE_ROUTER_MODEL`). Without one, an
    /// agent-CLI turn is not routed at all: keyword guesses picked wrong roles.
    router_model: Option<se_protocol::ModelRef>,
    /// Roles that run on their own model (`~/.se/models.json` "roles"), e.g. the
    /// chief and the architects on the strongest model the user has.
    role_models: std::collections::HashMap<String, se_protocol::ModelRef>,
}

/// The conversation's specialist, saved beside the conversation (when a
/// file is given) so `se --resume` comes back to the same one.
#[derive(Default)]
struct Sticky {
    id: std::sync::Mutex<Option<String>>,
    file: Option<std::path::PathBuf>,
}

impl Sticky {
    fn get(&self) -> Option<String> {
        self.id.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Every change goes through here, so the saved copy never lags.
    /// The cleared state is saved as an empty file, not a deletion.
    fn set(&self, id: Option<String>) {
        let mut guard = self.id.lock().unwrap_or_else(|e| e.into_inner());
        if *guard == id {
            return;
        }
        *guard = id;
        if let Some(path) = &self.file {
            // shortcut: a failed save only loses the choice on resume.
            let _ = std::fs::write(path, guard.as_deref().unwrap_or(""));
        }
    }
}

impl ProviderRunner {
    /// Fails fast if the provider is unknown or unconfigured, so a turn
    /// is never started against something that cannot run it.
    pub fn new(provider_id: &str, model: &str) -> Result<Self, ProviderError> {
        let registry = Registry::new();
        let provider = registry.get(provider_id).ok_or_else(|| {
            ProviderError::NotConfigured(format!("`{provider_id}` is not a known provider"))
        })?;
        let status = provider.detect();
        if !status.configured {
            return Err(ProviderError::NotConfigured(format!(
                "`{provider_id}` is not configured: {}",
                status.detail
            )));
        }
        Self::with_default(registry, provider_id, model)
    }

    /// A runner with no default model yet: every turn needs one chosen in
    /// the interface (`/models`), and a turn without one fails saying so.
    /// Used when several providers are configured and none was named, so
    /// SE talks to a real model instead of falling back to the local echo.
    pub fn unselected() -> Result<Self, ProviderError> {
        Self::with_default(Registry::new(), "", "")
    }

    fn with_default(
        registry: Registry,
        provider_id: &str,
        model: &str,
    ) -> Result<Self, ProviderError> {
        Ok(ProviderRunner {
            registry,
            provider_id: provider_id.to_string(),
            model: model.to_string(),
            turn_deadline: DEFAULT_TURN_DEADLINE,
            ruleset: Ruleset::default(),
            ledger: NonceLedger::new(),
            catalogue_root: None,
            workspace_root: std::env::current_dir()
                .map_err(|e| ProviderError::NotConfigured(format!("no working directory: {e}")))?,
            audit: std::sync::Arc::new(NullAuditSink),
            auto_route: false,
            compact_at_tokens: None,
            sticky_specialist: Default::default(),
            mode: Default::default(),
            router_model: None,
            role_models: Default::default(),
            team: None,
        })
    }

    /// Automatically picks one of the catalogue's specialists for any
    /// turn that names none. Needs a catalogue root to have any effect.
    /// Compacts the conversation once its history passes `tokens`
    /// (estimated as chars/4): older exchanges are summarized once by the
    /// turn's own model and the summary replaces them in the shared,
    /// saved transcript. Later turns, a resumed session and a switched
    /// provider then send the summary, not the whole history.
    pub fn with_compaction(mut self, tokens: Option<u32>) -> Self {
        self.compact_at_tokens = tokens.filter(|t| *t > 0);
        self
    }

    /// Saves the conversation's specialist to `path` and starts from
    /// whatever it holds (missing or empty: no specialist).
    pub fn with_specialist_file(mut self, path: std::path::PathBuf) -> Self {
        let saved = std::fs::read_to_string(&path).unwrap_or_default();
        let saved = saved.trim();
        self.sticky_specialist = Sticky {
            id: std::sync::Mutex::new((!saved.is_empty()).then(|| saved.to_string())),
            file: Some(path),
        };
        self
    }

    pub fn with_auto_route(mut self, on: bool) -> Self {
        self.auto_route = on;
        self
    }

    /// Routes specialist choice through this model instead of the turn's own.
    pub fn with_router_model(mut self, model: Option<se_protocol::ModelRef>) -> Self {
        self.router_model = model;
        self
    }

    /// Gives these roles their own model, by role id.
    pub fn with_role_models(
        mut self,
        models: std::collections::HashMap<String, se_protocol::ModelRef>,
    ) -> Self {
        self.role_models = models;
        self
    }

    /// Records every gateway decision to `audit` (row 2.5).
    pub fn with_audit_sink(
        mut self,
        audit: std::sync::Arc<dyn se_permission::AuditSink + Send + Sync>,
    ) -> Self {
        self.audit = audit;
        self
    }

    /// Confines every tool path to `root` instead of the working directory.
    pub fn with_workspace_root(mut self, root: std::path::PathBuf) -> Self {
        self.workspace_root = root;
        self
    }

    /// Overrides the whole-turn deadline. Rejects a non-positive value
    /// at construction time rather than racing against an already-
    /// expired deadline on the first poll.
    pub fn with_turn_deadline(mut self, deadline: Duration) -> Result<Self, ProviderError> {
        if deadline.is_zero() {
            return Err(ProviderError::NotConfigured(
                "a turn deadline must be greater than zero".to_string(),
            ));
        }
        self.turn_deadline = deadline;
        Ok(self)
    }

    /// Supplies the policy tool calls are checked against. Not required:
    /// the default empty ruleset asks a human for everything, which is
    /// the safe starting point this crate does not relax on its own.
    pub fn with_ruleset(mut self, ruleset: Ruleset) -> Self {
        self.ruleset = ruleset;
        self
    }

    /// Supplies the catalogue root a specialist-routed turn resolves
    /// against. Without this, `Runner::run` fails any turn that names
    /// a `specialist_id` -- clearly, not by guessing a default path.
    pub fn with_catalogue_root(mut self, root: std::path::PathBuf) -> Self {
        self.catalogue_root = Some(root);
        self
    }

    fn tool_definitions(&self) -> Vec<se_provider::ToolDefinition> {
        tool_loop::tool_definitions()
    }
}

/// How a fired whole-turn deadline and a possible cancellation resolve
/// against each other, as a decision over three booleans with nothing
/// else in scope.
///
/// A fired deadline and a real user cancellation reach `stream()` as
/// the same closure returning `true` (a provider has one cancellation
/// mechanism, not two), so they are indistinguishable from inside the
/// call. `cancelled` is the tie-breaker precisely because user
/// cancellation must always win over a deadline that happened to fire
/// in the same instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeadlinePrecedence {
    /// The deadline did not fire, or the user also cancelled and that
    /// takes precedence -- the caller's ordinary `Err`/`Ok` handling
    /// applies.
    NotApplicable,
    Failed,
    Incomplete,
}

fn deadline_precedence(
    deadline_fired: bool,
    cancelled: bool,
    answer_is_empty: bool,
) -> DeadlinePrecedence {
    if !deadline_fired || cancelled {
        return DeadlinePrecedence::NotApplicable;
    }
    if answer_is_empty {
        DeadlinePrecedence::Failed
    } else {
        DeadlinePrecedence::Incomplete
    }
}

/// The newest suffix of `history` that fits `budget_tokens` alongside
/// `fixed_chars` of system prompt and request, which are never dropped.
/// Whole messages only, oldest first, so no message is ever cut mid-way.
///
/// shortcut: tokens are estimated as chars / 4 and only prior history is
/// trimmed, not tool results growing within a turn. Use the provider's
/// tokenizer and trim mid-turn when a real overflow is observed.
fn fit_history(history: &[Message], budget_tokens: Option<u32>, fixed_chars: usize) -> &[Message] {
    let Some(budget) = budget_tokens else {
        return history;
    };
    let size = |m: &Message| {
        m.content.len()
            + m.tool_calls
                .iter()
                .map(|c| c.name.len() + c.arguments.len())
                .sum::<usize>()
    };
    let mut remaining = (budget as usize * 4).saturating_sub(fixed_chars);
    let mut start = history.len();
    while start > 0 && size(&history[start - 1]) <= remaining {
        remaining -= size(&history[start - 1]);
        start -= 1;
    }
    // Never open on an answer whose question was trimmed away.
    while start < history.len() && history[start].role != Role::User {
        start += 1;
    }
    &history[start..]
}

/// Retries of one model call after a temporary provider failure.
const MAX_RETRIES: u32 = 2;

/// How long to wait before retry `attempt` (1-based): the provider's own
/// `retry_after` when it gave one (capped at 30s), else 1s then 3s.
fn retry_wait(error: &ProviderError, attempt: u32) -> Duration {
    match error {
        ProviderError::RateLimited {
            retry_after: Some(d),
            ..
        } => (*d).min(Duration::from_secs(30)),
        _ => Duration::from_secs(3u64.pow(attempt.saturating_sub(1))),
    }
}

/// Messages kept verbatim after a compaction: the last two exchanges.
const COMPACT_KEEP: usize = 4;

/// Opens the summary message, so it reads as memory, not as a request.
pub const MEMORY_PREFIX: &str = "[SE memory] Summary of our conversation so far:
";

const COMPACT_SYSTEM: &str = "You compress a conversation into a memory note that another model will continue from. Keep the user's goals, decisions made and why, facts, names, file paths, code identifiers, open questions and next steps. Drop pleasantries and repetition. Plain text, at most 300 words.";

/// Where to cut `history` for a compaction: everything before the
/// returned index is summarized. `None` while the history is under
/// `at_tokens` (chars/4), or when the older part is too small to be worth
/// a model call (so oversized recent turns cannot cause a summary on
/// every turn). The cut always lands on a user message.
fn compaction_split(history: &[Message], at_tokens: u32) -> Option<usize> {
    let tokens = |ms: &[Message]| ms.iter().map(|m| m.content.len()).sum::<usize>() / 4;
    if tokens(history) <= at_tokens as usize || history.len() <= COMPACT_KEEP {
        return None;
    }
    let mut split = history.len() - COMPACT_KEEP;
    while split > 0 && history[split].role != Role::User {
        split -= 1;
    }
    (split > 0 && tokens(&history[..split]) * 4 > at_tokens as usize).then_some(split)
}

/// Catalogue `tools:` names (`read-files`, `run-shell`, ...) as the SE
/// tool names the gateway checks (`read`, `shell`, ...). The one place
/// that translation lives. Catalogue capabilities SE has no tool for are
/// dropped; `run-tests` is deliberately among them -- mapping it to
/// `shell` would hand arbitrary shell to roles that declare only tests.
/// SE's own names pass through so a definition may use either.
pub(crate) fn se_tools_for(declared: &[String]) -> Vec<String> {
    const KNOWN_UNMAPPED: &[&str] = &[
        "escalate",
        "memory-read",
        "memory-write",
        "task-graph",
        "invoke-skill",
        "web-fetch",
        "web-search",
        "run-tests",
        "edit-notebook",
    ];
    static WARNED: std::sync::Once = std::sync::Once::new();
    let mut out: Vec<String> = Vec::new();
    for name in declared {
        let mapped: &[&str] = match name.as_str() {
            "read-files" | "read" => &["read"],
            "search" => &["search"],
            "write-files" => &["write", "edit"],
            "write" => &["write"],
            "edit" => &["edit"],
            "run-shell" | "shell" => &["shell"],
            "invoke-agent" | "agent" => &["agent"],
            n if KNOWN_UNMAPPED.contains(&n) => &[],
            n => {
                WARNED.call_once(|| {
                    eprintln!("se: agent definition declares unknown tool `{n}`; it grants nothing")
                });
                &[]
            }
        };
        for m in mapped {
            if !out.iter().any(|o| o == m) {
                out.push((*m).to_string());
            }
        }
    }
    out
}

/// The older part of a conversation as one plain-text transcript.
fn render_transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        let who = match m.role {
            Role::User => "USER",
            Role::Assistant => "ASSISTANT",
            _ => continue,
        };
        out.push_str(who);
        out.push_str(": ");
        out.push_str(&m.content);
        out.push_str(
            "

",
        );
    }
    out
}

/// One short, tool-less question to the model; its text answer or `None`.
/// Appended to every specialist's body. The 194 roles were written for a richer host
/// (memory, escalation, web search, pre-flight questions); without this a role waits on
/// tools that do not exist here, which is how a turn ended in a plan and no change.
const RUNTIME_NOTE: &str = concat!(
    "\n\n---\nSE runtime (this overrides anything above that assumes otherwise):\n",
    "- You work on the user's real project through the tools offered to you this turn; ",
    "no other tool exists here. Memory, escalation, web search, task graphs and skills are not available.\n",
    "- Once decisions are settled and the task asks for a change you have a write tool for, make the change; ",
    "do not only describe it.\n",
    "- To escalate, finish what you safely can, then add a line `Escalation: <role id or user> - <reason>`.\n",
    "- End with a short handoff: files changed, what you checked and how, what is left.\n",
);

/// Who can settle an open decision, which decides how a role handles one.
#[derive(Clone, Copy)]
enum Asker {
    /// The user is in the conversation: ask them, with options, then wait.
    User,
    /// A sub-agent: hand the question back to the agent that deployed it.
    Parent,
    /// An unattended team task: nobody can answer mid-run.
    Nobody,
}

const ASK_USER: &str = "- Do not guess at a decision that changes what gets built (scope, stack, design, \
data model, anything destructive or hard to undo), including where your procedure says to read memory or \
ask a pre-flight question. If the user has not settled it, ask before building: at most 3 questions, each \
with 2-3 concrete options, your recommendation first and marked (recommended), and a last option \
\"Something else - tell me what you prefer\". Then stop and wait for the answer. A small, clear task needs \
no questions: just do it.\n";
const ASK_PARENT: &str = "- You cannot reach the user. If a decision that changes what gets built is \
unsettled, do not guess: stop and return the question, with 2-3 options and your recommendation first, to \
the agent that deployed you, which will ask the user.\n";
const ASSUME: &str = "- This is an unattended team task and nobody can answer mid-run: where a decision is \
unsettled, take the safest reasonable option and list it under `Assumptions:` in your handoff. Never take a \
destructive or irreversible step on an assumption.\n";

fn runtime_note(body: String, roster: Option<&str>, asker: Asker, root: &str) -> String {
    let decide = match asker {
        Asker::User => ASK_USER,
        Asker::Parent => ASK_PARENT,
        Asker::Nobody => ASSUME,
    };
    // An agent CLI runs in an empty folder of its own and took that for the project.
    let mut out = format!(
        "{body}{RUNTIME_NOTE}{decide}- The project root is `{root}`. Use paths relative to it, \
         also in a task you hand to another agent.\n"
    );
    if let Some(r) = roster {
        out.push_str(
            "- A change your own tools cannot make goes to the specialist who owns it: call the \
             `agent` tool with its role id and the complete task, then report its result. If it \
             returns a question, put it to the user with its options. Specialists by department:\n",
        );
        out.push_str(r);
    }
    out
}

/// The role body a specialist prompt starts with, without SE's runtime note (tests).
#[cfg(test)]
pub(crate) fn role_body(prompt: &str) -> &str {
    prompt.split("\n\n---\nSE runtime").next().unwrap_or(prompt)
}

fn ask(
    provider: &dyn se_provider::ModelProvider,
    model: &str,
    prompt: &str,
    sink: &TurnSink,
) -> (Option<String>, Option<Usage>) {
    ask_with(
        provider,
        model,
        crate::router::ROUTER_SYSTEM,
        prompt,
        // Room for a reasoning model to think before its one-word answer: at 64
        // tokens, MiniMax and Nemotron spent it all thinking and answered nothing.
        1024,
        sink,
    )
}

fn ask_with(
    provider: &dyn se_provider::ModelProvider,
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    sink: &TurnSink,
) -> (Option<String>, Option<Usage>) {
    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![
            Message::text(Role::System, system),
            Message::text(Role::User, prompt),
        ],
        max_tokens,
        tools: Vec::new(),
    };
    let mut text = String::new();
    let mut usage = None;
    let result = provider.stream(&request, &|| sink.is_cancelled(), &mut |event| {
        match event {
            StreamEvent::TextDelta(t) => text.push_str(&t),
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
            } => {
                usage = Some(Usage {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                    source: UsageSource::ProviderReported,
                })
            }
            _ => {}
        }
        true
    });
    (result.ok().map(|()| text), usage)
}

/// Fresh tokens a call reported (input minus cache reads, plus output), or
/// `None` when it reported neither figure (never treated as zero). Cache
/// reads are left out: an agent CLI resends its ~30k-token prompt every
/// round from cache, which spent a 60k sub-agent budget in two rounds.
fn reported_tokens(u: Option<&Usage>) -> Option<u64> {
    let u = u?;
    if u.input_tokens.is_none() && u.output_tokens.is_none() {
        return None;
    }
    let fresh_input = u
        .input_tokens
        .unwrap_or(0)
        .saturating_sub(u.cached_input_tokens.unwrap_or(0));
    Some(fresh_input + u.output_tokens.unwrap_or(0))
}

impl Runner for ProviderRunner {
    fn describe(&self) -> (String, String) {
        (self.provider_id.clone(), self.model.clone())
    }

    fn attach_team(&mut self, team: crate::team::TeamContext) {
        self.team = Some(team);
    }

    fn set_specialist(&self, id: Option<String>) {
        self.sticky_specialist.set(id);
    }

    fn set_mode(&self, mode: se_protocol::WorkMode) {
        *self.mode.lock().unwrap_or_else(|p| p.into_inner()) = mode;
    }

    fn run(&self, turn: &UserTurn, history: &[Message], sink: &mut TurnSink) -> Outcome {
        self.run_inner(turn, history, sink, None)
    }
}

impl ProviderRunner {
    pub(crate) fn work_mode(&self) -> se_protocol::WorkMode {
        *self.mode.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl ProviderRunner {
    /// One turn. `child` is set when the turn is a sub-agent's, deployed
    /// by a parent turn's `agent` tool (see `agent_tool`).
    fn run_inner(
        &self,
        turn: &UserTurn,
        history: &[Message],
        sink: &mut TurnSink,
        child: Option<&agent_tool::Child>,
    ) -> Outcome {
        // Charter §6.6 precedence. The runner's own provider is the
        // global default; the catalogue has no per-agent model field
        // yet, and teams/departments have no config, so those are None.
        let default = se_protocol::ModelRef {
            provider: self.provider_id.clone(),
            model: self.model.clone(),
        };
        if turn.model.is_none() && self.provider_id.is_empty() {
            return Outcome::Failed {
                detail: "no model selected: pick one with /models (SE remembers it for next time)"
                    .to_string(),
            };
        }
        let chosen =
            match crate::routing::resolve(turn.model.as_ref(), None, None, None, Some(&default)) {
                Ok(m) => m,
                Err(detail) => return Outcome::Failed { detail },
            };
        let Some(provider) = self.registry.get(&chosen.provider) else {
            return Outcome::Failed {
                detail: format!("`{}` is not a known provider", chosen.provider),
            };
        };
        // The default was checked at construction; an override was not.
        if chosen != default {
            let status = provider.detect();
            if !status.configured {
                return Outcome::Failed {
                    detail: format!("`{}` is not configured: {}", chosen.provider, status.detail),
                };
            }
        }

        // Resolved before `start_turn` fires, not after: a failed
        // lookup must never leave a `TurnStarted` with no matching
        // terminal event's worth of setup half-done. This is also the
        // ONLY place the catalogue is read for this turn -- the
        // resolved `system_prompt` string is reused for every round
        // trip the loop below makes, which is the entire "pinned for
        // the run, never re-read mid-run" requirement (Phase 3 item 4):
        // nothing after this point ever calls `se_catalogue::load`
        // again.
        // `invoked` is emitted after `start_turn` below, once this
        // turn's `TurnId` actually exists, so the provenance event
        // correlates with the right turn rather than carrying `turn:
        // None`. The resolution that can fail happens here, though,
        // before `start_turn` opens anything -- a failed lookup must
        // never leave a `TurnStarted` with nothing to close it.
        // The catalogue is read once per turn, shared by automatic routing
        // and the specialist lookup. Only a specialist the user *named*
        // makes a missing or unreadable catalogue fatal; automatic
        // routing just falls back to a plain turn.
        let node = turn.team_node.as_deref();
        let team_kind = match (&self.team, node) {
            (Some(t), Some(n)) => t.kind(n),
            _ => crate::team::NodeKind::Work,
        };
        let evaluator = team_kind == crate::team::NodeKind::Evaluate;
        // An evaluator speaks only as the evaluator. An ensemble member
        // with no named agent is not routed: routing on each member's own
        // model could pick different specialists, and then the members
        // would not have received the same packet (row 4.3).
        let unrouted = evaluator
            || match (&self.team, node) {
                (Some(t), Some(n)) => t.is_ensemble_member(n),
                _ => false,
            };
        let catalogue = match (&turn.specialist_id, &self.catalogue_root) {
            _ if evaluator => None,
            (Some(_), None) => {
                return Outcome::Failed {
                    detail: "no catalogue root configured for specialist dispatch".to_string(),
                };
            }
            (Some(_), Some(root)) => match se_catalogue::load(root) {
                Ok(r) => Some(r),
                Err(e) => {
                    return Outcome::Failed {
                        detail: format!("could not read the agent catalogue: {e}"),
                    };
                }
            },
            // Forced routing: a catalogue that will not load fails the
            // turn rather than silently running it with no specialist.
            (None, Some(root)) if self.auto_route => match se_catalogue::load(root) {
                Ok(r) => Some(r),
                Err(e) => {
                    return Outcome::Failed {
                        detail: format!(
                            "every turn runs as an SE specialist, but the agent catalogue at {} could not be read: {e}",
                            root.display()
                        ),
                    };
                }
            },
            (None, _) => None,
        };
        // Scheduled work with no named agent spends the chosen vendor's
        // credential on routing calls, so it is authorized first, under
        // the router's own identity.
        let pre_authorized = turn.specialist_id.is_none() && node.is_some();
        if pre_authorized {
            let router_id = (
                AUTO_ROUTE_AGENT_ID.to_string(),
                NATIVE_LOOP_AGENT_DEFINITION_HASH.to_string(),
            );
            if let Err(detail) = self.authorize_provider(sink, turn, &chosen, &default, &router_id)
            {
                return Outcome::Failed { detail };
            }
        }
        let mut total_usage: Option<Usage> = None;
        let specialist_id: Option<String> = match &turn.specialist_id {
            Some(id) => Some(id.clone()),
            // An unnamed ensemble member is routed by the free, deterministic
            // keyword match, never by its own model, so every member still
            // receives a byte-identical packet (row 4.3).
            None if unrouted => catalogue.as_ref().map(|c| {
                crate::router::lexical(&turn.text, &c.agents)
                    .map_or(FALLBACK_AGENT_ID.to_string(), |a| a.frontmatter.id.clone())
            }),
            None => catalogue.as_ref().map(|c| {
                let kept = node
                    .is_none()
                    .then(|| self.sticky_specialist.get())
                    .flatten()
                    .filter(|id| c.agents.iter().any(|a| &a.frontmatter.id == id));
                // Without a router model the specialist stays put (a re-route would cost
                // the turn's own model two calls a turn); with one, it may move.
                if let Some(id) = kept.as_ref().filter(|_| self.router_model.is_none()) {
                    return id.clone();
                }
                let (picked, usage) = self.route(
                    provider,
                    &chosen.model,
                    &turn.text,
                    &c.agents,
                    sink,
                    node,
                    kept.as_deref(),
                );
                total_usage = usage;
                picked
                    .or(kept)
                    .unwrap_or_else(|| FALLBACK_AGENT_ID.to_string())
            }),
        };

        let specialist_id = specialist_id.filter(|_| !evaluator);
        // The fallback only stands in for a turn nothing matched; it is
        // kept only when the user named it.
        let keep = |id: &String| id != FALLBACK_AGENT_ID || turn.specialist_id.is_some();
        // A sub-agent's role is never the conversation's specialist.
        if let (None, None, Some(id)) = (
            node,
            child,
            specialist_id.as_ref().filter(|id| keep(id)),
        ) {
            self.sticky_specialist.set(Some(id.clone()));
        }
        let (system_prompt, invoked, allowed_tools, max_calls, budget_tokens, roster) =
            match &specialist_id {
            None if evaluator => (
                crate::team::evaluate::EVALUATOR_SYSTEM.to_string(),
                None,
                None,
                None,
                None,
                String::new(),
            ),
            None => (SYSTEM_PROMPT.to_string(), None, None, None, None, String::new()),
            Some(id) => {
                let report = catalogue.expect("loaded whenever a specialist id exists");
                let roster = crate::router::roster(&report.agents);
                let Some(agent) = report.agents.into_iter().find(|a| &a.frontmatter.id == id)
                else {
                    return Outcome::Failed {
                        detail: format!(
                            "no specialist `{id}` in the catalogue -- try `se agents` to list ids"
                        ),
                    };
                };
                // An empty `tools:` list is "not specified", not "no
                // tools allowed" -- the 194 real definitions vary in
                // whether they set it, and an unset allowlist must not
                // silently deny every call a specialist with no
                // opinion on tools makes.
                // Declared but mapping to nothing (e.g. only memory-*)
                // stays `Some(empty)`: deny-all, never widened to `None`.
                let allowed = if agent.frontmatter.tools.is_empty() {
                    None
                } else {
                    Some(se_tools_for(&agent.frontmatter.tools))
                };
                (
                    agent.body,
                    Some((agent.frontmatter.id, agent.content_hash)),
                    allowed,
                    agent.frontmatter.max_calls,
                    agent.frontmatter.context_budget_tokens,
                    roster,
                )
            }
        };
        // Depth 1, enforced here: a sub-agent is never offered `agent`,
        // and neither is a role that did not declare it, an evaluator,
        // or a team node (its claims and budget would not reach the child).
        // Agent CLIs get it too, through the text tool bridge in se-provider.
        let may_spawn = child.is_none()
            && !evaluator
            && node.is_none()
            && allowed_tools
                .as_ref()
                .is_none_or(|a| a.iter().any(|t| t == agent_tool::AGENT_TOOL));
        let system_prompt = match &invoked {
            Some(_) => {
                let asker = match (child, node) {
                    (Some(_), _) => Asker::Parent,
                    (None, Some(_)) => Asker::Nobody,
                    (None, None) => Asker::User,
                };
                runtime_note(
                    system_prompt,
                    may_spawn.then_some(roster.as_str()),
                    asker,
                    &self.workspace_root.display().to_string(),
                )
            }
            None => system_prompt,
        };

        // Charter §6.6 level 2: a role given its own model (`~/.se/models.json`) runs on it.
        // It outranks the session's `/models` pick (the user set both, and this one names
        // the role), never a team task's explicit `model:`.
        let profile = invoked
            .as_ref()
            .and_then(|(id, _)| self.role_models.get(id))
            .filter(|m| **m != chosen && !(node.is_some() && turn.model.is_some()));
        // Sub-agents start from the session's model, not this role's own: the chief on
        // the top model must not put every specialist it deploys there too (seen live).
        let session_model = chosen.clone();
        let (chosen, provider) = match profile {
            None => (chosen, provider),
            Some(m) => match self.registry.get(&m.provider) {
                Some(p) if p.detect().configured => (m.clone(), p),
                _ => {
                    return Outcome::Failed {
                        detail: format!(
                            "this role runs on {}/{} (~/.se/models.json), but that provider is not \
                             configured -- connect it or change the file",
                            m.provider, m.model
                        ),
                    };
                }
            },
        };

        // Who this turn acts as, for every gateway decision it causes:
        // the specialist (pinned to its definition's hash) or the plain
        // native loop. Rules can then target a specialist by id.
        let agent: (String, String) = match &invoked {
            Some((id, hash)) => (id.clone(), format!("{hash:016x}")),
            None => (
                NATIVE_LOOP_AGENT_ID.to_string(),
                NATIVE_LOOP_AGENT_DEFINITION_HASH.to_string(),
            ),
        };
        if !pre_authorized {
            if let Err(detail) = self.authorize_provider(sink, turn, &chosen, &default, &agent) {
                return Outcome::Failed { detail };
            }
        }

        if sink.start_turn(&chosen.provider, &chosen.model).is_err() {
            return Outcome::Failed {
                detail: "the interface disconnected".to_string(),
            };
        }
        if let Some((agent_id, content_hash)) = invoked {
            if sink.specialist_invoked(&agent_id, content_hash).is_err() {
                return Outcome::Failed {
                    detail: "the interface disconnected".to_string(),
                };
            }
        }

        // Context assembly is measured even though it is currently
        // trivial, so that it stays visible when it stops being trivial.
        // `messages` grows across round trips (an assistant tool-calls
        // turn, then one tool-result turn per call) as the loop below
        // runs; what the model sees this turn and what gets reported
        // back as the final answer never includes those intermediate
        // turns in cross-turn `history` -- only `engine.rs` deciding to
        // persist `Outcome::Completed`'s `final_text` does that.
        // Compaction: summarize older history once, hand the summary back
        // to the core to replace it, and send summary + recent turns. A
        // failed summary never fails the turn; it just sends the history.
        let compacted: Vec<Message>;
        let mut history = history;
        if let Some(split) = self
            .compact_at_tokens
            .and_then(|at| compaction_split(history, at))
        {
            let (summary, usage) = ask_with(
                provider,
                &chosen.model,
                COMPACT_SYSTEM,
                &render_transcript(&history[..split]),
                1024,
                sink,
            );
            self.charge(node, usage.as_ref());
            if let Some(u) = usage {
                total_usage = Some(match total_usage {
                    None => u,
                    Some(prev) => merge_usage(prev, u),
                });
            }
            if let Some(summary) = summary.filter(|t| !t.trim().is_empty()) {
                let with = vec![
                    Message::text(Role::User, format!("{MEMORY_PREFIX}{}", summary.trim())),
                    Message::text(
                        Role::Assistant,
                        "Understood. I'll continue from that summary.",
                    ),
                ];
                sink.compact_history(split, with.clone());
                compacted = with
                    .into_iter()
                    .chain(history[split..].iter().cloned())
                    .collect();
                history = &compacted;
            }
        }

        let mut messages: Vec<Message> = sink.timed(Stage::ContextBuild, || {
            let mut messages = Vec::with_capacity(history.len() + 2);
            messages.push(Message::text(Role::System, &system_prompt));
            let fixed = system_prompt.len() + turn.text.len();
            messages.extend_from_slice(fit_history(history, budget_tokens, fixed));
            messages.push(Message::text(Role::User, turn.text.clone()));
            messages
        });

        let mut answer = String::new();
        let mut first_token_at: Option<Instant> = None;
        // Measured from the very first provider call of the turn, never
        // reset on a later round trip -- the brief's requirement that
        // time-to-first-token is not re-measured per round trip.
        let request_started = Instant::now();
        // One deadline for the whole turn, computed once; every round
        // trip below shares the same `turn_started` and `turn_deadline`,
        // so the bound is on the loop, not reset per model call.
        let turn_started = Instant::now();
        let turn_deadline = child.map_or(self.turn_deadline, |c| c.deadline);
        let deadline_fired = Arc::new(AtomicBool::new(false));
        let mut tools = if evaluator {
            Vec::new()
        } else {
            self.tool_definitions()
        };
        if let (Some(_), Some(allowed)) = (child, &allowed_tools) {
            tools.retain(|t| allowed.contains(&t.name));
        }
        if may_spawn {
            tools.push(agent_tool::definition());
        }
        // Plan mode offers no tool that changes the project; handle_tool_call re-checks.
        let mode = self.work_mode();
        tools.retain(|t| mode.allows(&t.name));
        let mut child_used: u64 = 0;

        let mut round = 0usize;
        let mut calls_run: u32 = 0;
        let outcome = loop {
            round += 1;
            if sink.is_cancelled() {
                break Outcome::Cancelled;
            }
            if round > MAX_TOOL_ROUNDS {
                break Outcome::Incomplete {
                    partial_text: answer,
                    reason: "the turn made too many tool-call round trips".to_string(),
                };
            }

            // Row 4.16: at the cap the call is not made at all.
            if let Err(detail) = self.admit(node) {
                break if answer.is_empty() {
                    Outcome::Failed { detail }
                } else {
                    Outcome::Incomplete {
                        partial_text: answer,
                        reason: detail,
                    }
                };
            }
            let request = ChatRequest {
                model: chosen.model.clone(),
                messages: messages.clone(),
                max_tokens: MAX_TOKENS,
                // An evaluator is offered no tools at all (item 4).
                tools: tools.clone(),
            };

            let mut round_answer = String::new();
            let mut finish_reason: Option<String> = None;
            let mut round_usage: Option<Usage> = None;
            let mut disconnected = false;
            let mut tool_calls: Vec<ToolCallRecord> = Vec::new();

            // Cloned rather than borrowed from `sink`, which is held
            // mutably below to report deltas.
            let cancelled = sink.cancel_flag();
            // Distinguishes "the deadline fired" from "the user
            // cancelled" after the fact: both reach the provider
            // through the same closure returning `true` (a provider has
            // one cancellation mechanism, not two), so this flag is the
            // only way to tell them apart once `stream` has returned.
            let deadline_fired_writer = deadline_fired.clone();
            let should_stop = move || {
                if cancelled.load(Ordering::SeqCst) {
                    return true;
                }
                if turn_started.elapsed() >= turn_deadline {
                    deadline_fired_writer.store(true, Ordering::SeqCst);
                    return true;
                }
                false
            };
            let mut attempt = 0u32;
            let result = loop {
                let result = provider.stream(&request, &should_stop, &mut |event| match event {
                    StreamEvent::TextDelta(text) => {
                        if first_token_at.is_none() {
                            first_token_at = Some(Instant::now());
                        }
                        round_answer.push_str(&text);
                        if sink.text_delta(&text).is_err() {
                            disconnected = true;
                            return false;
                        }
                        true
                    }
                    StreamEvent::ReasoningDelta(text) => {
                        if first_token_at.is_none() {
                            first_token_at = Some(Instant::now());
                        }
                        if sink.reasoning_delta(&text).is_err() {
                            disconnected = true;
                            return false;
                        }
                        true
                    }
                    StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cached_input_tokens,
                    } => {
                        round_usage = Some(Usage {
                            input_tokens,
                            output_tokens,
                            cached_input_tokens,
                            source: UsageSource::ProviderReported,
                        });
                        true
                    }
                    // A mid-stream provider error becomes a transport-level
                    // failure via the `Err` the provider returns; nothing is
                    // appended to `answer`, so a failed turn cannot present
                    // a partial reply as the answer.
                    StreamEvent::Error(_) => false,
                    // Recorded for the outcome, not shown as content.
                    StreamEvent::FinishReason(reason) => {
                        finish_reason = Some(reason);
                        true
                    }
                    StreamEvent::ToolCallRequested {
                        id,
                        name,
                        arguments,
                    } => {
                        tool_calls.push(ToolCallRecord {
                            id,
                            name,
                            arguments,
                        });
                        true
                    }
                    StreamEvent::Done => true,
                });
                // A temporary failure (429, 5xx, network) before anything has
                // streamed is retried a bounded number of times. Once text or a
                // tool call has arrived, a retry could duplicate it, so it is not.
                match &result {
                    Err(e)
                        if e.is_retriable()
                            && attempt < MAX_RETRIES
                            && round_answer.is_empty()
                            && tool_calls.is_empty()
                            && !should_stop() =>
                    {
                        attempt += 1;
                        let until = Instant::now() + retry_wait(e, attempt);
                        while Instant::now() < until && !should_stop() {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        if should_stop() {
                            break result;
                        }
                        round_usage = None;
                        finish_reason = None;
                    }
                    _ => break result,
                }
            };

            answer.push_str(&round_answer);
            self.charge(node, round_usage.as_ref());
            if let (Some(c), Some(used)) = (child, reported_tokens(round_usage.as_ref())) {
                child_used += used;
                let _ = sink.emit(se_protocol::Payload::Agent(se_protocol::AgentEvent::Tokens {
                    agent_id: c.id.clone(),
                    used: child_used,
                }));
                if child_used > c.budget {
                    break Outcome::Failed {
                        detail: format!(
                            "token budget exceeded: used {child_used} of {} tokens",
                            c.budget
                        ),
                    };
                }
            }
            if let Some(usage) = round_usage {
                total_usage = Some(match total_usage {
                    None => usage,
                    Some(prev) => merge_usage(prev, usage),
                });
            }

            // The tie-break is a pure function over three booleans (see
            // `deadline_precedence` below) specifically so every one of
            // its input combinations is a named row in a test, rather
            // than a property inferred from reading this `if`.
            match deadline_precedence(
                deadline_fired.load(Ordering::SeqCst),
                sink.is_cancelled(),
                answer.is_empty(),
            ) {
                DeadlinePrecedence::NotApplicable => {}
                DeadlinePrecedence::Failed => {
                    break Outcome::Failed {
                        detail: ProviderError::Timeout(TimeoutKind::WholeTurn).to_string(),
                    };
                }
                DeadlinePrecedence::Incomplete => {
                    break Outcome::Incomplete {
                        partial_text: answer,
                        reason: ProviderError::Timeout(TimeoutKind::WholeTurn).to_string(),
                    };
                }
            }

            match result {
                Ok(()) if disconnected => {
                    // A real answer may already have streamed before the
                    // interface disappeared; preserve it as Incomplete
                    // rather than discarding it as a bare failure.
                    break if answer.is_empty() {
                        Outcome::Failed {
                            detail: "the interface disconnected mid-answer".to_string(),
                        }
                    } else {
                        Outcome::Incomplete {
                            partial_text: answer,
                            reason: "the interface disconnected mid-answer".to_string(),
                        }
                    };
                }
                Ok(())
                    if !tool_calls.is_empty() && finish_reason.as_deref() == Some("tool_calls") =>
                {
                    // The model asked to run tools: carry its own
                    // tool-calls message, then exactly one tool-result
                    // message per call (including a denied or errored
                    // one -- the model must see why, not have the turn
                    // silently stop), then loop back with a fresh
                    // request rather than returning.
                    messages.push(Message::assistant_tool_calls(tool_calls.clone()));
                    // A round's `agent` calls run first, side by side; the
                    // results are fed back below in the model's order.
                    let mut spawned = if may_spawn {
                        self.run_agents(sink, &tool_calls, &agent, &session_model)
                    } else {
                        vec![None; tool_calls.len()]
                    };
                    let mut interrupted = false;
                    for (i, call) in tool_calls.iter().enumerate() {
                        if sink.is_cancelled() {
                            interrupted = true;
                            break;
                        }
                        // The specialist's own `max_calls` (row 3.9):
                        // past it a call is not run, and the model is
                        // told why so it can finish with what it has.
                        let result_text = match max_calls {
                            _ if spawned[i].is_some() => spawned[i].take().unwrap_or_default(),
                            _ if child.is_some() && call.name == agent_tool::AGENT_TOOL => self
                                .reject_unparsed(
                                    sink,
                                    call,
                                    &agent,
                                    "a sub-agent cannot deploy agents",
                                ),
                            _ if evaluator => self.reject_unparsed(
                                sink,
                                call,
                                &agent,
                                "an evaluator has no tools",
                            ),
                            Some(max) if calls_run >= max => self.reject_unparsed(
                                sink,
                                call,
                                &agent,
                                &format!("this specialist's budget of {max} tool calls is spent"),
                            ),
                            _ => {
                                calls_run += 1;
                                self.handle_tool_call(
                                    sink,
                                    call,
                                    allowed_tools.as_deref(),
                                    &agent,
                                    node,
                                )
                            }
                        };
                        messages.push(Message::tool_result(call.id.clone(), result_text));
                    }
                    if interrupted {
                        break Outcome::Cancelled;
                    }
                    continue;
                }
                Ok(()) => {
                    // A provider that completed without ever saying why
                    // it stopped is accepted (many send only `[DONE]`);
                    // `length` and `content_filter` never reach here --
                    // the provider reports those as a Truncated/Policy
                    // `Err` so they are handled below, with the partial
                    // text preserved.
                    break match finish_reason.as_deref() {
                        None | Some("stop") | Some("end_turn") => {
                            Outcome::Completed { final_text: answer }
                        }
                        Some(other) => Outcome::Incomplete {
                            partial_text: answer,
                            reason: format!("the provider stopped early ({other})"),
                        },
                    };
                }
                Err(ProviderError::Cancelled) => break Outcome::Cancelled,
                // Truncation and any other mid-stream failure both
                // preserve whatever already streamed, as Incomplete
                // rather than Failed -- the distinction the outcome type
                // exists to make. Only a failure with nothing to
                // preserve (e.g. Auth rejected before the first byte) is
                // a bare Failed.
                Err(e) if !answer.is_empty() => {
                    break Outcome::Incomplete {
                        partial_text: answer,
                        reason: e.to_string(),
                    };
                }
                Err(e) => {
                    break Outcome::Failed {
                        detail: e.to_string(),
                    };
                }
            }
        };

        // Recorded once, after every round trip this turn made --
        // whichever one actually produced the first token (or none
        // did), not re-measured per round.
        if let Some(at) = first_token_at {
            sink.record(se_protocol::Span {
                stage: Stage::TimeToFirstToken,
                duration: at.saturating_duration_since(request_started),
            });
        } else {
            sink.record(se_protocol::Span {
                stage: Stage::TimeToFirstToken,
                duration: request_started.elapsed(),
            });
        }
        // Only ever the provider's own figures, summed across every
        // round trip that reported any.
        if let Some(usage) = total_usage {
            let _ = sink.usage(usage);
        }

        outcome
    }
}

/// Sums two reported usage figures field by field. A field absent on
/// both sides stays absent; present on only one side is carried through
/// unchanged -- summing is never invented across a gap the provider
/// itself left silent.
fn merge_usage(a: Usage, b: Usage) -> Usage {
    fn add(x: Option<u64>, y: Option<u64>) -> Option<u64> {
        match (x, y) {
            (None, None) => None,
            (Some(x), None) => Some(x),
            (None, Some(y)) => Some(y),
            (Some(x), Some(y)) => Some(x + y),
        }
    }
    Usage {
        input_tokens: add(a.input_tokens, b.input_tokens),
        output_tokens: add(a.output_tokens, b.output_tokens),
        cached_input_tokens: add(a.cached_input_tokens, b.cached_input_tokens),
        source: UsageSource::ProviderReported,
    }
}

impl ProviderRunner {
    /// A call refused before it could reach the gateway (bad arguments,
    /// a path outside the workspace, a spent `max_calls`) is still shown
    /// on the tool lifecycle and audited as a `Deny`, so an escape
    /// attempt is never invisible.
    fn reject_unparsed(
        &self,
        sink: &mut TurnSink,
        call: &ToolCallRecord,
        agent: &(String, String),
        reason: &str,
    ) -> String {
        let text = format!("tool call rejected before it ran: {reason}");
        let id = se_protocol::ToolCallId::next();
        let _ = sink.tool_requested(id, &call.name, &call.name);
        let _ = sink.tool_completed(id, &call.name, false, &text);
        // Already a refusal, so a failed record cannot make it unsafe.
        let _ = self.audit.record(se_permission::AuditRecord {
            agent_id: sink.agent_tag().unwrap_or_else(|| agent.0.clone()),
            session: sink.session(),
            task: sink.task(),
            tool_name: call.name.clone(),
            // The reason, never the raw arguments: a `write`'s arguments
            // carry file content, which has no place in the audit trail.
            resource_scope: reason.to_string(),
            effect: se_permission::Effect::Deny,
        });
        text
    }

    /// Picks a specialist for a turn that named none (see `crate::router`):
    /// two short multiple-choice questions to the router model (or the turn's
    /// own API model), each answer checked against the catalogue, else `None`.
    /// No keyword fallback: on real wording it picked confidently wrong roles.
    /// With `current`, the router may answer KEEP, and a same-department
    /// answer keeps it too, so a follow-up never flip-flops within a field.
    /// A failed routing call never fails the turn.
    ///
    /// shortcut: the two routing calls' token usage is not reported.
    /// Fold it into the turn's usage when cost attribution needs it.
    #[allow(clippy::too_many_arguments)] // one call site; a params struct would only rename them
    fn route(
        &self,
        provider: &dyn se_provider::ModelProvider,
        model: &str,
        task: &str,
        agents: &[se_catalogue::AgentDefinition],
        sink: &TurnSink,
        node: Option<&str>,
        current: Option<&str>,
    ) -> (Option<String>, Option<Usage>) {
        use crate::router;
        let routed = self
            .router_model
            .as_ref()
            .and_then(|m| Some((self.registry.get(&m.provider)?, m.model.as_str())));
        let (provider, model) = match routed {
            Some(r) => r,
            // An agent CLI costs a process start and its own large prompt per call.
            None if provider.is_agent_cli() => return (None, None),
            None => (provider, model),
        };
        let mut usage: Option<Usage> = None;
        // Each routing call is budgeted and charged like any other; a
        // spent budget skips straight to the free keyword fallback.
        let mut ask_once = |prompt: &str| -> Option<String> {
            self.admit(node).ok()?;
            let (text, u) = ask(provider, model, prompt, sink);
            self.charge(node, u.as_ref());
            if let Some(u) = u {
                usage = Some(match usage {
                    None => u,
                    Some(prev) => merge_usage(prev, u),
                });
            }
            text
        };
        let depts = router::departments(agents);
        let names: Vec<&str> = depts.iter().map(|(d, _)| d.as_str()).collect();
        let current_dept = current.and_then(|id| {
            agents
                .iter()
                .find(|a| a.frontmatter.id == id)
                .map(|a| (id, a.frontmatter.department.as_str()))
        });
        let reply = ask_once(&router::department_prompt(task, &depts, current_dept));
        if let Some((id, d)) = current_dept {
            let keep = reply.as_deref().is_none_or(|r| {
                r.trim().eq_ignore_ascii_case("keep") || router::pick(r, &names) == Some(d)
            });
            if keep {
                return (Some(id.to_string()), usage);
            }
        }
        let dept = reply.and_then(|r| router::pick(&r, &names).map(str::to_string));
        if let Some(dept) = dept {
            let roles: Vec<&se_catalogue::AgentDefinition> = agents
                .iter()
                .filter(|a| a.frontmatter.department == dept && a.frontmatter.enabled)
                .collect();
            let ids: Vec<&str> = roles.iter().map(|a| a.frontmatter.id.as_str()).collect();
            let role = ask_once(&router::role_prompt(task, &roles))
                .and_then(|r| router::pick(&r, &ids).map(str::to_string));
            if role.is_some() {
                return (role, usage);
            }
        }
        (None, usage)
    }

    fn admit(&self, node: Option<&str>) -> Result<(), String> {
        match (&self.team, node) {
            (Some(t), Some(n)) => t.admit(n),
            _ => Ok(()),
        }
    }

    fn charge(&self, node: Option<&str>, usage: Option<&Usage>) {
        if let (Some(t), Some(n)) = (&self.team, node) {
            t.charge(n, reported_tokens(usage));
        }
    }

    /// Per-agent credential scoping (Phase 4 item 2). Using a provider
    /// spends its stored credential, so a specialist turn may only use a
    /// provider other than the operator's configured default through the
    /// gateway (`use_provider` on `provider:<id>`): a ruleset `Deny`
    /// blocks it, `Ask` asks the human once, and every outcome is
    /// audited. The default provider, and a direct user turn (the user
    /// picked the model themselves with `/models`), need no check.
    fn authorize_provider(
        &self,
        sink: &mut TurnSink,
        turn: &UserTurn,
        chosen: &se_protocol::ModelRef,
        default: &se_protocol::ModelRef,
        agent: &(String, String),
    ) -> Result<(), String> {
        if chosen.provider == default.provider
            || (turn.specialist_id.is_none() && turn.team_node.is_none())
        {
            return Ok(());
        }
        // A model the user picked in the interface (`/models`) is their own
        // choice: it still goes through the gateway (a deny rule wins, and
        // it is audited), but it is never put back to them as a question.
        // Every interactive turn runs as a specialist, so asking here would
        // ask on every turn after a `/models` pick.
        // A model the user gave a role in ~/.se/models.json is their pick too.
        let users_pick = (turn.team_node.is_none() && turn.model.as_ref() == Some(chosen))
            || self.role_models.values().any(|m| m == chosen);
        let shown = format!(
            "{} wants to use {}/{} with its stored credential",
            agent.0, chosen.provider, chosen.model
        );
        let req = PermRequest {
            agent_id: agent.0.clone(),
            agent_definition_hash: agent.1.clone(),
            session: sink.session(),
            task: sink.task(),
            tool_name: "use_provider".to_string(),
            tool_schema_version: 1,
            resource: se_permission::ResourceIdentity(format!("provider:{}", chosen.provider)),
            resource_scope: chosen.provider.clone(),
            cwd: self.workspace_root.to_string_lossy().into_owned(),
            env_subset: Vec::new(),
            shown_to_human: shown.clone(),
        };
        let audit = agent_tool::Tagged(self.audit.as_ref(), sink.agent_tag());
        let gateway = Gateway::new(&self.ruleset, &self.ledger, &audit);
        match gateway.decide(&req) {
            se_permission::Decision::Allow(_) => Ok(()),
            se_permission::Decision::Deny { reason } => Err(reason),
            se_permission::Decision::Ask if users_pick => gateway.approve(&req).map(|_| ()),
            se_permission::Decision::Ask => {
                if sink.request_approval("use_provider", &shown).is_some() {
                    gateway.approve(&req).map(|_| ())
                } else {
                    gateway.refuse(&req);
                    Err(format!("not approved: {shown}"))
                }
            }
        }
    }

    /// Runs one tool call end to end: ruleset check, human approval if
    /// the ruleset defers, execution, and the sink events that make it
    /// visible. Returns the text to feed back to the model as that
    /// call's `Role::Tool` result -- a denial or an error included,
    /// since the model must see why a call did not run rather than have
    /// the turn silently stop.
    ///
    /// The approval mapping, chosen here because `se-permission`
    /// deliberately leaves it to the caller:
    /// * `Decision::Deny` is checked *before* any human is asked, and
    ///   is final -- a ruleset denial can never be overridden by a
    ///   human's "yes", because the human is never shown the request at
    ///   all.
    /// * `Decision::Allow` (a ruleset that pre-approves this exact
    ///   call) also never asks a human; the token it already minted is
    ///   used directly.
    /// * `Decision::Ask` is the only case that reaches
    ///   `TurnSink::request_approval`. A human's "yes" is then minted
    ///   into a token via `Gateway::approve` (the documented "approved
    ///   out of band" path); "no", a cancellation, or a disconnected
    ///   interface (`request_approval` returning `None`, which does not
    ///   distinguish the three) is always treated as a refusal -- a
    ///   withheld decision is never permission.
    fn handle_tool_call(
        &self,
        sink: &mut TurnSink,
        call: &ToolCallRecord,
        allowed_tools: Option<&[String]>,
        agent: &(String, String),
        node: Option<&str>,
    ) -> String {
        let mode = self.work_mode();
        if !mode.allows(&call.name) {
            return self.reject_unparsed(
                sink,
                call,
                agent,
                "plan mode: only read and search run; describe the change instead of making it",
            );
        }
        // A tool the role was never offered. Said plainly: "denied by policy" read as a
        // permission wall, and the chief stopped instead of handing the change on (seen live).
        if let Some(own) = allowed_tools.filter(|a| !a.iter().any(|t| t == &call.name)) {
            let next = if own.iter().any(|t| t == agent_tool::AGENT_TOOL) {
                "call the `agent` tool with the owning specialist's role id and the complete task"
            } else {
                "name the change and the role that should make it in your answer"
            };
            let reason = format!(
                "`{}` is not one of your tools (yours: {}); {next}",
                call.name,
                own.join(", ")
            );
            return self.reject_unparsed(sink, call, agent, &reason);
        }
        let parsed =
            match tool_loop::parse_tool_call(&self.workspace_root, &call.name, &call.arguments) {
                Ok(p) => p,
                Err(reason) => return self.reject_unparsed(sink, call, agent, &reason),
            };
        // Rows 4.13/4.14: in a team run, a file another task wrote is
        // refused before anyone is asked to approve the write.
        let claim = match (&self.team, node, &parsed.action) {
            (
                Some(team),
                Some(node),
                ToolAction::Write { path, .. } | ToolAction::Edit { path, .. },
            ) => Some((team, node, path.clone())),
            _ => None,
        };
        if let Some((team, node, path)) = &claim {
            if let Err(conflict) = team.check(node, path) {
                return self.reject_unparsed(sink, call, agent, &conflict.to_string());
            }
        }
        if let (Some(team), Some(node), ToolAction::Shell { .. }) =
            (&self.team, node, &parsed.action)
        {
            if let Some(reason) = team.shell_refused(node) {
                return self.reject_unparsed(sink, call, agent, &reason);
            }
        }
        let resource = match tool_loop::resolve_identity(&parsed) {
            Ok(r) => r,
            Err(e) => return self.reject_unparsed(sink, call, agent, &e.to_string()),
        };

        let req = PermRequest {
            agent_id: agent.0.clone(),
            agent_definition_hash: agent.1.clone(),
            session: sink.session(),
            task: sink.task(),
            tool_name: call.name.clone(),
            tool_schema_version: 1,
            resource: resource.clone(),
            resource_scope: parsed.resource_scope.clone(),
            cwd: parsed.cwd.clone(),
            env_subset: Vec::new(),
            shown_to_human: parsed.display.clone(),
        };
        // A sub-agent is audited as `role#id`; rules still match its role.
        let audit = agent_tool::Tagged(self.audit.as_ref(), sink.agent_tag());
        let gateway = Gateway::new(&self.ruleset, &self.ledger, &audit);

        let (id, approved): (se_protocol::ToolCallId, Box<Approved>) =
            match gateway.decide_scoped(&req, allowed_tools) {
                se_permission::Decision::Deny { reason } => {
                    let id = se_protocol::ToolCallId::next();
                    let _ = sink.tool_requested(id, &call.name, &parsed.display);
                    let _ = sink.tool_completed(
                        id,
                        &call.name,
                        false,
                        &format!("denied by policy: {reason}"),
                    );
                    return format!("denied by policy: {reason}");
                }
                se_permission::Decision::Allow(token) => {
                    let id = se_protocol::ToolCallId::next();
                    if sink
                        .tool_requested(id, &call.name, &parsed.display)
                        .is_err()
                    {
                        return "the interface disconnected before this call could run".to_string();
                    }
                    (id, token)
                }
                // Auto: the user approved every call in advance. Plan: only read/search
                // got this far, and they change nothing. A deny rule still wins above.
                se_permission::Decision::Ask if mode != se_protocol::WorkMode::Manual => {
                    let id = se_protocol::ToolCallId::next();
                    if sink
                        .tool_requested(id, &call.name, &parsed.display)
                        .is_err()
                    {
                        gateway.refuse(&req);
                        return "the interface disconnected before this call could run".to_string();
                    }
                    match gateway.approve(&req) {
                        Ok(token) => (id, token),
                        Err(reason) => {
                            let _ = sink.tool_completed(id, &call.name, false, &reason);
                            return reason;
                        }
                    }
                }
                se_permission::Decision::Ask => {
                    let Some(id) = sink.request_approval(&call.name, &parsed.display) else {
                        gateway.refuse(&req);
                        return "denied: not approved".to_string();
                    };
                    if sink
                        .tool_requested(id, &call.name, &parsed.display)
                        .is_err()
                    {
                        gateway.refuse(&req);
                        return "the interface disconnected before this call could run".to_string();
                    }
                    if sink.is_cancelled() {
                        gateway.refuse(&req);
                        let _ = sink.tool_completed(id, &call.name, false, "cancelled");
                        return "cancelled before this call ran".to_string();
                    }
                    match gateway.approve(&req) {
                        Ok(token) => (id, token),
                        Err(reason) => {
                            let _ = sink.tool_completed(id, &call.name, false, &reason);
                            return reason;
                        }
                    }
                }
            };

        let ctx = ExecutionContext {
            agent_id: req.agent_id.clone(),
            agent_definition_hash: req.agent_definition_hash.clone(),
            session: req.session,
            task: req.task,
            tool_name: req.tool_name.clone(),
            tool_schema_version: req.tool_schema_version,
            resource,
            cwd: req.cwd.clone(),
            env_subset: req.env_subset.clone(),
        };
        let granted = match Granted::new(*approved, ctx, &self.ledger) {
            Ok(g) => g,
            Err(e) => {
                let detail = format!("token verification failed at execution time: {e:?}");
                let _ = sink.tool_completed(id, &call.name, false, &detail);
                return detail;
            }
        };

        // Claimed under the same lock as the final check, right before
        // the write: of two racing writers exactly one gets the file.
        if let Some((team, node, path)) = &claim {
            if let Err(conflict) = team.claim(node, path) {
                let text = conflict.to_string();
                let _ = sink.tool_completed(id, &call.name, false, &text);
                return text;
            }
        }
        let outcome = tool_loop::execute(&granted, parsed.action);
        match outcome {
            Ok(preview) => {
                let _ = sink.tool_completed(id, &call.name, true, &preview);
                preview
            }
            Err(detail) => {
                let _ = sink.tool_completed(id, &call.name, false, &detail);
                format!("tool error: {detail}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_protocol::Payload;
    use se_provider::{ModelInfo, ModelProvider, Service, Status};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Every one of the eight (deadline, cancelled, answer-empty)
    /// combinations, named -- the gap the Phase 0 security review
    /// found: the precedence logic existed, but no test had ever set
    /// both a deadline and a cancellation flag at once.
    #[test]
    fn deadline_precedence_truth_table() {
        let cases = [
            // (deadline_fired, cancelled, answer_is_empty, expected)
            (false, false, false, DeadlinePrecedence::NotApplicable),
            (false, false, true, DeadlinePrecedence::NotApplicable),
            (false, true, false, DeadlinePrecedence::NotApplicable),
            (false, true, true, DeadlinePrecedence::NotApplicable),
            // The deadline fired but cancellation still wins, with an
            // empty or a partial answer -- the exact race the review
            // flagged as having zero coverage.
            (true, true, false, DeadlinePrecedence::NotApplicable),
            (true, true, true, DeadlinePrecedence::NotApplicable),
            (true, false, true, DeadlinePrecedence::Failed),
            (true, false, false, DeadlinePrecedence::Incomplete),
        ];
        for (deadline_fired, cancelled, answer_is_empty, expected) in cases {
            assert_eq!(
                deadline_precedence(deadline_fired, cancelled, answer_is_empty),
                expected,
                "deadline_fired={deadline_fired} cancelled={cancelled} answer_is_empty={answer_is_empty}"
            );
        }
    }
    use std::sync::Arc;
    use std::time::Duration;

    /// A provider under the test's control: it streams what it is told
    /// to, counts how many requests it received, and can be made slow.
    struct ScriptedProvider {
        frames: Vec<StreamEvent>,
        per_frame_delay: Duration,
        requests: Arc<AtomicUsize>,
        /// Fails before streaming anything -- for "nothing happened"
        /// cases (e.g. Auth rejected up front).
        fail_with: Option<ProviderError>,
        /// Streams every frame in `frames`, *then* fails -- for "a real
        /// answer started but did not finish" cases.
        fail_after: Option<ProviderError>,
        /// Ignores `frames` and streams forever (one delta every
        /// `per_frame_delay`) until cancelled -- for deadline tests,
        /// where the point is that nothing *else* ever stops it.
        never_stops: bool,
    }

    impl ModelProvider for ScriptedProvider {
        fn id(&self) -> &str {
            "scripted"
        }
        fn display_name(&self) -> &str {
            "Scripted"
        }
        fn env_var(&self) -> &str {
            "SE_TEST_SCRIPTED_KEY"
        }
        fn detect(&self) -> Status {
            Status {
                configured: true,
                detail: "scripted".into(),
                source: se_provider::Source::Environment,
            }
        }
        fn stream(
            &self,
            _request: &ChatRequest,
            cancelled: &dyn Fn() -> bool,
            on_event: &mut dyn FnMut(StreamEvent) -> bool,
        ) -> Result<(), ProviderError> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            if let Some(e) = &self.fail_with {
                return Err(e.clone());
            }
            if self.never_stops {
                let mut i = 0usize;
                loop {
                    if cancelled() {
                        return Err(ProviderError::Cancelled);
                    }
                    if !self.per_frame_delay.is_zero() {
                        std::thread::sleep(self.per_frame_delay);
                    }
                    // Rechecked after the wait, as a real provider does
                    // before putting more bytes on the wire.
                    if cancelled() {
                        return Err(ProviderError::Cancelled);
                    }
                    if !on_event(StreamEvent::TextDelta(format!("{i} "))) {
                        return Err(ProviderError::Cancelled);
                    }
                    i += 1;
                }
            }
            for frame in &self.frames {
                if cancelled() {
                    return Err(ProviderError::Cancelled);
                }
                if !self.per_frame_delay.is_zero() {
                    std::thread::sleep(self.per_frame_delay);
                }
                if !on_event(frame.clone()) {
                    return Err(ProviderError::Cancelled);
                }
            }
            match &self.fail_after {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    const SCRIPTED: Service = Service {
        id: "scripted",
        display_name: "Scripted",
        key_var: "SE_TEST_SCRIPTED_KEY",
        default_base_url: "http://127.0.0.1:1",
        base_url_var: "SE_TEST_SCRIPTED_BASE_URL",
        key_required: true,
        wire: se_provider::openai_compatible::Wire::OpenAi,
    };

    fn runner_with(
        frames: Vec<StreamEvent>,
        delay: Duration,
        fail_with: Option<ProviderError>,
    ) -> (ProviderRunner, Arc<AtomicUsize>) {
        scripted_runner(frames, delay, fail_with, None, false)
    }

    /// Streams `frames` and then fails -- "a real answer started but did
    /// not finish", which is what distinguishes Incomplete from Failed.
    fn runner_failing_after(
        frames: Vec<StreamEvent>,
        fail_after: ProviderError,
    ) -> (ProviderRunner, Arc<AtomicUsize>) {
        scripted_runner(frames, Duration::ZERO, None, Some(fail_after), false)
    }

    /// Streams forever until something stops it -- so a deadline test is
    /// measuring the deadline, not the end of a script.
    fn runner_that_never_stops(delay: Duration) -> (ProviderRunner, Arc<AtomicUsize>) {
        scripted_runner(Vec::new(), delay, None, None, true)
    }

    fn scripted_runner(
        frames: Vec<StreamEvent>,
        delay: Duration,
        fail_with: Option<ProviderError>,
        fail_after: Option<ProviderError>,
        never_stops: bool,
    ) -> (ProviderRunner, Arc<AtomicUsize>) {
        let requests = Arc::new(AtomicUsize::new(0));
        let mut registry = Registry::empty();
        registry.register(Box::new(ScriptedProvider {
            frames,
            per_frame_delay: delay,
            requests: requests.clone(),
            fail_with,
            fail_after,
            never_stops,
        }));
        let _ = SCRIPTED;
        (
            ProviderRunner {
                registry,
                provider_id: "scripted".into(),
                model: "scripted-model".into(),
                turn_deadline: DEFAULT_TURN_DEADLINE,
                ruleset: Ruleset::default(),
                ledger: NonceLedger::new(),
                catalogue_root: None,
                workspace_root: std::env::temp_dir(),
                audit: std::sync::Arc::new(NullAuditSink),
                auto_route: false,
                compact_at_tokens: None,
                sticky_specialist: Default::default(),
                mode: Default::default(),
                router_model: None,
                role_models: Default::default(),
                team: None,
            },
            requests,
        )
    }

    fn run(runner: ProviderRunner, text: &str) -> (Outcome, Vec<Payload>) {
        let core = crate::start(runner);
        core.submit(text, vec![]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut payloads = Vec::new();
        while std::time::Instant::now() < deadline {
            for event in core.drain_blocking(Duration::from_millis(20)) {
                let terminal = event.is_terminal();
                payloads.push(event.payload.clone());
                if terminal {
                    core.shutdown();
                    let Payload::TaskFinished { outcome, .. } = event.payload else {
                        unreachable!()
                    };
                    return (outcome, payloads);
                }
            }
        }
        panic!("the turn never finished");
    }

    /// Like [`run`], but auto-answers every approval request with
    /// `allow` as it arrives, for tests about the tool loop rather than
    /// about approval itself.
    fn run_auto_approving(
        runner: ProviderRunner,
        text: &str,
        allow: bool,
    ) -> (Outcome, Vec<Payload>) {
        let core = crate::start(runner);
        core.submit(text, vec![]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut payloads = Vec::new();
        while std::time::Instant::now() < deadline {
            for event in core.drain_blocking(Duration::from_millis(20)) {
                if let Payload::ApprovalRequired { call, .. } = &event.payload {
                    core.approve(se_protocol::ApprovalDecision {
                        task: event.task,
                        call: *call,
                        allow,
                    });
                }
                let terminal = event.is_terminal();
                payloads.push(event.payload.clone());
                if terminal {
                    core.shutdown();
                    let Payload::TaskFinished { outcome, .. } = event.payload else {
                        unreachable!()
                    };
                    return (outcome, payloads);
                }
            }
        }
        panic!("the turn never finished");
    }

    /// A provider that requests one tool call on its first request
    /// (one that the five real tools recognise: `write`), then answers
    /// plainly on every later request -- so the loop's second round
    /// trip is exercised with a real model-shaped request/response,
    /// not a stub.
    struct ToolCallingProvider {
        requests_seen: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
        tool_name: String,
        arguments: String,
    }

    impl ModelProvider for ToolCallingProvider {
        fn id(&self) -> &str {
            "toolcalling"
        }
        fn display_name(&self) -> &str {
            "ToolCalling"
        }
        fn env_var(&self) -> &str {
            "SE_TEST_TOOLCALLING_KEY"
        }
        fn detect(&self) -> Status {
            Status {
                configured: true,
                detail: "toolcalling".into(),
                source: se_provider::Source::Environment,
            }
        }
        fn stream(
            &self,
            request: &ChatRequest,
            _cancelled: &dyn Fn() -> bool,
            on_event: &mut dyn FnMut(StreamEvent) -> bool,
        ) -> Result<(), ProviderError> {
            let round = {
                let mut seen = self.requests_seen.lock().unwrap();
                seen.push(request.clone());
                seen.len()
            };
            if round == 1 {
                on_event(StreamEvent::ToolCallRequested {
                    id: "call_1".into(),
                    name: self.tool_name.clone(),
                    arguments: self.arguments.clone(),
                });
                on_event(StreamEvent::FinishReason("tool_calls".into()));
            } else {
                on_event(StreamEvent::TextDelta("acknowledged".into()));
                on_event(StreamEvent::FinishReason("stop".into()));
            }
            on_event(StreamEvent::Done);
            Ok(())
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    fn tool_calling_runner(
        tool_name: &str,
        arguments: String,
        ruleset: Ruleset,
    ) -> (ProviderRunner, Arc<std::sync::Mutex<Vec<ChatRequest>>>) {
        let requests_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = Registry::empty();
        registry.register(Box::new(ToolCallingProvider {
            requests_seen: requests_seen.clone(),
            tool_name: tool_name.to_string(),
            arguments,
        }));
        let runner = ProviderRunner {
            registry,
            provider_id: "toolcalling".into(),
            model: "toolcalling-model".into(),
            turn_deadline: DEFAULT_TURN_DEADLINE,
            ruleset,
            ledger: NonceLedger::new(),
            catalogue_root: None,
            workspace_root: std::env::temp_dir(),
            audit: std::sync::Arc::new(NullAuditSink),
            auto_route: false,
            compact_at_tokens: None,
            sticky_specialist: Default::default(),
            mode: Default::default(),
            router_model: None,
            role_models: Default::default(),
            team: None,
        };
        (runner, requests_seen)
    }

    /// End to end through a real `CoreHandle`: a tool call the ruleset
    /// defers (`Ask`) and a human approves actually runs the real
    /// `write` tool, and the second round trip carries the tool's
    /// result back to the model as a `Role::Tool` message.
    #[test]
    fn an_approved_tool_call_runs_the_real_tool_and_feeds_the_result_back() {
        let dir =
            std::env::temp_dir().join(format!("se-core-loop-approved-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("out.txt");
        let args =
            serde_json::json!({"path": file.to_string_lossy(), "content": "hello from the model"})
                .to_string();

        let (runner, requests_seen) = tool_calling_runner(
            "write",
            args,
            Ruleset::from_rules(vec![se_permission::Rule::allow_all()]),
        );
        let (outcome, _payloads) = run_auto_approving(runner, "please write the file", true);

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "hello from the model",
            "the real write tool must have run"
        );
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "acknowledged".into()
            }
        );
        let seen = requests_seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "one round trip per model/tool exchange");
        let second = &seen[1];
        assert!(
            second
                .messages
                .iter()
                .any(|m| m.role == Role::Assistant && !m.tool_calls.is_empty()),
            "the assistant's own tool-calls message must be replayed: {:?}",
            second.messages
        );
        assert!(
            second.messages.iter().any(|m| m.role == Role::Tool
                && m.tool_call_id.as_deref() == Some("call_1")
                && m.content.contains("wrote file")),
            "the tool's result must be fed back by the same call id: {:?}",
            second.messages
        );
    }

    /// A denial must not execute the tool, and the model must be told
    /// why -- the turn continues, it is not silently abandoned.
    #[test]
    fn a_denied_tool_call_never_runs_and_the_model_is_told_it_was_denied() {
        let dir = std::env::temp_dir().join(format!("se-core-loop-denied-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("should-not-exist.txt");
        let args =
            serde_json::json!({"path": file.to_string_lossy(), "content": "nope"}).to_string();

        // The default `Ruleset` resolves to `Ask` for everything, so
        // this call actually reaches the human approval gate rather
        // than being auto-allowed by policy before it gets there.
        let (runner, requests_seen) = tool_calling_runner("write", args, Ruleset::default());
        let (outcome, _payloads) = run_auto_approving(runner, "please write the file", false);

        assert!(
            !file.exists(),
            "a denied write must never touch the filesystem"
        );
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "acknowledged".into()
            },
            "the turn continues after a denial rather than stopping"
        );
        let seen = requests_seen.lock().unwrap();
        let second = &seen[1];
        assert!(
            second
                .messages
                .iter()
                .any(|m| m.role == Role::Tool && m.content.to_lowercase().contains("denied")),
            "the model must be told the call was denied: {:?}",
            second.messages
        );
    }

    /// Deny wins: a ruleset that denies this tool is never overridden
    /// by a human saying yes, because the human is never asked at all.
    #[test]
    fn a_ruleset_deny_overrides_a_human_who_always_says_yes() {
        let dir =
            std::env::temp_dir().join(format!("se-core-loop-ruleset-deny-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("should-not-exist.txt");
        let args =
            serde_json::json!({"path": file.to_string_lossy(), "content": "nope"}).to_string();

        let (runner, _requests_seen) = tool_calling_runner(
            "write",
            args,
            Ruleset::from_rules(vec![se_permission::Rule::deny_tool("write")]),
        );
        // `allow: true` on every approval -- the point is that this
        // never gets asked, so always saying yes must not matter.
        let (outcome, payloads) = run_auto_approving(runner, "please write the file", true);

        assert!(
            !file.exists(),
            "a ruleset denial must never touch the filesystem"
        );
        assert!(
            !payloads
                .iter()
                .any(|p| matches!(p, Payload::ApprovalRequired { .. })),
            "a ruleset deny must be final before a human is ever asked: {payloads:?}"
        );
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "acknowledged".into()
            }
        );
    }

    #[test]
    fn a_greeting_causes_exactly_one_model_request() {
        // The core request gate: no planner pre-flight, no classifier,
        // no specialist swarm. One turn, one request.
        let (runner, requests) = runner_with(
            vec![StreamEvent::TextDelta("Hello".into()), StreamEvent::Done],
            Duration::ZERO,
            None,
        );
        let (outcome, _p) = run(runner, "hi");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "a greeting must cost exactly one model request"
        );
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "Hello".into()
            }
        );
    }

    #[test]
    fn streamed_deltas_reach_the_interface_and_build_the_answer() {
        let (runner, _r) = runner_with(
            vec![
                StreamEvent::TextDelta("one ".into()),
                StreamEvent::TextDelta("two ".into()),
                StreamEvent::TextDelta("three".into()),
                StreamEvent::Done,
            ],
            Duration::ZERO,
            None,
        );
        let (outcome, payloads) = run(runner, "count");
        let streamed: String = payloads
            .iter()
            .filter_map(|p| match p {
                Payload::Delta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, "one two three");
        // Same invariant as the echo runner: the transcript and the
        // reported answer are the same bytes.
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "one two three".into()
            }
        );
    }

    #[test]
    fn reasoning_is_streamed_as_reasoning_not_as_the_answer() {
        let (runner, _r) = runner_with(
            vec![
                StreamEvent::ReasoningDelta("let me think".into()),
                StreamEvent::TextDelta("42".into()),
                StreamEvent::Done,
            ],
            Duration::ZERO,
            None,
        );
        let (outcome, payloads) = run(runner, "the answer?");
        let kinds: Vec<se_protocol::DeltaKind> = payloads
            .iter()
            .filter_map(|p| match p {
                Payload::Delta { kind, .. } => Some(*kind),
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                se_protocol::DeltaKind::Reasoning,
                se_protocol::DeltaKind::Text
            ]
        );
        // Thinking must not end up in the answer.
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "42".into()
            }
        );
    }

    #[test]
    fn provider_reported_usage_is_forwarded_and_labelled_as_theirs() {
        let (runner, _r) = runner_with(
            vec![
                StreamEvent::TextDelta("x".into()),
                StreamEvent::Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(1),
                    cached_input_tokens: Some(4),
                },
                StreamEvent::Done,
            ],
            Duration::ZERO,
            None,
        );
        let (_outcome, payloads) = run(runner, "hi");
        let usage = payloads
            .iter()
            .find_map(|p| match p {
                Payload::UsageUpdated(u) => Some(*u),
                _ => None,
            })
            .expect("usage forwarded");
        assert_eq!(usage.source, UsageSource::ProviderReported);
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.cached_input_tokens, Some(4));
    }

    #[test]
    fn a_provider_that_reports_no_usage_produces_no_invented_figures() {
        let (runner, _r) = runner_with(
            vec![StreamEvent::TextDelta("x".into()), StreamEvent::Done],
            Duration::ZERO,
            None,
        );
        let (_outcome, payloads) = run(runner, "hi");
        let any_usage = payloads
            .iter()
            .any(|p| matches!(p, Payload::UsageUpdated(_)));
        assert!(
            !any_usage,
            "silence from the provider must stay silence, not become a zero"
        );
    }

    #[test]
    fn time_to_first_token_is_measured_separately_from_generation() {
        let (runner, _r) = runner_with(
            vec![
                StreamEvent::TextDelta("slow".into()),
                StreamEvent::TextDelta(" answer".into()),
                StreamEvent::Done,
            ],
            Duration::from_millis(40),
            None,
        );
        let (_outcome, payloads) = run(runner, "hi");
        let Some(Payload::TaskFinished { timings, .. }) = payloads.last() else {
            panic!("expected a terminal event");
        };
        let ttft = timings
            .of(Stage::TimeToFirstToken)
            .expect("ttft must be recorded");
        assert!(
            ttft >= Duration::from_millis(30),
            "ttft should reflect the real wait: {ttft:?}"
        );
        // And it is not the whole turn: two frames were delayed, so
        // generation is longer than the first-token wait.
        let generation = timings.of(Stage::Generation).expect("generation recorded");
        assert!(
            generation > ttft,
            "generation {generation:?} must exceed ttft {ttft:?}"
        );
    }

    #[test]
    fn interrupting_a_stream_reports_cancelled_not_a_partial_answer() {
        let (runner, _r) = runner_with(
            (0..200)
                .map(|i| StreamEvent::TextDelta(format!("{i} ")))
                .collect(),
            Duration::from_millis(10),
            None,
        );
        let core = crate::start(runner);
        let task = core.submit("a long answer", vec![]).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        core.interrupt(task);

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            for event in core.drain_blocking(Duration::from_millis(20)) {
                if let Payload::TaskFinished { outcome, .. } = &event.payload {
                    assert_eq!(*outcome, Outcome::Cancelled);
                    core.shutdown();
                    return;
                }
            }
        }
        panic!("an interrupted stream must still finish");
    }

    #[test]
    fn a_provider_error_is_reported_verbatim_rather_than_as_an_empty_answer() {
        let (runner, _r) = runner_with(
            Vec::new(),
            Duration::ZERO,
            Some(ProviderError::Protocol(
                "nvidia returned HTTP 401: invalid api key".into(),
            )),
        );
        let (outcome, _p) = run(runner, "hi");
        match outcome {
            Outcome::Failed { detail } => {
                assert!(detail.contains("401"), "{detail}");
                assert!(detail.contains("invalid api key"), "{detail}");
            }
            other => panic!("expected a reported failure, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_provider_is_refused_before_a_turn_starts() {
        // `ProviderRunner` holds boxed trait objects and is not
        // `Debug`, so the result is matched rather than unwrapped.
        match ProviderRunner::new("not-a-provider", "m") {
            Err(ProviderError::NotConfigured(d)) => {
                assert!(d.contains("not-a-provider"), "{d}")
            }
            Err(other) => panic!("expected NotConfigured, got {other:?}"),
            Ok(_) => panic!("an unknown provider must be refused"),
        }
    }

    #[test]
    fn an_unconfigured_provider_is_refused_with_the_reason() {
        // `custom` has no default endpoint and no credential in a test
        // environment, so it must refuse rather than start a turn that
        // cannot possibly run.
        std::env::remove_var("SE_CUSTOM_API_KEY");
        std::env::remove_var("SE_CUSTOM_BASE_URL");
        match ProviderRunner::new("custom", "m") {
            Err(ProviderError::NotConfigured(d)) => assert!(d.contains("custom"), "{d}"),
            Err(other) => panic!("expected NotConfigured, got {other:?}"),
            Ok(_) => panic!("an unconfigured provider must be refused"),
        }
    }

    /// A conversation, not a sequence of amnesiac requests.
    ///
    /// The defect this covers: the runner used to send only the system
    /// prompt and the current message, so the second turn could not see
    /// the first -- while the phase table claimed a "real session".
    #[test]
    fn a_second_turn_sends_the_first_exchange_back_to_the_model() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let recorder = seen.clone();

        struct Recorder {
            seen: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        }
        impl ModelProvider for Recorder {
            fn id(&self) -> &str {
                "recorder"
            }
            fn display_name(&self) -> &str {
                "Recorder"
            }
            fn env_var(&self) -> &str {
                "SE_TEST_RECORDER_KEY"
            }
            fn detect(&self) -> Status {
                Status {
                    configured: true,
                    detail: "recorder".into(),
                    source: se_provider::Source::Environment,
                }
            }
            fn stream(
                &self,
                request: &ChatRequest,
                _cancelled: &dyn Fn() -> bool,
                on_event: &mut dyn FnMut(StreamEvent) -> bool,
            ) -> Result<(), ProviderError> {
                self.seen.lock().unwrap().push(
                    request
                        .messages
                        .iter()
                        .map(|m| format!("{:?}:{}", m.role, m.content))
                        .collect(),
                );
                on_event(StreamEvent::TextDelta("ok".into()));
                on_event(StreamEvent::Done);
                Ok(())
            }
            fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
                Ok(Vec::new())
            }
        }

        let mut registry = Registry::empty();
        registry.register(Box::new(Recorder { seen: recorder }));
        let runner = ProviderRunner {
            registry,
            provider_id: "recorder".into(),
            model: "m".into(),
            turn_deadline: DEFAULT_TURN_DEADLINE,
            ruleset: Ruleset::default(),
            ledger: NonceLedger::new(),
            catalogue_root: None,
            workspace_root: std::env::temp_dir(),
            audit: std::sync::Arc::new(NullAuditSink),
            auto_route: false,
            compact_at_tokens: None,
            sticky_specialist: Default::default(),
            mode: Default::default(),
            router_model: None,
            role_models: Default::default(),
            team: None,
        };

        let core = crate::start(runner);
        for request in ["first question", "second question"] {
            core.submit(request, vec![]).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut done = false;
            while !done && std::time::Instant::now() < deadline {
                for event in core.drain_blocking(Duration::from_millis(20)) {
                    if event.is_terminal() {
                        done = true;
                    }
                }
            }
            assert!(done, "{request} did not finish");
        }
        core.shutdown();

        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 2, "two turns, two requests");
        // The first turn carries system + the question only.
        assert_eq!(requests[0].len(), 2, "{:?}", requests[0]);
        // The second carries system + the whole first exchange + the
        // new question.
        assert_eq!(requests[1].len(), 4, "{:?}", requests[1]);
        assert!(
            requests[1][1].contains("first question"),
            "{:?}",
            requests[1]
        );
        assert!(
            requests[1][2].starts_with("Assistant:") && requests[1][2].contains("ok"),
            "the model's own previous answer must be included: {:?}",
            requests[1]
        );
        assert!(
            requests[1][3].contains("second question"),
            "{:?}",
            requests[1]
        );
    }

    #[test]
    fn the_turn_declares_the_provider_and_model_that_actually_ran() {
        let (runner, _r) = runner_with(
            vec![StreamEvent::TextDelta("x".into()), StreamEvent::Done],
            Duration::ZERO,
            None,
        );
        let (_outcome, payloads) = run(runner, "hi");
        let started = payloads
            .iter()
            .find_map(|p| match p {
                Payload::TurnStarted { provider, model } => Some((provider.clone(), model.clone())),
                _ => None,
            })
            .expect("the turn must say what ran it");
        assert_eq!(
            started,
            ("scripted".to_string(), "scripted-model".to_string())
        );
    }

    #[test]
    fn a_turn_that_outruns_its_deadline_is_incomplete_not_completed() {
        let (runner, _r) = runner_that_never_stops(Duration::from_millis(5));
        let runner = runner
            .with_turn_deadline(Duration::from_millis(80))
            .expect("a positive deadline is accepted");
        let (outcome, _p) = run(runner, "stream forever");
        match outcome {
            Outcome::Incomplete {
                partial_text,
                reason,
            } => {
                assert!(
                    !partial_text.is_empty(),
                    "the text that did arrive must be preserved"
                );
                assert!(reason.contains("turn"), "reason was {reason:?}");
            }
            other => panic!("a deadline must not report success: {other:?}"),
        }
    }

    #[test]
    fn a_deadline_that_fires_before_any_token_is_a_failure_not_a_partial_answer() {
        // One frame per 50ms against a 1ms deadline: the deadline is
        // seen on the first poll, before anything is streamed.
        let (runner, _r) = runner_that_never_stops(Duration::from_millis(50));
        let runner = runner
            .with_turn_deadline(Duration::from_millis(1))
            .expect("a positive deadline is accepted");
        let (outcome, _p) = run(runner, "stream forever");
        match outcome {
            Outcome::Failed { detail } => assert!(detail.contains("turn"), "detail was {detail:?}"),
            other => panic!("an empty timed-out turn is a failure: {other:?}"),
        }
    }

    // A timing-based integration test for the exact simultaneous race
    // (deadline_fired and is_cancelled both true at the outcome
    // decision) was attempted and removed: the window between
    // `stream()` returning and the decision reading `is_cancelled()` is
    // one synchronous code path, not a sleep-measurable interval, and
    // every attempt to force it via `thread::sleep` either missed the
    // window (cancellation too early, correctly reported as Cancelled
    // by the ordinary `Err(Cancelled)` match arm) or arrived after the
    // task had already finished from the deadline alone, which only
    // proves that interrupting a finished task is a no-op -- a true
    // but uninteresting property. `deadline_precedence_truth_table`
    // above is the complete, deterministic proof of this property.

    #[test]
    fn a_zero_deadline_is_refused_at_construction() {
        let (runner, _r) = runner_with(vec![StreamEvent::Done], Duration::ZERO, None);
        assert!(runner.with_turn_deadline(Duration::ZERO).is_err());
    }

    #[test]
    fn an_answer_that_started_and_then_failed_is_incomplete_not_failed() {
        let (runner, _r) = runner_failing_after(
            vec![StreamEvent::TextDelta("half an ans".into())],
            ProviderError::Transport("the connection dropped".into()),
        );
        let (outcome, _p) = run(runner, "hi");
        match outcome {
            Outcome::Incomplete {
                partial_text,
                reason,
            } => {
                assert_eq!(partial_text, "half an ans");
                assert!(reason.contains("connection dropped"), "reason {reason:?}");
            }
            other => panic!("a partial answer must survive the failure: {other:?}"),
        }
    }

    #[test]
    fn a_failure_before_any_token_stays_a_failure() {
        let (runner, _r) = runner_failing_after(
            Vec::new(),
            ProviderError::Transport("the connection dropped".into()),
        );
        let (outcome, _p) = run(runner, "hi");
        match outcome {
            Outcome::Failed { detail } => {
                assert!(detail.contains("connection dropped"), "detail {detail:?}")
            }
            other => panic!("nothing arrived, so this is a failure: {other:?}"),
        }
    }

    // --- Phase 3 items 3-4: specialist invocation and provenance ---

    /// Writes one minimal valid agent definition to a fresh temp
    /// catalogue root and returns the root. Mirrors the fixture shape
    /// `se-catalogue`'s own tests already use.
    fn temp_catalogue(agent_id: &str, body: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "se-core-specialist-{agent_id}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dept = root.join("dept");
        std::fs::create_dir_all(&dept).unwrap();
        std::fs::write(
            dept.join(format!("{agent_id}.md")),
            format!(
                "---\nschema_version: 1\nid: {agent_id}\nname: Test Specialist\ndepartment: dept\n---\n\n{body}\n"
            ),
        )
        .unwrap();
        root
    }

    /// Like [`temp_catalogue`], but the agent's frontmatter declares a
    /// `tools:` allowlist -- for proving Phase 4 item 4's "intersect
    /// with a ceiling, never union".
    fn temp_catalogue_with_tools(agent_id: &str, body: &str, tools: &[&str]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "se-core-specialist-tools-{agent_id}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dept = root.join("dept");
        std::fs::create_dir_all(&dept).unwrap();
        let tools_yaml = tools
            .iter()
            .map(|t| format!("  - {t}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            dept.join(format!("{agent_id}.md")),
            format!(
                "---\nschema_version: 1\nid: {agent_id}\nname: Test Specialist\ndepartment: dept\ntools:\n{tools_yaml}\n---\n\n{body}\n"
            ),
        )
        .unwrap();
        root
    }

    fn run_specialist(runner: ProviderRunner, id: &str, text: &str) -> (Outcome, Vec<Payload>) {
        let core = crate::start(runner);
        core.submit_specialist(id, text, vec![]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut payloads = Vec::new();
        while std::time::Instant::now() < deadline {
            for event in core.drain_blocking(Duration::from_millis(20)) {
                let terminal = event.is_terminal();
                payloads.push(event.payload.clone());
                if terminal {
                    core.shutdown();
                    let Payload::TaskFinished { outcome, .. } = event.payload else {
                        unreachable!()
                    };
                    return (outcome, payloads);
                }
            }
        }
        panic!("the turn never finished");
    }

    #[test]
    fn a_specialist_invocation_uses_the_agents_body_as_the_system_prompt() {
        let root = temp_catalogue("test-specialist", "YOU ARE THE TEST SPECIALIST.");
        let (mut runner, _r) = runner_with(
            vec![StreamEvent::TextDelta("hi".into()), StreamEvent::Done],
            Duration::ZERO,
            None,
        );
        runner = runner.with_catalogue_root(root.clone());

        // `runner_with` builds a `ScriptedProvider`, which does not
        // record requests; reuse `scripted_runner`'s sibling pattern by
        // asserting on the *payload* side instead -- `SpecialistInvoked`
        // itself carries the one fact that matters here (which agent,
        // which hash), and a second test below proves the body text
        // actually reaches the provider.
        let (outcome, payloads) = run_specialist(runner, "test-specialist", "hello");
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "hi".into()
            }
        );
        let invoked = payloads.iter().find_map(|p| match p {
            Payload::SpecialistInvoked {
                agent_id,
                content_hash,
            } => Some((agent_id.clone(), *content_hash)),
            _ => None,
        });
        assert!(
            invoked.is_some(),
            "a specialist-routed turn must emit SpecialistInvoked: {payloads:?}"
        );
        assert_eq!(invoked.unwrap().0, "test-specialist");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_resolved_specialist_body_is_what_the_provider_actually_receives() {
        let root = temp_catalogue("recorder-specialist", "SPECIALIST SYSTEM PROMPT MARKER");
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let recorder = seen.clone();

        struct Recorder {
            seen: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        }
        impl ModelProvider for Recorder {
            fn id(&self) -> &str {
                "recorder2"
            }
            fn display_name(&self) -> &str {
                "Recorder2"
            }
            fn env_var(&self) -> &str {
                "SE_TEST_RECORDER2_KEY"
            }
            fn detect(&self) -> Status {
                Status {
                    configured: true,
                    detail: "recorder2".into(),
                    source: se_provider::Source::Environment,
                }
            }
            fn stream(
                &self,
                request: &ChatRequest,
                _cancelled: &dyn Fn() -> bool,
                on_event: &mut dyn FnMut(StreamEvent) -> bool,
            ) -> Result<(), ProviderError> {
                self.seen
                    .lock()
                    .unwrap()
                    .push(request.messages.iter().map(|m| m.content.clone()).collect());
                on_event(StreamEvent::TextDelta("ok".into()));
                on_event(StreamEvent::Done);
                Ok(())
            }
            fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
                Ok(Vec::new())
            }
        }

        let mut registry = Registry::empty();
        registry.register(Box::new(Recorder { seen: recorder }));
        let runner = ProviderRunner {
            registry,
            provider_id: "recorder2".into(),
            model: "m".into(),
            turn_deadline: DEFAULT_TURN_DEADLINE,
            ruleset: Ruleset::default(),
            ledger: NonceLedger::new(),
            catalogue_root: Some(root.clone()),
            workspace_root: std::env::temp_dir(),
            audit: std::sync::Arc::new(NullAuditSink),
            auto_route: false,
            compact_at_tokens: None,
            sticky_specialist: Default::default(),
            mode: Default::default(),
            router_model: None,
            role_models: Default::default(),
            team: None,
        };

        let (outcome, _p) = run_specialist(runner, "recorder-specialist", "do the thing");
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "ok".into()
            }
        );
        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            role_body(&requests[0][0]).trim(), "SPECIALIST SYSTEM PROMPT MARKER",
            "the provider's first message must be the agent's own body, not the generic system prompt"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unknown_specialist_id_fails_without_ever_calling_the_provider() {
        let root = temp_catalogue("real-one", "irrelevant body");
        let (mut runner, requests) = runner_with(
            vec![StreamEvent::TextDelta("hi".into()), StreamEvent::Done],
            Duration::ZERO,
            None,
        );
        runner = runner.with_catalogue_root(root.clone());

        let (outcome, _p) = run_specialist(runner, "no-such-specialist", "hello");
        match outcome {
            Outcome::Failed { detail } => {
                assert!(detail.contains("no-such-specialist"), "{detail}")
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the provider must never be called for an unresolvable specialist"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_catalogue_root_fails_a_specialist_turn_without_guessing_a_path() {
        let (runner, requests) = runner_with(
            vec![StreamEvent::TextDelta("hi".into()), StreamEvent::Done],
            Duration::ZERO,
            None,
        );
        // No `.with_catalogue_root(...)` call at all.
        let (outcome, _p) = run_specialist(runner, "anything", "hello");
        match outcome {
            Outcome::Failed { detail } => assert!(detail.contains("catalogue"), "{detail}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// Proves "pinned for the run, never re-read mid-run": across two
    /// tool-call round trips inside one specialist-invoked turn (forced
    /// by a ruleset denial, which the existing loop already makes the
    /// model see and respond to), the exact same system-prompt text
    /// reaches the provider both times -- read from the catalogue
    /// exactly once, before the loop started, not re-read per round.
    #[test]
    fn the_specialist_system_prompt_is_identical_across_every_round_trip_of_one_turn() {
        let root = temp_catalogue("pinned-specialist", "PINNED SYSTEM PROMPT");
        let args = serde_json::json!({"path": "irrelevant.txt", "content": "x"}).to_string();
        let (mut runner, requests_seen) = tool_calling_runner(
            "write",
            args,
            Ruleset::from_rules(vec![se_permission::Rule::deny_tool("write")]),
        );
        runner = runner.with_catalogue_root(root.clone());

        let core = crate::start(runner);
        core.submit_specialist("pinned-specialist", "please write", vec![])
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if std::time::Instant::now() >= deadline {
                panic!("the turn never finished");
            }
            let events = core.drain_blocking(Duration::from_millis(20));
            if events.iter().any(|e| e.is_terminal()) {
                core.shutdown();
                break;
            }
        }

        let seen = requests_seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "a denial forces a second round trip");
        let first_system = seen[0].messages[0].content.clone();
        let second_system = seen[1].messages[0].content.clone();
        assert_eq!(role_body(&first_system).trim(), "PINNED SYSTEM PROMPT");
        assert_eq!(
            first_system, second_system,
            "the system prompt must be identical across round trips, not re-resolved"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Phase 4 item 4: a specialist's own `tools:` allowlist narrows
    /// what its calls may do, even against a ruleset that would
    /// otherwise allow everything -- the allowlist can only shrink the
    /// grant, never widen it (intersect, never union).
    #[test]
    fn a_specialists_tool_allowlist_denies_a_call_outside_it_even_under_an_allow_all_ruleset() {
        let root = temp_catalogue_with_tools("narrow-specialist", "YOU MAY ONLY READ.", &["read"]);
        let dir =
            std::env::temp_dir().join(format!("se-core-specialist-narrow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("out.txt");
        let args =
            serde_json::json!({"path": file.to_string_lossy(), "content": "should never land"})
                .to_string();

        let (mut runner, _requests_seen) = tool_calling_runner(
            "write",
            args,
            Ruleset::from_rules(vec![se_permission::Rule::allow_all()]),
        );
        runner = runner.with_catalogue_root(root.clone());

        let (outcome, payloads) = run_specialist(runner, "narrow-specialist", "please write");

        assert!(
            !file.exists(),
            "the specialist's own allowlist must have denied the write before it ran"
        );
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "acknowledged".into()
            },
            "the model sees the denial as a tool result, not a failed turn"
        );
        let saw_denial = payloads.iter().any(|p| match p {
            Payload::ToolCompleted { ok, preview, .. } => {
                !ok && preview.contains("is not one of your tools")
            }
            _ => false,
        });
        assert!(
            saw_denial,
            "the denial must be reported on the real tool-call lifecycle: {payloads:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Row 2.5: every tool call leaves exactly one final gateway
    /// decision (Allow or Deny) in the durable audit file, asserted by
    /// count across all four paths: ruleset allow, human approve, human
    /// deny, ruleset deny. A human "no" used to leave no record at all.
    #[test]
    fn every_tool_call_has_exactly_one_final_decision_in_the_audit_file() {
        let dir = std::env::temp_dir().join(format!("se-core-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let audit_path = dir.join("audit.jsonl");
        let sink: Arc<dyn se_permission::AuditSink + Send + Sync> =
            Arc::new(crate::audit_log::FileAuditSink::open(&audit_path).unwrap());

        let cases = [
            (
                "ruleset-allow",
                Ruleset::from_rules(vec![se_permission::Rule::allow_all()]),
                true,
            ),
            ("human-approve", Ruleset::default(), true),
            ("human-deny", Ruleset::default(), false),
            (
                "ruleset-deny",
                Ruleset::from_rules(vec![se_permission::Rule::deny_tool("write")]),
                true,
            ),
        ];
        for (name, ruleset, human_says) in cases {
            let file = dir.join(format!("{name}.txt"));
            let args =
                serde_json::json!({"path": file.to_string_lossy(), "content": name}).to_string();
            let (runner, _) = tool_calling_runner("write", args, ruleset);
            let runner = runner.with_audit_sink(sink.clone());
            run_auto_approving(runner, name, human_says);
            let ran = file.exists();
            assert_eq!(
                ran,
                name == "ruleset-allow" || name == "human-approve",
                "{name}"
            );
        }

        let records = crate::audit_log::read(&audit_path).unwrap();
        let finals: Vec<&str> = records
            .iter()
            .map(|r| r["effect"].as_str().unwrap())
            .filter(|e| *e != "Ask")
            .collect();
        assert_eq!(finals, ["Allow", "Allow", "Deny", "Deny"], "{records:?}");
        let asks = records.iter().filter(|r| r["effect"] == "Ask").count();
        assert_eq!(asks, 2, "only the two human paths ask");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Answers "from <id>" and records the model each request named.
    struct NamedProvider {
        id: &'static str,
        configured: bool,
        models_seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl ModelProvider for NamedProvider {
        fn id(&self) -> &str {
            self.id
        }
        fn display_name(&self) -> &str {
            self.id
        }
        fn env_var(&self) -> &str {
            "SE_TEST_NAMED_KEY"
        }
        fn detect(&self) -> Status {
            Status {
                configured: self.configured,
                detail: "named".into(),
                source: se_provider::Source::Environment,
            }
        }
        fn stream(
            &self,
            request: &ChatRequest,
            _cancelled: &dyn Fn() -> bool,
            on_event: &mut dyn FnMut(StreamEvent) -> bool,
        ) -> Result<(), ProviderError> {
            self.models_seen.lock().unwrap().push(request.model.clone());
            on_event(StreamEvent::TextDelta(format!("from {}", self.id)));
            on_event(StreamEvent::Done);
            Ok(())
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    type Seen = Arc<std::sync::Mutex<Vec<String>>>;

    /// Default provider `alpha`; `beta` is configured, `gamma` is not.
    fn two_provider_runner() -> (ProviderRunner, Seen, Seen, Seen) {
        let (a, b, g) = (Seen::default(), Seen::default(), Seen::default());
        let mut registry = Registry::empty();
        for (id, configured, seen) in [
            ("alpha", true, &a),
            ("beta", true, &b),
            ("gamma", false, &g),
        ] {
            registry.register(Box::new(NamedProvider {
                id,
                configured,
                models_seen: seen.clone(),
            }));
        }
        let (mut runner, _) = runner_with(Vec::new(), Duration::ZERO, None);
        runner.registry = registry;
        runner.provider_id = "alpha".into();
        runner.model = "alpha-model".into();
        (runner, a, b, g)
    }

    fn run_with_model(runner: ProviderRunner, model: Option<se_protocol::ModelRef>) -> Outcome {
        let core = crate::start(runner);
        core.submit_with_model("hi", vec![], model).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            for event in core.drain_blocking(Duration::from_millis(20)) {
                if let Payload::TaskFinished { outcome, .. } = event.payload {
                    core.shutdown();
                    return outcome;
                }
            }
        }
        panic!("no terminal event");
    }

    fn model_ref(provider: &str, model: &str) -> se_protocol::ModelRef {
        se_protocol::ModelRef {
            provider: provider.into(),
            model: model.into(),
        }
    }

    /// 2a item 6: a per-turn override actually changes which provider
    /// and model execute, rather than being accepted and ignored.
    #[test]
    fn a_model_override_routes_the_turn_to_that_provider_and_model() {
        let (runner, alpha, beta, _) = two_provider_runner();
        let outcome = run_with_model(runner, Some(model_ref("beta", "beta-large")));
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "from beta".into()
            }
        );
        assert_eq!(*beta.lock().unwrap(), ["beta-large"]);
        assert!(
            alpha.lock().unwrap().is_empty(),
            "the default must not be called"
        );
    }

    #[test]
    fn no_override_uses_the_runners_default() {
        let (runner, alpha, beta, _) = two_provider_runner();
        let outcome = run_with_model(runner, None);
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "from alpha".into()
            }
        );
        assert_eq!(*alpha.lock().unwrap(), ["alpha-model"]);
        assert!(beta.lock().unwrap().is_empty());
    }

    #[test]
    fn an_unknown_or_unconfigured_override_fails_without_calling_anything() {
        for (provider, expect) in [
            ("nope", "not a known provider"),
            ("gamma", "not configured"),
        ] {
            let (runner, alpha, beta, gamma) = two_provider_runner();
            let outcome = run_with_model(runner, Some(model_ref(provider, "m")));
            match outcome {
                Outcome::Failed { detail } => assert!(detail.contains(expect), "{detail}"),
                other => panic!("{provider}: expected failure, got {other:?}"),
            }
            for seen in [alpha, beta, gamma] {
                assert!(
                    seen.lock().unwrap().is_empty(),
                    "{provider}: nothing may be called"
                );
            }
        }
    }

    /// Round 1 asks for two writes; round 2 answers with text.
    struct TwoWritesProvider {
        dir: std::path::PathBuf,
        rounds: AtomicUsize,
    }

    impl ModelProvider for TwoWritesProvider {
        fn id(&self) -> &str {
            "twowrites"
        }
        fn display_name(&self) -> &str {
            "twowrites"
        }
        fn env_var(&self) -> &str {
            "SE_TEST_TWOWRITES_KEY"
        }
        fn detect(&self) -> Status {
            Status {
                configured: true,
                detail: "t".into(),
                source: se_provider::Source::Environment,
            }
        }
        fn stream(
            &self,
            _request: &ChatRequest,
            _cancelled: &dyn Fn() -> bool,
            on_event: &mut dyn FnMut(StreamEvent) -> bool,
        ) -> Result<(), ProviderError> {
            if self.rounds.fetch_add(1, Ordering::SeqCst) == 0 {
                for n in 1..=2 {
                    let path = self.dir.join(format!("{n}.txt"));
                    on_event(StreamEvent::ToolCallRequested {
                        id: format!("call_{n}"),
                        name: "write".into(),
                        arguments:
                            serde_json::json!({"path": path.to_string_lossy(), "content": "x"})
                                .to_string(),
                    });
                }
                on_event(StreamEvent::FinishReason("tool_calls".into()));
            } else {
                on_event(StreamEvent::TextDelta("done".into()));
            }
            on_event(StreamEvent::Done);
            Ok(())
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    /// Row 3.9: the specialist's own `max_calls` from the catalogue
    /// bounds how many tool calls its turn runs.
    #[test]
    fn a_specialists_max_calls_stops_tool_calls_past_the_cap() {
        let root = std::env::temp_dir().join(format!("se-core-maxcalls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("dept")).unwrap();
        std::fs::write(
            root.join("dept/capped.md"),
            "---\nschema_version: 1\nid: capped\nname: Capped\ndepartment: dept\nmax_calls: 1\n---\n\nbody\n",
        )
        .unwrap();
        let out = root.join("out");
        std::fs::create_dir_all(&out).unwrap();

        let (mut runner, _) = runner_with(Vec::new(), Duration::ZERO, None);
        let mut registry = Registry::empty();
        registry.register(Box::new(TwoWritesProvider {
            dir: out.clone(),
            rounds: AtomicUsize::new(0),
        }));
        runner.registry = registry;
        runner.provider_id = "twowrites".into();
        runner.ruleset = Ruleset::from_rules(vec![se_permission::Rule::allow_all()]);
        let runner = runner.with_catalogue_root(root.clone());

        let (outcome, _) = run_specialist(runner, "capped", "write two");
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "done".into()
            }
        );
        assert!(
            out.join("1.txt").exists(),
            "the first call is within budget"
        );
        assert!(
            !out.join("2.txt").exists(),
            "the second call exceeds max_calls: 1"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fit_history_keeps_the_newest_whole_exchanges_that_fit() {
        let h = |role, n: usize| Message::text(role, "x".repeat(n));
        let history = vec![
            h(Role::User, 400),
            h(Role::Assistant, 400),
            h(Role::User, 40),
            h(Role::Assistant, 40),
        ];
        assert_eq!(
            fit_history(&history, None, 0).len(),
            4,
            "no budget, no trim"
        );
        // 100 tokens ~ 400 chars: only the newest exchange (80 chars) fits.
        assert_eq!(fit_history(&history, Some(100), 0), &history[2..]);
        // Room for the last answer alone must not keep it without its question.
        assert!(fit_history(&history, Some(15), 0).is_empty());
        // Fixed system prompt + request is charged against the budget.
        assert!(fit_history(&history, Some(100), 400).is_empty());
    }

    /// Runs a specialist turn with a model override, answering every
    /// approval with `allow`. Returns the outcome and approvals asked.
    fn run_specialist_with_model(
        runner: ProviderRunner,
        model: se_protocol::ModelRef,
        allow: bool,
    ) -> (Outcome, usize) {
        let core = crate::start(runner);
        core.submit_specialist_with_model("spec", "hi", vec![], Some(model))
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut asked = 0;
        while std::time::Instant::now() < deadline {
            for event in core.drain_blocking(Duration::from_millis(20)) {
                match event.payload {
                    Payload::ApprovalRequired { call, .. } => {
                        asked += 1;
                        core.approve(se_protocol::ApprovalDecision {
                            task: event.task,
                            call,
                            allow,
                        });
                    }
                    Payload::TaskFinished { outcome, .. } => {
                        core.shutdown();
                        return (outcome, asked);
                    }
                    _ => {}
                }
            }
        }
        panic!("no terminal event");
    }

    fn scoped_runner(ruleset: Ruleset) -> (ProviderRunner, Seen, Seen, std::path::PathBuf) {
        let (runner, alpha, beta, _) = two_provider_runner();
        let root = temp_catalogue("spec", "body");
        let mut runner = runner.with_catalogue_root(root.clone());
        runner.ruleset = ruleset;
        (runner, alpha, beta, root)
    }

    /// Phase 4 item 2, interactive case: a model the user picked
    /// (`/models`) is their consent, so a specialist turn on it is not
    /// put back to them as a question -- it still passes the gateway (see
    /// the deny test below). Scheduled team work is covered separately.
    #[test]
    fn a_users_own_model_pick_is_not_asked_again() {
        let (runner, alpha, beta, root) = scoped_runner(Ruleset::default());
        let (outcome, asked) =
            run_specialist_with_model(runner, model_ref("beta", "beta-large"), false);
        assert_eq!(asked, 0, "the user already chose this model");
        assert!(alpha.lock().unwrap().is_empty());
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "from beta".into()
            }
        );
        assert_eq!(*beta.lock().unwrap(), ["beta-large"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_ruleset_deny_blocks_a_specialists_provider_without_asking() {
        let (runner, _alpha, beta, root) =
            scoped_runner(Ruleset::from_rules(vec![se_permission::Rule::deny_tool(
                "use_provider",
            )]));
        let (outcome, asked) = run_specialist_with_model(runner, model_ref("beta", "m"), true);
        assert_eq!(asked, 0, "a denial never reaches a human");
        assert!(matches!(outcome, Outcome::Failed { .. }), "{outcome:?}");
        assert!(beta.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Tool calls made during a specialist turn are decided and audited
    /// under that specialist's id, not a shared native-loop identity.
    #[test]
    fn a_specialists_tool_calls_are_audited_under_its_own_id() {
        let root = temp_catalogue("spec", "body");
        let dir = std::env::temp_dir().join(format!("se-core-agent-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let audit_path = dir.join("audit.jsonl");
        let args = serde_json::json!({"path": dir.join("f.txt").to_string_lossy(), "content": "c"})
            .to_string();
        let (runner, _) = tool_calling_runner(
            "write",
            args,
            Ruleset::from_rules(vec![se_permission::Rule::allow_all()]),
        );
        let runner = runner
            .with_catalogue_root(root.clone())
            .with_audit_sink(Arc::new(
                crate::audit_log::FileAuditSink::open(&audit_path).unwrap(),
            ));
        run_specialist(runner, "spec", "write it");
        let records = crate::audit_log::read(&audit_path).unwrap();
        assert!(!records.is_empty());
        assert!(records.iter().all(|r| r["agent"] == "spec"), "{records:?}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A call refused before the gateway (here: a path escaping the
    /// workspace) still shows on the tool lifecycle and is audited.
    #[test]
    fn a_rejected_escape_attempt_is_visible_and_audited() {
        let dir = std::env::temp_dir().join(format!("se-core-escape-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        let audit_path = dir.join("audit.jsonl");
        let args = serde_json::json!({"path": "../escaped.txt", "content": "x"}).to_string();
        let (runner, _) = tool_calling_runner(
            "write",
            args,
            Ruleset::from_rules(vec![se_permission::Rule::allow_all()]),
        );
        let runner = runner
            .with_workspace_root(dir.join("ws"))
            .with_audit_sink(Arc::new(
                crate::audit_log::FileAuditSink::open(&audit_path).unwrap(),
            ));
        let (_, payloads) = run_auto_approving(runner, "escape", true);

        assert!(!dir.join("escaped.txt").exists());
        assert!(payloads.iter().any(|p| matches!(
            p,
            Payload::ToolCompleted { ok: false, preview, .. } if preview.contains("outside the workspace")
        )), "{payloads:?}");
        let records = crate::audit_log::read(&audit_path).unwrap();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0]["effect"], "Deny");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Answers the two routing questions with fixed replies, and any
    /// other request with "answered", recording each request's system
    /// prompt.
    struct RoutingProvider {
        dept_reply: &'static str,
        role_reply: &'static str,
        systems: Arc<std::sync::Mutex<Vec<String>>>,
        /// Routing questions asked so far.
        asked: Arc<AtomicUsize>,
    }

    impl ModelProvider for RoutingProvider {
        fn id(&self) -> &str {
            "routing"
        }
        fn display_name(&self) -> &str {
            "routing"
        }
        fn env_var(&self) -> &str {
            "SE_TEST_ROUTING_KEY"
        }
        fn detect(&self) -> Status {
            Status {
                configured: true,
                detail: "r".into(),
                source: se_provider::Source::Environment,
            }
        }
        fn stream(
            &self,
            request: &ChatRequest,
            _cancelled: &dyn Fn() -> bool,
            on_event: &mut dyn FnMut(StreamEvent) -> bool,
        ) -> Result<(), ProviderError> {
            let user = &request.messages.last().unwrap().content;
            if user.contains("Which department") || user.contains("Which specialist") {
                self.asked.fetch_add(1, Ordering::SeqCst);
            }
            let reply = if user.contains("Which department") {
                self.dept_reply
            } else if user.contains("Which specialist") {
                self.role_reply
            } else {
                self.systems
                    .lock()
                    .unwrap()
                    .push(request.messages[0].content.clone());
                "answered"
            };
            on_event(StreamEvent::TextDelta(reply.into()));
            on_event(StreamEvent::Done);
            Ok(())
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    fn routing_catalogue() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "se-core-router-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for (dept, id, activation, body) in [
            (
                "backend-and-data",
                "database-engineer",
                "database schema migration",
                "DB BODY",
            ),
            (
                "backend-and-data",
                "backend-lead",
                "service endpoint",
                "BACKEND BODY",
            ),
            (
                "frontend",
                "frontend-lead",
                "react component screen",
                "FRONTEND BODY",
            ),
            ("orchestration", FALLBACK_AGENT_ID, "always", "ORCH BODY"),
        ] {
            std::fs::create_dir_all(root.join(dept)).unwrap();
            std::fs::write(
                root.join(dept).join(format!("{id}.md")),
                format!(
                    "---\nschema_version: 1\nid: {id}\nname: {id}\ndepartment: {dept}\nenabled: true\nmission: works on {activation}\nactivation:\n  - {activation}\n---\n\n{body}\n"
                ),
            )
            .unwrap();
        }
        root
    }

    fn run_routed(
        dept_reply: &'static str,
        role_reply: &'static str,
        task: &str,
        auto: bool,
    ) -> (Vec<String>, Vec<Payload>) {
        let systems = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut runner, _) = runner_with(Vec::new(), Duration::ZERO, None);
        let mut registry = Registry::empty();
        registry.register(Box::new(RoutingProvider {
            dept_reply,
            role_reply,
            systems: systems.clone(),
            asked: Arc::new(AtomicUsize::new(0)),
        }));
        runner.registry = registry;
        runner.provider_id = "routing".into();
        let root = routing_catalogue();
        let runner = runner
            .with_catalogue_root(root.clone())
            .with_auto_route(auto);
        let (outcome, payloads) = run(runner, task);
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "answered".into()
            }
        );
        let _ = std::fs::remove_dir_all(&root);
        let seen = systems.lock().unwrap().clone();
        (seen, payloads)
    }

    fn invoked(payloads: &[Payload]) -> Option<String> {
        payloads.iter().find_map(|p| match p {
            Payload::SpecialistInvoked { agent_id, .. } => Some(agent_id.clone()),
            _ => None,
        })
    }

    /// A turn naming no specialist is routed to one automatically, and
    /// that specialist's own definition is the system prompt.
    #[test]
    fn an_unnamed_turn_is_routed_to_a_specialist() {
        let (systems, payloads) = run_routed(
            "backend-and-data",
            "I'd pick database-engineer.",
            "add an orders table",
            true,
        );
        assert_eq!(invoked(&payloads).as_deref(), Some("database-engineer"));
        assert_eq!(role_body(&systems[0]).trim(), "DB BODY");
    }

    /// A small model's useless answers route to the fallback, never to a keyword guess.
    #[test]
    fn bad_routing_answers_go_to_the_fallback_not_a_keyword_guess() {
        let (systems, payloads) = run_routed(
            "no idea",
            "",
            "build a react component for the settings screen",
            true,
        );
        assert_eq!(invoked(&payloads).as_deref(), Some(FALLBACK_AGENT_ID));
        assert_eq!(role_body(systems.last().unwrap()).trim(), "ORCH BODY");
    }

    /// Nothing matches anywhere: the turn still runs as an SE specialist
    /// (the fallback), never as a bare model with no role.
    #[test]
    fn an_unroutable_turn_runs_as_the_fallback_specialist() {
        let (systems, payloads) = run_routed("??", "??", "hello there", true);
        assert_eq!(invoked(&payloads).as_deref(), Some(FALLBACK_AGENT_ID));
        assert_eq!(role_body(systems.last().unwrap()).trim(), "ORCH BODY");
    }

    /// Forced routing: an unreadable catalogue fails the turn before any
    /// model call, rather than quietly running it with no specialist.
    #[test]
    fn an_unreadable_catalogue_fails_the_turn_instead_of_running_unrouted() {
        let (runner, requests) = runner_with(Vec::new(), Duration::ZERO, None);
        let runner = runner
            .with_catalogue_root(std::env::temp_dir().join("se-no-such-catalogue-xyz"))
            .with_auto_route(true);
        let (outcome, _) = run(runner, "add an orders table");
        match outcome {
            Outcome::Failed { detail } => assert!(detail.contains("catalogue"), "{detail}"),
            other => panic!("expected a failure, got {other:?}"),
        }
        assert_eq!(requests.load(Ordering::SeqCst), 0, "no model was called");
    }

    #[test]
    fn with_auto_route_off_nothing_is_routed() {
        let (systems, payloads) = run_routed(
            "backend-and-data",
            "database-engineer",
            "add an orders table",
            false,
        );
        assert_eq!(invoked(&payloads), None);
        assert_eq!(systems.len(), 1, "no routing questions were asked");
    }

    #[test]
    fn compaction_waits_until_history_is_over_the_limit() {
        let h = |role, n: usize| Message::text(role, "x".repeat(n));
        let small = vec![h(Role::User, 40), h(Role::Assistant, 40)];
        assert_eq!(compaction_split(&small, 100), None);
        let big: Vec<Message> = (0..6)
            .map(|i| {
                h(
                    if i % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    400,
                )
            })
            .collect();
        // Keeps the last two exchanges verbatim and cuts on a user turn.
        assert_eq!(compaction_split(&big, 100), Some(2));
        assert_eq!(big[2].role, Role::User);
        // Only huge recent turns: the older part is too small to summarize.
        let recent_heavy = vec![
            h(Role::User, 10),
            h(Role::Assistant, 10),
            h(Role::User, 4000),
            h(Role::Assistant, 4000),
            h(Role::User, 4000),
            h(Role::Assistant, 4000),
        ];
        assert_eq!(compaction_split(&recent_heavy, 1000), None);
    }

    /// Records every request; answers the compaction prompt with a summary.
    struct MemoryProvider {
        requests: Arc<std::sync::Mutex<Vec<Vec<Message>>>>,
    }

    impl ModelProvider for MemoryProvider {
        fn id(&self) -> &str {
            "memory"
        }
        fn display_name(&self) -> &str {
            "memory"
        }
        fn env_var(&self) -> &str {
            "SE_TEST_MEMORY_KEY"
        }
        fn detect(&self) -> Status {
            Status {
                configured: true,
                detail: "m".into(),
                source: se_provider::Source::Environment,
            }
        }
        fn stream(
            &self,
            request: &ChatRequest,
            _cancelled: &dyn Fn() -> bool,
            on_event: &mut dyn FnMut(StreamEvent) -> bool,
        ) -> Result<(), ProviderError> {
            self.requests.lock().unwrap().push(request.messages.clone());
            let reply = if request.messages[0].content == COMPACT_SYSTEM {
                "SUMMARY: user is building an orders table in PostgreSQL."
            } else {
                "answered"
            };
            on_event(StreamEvent::TextDelta(reply.into()));
            on_event(StreamEvent::Done);
            Ok(())
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    fn memory_runner(
        compact: Option<u32>,
    ) -> (ProviderRunner, Arc<std::sync::Mutex<Vec<Vec<Message>>>>) {
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut runner, _) = runner_with(Vec::new(), Duration::ZERO, None);
        let mut registry = Registry::empty();
        registry.register(Box::new(MemoryProvider {
            requests: requests.clone(),
        }));
        runner.registry = registry;
        runner.provider_id = "memory".into();
        (runner.with_compaction(compact), requests)
    }

    fn finish(core: &crate::CoreHandle, text: &str) -> Outcome {
        core.submit(text, vec![]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            for event in core.drain_blocking(Duration::from_millis(20)) {
                if let Payload::TaskFinished { outcome, .. } = event.payload {
                    return outcome;
                }
            }
        }
        panic!("the turn never finished");
    }

    /// The memory the user asked for: a long conversation is summarized
    /// once, the summary replaces the older turns in the saved file, and a
    /// resumed session sends the summary instead of the whole history,
    /// without summarizing again.
    #[test]
    fn a_long_conversation_is_compacted_once_and_resumes_from_the_summary() {
        let dir = std::env::temp_dir().join(format!("se-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.conversation.json");
        let old: Vec<Message> = (0..6)
            .map(|i| {
                let role = if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                };
                Message::text(role, format!("turn {i} {}", "x".repeat(400)))
            })
            .collect();
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

        let (runner, requests) = memory_runner(Some(100));
        let core = crate::start_conversation(runner, &path).unwrap();
        assert!(matches!(
            finish(&core, "next step?"),
            Outcome::Completed { .. }
        ));
        core.shutdown();

        let seen = requests.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "one summary call, then the turn itself");
        assert_eq!(seen[0][0].content, COMPACT_SYSTEM);
        assert!(seen[0][1].content.contains("turn 0") && !seen[0][1].content.contains("turn 2"));
        let main = &seen[1];
        assert!(
            main[1].content.starts_with(MEMORY_PREFIX),
            "{:?}",
            main[1].content
        );
        assert!(!main.iter().any(|m| m.content.starts_with("turn 0")));
        assert!(main.iter().any(|m| m.content.starts_with("turn 5")));

        let saved = crate::load_conversation(&path).unwrap();
        assert_eq!(
            saved.len(),
            2 + 4 + 2,
            "summary pair + last 4 + this exchange"
        );
        assert!(saved[0].content.starts_with(MEMORY_PREFIX));
        assert_eq!(saved.last().unwrap().content, "answered");

        // A new process resumes the file: the summary is sent, never redone.
        let (runner, requests) = memory_runner(Some(100_000));
        let core = crate::start_conversation(runner, &path).unwrap();
        assert!(matches!(
            finish(&core, "and then?"),
            Outcome::Completed { .. }
        ));
        core.shutdown();
        let seen = requests.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "no second summary call");
        assert!(seen[0][1].content.starts_with(MEMORY_PREFIX));
        assert_eq!(crate::load_conversation(&path).unwrap().len(), 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An agent CLI that records each turn's system prompt, and a router
    /// that answers routing questions from a script (KEEP, departments, roles).
    struct FakeCli(Arc<std::sync::Mutex<Vec<String>>>);
    impl ModelProvider for FakeCli {
        fn id(&self) -> &str {
            "cli"
        }
        fn display_name(&self) -> &str {
            "cli"
        }
        fn env_var(&self) -> &str {
            "SE_TEST_CLI"
        }
        fn detect(&self) -> Status {
            Status {
                configured: true,
                detail: "c".into(),
                source: se_provider::Source::Environment,
            }
        }
        fn is_agent_cli(&self) -> bool {
            true
        }
        fn stream(
            &self,
            request: &ChatRequest,
            _cancelled: &dyn Fn() -> bool,
            on_event: &mut dyn FnMut(StreamEvent) -> bool,
        ) -> Result<(), ProviderError> {
            let user = &request.messages.last().unwrap().content;
            assert!(!user.contains("Which department"), "an agent CLI is never asked to route");
            self.0.lock().unwrap().push(request.messages[0].content.clone());
            on_event(StreamEvent::TextDelta("answered".into()));
            on_event(StreamEvent::Done);
            Ok(())
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    struct ScriptRouter(std::sync::Mutex<std::collections::VecDeque<&'static str>>);
    impl ModelProvider for ScriptRouter {
        fn id(&self) -> &str {
            "router"
        }
        fn display_name(&self) -> &str {
            "router"
        }
        fn env_var(&self) -> &str {
            "SE_TEST_ROUTER"
        }
        fn detect(&self) -> Status {
            Status {
                configured: true,
                detail: "r".into(),
                source: se_provider::Source::Environment,
            }
        }
        fn stream(
            &self,
            _request: &ChatRequest,
            _cancelled: &dyn Fn() -> bool,
            on_event: &mut dyn FnMut(StreamEvent) -> bool,
        ) -> Result<(), ProviderError> {
            let reply = self.0.lock().unwrap().pop_front().expect("an unscripted routing call");
            on_event(StreamEvent::TextDelta(reply.into()));
            on_event(StreamEvent::Done);
            Ok(())
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    fn cli_core(router: Option<Vec<&'static str>>) -> (crate::CoreHandle, Arc<std::sync::Mutex<Vec<String>>>, std::path::PathBuf) {
        let systems = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut runner, _) = runner_with(Vec::new(), Duration::ZERO, None);
        let mut registry = Registry::empty();
        registry.register(Box::new(FakeCli(systems.clone())));
        let router_model = router.map(|script| {
            registry.register(Box::new(ScriptRouter(std::sync::Mutex::new(script.into()))));
            se_protocol::ModelRef {
                provider: "router".into(),
                model: "small".into(),
            }
        });
        runner.registry = registry;
        runner.provider_id = "cli".into();
        let root = routing_catalogue();
        let core = crate::start(
            runner
                .with_catalogue_root(root.clone())
                .with_auto_route(true)
                .with_router_model(router_model),
        );
        (core, systems, root)
    }

    /// No router model: an agent-CLI turn is never keyword-guessed into a
    /// role; it goes to the fallback, which is told the real roster.
    #[test]
    fn an_agent_cli_turn_without_a_router_goes_to_the_fallback_with_the_roster() {
        // "react component screen" would keyword-match frontend-lead.
        let (core, systems, root) = cli_core(None);
        finish(&core, "build a react component for the settings screen");
        core.shutdown();
        let seen = systems.lock().unwrap().clone();
        assert_eq!(role_body(&seen[0]).trim(), "ORCH BODY");
        assert!(seen[0].contains("SE runtime"), "{}", seen[0]);
        assert!(
            seen[0].contains("backend-and-data: backend-lead, database-engineer"),
            "{}",
            seen[0]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// With a router model, the CLI turn is routed by it; a follow-up the
    /// router calls KEEP or same-department stays; a move to another
    /// department switches the specialist.
    #[test]
    fn a_router_model_routes_cli_turns_keeps_follow_ups_and_switches_on_a_new_field() {
        let (core, systems, root) = cli_core(Some(vec![
            "backend-and-data",
            "database-engineer",
            "KEEP",
            "backend-and-data",
            "frontend",
            "frontend-lead",
        ]));
        for task in [
            "add an orders table",
            "what is the codename again?",
            "and an index on it",
            "now the settings screen",
        ] {
            finish(&core, task);
        }
        core.shutdown();
        let seen: Vec<String> = systems
            .lock()
            .unwrap()
            .iter()
            .map(|s| role_body(s).trim().to_string())
            .collect();
        assert_eq!(seen, ["DB BODY", "DB BODY", "DB BODY", "FRONTEND BODY"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A role given its own model runs on it, even over the session's default,
    /// and without asking (the user set it in models.json).
    #[test]
    fn a_role_with_its_own_model_runs_on_that_model() {
        let default_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let role_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut runner, _) = runner_with(Vec::new(), Duration::ZERO, None);
        let mut registry = Registry::empty();
        registry.register(Box::new(RoutingProvider {
            dept_reply: "backend-and-data",
            role_reply: "database-engineer",
            systems: default_seen.clone(),
            asked: Arc::new(AtomicUsize::new(0)),
        }));
        registry.register(Box::new(FakeCli(role_seen.clone())));
        runner.registry = registry;
        runner.provider_id = "routing".into();
        let root = routing_catalogue();
        let roles = std::collections::HashMap::from([(
            "database-engineer".to_string(),
            se_protocol::ModelRef {
                provider: "cli".into(),
                model: "top".into(),
            },
        )]);
        let core = crate::start(
            runner
                .with_catalogue_root(root.clone())
                .with_role_models(roles),
        );
        core.submit_specialist("database-engineer", "add an orders table", vec![])
            .unwrap();
        wait_finished(&core);
        core.submit_specialist("frontend-lead", "now the screen", vec![])
            .unwrap();
        wait_finished(&core);
        core.shutdown();
        let role: Vec<String> = role_seen.lock().unwrap().iter().map(|s| role_body(s).trim().to_string()).collect();
        let default: Vec<String> = default_seen.lock().unwrap().iter().map(|s| role_body(s).trim().to_string()).collect();
        assert_eq!(role, ["DB BODY"], "the mapped role ran on its own model");
        assert_eq!(default, ["FRONTEND BODY"], "an unmapped role stayed on the default");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Budgets count fresh tokens: an agent CLI's cached, resent prompt is not new spend.
    #[test]
    fn budgets_count_fresh_tokens_not_cache_reads() {
        let u = Usage {
            input_tokens: Some(31_000),
            output_tokens: Some(200),
            cached_input_tokens: Some(30_000),
            ..Default::default()
        };
        assert_eq!(reported_tokens(Some(&u)), Some(1_200));
        let none = Usage::default();
        assert_eq!(reported_tokens(Some(&none)), None, "silence is not zero");
    }

    /// A role that cannot deploy agents is not shown the roster.
    #[test]
    fn the_roster_is_only_shown_to_a_role_that_may_deploy_agents() {
        let with = runtime_note("B".into(), Some("  d: a, b\n"), Asker::User, "/p");
        let without = runtime_note("B".into(), None, Asker::Parent, "/p");
        assert!(with.contains("d: a, b") && with.starts_with("B"));
        assert!(!without.contains("call the `agent` tool"));
        assert_eq!(role_body(&without), "B");
    }

    /// Open decisions: the user is asked (with options and a free choice), a
    /// sub-agent hands the question up, an unattended team task records assumptions.
    #[test]
    fn open_decisions_are_asked_handed_up_or_recorded_by_who_can_answer() {
        let user = runtime_note("B".into(), None, Asker::User, "/p");
        assert!(user.contains("ask before building") && user.contains("Something else - tell me"));
        assert!(user.contains("(recommended)"));
        let child = runtime_note("B".into(), None, Asker::Parent, "/p");
        assert!(child.contains("cannot reach the user") && !child.contains("ask before building"));
        let team = runtime_note("B".into(), None, Asker::Nobody, "/p");
        assert!(team.contains("Assumptions:") && !team.contains("ask before building"));
    }

    /// A conversation keeps its specialist: a follow-up is not re-routed
    /// on its own words (no routing calls), and naming another switches it.
    #[test]
    fn a_follow_up_keeps_the_conversations_specialist() {
        let systems = Arc::new(std::sync::Mutex::new(Vec::new()));
        let asked = Arc::new(AtomicUsize::new(0));
        let (mut runner, _) = runner_with(Vec::new(), Duration::ZERO, None);
        let mut registry = Registry::empty();
        registry.register(Box::new(RoutingProvider {
            dept_reply: "backend-and-data",
            role_reply: "database-engineer",
            systems: systems.clone(),
            asked: asked.clone(),
        }));
        runner.registry = registry;
        runner.provider_id = "routing".into();
        let root = routing_catalogue();
        let core = crate::start(
            runner
                .with_catalogue_root(root.clone())
                .with_auto_route(true),
        );
        finish(&core, "add an orders table");
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        finish(&core, "what is the codename again?");
        assert_eq!(
            asked.load(Ordering::SeqCst),
            2,
            "no routing calls for a follow-up"
        );
        core.submit_specialist("frontend-lead", "now the screen", vec![])
            .unwrap();
        wait_finished(&core);
        finish(&core, "and the button?");
        core.shutdown();
        let seen: Vec<String> = systems
            .lock()
            .unwrap()
            .iter()
            .map(|s| role_body(s).trim().to_string())
            .collect();
        assert_eq!(
            seen,
            ["DB BODY", "DB BODY", "FRONTEND BODY", "FRONTEND BODY"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The fallback never sticks: the next turn is routed afresh.
    #[test]
    fn the_fallback_specialist_does_not_stick() {
        let asked = Arc::new(AtomicUsize::new(0));
        let (mut runner, _) = runner_with(Vec::new(), Duration::ZERO, None);
        let mut registry = Registry::empty();
        registry.register(Box::new(RoutingProvider {
            dept_reply: "??",
            role_reply: "??",
            systems: Arc::new(std::sync::Mutex::new(Vec::new())),
            asked: asked.clone(),
        }));
        runner.registry = registry;
        runner.provider_id = "routing".into();
        let root = routing_catalogue();
        let core = crate::start(
            runner
                .with_catalogue_root(root.clone())
                .with_auto_route(true),
        );
        finish(&core, "hello there");
        let first = asked.load(Ordering::SeqCst);
        finish(&core, "hello again");
        core.shutdown();
        assert_eq!(asked.load(Ordering::SeqCst), first * 2, "routed again");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn wait_finished(core: &crate::CoreHandle) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            for event in core.drain_blocking(Duration::from_millis(20)) {
                if matches!(event.payload, Payload::TaskFinished { .. }) {
                    return;
                }
            }
        }
        panic!("the turn never finished");
    }

    /// The screenshot bug: with several providers and none named, SE used
    /// to start the local echo. Now a real runner starts with no default:
    /// a turn without a model says how to pick one and calls nothing...
    #[test]
    fn with_no_model_chosen_a_turn_says_to_pick_one_and_calls_nothing() {
        let (mut runner, requests) = memory_runner(None);
        runner.provider_id = String::new();
        runner.model = String::new();
        let (outcome, _) = run(runner, "hi");
        match outcome {
            Outcome::Failed { detail } => assert!(detail.contains("/models"), "{detail}"),
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(requests.lock().unwrap().is_empty());
    }

    /// ...and once a model is picked, turns run on it -- as a specialist,
    /// without asking permission for the user's own choice.
    #[test]
    fn a_picked_model_runs_specialist_turns_without_asking_permission_for_it() {
        let (mut runner, requests) = memory_runner(None);
        runner.provider_id = String::new();
        runner.model = String::new();
        let root = routing_catalogue();
        let core = crate::start(
            runner
                .with_catalogue_root(root.clone())
                .with_auto_route(true),
        );
        let pick = se_protocol::ModelRef {
            provider: "memory".into(),
            model: "m1".into(),
        };
        core.submit_with_model("hi", vec![], Some(pick)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut payloads = Vec::new();
        'outer: while std::time::Instant::now() < deadline {
            for event in core.drain_blocking(Duration::from_millis(20)) {
                let done = matches!(event.payload, Payload::TaskFinished { .. });
                payloads.push(event.payload);
                if done {
                    break 'outer;
                }
            }
        }
        core.shutdown();
        assert!(
            !payloads
                .iter()
                .any(|p| matches!(p, Payload::ApprovalRequired { .. })),
            "{payloads:?}"
        );
        assert!(payloads
            .iter()
            .any(|p| matches!(p, Payload::SpecialistInvoked { .. })));
        assert!(
            matches!(
                payloads.last(),
                Some(Payload::TaskFinished {
                    outcome: Outcome::Completed { .. },
                    ..
                })
            ),
            "{payloads:?}"
        );
        assert!(!requests.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Fails its first `fail` calls with `error`, then answers.
    struct FlakyProvider {
        fail: usize,
        error: ProviderError,
        calls: Arc<AtomicUsize>,
    }

    impl ModelProvider for FlakyProvider {
        fn id(&self) -> &str {
            "flaky"
        }
        fn display_name(&self) -> &str {
            "flaky"
        }
        fn env_var(&self) -> &str {
            "SE_TEST_FLAKY_KEY"
        }
        fn detect(&self) -> Status {
            Status {
                configured: true,
                detail: "f".into(),
                source: se_provider::Source::Environment,
            }
        }
        fn stream(
            &self,
            _request: &ChatRequest,
            _cancelled: &dyn Fn() -> bool,
            on_event: &mut dyn FnMut(StreamEvent) -> bool,
        ) -> Result<(), ProviderError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) < self.fail {
                return Err(self.error.clone());
            }
            on_event(StreamEvent::TextDelta("answered".into()));
            on_event(StreamEvent::Done);
            Ok(())
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    fn flaky(fail: usize, error: ProviderError) -> (Outcome, usize) {
        let calls = Arc::new(AtomicUsize::new(0));
        let (mut runner, _) = runner_with(Vec::new(), Duration::ZERO, None);
        let mut registry = Registry::empty();
        registry.register(Box::new(FlakyProvider {
            fail,
            error,
            calls: calls.clone(),
        }));
        runner.registry = registry;
        runner.provider_id = "flaky".into();
        let (outcome, _) = run(runner, "hi");
        (outcome, calls.load(Ordering::SeqCst))
    }

    fn throttled() -> ProviderError {
        ProviderError::RateLimited {
            retry_after: Some(Duration::from_millis(1)),
            detail: "slow down".into(),
        }
    }

    #[test]
    fn a_temporary_provider_failure_is_retried() {
        let (outcome, calls) = flaky(1, throttled());
        assert!(matches!(outcome, Outcome::Completed { .. }), "{outcome:?}");
        assert_eq!(calls, 2);
    }

    #[test]
    fn retries_are_bounded() {
        let (outcome, calls) = flaky(10, throttled());
        assert!(!matches!(outcome, Outcome::Completed { .. }), "{outcome:?}");
        assert_eq!(calls, 1 + MAX_RETRIES as usize);
    }

    #[test]
    fn a_refusal_is_never_retried() {
        let (_, calls) = flaky(10, ProviderError::Policy("blocked".into()));
        assert_eq!(calls, 1);
    }

    // --- catalogue tool names -> SE tool names ---

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn catalogue_tool_names_map_to_se_tools() {
        assert_eq!(
            se_tools_for(&names(&["read-files", "search", "write-files", "run-shell"])),
            names(&["read", "search", "write", "edit", "shell"])
        );
        // SE's own names pass through (definitions may use either).
        assert_eq!(se_tools_for(&names(&["read", "edit"])), names(&["read", "edit"]));
        // run-tests never becomes shell.
        assert!(se_tools_for(&names(&["read-files", "run-tests"])) == names(&["read"]));
        // Declared, but nothing SE has: deny-all, not "unrestricted".
        assert!(se_tools_for(&names(&["memory-read", "memory-write", "escalate"])).is_empty());
        assert!(se_tools_for(&names(&["no-such-tool"])).is_empty());
        // C2: deploying sub-agents.
        assert_eq!(se_tools_for(&names(&["invoke-agent"])), names(&["agent"]));
        assert_eq!(se_tools_for(&names(&["agent", "read"])), names(&["agent", "read"]));
    }

    /// The real catalogue's definition of `id`, or `None` (skipped) off
    /// this machine.
    fn real_allowlist(id: &str) -> Option<Vec<String>> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../se/agents");
        if !root.exists() {
            eprintln!("skipped: no real catalogue at {}", root.display());
            return None;
        }
        let report = se_catalogue::load(&root).unwrap();
        let agent = report
            .agents
            .into_iter()
            .find(|a| a.frontmatter.id == id)
            .unwrap_or_else(|| panic!("`{id}` is not in the real catalogue"));
        assert!(!agent.frontmatter.tools.is_empty());
        Some(se_tools_for(&agent.frontmatter.tools))
    }

    fn denied(allowed: &[String], tool: &str) -> bool {
        Ruleset::from_rules(vec![se_permission::Rule::allow_all()]).evaluate_scoped(
            "x",
            tool,
            "/x",
            Some(allowed),
        ) == se_permission::Effect::Deny
    }

    #[test]
    fn the_real_chief_orchestrator_may_read_and_search_but_not_write_or_shell() {
        let Some(allowed) = real_allowlist("chief-orchestrator") else {
            return;
        };
        assert!(!denied(&allowed, "read") && !denied(&allowed, "search"), "{allowed:?}");
        // It declares neither write-files nor run-shell.
        for tool in ["write", "edit", "shell"] {
            assert!(denied(&allowed, tool), "{tool}: {allowed:?}");
        }
        // It declares invoke-agent, so it is offered the `agent` tool.
        assert!(allowed.iter().any(|t| t == "agent"), "{allowed:?}");
    }

    #[test]
    fn the_real_backend_lead_may_read_write_edit_and_shell() {
        let Some(allowed) = real_allowlist("backend-lead") else {
            return;
        };
        for tool in ["read", "search", "write", "edit", "shell"] {
            assert!(!denied(&allowed, tool), "{tool}: {allowed:?}");
        }
    }

    // --- the conversation's specialist, saved for resume ---

    fn temp_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "se-core-sticky-{name}-{}-{}",
            std::process::id(),
            unix_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("c.specialist")
    }

    fn unix_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn a_chosen_specialist_is_saved_and_restored_and_clearing_saves_none() {
        let path = temp_file("set-clear");
        let (runner, _) = runner_with(Vec::new(), Duration::ZERO, None);
        let runner = runner.with_specialist_file(path.clone());
        assert_eq!(runner.sticky_specialist.get(), None);
        runner.set_specialist(Some("backend-lead".into()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "backend-lead");
        // Resume: a fresh runner on the same file comes back to it.
        let (resumed, _) = runner_with(Vec::new(), Duration::ZERO, None);
        let resumed = resumed.with_specialist_file(path.clone());
        assert_eq!(resumed.sticky_specialist.get().as_deref(), Some("backend-lead"));
        // `/agents none`: cleared, and saved as cleared.
        resumed.set_specialist(None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        let (again, _) = runner_with(Vec::new(), Duration::ZERO, None);
        assert_eq!(again.with_specialist_file(path.clone()).sticky_specialist.get(), None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_new_chat_starts_with_no_specialist() {
        let old = temp_file("old-chat");
        std::fs::write(&old, "backend-lead").unwrap();
        // A new chat is a new conversation id, so a file not yet written.
        let fresh = old.with_file_name("new.specialist");
        let (runner, _) = runner_with(Vec::new(), Duration::ZERO, None);
        assert_eq!(runner.with_specialist_file(fresh).sticky_specialist.get(), None);
        let _ = std::fs::remove_dir_all(old.parent().unwrap());
    }
}

#[cfg(test)]
#[path = "team_run_tests.rs"]
mod team_run_tests;

#[path = "agent_tool.rs"]
mod agent_tool;
