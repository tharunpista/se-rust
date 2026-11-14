//! The SE specialist catalogue: a read-only loader over
//! `SE\se\agents\<department>\<id>.md`.
//!
//! One crate, scoped to Phase 3 item 1: the agent loader over the 194
//! canonical definitions, with the 2 non-agent files in that tree
//! (`ownership-matrix.md`, `role-inventory.md`) reported as
//! skipped-with-reason rather than silently dropped or treated as
//! agents. Skills (item 2) and TUI wiring (item 3) are separate,
//! later work, not built here.
//!
//! No directory watcher: `docs/architecture/SE_TARGET_ARCHITECTURE.md`
//! is explicit that an explicit refresh suffices until something needs
//! more. Scanning is read-only in the stronger sense too -- nothing in
//! this crate ever opens a path it scanned for writing.

pub mod agent;
pub mod loader;

pub use agent::{AgentDefinition, AgentFrontmatter, FailureMode, MemoryScopes};
pub use loader::{load, LoadReport, Skipped};
