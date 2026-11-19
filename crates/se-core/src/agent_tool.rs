//! The `agent` tool (Batch C2): a turn deploys catalogue specialists as
//! sub-agents. Each child is a full turn of the same runner
//! (`run_inner` with a [`Child`]): its role's prompt, its role's own
//! tools minus `agent`, the same gateway, audit log and approval
//! prompts as the parent. A child module of `provider_runner`.
//!
//! Foreground only: the call blocks until every child of the round ends.
//! `background: true` is accepted but runs in the foreground (with a note
//! in the result), because a child outliving its parent task could no
//! longer have its approvals or interrupts routed to it by the engine.

use super::*;
use se_protocol::{AgentEvent, AgentState, ModelRef, Payload};
use std::sync::atomic::AtomicU64;

pub(super) const AGENT_TOOL: &str = "agent";
/// Children one parent round may run at once; extra calls are refused.
pub(super) const MAX_CHILDREN: usize = 4;
/// Tokens (input + output, as the provider reports them) one child may use.
pub(super) const CHILD_TOKEN_BUDGET: u64 = 60_000;
/// Wall-clock bound on one child, approval waits included.
pub(super) const CHILD_DEADLINE: Duration = Duration::from_secs(600);
/// A child's answer is cut to this many chars before it reaches the parent.
const RESULT_CHARS: usize = 8_000;

/// What a sub-agent's turn needs beyond an ordinary turn.
pub(super) struct Child {
    pub id: String,
    pub budget: u64,
    pub deadline: Duration,
}

pub(super) fn definition() -> se_provider::ToolDefinition {
    se_provider::ToolDefinition {
        name: AGENT_TOOL.into(),
        description: "Deploys a specialist sub-agent from the SE catalogue to do one task and returns its final answer. Several calls in one reply run in parallel (at most 4). The sub-agent has its role's own tools and cannot deploy agents itself.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "role": {"type": "string", "description": "catalogue agent id, e.g. backend-lead"},
                "task": {"type": "string", "description": "the complete task; the sub-agent sees nothing else"},
                "background": {"type": "boolean", "description": "not supported yet: runs in the foreground"},
            },
            "required": ["role", "task"],
        }),
    }
}

/// Records a sub-agent's decisions as `role#id`. The request itself
/// keeps the bare role, so ruleset rules for that role still apply.
pub(super) struct Tagged<'a>(
    pub &'a (dyn se_permission::AuditSink + Send + Sync),
    pub Option<String>,
);

impl se_permission::AuditSink for Tagged<'_> {
    fn record(&self, mut record: se_permission::AuditRecord) -> Result<(), String> {
        if let Some(tag) = &self.1 {
            record.agent_id = tag.clone();
        }
        self.0.record(record)
    }
}

struct Job {
    index: usize,
    id: String,
    role: String,
    task: String,
    background: bool,
    call: se_protocol::ToolCallId,
    stop: Arc<AtomicBool>,
    timed_out: AtomicBool,
}

fn next_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("a{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// `(role, task, background)` from a call's arguments, the role checked
/// against the catalogue.
fn parse(
    arguments: &str,
    agents: &[se_catalogue::AgentDefinition],
) -> Result<(String, String, bool), String> {
    let v: serde_json::Value =
        serde_json::from_str(arguments).map_err(|e| format!("invalid arguments JSON: {e}"))?;
    let get = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("missing or empty `{k}`"))
    };
    let (role, task) = (get("role")?, get("task")?);
    let background = v.get("background").and_then(|b| b.as_bool()).unwrap_or(false);
    if agents.iter().any(|a| a.frontmatter.id == role) {
        return Ok((role, task, background));
    }
    let words: Vec<&str> = role
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() > 2)
        .collect();
    let near: Vec<&str> = agents
        .iter()
        .map(|a| a.frontmatter.id.as_str())
        .filter(|id| words.iter().any(|w| id.contains(&w.to_ascii_lowercase())))
        .take(6)
        .collect();
    Err(if near.is_empty() {
        format!("no agent `{role}` in the catalogue (`se agents` lists ids)")
    } else {
        format!(
            "no agent `{role}` in the catalogue; near matches: {}",
            near.join(", ")
        )
    })
}

/// The first `RESULT_CHARS` chars of `text`, saying so when cut.
fn truncate(text: &str) -> String {
    let total = text.chars().count();
    if total <= RESULT_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(RESULT_CHARS).collect();
    format!("{kept}\n[truncated: the agent's answer was {total} chars; the first {RESULT_CHARS} are shown]")
}

impl ProviderRunner {
    /// Runs this round's `agent` calls. Returns one entry per call in
    /// `calls`: the tool result for an `agent` call, `None` for any other.
    pub(super) fn run_agents(
        &self,
        sink: &mut TurnSink,
        calls: &[ToolCallRecord],
        parent: &(String, String),
        chosen: &ModelRef,
    ) -> Vec<Option<String>> {
        let mut out: Vec<Option<String>> = vec![None; calls.len()];
        if !calls.iter().any(|c| c.name == AGENT_TOOL) {
            return out;
        }
        let catalogue = match &self.catalogue_root {
            None => Err("no agent catalogue is configured".to_string()),
            Some(root) => se_catalogue::load(root)
                .map_err(|e| format!("could not read the agent catalogue: {e}")),
        };
        let deadline = CHILD_DEADLINE.min(self.turn_deadline);
        let mut jobs: Vec<Job> = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            if call.name != AGENT_TOOL {
                continue;
            }
            let spec = catalogue
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|c| parse(&call.arguments, &c.agents))
                .and_then(|spec| {
                    if jobs.len() >= MAX_CHILDREN {
                        Err(format!("too many agents running: at most {MAX_CHILDREN} per reply; call again once these have finished"))
                    } else {
                        Ok(spec)
                    }
                });
            let (role, task, background) = match spec {
                Ok(s) => s,
                Err(reason) => {
                    out[index] = Some(self.reject_unparsed(sink, call, parent, &reason));
                    continue;
                }
            };
            let id = next_id();
            let shown = format!("{role} [{id}]: {}", crate::team::driver::one_line(&task, 80));
            let scope = format!("spawn {role}#{id}");
            // A spawn goes through the same policy and mode as any tool call:
            // deny is final, manual asks, auto and an allow rule run it.
            let effect = self.ruleset.evaluate(&parent.0, AGENT_TOOL, &scope);
            let asked = match effect {
                se_permission::Effect::Deny => {
                    out[index] = Some(self.reject_unparsed(sink, call, parent, "denied by policy"));
                    continue;
                }
                se_permission::Effect::Ask if self.work_mode() == se_protocol::WorkMode::Manual => {
                    match sink.request_approval(AGENT_TOOL, &shown) {
                        Some(call_id) => Some(call_id),
                        None => {
                            let _ = self.audit.record(se_permission::AuditRecord {
                                agent_id: parent.0.clone(),
                                session: sink.session(),
                                task: sink.task(),
                                tool_name: AGENT_TOOL.to_string(),
                                resource_scope: scope,
                                effect: se_permission::Effect::Deny,
                            });
                            out[index] = Some("denied: not approved".to_string());
                            continue;
                        }
                    }
                }
                _ => None,
            };
            // No action without a record of it: an unrecordable spawn is refused.
            let recorded = self.audit.record(se_permission::AuditRecord {
                agent_id: parent.0.clone(),
                session: sink.session(),
                task: sink.task(),
                tool_name: AGENT_TOOL.to_string(),
                resource_scope: scope,
                effect: se_permission::Effect::Allow,
            });
            if let Err(e) = recorded {
                out[index] = Some(self.reject_unparsed(sink, call, parent, &e));
                continue;
            }
            let call_id = asked.unwrap_or_else(se_protocol::ToolCallId::next);
            let _ = sink.tool_requested(call_id, AGENT_TOOL, &shown);
            jobs.push(Job {
                index,
                id,
                role,
                task,
                background,
                call: call_id,
                stop: Arc::new(AtomicBool::new(false)),
                timed_out: AtomicBool::new(false),
            });
        }
        if jobs.is_empty() {
            return out;
        }

        let parent_sink: &TurnSink = sink;
        let results: Vec<Result<String, String>> = std::thread::scope(|s| {
            let handles: Vec<_> = jobs
                .iter()
                .map(|job| {
                    let mut child_sink = parent_sink.child(&job.id, &job.role, job.stop.clone());
                    s.spawn(move || self.run_child(&mut child_sink, job, &parent.0, chosen, deadline))
                })
                .collect();
            // The parent's interrupt (Esc, /new, /resume, shutdown) and each
            // child's deadline reach the children from here.
            let started = Instant::now();
            while !handles.iter().all(|h| h.is_finished()) {
                let cancelled = parent_sink.is_cancelled();
                let late = started.elapsed() >= deadline;
                for job in &jobs {
                    if late && !cancelled && !job.stop.load(Ordering::SeqCst) {
                        job.timed_out.store(true, Ordering::SeqCst);
                    }
                    if cancelled || late {
                        job.stop.store(true, Ordering::SeqCst);
                    }
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err("the agent crashed".to_string())))
                .collect()
        });

        for (job, result) in jobs.iter().zip(results) {
            let (ok, text) = match result {
                Ok(answer) if job.background => (
                    true,
                    format!("(background agents are not supported yet; {} [{}] ran in the foreground)\n{answer}", job.role, job.id),
                ),
                Ok(answer) => (true, answer),
                Err(reason) => (
                    false,
                    format!("tool error: agent {} [{}] failed: {reason}", job.role, job.id),
                ),
            };
            let _ = sink.tool_completed(
                job.call,
                AGENT_TOOL,
                ok,
                &crate::team::driver::one_line(&text, 120),
            );
            out[job.index] = Some(text);
        }
        out
    }

    /// One child, on its own thread: lifecycle events around a full turn.
    fn run_child(
        &self,
        sink: &mut TurnSink,
        job: &Job,
        parent: &str,
        chosen: &ModelRef,
        deadline: Duration,
    ) -> Result<String, String> {
        let agent_id = job.id.clone();
        let _ = sink.emit(Payload::Agent(AgentEvent::Spawned {
            agent_id: agent_id.clone(),
            role: job.role.clone(),
            parent: Some(parent.to_string()),
            task: job.task.clone(),
        }));
        let _ = sink.emit(Payload::Agent(AgentEvent::State {
            agent_id: agent_id.clone(),
            state: AgentState::Running,
        }));
        let turn = UserTurn {
            session: sink.session(),
            task: sink.task(),
            text: job.task.clone(),
            attachments: Vec::new(),
            // The parent's own model, so an `/models` pick carries over and
            // an unselected runner still has one.
            model: Some(chosen.clone()),
            specialist_id: Some(job.role.clone()),
            team_node: None,
        };
        let child = Child {
            id: agent_id.clone(),
            budget: CHILD_TOKEN_BUDGET,
            deadline,
        };
        let outcome = self.run_inner(&turn, &[], sink, Some(&child));
        let failed = |sink: &mut TurnSink, error: String| {
            let _ = sink.emit(Payload::Agent(AgentEvent::Failed {
                agent_id: agent_id.clone(),
                error: error.clone(),
            }));
            Err(error)
        };
        match outcome {
            Outcome::Completed { final_text } => {
                let _ = sink.emit(Payload::Agent(AgentEvent::Done {
                    agent_id: agent_id.clone(),
                    summary: crate::team::driver::one_line(&final_text, 120),
                }));
                Ok(truncate(&final_text))
            }
            Outcome::Cancelled if job.timed_out.load(Ordering::SeqCst) => failed(
                sink,
                format!("deadline of {}s exceeded", deadline.as_secs()),
            ),
            Outcome::Cancelled => {
                let _ = sink.emit(Payload::Agent(AgentEvent::State {
                    agent_id: agent_id.clone(),
                    state: AgentState::Cancelled,
                }));
                Err("cancelled".to_string())
            }
            Outcome::Failed { detail } => failed(sink, detail),
            Outcome::Incomplete { reason, .. } => failed(sink, reason),
            other => failed(sink, other.label().to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_protocol::CoreEvent;
    use se_provider::{ModelInfo, ModelProvider, Status};
    use std::path::PathBuf;
    use std::sync::Mutex;

    type Script = dyn Fn(&ChatRequest) -> Vec<StreamEvent> + Send + Sync;
    type Seen = Arc<Mutex<Vec<ChatRequest>>>;

    /// A model under the test's control. A request whose system prompt
    /// contains `HANG` streams nothing until it is stopped.
    struct Fake {
        seen: Seen,
        script: Box<Script>,
    }

    impl ModelProvider for Fake {
        fn id(&self) -> &str {
            "fake"
        }
        fn display_name(&self) -> &str {
            "Fake"
        }
        fn env_var(&self) -> &str {
            "SE_TEST_UNUSED"
        }
        fn detect(&self) -> Status {
            Status {
                configured: true,
                detail: "fake".into(),
                source: se_provider::Source::Environment,
            }
        }
        fn stream(
            &self,
            request: &ChatRequest,
            cancelled: &dyn Fn() -> bool,
            on_event: &mut dyn FnMut(StreamEvent) -> bool,
        ) -> Result<(), ProviderError> {
            self.seen.lock().unwrap().push(request.clone());
            if request.messages[0].content.contains("HANG") {
                while !cancelled() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                return Err(ProviderError::Cancelled);
            }
            for event in (self.script)(request) {
                if cancelled() || !on_event(event) {
                    return Err(ProviderError::Cancelled);
                }
            }
            Ok(())
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    fn calls(list: &[(&str, serde_json::Value)]) -> Vec<StreamEvent> {
        let mut out: Vec<StreamEvent> = list
            .iter()
            .enumerate()
            .map(|(i, (name, args))| StreamEvent::ToolCallRequested {
                id: format!("c{i}"),
                name: name.to_string(),
                arguments: args.to_string(),
            })
            .collect();
        out.push(StreamEvent::FinishReason("tool_calls".into()));
        out
    }

    fn say(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta(text.into()),
            StreamEvent::FinishReason("stop".into()),
        ]
    }

    fn is_child(r: &ChatRequest) -> bool {
        r.messages[0].content.contains("ROLE:")
    }

    fn last_is_tool(r: &ChatRequest) -> bool {
        r.messages.last().unwrap().role == Role::Tool
    }

    fn spawn(role: &str, task: &str) -> (&'static str, serde_json::Value) {
        ("agent", serde_json::json!({"role": role, "task": task}))
    }

    /// A workspace holding `note.txt` and a catalogue with `reader`,
    /// `slow` (its body says HANG) and `spender`, each declaring
    /// read-files + invoke-agent. Returns (workspace, catalogue, audit).
    fn fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "se-core-agent-tool-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let ws = base.join("ws");
        let dept = base.join("agents").join("dept");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&dept).unwrap();
        std::fs::write(ws.join("note.txt"), "the codename is heron").unwrap();
        for (id, body) in [
            ("reader", "ROLE:reader"),
            ("slow", "ROLE:slow HANG"),
            ("spender", "ROLE:spender"),
        ] {
            std::fs::write(
                dept.join(format!("{id}.md")),
                format!("---\nschema_version: 1\nid: {id}\nname: {id}\ndepartment: dept\ntools:\n  - read-files\n  - invoke-agent\n---\n\n{body}\n"),
            )
            .unwrap();
        }
        (ws, base.join("agents"), base.join("audit.jsonl"))
    }

    fn runner(
        tag: &str,
        ruleset: Ruleset,
        script: impl Fn(&ChatRequest) -> Vec<StreamEvent> + Send + Sync + 'static,
    ) -> (ProviderRunner, Seen, PathBuf) {
        let (ws, catalogue, audit) = fixture(tag);
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::empty();
        registry.register(Box::new(Fake {
            seen: seen.clone(),
            script: Box::new(script),
        }));
        let runner = ProviderRunner {
            registry,
            provider_id: "fake".into(),
            model: "fake-model".into(),
            turn_deadline: DEFAULT_TURN_DEADLINE,
            ruleset,
            ledger: NonceLedger::new(),
            catalogue_root: Some(catalogue),
            workspace_root: ws,
            audit: Arc::new(crate::audit_log::FileAuditSink::open(&audit).unwrap()),
            auto_route: false,
            compact_at_tokens: None,
            sticky_specialist: Default::default(),
            mode: Default::default(),
            team: None,
        };
        (runner, seen, audit)
    }

    /// Runs one plain turn to its end; `on` sees every event first.
    fn drive(
        runner: ProviderRunner,
        mut on: impl FnMut(&crate::CoreHandle, &CoreEvent),
    ) -> (Outcome, Vec<Payload>) {
        let core = crate::start(runner);
        core.submit("go", vec![]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut payloads = Vec::new();
        while Instant::now() < deadline {
            for event in core.drain_blocking(Duration::from_millis(20)) {
                approve_spawns(&core, &event);
                on(&core, &event);
                payloads.push(event.payload.clone());
                if let Payload::TaskFinished { outcome, .. } = event.payload {
                    core.shutdown();
                    return (outcome, payloads);
                }
            }
        }
        panic!("the turn never finished: {payloads:?}");
    }

    /// Manual mode asks before each spawn; these tests are about what the child does.
    fn approve_spawns(core: &crate::CoreHandle, e: &CoreEvent) {
        if let Payload::ApprovalRequired { call, name, .. } = &e.payload {
            if name == AGENT_TOOL {
                core.approve(se_protocol::ApprovalDecision { task: e.task, call: *call, allow: true });
            }
        }
    }

    /// Manual mode asks before a spawn, and a "no" means the child never runs.
    #[test]
    fn a_spawn_asks_first_and_a_refusal_runs_no_child() {
        let (runner, seen, _) = runner("spawn-no", Ruleset::default(), reader_script(vec![spawn("reader", "one")]));
        let core = crate::start(runner);
        core.submit("go", vec![]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut asked = false;
        'run: while Instant::now() < deadline {
            for e in core.drain_blocking(Duration::from_millis(20)) {
                if let Payload::ApprovalRequired { call, name, .. } = &e.payload {
                    asked |= name == AGENT_TOOL;
                    core.approve(se_protocol::ApprovalDecision { task: e.task, call: *call, allow: false });
                }
                if let Payload::TaskFinished { .. } = e.payload {
                    break 'run;
                }
            }
        }
        core.shutdown();
        assert!(asked, "the spawn was not put to the user");
        assert!(!seen.lock().unwrap().iter().any(is_child), "a refused child ran");
        assert_eq!(results(&seen), ["denied: not approved"]);
    }

    fn approve_all(core: &crate::CoreHandle, e: &CoreEvent) {
        if let Payload::ApprovalRequired { call, .. } = e.payload {
            core.approve(se_protocol::ApprovalDecision {
                task: e.task,
                call,
                allow: true,
            });
        }
    }

    fn agent_events(payloads: &[Payload]) -> Vec<&AgentEvent> {
        payloads
            .iter()
            .filter_map(|p| match p {
                Payload::Agent(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    fn spawned(payloads: &[Payload]) -> Vec<String> {
        agent_events(payloads)
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::Spawned { agent_id, .. } => Some(agent_id.clone()),
                _ => None,
            })
            .collect()
    }

    /// The tool results the parent's last request carried back.
    fn results(seen: &Seen) -> Vec<String> {
        let seen = seen.lock().unwrap();
        let parent = seen.iter().rfind(|r| !is_child(r)).unwrap();
        parent
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.clone())
            .collect()
    }

    /// The parent makes `parent` calls; a child reads note.txt and
    /// answers with what it read.
    fn reader_script(
        parent: Vec<(&'static str, serde_json::Value)>,
    ) -> impl Fn(&ChatRequest) -> Vec<StreamEvent> {
        move |r| match (is_child(r), last_is_tool(r)) {
            (false, false) => calls(&parent),
            (false, true) => say("parent done"),
            (true, false) => calls(&[("read", serde_json::json!({"path": "note.txt"}))]),
            (true, true) => say(&format!("summary: {}", r.messages.last().unwrap().content)),
        }
    }

    fn id_of(e: &AgentEvent) -> &str {
        match e {
            AgentEvent::Spawned { agent_id, .. }
            | AgentEvent::State { agent_id, .. }
            | AgentEvent::Tokens { agent_id, .. }
            | AgentEvent::Done { agent_id, .. }
            | AgentEvent::Failed { agent_id, .. }
            | AgentEvent::Activity { agent_id, .. } => agent_id,
        }
    }

    #[test]
    fn a_child_runs_as_its_role_with_its_own_tools_and_its_answer_is_the_tool_result() {
        // A rule naming the bare role still applies to the child.
        let rules = Ruleset::from_rules(vec![se_permission::Rule::allow_resource_prefix(
            "reader", "",
        )]);
        let (runner, seen, audit) = runner(
            "happy",
            rules,
            reader_script(vec![spawn("reader", "summarise note.txt")]),
        );
        let (outcome, payloads) = drive(runner, |_, _| {});
        assert!(matches!(outcome, Outcome::Completed { .. }), "{outcome:?}");

        let requests = seen.lock().unwrap().clone();
        assert!(requests[0].tools.iter().any(|t| t.name == "agent"));
        let child = requests.iter().find(|r| is_child(r)).unwrap();
        assert_eq!(child.messages[0].content.trim(), "ROLE:reader");
        assert_eq!(child.messages.last().unwrap().content, "summarise note.txt");
        assert_eq!(child.model, "fake-model");
        // Declared read-files + invoke-agent: offered `read` only.
        let child_tools: Vec<&str> = child.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(child_tools, vec!["read"]);

        let res = results(&seen);
        assert_eq!(res.len(), 1);
        assert!(res[0].starts_with("summary: ") && res[0].contains("heron"), "{res:?}");

        // Spawned, Running, ..., Done -- and Done is the last for that id.
        let id = spawned(&payloads).pop().expect("spawned");
        let mine: Vec<&AgentEvent> = agent_events(&payloads)
            .into_iter()
            .filter(|e| id_of(e) == id)
            .collect();
        assert!(
            matches!(mine[0], AgentEvent::Spawned { parent: Some(p), role, .. } if p == NATIVE_LOOP_AGENT_ID && role == "reader")
        );
        assert!(matches!(mine[1], AgentEvent::State { state: AgentState::Running, .. }));
        assert!(
            matches!(mine.last().unwrap(), AgentEvent::Done { summary, .. } if summary.contains("heron"))
        );
        // The child's tool events are wrapped, never shown as the parent's.
        assert!(mine.iter().any(|e| matches!(e, AgentEvent::Activity { payload, .. }
            if matches!(payload.as_ref(), Payload::ToolCompleted { name, ok: true, .. } if name == "read"))));
        assert!(!payloads
            .iter()
            .any(|p| matches!(p, Payload::ToolCompleted { name, .. } if name == "read")));
        assert!(payloads
            .iter()
            .any(|p| matches!(p, Payload::ToolCompleted { name, ok: true, .. } if name == "agent")));

        let records = crate::audit_log::read(&audit).unwrap();
        let tag = format!("reader#{id}");
        assert!(
            records.iter().any(|r| r["agent"] == tag.as_str()
                && r["tool"] == "read"
                && r["effect"] == "Allow"),
            "{records:?}"
        );
        assert!(records
            .iter()
            .any(|r| r["tool"] == "agent" && r["resource"] == format!("spawn {tag}").as_str()));
    }

    #[test]
    fn a_child_cannot_deploy_agents_even_when_its_role_declares_it() {
        let (runner, seen, audit) =
            runner("depth", Ruleset::default(), |r| match (is_child(r), last_is_tool(r)) {
                (false, false) => calls(&[spawn("reader", "try to recurse")]),
                (false, true) => say("parent done"),
                // Forced: not offered, called anyway.
                (true, false) => calls(&[spawn("reader", "grandchild")]),
                (true, true) => say(&r.messages.last().unwrap().content.clone()),
            });
        let (_, payloads) = drive(runner, approve_all);
        assert_eq!(spawned(&payloads).len(), 1, "no grandchild may spawn");
        let child = seen
            .lock()
            .unwrap()
            .iter()
            .find(|r| is_child(r))
            .unwrap()
            .clone();
        assert!(!child.tools.iter().any(|t| t.name == "agent"));
        let res = results(&seen);
        assert!(res[0].contains("a sub-agent cannot deploy agents"), "{res:?}");
        let records = crate::audit_log::read(&audit).unwrap();
        assert!(records.iter().any(|r| r["tool"] == "agent"
            && r["effect"] == "Deny"
            && r["agent"].as_str().unwrap().starts_with("reader#a")));
    }

    #[test]
    fn at_most_four_children_run_per_reply() {
        let five: Vec<_> = (0..5).map(|_| spawn("spender", "work")).collect();
        let (runner, seen, _) =
            runner("cap", Ruleset::default(), move |r| match (is_child(r), last_is_tool(r)) {
                (false, false) => calls(&five),
                (false, true) => say("parent done"),
                (true, _) => say("ok"),
            });
        let (_, payloads) = drive(runner, |_, _| {});
        assert_eq!(spawned(&payloads).len(), MAX_CHILDREN);
        let res = results(&seen);
        assert_eq!(res.len(), 5, "one result per call");
        assert_eq!(res.iter().filter(|r| *r == "ok").count(), 4);
        assert!(res[4].contains("too many agents running"), "{res:?}");
    }

    #[test]
    fn a_child_over_its_token_budget_fails_and_the_parent_gets_an_error() {
        let (runner, seen, _) =
            runner("budget", Ruleset::default(), |r| match (is_child(r), last_is_tool(r)) {
                (false, false) => calls(&[spawn("spender", "spend")]),
                (false, true) => say("parent done"),
                (true, _) => {
                    let mut e = say("a long answer");
                    e.push(StreamEvent::Usage {
                        input_tokens: Some(CHILD_TOKEN_BUDGET),
                        output_tokens: Some(1),
                        cached_input_tokens: None,
                    });
                    e
                }
            });
        let (_, payloads) = drive(runner, |_, _| {});
        let events = agent_events(&payloads);
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::Tokens { used, .. } if *used == CHILD_TOKEN_BUDGET + 1)
        ));
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::Failed { error, .. } if error.contains("token budget exceeded"))
        ));
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::Done { .. })));
        let res = results(&seen);
        assert!(
            res[0].starts_with("tool error: agent spender") && res[0].contains("token budget"),
            "{res:?}"
        );
    }

    #[test]
    fn a_child_past_its_deadline_fails() {
        let (mut runner, seen, _) =
            runner("deadline", Ruleset::default(), |r| match (is_child(r), last_is_tool(r)) {
                (false, false) => calls(&[spawn("slow", "never ends")]),
                _ => say("parent done"),
            });
        // A child's deadline is min(10 min, the runner's turn deadline).
        runner.turn_deadline = Duration::from_millis(300);
        let started = Instant::now();
        let (_, payloads) = drive(runner, |_, _| {});
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(
            agent_events(&payloads)
                .iter()
                .any(|e| matches!(e, AgentEvent::Failed { .. })),
            "{payloads:?}"
        );
        let res = results(&seen);
        assert!(res[0].starts_with("tool error: agent slow"), "{res:?}");
    }

    #[test]
    fn interrupting_the_parent_cancels_its_children() {
        let (runner, _, _) =
            runner("cancel", Ruleset::default(), |r| match (is_child(r), last_is_tool(r)) {
                (false, false) => calls(&[spawn("slow", "a"), spawn("slow", "b")]),
                _ => say("parent done"),
            });
        let mut running = 0;
        let started = Instant::now();
        let (outcome, payloads) = drive(runner, |core, e| {
            if matches!(
                e.payload,
                Payload::Agent(AgentEvent::State {
                    state: AgentState::Running,
                    ..
                })
            ) {
                running += 1;
                if running == 2 {
                    core.interrupt(e.task);
                }
            }
        });
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(matches!(outcome, Outcome::Cancelled), "{outcome:?}");
        let cancelled = agent_events(&payloads)
            .into_iter()
            .filter(|e| {
                matches!(
                    e,
                    AgentEvent::State {
                        state: AgentState::Cancelled,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(cancelled, 2, "{payloads:?}");
    }

    /// Manual asks, auto runs without asking, plan neither offers nor runs a write.
    #[test]
    fn work_modes_decide_whether_a_write_asks_runs_or_is_refused() {
        use se_protocol::WorkMode;
        for mode in WorkMode::ALL {
            let (runner, seen, audit) = runner(mode.name(), Ruleset::default(), |r| {
                if last_is_tool(r) {
                    say("done")
                } else {
                    calls(&[("write", serde_json::json!({"path": "out.txt", "content": "hi"}))])
                }
            });
            runner.set_mode(mode);
            let mut asked = false;
            let (outcome, _) = drive(runner, |core, e| {
                asked |= matches!(e.payload, Payload::ApprovalRequired { .. });
                approve_all(core, e);
            });
            assert!(matches!(outcome, Outcome::Completed { .. }), "{mode:?}: {outcome:?}");
            let written = audit.parent().unwrap().join("ws").join("out.txt").exists();
            let offered: Vec<String> = seen.lock().unwrap()[0].tools.iter().map(|t| t.name.clone()).collect();
            match mode {
                WorkMode::Manual => assert!(asked && written, "manual asks, then writes"),
                WorkMode::Auto => assert!(!asked && written, "auto writes without asking"),
                WorkMode::Plan => {
                    assert!(!asked && !written, "plan never writes");
                    assert!(results(&seen)[0].contains("plan mode"), "{:?}", results(&seen));
                    assert!(!offered.iter().any(|t| t == "write" || t == "edit" || t == "shell"), "{offered:?}");
                }
            }
        }
    }

    /// A policy deny rule wins even in auto mode.
    #[test]
    fn auto_mode_never_overrides_a_deny_rule() {
        let rules = Ruleset::from_rules(vec![se_permission::Rule::deny_tool("write")]);
        let (runner, seen, audit) = runner("auto-deny", rules, |r| {
            if last_is_tool(r) {
                say("done")
            } else {
                calls(&[("write", serde_json::json!({"path": "out.txt", "content": "hi"}))])
            }
        });
        runner.set_mode(se_protocol::WorkMode::Auto);
        drive(runner, approve_all);
        assert!(!audit.parent().unwrap().join("ws").join("out.txt").exists());
        assert!(results(&seen)[0].contains("denied by policy"), "{:?}", results(&seen));
    }

    #[test]
    fn approval_prompts_name_the_agent_and_come_one_at_a_time() {
        // The default ruleset asks for every read.
        let (runner, _, audit) = runner(
            "queue",
            Ruleset::default(),
            reader_script(vec![spawn("reader", "one"), spawn("reader", "two")]),
        );
        let mut pending: Option<(se_protocol::TaskId, se_protocol::ToolCallId, Instant)> = None;
        let mut prompts: Vec<String> = Vec::new();
        let mut overlapped = false;
        let core = crate::start(runner);
        core.submit("go", vec![]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut payloads = Vec::new();
        let outcome = 'run: loop {
            assert!(Instant::now() < deadline, "never finished: {payloads:?}");
            for e in core.drain_blocking(Duration::from_millis(20)) {
                if let Payload::ApprovalRequired { name, .. } = &e.payload {
                    if name == AGENT_TOOL {
                        approve_spawns(&core, &e);
                        continue;
                    }
                }
                if let Payload::ApprovalRequired { call, summary, .. } = &e.payload {
                    overlapped |= pending.is_some();
                    prompts.push(summary.clone());
                    pending = Some((e.task, *call, Instant::now()));
                }
                payloads.push(e.payload.clone());
                if let Payload::TaskFinished { outcome, .. } = e.payload {
                    break 'run outcome;
                }
            }
            // Held a while, so a second prompt would have had time to show.
            if let Some((task, call, at)) = pending {
                if at.elapsed() > Duration::from_millis(300) {
                    core.approve(se_protocol::ApprovalDecision {
                        task,
                        call,
                        allow: true,
                    });
                    pending = None;
                }
            }
        };
        core.shutdown();
        assert!(matches!(outcome, Outcome::Completed { .. }), "{outcome:?}");
        assert!(!overlapped, "a second prompt showed before the first was answered");
        assert_eq!(prompts.len(), 2, "{prompts:?}");
        for p in &prompts {
            assert!(
                p.starts_with("[reader #a") && p.contains("] wants to run read: "),
                "{p}"
            );
        }
        assert!(agent_events(&payloads).iter().any(|e| matches!(
            e,
            AgentEvent::State {
                state: AgentState::WaitingApproval,
                ..
            }
        )));
        let records = crate::audit_log::read(&audit).unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|r| r["tool"] == "read" && r["effect"] == "Allow")
                .count(),
            2,
            "{records:?}"
        );
    }

    #[test]
    fn an_unknown_role_is_refused_with_near_matches() {
        let (runner, seen, _) = runner("unknown", Ruleset::default(), |r| {
            if last_is_tool(r) {
                say("parent done")
            } else {
                calls(&[spawn("reader-agent", "x")])
            }
        });
        let (_, payloads) = drive(runner, |_, _| {});
        assert!(spawned(&payloads).is_empty());
        let res = results(&seen);
        assert!(
            res[0].contains("no agent `reader-agent`") && res[0].contains("near matches: reader"),
            "{res:?}"
        );
    }

    #[test]
    fn background_runs_in_the_foreground_and_says_so() {
        let (runner, seen, _) =
            runner("bg", Ruleset::default(), |r| match (is_child(r), last_is_tool(r)) {
                (false, false) => calls(&[(
                    "agent",
                    serde_json::json!({"role": "spender", "task": "t", "background": true}),
                )]),
                (false, true) => say("parent done"),
                (true, _) => say("bg answer"),
            });
        drive(runner, |_, _| {});
        let res = results(&seen);
        assert!(
            res[0].contains("not supported yet") && res[0].ends_with("bg answer"),
            "{res:?}"
        );
    }

    #[test]
    fn a_long_answer_is_truncated_with_a_note() {
        let long = "x".repeat(RESULT_CHARS + 5);
        let cut = truncate(&long);
        assert!(cut.starts_with(&"x".repeat(RESULT_CHARS)));
        assert!(cut.contains("[truncated: the agent's answer was 8005 chars"));
        assert_eq!(truncate("short"), "short");
    }

    #[test]
    fn a_role_without_invoke_agent_is_not_offered_the_tool() {
        let (runner, seen, _) = runner("noagent", Ruleset::default(), |_| say("hi"));
        let root = runner.catalogue_root.clone().unwrap();
        std::fs::write(
            root.join("dept").join("spender.md"),
            "---\nschema_version: 1\nid: spender\nname: spender\ndepartment: dept\ntools:\n  - read-files\n---\n\nROLE:spender\n",
        )
        .unwrap();
        let core = crate::start(runner);
        core.submit_specialist("spender", "hello", vec![]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        'wait: while Instant::now() < deadline {
            for e in core.drain_blocking(Duration::from_millis(20)) {
                if e.is_terminal() {
                    break 'wait;
                }
            }
        }
        core.shutdown();
        let first = seen.lock().unwrap()[0].clone();
        assert!(!first.tools.iter().any(|t| t.name == "agent"));
        assert!(first.tools.iter().any(|t| t.name == "read"));
    }
}
