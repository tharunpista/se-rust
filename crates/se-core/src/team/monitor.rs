//! The team monitor (item 10, row 4.18): one row per node, derived only
//! from the scheduler's own state and the attribution record, so a
//! panel can never show something the scheduler does not believe.

use super::driver::NodeRecord;
use super::graph::{NodeKind, NodeState, TaskGraph};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub node: String,
    pub state: NodeState,
    /// The specialist that ran (or will run) it.
    pub agent: String,
    /// `provider/model`.
    pub model: String,
    /// Reported tokens, `not reported`, or `-` before it ran.
    pub tokens: String,
}

pub fn rows<'a>(graph: &TaskGraph, record: impl Fn(&str) -> Option<&'a NodeRecord>) -> Vec<Row> {
    graph
        .nodes()
        .iter()
        .map(|n| {
            let rec = record(&n.id);
            let agent = rec
                .and_then(|r| r.agent.clone())
                .or_else(|| Some(n.agent.clone()).filter(|a| !a.is_empty()))
                .unwrap_or_else(|| {
                    match n.kind {
                        NodeKind::Evaluate => "evaluator",
                        _ => "auto",
                    }
                    .to_string()
                });
            let model = match rec.and_then(|r| r.provider.clone().zip(r.model.clone())) {
                Some((p, m)) => format!("{p}/{m}"),
                None => n.model.as_ref().map_or("default".to_string(), |m| {
                    format!("{}/{}", m.provider, m.model)
                }),
            };
            let tokens = match rec {
                None => "-".to_string(),
                Some(r) => r
                    .tokens()
                    .map_or("not reported".to_string(), |t| t.to_string()),
            };
            Row {
                node: n.id.clone(),
                state: graph.state(&n.id).expect("own node"),
                agent,
                model,
                tokens,
            }
        })
        .collect()
}

/// Plain-text table, one line per row, for a terminal.
pub fn render(rows: &[Row]) -> Vec<String> {
    let w = |f: fn(&Row) -> usize, min: usize| rows.iter().map(f).max().unwrap_or(0).max(min);
    let (wn, ws, wa, wm) = (
        w(|r| r.node.len(), 4),
        w(|r| format!("{:?}", r.state).len(), 5),
        w(|r| r.agent.len(), 5),
        w(|r| r.model.len(), 5),
    );
    let mut out = vec![format!(
        "{:wn$}  {:ws$}  {:wa$}  {:wm$}  tokens",
        "task", "state", "agent", "model"
    )];
    for r in rows {
        out.push(format!(
            "{:wn$}  {:ws$}  {:wa$}  {:wm$}  {}",
            r.node,
            format!("{:?}", r.state),
            r.agent,
            r.model,
            r.tokens
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::NodeSpec;
    use se_protocol::{Usage, UsageSource};

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

    #[test]
    fn rows_come_from_scheduler_state_and_never_invent_usage() {
        let mut g = TaskGraph::new(vec![
            node("a", &[], NodeKind::Work),
            node("b", &["a"], NodeKind::Work),
            node("e", &["a"], NodeKind::Evaluate),
        ])
        .unwrap();
        g.start("a").unwrap();
        g.finish("a", false).unwrap();
        let reported = NodeRecord {
            agent: Some("backend-lead".into()),
            provider: Some("p1".into()),
            model: Some("m".into()),
            usage: Some(Usage {
                input_tokens: Some(7),
                output_tokens: Some(3),
                cached_input_tokens: None,
                source: UsageSource::ProviderReported,
            }),
        };
        let r = rows(&g, |id| (id == "a").then_some(&reported));
        assert_eq!(r[0].state, NodeState::Failed);
        assert_eq!(
            (
                r[0].agent.as_str(),
                r[0].model.as_str(),
                r[0].tokens.as_str()
            ),
            ("backend-lead", "p1/m", "10")
        );
        assert_eq!(
            r[1].state,
            NodeState::Blocked,
            "blocked never shows as running"
        );
        assert_eq!((r[1].agent.as_str(), r[1].tokens.as_str()), ("auto", "-"));
        assert_eq!(r[2].agent, "evaluator");

        let silent = NodeRecord::default();
        let r = rows(&g, |_| Some(&silent));
        assert_eq!(r[0].tokens, "not reported", "never zero");
        assert_eq!(render(&r).len(), 4, "header + one line per node");
    }
}
