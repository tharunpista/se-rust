//! An SE specialist agent definition: the parsed frontmatter plus the
//! original Markdown body verbatim, never reinterpreted.

use serde::Deserialize;

/// One `{mode, guard}` pair from a definition's `failure_modes` list.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FailureMode {
    pub mode: String,
    pub guard: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct MemoryScopes {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
}

/// The frontmatter fields this workspace actually reads. Every field
/// but the four required ones is `#[serde(default)]`: three of the
/// 194 real files omit `verify_commands`, and nothing here should
/// refuse to load a definition for omitting an optional field that
/// nothing downstream currently needs.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentFrontmatter {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub department: String,
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub mission: String,
    #[serde(default)]
    pub activation: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub memory_scopes: MemoryScopes,
    #[serde(default)]
    pub owns_artifacts: Vec<String>,
    #[serde(default)]
    pub contributes_artifacts: Vec<String>,
    #[serde(default)]
    pub owns_interfaces: Vec<String>,
    #[serde(default)]
    pub collaborators: Vec<String>,
    #[serde(default)]
    pub escalates_to: Option<String>,
    #[serde(default)]
    pub context_budget_tokens: Option<u32>,
    #[serde(default)]
    pub max_calls: Option<u32>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub stack_aware: bool,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub boundary: String,
    #[serde(default)]
    pub verify_commands: Vec<String>,
    #[serde(default)]
    pub escalates_on_failure_to: Option<String>,
    #[serde(default)]
    pub requires_evidence: bool,
    #[serde(default)]
    pub writes_files: bool,
    #[serde(default)]
    pub failure_modes: Vec<FailureMode>,
}

/// A loaded specialist: its frontmatter, the body exactly as written
/// (never reformatted or summarized), where it came from, and a
/// content hash of the whole file so a caller can detect the
/// definition changing between two reads without re-reading it.
#[derive(Debug, Clone)]
pub struct AgentDefinition {
    pub frontmatter: AgentFrontmatter,
    pub body: String,
    pub source_path: std::path::PathBuf,
    pub content_hash: u64,
}
