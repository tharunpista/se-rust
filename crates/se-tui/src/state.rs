//! The interface's own state, derived entirely from events.
//!
//! The rule that makes this crate safe: `AppState` is updated only by
//! [`AppState::apply`] (from a [`CoreEvent`]) and by local input
//! handling. It never asks the core a question and waits for an answer,
//! so there is no code path here that can block (I1), and no second
//! source of truth that could disagree with the transcript (I2).

use se_protocol::{CoreEvent, DeltaKind, Outcome, Payload, TaskId, Timings, Usage, UsageSource};

/// One entry in the visible transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    User(String),
    /// Assistant text. Grows in place as deltas arrive, which is what
    /// makes streaming look like streaming.
    Assistant(String),
    Reasoning(String),
    /// Who is answering: `agent · provider/model`, shown above a reply.
    Speaker(String),
    Tool {
        name: String,
        summary: String,
        state: ToolState,
    },
    /// Shown while the core is blocked on the user.
    Approval {
        name: String,
        summary: String,
        decision: Option<bool>,
    },
    Notice(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Failed,
    Denied,
}

/// What the interface is currently able to do. Derived from events, so
/// the composer cannot think a turn is over while the core disagrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Idle,
    /// A task is running; `Esc`/`Ctrl+C` interrupts it.
    Working {
        task: TaskId,
    },
    /// A task is blocked on the user. The composer is not the focus
    /// here: `y`/`n` answer.
    AwaitingApproval {
        task: TaskId,
        call: se_protocol::ToolCallId,
    },
}

/// What a Ctrl+C press resolved to, given application state and timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtrlC {
    /// Interrupt the running task (first press while working).
    Interrupt(TaskId),
    /// Exit (a second press inside the window, or a press while idle
    /// after the first was armed).
    Quit,
    /// First press while idle: armed, waiting for a confirming second.
    Armed,
}

/// How long a second Ctrl+C still counts as confirming the first.
const CTRL_C_WINDOW: std::time::Duration = std::time::Duration::from_millis(1500);

/// How long an approval prompt must have been on screen before `y`/`n`
/// answer it, so a keystroke already in flight when it appeared cannot.
pub const APPROVAL_ARM_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// The two fields `/connect` can turn the composer into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectStep {
    /// Visible field for the provider's API address; the hidden key field follows.
    Address { id: String, name: String },
    /// Hidden field for the key, saved with the address given (if any).
    Key {
        id: String,
        name: String,
        url: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub transcript: Vec<Entry>,
    pub composer: crate::composer::Composer,
    /// The `/` command picker. Composer state, not a separate dialog:
    /// it holds only a trigger offset and a highlight, and the
    /// candidate rows are recomputed from `commands` each frame.
    pub picker: crate::picker::Picker,
    /// The single command metadata source both the picker and `/help`
    /// read, so the two cannot disagree about what exists.
    pub commands: se_command::Registry,
    /// Parsed template bodies for loaded (non-builtin) prompt-template
    /// commands, keyed by `CommandSpec::id`. Populated once at
    /// discovery time rather than re-read from disk on every
    /// invocation: the file was already parsed to build the registry
    /// entry, and discarding that parse just to redo it on first use
    /// would be pure waste, not stricter just-in-time loading.
    pub templates: std::collections::HashMap<String, se_command::markdown::CommandDef>,
    /// What this terminal has been observed to deliver. Fills in as the
    /// user types, so the help bar advertises a newline chord that
    /// actually arrives rather than one that ought to.
    pub keys: crate::keys::Probe,
    pub activity: Activity,
    pub status: String,
    pub should_quit: bool,
    /// A persistent warning shown above the transcript, e.g. that no
    /// model is connected and replies are a local echo rather than AI.
    pub banner: Option<String>,
    /// The model chosen with `/models <provider>/<model>` for this
    /// session; `None` means the runner's configured default.
    pub model_override: Option<se_protocol::ModelRef>,
    /// Lines scrolled back from the newest. 0 pins to the latest.
    pub scroll: u16,
    /// Total wrapped rows the transcript last occupied, and the viewport
    /// height it was drawn in. Recorded by the renderer so the scroll
    /// indicator and max-scroll clamp use real measurements, not logical
    /// line counts.
    pub last_total_rows: usize,
    pub last_viewport_rows: usize,
    pub usage: Usage,
    /// Token totals across this session's finished tasks (`/usage`).
    pub session_usage: Usage,
    /// Whether the current task has reported usage yet, so a task that
    /// reported none is not counted twice.
    task_usage_seen: bool,
    /// Models the providers reported, listed off the interface thread.
    pub model_listing: crate::services::Listing,
    /// `/models` was asked for while the listing was still running.
    pub open_models_when_ready: bool,
    /// `/providers` and `/doctor`, as the binary reports them.
    pub providers_report: Option<crate::services::Report>,
    pub doctor_report: Option<String>,
    /// Saves the `/models` pick for the next launch.
    pub remember_model: crate::services::Hook<crate::services::ModelMemory>,
    pub list_providers: crate::services::Hook<crate::services::ProviderLister>,
    pub save_key: crate::services::Hook<crate::services::KeySaver>,
    pub copy: crate::services::Hook<crate::services::Clipboard>,
    /// Whether the terminal reports the mouse to SE. Off by default so the
    /// terminal's own drag-select works; on gives wheel scrolling (F2, /mouse).
    pub mouse_capture: bool,
    /// Manual, auto or plan; the core holds the copy that decides.
    pub mode: se_protocol::WorkMode,
    /// `/resume`, `/new`: see [`crate::services::Services`].
    pub open_session: crate::services::Hook<crate::services::SessionOpener>,
    pub session_id: Option<String>,
    pub sessions_dir: Option<std::path::PathBuf>,
    pub recent_projects: Option<std::path::PathBuf>,
    /// The `/resume` picker, while open.
    pub sessions: Option<crate::sessions::Overlay>,
    /// Deletes made this run, newest last, for `u` in the picker.
    pub trashed: Vec<(std::path::PathBuf, se_core::sessions::Trashed)>,
    /// A conversation switch asked for (`Some(None)`: a new one); the loop
    /// performs it with [`crate::switch_session`].
    pub switch_to: Option<Option<String>>,
    /// Results from background work (provider list, key validation).
    pub inbox: crate::services::Inbox,
    /// The provider list as last reported, for `/connect <id>`.
    pub providers: Vec<crate::services::ProviderEntry>,
    /// `/connect <id>` asked for before the provider list arrived.
    pub connect_pending: Option<String>,
    /// While set, the composer belongs to `/connect`, not the chat: its text is never
    /// submitted to the model and never added to the transcript.
    pub connect: Option<ConnectStep>,
    /// The model SE started with; `model_override` wins over it.
    pub default_model: Option<se_protocol::ModelRef>,
    /// The specialist that answered last, for the header.
    pub agent: Option<String>,
    /// `provider/model` of the turn now running or last run.
    pub turn_model: Option<String>,
    /// The working folder, shortened for the header.
    pub folder: String,
    /// Timings of the most recently finished task, shown in diagnostic
    /// mode (the Phase 1 gate requires stage timings to be visible).
    pub last_timings: Option<Timings>,
    pub show_diagnostics: bool,
    /// Events the interface was told about but could not render in
    /// order. Surfaced rather than hidden, so a gap is visible (I6).
    pub dropped_events: u64,
    next_expected_seq: Option<u64>,
    /// When Ctrl+C was last pressed, for the two-press exit.
    last_ctrl_c: Option<std::time::Instant>,
    /// When the pending approval prompt appeared, for [`APPROVAL_ARM_DELAY`].
    pub approval_shown_at: Option<std::time::Instant>,
    /// Deployed agents: the strip below the composer and its focus.
    pub agents: crate::agents::Strip,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            transcript: Vec::new(),
            composer: crate::composer::Composer::new(),
            picker: crate::picker::Picker::default(),
            commands: se_command::builtins::registry_with_builtins(),
            templates: std::collections::HashMap::new(),
            keys: crate::keys::Probe::new(),
            activity: Activity::Idle,
            status: String::new(),
            should_quit: false,
            banner: None,
            model_override: None,
            scroll: 0,
            last_total_rows: 0,
            last_viewport_rows: 0,
            usage: Usage::default(),
            session_usage: Usage::default(),
            task_usage_seen: false,
            model_listing: crate::services::Listing::default(),
            open_models_when_ready: false,
            providers_report: None,
            doctor_report: None,
            remember_model: Default::default(),
            list_providers: Default::default(),
            save_key: Default::default(),
            copy: Default::default(),
            mouse_capture: false,
            mode: Default::default(),
            open_session: Default::default(),
            session_id: None,
            sessions_dir: None,
            recent_projects: None,
            sessions: None,
            trashed: Vec::new(),
            switch_to: None,
            inbox: Default::default(),
            providers: Vec::new(),
            connect_pending: None,
            connect: None,
            default_model: None,
            agent: None,
            turn_model: None,
            folder: String::new(),
            last_timings: None,
            show_diagnostics: false,
            dropped_events: 0,
            next_expected_seq: None,
            last_ctrl_c: None,
            approval_shown_at: None,
            agents: Default::default(),
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `y`/`n` may answer the pending approval at `now`: one is
    /// pending, the composer holds no draft, and it has been visible for
    /// at least [`APPROVAL_ARM_DELAY`].
    pub fn approval_answerable(&self, now: std::time::Instant) -> bool {
        self.awaiting_approval().is_some()
            && self.composer.is_empty()
            && self
                .approval_shown_at
                .is_some_and(|t| now.saturating_duration_since(t) >= APPROVAL_ARM_DELAY)
    }

    /// Forgets the open conversation's transcript and turn state, for a
    /// switch to another core (whose event sequence starts over). Keeps the
    /// model pick, mouse mode, commands and the composer draft.
    pub fn reset_conversation(&mut self) {
        self.transcript.clear();
        self.activity = Activity::Idle;
        self.status.clear();
        self.scroll = 0;
        self.usage = Usage::default();
        self.session_usage = Usage::default();
        self.task_usage_seen = false;
        self.turn_model = None;
        self.agent = None;
        self.last_timings = None;
        self.dropped_events = 0;
        self.next_expected_seq = None;
        self.approval_shown_at = None;
        self.picker.close();
        // The agents outlive the conversation (a team run keeps going); a view does not.
        self.agents.focus = crate::agents::Focus::Composer;
    }

    pub fn is_busy(&self) -> bool {
        !matches!(self.activity, Activity::Idle)
    }

    pub fn awaiting_approval(&self) -> Option<se_protocol::ToolCallId> {
        match self.activity {
            Activity::AwaitingApproval { call, .. } => Some(call),
            _ => None,
        }
    }

    pub fn running_task(&self) -> Option<TaskId> {
        match self.activity {
            Activity::Working { task } | Activity::AwaitingApproval { task, .. } => Some(task),
            Activity::Idle => None,
        }
    }

    /// The stored text of the `n`th-from-last answer (1 = the last), as the
    /// model sent it: no wrapping, borders or colour.
    pub fn nth_last_answer(&self, n: usize) -> Option<&str> {
        let i = n.checked_sub(1)?;
        self.transcript
            .iter()
            .rev()
            .filter_map(|e| match e {
                Entry::Assistant(t) => Some(t.as_str()),
                _ => None,
            })
            .nth(i)
    }

    pub fn push(&mut self, entry: Entry) {
        self.transcript.push(entry);
        // Deliberately does NOT force the view to the bottom. When the
        // user is at the bottom (scroll == 0) the offset is computed
        // from the end, so new content stays in view automatically; when
        // they have scrolled up to read, their position is left alone,
        // which is what "do not jump to the bottom on every render while
        // I am reading older content" asks for.
    }

    /// Resolves a Ctrl+C press against state and timing.
    ///
    /// An explicit two-press machine (the directive): while working, the
    /// first press interrupts; while idle, the first press only arms,
    /// and a second press inside the window exits. This is what stops a
    /// reflexive Ctrl+C from killing the session, while keeping a
    /// deliberate double-press as a fast exit.
    pub fn ctrl_c(&mut self, now: std::time::Instant) -> CtrlC {
        let armed = self
            .last_ctrl_c
            .is_some_and(|t| now.duration_since(t) < CTRL_C_WINDOW);
        self.last_ctrl_c = Some(now);

        if armed {
            return CtrlC::Quit;
        }
        match self.running_task() {
            Some(task) => {
                self.status = "interrupting… (Ctrl+C again to exit)".to_string();
                CtrlC::Interrupt(task)
            }
            None => {
                self.status = "press Ctrl+C again to exit".to_string();
                CtrlC::Armed
            }
        }
    }

    /// How many wrapped rows exist below the current viewport, from the
    /// last render. Zero means the newest line is visible.
    pub fn rows_below(&self) -> usize {
        self.scroll as usize
    }

    /// Clamps `scroll` so it can never go past the oldest content. Uses
    /// the real measurements the renderer recorded.
    /// The commands the picker should currently show, recomputed from
    /// the registry and the live query every frame.
    ///
    /// Deliberately not cached: a stored candidate list is how "what
    /// is highlighted" and "what Enter runs" drift apart.
    pub fn picker_candidates(&self) -> Vec<&se_command::CommandSpec> {
        let query = match self
            .picker
            .query(self.composer.text(), self.composer.cursor())
        {
            Some(q) => q,
            None => return Vec::new(),
        };
        let is_specialist =
            |c: &&se_command::CommandSpec| c.handler == se_command::HandlerKind::SpecialistInvoke;
        let is_model =
            |c: &&se_command::CommandSpec| c.handler == se_command::HandlerKind::ModelSelect;
        let is_provider =
            |c: &&se_command::CommandSpec| c.handler == se_command::HandlerKind::ProviderConnect;
        match self.picker.scope() {
            // `/agents` is the one gateway into the specialist
            // catalogue (mirrors upstream OpenCode's single
            // `agent.list` command opening a dedicated dialog, rather
            // than one slash entry per agent) -- the default, mixed
            // list never shows one of the 194 real specialists, so
            // `/` alone stays a short, memorable list of actual
            // commands.
            crate::picker::PickerScope::Commands => self
                .commands
                .query(query)
                .into_iter()
                .filter(|c| !is_specialist(c) && !is_model(c) && !is_provider(c))
                .collect(),
            crate::picker::PickerScope::Agents => self
                .commands
                .query(query)
                .into_iter()
                .filter(is_specialist)
                .collect(),
            crate::picker::PickerScope::Models => self
                .commands
                .query(query)
                .into_iter()
                .filter(is_model)
                .collect(),
            crate::picker::PickerScope::Providers => self
                .commands
                .query(query)
                .into_iter()
                .filter(is_provider)
                .collect(),
        }
    }

    /// The canonical path of the highlighted candidate, if any.
    pub fn highlighted_command_path(&self) -> Option<String> {
        let candidates = self.picker_candidates();
        if candidates.is_empty() {
            return None;
        }
        // The stored index is clamped on read rather than on write,
        // because the list shrinks as the user types and a stored
        // index would otherwise point past the end for one frame.
        let idx = self.picker.selected().min(candidates.len() - 1);
        Some(candidates[idx].path.clone())
    }

    /// Replaces the partially-typed command word with `path`, leaving
    /// the leading `/` in place and the cursor after the inserted text.
    pub fn replace_command_word(&mut self, path: &str) {
        let Some(trigger) = self.picker.trigger() else {
            return;
        };
        let cursor = self.composer.cursor();
        let start = trigger + 1;
        if cursor < start || cursor > self.composer.text().len() {
            return;
        }
        let mut text = self.composer.text().to_string();
        text.replace_range(start..cursor, path);
        let new_cursor = start + path.len();
        self.composer.set(text);
        self.composer.set_cursor(new_cursor);
    }

    pub fn clamp_scroll(&mut self) {
        let max = self.last_total_rows.saturating_sub(self.last_viewport_rows);
        if (self.scroll as usize) > max {
            self.scroll = max.min(u16::MAX as usize) as u16;
        }
    }

    /// Folds one core event into the view.
    pub fn apply(&mut self, event: &CoreEvent) {
        // Sequence checking: a gap means the core produced something
        // this interface never saw. Counted and shown, never ignored.
        match self.next_expected_seq {
            Some(expected) if event.seq > expected => {
                self.dropped_events += event.seq - expected;
            }
            _ => {}
        }
        self.next_expected_seq = Some(event.seq + 1);

        match &event.payload {
            Payload::TaskStarted { request } => {
                self.task_usage_seen = false;
                self.push(Entry::User(request.clone()));
                self.activity = Activity::Working { task: event.task };
                // A new exchange the user just started is the one thing
                // that does snap to the bottom: they want to see their
                // message and the reply to it.
                self.scroll = 0;
            }
            Payload::TurnStarted { provider, model } => {
                self.status = format!("{provider} · {model}");
                self.turn_model = Some(format!("{provider}/{model}"));
                // A pre-turn approval (a specialist's `use_provider`) has
                // no tool lifecycle after it; the turn starting is the
                // proof it was answered.
                if let Activity::AwaitingApproval { task, .. } = self.activity {
                    self.activity = Activity::Working { task };
                }
            }
            // The invocation-provenance record (Phase 3 item 4): which
            // agent, and the content hash of the exact definition that
            // ran. Durable via the session log already recording every
            // `Payload` verbatim; a richer panel showing this history
            // is later TUI work, not this one line.
            Payload::SpecialistInvoked { agent_id, .. } => {
                // Enriched from the already-registered `CommandSpec`
                // (department + mission), not a new protocol field --
                // this is a synchronous, local lookup, never a second
                // round trip to the core.
                // One short line per answer naming the agent and model;
                // the agent's mission is in `/agents`, not on every turn.
                self.agent = Some(agent_id.clone());
                let who = match &self.turn_model {
                    Some(m) => format!("{agent_id} \u{b7} {m}"),
                    None => agent_id.clone(),
                };
                self.push(Entry::Speaker(who));
            }
            Payload::Delta { kind, text } => match kind {
                DeltaKind::Text => self.append_assistant(text),
                DeltaKind::Reasoning => self.append_reasoning(text),
            },
            Payload::ToolRequested { name, summary, .. } => {
                self.push(Entry::Tool {
                    name: name.clone(),
                    summary: summary.clone(),
                    state: ToolState::Running,
                });
            }
            Payload::ApprovalRequired {
                call,
                name,
                summary,
            } => {
                self.push(Entry::Approval {
                    name: name.clone(),
                    summary: summary.clone(),
                    decision: None,
                });
                self.activity = Activity::AwaitingApproval {
                    task: event.task,
                    call: *call,
                };
                self.approval_shown_at = Some(std::time::Instant::now());
                // The prompt must be on screen: nothing may cover it or
                // leave it scrolled out of view.
                self.picker.close();
                // Else a `y` meant for a delete confirm could allow the tool.
                self.sessions = None;
                // An agent view would cover the prompt.
                self.agents.focus = crate::agents::Focus::Composer;
                self.scroll = 0;
            }
            Payload::ToolCompleted {
                name, ok, preview, ..
            } => {
                self.settle_tool(
                    name,
                    if *ok {
                        ToolState::Ok
                    } else {
                        ToolState::Failed
                    },
                    preview,
                );
                // Returning from approval resumes ordinary work.
                if let Activity::AwaitingApproval { task, .. } = self.activity {
                    self.activity = Activity::Working { task };
                }
            }
            Payload::ToolDenied { name, .. } => {
                self.mark_approval_decided(false);
                self.settle_tool(name, ToolState::Denied, "not run");
                if let Activity::AwaitingApproval { task, .. } = self.activity {
                    self.activity = Activity::Working { task };
                }
            }
            Payload::UsageUpdated(usage) => {
                self.usage = *usage;
                self.task_usage_seen = true;
            }
            Payload::TurnCompleted { .. } => {}
            // C2's child agents arrive here; team runs feed the same fold.
            Payload::Agent(e) => {
                // A sub-agent's prompt is shown as a main prompt; its own
                // (wrapped) next step for that call is the answer to it.
                if let se_protocol::AgentEvent::Activity { payload, .. } = e {
                    let answered = match payload.as_ref() {
                        Payload::ToolRequested { call, .. } | Payload::ToolCompleted { call, .. } => {
                            Some((*call, true))
                        }
                        Payload::ToolDenied { call, .. } => Some((*call, false)),
                        _ => None,
                    };
                    if let (Some((call, allow)), Activity::AwaitingApproval { task, call: waiting }) =
                        (answered, self.activity)
                    {
                        if call == waiting {
                            self.mark_approval_decided(allow);
                            self.activity = Activity::Working { task };
                        }
                    }
                }
                self.agents.apply(e)
            }
            Payload::TaskFinished { outcome, timings } => {
                if std::mem::take(&mut self.task_usage_seen) {
                    let add = |a: Option<u64>, b: Option<u64>| match (a, b) {
                        (None, None) => None,
                        _ => Some(a.unwrap_or(0) + b.unwrap_or(0)),
                    };
                    let (s, u) = (self.session_usage, self.usage);
                    self.session_usage = Usage {
                        input_tokens: add(s.input_tokens, u.input_tokens),
                        output_tokens: add(s.output_tokens, u.output_tokens),
                        cached_input_tokens: add(s.cached_input_tokens, u.cached_input_tokens),
                        source: u.source,
                    };
                }
                self.activity = Activity::Idle;
                self.last_timings = Some(timings.clone());
                match outcome {
                    // The streamed text is already in the transcript;
                    // repeating the final text would show it twice.
                    Outcome::Completed { final_text } => {
                        if !self
                            .transcript
                            .iter()
                            .rev()
                            .any(|e| matches!(e, Entry::Assistant(_)))
                            && !final_text.is_empty()
                        {
                            self.push(Entry::Assistant(final_text.clone()));
                        }
                        self.status = format!("done · {}", timings.summary());
                    }
                    Outcome::Cancelled => {
                        self.push(Entry::Notice("interrupted".to_string()));
                        self.status = "interrupted".to_string();
                    }
                    Outcome::Incomplete {
                        partial_text,
                        reason,
                    } => {
                        // The partial answer is shown as what it is --
                        // not silently completed, not thrown away.
                        if !self
                            .transcript
                            .iter()
                            .rev()
                            .any(|e| matches!(e, Entry::Assistant(_)))
                            && !partial_text.is_empty()
                        {
                            self.push(Entry::Assistant(partial_text.clone()));
                        }
                        self.push(Entry::Notice(format!("incomplete: {reason}")));
                        self.status = "incomplete".to_string();
                    }
                    Outcome::ExhaustedTurns { turns } => {
                        self.push(Entry::Notice(format!(
                            "stopped after {turns} turn(s) without a final answer"
                        )));
                        self.status = "exhausted".to_string();
                    }
                    Outcome::Failed { detail } => {
                        // The provider marked a model unusable; the next /models must not offer it.
                        if detail.contains("is now hidden from /models") {
                            self.model_listing.reset();
                        }
                        self.push(Entry::Error(detail.clone()));
                        self.status = "failed".to_string();
                    }
                }
            }
        }
    }

    fn append_assistant(&mut self, text: &str) {
        // No scroll reset: streaming into the newest entry stays pinned
        // when the user is at the bottom and leaves their position alone
        // when they have scrolled up to read.
        if let Some(Entry::Assistant(existing)) = self.transcript.last_mut() {
            existing.push_str(text);
            return;
        }
        self.push(Entry::Assistant(text.to_string()));
    }

    fn append_reasoning(&mut self, text: &str) {
        if let Some(Entry::Reasoning(existing)) = self.transcript.last_mut() {
            existing.push_str(text);
            return;
        }
        self.push(Entry::Reasoning(text.to_string()));
    }

    fn settle_tool(&mut self, name: &str, new_state: ToolState, preview: &str) {
        settle_tool(&mut self.transcript, name, new_state, preview);
    }

    fn mark_approval_decided(&mut self, allow: bool) {
        mark_approval_decided(&mut self.transcript, allow);
    }

    /// The assistant text accumulated so far, for tests and for copying.
    pub fn assistant_text(&self) -> String {
        self.transcript
            .iter()
            .filter_map(|e| match e {
                Entry::Assistant(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Settles the newest running `name` tool entry; shared with agent transcripts.
pub(crate) fn settle_tool(transcript: &mut Vec<Entry>, name: &str, new_state: ToolState, preview: &str) {
    for entry in transcript.iter_mut().rev() {
        if let Entry::Tool {
            name: existing,
            state,
            summary,
        } = entry
        {
            if existing == name && *state == ToolState::Running {
                *state = new_state;
                if !preview.is_empty() {
                    *summary = preview.to_string();
                }
                return;
            }
        }
    }
    // No matching request: record it anyway rather than dropping the
    // fact that a tool ran (I7).
    transcript.push(Entry::Tool {
        name: name.to_string(),
        summary: preview.to_string(),
        state: new_state,
    });
}

pub(crate) fn mark_approval_decided(transcript: &mut [Entry], allow: bool) {
    for entry in transcript.iter_mut().rev() {
        if let Entry::Approval { decision, .. } = entry {
            if decision.is_none() {
                *decision = Some(allow);
                return;
            }
        }
    }
}

impl AppState {
    /// One line describing usage, honest about where it came from.
    /// The model the next turn will use, as `provider/model`.
    pub fn current_model(&self) -> Option<String> {
        self.model_override
            .as_ref()
            .or(self.default_model.as_ref())
            .map(|m| format!("{}/{}", m.provider, m.model))
    }

    pub fn usage_line(&self) -> String {
        let render = |v: Option<u64>| match v {
            Some(n) => n.to_string(),
            None => "not reported".to_string(),
        };
        match self.usage.source {
            UsageSource::Unavailable => "tokens not reported".to_string(),
            source => format!(
                "in {} · out {} · {}",
                render(self.usage.input_tokens),
                render(self.usage.output_tokens),
                source.label()
            ),
        }
    }
}

#[cfg(test)]
mod ctrl_c_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn idle_first_press_only_arms_it_does_not_quit() {
        let mut s = AppState::new();
        assert_eq!(s.ctrl_c(Instant::now()), CtrlC::Armed);
        assert!(!s.should_quit, "a single press while idle must not exit");
    }

    #[test]
    fn idle_second_press_inside_the_window_quits() {
        let mut s = AppState::new();
        let t0 = Instant::now();
        assert_eq!(s.ctrl_c(t0), CtrlC::Armed);
        assert_eq!(s.ctrl_c(t0 + Duration::from_millis(200)), CtrlC::Quit);
    }

    #[test]
    fn a_second_press_after_the_window_re_arms_instead_of_quitting() {
        let mut s = AppState::new();
        let t0 = Instant::now();
        assert_eq!(s.ctrl_c(t0), CtrlC::Armed);
        assert_eq!(s.ctrl_c(t0 + Duration::from_millis(2000)), CtrlC::Armed);
    }

    #[test]
    fn working_first_press_interrupts_the_running_task_rather_than_arming() {
        let mut s = AppState::new();
        let task = TaskId::from_raw(7);
        s.activity = Activity::Working { task };
        assert_eq!(s.ctrl_c(Instant::now()), CtrlC::Interrupt(task));
    }

    #[test]
    fn a_second_press_while_still_working_quits_rather_than_interrupting_again() {
        let mut s = AppState::new();
        let task = TaskId::from_raw(7);
        s.activity = Activity::Working { task };
        let t0 = Instant::now();
        assert_eq!(s.ctrl_c(t0), CtrlC::Interrupt(task));
        assert_eq!(s.ctrl_c(t0 + Duration::from_millis(100)), CtrlC::Quit);
    }

    #[test]
    fn mere_typing_between_two_ctrl_c_presses_does_not_reset_the_window() {
        // The directive: avoid accidental shutdown while editing, but a
        // deliberate double Ctrl+C must still exit even if the user kept
        // typing in between -- typing itself does not call `ctrl_c` at
        // all, so this is really asserting that nothing except time
        // resets the arming.
        let mut s = AppState::new();
        let t0 = Instant::now();
        assert_eq!(s.ctrl_c(t0), CtrlC::Armed);
        s.composer.insert_char('x');
        assert_eq!(s.ctrl_c(t0 + Duration::from_millis(500)), CtrlC::Quit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_protocol::{SessionId, ToolCallId, TurnId};

    fn ev(seq: u64, payload: Payload) -> CoreEvent {
        CoreEvent {
            seq,
            session: SessionId::from_raw(1),
            task: TaskId::from_raw(1),
            turn: Some(TurnId::from_raw(1)),
            payload,
        }
    }

    fn delta(seq: u64, text: &str) -> CoreEvent {
        ev(
            seq,
            Payload::Delta {
                kind: DeltaKind::Text,
                text: text.to_string(),
            },
        )
    }

    #[test]
    fn deltas_accumulate_into_one_growing_assistant_entry() {
        let mut app = AppState::new();
        app.apply(&delta(0, "Hel"));
        app.apply(&delta(1, "lo "));
        app.apply(&delta(2, "world"));
        assert_eq!(app.assistant_text(), "Hello world");
        // One entry, not three: this is what makes it read as streaming
        // rather than as three separate replies.
        assert_eq!(app.transcript.len(), 1);
    }

    /// What the user actually typed and what the task reports back are
    /// kept as two distinct entries that can never collapse into one.
    ///
    /// This is the regression for "SE appears to repeat the exact text I
    /// typed": that behaviour was never a rendering bug -- a `Delta`
    /// carrying the literal input text is exactly what the local echo
    /// runner's output looks like once it reaches this function. What
    /// *is* checked here is the one way a real bug could produce the
    /// same symptom: the user's own `TaskStarted` message must never be
    /// folded into, or mistaken for, the `Assistant` entry a later
    /// `Delta` starts.
    #[test]
    fn a_users_own_message_never_becomes_the_assistant_entry() {
        let mut app = AppState::new();
        app.apply(&ev(
            0,
            Payload::TaskStarted {
                request: "hello".into(),
            },
        ));
        app.apply(&delta(1, "hello"));

        assert_eq!(app.transcript.len(), 2, "{:?}", app.transcript);
        assert!(matches!(&app.transcript[0], Entry::User(t) if t == "hello"));
        assert!(matches!(&app.transcript[1], Entry::Assistant(t) if t == "hello"));
        // They must be genuinely separate entries, not the same String
        // appearing twice because one was copied from the other.
        assert_eq!(app.transcript.len(), 2);
    }

    #[test]
    fn a_task_moves_from_working_to_idle_exactly_on_the_terminal_event() {
        let mut app = AppState::new();
        assert_eq!(app.activity, Activity::Idle);
        app.apply(&ev(
            0,
            Payload::TaskStarted {
                request: "hi".into(),
            },
        ));
        assert!(app.is_busy());
        app.apply(&delta(1, "hello"));
        assert!(app.is_busy(), "a delta must not end the task");
        app.apply(&ev(
            2,
            Payload::TaskFinished {
                outcome: Outcome::Completed {
                    final_text: "hello".into(),
                },
                timings: Timings::default(),
            },
        ));
        assert_eq!(app.activity, Activity::Idle);
    }

    #[test]
    fn the_final_text_is_not_duplicated_after_streaming_it() {
        let mut app = AppState::new();
        app.apply(&delta(0, "the answer"));
        app.apply(&ev(
            1,
            Payload::TaskFinished {
                outcome: Outcome::Completed {
                    final_text: "the answer".into(),
                },
                timings: Timings::default(),
            },
        ));
        assert_eq!(app.assistant_text(), "the answer");
    }

    #[test]
    fn a_non_streaming_completion_still_shows_its_answer() {
        let mut app = AppState::new();
        app.apply(&ev(
            0,
            Payload::TaskFinished {
                outcome: Outcome::Completed {
                    final_text: "no deltas were sent".into(),
                },
                timings: Timings::default(),
            },
        ));
        assert_eq!(app.assistant_text(), "no deltas were sent");
    }

    #[test]
    fn approval_blocks_the_view_on_a_decision_and_records_the_answer() {
        let mut app = AppState::new();
        app.apply(&ev(
            0,
            Payload::TaskStarted {
                request: "delete everything".into(),
            },
        ));
        app.apply(&ev(
            1,
            Payload::ApprovalRequired {
                call: ToolCallId::from_raw(5),
                name: "run_shell".into(),
                summary: "rm -rf /".into(),
            },
        ));
        assert_eq!(app.awaiting_approval(), Some(ToolCallId::from_raw(5)));

        app.apply(&ev(
            2,
            Payload::ToolDenied {
                call: ToolCallId::from_raw(5),
                name: "run_shell".into(),
            },
        ));
        assert_eq!(app.awaiting_approval(), None);
        assert!(app.is_busy(), "denying a tool does not end the task");
        let decided = app.transcript.iter().any(|e| {
            matches!(
                e,
                Entry::Approval {
                    decision: Some(false),
                    ..
                }
            )
        });
        assert!(decided, "the decision must be visible in the transcript");
    }

    /// C2: a sub-agent's prompt is a main prompt; the agent's own wrapped
    /// next step for that call (run or denied) settles it.
    #[test]
    fn a_sub_agents_answered_prompt_clears_on_its_wrapped_next_step() {
        use se_protocol::AgentEvent;
        let wrap = |payload| {
            Payload::Agent(AgentEvent::Activity {
                agent_id: "a1".into(),
                payload: Box::new(payload),
            })
        };
        for (allow, next) in [
            (
                true,
                Payload::ToolRequested {
                    call: ToolCallId::from_raw(7),
                    name: "read".into(),
                    summary: "note.txt".into(),
                },
            ),
            (
                false,
                Payload::ToolDenied {
                    call: ToolCallId::from_raw(7),
                    name: "read".into(),
                },
            ),
        ] {
            let mut app = AppState::new();
            app.apply(&ev(0, Payload::TaskStarted { request: "x".into() }));
            app.apply(&ev(
                1,
                Payload::Agent(AgentEvent::Spawned {
                    agent_id: "a1".into(),
                    role: "reader".into(),
                    parent: Some("main".into()),
                    task: "t".into(),
                }),
            ));
            app.apply(&ev(
                2,
                Payload::ApprovalRequired {
                    call: ToolCallId::from_raw(7),
                    name: "read".into(),
                    summary: "[reader #a1] wants to run read: note.txt".into(),
                },
            ));
            // Another call's step leaves the prompt up.
            app.apply(&ev(
                3,
                wrap(Payload::ToolCompleted {
                    call: ToolCallId::from_raw(8),
                    name: "read".into(),
                    ok: true,
                    preview: String::new(),
                }),
            ));
            assert_eq!(app.awaiting_approval(), Some(ToolCallId::from_raw(7)));
            app.apply(&ev(4, wrap(next)));
            assert_eq!(app.awaiting_approval(), None, "allow={allow}");
            assert!(app.is_busy());
            assert!(app.transcript.iter().any(|e| matches!(
                e,
                Entry::Approval { decision: Some(d), .. } if *d == allow
            )));
        }
    }

    #[test]
    fn an_arriving_approval_closes_the_picker_and_snaps_to_the_bottom() {
        let mut app = AppState::new();
        app.apply(&ev(0, Payload::TaskStarted { request: "x".into() }));
        app.composer.set("/".to_string());
        app.picker.open(0);
        app.scroll = 7;
        app.apply(&ev(
            1,
            Payload::ApprovalRequired {
                call: ToolCallId::from_raw(1),
                name: "run_shell".into(),
                summary: "ls".into(),
            },
        ));
        assert!(!app.picker.is_open());
        assert_eq!(app.scroll, 0);
        assert!(app.approval_shown_at.is_some());
    }

    #[test]
    fn a_tool_result_settles_the_entry_that_requested_it() {
        let mut app = AppState::new();
        app.apply(&ev(
            0,
            Payload::ToolRequested {
                call: ToolCallId::from_raw(1),
                name: "read_file".into(),
                summary: "src/main.rs".into(),
            },
        ));
        app.apply(&ev(
            1,
            Payload::ToolCompleted {
                call: ToolCallId::from_raw(1),
                name: "read_file".into(),
                ok: true,
                preview: "fn main() {}".into(),
            },
        ));
        assert_eq!(app.transcript.len(), 1, "it settles in place");
        assert!(matches!(
            app.transcript[0],
            Entry::Tool {
                state: ToolState::Ok,
                ..
            }
        ));
    }

    #[test]
    fn a_result_with_no_matching_request_is_still_recorded() {
        // Invariant I7: a tool that ran is a fact, even if the interface
        // missed the request event.
        let mut app = AppState::new();
        app.apply(&ev(
            0,
            Payload::ToolCompleted {
                call: ToolCallId::from_raw(9),
                name: "git_status".into(),
                ok: false,
                preview: "not a repository".into(),
            },
        ));
        assert!(matches!(
            app.transcript[0],
            Entry::Tool {
                state: ToolState::Failed,
                ..
            }
        ));
    }

    #[test]
    fn a_gap_in_the_sequence_is_counted_rather_than_hidden() {
        let mut app = AppState::new();
        app.apply(&delta(0, "a"));
        // seq 1 and 2 never arrive.
        app.apply(&delta(3, "b"));
        assert_eq!(app.dropped_events, 2);
    }

    #[test]
    fn cancellation_is_shown_as_an_interruption_not_an_error() {
        let mut app = AppState::new();
        app.apply(&ev(
            0,
            Payload::TaskFinished {
                outcome: Outcome::Cancelled,
                timings: Timings::default(),
            },
        ));
        assert!(app.transcript.iter().any(|e| matches!(e, Entry::Notice(_))));
        assert!(
            !app.transcript.iter().any(|e| matches!(e, Entry::Error(_))),
            "an interrupt the user asked for is not an error"
        );
    }

    #[test]
    fn unreported_usage_is_never_shown_as_zero() {
        let app = AppState::new();
        assert_eq!(app.usage_line(), "tokens not reported");
    }

    #[test]
    fn locally_measured_usage_says_so_on_screen() {
        let mut app = AppState::new();
        app.apply(&ev(
            0,
            Payload::UsageUpdated(Usage {
                input_tokens: Some(3),
                output_tokens: Some(9),
                cached_input_tokens: None,
                source: UsageSource::LocallyMeasured,
            }),
        ));
        let line = app.usage_line();
        assert!(line.contains("in 3"), "{line}");
        assert!(line.contains("out 9"), "{line}");
        assert!(
            line.contains("locally measured"),
            "an estimate must be labelled: {line}"
        );
    }

    #[test]
    fn finished_timings_are_kept_for_the_diagnostic_view() {
        let mut app = AppState::new();
        let mut timings = Timings::default();
        timings.record(se_protocol::Span {
            stage: se_protocol::Stage::Generation,
            duration: std::time::Duration::from_millis(42),
        });
        app.apply(&ev(
            0,
            Payload::TaskFinished {
                outcome: Outcome::Completed {
                    final_text: "x".into(),
                },
                timings,
            },
        ));
        let kept = app.last_timings.as_ref().expect("timings kept");
        assert_eq!(
            kept.of(se_protocol::Stage::Generation),
            Some(std::time::Duration::from_millis(42))
        );
    }

    /// A pre-turn provider approval has no tool lifecycle; once the turn
    /// starts the interface must stop showing an approval prompt.
    #[test]
    fn an_approved_provider_prompt_clears_when_the_turn_starts() {
        let mut app = AppState::new();
        app.apply(&ev(
            0,
            Payload::TaskStarted {
                request: "hi".into(),
            },
        ));
        app.apply(&ev(
            1,
            Payload::ApprovalRequired {
                call: ToolCallId::from_raw(9),
                name: "use_provider".into(),
                summary: "spec wants to use beta/m".into(),
            },
        ));
        assert!(app.awaiting_approval().is_some());
        app.apply(&ev(
            2,
            Payload::TurnStarted {
                provider: "beta".into(),
                model: "m".into(),
            },
        ));
        assert_eq!(app.awaiting_approval(), None);
        assert!(app.is_busy(), "the turn is now running");
    }
}
