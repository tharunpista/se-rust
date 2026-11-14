//! E10: the child environment allowlist, in one named place so it is
//! auditable without grepping for every `env_clear()` call site.
//!
//! Nothing provider-API-key-shaped or credential-shaped belongs here.
//! These four exist only so a spawned shell can resolve a binary on
//! `PATH` and write to a temp directory, which is what a shell tool
//! that cannot do either is actually broken, not actually safer.

pub const ENV_ALLOWLIST: &[&str] = &["PATH", "TEMP", "TMP", "SystemRoot", "windir"];

/// The environment a spawned child actually gets: the allowlisted
/// subset of *this* process's environment, plus whatever the gateway's
/// `Approved` token itself bound (`env_subset`) — never the parent
/// process's full environment, which is the E10 fix.
pub fn build_child_env(granted_env_subset: &[(String, String)]) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = Vec::new();
    for key in ENV_ALLOWLIST {
        if let Ok(v) = std::env::var(key) {
            vars.push((key.to_string(), v));
        }
    }
    for (k, v) in granted_env_subset {
        if !vars.iter().any(|(ek, _)| ek == k) {
            vars.push((k.clone(), v.clone()));
        }
    }
    vars
}
