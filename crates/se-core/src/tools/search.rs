use super::{require_fresh_identity, Granted, ToolError};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub path: PathBuf,
    pub line: usize,
    pub text: String,
}

/// Plain substring search (no regex engine — the workspace does not
/// pin one, and a hand-rolled substring scan is all this task needs)
/// over every file under `root`, recursively. Refuses if `root`'s
/// identity does not match `granted`'s.
pub fn search_text(
    granted: &Granted,
    root: &Path,
    needle: &str,
) -> Result<Vec<SearchMatch>, ToolError> {
    require_fresh_identity(granted, root)?;
    let mut matches = Vec::new();
    walk(root, needle, &mut matches)?;
    Ok(matches)
}

fn walk(dir: &Path, needle: &str, out: &mut Vec<SearchMatch>) -> Result<(), ToolError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk(&path, needle, out)?;
        } else if let Ok(content) = std::fs::read_to_string(&path) {
            for (i, line) in content.lines().enumerate() {
                if line.contains(needle) {
                    out.push(SearchMatch {
                        path: path.clone(),
                        line: i + 1,
                        text: line.to_string(),
                    });
                }
            }
        }
        // A file that isn't valid UTF-8 (e.g. binary) is silently
        // skipped rather than erroring the whole search out.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::granted_for;
    use se_permission::NonceLedger;
    use std::fs;

    #[test]
    fn finds_matching_lines_across_nested_files() {
        let dir = std::env::temp_dir().join(format!("se-core-search-{}", std::process::id()));
        let nested = dir.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(dir.join("a.txt"), "needle here\nno match").unwrap();
        fs::write(nested.join("b.txt"), "another needle line").unwrap();
        fs::write(dir.join("c.txt"), "nothing to see").unwrap();

        let ledger = NonceLedger::new();
        let granted = granted_for("search", &dir, &dir, vec![], &ledger);
        let mut results = search_text(&granted, &dir, "needle").unwrap();
        results.sort_by(|a, b| a.path.cmp(&b.path));

        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|m| m.text == "needle here"));
        assert!(results.iter().any(|m| m.text == "another needle line"));

        let _ = fs::remove_dir_all(&dir);
    }
}
