//! Team runs through the real `ProviderRunner` (Phase 4 rows 4.2–4.4,
//! 4.13–4.17): real tool loop, real file system, mock vendors with
//! distinct identities. A child module of `provider_runner`, so it can
//! build a runner without a configured credential.

use super::*;
use crate::team::{file, Limits, NodeKind, NodeSpec, NodeState, TaskGraph, TeamRun};
use se_protocol::{ModelRef, Payload};
use se_provider::{ModelInfo, ModelProvider, Status};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A vendor under the test's control. A user message containing
/// `write:<path>` makes it call the `write` tool once (content = its own
/// id); after a tool result it says `done`; otherwise it answers
/// `<id> says: <first line>`. Reports `usage` per call when set.
struct Vendor {
    id: String,
    usage: Option<(u64, u64)>,
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

impl ModelProvider for Vendor {
    fn id(&self) -> &str {
        &self.id
    }
    fn display_name(&self) -> &str {
        &self.id
    }
    fn env_var(&self) -> &str {
        "SE_TEST_UNUSED"
    }
    fn detect(&self) -> Status {
        Status {
            configured: true,
            detail: "test".into(),
            source: se_provider::Source::Environment,
        }
    }
    fn stream(
        &self,
        request: &ChatRequest,
        _cancelled: &dyn Fn() -> bool,
        on_event: &mut dyn FnMut(StreamEvent) -> bool,
    ) -> Result<(), ProviderError> {
        self.seen.lock().unwrap().push(request.clone());
        let last = request.messages.last().expect("a request has messages");
        // Fails only an evaluator's call (its request opens this way),
        // when the task asked for it.
        if last.content.starts_with("Compare these answers") && last.content.contains("FAIL-EVAL") {
            return Err(ProviderError::Server {
                status: 503,
                detail: "evaluator vendor down".into(),
            });
        }
        let mut events = Vec::new();
        if last.role == Role::Tool {
            events.push(StreamEvent::TextDelta("done".into()));
            events.push(StreamEvent::FinishReason("stop".into()));
        } else if last.content.contains("shell:append") {
            let (program, args) = append_marker_command();
            events.push(StreamEvent::ToolCallRequested {
                id: "call-1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"program": program, "args": args}).to_string(),
            });
            events.push(StreamEvent::FinishReason("tool_calls".into()));
        } else if let Some(path) = last
            .content
            .split("write:")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
        {
            events.push(StreamEvent::ToolCallRequested {
                id: "call-1".into(),
                name: "write".into(),
                arguments: serde_json::json!({"path": path, "content": self.id}).to_string(),
            });
            events.push(StreamEvent::FinishReason("tool_calls".into()));
        } else {
            let first = last.content.lines().next().unwrap_or_default();
            events.push(StreamEvent::TextDelta(format!("{} says: {first}", self.id)));
            events.push(StreamEvent::FinishReason("stop".into()));
        }
        if let Some((i, o)) = self.usage {
            events.push(StreamEvent::Usage {
                input_tokens: Some(i),
                output_tokens: Some(o),
                cached_input_tokens: None,
            });
        }
        for e in events {
            if !on_event(e) {
                return Err(ProviderError::Cancelled);
            }
        }
        Ok(())
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

/// A real shell command that appends one line to `marker.txt` in the
/// workspace, then lingers a moment, so a test can count real runs.
fn append_marker_command() -> (&'static str, Vec<&'static str>) {
    if cfg!(windows) {
        (
            "cmd",
            vec!["/c", "echo ran>>marker.txt & ping -n 2 127.0.0.1 >nul"],
        )
    } else {
        ("sh", vec!["-c", "echo ran >> marker.txt; sleep 1"])
    }
}

fn marker_lines(ws: &Path) -> usize {
    std::fs::read_to_string(ws.join("marker.txt")).map_or(0, |s| s.lines().count())
}

type Seen = Arc<Mutex<Vec<ChatRequest>>>;

/// A runner whose default vendor is the first of `vendors`, with every
/// tool call pre-approved (claims, not approvals, are under test).
fn runner(ws: &Path, vendors: &[(&str, Option<(u64, u64)>)]) -> (ProviderRunner, Vec<Seen>) {
    let mut registry = Registry::empty();
    let mut seen = Vec::new();
    for (id, usage) in vendors {
        let s: Seen = Arc::default();
        registry.register(Box::new(Vendor {
            id: id.to_string(),
            usage: *usage,
            seen: s.clone(),
        }));
        seen.push(s);
    }
    let runner = ProviderRunner {
        registry,
        provider_id: vendors[0].0.to_string(),
        model: "m".into(),
        turn_deadline: DEFAULT_TURN_DEADLINE,
        ruleset: Ruleset::from_rules(vec![se_permission::Rule::allow_all()]),
        ledger: NonceLedger::new(),
        catalogue_root: None,
        workspace_root: ws.to_path_buf(),
        audit: Arc::new(NullAuditSink),
        auto_route: false,
        compact_at_tokens: None,
        sticky_specialist: Default::default(),
        mode: Default::default(),
        router_model: None,
        role_models: Default::default(),
        team: None,
    };
    (runner, seen)
}

fn workspace(name: &str) -> PathBuf {
    let ws = std::env::temp_dir().join(format!(
        "se-team-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&ws).unwrap();
    ws
}

/// A catalogue of minimal specialists, one per id, in `<ws>/catalogue`.
fn catalogue(ws: &Path, ids: &[&str]) -> PathBuf {
    let root = ws.join("catalogue");
    std::fs::create_dir_all(root.join("dept")).unwrap();
    for id in ids {
        std::fs::write(
            root.join(format!("dept/{id}.md")),
            format!(
                "---\nschema_version: 1\nid: {id}\nname: {id}\ndepartment: dept\n---\n\nYou are {id}.\n"
            ),
        )
        .unwrap();
    }
    root
}

fn node(id: &str, request: &str, deps: &[&str]) -> NodeSpec {
    NodeSpec {
        id: id.into(),
        agent: String::new(),
        request: request.into(),
        deps: deps.iter().map(|d| d.to_string()).collect(),
        model: None,
        kind: NodeKind::Work,
    }
}

fn on(mut n: NodeSpec, vendor: &str) -> NodeSpec {
    n.model = Some(ModelRef {
        provider: vendor.into(),
        model: format!("{vendor}-model"),
    });
    n
}

/// Runs to the end; returns every (node, payload) seen.
fn drive(run: &mut TeamRun) -> Vec<(String, Payload)> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut log = Vec::new();
    while !run.is_done() {
        assert!(Instant::now() < deadline, "run never finished: {log:?}");
        for e in run.tick_blocking(Duration::from_millis(20)) {
            let n = run.node_of(e.task).unwrap_or("?").to_string();
            log.push((n, e.payload));
        }
        if run.budget_stalled().is_some() {
            break;
        }
    }
    log
}

fn tool_results(log: &[(String, Payload)], node: &str) -> Vec<(bool, String)> {
    log.iter()
        .filter(|(n, _)| n == node)
        .filter_map(|(_, p)| match p {
            Payload::ToolCompleted { ok, preview, .. } => Some((*ok, preview.clone())),
            _ => None,
        })
        .collect()
}

fn final_text(run: &TeamRun, node: &str) -> String {
    match run.outcome(node) {
        Some(Outcome::Completed { final_text }) => final_text.clone(),
        other => panic!("{node} did not complete: {other:?}"),
    }
}

/// Row 4.13: two workers, one path. The second write is refused with a
/// typed conflict (not merged, not last-write-wins) and the file keeps
/// the first writer's content.
#[test]
fn a_second_writer_of_one_file_is_refused_with_a_conflict() {
    let ws = workspace("conflict");
    let (r, _) = runner(&ws, &[("p1", None)]);
    let graph = TaskGraph::new(vec![
        node("a", "write:shared.txt", &[]),
        node("b", "write:shared.txt", &[]),
    ])
    .unwrap();
    let mut run = TeamRun::start(graph, r, 1);
    let log = drive(&mut run);
    assert_eq!(
        std::fs::read_to_string(ws.join("shared.txt")).unwrap(),
        "p1"
    );
    let (a, b) = (tool_results(&log, "a"), tool_results(&log, "b"));
    assert!(a.iter().any(|(ok, _)| *ok), "a wrote it: {a:?}");
    assert!(
        b.iter()
            .any(|(ok, t)| !*ok && t.contains("file conflict") && t.contains("`a`")),
        "b is refused, naming the owner: {b:?}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// A pipeline hand-off: a task may change the file its upstream wrote.
#[test]
fn a_dependent_may_overwrite_its_upstreams_file() {
    let ws = workspace("handoff");
    let (r, _) = runner(&ws, &[("p1", None), ("p2", None)]);
    let graph = TaskGraph::new(vec![
        node("a", "write:f.txt", &[]),
        on(node("b", "write:f.txt", &["a"]), "p2"),
    ])
    .unwrap();
    let mut run = TeamRun::start(graph, r, 2);
    drive(&mut run);
    assert_eq!(std::fs::read_to_string(ws.join("f.txt")).unwrap(), "p2");
    let _ = std::fs::remove_dir_all(&ws);
}

/// Row 4.14 on Windows: a path differing only in case is the same file
/// (refused), a path over 260 characters works, and a file held open
/// without sharing fails with an explicit error, never silently.
#[cfg(windows)]
#[test]
fn windows_case_long_paths_and_locked_files_are_handled_explicitly() {
    use std::os::windows::fs::OpenOptionsExt;
    let ws = workspace("win");
    let deep: PathBuf = (0..6).map(|i| format!("{i}{}", "d".repeat(45))).collect();
    std::fs::create_dir_all(ws.join(&deep)).unwrap();
    let long_rel = deep.join("long.txt");
    assert!(ws.join(&long_rel).to_string_lossy().len() > 260);
    std::fs::write(ws.join("locked.txt"), "original").unwrap();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(ws.join("locked.txt"))
        .unwrap();

    let (r, _) = runner(&ws, &[("p1", None)]);
    let graph = TaskGraph::new(vec![
        node("a", "write:Dir.txt", &[]),
        node("b", "write:dir.TXT", &[]),
        node("long", &format!("write:{}", long_rel.display()), &[]),
        node("lock", "write:locked.txt", &[]),
    ])
    .unwrap();
    let mut run = TeamRun::start(graph, r, 1);
    let log = drive(&mut run);
    drop(held);

    assert!(
        tool_results(&log, "b")
            .iter()
            .any(|(ok, t)| !*ok && t.contains("file conflict")),
        "case-only difference is one file"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join(&long_rel)).unwrap(),
        "p1",
        "long path written"
    );
    let lock = tool_results(&log, "lock");
    assert!(
        lock.iter().any(|(ok, t)| !*ok && !t.is_empty()),
        "a locked file is an explicit failure: {lock:?}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("locked.txt")).unwrap(),
        "original"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// Rows 4.3/4.4: every ensemble member receives the same packet (only
/// the model differs), no member sees another's answer, and the
/// evaluator is offered no tools and sees every answer.
#[test]
fn ensemble_members_get_identical_isolated_packets() {
    let ws = workspace("ensemble");
    let (r, seen) = runner(&ws, &[("p1", None), ("p2", None), ("p3", None)]);
    let team = file::parse(
        "tasks:
  - id: plan
    request: Plan the orders table.
  - id: db
    deps: [plan]
    ensemble: [p2/m2, p3/m3]
    request: Which database fits?
",
    )
    .unwrap();
    let mut run = TeamRun::start(team.graph, r, 3);
    drive(&mut run);

    let member = |i: usize| seen[i].lock().unwrap()[0].clone();
    let (a, b) = (member(1), member(2));
    assert_ne!(a.model, b.model);
    assert_eq!(a.messages, b.messages, "byte-identical packets");
    assert_eq!(a.tools, b.tools);
    let text = |r: &ChatRequest| format!("{:?}", r.messages);
    assert!(!text(&a).contains("p3 says") && !text(&b).contains("p2 says"));

    let eval = seen[0].lock().unwrap().last().unwrap().clone();
    assert!(eval.tools.is_empty(), "the evaluator has no tools");
    assert!(text(&eval).contains("p2 says") && text(&eval).contains("p3 says"));
    let report = final_text(&run, "db");
    assert!(report.contains("Ensemble comparison"));
    assert!(report.contains("p2 says") && report.contains("p3 says"));
    let _ = std::fs::remove_dir_all(&ws);
}

/// Rows 4.2/4.15/4.17 in one recorded run: three roles' worth of nodes
/// (a plan, two parallel workers, a review), two vendors, a conflict
/// refused, and a journal from which agent, vendor and usage per node
/// can be read back; a vendor reporting nothing reads "not reported".
#[test]
fn a_full_team_run_leaves_an_inspectable_attributed_record() {
    let ws = workspace("full");
    let (r, _) = runner(&ws, &[("p1", Some((100, 20))), ("p2", None)]);
    let r = r.with_catalogue_root(catalogue(&ws, &["architect", "developer", "qa"]));
    let as_agent = |mut n: NodeSpec, agent: &str| {
        n.agent = agent.into();
        n
    };
    let mut review = as_agent(
        on(node("review", "Review both.", &["code", "docs"]), "p2"),
        "qa",
    );
    review.kind = NodeKind::Review;
    let graph = TaskGraph::new(vec![
        as_agent(node("plan", "Plan it.", &[]), "architect"),
        as_agent(
            on(node("code", "write:out.rs", &["plan"]), "p2"),
            "developer",
        ),
        as_agent(node("docs", "write:out.rs", &["plan"]), "developer"),
        review,
    ])
    .unwrap();
    let journal = ws.join(".se/runs/run.jsonl");
    let mut run = TeamRun::start_journaled(graph, r, 2, &journal).unwrap();
    drive(&mut run);

    for n in ["plan", "code", "docs", "review"] {
        assert_eq!(run.graph().state(n), Some(NodeState::Completed), "{n}");
    }
    let rec = |n: &str| run.record(n).cloned().unwrap();
    assert_eq!(rec("plan").provider.as_deref(), Some("p1"));
    assert_eq!(rec("plan").tokens(), Some(120), "p1's reported usage");
    assert_eq!(rec("code").provider.as_deref(), Some("p2"));
    assert_eq!(rec("code").usage, None, "p2 reports nothing: not zero");
    // Exactly one of the two parallel writers got the file.
    let content = std::fs::read_to_string(ws.join("out.rs")).unwrap();
    assert!(content == "p1" || content == "p2");

    let r = crate::team::journal::resume(&journal).unwrap();
    let vendors: std::collections::HashSet<_> = r
        .records
        .values()
        .filter_map(|x| x.provider.clone())
        .collect();
    let agents: std::collections::HashSet<_> =
        r.records.values().filter_map(|x| x.agent.clone()).collect();
    assert_eq!(r.records.len(), 4, "every node attributed on disk");
    assert!(vendors.len() >= 2, "{vendors:?}");
    assert_eq!(
        agents.len(),
        3,
        "three specialist roles on disk: {agents:?}"
    );
    assert_eq!(r.records["plan"].tokens(), Some(120));
    assert_eq!(r.limits.unwrap().max_concurrent, 2, "limits journaled");
    let _ = std::fs::remove_dir_all(&ws);
}

/// Row 4.16: at the cap the next call is not issued (the vendor
/// receives no request) and the run waits for a human; raising the
/// budget lets it continue. Review work may spend the reserve.
#[test]
fn a_spent_budget_issues_no_further_call_until_a_human_raises_it() {
    let ws = workspace("budget");
    let (r, seen) = runner(&ws, &[("p1", Some((60, 10))), ("p2", Some((5, 5)))]);
    let mut check = on(node("check", "Check it.", &["a"]), "p2");
    check.kind = NodeKind::Review;
    let graph = TaskGraph::new(vec![
        node("a", "First.", &[]),
        on(node("b", "Second.", &["a"]), "p2"),
        check,
    ])
    .unwrap();
    let limits = Limits {
        max_concurrent: 1,
        budget_tokens: Some(100),
        review_reserve_tokens: 40,
        ..Limits::default()
    };
    let mut run = TeamRun::start(graph, r, limits);
    drive(&mut run);

    // a spent 70 of the 60 non-reserve tokens: b may not start, but the
    // review node may run on the reserve.
    assert_eq!(run.graph().state("check"), Some(NodeState::Completed));
    assert_eq!(run.graph().state("b"), Some(NodeState::Ready));
    let b_calls = seen[1]
        .lock()
        .unwrap()
        .iter()
        .filter(|req| format!("{:?}", req.messages).contains("Second."))
        .count();
    assert_eq!(b_calls, 0, "no request was issued for b");
    assert!(run.budget_stalled().unwrap().contains("budget"));

    run.raise_budget(100);
    drive(&mut run);
    assert_eq!(run.graph().state("b"), Some(NodeState::Completed));
    let _ = std::fs::remove_dir_all(&ws);
}

/// Item 2 for scheduled work: an agent-less node on a vendor the rules
/// deny is refused before routing, so that vendor receives no request
/// at all, and the refusal is in the audit file under `auto-router`.
#[test]
fn a_denied_vendor_gets_no_routing_calls_and_the_refusal_is_audited() {
    let ws = workspace("deny");
    let (mut r, seen) = runner(&ws, &[("p1", None), ("p2", None)]);
    r.ruleset = Ruleset::from_rules(vec![
        se_permission::Rule::allow_all(),
        se_permission::Rule::deny_tool("use_provider"),
    ]);
    let audit = ws.join("audit.jsonl");
    r.audit = Arc::new(crate::audit_log::FileAuditSink::open(&audit).unwrap());
    let r = r
        .with_catalogue_root(catalogue(&ws, &["developer"]))
        .with_auto_route(true);
    let graph = TaskGraph::new(vec![on(node("x", "Build it.", &[]), "p2")]).unwrap();
    let mut run = TeamRun::start(graph, r, 1);
    drive(&mut run);

    assert_eq!(run.graph().state("x"), Some(NodeState::Failed));
    assert!(seen[1].lock().unwrap().is_empty(), "p2 was never called");
    let lines = crate::audit_log::read(&audit).unwrap();
    assert!(
        lines.iter().any(|l| l["tool"] == "use_provider"
            && l["agent"] == "auto-router"
            && l["effect"] == "Deny"),
        "{lines:?}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// Item 6: when the evaluator's own call fails, the comparison still
/// arrives with every member's answer, says plainly that nothing was
/// compared, and never reads as agreement; dependents still run.
#[test]
fn a_failed_evaluator_still_delivers_every_answer_marked_uncompared() {
    let ws = workspace("evalfail");
    let (r, _) = runner(&ws, &[("p1", None), ("p2", None), ("p3", None)]);
    let team = file::parse(
        "tasks:
  - id: db
    ensemble: [p2/m2, p3/m3]
    request: Which database? FAIL-EVAL
  - id: after
    deps: [db]
    request: Use the comparison.
",
    )
    .unwrap();
    let mut run = TeamRun::start(team.graph, r, 3);
    drive(&mut run);
    let report = final_text(&run, "db");
    assert!(
        report.contains("could not be read (the evaluator failed"),
        "{report}"
    );
    assert!(report.contains("p2 says") && report.contains("p3 says"));
    assert!(!report.contains("no substantive disagreement"));
    assert_eq!(run.graph().state("after"), Some(NodeState::Completed));
    let _ = std::fs::remove_dir_all(&ws);
}

/// Row 4.16, mid-turn: the routing call spends the budget, so the main
/// call is refused. Nothing with side effects ran, so the task goes back
/// to ready (not failed), waits for a human, and completes once the
/// budget is raised.
#[test]
fn a_task_stopped_by_the_budget_before_any_tool_runs_is_retried_after_a_raise() {
    let ws = workspace("requeue");
    // One routing call (the mock's answer names no department, so the
    // router falls back to keywords) costs 110 of a 100-token budget.
    let (r, seen) = runner(&ws, &[("p1", Some((90, 20)))]);
    let r = r
        .with_catalogue_root(catalogue(
            &ws,
            &["developer", crate::provider_runner::FALLBACK_AGENT_ID],
        ))
        .with_auto_route(true);
    let graph = TaskGraph::new(vec![node("x", "Build it.", &[])]).unwrap();
    let limits = Limits {
        max_concurrent: 1,
        budget_tokens: Some(100),
        ..Limits::default()
    };
    let mut run = TeamRun::start(graph, r, limits);
    drive(&mut run);
    assert_eq!(
        run.graph().state("x"),
        Some(NodeState::Ready),
        "requeued, not failed"
    );
    assert!(run.budget_stalled().is_some());
    let calls_before = seen[0].lock().unwrap().len();
    assert_eq!(calls_before, 1, "only the routing call was made");

    run.raise_budget(1_000);
    drive(&mut run);
    assert_eq!(run.graph().state("x"), Some(NodeState::Completed));
    let _ = std::fs::remove_dir_all(&ws);
}

/// Row 4.13 across a restart: who owns which file is journaled, so after
/// a resume a sibling still cannot overwrite a file the first task wrote.
#[test]
fn file_ownership_survives_a_resume() {
    let ws = workspace("claimresume");
    let journal = ws.join(".se/runs/run.jsonl");
    let graph = || {
        TaskGraph::new(vec![
            node("a", "write:owned.txt", &[]),
            node("b", "write:owned.txt", &["gate"]),
            // `gate` holds b back, so b has not run when the process
            // stops right after a finishes.
            node("gate", "Wait.", &[]),
        ])
        .unwrap()
    };
    {
        let (r, _) = runner(&ws, &[("p1", None)]);
        let mut run = TeamRun::start_journaled(graph(), r, 1, &journal).unwrap();
        // Run until `a` has written its file, then stop the process.
        let deadline = Instant::now() + Duration::from_secs(10);
        while run.graph().state("a") != Some(NodeState::Completed) {
            assert!(Instant::now() < deadline);
            run.tick_blocking(Duration::from_millis(20));
        }
        run.shutdown();
    }
    let r = crate::team::journal::resume(&journal).unwrap();
    assert!(
        r.claims
            .iter()
            .any(|(k, n)| k.ends_with("owned.txt") && n == "a"),
        "{:?}",
        r.claims
    );
    let (runner2, _) = runner(&ws, &[("p1", None)]);
    let mut run = TeamRun::resume(&journal, runner2, None).unwrap();
    for n in ["gate"] {
        if run.graph().state(n) == Some(NodeState::NeedsConfirmation) {
            run.confirm(n, false).unwrap();
        }
    }
    let log = drive(&mut run);
    assert!(
        tool_results(&log, "b")
            .iter()
            .any(|(ok, t)| !*ok && t.contains("file conflict") && t.contains("`a`")),
        "b is still refused after the resume: {:?}",
        tool_results(&log, "b")
    );
    assert_eq!(std::fs::read_to_string(ws.join("owned.txt")).unwrap(), "p1");
    let _ = std::fs::remove_dir_all(&ws);
}

/// Row 4.12: the process dies with a real shell command in flight. On
/// resume the task is marked as needing confirmation, the command is not
/// run again on its own, and confirming it as done never re-runs it.
#[test]
fn a_crash_with_a_shell_call_in_flight_never_replays_it() {
    let ws = workspace("crashshell");
    let journal = ws.join(".se/runs/run.jsonl");
    {
        let (r, _) = runner(&ws, &[("p1", None)]);
        let graph = TaskGraph::new(vec![node("x", "shell:append", &[])]).unwrap();
        let mut run = TeamRun::start_journaled(graph, r, 1, &journal).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        // ToolRequested on the allow path means execution has begun.
        'started: loop {
            assert!(Instant::now() < deadline, "the shell call never started");
            for e in run.tick_blocking(Duration::from_millis(20)) {
                if matches!(e.payload, Payload::ToolRequested { ref name, .. } if name == "shell") {
                    break 'started;
                }
            }
        }
        // "Crash": the process stops without recording another line.
        drop(run);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while marker_lines(&ws) == 0 {
        assert!(Instant::now() < deadline, "the shell command never ran");
        std::thread::sleep(Duration::from_millis(20));
    }

    let (r, seen) = runner(&ws, &[("p1", None)]);
    let mut run = TeamRun::resume(&journal, r, None).unwrap();
    assert_eq!(run.graph().state("x"), Some(NodeState::NeedsConfirmation));
    for _ in 0..5 {
        run.tick_blocking(Duration::from_millis(20));
    }
    assert!(seen[0].lock().unwrap().is_empty(), "nothing re-ran unasked");
    assert_eq!(marker_lines(&ws), 1);

    run.confirm("x", true).unwrap();
    drive(&mut run);
    assert_eq!(run.graph().state("x"), Some(NodeState::Completed));
    assert_eq!(marker_lines(&ws), 1, "confirmed as done, never replayed");
    let _ = std::fs::remove_dir_all(&ws);
}

/// A task that can overlap another may not run `shell`: its writes are
/// invisible to file claims. The command never runs.
#[test]
fn shell_is_refused_in_a_task_that_can_run_in_parallel() {
    let ws = workspace("shellpeer");
    let (r, _) = runner(&ws, &[("p1", None)]);
    let graph = TaskGraph::new(vec![
        node("x", "shell:append", &[]),
        node("y", "Other work.", &[]),
    ])
    .unwrap();
    let mut run = TeamRun::start(graph, r, 2);
    let log = drive(&mut run);
    assert!(
        tool_results(&log, "x")
            .iter()
            .any(|(ok, t)| !*ok && t.contains("shell is disabled")),
        "{:?}",
        tool_results(&log, "x")
    );
    assert_eq!(marker_lines(&ws), 0, "the command never ran");
    let _ = std::fs::remove_dir_all(&ws);
}
