//! What a team run shares with the runner doing its work, keyed by graph
//! node id (stable across restarts, unlike a `TaskId`):
//!
//! * **File claims** (Phase 4 item 7, rows 4.13/4.14). The first node to
//!   write a path owns it for the rest of the run. A node may take over a
//!   path an *ancestor* owns (a pipeline hand-off), never one a sibling
//!   or an unrelated node owns: that write is refused with a typed
//!   [`Conflict`], never merged and never last-write-wins.
//! * **The token budget** (items 9–10, row 4.16), with a reserve only
//!   review/evaluate nodes may spend, so ordinary work can never starve
//!   the verification stage.
//!
//! shortcut: only the `write` and `edit` tools are claim-checked. A
//! `shell` command can still write any file in the workspace; it is
//! gated by human approval, not by claims.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::graph::{NodeKind, TaskGraph};

/// A write refused because another node owns the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    pub owner: String,
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "file conflict: `{}` was already written by task `{}` in this run, \
             and only that task or one that depends on it may change it",
            self.path, self.owner
        )
    }
}

#[derive(Debug)]
struct NodeInfo {
    ancestors: HashSet<String>,
    kind: NodeKind,
    /// A dependency of an evaluate node: runs unrouted when it names no
    /// agent, so every member of an ensemble gets the same packet.
    ensemble_member: bool,
}

#[derive(Debug, Default)]
struct Shared {
    nodes: HashMap<String, NodeInfo>,
    /// Normalised path -> owning node.
    claims: HashMap<String, String>,
    limit_tokens: Option<u64>,
    review_reserve_tokens: u64,
    /// Spent by work nodes and by review/evaluate nodes, kept apart so
    /// work that overshoots can never eat the review reserve.
    spent_work: u64,
    spent_review: u64,
    /// Model calls whose provider reported no usage. Never counted as 0
    /// tokens: the budget simply could not see them.
    unreported_calls: u64,
}

/// Cheap to clone; every clone is the same shared state.
#[derive(Debug, Clone, Default)]
pub struct TeamContext {
    shared: Arc<Mutex<Shared>>,
}

impl TeamContext {
    pub fn new(
        graph: &TaskGraph,
        limit_tokens: Option<u64>,
        review_reserve_tokens: u64,
        spent: impl Fn(&str) -> u64,
    ) -> Self {
        let (mut spent_work, mut spent_review) = (0, 0);
        for n in graph.nodes() {
            if n.kind.is_verification() {
                spent_review += spent(&n.id);
            } else {
                spent_work += spent(&n.id);
            }
        }
        let evaluated: HashSet<&str> = graph
            .nodes()
            .iter()
            .filter(|n| n.kind == NodeKind::Evaluate)
            .flat_map(|n| n.deps.iter().map(String::as_str))
            .collect();
        let nodes = graph
            .nodes()
            .iter()
            .map(|n| {
                (
                    n.id.clone(),
                    NodeInfo {
                        ancestors: ancestors(graph, &n.id),
                        kind: n.kind,
                        ensemble_member: evaluated.contains(n.id.as_str()),
                    },
                )
            })
            .collect();
        Self {
            shared: Arc::new(Mutex::new(Shared {
                nodes,
                limit_tokens,
                review_reserve_tokens,
                spent_work,
                spent_review,
                ..Shared::default()
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn kind(&self, node: &str) -> NodeKind {
        self.lock()
            .nodes
            .get(node)
            .map_or(NodeKind::Work, |n| n.kind)
    }

    pub fn is_ensemble_member(&self, node: &str) -> bool {
        self.lock()
            .nodes
            .get(node)
            .is_some_and(|n| n.ensemble_member)
    }

    /// Whether `node` may write `path`, without claiming it. Checked
    /// before a human is asked, so nobody approves a doomed write.
    pub fn check(&self, node: &str, path: &Path) -> Result<(), Conflict> {
        let shared = self.lock();
        owner_allows(&shared, node, &key(path), path)
    }

    /// Claims `path` for `node` (or confirms it already holds it). Done
    /// right before the write, under the same lock as the check.
    pub fn claim(&self, node: &str, path: &Path) -> Result<(), Conflict> {
        let mut shared = self.lock();
        let k = key(path);
        owner_allows(&shared, node, &k, path)?;
        shared.claims.insert(k, node.to_string());
        Ok(())
    }

    /// Whether `node` may make another model call (item 9). Work may
    /// spend `limit - reserve`, minus anything review took from the
    /// shared pool. Review and evaluate nodes always have the reserve,
    /// however much work spent, plus whatever of the limit is left.
    ///
    /// shortcut: a call is charged only after it returns, so calls that
    /// are admitted at the same moment in parallel tasks can together
    /// overshoot by up to one call each. Reserve tokens per call before
    /// it is sent if a hard ceiling is ever needed.
    pub fn admit(&self, node: &str) -> Result<(), String> {
        let s = self.lock();
        let Some(limit) = s.limit_tokens else {
            return Ok(());
        };
        let reserve = s.review_reserve_tokens.min(limit);
        let verification = s.nodes.get(node).is_some_and(|n| n.kind.is_verification());
        let total = s.spent_work + s.spent_review;
        if verification {
            if s.spent_review < reserve || total < limit {
                return Ok(());
            }
            return Err(format!(
                "the run's token budget is spent ({total} of {limit} tokens, including the {reserve}-token review reserve)"
            ));
        }
        let work_ceiling = limit - reserve;
        let used = s.spent_work + s.spent_review.saturating_sub(reserve);
        if used < work_ceiling {
            Ok(())
        } else {
            Err(format!(
                "the run's token budget is spent ({used} of the {work_ceiling} tokens not reserved for review)"
            ))
        }
    }

    /// Records one model call by `node`: its reported tokens, or that it
    /// reported none.
    pub fn charge(&self, node: &str, tokens: Option<u64>) {
        let mut s = self.lock();
        let verification = s.nodes.get(node).is_some_and(|n| n.kind.is_verification());
        match (tokens, verification) {
            (Some(t), true) => s.spent_review += t,
            (Some(t), false) => s.spent_work += t,
            (None, _) => s.unreported_calls += 1,
        }
    }

    pub fn spent_tokens(&self) -> u64 {
        let s = self.lock();
        s.spent_work + s.spent_review
    }

    pub fn unreported_calls(&self) -> u64 {
        self.lock().unreported_calls
    }

    pub fn limit_tokens(&self) -> Option<u64> {
        self.lock().limit_tokens
    }

    /// The human's answer to "the budget is spent": allow `extra` more.
    pub fn raise_limit(&self, extra: u64) {
        let mut shared = self.lock();
        if let Some(limit) = shared.limit_tokens.as_mut() {
            *limit += extra;
        }
    }
}

fn owner_allows(shared: &Shared, node: &str, k: &str, path: &Path) -> Result<(), Conflict> {
    match shared.claims.get(k) {
        None => Ok(()),
        Some(owner) if owner == node => Ok(()),
        Some(owner)
            if shared
                .nodes
                .get(node)
                .is_some_and(|n| n.ancestors.contains(owner)) =>
        {
            Ok(())
        }
        Some(owner) => Err(Conflict {
            path: path.display().to_string(),
            owner: owner.clone(),
        }),
    }
}

/// One key per file. Windows paths are case-insensitive, so `Foo.rs`
/// and `foo.rs` are one claim there (row 4.14); the `\\?\` long-path
/// prefix and separator style never make two keys for one file.
fn key(path: &Path) -> String {
    let s = path.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    if cfg!(windows) {
        s.replace('/', "\\").to_lowercase()
    } else {
        s.to_string()
    }
}

fn ancestors(graph: &TaskGraph, id: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut frontier: Vec<&str> = vec![id];
    while let Some(cur) = frontier.pop() {
        if let Some(n) = graph.node(cur) {
            for d in &n.deps {
                if out.insert(d.clone()) {
                    frontier.push(d);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::NodeSpec;

    fn node(id: &str, deps: &[&str], kind: NodeKind) -> NodeSpec {
        NodeSpec {
            id: id.into(),
            agent: String::new(),
            request: "x".into(),
            deps: deps.iter().map(|d| d.to_string()).collect(),
            model: None,
            kind,
        }
    }

    /// plan -> {schema, api} -> review
    fn ctx(limit: Option<u64>, reserve: u64) -> TeamContext {
        let graph = TaskGraph::new(vec![
            node("plan", &[], NodeKind::Work),
            node("schema", &["plan"], NodeKind::Work),
            node("api", &["plan"], NodeKind::Work),
            node("review", &["schema", "api"], NodeKind::Review),
        ])
        .unwrap();
        TeamContext::new(&graph, limit, reserve, |_| 0)
    }

    #[test]
    fn a_sibling_cannot_overwrite_a_file_another_task_wrote() {
        let c = ctx(None, 0);
        let f = Path::new("C:/ws/src/orders.rs");
        c.claim("schema", f).unwrap();
        let err = c.claim("api", f).unwrap_err();
        assert_eq!(err.owner, "schema");
        assert!(c.check("api", f).is_err(), "check agrees with claim");
        // The owner itself may keep editing it.
        c.claim("schema", f).unwrap();
    }

    #[test]
    fn a_dependent_takes_over_its_upstreams_file_and_siblings_still_cannot() {
        let c = ctx(None, 0);
        let f = Path::new("/ws/plan.md");
        c.claim("plan", f).unwrap();
        c.claim("schema", f).unwrap();
        assert_eq!(
            c.claim("api", f).unwrap_err().owner,
            "schema",
            "after schema took it over, its sibling may not"
        );
        c.claim("review", f).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn paths_differing_only_in_case_or_prefix_are_one_claim_on_windows() {
        let c = ctx(None, 0);
        c.claim("schema", Path::new(r"\\?\C:\WS\Src\Orders.rs"))
            .unwrap();
        assert!(c.claim("api", Path::new("c:/ws/src/orders.RS")).is_err());
    }

    #[test]
    fn ordinary_work_stops_at_the_reserve_and_review_may_spend_it() {
        let c = ctx(Some(100), 30);
        c.admit("plan").unwrap();
        c.charge("plan", Some(70));
        assert!(c.admit("schema").is_err(), "70 of 100 with 30 reserved");
        c.admit("review").unwrap();
        c.charge("review", Some(30));
        assert!(c.admit("review").is_err(), "the reserve is spent too");
        c.raise_limit(50);
        c.admit("schema").unwrap();
    }

    /// The case the smoke run exposed: work overshooting the whole limit
    /// in one call still leaves review its full reserve.
    #[test]
    fn work_overshooting_the_limit_never_starves_review() {
        let c = ctx(Some(120), 20);
        c.charge("plan", Some(150));
        assert!(c.admit("schema").is_err());
        c.admit("review").unwrap();
        c.charge("review", Some(15));
        c.admit("review").unwrap();
        c.charge("review", Some(10));
        assert!(c.admit("review").is_err(), "25 of a 20 reserve, limit gone");
    }

    #[test]
    fn review_may_also_use_what_work_left_unspent() {
        let c = ctx(Some(100), 10);
        c.charge("plan", Some(20));
        c.charge("review", Some(50));
        c.admit("review").unwrap();
        // Review took 40 beyond its reserve from the shared pool.
        c.admit("schema").unwrap();
        c.charge("schema", Some(30));
        assert!(c.admit("schema").is_err(), "20 + 30 + 40 = 90 of 90");
    }

    #[test]
    fn an_unreported_call_is_counted_as_unseen_never_as_zero_tokens() {
        let c = ctx(Some(100), 0);
        c.charge("plan", None);
        assert_eq!((c.spent_tokens(), c.unreported_calls()), (0, 1));
    }
}
