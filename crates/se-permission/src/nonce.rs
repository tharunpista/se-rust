//! Single-use token nonces. Deliberately never persisted: the
//! architecture requires "a persisted token must never be honoured
//! after resume," and an in-memory-only ledger gets that for free
//! rather than needing a separate invalidate-on-restart step.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nonce(u64);

static NEXT: AtomicU64 = AtomicU64::new(1);

pub struct NonceLedger {
    spent: Mutex<HashSet<Nonce>>,
}

impl NonceLedger {
    pub fn new() -> Self {
        Self {
            spent: Mutex::new(HashSet::new()),
        }
    }

    pub fn issue(&self) -> Nonce {
        Nonce(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns `true` the first time a nonce is consumed, `false`
    /// every time after — including the very first `issue()`, a
    /// nonce is never valid until a token actually presents it here.
    pub fn consume(&self, nonce: &Nonce) -> bool {
        self.spent.lock().unwrap().insert(*nonce)
    }
}

impl Default for NonceLedger {
    fn default() -> Self {
        Self::new()
    }
}
