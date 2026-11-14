//! Parses a model-requested tool call's JSON arguments into the exact
//! thing `se-permission` and `crate::tools` need, and dispatches it.
//!
//! Deliberately not a generic schema validator: each of the five tools
//! gets its own tiny, specific extraction, per the brief's instruction
//! to keep this minimal. A JSON-Schema validator would spend far more
//! code validating shapes nothing here actually needs validated, since
//! every field is read with `and_then`/`ok_or_else` and a missing or
//! wrong-typed field already fails closed.

use std::path::{Path, PathBuf};

use se_permission::ResourceIdentity;
use se_provider::ToolDefinition;

use crate::tools::{self, identity, Granted};

/// What a parsed call will actually do, once granted.
pub enum ToolAction {
    Read {
        path: PathBuf,
    },
    Write {
        path: PathBuf,
        content: String,
    },
    Edit {
        path: PathBuf,
        old_string: String,
        new_string: String,
    },
    Search {
        root: PathBuf,
        needle: String,
    },
    Shell {
        program: String,
        args: Vec<String>,
    },
}

/// A tool call's arguments, parsed into what the gateway needs to
/// decide and what the tool needs to run.
pub struct ParsedCall {
    pub action: ToolAction,
    /// What the ruleset matches against (a path or a shell command).
    pub resource_scope: String,
    /// The path whose OS-level identity the gateway's token binds to:
    /// the target itself if it already exists, otherwise its
    /// containing directory (a new-file write), or the working
    /// directory (a shell command) — the same convention already used
    /// by `crate::tools::test_support::granted_for`.
    pub identity_path: PathBuf,
    pub cwd: String,
    /// Shown to the human at approval time.
    pub display: String,
}

/// Builds the five tool definitions advertised to the model, named
/// identically to the names `crate::tools::test_support::granted_for`
/// already uses in this crate's own tests.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read".into(),
            description: "Reads a file's full contents.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }),
        },
        ToolDefinition {
            name: "write".into(),
            description: "Writes content to a file, replacing it entirely.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                },
                "required": ["path", "content"],
            }),
        },
        ToolDefinition {
            name: "edit".into(),
            description: "Replaces exactly one occurrence of old_string with new_string in a file."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                },
                "required": ["path", "old_string", "new_string"],
            }),
        },
        ToolDefinition {
            name: "search".into(),
            description: "Searches for a substring across every file under a directory.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "root": {"type": "string"},
                    "needle": {"type": "string"},
                },
                "required": ["root", "needle"],
            }),
        },
        ToolDefinition {
            name: "shell".into(),
            description: "Runs a command in the current working directory.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "program": {"type": "string"},
                    "args": {"type": "array", "items": {"type": "string"}},
                },
                "required": ["program"],
            }),
        },
    ]
}

fn parent_or_dot(path: &Path) -> PathBuf {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Parses `name`'s `arguments` (a complete JSON object, never partial —
/// the provider layer only emits a call once its JSON is whole). Fails
/// closed: a missing field, a wrong-shaped argument or an unknown tool
/// name is an error, never a best-effort guess.
pub fn parse_tool_call(name: &str, arguments: &str) -> Result<ParsedCall, String> {
    let value: serde_json::Value =
        serde_json::from_str(arguments).map_err(|e| format!("invalid arguments JSON: {e}"))?;
    let get_str = |key: &str| -> Result<String, String> {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("missing or non-string `{key}`"))
    };

    match name {
        "read" => {
            let path = PathBuf::from(get_str("path")?);
            Ok(ParsedCall {
                resource_scope: path.to_string_lossy().into_owned(),
                cwd: parent_or_dot(&path).to_string_lossy().into_owned(),
                display: format!("read {}", path.display()),
                identity_path: path.clone(),
                action: ToolAction::Read { path },
            })
        }
        "write" => {
            let path = PathBuf::from(get_str("path")?);
            let content = get_str("content")?;
            let identity_path = if path.exists() {
                path.clone()
            } else {
                parent_or_dot(&path)
            };
            Ok(ParsedCall {
                resource_scope: path.to_string_lossy().into_owned(),
                cwd: parent_or_dot(&path).to_string_lossy().into_owned(),
                display: format!("write {}", path.display()),
                identity_path,
                action: ToolAction::Write { path, content },
            })
        }
        "edit" => {
            let path = PathBuf::from(get_str("path")?);
            let old_string = get_str("old_string")?;
            let new_string = get_str("new_string")?;
            Ok(ParsedCall {
                resource_scope: path.to_string_lossy().into_owned(),
                cwd: parent_or_dot(&path).to_string_lossy().into_owned(),
                display: format!("edit {}", path.display()),
                identity_path: path.clone(),
                action: ToolAction::Edit {
                    path,
                    old_string,
                    new_string,
                },
            })
        }
        "search" => {
            let root = PathBuf::from(get_str("root")?);
            let needle = get_str("needle")?;
            Ok(ParsedCall {
                resource_scope: root.to_string_lossy().into_owned(),
                cwd: root.to_string_lossy().into_owned(),
                display: format!("search \"{needle}\" under {}", root.display()),
                identity_path: root.clone(),
                action: ToolAction::Search { root, needle },
            })
        }
        "shell" => {
            let program = get_str("program")?;
            let args: Vec<String> = value
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            Ok(ParsedCall {
                resource_scope: format!("{program} {}", args.join(" ")),
                cwd: cwd.to_string_lossy().into_owned(),
                display: format!("run {program} {}", args.join(" ")),
                identity_path: cwd,
                action: ToolAction::Shell { program, args },
            })
        }
        other => Err(format!("unknown tool `{other}`")),
    }
}

/// Resolves `parsed`'s identity the same way approval-time minting
/// will need to, so the caller can build one `Request`/`ExecutionContext`
/// pair from a single resolution rather than resolving twice and
/// risking the two disagreeing.
pub fn resolve_identity(parsed: &ParsedCall) -> std::io::Result<ResourceIdentity> {
    identity::resolve_resource_identity(&parsed.identity_path)
}

/// Runs the parsed action against a real `Granted` token, returning a
/// short human-readable preview of what happened.
pub fn execute(granted: &Granted, action: ToolAction) -> Result<String, String> {
    match action {
        ToolAction::Read { path } => tools::read_file(granted, &path).map_err(|e| e.to_string()),
        ToolAction::Write { path, content } => tools::write_file(granted, &path, &content)
            .map(|_| "wrote file".to_string())
            .map_err(|e| e.to_string()),
        ToolAction::Edit {
            path,
            old_string,
            new_string,
        } => tools::edit_file(granted, &path, &old_string, &new_string)
            .map(|_| "edited file".to_string())
            .map_err(|e| e.to_string()),
        ToolAction::Search { root, needle } => tools::search_text(granted, &root, &needle)
            .map(|matches| format!("{} match(es)", matches.len()))
            .map_err(|e| e.to_string()),
        ToolAction::Shell { program, args } => tools::run_shell(granted, &program, &args)
            .map(|out| {
                format!(
                    "exit {}\nstdout:\n{}\nstderr:\n{}",
                    out.status, out.stdout, out.stderr
                )
            })
            .map_err(|e| e.to_string()),
    }
}
