use super::{require_fresh_identity, Granted, ToolError};
use std::path::Path;

/// Errors specific to `edit_file`'s string-replace semantics.
#[derive(Debug)]
pub enum EditError {
    /// `old_string` was not found, or was found more than once (an
    /// edit must target an unambiguous location, same as the model-
    /// facing Edit tool convention this follows).
    NotUniqueMatch {
        occurrences: usize,
    },
    Tool(ToolError),
}

impl From<ToolError> for EditError {
    fn from(e: ToolError) -> Self {
        EditError::Tool(e)
    }
}

impl From<std::io::Error> for EditError {
    fn from(e: std::io::Error) -> Self {
        EditError::Tool(ToolError::Io(e))
    }
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::NotUniqueMatch { occurrences } => write!(
                f,
                "old_string matched {occurrences} time(s); it must match exactly once"
            ),
            EditError::Tool(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EditError {}

/// Replaces exactly one occurrence of `old_string` with `new_string` in
/// the file at `path`. Refuses if `old_string` is absent or ambiguous,
/// and refuses (via [`require_fresh_identity`]) if the file was swapped
/// since approval.
pub fn edit_file(
    granted: &Granted,
    path: &Path,
    old_string: &str,
    new_string: &str,
) -> Result<(), EditError> {
    require_fresh_identity(granted, path)?;
    let content = std::fs::read_to_string(path)?;
    let occurrences = content.matches(old_string).count();
    if occurrences != 1 {
        return Err(EditError::NotUniqueMatch { occurrences });
    }
    let updated = content.replacen(old_string, new_string, 1);
    std::fs::write(path, updated)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::granted_for;
    use se_permission::NonceLedger;
    use std::fs;

    #[test]
    fn replaces_the_single_matching_occurrence() {
        let dir = std::env::temp_dir().join(format!("se-core-edit-ok-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        fs::write(&file, "hello world").unwrap();

        let ledger = NonceLedger::new();
        let granted = granted_for("edit", &file, &dir, vec![], &ledger);
        edit_file(&granted, &file, "world", "rust").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello rust");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_an_ambiguous_match_and_leaves_the_file_untouched() {
        let dir = std::env::temp_dir().join(format!("se-core-edit-ambig-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        fs::write(&file, "foo foo").unwrap();

        let ledger = NonceLedger::new();
        let granted = granted_for("edit", &file, &dir, vec![], &ledger);
        let err = edit_file(&granted, &file, "foo", "bar").unwrap_err();
        assert!(matches!(err, EditError::NotUniqueMatch { occurrences: 2 }));
        assert_eq!(fs::read_to_string(&file).unwrap(), "foo foo");

        let _ = fs::remove_dir_all(&dir);
    }
}
