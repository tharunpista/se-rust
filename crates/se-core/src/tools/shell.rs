use super::shell_env::build_child_env;
use super::{identity, Granted, ToolError};
use std::path::Path;
use std::process::Command;

pub struct ShellOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: i32,
}

/// Spawns `program` with `args`, running in `granted`'s cwd. This is
/// the E10 fix point: the child's environment is cleared and rebuilt
/// from nothing but [`build_child_env`]'s allowlist plus whatever the
/// gateway's token itself bound as `env_subset` — never this process's
/// inherited environment.
pub fn run_shell(
    granted: &Granted,
    program: &str,
    args: &[String],
) -> Result<ShellOutput, ToolError> {
    let cwd = granted.cwd();
    let actual = identity::resolve_resource_identity(Path::new(cwd))?;
    if &actual != granted.resource() {
        return Err(ToolError::ResourceMismatch);
    }

    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.current_dir(cwd);
    cmd.env_clear();
    for (k, v) in build_child_env(granted.env_subset()) {
        cmd.env(k, v);
    }

    let output = cmd.output()?;
    Ok(ShellOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        status: output.status.code().unwrap_or(-1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::granted_for;
    use se_permission::NonceLedger;
    use std::fs;

    fn cwd_for_test(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("se-core-shell-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn runs_a_command_in_the_approved_directory_and_captures_output() {
        let dir = cwd_for_test("basic");
        let ledger = NonceLedger::new();
        let granted = granted_for("shell", &dir, &dir, vec![], &ledger);

        let out = run_shell(
            &granted,
            "cmd",
            &["/C".into(), "echo hello-from-child".into()],
        )
        .unwrap();
        assert_eq!(out.status, 0);
        assert!(out.stdout.contains("hello-from-child"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// E10 regression test: a secret set in *this* (parent) process's
    /// environment must never reach the spawned child. This is the
    /// test that actually proves the fix, not just that `env_clear()`
    /// appears somewhere in the source.
    #[test]
    fn a_parent_only_secret_does_not_reach_the_spawned_child() {
        // SAFETY (test-only): single-threaded access to this one var,
        // scoped to this test and cleaned up at the end.
        std::env::set_var("SE_TEST_SECRET", "should-not-leak");

        let dir = cwd_for_test("e10");
        let ledger = NonceLedger::new();
        let granted = granted_for("shell", &dir, &dir, vec![], &ledger);

        let out = run_shell(&granted, "cmd", &["/C".into(), "set".into()]).unwrap();

        std::env::remove_var("SE_TEST_SECRET");

        assert!(
            !out.stdout.contains("SE_TEST_SECRET"),
            "child dumped parent-only secret env var: {}",
            out.stdout
        );
        assert!(!out.stdout.contains("should-not-leak"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_explicitly_bound_env_var_does_reach_the_child() {
        let dir = cwd_for_test("bound-env");
        let ledger = NonceLedger::new();
        let granted = granted_for(
            "shell",
            &dir,
            &dir,
            vec![("SE_BOUND_VAR".into(), "visible-value".into())],
            &ledger,
        );

        let out = run_shell(&granted, "cmd", &["/C".into(), "set".into()]).unwrap();
        assert!(out.stdout.contains("SE_BOUND_VAR=visible-value"));

        let _ = fs::remove_dir_all(&dir);
    }
}
