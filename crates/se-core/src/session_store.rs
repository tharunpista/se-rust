//! Durable session history (Phase 2a).
//!
//! The event log *is* the checkpoint: every [`CoreEvent`] the core
//! already emits is appended to a file as it happens, one JSON object
//! per line. There is no separate checkpoint format and no migration
//! machinery, because there is exactly one schema version and
//! `CoreEvent` is the thing it serialises -- adding a second format
//! nothing writes yet would be exactly the speculative abstraction the
//! user has repeatedly rejected.
//!
//! What this deliberately does not do: undo/redo. A model turn is not
//! a reversible edit (its side effects, once tools exist, are not
//! either), so "undo" has no honest meaning yet. Charter §9 asks for
//! "undo/redo semantics" as part of the persistence layer; the honest
//! semantics right now are resume-only, and that is what is built.
//!
//! What it must never do, per charter §9's resume rule: automatically
//! replay a non-idempotent operation whose outcome is unknown. A task
//! whose log has a start but no terminal event did not finish --the
//! process died, or was killed -- and [`reconcile`] reports that
//! honestly as [`TaskState::Interrupted`], never as completed and never
//! by re-running it.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use se_protocol::{CoreEvent, Payload, TaskId};

/// An append-only log of one session's events.
///
/// Each `append` is followed by a flush: a session log exists to
/// survive the process dying, so buffering across the exact event that
/// might not come back defeats the point.
pub struct SessionStore {
    file: File,
}

impl SessionStore {
    /// Opens (creating if needed) the log at `path` for appending.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(SessionStore { file })
    }

    /// Appends one event, flushed before returning.
    pub fn append(&mut self, event: &CoreEvent) -> io::Result<()> {
        let line = serde_json::to_string(event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()
    }
}

/// The default location for a project's session logs: `.se/sessions`
/// under the given project root.
pub fn default_sessions_dir(project_root: &Path) -> PathBuf {
    project_root.join(".se").join("sessions")
}

/// One malformed line, reported rather than silently dropped -- the
/// same "skip one, don't fail all" rule `se-command`'s discovery module
/// applies to command files applies here to session lines: a single
/// corrupted line (a partial write from a crash mid-flush, say) must
/// not make the rest of a session unreadable.
#[derive(Debug, Clone)]
pub struct LoadError {
    pub line_number: usize,
    pub reason: String,
}

/// Reads every valid event from a session log, in the order they were
/// written. A missing file is empty history, not an error -- the
/// ordinary case for a session that was never interrupted and so was
/// never resumed, or simply does not exist yet.
pub fn load(path: &Path) -> (Vec<CoreEvent>, Vec<LoadError>) {
    let mut events = Vec::new();
    let mut errors = Vec::new();
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (events, errors),
        Err(e) => {
            errors.push(LoadError {
                line_number: 0,
                reason: e.to_string(),
            });
            return (events, errors);
        }
    };
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line_number = i + 1;
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                errors.push(LoadError {
                    line_number,
                    reason: e.to_string(),
                });
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<CoreEvent>(&line) {
            Ok(event) => events.push(event),
            Err(e) => errors.push(LoadError {
                line_number,
                reason: e.to_string(),
            }),
        }
    }
    (events, errors)
}

/// What a resumed task actually ended up as, read from the log alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    /// A terminal event exists: `outcome` is exactly what the core
    /// reported, never re-derived.
    Finished { outcome: se_protocol::Outcome },
    /// A task started but the log has no terminal event for it. This is
    /// the state a process death leaves behind, and it is reported as
    /// exactly that -- interrupted -- never silently upgraded to
    /// finished and never replayed.
    Interrupted,
}

/// Reconstructs each task's real final state from a session's events,
/// without re-running anything.
///
/// Tasks are returned in the order their first event appears, so a
/// resumed session's history reads the same way it was written.
pub fn reconcile(events: &[CoreEvent]) -> Vec<(TaskId, TaskState)> {
    let mut order: Vec<TaskId> = Vec::new();
    let mut state: std::collections::HashMap<TaskId, TaskState> = std::collections::HashMap::new();
    for event in events {
        if !state.contains_key(&event.task) {
            order.push(event.task);
        }
        if let Payload::TaskFinished { outcome, .. } = &event.payload {
            state.insert(
                event.task,
                TaskState::Finished {
                    outcome: outcome.clone(),
                },
            );
        } else {
            state.entry(event.task).or_insert(TaskState::Interrupted);
        }
    }
    order
        .into_iter()
        .map(|task| (task, state.remove(&task).unwrap_or(TaskState::Interrupted)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_protocol::{Outcome, SessionId, Timings};
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempFile(PathBuf);

    impl TempFile {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "se-session-store-test-{}-{n}.jsonl",
                std::process::id()
            ));
            TempFile(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn event(task: TaskId, payload: Payload) -> CoreEvent {
        CoreEvent {
            seq: 0,
            session: SessionId::from_raw(1),
            task,
            turn: None,
            payload,
        }
    }

    #[test]
    fn appended_events_are_read_back_in_order() {
        let f = TempFile::new();
        let mut store = SessionStore::open(&f.0).unwrap();
        let task = TaskId::from_raw(1);
        store
            .append(&event(
                task,
                Payload::TaskStarted {
                    request: "hi".into(),
                },
            ))
            .unwrap();
        store
            .append(&event(
                task,
                Payload::TaskFinished {
                    outcome: Outcome::Completed {
                        final_text: "done".into(),
                    },
                    timings: Timings::default(),
                },
            ))
            .unwrap();
        let (events, errors) = load(&f.0);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].payload, Payload::TaskStarted { .. }));
        assert!(matches!(events[1].payload, Payload::TaskFinished { .. }));
    }

    #[test]
    fn a_missing_log_is_empty_history_not_an_error() {
        let path = std::env::temp_dir().join("se-session-store-test-does-not-exist.jsonl");
        let (events, errors) = load(&path);
        assert!(events.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn a_task_with_no_terminal_event_reconciles_as_interrupted_not_completed() {
        // The Phase 2a gate: a process killed mid-turn must read
        // interrupted on restart, never completed.
        let task = TaskId::from_raw(1);
        let events = vec![event(
            task,
            Payload::TaskStarted {
                request: "hi".into(),
            },
        )];
        let result = reconcile(&events);
        assert_eq!(result, vec![(task, TaskState::Interrupted)]);
    }

    #[test]
    fn a_task_with_a_terminal_event_reconciles_to_its_real_outcome_not_a_guess() {
        let task = TaskId::from_raw(1);
        let outcome = Outcome::Completed {
            final_text: "done".into(),
        };
        let events = vec![
            event(
                task,
                Payload::TaskStarted {
                    request: "hi".into(),
                },
            ),
            event(
                task,
                Payload::TaskFinished {
                    outcome: outcome.clone(),
                    timings: Timings::default(),
                },
            ),
        ];
        let result = reconcile(&events);
        assert_eq!(result, vec![(task, TaskState::Finished { outcome })]);
    }

    #[test]
    fn a_failed_or_cancelled_terminal_outcome_is_preserved_exactly_not_reinterpreted() {
        let task = TaskId::from_raw(1);
        let outcome = Outcome::Cancelled;
        let events = vec![
            event(
                task,
                Payload::TaskStarted {
                    request: "hi".into(),
                },
            ),
            event(
                task,
                Payload::TaskFinished {
                    outcome: outcome.clone(),
                    timings: Timings::default(),
                },
            ),
        ];
        assert_eq!(
            reconcile(&events),
            vec![(task, TaskState::Finished { outcome })]
        );
    }

    #[test]
    fn multiple_tasks_reconcile_independently_in_first_seen_order() {
        let a = TaskId::from_raw(1);
        let b = TaskId::from_raw(2);
        let events = vec![
            event(
                a,
                Payload::TaskStarted {
                    request: "first".into(),
                },
            ),
            event(
                b,
                Payload::TaskStarted {
                    request: "second".into(),
                },
            ),
            event(
                a,
                Payload::TaskFinished {
                    outcome: Outcome::Completed {
                        final_text: "a done".into(),
                    },
                    timings: Timings::default(),
                },
            ),
            // b never finishes -- a crash between the two tasks.
        ];
        let result = reconcile(&events);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, a);
        assert!(matches!(result[0].1, TaskState::Finished { .. }));
        assert_eq!(result[1].0, b);
        assert_eq!(result[1].1, TaskState::Interrupted);
    }

    #[test]
    fn one_malformed_line_is_reported_and_does_not_hide_the_valid_ones() {
        let f = TempFile::new();
        {
            let mut store = SessionStore::open(&f.0).unwrap();
            store
                .append(&event(
                    TaskId::from_raw(1),
                    Payload::TaskStarted {
                        request: "first".into(),
                    },
                ))
                .unwrap();
        }
        // Simulate a partial write: append a line that is not valid
        // JSON, as a crash mid-flush might leave behind.
        {
            use std::io::Write as _;
            let mut raw = OpenOptions::new().append(true).open(&f.0).unwrap();
            raw.write_all(b"{not valid json\n").unwrap();
        }
        {
            let mut store = SessionStore::open(&f.0).unwrap();
            store
                .append(&event(
                    TaskId::from_raw(2),
                    Payload::TaskStarted {
                        request: "second".into(),
                    },
                ))
                .unwrap();
        }
        let (events, errors) = load(&f.0);
        assert_eq!(events.len(), 2, "the two valid lines must still load");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line_number, 2);
    }

    #[test]
    fn a_blank_line_is_skipped_without_being_reported_as_an_error() {
        let f = TempFile::new();
        std::fs::write(&f.0, "\n\n").unwrap();
        let (events, errors) = load(&f.0);
        assert!(events.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn default_sessions_dir_is_under_dot_se_in_the_project_root() {
        let dir = default_sessions_dir(Path::new("/some/project"));
        assert_eq!(dir, Path::new("/some/project/.se/sessions"));
    }
}
