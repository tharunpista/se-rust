//! Which listed models the stored credential can actually call.
//!
//! A provider's `/models` lists its whole catalogue, not what this key may
//! use (Apinex lists paid and subscription-only models to every key; NVIDIA
//! lists models that answer 404/410). So each listed model gets one tiny
//! probe, and the verdicts are cached on disk per provider + key + address.
//! The cache stores a one-way hash of the key, never the key.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How long verdicts are trusted before a listing re-probes everything.
/// One hour, not a day: NVIDIA's free catalogue moves within hours (a model
/// answering 404 at noon serves again by evening, and the reverse).
pub const TTL: Duration = Duration::from_secs(60 * 60);

/// What one probe said about one model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum Verdict {
    /// Answered: usable.
    Ok,
    /// Needs a subscription, balance or permission this key lacks.
    Paid(String),
    /// Not a chat model, or not found.
    NotChat(String),
    /// No verdict (429, 5xx, timeout, firewall page): still shown, re-probed next time.
    Unchecked(String),
    /// The key itself was refused. Never stored.
    BadKey(String),
}

impl Verdict {
    /// Whether this verdict hides the model from listings.
    pub fn hides(&self) -> bool {
        matches!(self, Verdict::Paid(_) | Verdict::NotChat(_))
    }

    /// Whether this verdict is final enough to cache (re-probing changes nothing until TTL).
    pub fn settled(&self) -> bool {
        matches!(self, Verdict::Ok | Verdict::Paid(_) | Verdict::NotChat(_))
    }

    /// One short phrase for the user.
    pub fn why(&self) -> String {
        match self {
            Verdict::Paid(r) => format!("needs a subscription or balance ({r})"),
            Verdict::NotChat(r) => format!("not a chat model or not found ({r})"),
            Verdict::Unchecked(r) | Verdict::BadKey(r) => r.clone(),
            Verdict::Ok => "available".to_string(),
        }
    }
}

/// One provider+key+address entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Unix seconds when the entry was (re)started; the TTL runs from here.
    pub checked_at: u64,
    pub models: BTreeMap<String, Verdict>,
}

impl Entry {
    pub fn fresh() -> Self {
        Entry {
            checked_at: now(),
            models: BTreeMap::new(),
        }
    }

    /// Whether `id` still needs a probe.
    pub fn needs_probe(&self, id: &str) -> bool {
        !self.models.get(id).is_some_and(Verdict::settled)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    entries: BTreeMap<String, Entry>,
}

/// `~/.se/model-access.json`, resolved the way the rest of SE finds `~/.se`.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| PathBuf::from(h).join(".se").join("model-access.json"))
}

/// The cache key: provider, the base URL's origin, and a one-way hash of the key.
/// A new key or a different address therefore never reuses old verdicts.
pub fn entry_key(provider: &str, api_key: &str, base_url: &str) -> String {
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        format!("se-model-access\0{api_key}").as_bytes(),
    );
    let hash: String = digest.as_ref()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let origin = crate::credentials::origin_of(base_url).unwrap_or_else(|| base_url.to_string());
    format!("{provider}|{origin}|{hash}")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn read(path: &Path) -> File {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// The entry for `key`, unless missing or older than `ttl`.
pub fn load(path: &Path, key: &str, ttl: Duration) -> Option<Entry> {
    let entry = read(path).entries.remove(key)?;
    (now().saturating_sub(entry.checked_at) < ttl.as_secs()).then_some(entry)
}

/// Saves `entry` under `key`, keeping only settled verdicts. Best effort:
/// a cache that cannot be written only means probing again next time.
// shortcut: read-modify-write without a lock, so a listing and a chat failure landing in the
// same instant can lose one update (it is re-learned on the next probe or failure).
pub fn store(path: &Path, key: &str, entry: &Entry) {
    let mut file = read(path);
    let mut entry = entry.clone();
    entry.models.retain(|_, v| v.settled());
    file.entries.insert(key.to_string(), entry);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if let Ok(text) = serde_json::to_string_pretty(&file) {
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Records one verdict learned outside a listing (a real chat turn failing).
pub fn mark(path: &Path, key: &str, model: &str, verdict: Verdict) {
    let mut entry = load(path, key, TTL).unwrap_or_else(Entry::fresh);
    entry.models.insert(model.to_string(), verdict);
    store(path, key, &entry);
}

/// The one-line picker note, e.g. "32 hidden: 28 need a subscription or balance, 4 not chat
/// models — /models refresh to re-check". `None` when nothing was hidden.
pub fn hidden_summary(paid: usize, not_chat: usize, refresh_hint: &str) -> Option<String> {
    let mut parts = Vec::new();
    if paid > 0 {
        parts.push(format!("{paid} need a subscription or balance"));
    }
    if not_chat > 0 {
        parts.push(format!("{not_chat} not chat models"));
    }
    (!parts.is_empty()).then(|| {
        format!(
            "{} hidden: {} — {refresh_hint} to re-check",
            paid + not_chat,
            parts.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("se-model-access-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("model-access.json")
    }

    #[test]
    fn the_cache_key_never_contains_the_api_key_and_changes_with_key_and_address() {
        let secret = "sk-test-SECRETVALUE-123456";
        let k = entry_key("apinex", secret, "https://api.apinex.bond/v1");
        assert!(!k.contains(secret) && !k.contains("SECRETVALUE"), "{k}");
        assert!(k.starts_with("apinex|https://api.apinex.bond|"), "{k}");
        assert_ne!(
            k,
            entry_key("apinex", "sk-other", "https://api.apinex.bond/v1")
        );
        assert_ne!(k, entry_key("apinex", secret, "https://other.example/v1"));
        // Same origin, different path: same entry.
        assert_eq!(k, entry_key("apinex", secret, "https://api.apinex.bond/v2"));
    }

    #[test]
    fn the_stored_file_never_contains_the_key_and_drops_unsettled_verdicts() {
        let path = temp_file("nokey");
        let secret = "sk-test-NEVERSTORED-999";
        let key = entry_key("custom", secret, "http://127.0.0.1:1/v1");
        let mut e = Entry::fresh();
        e.models.insert("a".into(), Verdict::Ok);
        e.models
            .insert("b".into(), Verdict::Paid("Insufficient balance".into()));
        e.models
            .insert("c".into(), Verdict::Unchecked("HTTP 503".into()));
        store(&path, &key, &e);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("NEVERSTORED"));
        let back = load(&path, &key, TTL).unwrap();
        assert_eq!(back.models.len(), 2);
        assert!(back.needs_probe("c") && !back.needs_probe("a") && !back.needs_probe("b"));
    }

    #[test]
    fn an_expired_entry_is_not_loaded() {
        let path = temp_file("ttl");
        let key = entry_key("custom", "k", "http://127.0.0.1:1/v1");
        let mut old = Entry::fresh();
        old.checked_at = now() - TTL.as_secs() - 1;
        old.models.insert("a".into(), Verdict::Ok);
        store(&path, &key, &old);
        assert_eq!(load(&path, &key, TTL), None);
        old.checked_at = now();
        store(&path, &key, &old);
        assert!(load(&path, &key, TTL).is_some());
    }

    #[test]
    fn mark_adds_one_verdict_to_the_live_entry() {
        let path = temp_file("mark");
        let key = entry_key("custom", "k", "http://127.0.0.1:1/v1");
        let mut e = Entry::fresh();
        e.models.insert("a".into(), Verdict::Ok);
        store(&path, &key, &e);
        mark(&path, &key, "b", Verdict::Paid("x".into()));
        let back = load(&path, &key, TTL).unwrap();
        assert_eq!(back.models.get("a"), Some(&Verdict::Ok));
        assert!(back.models["b"].hides());
    }

    #[test]
    fn the_hidden_summary_is_one_line_with_counts_and_reasons() {
        assert_eq!(
            hidden_summary(28, 4, "/models refresh").as_deref(),
            Some("32 hidden: 28 need a subscription or balance, 4 not chat models — /models refresh to re-check")
        );
        assert_eq!(
            hidden_summary(0, 2, "x").as_deref(),
            Some("2 hidden: 2 not chat models — x to re-check")
        );
        assert_eq!(hidden_summary(0, 0, "x"), None);
    }
}
