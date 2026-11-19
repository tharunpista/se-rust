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

/// A separate keyring row recording which origin a credential may be
/// sent to. `#origin` cannot collide with a real provider id (ids are
/// alphanumeric), and an origin is not a secret -- it is stored for
/// detection, not confidentiality.
fn origin_entry(id: &str) -> Result<keyring::Entry, String> {
    entry(&format!("{id}#origin"))
}

/// The API address the user gave `se connect` for a service with none built in. Not a
/// secret: kept beside the key so it is set, pinned and removed together with it.
fn url_entry(id: &str) -> Result<keyring::Entry, String> {
    entry(&format!("{id}#url"))
}

pub fn stored_base_url(id: &str) -> Option<String> {
    url_entry(id).ok()?.get_password().ok()
}

pub fn store_base_url(id: &str, url: &str) -> Result<(), String> {
    url_entry(id)?
        .set_password(url)
        .map_err(|e| e.to_string())
}

/// Normalises a URL to its origin: scheme + authority (host and port,
/// if given), lowercased. Enough to detect "this credential is now
/// being sent somewhere new" without caring about the request path.
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    // A URL carrying userinfo is refused, not stripped to its host and
    // accepted: `is_safe_origin` in `openai_compatible.rs` already
    // rejects such a URL before any request is built, so returning
    // `None` here (rather than the host alone) is defense in depth,
    // not the primary control -- but it matters on its own, because
    // the caller of a `None` here (`check_pinned_origin`) treats "no
    // origin to check" as `Ok`. A silently-stripped userinfo origin
    // would otherwise get pinned and later echoed verbatim in
    // `check_origin`'s error message, which is exactly how a
    // credential embedded in a URL used to leak through this path.
    if authority.contains('@') {
        return None;
    }
    Some(format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    ))
}

/// Records the origin a newly stored credential is pinned to.
fn pin_origin(id: &str, origin: &str) -> Result<(), String> {
    origin_entry(id)?
        .set_password(origin)
        .map_err(|e| e.to_string())
}

fn pinned_origin(id: &str) -> Option<String> {
    origin_entry(id).ok()?.get_password().ok()
}

/// Checked before a stored credential is attached to a request.
///
/// Fix for a verified exfiltration path (E2): a credential used to be
/// keyed only by provider id, with no record of where it was meant to
/// go. Changing `*_BASE_URL` -- including by an attacker who can set
/// one environment variable -- silently redirected an already-stored
/// key to a new host on the very next request, no prompt. A credential
/// now carries the origin it was pinned to when stored; a later mismatch
/// is refused rather than silently honoured.
///
/// No pin recorded (a keyless service, or a credential stored before
/// this check existed) is not a mismatch -- `store` pins the origin
/// going forward, so this converges rather than locking existing users
/// out.
pub fn check_origin(id: &str, current_origin: &str) -> Result<(), String> {
    match pinned_origin(id) {
        None => Ok(()),
        Some(pinned) if pinned == current_origin => Ok(()),
        Some(pinned) => Err(format!(
            "this credential is pinned to {pinned}, but the configured endpoint is now {current_origin} — run `se connect` again to confirm and re-pin it"
        )),
    }
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
///
/// Rejects control characters (including CR/LF) as well as non-ASCII.
/// `sanitise` already strips these before a key reaches `from_raw` on
/// every production path, but `HeaderSafeKey::from_raw` is a public
/// constructor -- a future caller that builds one from an un-sanitised
/// source must not be able to smuggle a header-injection payload
/// (`"key\r\nX-Injected: 1"` is ASCII) past a check whose name claims
/// the value is header-safe. The type should guarantee its own name.
pub fn header_safe(secret: &str) -> bool {
    !secret.is_empty() && secret.is_ascii() && !secret.chars().any(|c| c.is_control())
}

/// A credential that has been checked and may be placed in an HTTP
/// header.
///
/// Constructible **only** through [`HeaderSafeKey::from_raw`], which
/// runs `header_safe`. The provider request path takes `&HeaderSafeKey`,
/// not `&str`, so a non-ASCII (or empty) value cannot reach
/// `set("Authorization", ...)` by any path -- the compiler, not a
/// reviewer, enforces it. This closes a verified defect: the earlier
/// version of this module defined `header_safe` but never called it on
/// the request path, so a non-ASCII key reached `ureq` and leaked back
/// through the invalid-header error (I8).
///
/// No `Debug` or `Display`: a secret must never appear in a `{:?}` or a
/// `{}`-formatted log line.
pub struct HeaderSafeKey(String);

impl HeaderSafeKey {
    /// `Err` carries a one-line, value-free reason. Never contains any
    /// part of `raw`.
    pub fn from_raw(raw: &str) -> Result<Self, &'static str> {
        if raw.is_empty() {
            return Err("the credential is empty");
        }
        if !header_safe(raw) {
            return Err(
                "the credential contains non-ASCII characters and cannot be sent as an HTTP header",
            );
        }
        Ok(HeaderSafeKey(raw.to_string()))
    }

    /// The exact header value, including the `Bearer ` prefix every
    /// catalogued service uses.
    pub fn as_bearer(&self) -> String {
        format!("Bearer {}", self.0)
    }

    /// The bare key, for services that take it in their own header
    /// (Anthropic's `x-api-key`). Same header-safety guarantee.
    pub fn as_header_value(&self) -> &str {
        &self.0
    }
}

pub fn source(id: &str, env_var: &str) -> Source {
    // Same `sanitise` rule as `load` -- otherwise a whitespace-only env
    // var reports `Environment` (configured) while `load` returns None
    // and falls through to the keyring, so the user thinks they
    // scoped a key to this shell while the keyring key is actually
    // being used. A silent account substitution.
    if std::env::var(env_var)
        .map(|v| !sanitise(&v).is_empty())
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
///
/// `origin`, when given, is pinned (E2): every later use of this
/// credential is checked against it via [`check_origin`], and a mismatch
/// is refused rather than silently honoured. Pass `None` only for a
/// service that genuinely has no fixed endpoint to pin (there are
/// none in the current catalogue -- every credential-bearing service
/// has a configured endpoint by the time `store` is called).
pub fn store(id: &str, secret: &str, origin: Option<&str>) -> Result<(), String> {
    if secret.trim().is_empty() {
        return Err("refusing to store an empty credential".to_string());
    }
    let entry = entry(id)?;
    entry.set_password(secret).map_err(|e| e.to_string())?;

    for _ in 0..VISIBILITY_ATTEMPTS {
        if entry.get_password().ok().as_deref() == Some(secret) {
            if let Some(origin) = origin {
                pin_origin(id, origin)?;
            }
            return Ok(());
        }
        std::thread::sleep(VISIBILITY_PAUSE);
    }
    Err(format!(
        "the credential for `{id}` was written but secure storage has not made it readable yet — try again"
    ))
}

/// Removes a credential, and the origin it was pinned to. Absent is
/// success: the caller asked for "no credential stored", which is the
/// end state either way.
pub fn delete(id: &str) -> Result<(), String> {
    // Best-effort: a missing or already-absent pin must never block
    // removing the credential itself.
    for side in [origin_entry(id), url_entry(id)] {
        let _ = side.and_then(|e| match e.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        });
    }
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

/// Vendor key prefixes (`sk-` also covers `sk-ant-`, `sk-proj-`, `sk-or-`).
const SECRET_PREFIXES: &[&str] = &[
    "sk-", "sk_", "nvapi-", "gsk_", "AIza", "ghp_", "gho_", "ghu_", "ghs_", "github_pat_",
    "xoxb-", "xoxp-", "xoxa-", "hf_", "r8_", "pplx-", "glpat-", "xai-", "fw_", "AKIA",
];

const KEY_WORDS: &[&str] = &["key", "token", "secret", "apikey", "="];

fn digits(s: &str) -> usize {
    s.chars().filter(char::is_ascii_digit).count()
}

/// A vendor-prefixed key: at least 16 characters after the prefix, and either 3+ digits
/// or one unbroken 20+ character run, so identifiers like `sk_buff_head_v2_allocation`
/// and prose like `sk-learn-and-numpy-...` do not count.
fn is_prefixed_key(t: &str) -> bool {
    SECRET_PREFIXES.iter().any(|p| {
        t.strip_prefix(p).is_some_and(|rest| {
            rest.len() >= 16
                && (digits(rest) >= 3 || rest.split(['-', '_']).any(|piece| piece.len() >= 20))
        })
    })
}

/// A random-looking key with no known prefix: one unbroken 24+ character run (split on
/// `-`/`_` too, so slugs and snake/kebab identifiers never qualify) with 3+ digits and
/// both cases. Or, on a line that talks about a key (`key`, `token`, `secret`, `=`), a
/// 40+ character lowercase+digit run containing a non-hex letter.
/// Not caught, by choice: pure hex (git SHAs, and Azure's 32-hex keys, which look the same).
fn is_random_key(t: &str, keyish_line: bool) -> bool {
    let has = |s: &str, f: fn(&char) -> bool| s.chars().any(|c| f(&c));
    t.split(['-', '_']).any(|piece| {
        piece.len() >= 24
            && digits(piece) >= 3
            && has(piece, char::is_ascii_lowercase)
            && has(piece, char::is_ascii_uppercase)
    }) || (keyish_line
        && t.len() >= 40
        && digits(t) >= 3
        && t.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && has(t, |c| ('g'..='z').contains(c)))
}

/// Runs of `[A-Za-z0-9_-]`, the alphabet keys are drawn from. Anything else ends a token,
/// and so does a backslash with the character after it, so JSON's `\nsk-...` is caught.
fn tokens(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = None;
    let mut chars = text.char_indices();
    while let Some((i, c)) = chars.next() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            start.get_or_insert(i);
            continue;
        }
        if let Some(s) = start.take() {
            out.push(&text[s..i]);
        }
        if c == '\\' {
            chars.next();
        }
    }
    if let Some(s) = start {
        out.push(&text[s..]);
    }
    out
}

/// Every credential-shaped token in `text`. Words that are URLs, `data:` URIs or
/// `sha256-`/`sha512-` integrity strings only face the vendor-prefix rule: their long
/// random ids (Google Docs, base64) are not keys.
fn secret_tokens(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        let keyish = KEY_WORDS.iter().any(|w| lower.contains(w));
        for word in line.split_whitespace() {
            let opaque = word.contains("://")
                || word.contains("data:")
                || ["sha1-", "sha256-", "sha384-", "sha512-"]
                    .iter()
                    .any(|p| word.contains(p));
            for t in tokens(word) {
                if is_prefixed_key(t) || (!opaque && is_random_key(t, keyish)) {
                    found.push(t);
                }
            }
        }
    }
    found
}

/// Whether text carries something shaped like an API key or token, anywhere in it.
/// Used to stop a pasted key from reaching a model, the transcript or a session file.
pub fn looks_like_secret(text: &str) -> bool {
    !secret_tokens(text).is_empty()
}

/// `text` with every credential-shaped token replaced by `[REDACTED]`: the last line of
/// defence before anything is written to disk or loaded back from it.
pub fn redact_secrets(text: &str) -> String {
    let mut out = text.to_string();
    for t in secret_tokens(text) {
        out = out.replace(t, "[REDACTED]");
    }
    out
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

    #[test]
    fn credential_shaped_text_is_caught_and_prose_is_not() {
        // Fake keys, shaped like real ones.
        for key in [
            "sk-abcdefghij0123456789ABCDEFGHIJ0123456789abcdefghij0",
            "sk-proj-Ab3dEf6hIj9kLm2nOp5q",
            "sk-ant-api03-abcdef1234567890",
            "sk-or-v1-0123456789abcdef0123456789abcdef",
            "nvapi-0123456789abcdefABCDEF",
            "xai-Ab12Cd34Ef56Gh78Ij90Kl",
            "fw_3ZkXq81vN2pLm7Rt4Yw9Bc",
            "AIzaSyA1234567890abcdefghijklmnopqrstu",
            "ghp_0123456789abcdefghijABCDEFGHIJ012345",
            "AKIAABCDEFGHIJ012345",
            "Zq8XkLm2Pw9vRt4Yn6Bc1Hd5Jf3Gs7Ae0Uo", // no prefix, random
        ] {
            assert!(looks_like_secret(key), "{key}");
            assert!(looks_like_secret(&format!("here is my key: {key} thanks")), "{key}");
            assert_eq!(redact_secrets(&format!("k={key}!")), "k=[REDACTED]!");
        }
        // 40+ lowercase+digit, only on a line that talks about a key.
        let lower = "q7w8e9r0t1y2u3i4o5p6a7s8d9f0g1h2j3k4l5z6x7";
        assert!(looks_like_secret(&format!("api_key={lower}")));
        assert!(!looks_like_secret(&format!("build {lower} finished")));
        // JSON-escaped newline in front of a key, as on disk.
        let json = r#"{"content":"hello\nsk-abcdefghij0123456789ABCDEFGHIJ"}"#;
        assert!(looks_like_secret(json));
        assert_eq!(redact_secrets(json), r#"{"content":"hello\n[REDACTED]"}"#);
        // A vendor key in a URL query still counts.
        assert!(looks_like_secret("https://x.example/v1?key=sk-abcdefghij0123456789ABCDEF"));

        for prose in [
            "did it connect the api key",
            "use sk-learn-and-numpy-for-this-task please",
            "free sk_buff_head_v2_allocation on exit",
            "see %USERPROFILE%\\SE\\se-rust\\crates\\se-provider\\src\\credentials.rs",
            "https://integrate.api.nvidia.com/v1/chat/completions?stream=true",
            "commit 3f9a1c2b4d5e6f7081928374655647382910abcd broke it", // lowercase hex SHA
            "sha=3f9a1c2b4d5e6f7081928374655647382910abcd",              // hex even with =
            "rename handle_provider_connection_request_for_tui to something shorter",
            "/connect apinex https://api.apinex.example/v1",
            "JIRA-1234-Add-new-login-flow-for-users",
            "ConfigureServicesForOAuth2Provider",
            "test_handles_HTTP2_upgrade_when_server_rejects_it",
            "https://docs.google.com/document/d/1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgvE2upms/edit",
            "\"integrity\": \"sha512-Ab3dEf6hIj9kLm2nOp5qRs8tUv1wXy4zAb3dEf6hIj9kLm2nOp5qRs8tUv1wXy4z==\"",
            "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
        ] {
            assert!(!looks_like_secret(prose), "{prose}");
            assert_eq!(redact_secrets(prose), prose);
        }
    }

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

        store(&id, "secret-value", None).unwrap();
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
        store(&id, "just-written", None).unwrap();
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
        store(&id, "from-store", None).unwrap();

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
        assert!(store(&id, "   ", None).is_err());
        assert!(store(&id, "", None).is_err());
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

    /// A sanity check that the token type's error messages never
    /// contain any part of the input. The earlier version of this
    /// module defined `header_safe` but never called it on the request
    /// path, so a non-ASCII key reached ureq and leaked back through
    /// the invalid-header error. `HeaderSafeKey::from_raw` is the one
    /// place that check now lives, and this test guards it.
    #[test]
    fn a_header_safe_key_rejects_non_ascii_and_empty_without_echoing_the_value() {
        // `HeaderSafeKey` deliberately has no `Debug` (I8: a secret
        // must never appear in a `{:?}` rendering), so the result is
        // `match`ed here rather than `.expect_err()`-ed, which would
        // need `Debug` on the `Ok` side.
        match HeaderSafeKey::from_raw("sk-caf\u{e9}-key") {
            Err(err) => {
                assert!(!err.contains("caf"), "{err}");
                assert!(!err.contains('\u{e9}'), "{err}");
                assert!(err.contains("non-ASCII"));
            }
            Ok(_) => panic!("non-ASCII must be refused"),
        }
        match HeaderSafeKey::from_raw("") {
            Err(err) => assert!(!err.is_empty()),
            Ok(_) => panic!("empty must be refused"),
        }
        // A good key builds a header value with a deterministic prefix
        // and no surprise additions.
        let Ok(key) = HeaderSafeKey::from_raw("sk-ascii") else {
            panic!("ascii is fine");
        };
        assert_eq!(key.as_bearer(), "Bearer sk-ascii");
    }

    /// Regression for a confirmed defect in the previous release:
    /// `source()` tested the raw env value while `load()` sanitised it,
    /// so a whitespace-only env var reported Environment-configured
    /// while the keyring key was actually used. A silent account
    /// substitution where the user thought they had scoped a shell key.
    #[test]
    fn a_whitespace_only_env_var_is_not_reported_as_an_env_credential() {
        let _guard = test_lock();
        let id = scratch_id("source-blank-env");
        let var = format!("SE_TEST_BLANK_{}", std::process::id());
        std::env::set_var(&var, "   \n\t  ");
        // load() returns None (sanitise strips the whitespace), so
        // source() must agree -- otherwise the UI shows one state and
        // the next call uses a different one.
        assert_eq!(source(&id, &var), Source::None);
        assert!(load(&id, &var).is_none());
        std::env::remove_var(&var);
    }

    /// E2: a credential pinned to one origin at store time must be
    /// refused when the configured endpoint later points somewhere
    /// else. This is the fix for a live exfiltration path: an
    /// attacker-set `*_BASE_URL` env var used to silently redirect an
    /// already-stored key to a new host on the very next request.
    #[test]
    fn a_credential_is_pinned_and_a_later_origin_change_is_refused() {
        let _guard = test_lock();
        let id = scratch_id("origin-pin");
        store(&id, "secret-value", Some("https://api.example.com")).unwrap();

        // Same origin: fine.
        assert!(check_origin(&id, "https://api.example.com").is_ok());

        // A different origin: refused, and the message names neither
        // the secret nor anything that could be mistaken for it.
        let err = check_origin(&id, "https://attacker.example.com").unwrap_err();
        assert!(err.contains("api.example.com"), "{err}");
        assert!(err.contains("attacker.example.com"), "{err}");
        assert!(!err.contains("secret-value"), "{err}");

        delete(&id).unwrap();
    }

    /// No pin recorded (a credential stored before this check existed,
    /// or a keyless service) must not lock the user out -- it converges
    /// by pinning going forward, rather than failing closed on
    /// everyone's existing setup.
    #[test]
    fn a_credential_with_no_pinned_origin_is_allowed_through() {
        let _guard = test_lock();
        let id = scratch_id("origin-unpinned");
        // Stored with no origin -- as `store` allows.
        store(&id, "secret-value", None).unwrap();
        assert!(check_origin(&id, "https://anything.example.com").is_ok());
        delete(&id).unwrap();
    }

    /// Deleting a credential must also forget its pin, so a later
    /// `connect` to the same id starts clean rather than inheriting a
    /// stale origin from a previous, unrelated setup.
    #[test]
    fn deleting_a_credential_also_forgets_its_pinned_origin() {
        let _guard = test_lock();
        let id = scratch_id("origin-delete");
        store(&id, "secret-value", Some("https://api.example.com")).unwrap();
        delete(&id).unwrap();
        // Re-store under a different origin: must not be refused by a
        // pin that should have been removed.
        store(&id, "secret-value", Some("https://other.example.com")).unwrap();
        assert!(check_origin(&id, "https://other.example.com").is_ok());
        delete(&id).unwrap();
    }

    #[test]
    fn origin_of_normalises_scheme_and_authority_and_ignores_the_path() {
        assert_eq!(
            origin_of("https://API.Example.com:443/v1/models"),
            Some("https://api.example.com:443".to_string())
        );
        assert_eq!(
            origin_of("http://127.0.0.1:11434/v1/chat/completions"),
            Some("http://127.0.0.1:11434".to_string())
        );
        assert_eq!(origin_of("not-a-url"), None);
        assert_eq!(origin_of(""), None);
    }

    #[test]
    fn origin_of_refuses_rather_than_strips_a_url_carrying_userinfo() {
        // `is_safe_origin` in openai_compatible.rs is the primary gate
        // and already refuses such a URL before a request is ever
        // built, so this is defense in depth -- but it is still load
        // bearing: `check_pinned_origin` treats `origin_of`'s `None` as
        // "nothing to check" (Ok), and treats a *stripped* host-only
        // origin as something to pin and later echo back verbatim in
        // an error message. Stripping, not refusing, is exactly how a
        // credential embedded in a URL used to leak through this path.
        assert_eq!(
            origin_of("https://user:secret-key@attacker.example.com/v1"),
            None
        );
        assert_eq!(origin_of("http://user:secret-key@127.0.0.1/v1"), None);
    }

    /// A control character hidden in an otherwise-ASCII secret must be
    /// refused too, not only non-ASCII. Closes a defense-in-depth gap:
    /// `HeaderSafeKey::from_raw` is a public constructor, and CR/LF are
    /// ASCII, so a header-injection payload like
    /// `"key\r\nX-Injected: 1"` would otherwise pass the ASCII check
    /// even though production `sanitise` already strips it upstream.
    #[test]
    fn header_safe_rejects_control_characters_even_though_they_are_ascii() {
        assert!(!header_safe("key\r\nX-Injected: 1"));
        assert!(!header_safe("key\nbody"));
        assert!(!header_safe("key\u{7}bell"));
        assert!(header_safe("key-with-no-control-chars"));
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
