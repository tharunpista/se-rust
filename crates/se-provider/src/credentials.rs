//! Credentials, in OS-native storage.
//!
//! Windows Credential Manager, macOS Keychain or the Linux Secret
//! Service, via `keyring`. SE invents no cryptography of its own.
//!
//! Invariant I8: a secret enters through one function and leaves through
//! exactly one outgoing HTTP header. Nothing here returns a secret
//! except [`load`], and no type carrying one implements `Debug` or
//! `Display`.
//!
//! Two behaviours below exist because of defects observed live in the
//! previous implementation, not because they seemed prudent:
//!
//! * [`store`] polls until the write is actually readable. Windows
//!   Credential Manager does not guarantee that a `get_password`
//!   immediately after a successful `set_password` sees the value. The
//!   connect flow stores and then validates, so an unconfirmed write
//!   made a perfectly good key look rejected, intermittently.
//! * Nothing caches a credential at construction time. The previous
//!   adapters read the key once into a struct field, so a key stored
//!   after start-up was invisible to the long-lived provider the
//!   interface actually used: validation succeeded through a freshly
//!   built provider, then every real request failed through the stale
//!   one. Readers here consult the store on every call.

const SERVICE: &str = "se";

/// Where a credential came from.
///
/// "Configured" is not "authenticated": only a real request to the
/// provider establishes that, which is why [`crate::ModelProvider::validate`]
/// exists separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Stored by the user, in OS-native storage.
    SecureStore,
    /// Supplied by an environment variable for this process only. Takes
    /// precedence, so a test or CI run never picks up a developer's
    /// stored key by accident.
    Environment,
    None,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::SecureStore => "secure store",
            Source::Environment => "environment variable",
            Source::None => "not configured",
        }
    }

    pub fn is_configured(self) -> bool {
        !matches!(self, Source::None)
    }
}

fn entry(id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, id).map_err(|e| e.to_string())
}

/// Reads a credential: environment first, then the secure store.
///
/// Consulted on every call by design -- see the module note.
pub fn load(id: &str, env_var: &str) -> Option<String> {
    if let Ok(v) = std::env::var(env_var) {
        let v = sanitise(&v);
        if !v.is_empty() {
            return Some(v);
        }
    }
    entry(id)
        .ok()?
        .get_password()
        .ok()
        .map(|v| sanitise(&v))
        .filter(|v| !v.is_empty())
}

/// Trims a credential and drops anything that cannot go in an HTTP
/// header.
///
/// A pasted key routinely carries a trailing newline or stray
/// whitespace. Left in place it produced an invalid-header error whose
/// text could contain the header *value* -- the key itself leaking into
/// a user-visible error, which breaks I8. Control characters go for the
/// same reason. Non-ASCII is kept here but rejected by `header_safe`,
/// which reports the problem without quoting the value.
fn sanitise(raw: &str) -> String {
    raw.trim().chars().filter(|c| !c.is_control()).collect()
}

/// Whether a credential can be placed in an HTTP header at all.
///
/// Checked before building the header so the failure is a clear message
/// rather than a transport error that might echo the value.
pub fn header_safe(secret: &str) -> bool {
    !secret.is_empty() && secret.is_ascii()
}

pub fn source(id: &str, env_var: &str) -> Source {
    if std::env::var(env_var)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return Source::Environment;
    }
    match entry(id).and_then(|e| e.get_password().map_err(|e| e.to_string())) {
        Ok(v) if !v.is_empty() => Source::SecureStore,
        _ => Source::None,
    }
}

/// How long to wait for a stored write to become readable.
const VISIBILITY_ATTEMPTS: usize = 20;
const VISIBILITY_PAUSE: std::time::Duration = std::time::Duration::from_millis(10);

/// Stores a credential, and does not return success until the value can
/// actually be read back.
///
/// See the module note: returning as soon as the OS call succeeds made a
/// subsequent validation fail intermittently on Windows.
pub fn store(id: &str, secret: &str) -> Result<(), String> {
    if secret.trim().is_empty() {
        return Err("refusing to store an empty credential".to_string());
    }
    let entry = entry(id)?;
    entry.set_password(secret).map_err(|e| e.to_string())?;

    for _ in 0..VISIBILITY_ATTEMPTS {
        if entry.get_password().ok().as_deref() == Some(secret) {
            return Ok(());
        }
        std::thread::sleep(VISIBILITY_PAUSE);
    }
    Err(format!(
        "the credential for `{id}` was written but secure storage has not made it readable yet — try again"
    ))
}

/// Removes a credential. Absent is success: the caller asked for "no
/// credential stored", which is the end state either way.
pub fn delete(id: &str) -> Result<(), String> {
    match entry(id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// A description safe to display: never any part of the secret.
///
/// Length alone lets a user tell "I pasted the wrong thing" from "it
/// saved", without exposing characters to a screenshot or a shoulder.
pub fn fingerprint(secret: &str) -> String {
    format!("{} characters", secret.chars().count())
}

/// Serialises tests that touch real OS credential storage.
///
/// The OS stores `keyring` wraps are not reliably safe under the test
/// harness's thread parallelism; this was observed as a flake, not
/// assumed.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-run id, so a test never touches a real stored credential.
    fn scratch_id(name: &str) -> String {
        format!(
            "se-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    fn a_credential_round_trips_and_deletes() {
        let _guard = test_lock();
        let id = scratch_id("roundtrip");
        let unset = "SE_TEST_VAR_UNSET_ON_PURPOSE";

        assert_eq!(source(&id, unset), Source::None);
        assert!(load(&id, unset).is_none());

        store(&id, "secret-value").unwrap();
        assert_eq!(source(&id, unset), Source::SecureStore);
        assert_eq!(load(&id, unset).as_deref(), Some("secret-value"));

        delete(&id).unwrap();
        assert_eq!(source(&id, unset), Source::None);
        // Deleting what is already gone is still success.
        delete(&id).unwrap();
    }

    #[test]
    fn store_does_not_return_until_the_write_is_readable() {
        // Regression for the live defect: connect stores then validates,
        // so an unconfirmed write made a good key look rejected.
        let _guard = test_lock();
        let id = scratch_id("visibility");
        store(&id, "just-written").unwrap();
        assert_eq!(
            load(&id, "SE_TEST_VAR_UNSET_ON_PURPOSE").as_deref(),
            Some("just-written"),
            "a successful store must guarantee an immediate read succeeds"
        );
        delete(&id).unwrap();
    }

    #[test]
    fn an_environment_variable_wins_over_the_store() {
        let _guard = test_lock();
        let id = scratch_id("precedence");
        let var = format!("SE_TEST_VAR_{}", std::process::id());
        store(&id, "from-store").unwrap();

        std::env::set_var(&var, "from-env");
        assert_eq!(source(&id, &var), Source::Environment);
        assert_eq!(load(&id, &var).as_deref(), Some("from-env"));

        std::env::remove_var(&var);
        assert_eq!(source(&id, &var), Source::SecureStore);
        assert_eq!(load(&id, &var).as_deref(), Some("from-store"));

        delete(&id).unwrap();
    }

    #[test]
    fn an_empty_environment_variable_is_not_a_credential() {
        let _guard = test_lock();
        let id = scratch_id("empty-env");
        let var = format!("SE_TEST_EMPTY_{}", std::process::id());
        std::env::set_var(&var, "");
        assert_eq!(source(&id, &var), Source::None);
        assert!(load(&id, &var).is_none());
        std::env::remove_var(&var);
    }

    #[test]
    fn an_empty_credential_is_refused_rather_than_stored() {
        let _guard = test_lock();
        let id = scratch_id("empty");
        assert!(store(&id, "   ").is_err());
        assert!(store(&id, "").is_err());
        assert_eq!(source(&id, "SE_TEST_VAR_UNSET_ON_PURPOSE"), Source::None);
    }

    /// A pasted key routinely carries a trailing newline. Left in, it
    /// produced an invalid-header error whose text could echo the
    /// header value -- the key itself leaking into a visible error.
    #[test]
    fn a_pasted_credential_is_trimmed_of_whitespace_and_control_characters() {
        let _guard = test_lock();
        let id = scratch_id("sanitise");
        let var = format!("SE_TEST_SANITISE_{}", std::process::id());

        std::env::set_var(&var, "  sk-with-newline\n");
        assert_eq!(load(&id, &var).as_deref(), Some("sk-with-newline"));

        std::env::set_var(&var, "sk\u{7}bell");
        assert_eq!(load(&id, &var).as_deref(), Some("skbell"));

        // Whitespace only is not a credential at all.
        std::env::set_var(&var, "   \n  ");
        assert!(load(&id, &var).is_none());
        std::env::remove_var(&var);
    }

    #[test]
    fn a_credential_that_cannot_go_in_a_header_is_rejected_without_quoting_it() {
        assert!(header_safe("sk-ascii-key"));
        assert!(!header_safe(""));
        // Non-ASCII cannot be a header value; the check reports that
        // without ever including the value in the message.
        assert!(!header_safe("sk-caf\u{e9}-key"));
    }

    #[test]
    fn the_fingerprint_never_contains_any_part_of_the_secret() {
        let secret = "nvapi-super-secret-value";
        let fp = fingerprint(secret);
        assert!(!fp.contains("nvapi"));
        assert!(!fp.contains("secret"));
        assert_eq!(fp, "24 characters");
    }

    #[test]
    fn a_configured_source_is_not_a_claim_of_authentication() {
        // The distinction the product direction insists on: these are
        // different questions, and only a request answers the second.
        assert!(Source::SecureStore.is_configured());
        assert!(Source::Environment.is_configured());
        assert!(!Source::None.is_configured());
        assert_eq!(Source::None.label(), "not configured");
    }
}
