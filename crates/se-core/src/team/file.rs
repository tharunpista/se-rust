//! The team file: a YAML description of a multi-agent, multi-vendor run
//! that a user writes and edits by hand.
//!
//! ```yaml
//! max_concurrent: 4          # optional, default 4
//! limits:                    # optional per-vendor caps
//!   anthropic: 2
//! tasks:
//!   - id: plan
//!     agent: solution-architect        # optional: picked automatically
//!     model: anthropic/claude-sonnet-4-5  # optional: the default model
//!     request: Design the change.
//!   - id: code
//!     agent: backend-lead
//!     model: openai/gpt-5
//!     deps: [plan]
//!     request: Implement the design.
//!   - id: compare                     # an ensemble: the same packet to
//!     ensemble:                       # each model, then an evaluator
//!       - anthropic/claude-sonnet-4-5 # that surfaces every disagreement
//!       - openai/gpt-5
//!     deps: [plan]
//!     request: Which database fits?
//!   - id: review
//!     review: true                    # may spend the reserved budget
//!     deps: [code, compare]
//!     request: Review the change.
//! budget_tokens: 400000              # optional run budget
//! review_reserve_tokens: 50000       # only review/evaluate nodes may use it
//! approvals_per_minute: 10           # per task; excess refused unasked
//! ```
//!
//! An ensemble task `x` becomes members `x#1`, `x#2`, ... (one per model,
//! same agent, same request, same dependencies) and an evaluate node `x`
//! that depends on them, so anything depending on `x` receives the
//! comparison report.
//!
//! Naming a vendor in the file is the operator's consent for that run to
//! use that vendor's stored credential, so every named provider gets an
//! explicit `use_provider` allow rule. Nothing else is pre-approved:
//! tool calls still go through the gateway as usual.

use std::collections::HashMap;

use se_permission::Rule;
use se_protocol::ModelRef;

use super::driver::Limits;
use super::graph::{NodeKind, NodeSpec, TaskGraph};

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileTask {
    id: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    request: String,
    #[serde(default)]
    deps: Vec<String>,
    #[serde(default)]
    ensemble: Vec<String>,
    #[serde(default)]
    review: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    max_concurrent: Option<usize>,
    #[serde(default)]
    limits: HashMap<String, usize>,
    #[serde(default)]
    budget_tokens: Option<u64>,
    #[serde(default)]
    review_reserve_tokens: u64,
    #[serde(default)]
    approvals_per_minute: Option<usize>,
    tasks: Vec<FileTask>,
}

/// A parsed, validated team file.
#[derive(Debug)]
pub struct TeamFile {
    pub graph: TaskGraph,
    pub limits: Limits,
    /// One `use_provider` allow rule per vendor the file names.
    pub provider_rules: Vec<Rule>,
}

pub const DEFAULT_CONCURRENCY: usize = 4;
const DEFAULT_APPROVALS_PER_MINUTE: usize = 10;

/// Parses and validates `text`. Every problem is reported before
/// anything runs: an unknown key, a bad model reference, a zero limit,
/// or an invalid graph (cycle, duplicate id, unknown dependency).
pub fn parse(text: &str) -> Result<TeamFile, String> {
    let file: File = serde_yaml_ng::from_str(text).map_err(|e| format!("team file: {e}"))?;
    let max_concurrent = file.max_concurrent.unwrap_or(DEFAULT_CONCURRENCY);
    if max_concurrent == 0 {
        return Err("team file: max_concurrent must be at least 1".into());
    }
    if let Some((vendor, _)) = file.limits.iter().find(|(_, n)| **n == 0) {
        return Err(format!(
            "team file: limit for `{vendor}` must be at least 1"
        ));
    }
    let mut nodes = Vec::new();
    for t in file.tasks {
        if t.request.trim().is_empty() {
            return Err(format!("team file: task `{}` has an empty request", t.id));
        }
        let model = t
            .model
            .as_deref()
            .map(|m| model_ref(&t.id, m))
            .transpose()?;
        // Empty means "pick a specialist automatically".
        let agent = t.agent.unwrap_or_default();
        if t.ensemble.is_empty() {
            nodes.push(NodeSpec {
                id: t.id,
                agent,
                request: t.request,
                deps: t.deps,
                model,
                kind: if t.review {
                    NodeKind::Review
                } else {
                    NodeKind::Work
                },
            });
            continue;
        }
        if t.ensemble.len() < 2 || model.is_some() || t.review {
            return Err(format!(
                "team file: task `{}`: an ensemble needs at least two models, \
                 and takes no `model` or `review` of its own",
                t.id
            ));
        }
        let mut members = Vec::new();
        for (i, m) in t.ensemble.iter().enumerate() {
            let id = format!("{}#{}", t.id, i + 1);
            nodes.push(NodeSpec {
                id: id.clone(),
                agent: agent.clone(),
                request: t.request.clone(),
                deps: t.deps.clone(),
                model: Some(model_ref(&t.id, m)?),
                kind: NodeKind::Work,
            });
            members.push(id);
        }
        nodes.push(NodeSpec {
            id: t.id,
            agent: String::new(),
            request: format!(
                "Compare these answers to the same task. The task was:\n\n{}",
                t.request
            ),
            deps: members,
            model: None,
            kind: NodeKind::Evaluate,
        });
    }
    let graph = TaskGraph::new(nodes).map_err(|e| format!("team file: {e}"))?;
    Ok(TeamFile {
        provider_rules: provider_rules(&graph),
        graph,
        limits: Limits {
            max_concurrent,
            per_provider: file.limits,
            budget_tokens: file.budget_tokens,
            review_reserve_tokens: file.review_reserve_tokens,
            approvals_per_minute: Some(
                file.approvals_per_minute
                    .unwrap_or(DEFAULT_APPROVALS_PER_MINUTE),
            ),
        },
    })
}

fn model_ref(task: &str, m: &str) -> Result<ModelRef, String> {
    let Some((provider, model)) = m
        .split_once('/')
        .filter(|(p, m)| !p.is_empty() && !m.is_empty())
    else {
        return Err(format!(
            "team file: task `{task}` model `{m}` must be `<provider>/<model>`"
        ));
    };
    Ok(ModelRef {
        provider: provider.to_string(),
        model: model.to_string(),
    })
}

/// One `use_provider` allow rule per distinct vendor the graph's nodes
/// name. Derived from the graph, not the file, so a resumed run (which
/// has only its journal) gets exactly the consent the original file gave.
pub fn provider_rules(graph: &TaskGraph) -> Vec<Rule> {
    let mut seen: Vec<&str> = Vec::new();
    for n in graph.nodes() {
        if let Some(m) = &n.model {
            if !seen.contains(&m.provider.as_str()) {
                seen.push(&m.provider);
            }
        }
    }
    seen.into_iter()
        .map(|p| Rule::allow_tool_on("use_provider", p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = "
max_concurrent: 3
limits:
  anthropic: 1
tasks:
  - id: plan
    agent: solution-architect
    model: anthropic/claude-sonnet-4-5
    request: Design it.
  - id: code
    model: openrouter/meta-llama/llama-3-70b
    deps: [plan]
    request: Build it.
";

    #[test]
    fn a_valid_file_becomes_a_graph_limits_and_provider_consent() {
        let team = parse(EXAMPLE).unwrap();
        assert_eq!(team.limits.max_concurrent, 3);
        assert_eq!(team.limits.per_provider["anthropic"], 1);
        let code = team.graph.node("code").unwrap();
        assert_eq!(code.agent, "", "no agent means automatic");
        let m = code.model.as_ref().unwrap();
        assert_eq!(
            (m.provider.as_str(), m.model.as_str()),
            ("openrouter", "meta-llama/llama-3-70b")
        );
        assert_eq!(team.provider_rules.len(), 2);
        assert_eq!(team.graph.ready(), ["plan"]);
    }

    #[test]
    fn mistakes_are_refused_before_anything_runs() {
        for (bad, why) in [
            ("tasks: []", "at least one task"),
            (
                "tasks:\n  - id: a\n    request: x\n    model: gpt-5\n",
                "<provider>/<model>",
            ),
            (
                "tasks:\n  - id: a\n    request: x\n    deps: [b]\n",
                "unknown task",
            ),
            ("tasks:\n  - id: a\n    request: '  '\n", "empty request"),
            (
                "max_concurrent: 0\ntasks:\n  - id: a\n    request: x\n",
                "at least 1",
            ),
            (
                "tasks:\n  - id: a\n    request: x\n    agnet: typo\n",
                "unknown field",
            ),
            (
                "limits: {groq: 0}\ntasks:\n  - id: a\n    request: x\n",
                "at least 1",
            ),
            (
                "tasks:\n  - id: a\n    request: x\n    ensemble: [p/m]\n",
                "at least two models",
            ),
            (
                "tasks:\n  - id: a\n    request: x\n    ensemble: [p/m, q/n]\n    model: p/m\n",
                "no `model`",
            ),
        ] {
            let err = parse(bad).unwrap_err();
            assert!(err.contains(why), "{bad:?}: {err}");
        }
    }

    #[test]
    fn an_ensemble_becomes_members_and_an_evaluator_with_identical_requests() {
        let team = parse(
            "tasks:
  - id: plan
    request: Plan.
  - id: db
    agent: data-architect
    deps: [plan]
    ensemble: [anthropic/claude-x, openai/gpt-x, groq/llama-x]
    request: Which database?
  - id: review
    review: true
    deps: [db]
    request: Check it.
",
        )
        .unwrap();
        let g = &team.graph;
        for id in ["db#1", "db#2", "db#3"] {
            let m = g.node(id).unwrap();
            assert_eq!(
                (m.agent.as_str(), m.request.as_str(), m.deps.as_slice()),
                (
                    "data-architect",
                    "Which database?",
                    ["plan".to_string()].as_slice()
                )
            );
        }
        let eval = g.node("db").unwrap();
        assert_eq!(eval.kind, NodeKind::Evaluate);
        assert_eq!(eval.deps, ["db#1", "db#2", "db#3"]);
        assert_eq!(g.node("review").unwrap().kind, NodeKind::Review);
        assert_eq!(team.provider_rules.len(), 3, "consent for each vendor");
    }
}
