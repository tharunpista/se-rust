//! Team orchestration (Phase 4): a task graph run by specialists.

pub mod context;
pub mod driver;
pub mod evaluate;
pub mod file;
pub mod graph;
pub mod journal;
pub mod monitor;

pub use context::{Conflict, TeamContext};
pub use driver::{agent_state, Limits, NodeRecord, TeamRun};
pub use graph::{GraphError, NodeKind, NodeSpec, NodeState, TaskGraph, TransitionError};
