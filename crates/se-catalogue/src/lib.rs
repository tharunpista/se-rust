//! The SE specialist catalogue: a read-only loader over
//! `SE\se\agents\<department>\<id>.md` and over skill roots
//! (`SE\se\skills`, `SE\.agents\skills`, `~\.claude\skills`).
//!
//! Phase 3 item 1 (agents): the loader over the 194 canonical
//! definitions, with the 2 non-agent files in that tree
//! (`ownership-matrix.md`, `role-inventory.md`) reported as
//! skipped-with-reason rather than silently dropped or treated as
//! agents.
//!
//! Phase 3 item 2 (skills): [`load_skills`] scans one or more skill
//! roots for `<root>/<skill-name>/SKILL.md`. Bodies are lazy --
//! [`SkillDefinition`] carries no body field, only [`load_skill_body`]
//! reads one, on demand, for a caller that actually wants a specific
//! skill's instructions.
//!
//! TUI wiring (item 3) is separate, later work, not built here.
//!
//! No directory watcher: `docs/architecture/SE_TARGET_ARCHITECTURE.md`
//! is explicit that an explicit refresh suffices until something needs
//! more. Scanning is read-only in the stronger sense too -- nothing in
//! this crate ever opens a path it scanned for writing.

pub mod agent;
pub mod loader;
pub mod skill;

pub use agent::{AgentDefinition, AgentFrontmatter, FailureMode, MemoryScopes};
pub use loader::{load, LoadReport, Skipped};
pub use skill::{load_skill_body, load_skills, SkillDefinition, SkillFrontmatter, SkillLoadReport};
