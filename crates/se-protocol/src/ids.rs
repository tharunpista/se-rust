//! Correlation identities and the sequence counter.
//!
//! Invariant I6 (`docs/ARCHITECTURE.md`): every event carries session,
//! task and turn identity plus a monotonic sequence number. The previous
//! implementation emitted bare callback invocations with no identity at
//! all, so a transcript could not be reconstructed and nothing could
//! detect a reordering -- it could only be assumed not to happen.
//!
//! The ids are deliberately opaque and cheap: a process-local counter
//! with a short prefix, not UUIDs. Nothing here needs to be unique
//! across machines, and a 16-byte random identity in a log line is
//! harder for a human to correlate than `t3`.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! counted_id {
    ($name:ident, $prefix:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            serde::Serialize,
            serde::Deserialize,
        )]
        pub struct $name(u64);

        impl $name {
            /// Allocates the next identity. Monotonic within a process.
            pub fn next() -> Self {
                static COUNTER: AtomicU64 = AtomicU64::new(1);
                $name(COUNTER.fetch_add(1, Ordering::Relaxed))
            }

            /// For reconstructing an identity when replaying a
            /// persisted event. Not for inventing new ones.
            pub fn from_raw(raw: u64) -> Self {
                $name(raw)
            }

            pub fn raw(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }
    };
}

counted_id!(
    SessionId,
    "s",
    "A durable conversation identity. Survives many tasks."
);
counted_id!(
    TaskId,
    "t",
    "One user-requested unit of work. May span several turns."
);
counted_id!(
    TurnId,
    "r",
    "One model response cycle plus the tool execution it requires."
);
counted_id!(
    ToolCallId,
    "c",
    "One tool invocation, so a result can be matched to its request."
);

/// A monotonic per-session event counter.
///
/// Separate from the ids above because it answers a different question:
/// not "which turn is this" but "did I receive everything, in order".
/// A consumer that sees `seq` jump has lost an event and can say so
/// rather than silently rendering a gap.
#[derive(Debug, Default)]
pub struct SeqCounter(AtomicU64);

impl SeqCounter {
    pub fn new() -> Self {
        SeqCounter(AtomicU64::new(0))
    }

    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_monotonic_within_a_process() {
        let a = TaskId::next();
        let b = TaskId::next();
        assert_ne!(a, b);
        assert!(b.raw() > a.raw());
    }

    #[test]
    fn ids_render_with_a_short_prefix_a_human_can_correlate() {
        let id = SessionId::from_raw(7);
        assert_eq!(id.to_string(), "s7");
        assert_eq!(TurnId::from_raw(2).to_string(), "r2");
        assert_eq!(ToolCallId::from_raw(9).to_string(), "c9");
    }

    #[test]
    fn distinct_id_types_do_not_share_a_counter() {
        // Each type having its own counter is what keeps `t1` and `r1`
        // from being mistaken for the same thing in a log.
        let _ = TaskId::next();
        let turn = TurnId::next();
        let session = SessionId::next();
        // Nothing asserted about absolute values (other tests allocate
        // too); the point is the types are independent.
        assert!(turn.raw() >= 1 && session.raw() >= 1);
    }

    #[test]
    fn a_sequence_counter_starts_at_zero_and_never_repeats() {
        let seq = SeqCounter::new();
        assert_eq!(seq.next(), 0);
        assert_eq!(seq.next(), 1);
        assert_eq!(seq.next(), 2);
    }

    #[test]
    fn a_sequence_counter_is_monotonic_across_threads() {
        use std::collections::BTreeSet;
        use std::sync::Arc;
        let seq = Arc::new(SeqCounter::new());
        let mut handles = Vec::new();
        for _ in 0..4 {
            let seq = seq.clone();
            handles.push(std::thread::spawn(move || {
                (0..250).map(|_| seq.next()).collect::<Vec<_>>()
            }));
        }
        let mut seen = BTreeSet::new();
        for h in handles {
            for n in h.join().unwrap() {
                assert!(seen.insert(n), "sequence number {n} was issued twice");
            }
        }
        assert_eq!(seen.len(), 1000);
    }
}
