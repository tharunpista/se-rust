//! The task graph and its state machine (Phase 4 item 3, slice one).
//!
//! Pure on purpose, like `se-permission`: no threads, no files, no core.
//! Every scheduling rule lives here and is unit-tested without timing;
//! the driver only asks "what is ready?" and reports what happened.
//!
//! Nodes are keyed by a caller-chosen string id, not a `TaskId`: a
//! `TaskId` is a per-process counter, and a run must survive a restart.

use std::collections::{HashMap, HashSet};

/// One unit of work. Deliberately minimal: no permission fields (an
/// orchestrator must never author those, item 4) and no upstream output
/// wiring (also item 4, where it enters as untrusted data).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeSpec {
    pub id: String,
    /// The catalogue agent that runs this node. Always set: scheduled
    /// work is never "the user's own turn".
    pub agent: String,
    pub request: String,
    pub deps: Vec<String>,
    /// Which vendor and model run this node. `None` uses the runner's
    /// default. This is what lets one run mix companies' models.
    #[serde(default)]
    pub model: Option<se_protocol::ModelRef>,
    #[serde(default)]
    pub kind: NodeKind,
}

/// What a node is for. Review and evaluate nodes are verification
/// stages: they draw on the run's reserved review budget (item 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    #[default]
    Work,
    Review,
    /// Compares the outputs of its dependencies (an ensemble's members).
    /// Runs with no tools, and runs on whichever members completed: one
    /// vendor's outage must not erase the others' answers.
    Evaluate,
}

impl NodeKind {
    pub fn is_verification(self) -> bool {
        matches!(self, Self::Review | Self::Evaluate)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum NodeState {
    Pending,
    Ready,
    Running,
    WaitingApproval,
    /// A dependency failed, was cancelled or was itself blocked.
    Blocked,
    Completed,
    Failed,
    Cancelled,
    /// Was running when the process stopped; its side effects are
    /// unknown, so it is never re-run without a human's say (row 4.12).
    NeedsConfirmation,
}

impl NodeState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Blocked
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    Empty,
    DuplicateId(String),
    UnknownDependency {
        node: String,
        dep: String,
    },
    /// Names one node on the cycle.
    Cycle(String),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "a task graph needs at least one task"),
            Self::DuplicateId(id) => write!(f, "task id `{id}` is used twice"),
            Self::UnknownDependency { node, dep } => {
                write!(f, "task `{node}` depends on unknown task `{dep}`")
            }
            Self::Cycle(id) => write!(f, "task `{id}` is part of a dependency cycle"),
        }
    }
}

impl std::error::Error for GraphError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError {
    pub node: String,
    pub from: Option<NodeState>,
    pub attempted: &'static str,
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.from {
            None => write!(f, "no task `{}`", self.node),
            Some(s) => write!(
                f,
                "task `{}` cannot {} from {s:?}",
                self.node, self.attempted
            ),
        }
    }
}

impl std::error::Error for TransitionError {}

#[derive(Debug, Clone)]
pub struct TaskGraph {
    /// In the caller's order, which is also the tie-break for readiness.
    nodes: Vec<NodeSpec>,
    state: HashMap<String, NodeState>,
}

impl TaskGraph {
    pub fn new(nodes: Vec<NodeSpec>) -> Result<Self, GraphError> {
        if nodes.is_empty() {
            return Err(GraphError::Empty);
        }
        let mut ids = HashSet::new();
        for n in &nodes {
            if !ids.insert(n.id.as_str()) {
                return Err(GraphError::DuplicateId(n.id.clone()));
            }
        }
        for n in &nodes {
            if let Some(dep) = n.deps.iter().find(|d| !ids.contains(d.as_str())) {
                return Err(GraphError::UnknownDependency {
                    node: n.id.clone(),
                    dep: dep.clone(),
                });
            }
        }
        // Kahn's algorithm: whatever never reaches in-degree 0 is on (or
        // behind) a cycle.
        let mut indegree: HashMap<&str, usize> = nodes
            .iter()
            .map(|n| (n.id.as_str(), n.deps.len()))
            .collect();
        let mut queue: Vec<&str> = nodes
            .iter()
            .filter(|n| n.deps.is_empty())
            .map(|n| n.id.as_str())
            .collect();
        let mut seen = 0;
        while let Some(id) = queue.pop() {
            seen += 1;
            for n in nodes.iter().filter(|n| n.deps.iter().any(|d| d == id)) {
                let d = indegree
                    .get_mut(n.id.as_str())
                    .expect("every node has an in-degree entry");
                *d -= 1;
                if *d == 0 {
                    queue.push(&n.id);
                }
            }
        }
        if seen < nodes.len() {
            let stuck = nodes
                .iter()
                .find(|n| indegree[n.id.as_str()] > 0)
                .expect("fewer nodes seen than exist means one is still blocked");
            return Err(GraphError::Cycle(stuck.id.clone()));
        }
        let state = nodes
            .iter()
            .map(|n| (n.id.clone(), NodeState::Pending))
            .collect();
        let mut graph = Self { nodes, state };
        graph.settle();
        Ok(graph)
    }

    pub fn nodes(&self) -> &[NodeSpec] {
        &self.nodes
    }

    pub fn node(&self, id: &str) -> Option<&NodeSpec> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn state(&self, id: &str) -> Option<NodeState> {
        self.state.get(id).copied()
    }

    /// Ready nodes, in the caller's original order.
    pub fn ready(&self) -> Vec<&str> {
        self.nodes
            .iter()
            .filter(|n| self.state[&n.id] == NodeState::Ready)
            .map(|n| n.id.as_str())
            .collect()
    }

    /// Every node is terminal. A `NeedsConfirmation` node keeps a run
    /// open until a human resolves it.
    pub fn is_done(&self) -> bool {
        self.state.values().all(|s| s.is_terminal())
    }

    pub fn start(&mut self, id: &str) -> Result<(), TransitionError> {
        self.transition(id, "start", &[NodeState::Ready], NodeState::Running)
    }

    pub fn waiting_approval(&mut self, id: &str) -> Result<(), TransitionError> {
        self.transition(
            id,
            "wait for approval",
            &[NodeState::Running],
            NodeState::WaitingApproval,
        )
    }

    pub fn approval_answered(&mut self, id: &str) -> Result<(), TransitionError> {
        self.transition(
            id,
            "resume",
            &[NodeState::WaitingApproval],
            NodeState::Running,
        )
    }

    /// Records how a running node ended, then propagates.
    pub fn finish(&mut self, id: &str, ok: bool) -> Result<(), TransitionError> {
        let to = if ok {
            NodeState::Completed
        } else {
            NodeState::Failed
        };
        self.transition(
            id,
            "finish",
            &[NodeState::Running, NodeState::WaitingApproval],
            to,
        )?;
        self.settle();
        Ok(())
    }

    /// Cancels `id` and every descendant that has not already ended,
    /// including one waiting on approval. Returns the nodes that were
    /// running or waiting, which the driver must interrupt.
    pub fn cancel(&mut self, id: &str) -> Result<Vec<String>, TransitionError> {
        let Some(from) = self.state(id) else {
            return Err(TransitionError {
                node: id.to_string(),
                from: None,
                attempted: "cancel",
            });
        };
        if from.is_terminal() {
            return Err(TransitionError {
                node: id.to_string(),
                from: Some(from),
                attempted: "cancel",
            });
        }
        let mut targets = vec![id.to_string()];
        targets.extend(self.descendants(id));
        Ok(self.cancel_nodes(targets))
    }

    /// Cancels every node that has not ended. Returns those to interrupt.
    pub fn cancel_all(&mut self) -> Vec<String> {
        let all = self.nodes.iter().map(|n| n.id.clone()).collect();
        self.cancel_nodes(all)
    }

    /// A running node that stopped before doing anything with side
    /// effects goes back to `Ready`, to run again later.
    pub fn requeue(&mut self, id: &str) -> Result<(), TransitionError> {
        self.transition(
            id,
            "be requeued",
            &[NodeState::Running, NodeState::WaitingApproval],
            NodeState::Ready,
        )
    }

    /// For resume: a node recorded as running or waiting when the
    /// process stopped is marked as needing confirmation.
    pub fn mark_interrupted(&mut self, id: &str) -> Result<(), TransitionError> {
        self.transition(
            id,
            "be marked interrupted",
            &[NodeState::Running, NodeState::WaitingApproval],
            NodeState::NeedsConfirmation,
        )
    }

    /// A human resolves an interrupted node: either its work did happen
    /// (`Completed`), or it may be run again (`Ready`).
    pub fn confirm(&mut self, id: &str, it_completed: bool) -> Result<(), TransitionError> {
        let to = if it_completed {
            NodeState::Completed
        } else {
            NodeState::Ready
        };
        self.transition(id, "confirm", &[NodeState::NeedsConfirmation], to)?;
        self.settle();
        Ok(())
    }

    /// Restores a node's state from a journal, bypassing transition
    /// rules. Only for replaying a record this graph itself produced.
    pub(crate) fn restore(&mut self, id: &str, to: NodeState) -> bool {
        match self.state.get_mut(id) {
            Some(s) => {
                *s = to;
                true
            }
            None => false,
        }
    }

    pub(crate) fn resettle(&mut self) {
        self.settle();
    }

    fn cancel_nodes(&mut self, ids: Vec<String>) -> Vec<String> {
        let mut interrupt = Vec::new();
        for id in ids {
            let s = self
                .state
                .get_mut(&id)
                .expect("cancel targets come from this graph");
            if s.is_terminal() {
                continue;
            }
            if matches!(*s, NodeState::Running | NodeState::WaitingApproval) {
                interrupt.push(id.clone());
            }
            *s = NodeState::Cancelled;
        }
        self.settle();
        interrupt
    }

    fn descendants(&self, id: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut frontier = vec![id.to_string()];
        while let Some(cur) = frontier.pop() {
            for n in self.nodes.iter().filter(|n| n.deps.contains(&cur)) {
                if !out.contains(&n.id) {
                    out.push(n.id.clone());
                    frontier.push(n.id.clone());
                }
            }
        }
        out
    }

    /// Pending nodes become Ready when every dependency completed, or
    /// Blocked when any dependency ended without completing. Repeats
    /// until nothing changes, so blocking is transitive.
    fn settle(&mut self) {
        loop {
            let mut changed = false;
            for n in &self.nodes {
                if self.state[&n.id] != NodeState::Pending {
                    continue;
                }
                let deps: Vec<NodeState> = n.deps.iter().map(|d| self.state[d]).collect();
                let next = if n.kind == NodeKind::Evaluate {
                    // Runs on the survivors once every member has ended.
                    if !deps.iter().all(|s| s.is_terminal()) {
                        continue;
                    } else if deps.contains(&NodeState::Completed) {
                        NodeState::Ready
                    } else {
                        NodeState::Blocked
                    }
                } else if deps
                    .iter()
                    .any(|s| s.is_terminal() && *s != NodeState::Completed)
                {
                    NodeState::Blocked
                } else if deps.iter().all(|s| *s == NodeState::Completed) {
                    NodeState::Ready
                } else {
                    continue;
                };
                self.state.insert(n.id.clone(), next);
                changed = true;
            }
            if !changed {
                break;
            }
        }
    }

    fn transition(
        &mut self,
        id: &str,
        attempted: &'static str,
        allowed_from: &[NodeState],
        to: NodeState,
    ) -> Result<(), TransitionError> {
        let from = self.state(id);
        match from {
            Some(s) if allowed_from.contains(&s) => {
                self.state.insert(id.to_string(), to);
                Ok(())
            }
            _ => Err(TransitionError {
                node: id.to_string(),
                from,
                attempted,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, deps: &[&str]) -> NodeSpec {
        NodeSpec {
            id: id.into(),
            agent: "backend-lead".into(),
            request: format!("do {id}"),
            deps: deps.iter().map(|d| d.to_string()).collect(),
            model: None,
            kind: NodeKind::Work,
        }
    }

    #[test]
    fn invalid_graphs_are_refused_with_the_reason() {
        assert_eq!(TaskGraph::new(vec![]).unwrap_err(), GraphError::Empty);
        assert_eq!(
            TaskGraph::new(vec![node("a", &[]), node("a", &[])]).unwrap_err(),
            GraphError::DuplicateId("a".into())
        );
        assert_eq!(
            TaskGraph::new(vec![node("a", &["ghost"])]).unwrap_err(),
            GraphError::UnknownDependency {
                node: "a".into(),
                dep: "ghost".into()
            }
        );
        assert!(matches!(
            TaskGraph::new(vec![node("a", &["a"])]),
            Err(GraphError::Cycle(_))
        ));
        assert!(matches!(
            TaskGraph::new(vec![
                node("a", &["c"]),
                node("b", &["a"]),
                node("c", &["b"]),
                node("d", &[])
            ]),
            Err(GraphError::Cycle(_))
        ));
    }

    /// Drives the graph the way a sequential driver would, starting
    /// every ready node then finishing them in order, and returns the
    /// event log as (node, "start"|"finish").
    fn run_to_end(graph: &mut TaskGraph) -> Vec<(String, &'static str)> {
        let mut log = Vec::new();
        while !graph.is_done() {
            let ready: Vec<String> = graph.ready().iter().map(|s| s.to_string()).collect();
            assert!(!ready.is_empty(), "stuck: nothing ready but not done");
            for id in &ready {
                graph.start(id).unwrap();
                log.push((id.clone(), "start"));
            }
            for id in &ready {
                graph.finish(id, true).unwrap();
                log.push((id.clone(), "finish"));
            }
        }
        log
    }

    /// Row 4.6: for every edge a→b, start(b) comes after finish(a), on a
    /// diamond joined to a long chain.
    #[test]
    fn dependency_ordering_holds_on_a_diamond_and_a_chain() {
        let nodes = vec![
            node("top", &[]),
            node("left", &["top"]),
            node("right", &["top"]),
            node("join", &["left", "right"]),
            node("c1", &["join"]),
            node("c2", &["c1"]),
            node("c3", &["c2"]),
            node("c4", &["c3"]),
            node("free", &[]),
        ];
        let mut graph = TaskGraph::new(nodes.clone()).unwrap();
        let log = run_to_end(&mut graph);
        let at = |id: &str, what| log.iter().position(|(n, w)| n == id && *w == what).unwrap();
        for n in &nodes {
            for dep in &n.deps {
                assert!(
                    at(&n.id, "start") > at(dep, "finish"),
                    "{} started before {dep} finished: {log:?}",
                    n.id
                );
            }
        }
        assert!(nodes
            .iter()
            .all(|n| graph.state(&n.id) == Some(NodeState::Completed)));
    }

    #[test]
    fn a_failure_blocks_its_dependents_transitively_but_not_siblings() {
        let mut graph = TaskGraph::new(vec![
            node("a", &[]),
            node("b", &["a"]),
            node("c", &["b"]),
            node("sibling", &[]),
        ])
        .unwrap();
        graph.start("a").unwrap();
        graph.start("sibling").unwrap();
        graph.finish("a", false).unwrap();
        assert_eq!(graph.state("b"), Some(NodeState::Blocked));
        assert_eq!(graph.state("c"), Some(NodeState::Blocked));
        assert_eq!(graph.state("sibling"), Some(NodeState::Running));
        graph.finish("sibling", true).unwrap();
        assert!(graph.is_done());
    }

    /// Row 4.10: cancelling a parent cancels every descendant, a
    /// waiting-approval one included, and nothing below it starts again.
    #[test]
    fn cancelling_a_node_cancels_every_descendant_and_nothing_starts_after() {
        let mut graph = TaskGraph::new(vec![
            node("parent", &[]),
            node("child", &["parent"]),
            node("grandchild", &["child"]),
            node("waiting", &[]),
            node("under_waiting", &["waiting"]),
            node("unrelated", &[]),
        ])
        .unwrap();
        graph.start("waiting").unwrap();
        graph.waiting_approval("waiting").unwrap();
        graph.start("parent").unwrap();

        let interrupt = graph.cancel("parent").unwrap();
        assert_eq!(interrupt, ["parent"]);
        for id in ["parent", "child", "grandchild"] {
            assert_eq!(graph.state(id), Some(NodeState::Cancelled), "{id}");
        }
        let interrupt = graph.cancel("waiting").unwrap();
        assert_eq!(
            interrupt,
            ["waiting"],
            "a waiting-approval node is interrupted"
        );
        assert_eq!(graph.state("under_waiting"), Some(NodeState::Cancelled));

        assert_eq!(
            graph.ready(),
            ["unrelated"],
            "only the unrelated node may run"
        );
        assert!(graph.start("child").is_err());
        assert!(
            graph.finish("parent", true).is_err(),
            "a late finish cannot revive it"
        );
    }

    #[test]
    fn terminal_states_never_change() {
        let mut graph = TaskGraph::new(vec![node("a", &[])]).unwrap();
        graph.start("a").unwrap();
        graph.finish("a", true).unwrap();
        assert!(graph.finish("a", false).is_err());
        assert!(graph.cancel("a").is_err());
        assert!(graph.mark_interrupted("a").is_err());
        assert_eq!(graph.state("a"), Some(NodeState::Completed));
        assert!(graph.cancel_all().is_empty());
    }

    #[test]
    fn an_interrupted_node_waits_for_a_human_and_holds_its_dependents() {
        let mut graph = TaskGraph::new(vec![node("a", &[]), node("b", &["a"])]).unwrap();
        graph.start("a").unwrap();
        graph.mark_interrupted("a").unwrap();
        assert!(!graph.is_done(), "an unresolved node keeps the run open");
        assert!(
            graph.ready().is_empty(),
            "nothing runs past an unknown outcome"
        );
        assert!(graph.start("a").is_err(), "never silently re-run");

        let mut rerun = graph.clone();
        rerun.confirm("a", false).unwrap();
        assert_eq!(rerun.ready(), ["a"], "a human may allow a re-run");

        graph.confirm("a", true).unwrap();
        assert_eq!(graph.ready(), ["b"]);
    }
}
