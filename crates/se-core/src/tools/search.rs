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

/// A symlink, or on Windows any reparse point (junctions included),
/// judged without following it.
fn is_link(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return true; // unreadable: treat as unsafe to follow
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

fn walk(dir: &Path, needle: &str, out: &mut Vec<SearchMatch>) -> Result<(), ToolError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // Never follow a link while walking: a symlink or junction inside
        // the root can point outside it, or at an ancestor and loop.
        if is_link(&path) {
            continue;
        }
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

    /// A junction (or symlink) inside the searched root is not followed,
    /// so search cannot read outside the workspace or loop on a cycle.
    #[test]
    fn search_does_not_follow_a_link_out_of_the_root() {
        let base = std::env::temp_dir().join(format!("se-core-search-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (root, outside) = (base.join("root"), base.join("outside"));
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "needle outside").unwrap();
        fs::write(root.join("in.txt"), "needle inside").unwrap();
        #[cfg(windows)]
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(root.join("link"))
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&outside, root.join("link")).is_ok();
        assert!(made, "could not create the link this test needs");

        let ledger = NonceLedger::new();
        let granted = granted_for("search", &root, &root, vec![], &ledger);
        let found = search_text(&granted, &root, "needle").unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].path.ends_with("in.txt"));
        let _ = fs::remove_dir_all(&base);
    }
}
