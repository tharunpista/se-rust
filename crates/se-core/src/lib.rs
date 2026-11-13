//! SE's execution core.
//!
//! Owns the session, task and turn lifecycle, and owns the threads that
//! run work. Knows nothing about terminals: this crate has no dependency
//! on `ratatui` or `crossterm`, and that absence is what enforces
//! invariant I1 (the render/input path never blocks) structurally rather
//! than by convention. An interface cannot accidentally run a model call
//! on its own thread, because the only way in is
//! [`CoreHandle::submit`](engine::CoreHandle::submit), which returns a
//! task id immediately.
//!
//! ```no_run
//! use se_core::{start, EchoRunner};
//!
//! let core = start(EchoRunner::instant());
//! core.submit("hello", vec![]).unwrap();
//! // The interface loop: draw, read input, drain. Never blocks.
//! for event in core.drain() {
//!     println!("{:?}", event.payload);
//! }
//! ```

pub mod engine;
pub mod provider_runner;
pub mod routing;
pub mod runner;
pub mod session_store;
pub mod sink;

pub use engine::{start, CoreHandle, SubmitError};
pub use provider_runner::ProviderRunner;
pub use runner::{ApprovalRunner, EchoRunner, Runner};
pub use sink::{Disconnected, TurnSink};

// Re-exported so an interface needs one dependency, not two, for the
// common types. The protocol crate stays separately usable for anything
// that only wants the vocabulary (a log reader, a replay tool).
//
// `se-provider` is deliberately **not** re-exported: a reachable
// `se_core::provider::Registry` from `se-tui` would mean the UI could
// call blocking `stream()` on its own thread, violating I1. Any crate
// that genuinely needs the provider depends on `se-provider` directly
// (the binary does; the UI does not).
pub use se_protocol as protocol;
