//! What the binary lends the interface. This crate never reaches into
//! providers or the catalogue itself; the binary hands it these instead,
//! the same way it hands over the team launcher.

use std::sync::{Arc, Mutex};

/// One provider's models, or why they could not be listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModels {
    pub provider: String,
    /// Only the models this credential can use (or that could not be checked).
    pub models: Result<Vec<String>, String>,
    /// Ids among `models` whose access check gave no verdict; shown as "unchecked".
    pub unchecked: Vec<String>,
    /// One line on what was hidden and why, if anything was.
    pub note: Option<String>,
}

/// Lists every configured provider's models; `true` re-checks access
/// instead of trusting the cache. Network I/O and process starts, so it
/// is only ever called on a background thread.
pub type ModelLister = Arc<dyn Fn(bool) -> Vec<ProviderModels> + Send + Sync>;

#[derive(Default)]
pub struct Services {
    pub team: Option<crate::team::Launcher>,
    pub list_models: Option<ModelLister>,
    /// The `se providers` report. Cheap: no network call.
    pub providers: Option<Report>,
    /// The `se doctor` report, built once at startup.
    pub doctor: Option<String>,
    /// Saves a model picked with `/models` so the next launch uses it.
    pub remember_model: Option<ModelMemory>,
    /// Every provider with its real state (key stored, CLI logged in).
    /// Starts processes (login checks), so it runs on a background thread.
    pub list_providers: Option<ProviderLister>,
    /// Stores an API key in OS secure storage and validates it against the
    /// provider. Network I/O: background thread.
    pub save_key: Option<KeySaver>,
    /// Puts text on the system clipboard (`/copy`); returns a line to show.
    pub copy: Option<Clipboard>,
    /// The model SE starts with (flags or the remembered pick), if any,
    /// so the header and `/models` can name it.
    pub default_model: Option<se_protocol::ModelRef>,
    /// Opens a conversation in this project with its own core, exactly as
    /// `se --resume` does (`None`: a new one). Used by `/resume` and `/new`.
    pub open_session: Option<SessionOpener>,
    /// The conversation SE started with.
    pub session_id: Option<String>,
    /// This project's `.se/sessions`.
    pub sessions_dir: Option<std::path::PathBuf>,
    /// `~/.se/recent-projects.json`, for the picker's all-projects view.
    pub recent_projects: Option<std::path::PathBuf>,
    /// The work mode to start in (`se --mode`).
    pub mode: se_protocol::WorkMode,
}

/// A conversation the binary opened: its core, id, the entries to redraw,
/// its saved specialist and any warning to show.
pub struct Opened {
    pub core: se_core::CoreHandle,
    pub id: String,
    pub prior: Vec<crate::Entry>,
    pub specialist: Option<String>,
    pub warning: Option<String>,
}

pub type SessionOpener = Arc<dyn Fn(Option<&str>) -> Result<Opened, String> + Send + Sync>;

pub type ModelMemory = Arc<dyn Fn(&se_protocol::ModelRef) + Send + Sync>;
pub type ProviderLister = Arc<dyn Fn() -> Vec<ProviderEntry> + Send + Sync>;
/// `(provider id, key, API address if the user gave one)` -> a line to show
/// the user. The key is never echoed back in either result.
pub type KeySaver = Arc<dyn Fn(&str, &str, Option<&str>) -> Result<String, String> + Send + Sync>;
pub type Clipboard = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// One provider as `/connect` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEntry {
    pub id: String,
    pub name: String,
    /// An API provider takes a pasted key; an agent CLI uses its own login.
    pub uses_key: bool,
    /// SE has no API address for it yet, so `/connect <id> <url>` must give one.
    pub needs_url: bool,
    /// Usable now: a key is stored, or the CLI is logged in.
    pub ready: bool,
    pub status: String,
}

/// A binary-supplied function held in the (Debug, Clone) interface state.
#[derive(Clone)]
pub struct Hook<T>(pub Option<T>);

impl<T> Default for Hook<T> {
    fn default() -> Self {
        Hook(None)
    }
}

impl<T> std::fmt::Debug for Hook<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Hook(set)"
        } else {
            "Hook(none)"
        })
    }
}

/// What background work hands back to the interface loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    Notice(String),
    Providers(Vec<ProviderEntry>),
    /// A key was stored: model lists are stale.
    KeySaved,
}

/// Messages from background threads, drained by the interface loop.
#[derive(Clone, Default)]
pub struct Inbox(Arc<Mutex<Vec<Inbound>>>);

impl std::fmt::Debug for Inbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Inbox")
    }
}

impl Inbox {
    pub fn send(&self, m: Inbound) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).push(m);
    }

    pub fn take(&self) -> Vec<Inbound> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// A report the binary can rebuild on demand (it reflects current state).
#[derive(Clone)]
pub struct Report(pub Arc<dyn Fn() -> String + Send + Sync>);

impl std::fmt::Debug for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Report")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ListingState {
    #[default]
    NotLoaded,
    Loading,
    Ready(Vec<ProviderModels>),
}

/// The model listing, filled in by a background thread and read by the
/// interface loop. Cloned handles share one slot.
#[derive(Clone, Default)]
pub struct Listing {
    slot: Arc<Mutex<ListingState>>,
    lister: Option<ModelLister>,
}

impl std::fmt::Debug for Listing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Listing").field(&self.state()).finish()
    }
}

impl Listing {
    pub fn new(lister: Option<ModelLister>) -> Self {
        Listing {
            slot: Arc::default(),
            lister,
        }
    }

    /// Forgets the listing, so the next `/models` asks again (after a
    /// new key, say).
    pub fn reset(&self) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if *slot != ListingState::Loading {
            *slot = ListingState::NotLoaded;
        }
    }

    pub fn state(&self) -> ListingState {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Starts listing on a background thread unless already started.
    /// Returns false when there is nothing to list with.
    pub fn start(&self) -> bool {
        self.begin(false)
    }

    /// `/models refresh`: forgets the listing and lists again, re-checking access.
    pub fn refresh(&self) -> bool {
        self.reset();
        self.begin(true)
    }

    fn begin(&self, refresh: bool) -> bool {
        let Some(lister) = self.lister.clone() else {
            return false;
        };
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if *slot != ListingState::NotLoaded {
            return true;
        }
        *slot = ListingState::Loading;
        let shared = self.slot.clone();
        std::thread::spawn(move || {
            let result = lister(refresh);
            *shared.lock().unwrap_or_else(|e| e.into_inner()) = ListingState::Ready(result);
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_runs_once_off_the_calling_thread() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        let lister: ModelLister = Arc::new(move |_| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            vec![ProviderModels {
                provider: "p".into(),
                models: Ok(vec!["m".into()]),
                unchecked: Vec::new(),
                note: None,
            }]
        });
        let listing = Listing::new(Some(lister));
        assert!(listing.start());
        assert!(listing.start(), "a second start is a no-op");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !matches!(listing.state(), ListingState::Ready(_)) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!Listing::new(None).start());
    }
}
