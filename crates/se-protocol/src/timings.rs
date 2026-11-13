//! Stage timings.
//!
//! The blueprint is blunt about why this exists: a 90-second reply to
//! "hi" is a diagnostic problem, not a language-choice problem, and it
//! cannot be diagnosed by timing the whole request. Local work and model
//! work must be measured separately, and time-to-first-token separately
//! from total generation.
//!
//! So timings are a first-class part of the protocol rather than
//! something bolted on later: the core records spans as it goes, and any
//! interface can show them. Nothing here ever records prompt text or a
//! credential -- only names and durations.

use std::time::{Duration, Instant};

/// The stages worth measuring independently. Named rather than free-form
/// so two builds produce comparable numbers.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Stage {
    /// Process start to the first frame drawn.
    Startup,
    /// Reading and validating configuration.
    Config,
    /// Loading or creating the session.
    Session,
    /// Locating the project root and its cheap facts.
    ProjectDiscovery,
    /// Choosing the provider/model and resolving the credential.
    ProviderResolution,
    /// Building the prompt and context actually sent.
    ContextBuild,
    /// Serialising and issuing the request.
    Request,
    /// Request issued to first token received.
    TimeToFirstToken,
    /// First token to last token.
    Generation,
    /// One tool invocation, start to result.
    ToolExecution,
    /// Time spent waiting for a human decision. Separated out because it
    /// is not the system being slow, and must never be averaged into
    /// latency figures as though it were.
    ApprovalWait,
    /// Writing durable state.
    Persistence,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Startup => "startup",
            Stage::Config => "config",
            Stage::Session => "session",
            Stage::ProjectDiscovery => "project-discovery",
            Stage::ProviderResolution => "provider-resolution",
            Stage::ContextBuild => "context-build",
            Stage::Request => "request",
            Stage::TimeToFirstToken => "ttft",
            Stage::Generation => "generation",
            Stage::ToolExecution => "tool-execution",
            Stage::ApprovalWait => "approval-wait",
            Stage::Persistence => "persistence",
        }
    }

    /// Whether this stage measures waiting on something outside SE.
    /// Local latency work should be judged on the stages where this is
    /// false; mixing them is how "the app is slow" becomes unactionable.
    pub fn is_external_wait(self) -> bool {
        matches!(
            self,
            Stage::TimeToFirstToken | Stage::Generation | Stage::ApprovalWait
        )
    }
}

/// One completed measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Span {
    pub stage: Stage,
    pub duration: Duration,
}

/// A running measurement. Dropping it without calling `finish` records
/// nothing, which is deliberate: a span that never completed is not a
/// duration of zero.
#[derive(Debug)]
pub struct Running {
    stage: Stage,
    started: Instant,
}

impl Running {
    pub fn start(stage: Stage) -> Self {
        Running {
            stage,
            started: Instant::now(),
        }
    }

    pub fn finish(self) -> Span {
        Span {
            stage: self.stage,
            duration: self.started.elapsed(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// Formats a duration at a resolution that is actually informative.
///
/// Local stages are routinely faster than a millisecond, and rounding
/// those to `0ms` discards exactly the measurement a latency budget is
/// built on. Sub-millisecond durations are therefore shown in
/// microseconds.
pub fn format_duration(d: Duration) -> String {
    if d < Duration::from_millis(1) {
        format!("{}µs", d.as_micros())
    } else if d < Duration::from_secs(10) {
        format!("{}ms", d.as_millis())
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

/// Collected spans for one turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Timings {
    pub spans: Vec<Span>,
}

impl Timings {
    pub fn record(&mut self, span: Span) {
        self.spans.push(span);
    }

    pub fn total(&self) -> Duration {
        self.spans.iter().map(|s| s.duration).sum()
    }

    /// Time this turn spent inside SE rather than waiting on a model or
    /// a human. This is the number local optimisation work moves.
    pub fn local(&self) -> Duration {
        self.spans
            .iter()
            .filter(|s| !s.stage.is_external_wait())
            .map(|s| s.duration)
            .sum()
    }

    pub fn of(&self, stage: Stage) -> Option<Duration> {
        self.spans
            .iter()
            .find(|s| s.stage == stage)
            .map(|s| s.duration)
    }

    /// A single diagnostic line, longest stage first. Shown by the
    /// interface in diagnostic mode, which the Phase 1 gate requires.
    pub fn summary(&self) -> String {
        let mut ordered: Vec<&Span> = self.spans.iter().collect();
        ordered.sort_by_key(|s| std::cmp::Reverse(s.duration));
        let parts: Vec<String> = ordered
            .iter()
            .map(|s| format!("{} {}", s.stage.label(), format_duration(s.duration)))
            .collect();
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_time_excludes_model_and_human_waiting() {
        let mut t = Timings::default();
        t.record(Span {
            stage: Stage::ContextBuild,
            duration: Duration::from_millis(5),
        });
        t.record(Span {
            stage: Stage::TimeToFirstToken,
            duration: Duration::from_millis(900),
        });
        t.record(Span {
            stage: Stage::ApprovalWait,
            duration: Duration::from_secs(30),
        });
        t.record(Span {
            stage: Stage::Persistence,
            duration: Duration::from_millis(2),
        });

        assert_eq!(t.local(), Duration::from_millis(7));
        // 5 + 900 + 30000 + 2
        assert_eq!(t.total(), Duration::from_millis(30907));
    }

    #[test]
    fn a_span_that_never_finished_is_not_recorded_as_zero() {
        let mut t = Timings::default();
        {
            // Started and then abandoned without `finish`: a span that
            // never completed must leave no measurement behind, rather
            // than appearing as a suspiciously fast zero.
            let _abandoned = Running::start(Stage::Request);
        }
        assert!(t.spans.is_empty());
        assert_eq!(t.of(Stage::Request), None);
        t.record(Running::start(Stage::Request).finish());
        assert!(t.of(Stage::Request).is_some());
    }

    #[test]
    fn the_summary_puts_the_slowest_stage_first_so_it_is_actionable() {
        let mut t = Timings::default();
        t.record(Span {
            stage: Stage::Config,
            duration: Duration::from_millis(1),
        });
        t.record(Span {
            stage: Stage::Generation,
            duration: Duration::from_millis(500),
        });
        let summary = t.summary();
        assert!(
            summary.starts_with("generation 500ms"),
            "slowest stage must lead: {summary}"
        );
        assert!(summary.contains("config 1ms"));
    }

    #[test]
    fn sub_millisecond_work_is_reported_in_microseconds_not_rounded_to_zero() {
        // The whole point of a local latency budget is visibility at
        // this scale; "0ms" would be worse than no number at all.
        assert_eq!(format_duration(Duration::from_micros(250)), "250µs");
        assert_eq!(format_duration(Duration::from_micros(0)), "0µs");
        assert_eq!(format_duration(Duration::from_millis(7)), "7ms");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1500ms");
        assert_eq!(format_duration(Duration::from_secs(12)), "12.0s");
    }

    #[test]
    fn every_stage_has_a_distinct_stable_label() {
        use std::collections::BTreeSet;
        let all = [
            Stage::Startup,
            Stage::Config,
            Stage::Session,
            Stage::ProjectDiscovery,
            Stage::ProviderResolution,
            Stage::ContextBuild,
            Stage::Request,
            Stage::TimeToFirstToken,
            Stage::Generation,
            Stage::ToolExecution,
            Stage::ApprovalWait,
            Stage::Persistence,
        ];
        let labels: BTreeSet<&str> = all.iter().map(|s| s.label()).collect();
        assert_eq!(labels.len(), all.len(), "labels must be unique");
        assert!(all.iter().all(|s| !s.label().is_empty()));
    }
}
