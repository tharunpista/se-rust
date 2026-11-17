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

/// Resolves a model-supplied path against `root` and refuses anything
/// that lands outside it (row 2.10). The nearest existing ancestor is
/// canonicalized, which resolves `..`, symlinks, junctions, case and
/// 8.3 short names the way the OS will; the not-yet-existing remainder
/// (a new file or directory) is checked lexically and may not contain
/// `..` or a root/prefix. Returns `root.join(raw)` so relative paths
/// mean the workspace, not wherever the process happens to run.
pub fn confine(root: &Path, raw: &str) -> Result<PathBuf, String> {
    use std::path::Component;
    let outside = || format!("`{raw}` is outside the workspace");
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("workspace root unavailable: {e}"))?;
    let joined = root.join(raw);
    let mut existing = joined.as_path();
    let mut tail = Vec::new();
    let canonical = loop {
        match existing.canonicalize() {
            Ok(c) => break c,
            Err(_) => {
                tail.push(existing.file_name().ok_or_else(outside)?);
                existing = existing.parent().ok_or_else(outside)?;
            }
        }
    };
    if !canonical.starts_with(&canonical_root) {
        return Err(outside());
    }
    // SE's own state (session logs, the audit trail) is not the
    // workspace: a model must not be able to rewrite its own record.
    let state_dir = canonical_root.join(".se");
    let lands_in_state = canonical.starts_with(&state_dir)
        || (canonical == canonical_root
            && tail
                .last()
                .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(".se")));
    if lands_in_state {
        return Err(format!("`{raw}` is SE's own state directory"));
    }
    // `file_name()` never yields `..` or a prefix, but a tail segment
    // could still be one if `existing` stopped at such a component.
    if tail
        .iter()
        .any(|s| matches!(Path::new(s).components().next(), Some(c) if !matches!(c, Component::Normal(_))))
    {
        return Err(outside());
    }
    Ok(joined)
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
pub fn parse_tool_call(root: &Path, name: &str, arguments: &str) -> Result<ParsedCall, String> {
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
            let path = confine(root, &get_str("path")?)?;
            Ok(ParsedCall {
                resource_scope: path.to_string_lossy().into_owned(),
                cwd: parent_or_dot(&path).to_string_lossy().into_owned(),
                display: format!("read {}", path.display()),
                identity_path: path.clone(),
                action: ToolAction::Read { path },
            })
        }
        "write" => {
            let path = confine(root, &get_str("path")?)?;
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
            let path = confine(root, &get_str("path")?)?;
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
            let search_root = confine(root, &get_str("root")?)?;
            let needle = get_str("needle")?;
            Ok(ParsedCall {
                resource_scope: search_root.to_string_lossy().into_owned(),
                cwd: search_root.to_string_lossy().into_owned(),
                display: format!("search \"{needle}\" under {}", search_root.display()),
                identity_path: search_root.clone(),
                action: ToolAction::Search {
                    root: search_root,
                    needle,
                },
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
            let cwd = root.to_path_buf();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("se-confine-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        root
    }

    #[test]
    fn paths_inside_the_workspace_are_accepted() {
        let root = workspace("inside");
        assert!(confine(&root, "src/main.rs").is_ok());
        assert!(confine(&root, "src/new_file.rs").is_ok(), "a new file");
        assert!(
            confine(&root, "new_dir/deeper/file.rs").is_ok(),
            "a new tree"
        );
        assert!(
            confine(&root, "src/../src/main.rs").is_ok(),
            ".. that stays inside"
        );
        let absolute = root.join("src/main.rs");
        assert!(confine(&root, &absolute.to_string_lossy()).is_ok());
    }

    #[test]
    fn paths_escaping_the_workspace_are_refused() {
        let root = workspace("escape");
        for raw in [
            "../outside.txt",
            "src/../../outside.txt",
            "missing/../../outside.txt",
            "missing/../../../etc/passwd",
        ] {
            assert!(confine(&root, raw).is_err(), "{raw} must be refused");
        }
        let elsewhere = std::env::temp_dir().join("elsewhere.txt");
        assert!(confine(&root, &elsewhere.to_string_lossy()).is_err());
        // A sibling whose name merely starts with the root's name.
        let sibling = format!("{}-evil/x", root.display());
        assert!(confine(&root, &sibling).is_err());
    }

    #[test]
    fn a_link_inside_the_workspace_pointing_outside_is_refused() {
        let root = workspace("link");
        let outside =
            std::env::temp_dir().join(format!("se-confine-target-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        // A junction, not a symlink: it needs no privilege, so it is the
        // escape an unprivileged Windows user actually has.
        #[cfg(windows)]
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(root.join("escape"))
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&outside, root.join("escape")).is_ok();
        if !made {
            eprintln!("skipped: creating a symlink needs privilege on this machine");
            return;
        }
        assert!(confine(&root, "escape/secret.txt").is_err());
    }

    #[test]
    fn every_path_tool_is_confined_and_shell_runs_in_the_workspace() {
        let root = workspace("tools");
        for (name, args) in [
            ("read", r#"{"path":"../x"}"#),
            ("write", r#"{"path":"../x","content":"c"}"#),
            (
                "edit",
                r#"{"path":"../x","old_string":"a","new_string":"b"}"#,
            ),
            ("search", r#"{"root":"..","needle":"n"}"#),
        ] {
            assert!(parse_tool_call(&root, name, args).is_err(), "{name}");
        }
        let shell = parse_tool_call(&root, "shell", r#"{"program":"cmd"}"#).unwrap();
        assert_eq!(shell.identity_path, root);
    }

    #[test]
    fn ses_own_state_directory_is_off_limits() {
        let root = workspace("state");
        for raw in [
            ".se/audit.jsonl",
            ".SE/sessions/x.jsonl",
            ".se",
            "src/../.se/a",
        ] {
            assert!(confine(&root, raw).is_err(), "{raw} must be refused");
        }
        std::fs::create_dir_all(root.join(".se")).unwrap();
        assert!(
            confine(&root, ".se/audit.jsonl").is_err(),
            "existing dir too"
        );
        assert!(confine(&root, ".sentinel").is_ok(), "only the exact dir");
    }
}
