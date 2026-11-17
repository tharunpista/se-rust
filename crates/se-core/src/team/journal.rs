//! A run's durable state (Phase 4 item 3, slice three; row 4.12).
//!
//! Append-only JSON lines: the graph once, then one line per node state
//! change. Separate from the session log on purpose: that log is keyed
//! by `TaskId`, a per-process counter, and pending or blocked nodes have
//! no task at all. This file holds only node states, keyed by node id,
//! so the two never record the same fact.

use std::io::Write;
use std::path::Path;

use std::collections::HashMap;

use super::driver::{Limits, NodeRecord};
use super::graph::{NodeSpec, NodeState, TaskGraph};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum Line {
    Graph {
        graph: Vec<NodeSpec>,
    },
    State {
        node: String,
        state: NodeState,
    },
    /// A completed node's answer, so a resumed run can still hand it to
    /// the nodes that depend on it (row 4.11).
    Output {
        node: String,
        output: String,
    },
    // `Line` is untagged: each later line has its own unique top-level
    // key, so it can never be mistaken for one of the lines above.
    Attribution {
        attribution: (String, NodeRecord),
    },
    Limits {
        limits: Limits,
    },
}

/// Everything a journal holds, read back.
pub struct Resumed {
    pub graph: TaskGraph,
    pub journal: RunJournal,
    pub outputs: HashMap<String, String>,
    /// `None` for a journal written before limits were recorded.
    pub limits: Option<Limits>,
    pub records: HashMap<String, NodeRecord>,
}

pub struct RunJournal {
    file: std::fs::File,
}

impl RunJournal {
    /// Starts a new journal. Refuses to overwrite an existing run.
    pub fn create(path: &Path, graph: &TaskGraph, limits: &Limits) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)?;
        let mut journal = Self { file };
        journal.write(&Line::Graph {
            graph: graph.nodes().to_vec(),
        })?;
        journal.write(&Line::Limits {
            limits: limits.clone(),
        })?;
        Ok(journal)
    }

    /// A journal whose every write fails, for testing that failure path.
    #[cfg(test)]
    pub(crate) fn broken(path: &Path) -> Self {
        std::fs::write(path, b"").unwrap();
        // Opened read-only, so `write_all` is refused by the OS.
        Self {
            file: std::fs::File::open(path).unwrap(),
        }
    }

    pub fn record(&mut self, node: &str, state: NodeState) -> std::io::Result<()> {
        self.write(&Line::State {
            node: node.to_string(),
            state,
        })
    }

    pub fn record_output(&mut self, node: &str, output: &str) -> std::io::Result<()> {
        self.write(&Line::Output {
            node: node.to_string(),
            output: output.to_string(),
        })
    }

    /// Who ran `node` and what it reported spending (rows 4.2/4.15).
    pub fn record_attribution(&mut self, node: &str, record: &NodeRecord) -> std::io::Result<()> {
        self.write(&Line::Attribution {
            attribution: (node.to_string(), record.clone()),
        })
    }

    fn write(&mut self, line: &Line) -> std::io::Result<()> {
        let mut text = serde_json::to_string(line)?;
        text.push('\n');
        self.file.write_all(text.as_bytes())?;
        // To the disk, not just the OS: the point of journaling `Running`
        // before the work starts is that it survives the crash.
        self.file.sync_data()
    }
}

/// Reads a journal back. A node last seen running or waiting comes back
/// as `NeedsConfirmation`: its side effects are unknown, so it is never
/// re-run on its own. A torn final line is skipped.
/// Also returns every recorded output, keyed by node id.
pub fn resume(path: &Path) -> std::io::Result<Resumed> {
    let invalid = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidData, m);
    let text = std::fs::read_to_string(path)?;
    let mut lines = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Line>(l).ok());
    let Some(Line::Graph { graph }) = lines.next() else {
        return Err(invalid("journal does not start with a graph".into()));
    };
    let mut graph = TaskGraph::new(graph).map_err(|e| invalid(e.to_string()))?;
    let mut last = HashMap::new();
    let mut outputs = HashMap::new();
    let mut records = HashMap::new();
    let mut limits = None;
    for line in lines {
        match line {
            Line::State { node, state } => {
                last.insert(node, state);
            }
            Line::Output { node, output } => {
                outputs.insert(node, output);
            }
            Line::Attribution {
                attribution: (node, record),
            } => {
                records.insert(node, record);
            }
            Line::Limits { limits: l } => limits = Some(l),
            Line::Graph { .. } => {}
        }
    }
    for (node, state) in last {
        let restored = match state {
            NodeState::Running | NodeState::WaitingApproval | NodeState::NeedsConfirmation => {
                NodeState::NeedsConfirmation
            }
            // Recomputed from dependencies below, never trusted as read.
            NodeState::Pending | NodeState::Ready | NodeState::Blocked => NodeState::Pending,
            done => done,
        };
        if !graph.restore(&node, restored) {
            return Err(invalid(format!("journal names unknown task `{node}`")));
        }
    }
    graph.resettle();
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    // Close a torn final line, or the next record would be glued onto it
    // and lost on the following resume.
    if !text.is_empty() && !text.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    Ok(Resumed {
        graph,
        journal: RunJournal { file },
        outputs,
        limits,
        records,
    })
}
