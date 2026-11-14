//! Drawing. Derives everything from [`AppState`]; reads no other source.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::state::{Activity, AppState, Entry, ToolState};

/// Charcoal and lavender, carried over from the approved design.
mod theme {
    use ratatui::style::{Color, Modifier, Style};

    pub const LAVENDER: Color = Color::Rgb(0xb4, 0xa7, 0xf5);
    pub const CHARCOAL: Color = Color::Rgb(0x1c, 0x1c, 0x20);
    pub const TEXT: Color = Color::Rgb(0xe4, 0xe4, 0xe8);
    pub const MUTED: Color = Color::Rgb(0x8a, 0x8a, 0x95);
    pub const OK: Color = Color::Rgb(0x86, 0xc9, 0xa0);
    pub const WARN: Color = Color::Rgb(0xe0, 0xb5, 0x72);
    pub const ERROR: Color = Color::Rgb(0xe0, 0x7a, 0x7a);

    pub fn base() -> Style {
        Style::default().fg(TEXT).bg(CHARCOAL)
    }
    pub fn muted() -> Style {
        Style::default().fg(MUTED).bg(CHARCOAL)
    }
    pub fn accent() -> Style {
        Style::default().fg(LAVENDER).bg(CHARCOAL)
    }
    pub fn accent_bold() -> Style {
        accent().add_modifier(Modifier::BOLD)
    }
}

/// Draws one frame and records what it actually measured.
///
/// Takes `&mut AppState` specifically to write back `last_total_rows`
/// and `last_viewport_rows`: the scroll clamp and indicator need the
/// real wrapped-row count the terminal just displayed, not a logical
/// line count computed separately and liable to disagree with it.
pub fn draw(f: &mut Frame, state: &mut AppState) {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(theme::CHARCOAL)),
        area,
    );

    let banner_height = if state.banner.is_some() { 1 } else { 0 };
    let composer_height = composer_height(state, area.width.saturating_sub(2));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),               // header
            Constraint::Length(banner_height),   // optional warning banner
            Constraint::Min(3),                  // transcript
            Constraint::Length(composer_height), // composer
            Constraint::Length(1),               // help bar
        ])
        .split(area);

    draw_header(f, chunks[0], state);
    if let Some(banner) = &state.banner {
        draw_banner(f, chunks[1], banner);
    }
    draw_transcript(f, chunks[2], state);
    draw_composer(f, chunks[3], state);
    draw_help_bar(f, chunks[4], state);
    // Drawn last, over the transcript, so opening the picker never
    // reflows the conversation or moves the composer under the user's
    // cursor.
    if state.picker.is_open() {
        draw_picker(f, chunks[2], state);
    }
}

/// The `/` command picker, as an overlay anchored to the bottom of the
/// transcript area -- next to the composer, where the user is looking.
fn draw_picker(f: &mut Frame, transcript_area: Rect, state: &AppState) {
    let candidates = state.picker_candidates();
    // Cap the visible rows so a long list cannot cover the whole
    // conversation, and keep the highlighted row in view.
    const MAX_ROWS: usize = 8;
    let visible = candidates.len().min(MAX_ROWS);
    // At least one content row, so the "nothing matches" message has
    // somewhere to go -- an empty result set must say so, not compute a
    // box too short to draw and vanish silently.
    let rows = visible.max(1) as u16;
    let height = rows.saturating_add(2).min(transcript_area.height);
    if height < 3 {
        return;
    }
    let area = Rect {
        x: transcript_area.x,
        y: transcript_area.y + transcript_area.height - height,
        width: transcript_area.width,
        height,
    };
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::LAVENDER).bg(theme::CHARCOAL))
            .style(Style::default().bg(theme::CHARCOAL)),
        area,
    );

    if candidates.is_empty() {
        let inner = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no command matches — Esc to keep typing",
                theme::muted(),
            )))
            .style(theme::base()),
            inner,
        );
        return;
    }

    let selected = state.picker.selected().min(candidates.len() - 1);
    // Scroll the window so the highlight is always inside it.
    let first = selected.saturating_sub(MAX_ROWS - 1);
    let mut lines: Vec<Line> = Vec::with_capacity(visible);
    for (offset, spec) in candidates.iter().skip(first).take(visible).enumerate() {
        let idx = first + offset;
        let is_selected = idx == selected;
        let marker = if is_selected { "▌" } else { " " };
        let name_style = if is_selected {
            theme::accent_bold()
        } else {
            theme::base()
        };
        // In `Agents` scope every row is a specialist (never mixed
        // with an ordinary command in the same list -- that is the
        // whole point of the scope split), so the department is shown
        // as a tag rather than needing a type marker to disambiguate
        // row kinds the way a mixed list would.
        let mut spans = vec![
            Span::styled(marker, Style::default().fg(theme::LAVENDER)),
            Span::styled(format!(" /{:<16}", spec.path), name_style),
        ];
        if state.picker.scope() == crate::picker::PickerScope::Agents {
            spans.push(Span::styled(
                format!("[{}] ", spec.category),
                theme::accent(),
            ));
        }
        spans.push(Span::styled(spec.description.clone(), theme::muted()));
        lines.push(Line::from(spans));
    }
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    f.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

fn composer_height(state: &AppState, width: u16) -> u16 {
    // Grows with the draft, capped so a long paste cannot swallow the
    // transcript. Uses the composer's own wrapped row count, the same
    // measurement the cursor placement uses, so the box is always tall
    // enough for the text it is about to show.
    state.composer.rows(width).clamp(1, 6) + 2
}

fn draw_banner(f: &mut Frame, area: Rect, text: &str) {
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {text}"),
            Style::default()
                .fg(theme::WARN)
                .bg(theme::CHARCOAL)
                .add_modifier(Modifier::BOLD),
        )))
        .style(theme::base()),
        area,
    );
}

fn draw_header(f: &mut Frame, area: Rect, state: &AppState) {
    let mut spans = vec![
        Span::styled("SE", theme::accent_bold()),
        Span::styled("  ", theme::base()),
    ];
    match state.activity {
        Activity::Idle => spans.push(Span::styled("ready", theme::muted())),
        Activity::Working { task } => {
            spans.push(Span::styled(format!("working {task}"), theme::accent()));
        }
        Activity::AwaitingApproval { .. } => {
            spans.push(Span::styled(
                "waiting for your decision",
                theme::accent_bold(),
            ));
        }
    }
    if !state.status.is_empty() {
        spans.push(Span::styled("  ·  ", theme::muted()));
        spans.push(Span::styled(state.status.clone(), theme::muted()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(theme::base()), area);
}

fn draw_transcript(f: &mut Frame, area: Rect, state: &mut AppState) {
    let mut lines: Vec<Line> = Vec::new();
    for entry in &state.transcript {
        match entry {
            Entry::User(text) => {
                for (i, l) in text.lines().enumerate() {
                    let prefix = if i == 0 { "› " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, theme::accent_bold()),
                        Span::styled(l.to_string(), theme::base()),
                    ]));
                }
            }
            Entry::Assistant(text) => {
                for l in text.lines() {
                    lines.push(Line::from(Span::styled(l.to_string(), theme::base())));
                }
            }
            Entry::Reasoning(text) => {
                for l in text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  {l}"),
                        theme::muted().add_modifier(Modifier::ITALIC),
                    )));
                }
            }
            Entry::Tool {
                name,
                summary,
                state: tool_state,
            } => {
                let (mark, style) = match tool_state {
                    ToolState::Running => ("…", theme::muted()),
                    ToolState::Ok => ("✓", Style::default().fg(theme::OK).bg(theme::CHARCOAL)),
                    ToolState::Failed => {
                        ("✗", Style::default().fg(theme::ERROR).bg(theme::CHARCOAL))
                    }
                    ToolState::Denied => {
                        ("⊘", Style::default().fg(theme::WARN).bg(theme::CHARCOAL))
                    }
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {mark} "), style),
                    Span::styled(name.clone(), style),
                    Span::styled(format!("  {}", truncate(summary, 60)), theme::muted()),
                ]));
            }
            Entry::Approval {
                name,
                summary,
                decision,
            } => {
                let text = match decision {
                    None => format!("  approve {name}?  {}   [y/n]", truncate(summary, 48)),
                    Some(true) => format!("  approved {name}"),
                    Some(false) => format!("  denied {name}"),
                };
                let style = if decision.is_none() {
                    theme::accent_bold()
                } else {
                    theme::muted()
                };
                lines.push(Line::from(Span::styled(text, style)));
            }
            Entry::Notice(text) => {
                lines.push(Line::from(Span::styled(
                    format!("  {text}"),
                    theme::muted(),
                )));
            }
            Entry::Error(text) => {
                lines.push(Line::from(Span::styled(
                    format!("  {text}"),
                    Style::default().fg(theme::ERROR).bg(theme::CHARCOAL),
                )));
            }
        }
    }

    if state.dropped_events > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "  {} event(s) were not received — the transcript above may have gaps",
                state.dropped_events
            ),
            Style::default().fg(theme::WARN).bg(theme::CHARCOAL),
        )));
    }

    if state.show_diagnostics {
        if let Some(timings) = &state.last_timings {
            lines.push(Line::from(Span::styled(
                format!("  timings: {}", timings.summary()),
                theme::muted(),
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "  local {} · total {}",
                    se_protocol::timings::format_duration(timings.local()),
                    se_protocol::timings::format_duration(timings.total())
                ),
                theme::muted(),
            )));
        }
    }

    // Wrapped here rather than by `Paragraph`, because the scroll
    // offset is counted in *rendered* lines.
    //
    // The defect this fixes: a model's answer is one logical line no
    // matter how long it is, so counting logical lines made a single
    // streamed paragraph look like one row. Everything below it was
    // then pushed off the bottom of the viewport, with no way to scroll
    // to it. Lines are now pre-wrapped to the available width, so the
    // count the offset uses is the count the terminal shows.
    let inner_width = area.width.saturating_sub(1).max(1);
    let wrapped = wrap_lines(lines, inner_width);

    // `scroll` counts lines back from the newest, so the view stays
    // pinned to the bottom while content streams in. Recorded on `state`
    // so input handling (PageUp/PageDown/wheel) can clamp against the
    // same real measurement used to draw this frame, rather than a
    // logical-line count computed separately that could disagree.
    let viewport = area.height.saturating_sub(1) as usize;
    state.last_total_rows = wrapped.len();
    state.last_viewport_rows = viewport;
    state.clamp_scroll();

    let offset = if wrapped.len() > viewport {
        (wrapped.len() - viewport).saturating_sub(state.scroll as usize)
    } else {
        0
    };

    // A compact position indicator in the border title, rather than a
    // separate scrollbar widget: "N above" when scrolled back from the
    // newest content, nothing when already at the bottom (the common
    // case, which should not be cluttered).
    let title = if state.scroll > 0 && wrapped.len() > viewport {
        let above = wrapped
            .len()
            .saturating_sub(viewport + state.scroll as usize);
        format!(" ↑ {above} line(s) above · PgDn to catch up ")
    } else {
        String::new()
    };
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme::muted())
        .title(Span::styled(title, theme::accent()));
    f.render_widget(
        Paragraph::new(wrapped.into_iter().skip(offset).collect::<Vec<_>>())
            .style(theme::base())
            .block(block),
        area,
    );
}

/// Breaks lines at `width`, preferring word boundaries.
///
/// Styling is preserved per span: a wrapped fragment keeps the style of
/// the span it came from, so a tool row stays coloured when it wraps.
fn wrap_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let total: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        if total <= width {
            out.push(line);
            continue;
        }
        let mut current: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        for span in line.spans {
            let style = span.style;
            for word in split_keeping_spaces(&span.content) {
                let len = word.chars().count();
                // A word longer than the viewport is broken rather than
                // dropped: narrow terminals and long paths both happen.
                if len > width {
                    for chunk in chunk_chars(&word, width) {
                        if used > 0 {
                            out.push(Line::from(std::mem::take(&mut current)));
                        }
                        used = chunk.chars().count();
                        current.push(Span::styled(chunk, style));
                    }
                    continue;
                }
                if used + len > width {
                    out.push(Line::from(std::mem::take(&mut current)));
                    used = 0;
                    // A wrap point swallows the space that caused it.
                    if word.trim().is_empty() {
                        continue;
                    }
                }
                current.push(Span::styled(word, style));
                used += len;
            }
        }
        if !current.is_empty() {
            out.push(Line::from(current));
        }
    }
    out
}

/// Splits into words, keeping each trailing run of spaces attached so
/// wrapping does not silently reflow deliberate spacing.
fn split_keeping_spaces(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_space = false;
    for c in text.chars() {
        let is_space = c == ' ';
        if !current.is_empty() && is_space != in_space && !is_space {
            parts.push(std::mem::take(&mut current));
        }
        current.push(c);
        in_space = is_space;
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn chunk_chars(text: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width.max(1))
        .map(|c| c.iter().collect())
        .collect()
}

/// Draws the composer, including placing the real terminal cursor.
///
/// Placing the cursor (rather than only showing text) is what makes this
/// feel like an editor instead of a text box that happens to update: the
/// blinking terminal cursor is the thing a user's eye actually tracks
/// while typing, and before this it was not drawn at all -- the
/// underlying text changed, but the cursor never left the top-left
/// corner of the pane.
fn draw_composer(f: &mut Frame, area: Rect, state: &AppState) {
    let inner_width = area.width.saturating_sub(2).max(1);

    if state.awaiting_approval().is_some() {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme::accent_bold());
        f.render_widget(
            Paragraph::new("a decision is waiting above — press y to allow, n to deny")
                .style(theme::accent_bold())
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
        return;
    }

    let placeholder = state.composer.is_empty();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if state.is_busy() {
            theme::muted()
        } else {
            theme::accent()
        });

    if placeholder {
        f.render_widget(
            Paragraph::new("describe the work  (Enter to send, Shift+Enter for a new line)")
                .style(theme::muted())
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
        // The cursor still belongs at the start of an empty composer,
        // not wherever the previous frame happened to leave it.
        f.set_cursor_position(ratatui::layout::Position::new(area.x + 1, area.y + 1));
        return;
    }

    f.render_widget(
        Paragraph::new(state.composer.text().to_string())
            .style(theme::base())
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );

    let (row, col) = state.composer.cursor_rowcol(inner_width);
    // Clamped to the box: a cursor computed past the rendered area
    // (possible only if `composer_height`'s cap and this wrap ever
    // disagree) must never be handed to the terminal as an out-of-frame
    // position.
    let max_row = area.height.saturating_sub(3);
    f.set_cursor_position(ratatui::layout::Position::new(
        area.x + 1 + col.min(inner_width.saturating_sub(1)),
        area.y + 1 + row.min(max_row),
    ));
}

/// A compact, state-sensitive shortcut bar in place of a paragraph of
/// instructions: only the keys that do something *right now*, plus a
/// short status when a task is running, in one line.
fn draw_help_bar(f: &mut Frame, area: Rect, state: &AppState) {
    // Owned rather than borrowed because the idle hint names the
    // newline chord this terminal was actually observed to deliver,
    // which is runtime state, not a literal (charter §4).
    let hint: String = if state.awaiting_approval().is_some() {
        "y Allow   n Deny   Ctrl+C Exit".to_string()
    } else if state.picker.is_open() {
        // While the picker owns the keyboard, advertise its keys rather
        // than the composer's -- the keybinding hints and the actual
        // keymap must not disagree (charter §4).
        "↑↓ Select   Enter Run   Tab Complete   Esc Cancel".to_string()
    } else {
        match state.activity {
            Activity::Idle => format!(
                "/ Commands   Enter Send   {} Newline   Ctrl+D Diag   Ctrl+C Exit",
                state.keys.newline_hint()
            ),
            Activity::Working { .. } => {
                "Esc Interrupt   Ctrl+C Exit   PgUp/PgDn Scroll".to_string()
            }
            Activity::AwaitingApproval { .. } => unreachable!("handled above"),
        }
    };
    let mut spans = vec![Span::styled(hint, theme::muted())];
    if !state.status.is_empty() {
        spans.push(Span::styled("   ", theme::muted()));
        spans.push(Span::styled(state.status.clone(), theme::accent()));
    }
    spans.push(Span::styled("   ", theme::muted()));
    spans.push(Span::styled(state.usage_line(), theme::muted()));
    f.render_widget(Paragraph::new(Line::from(spans)).style(theme::base()), area);
}

fn truncate(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if flat.chars().count() <= max {
        return flat;
    }
    let kept: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use se_protocol::{Stage, Timings};

    fn render(state: &mut AppState, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// The honesty requirement, rendered: when there is no real model
    /// connected, the fact that replies are a local echo must be on
    /// screen, not only in a line printed to a terminal the alternate
    /// screen has already covered.
    #[test]
    fn a_banner_is_shown_when_there_is_no_real_model_connected() {
        let mut state = AppState::new();
        state.banner = Some("no model connected — replies below are a local echo".into());
        let out = render(&mut state, 80, 16);
        assert!(out.contains("local echo"), "{out}");
    }

    #[test]
    fn no_banner_means_no_banner_line_is_drawn() {
        let mut state = AppState::new();
        assert!(state.banner.is_none());
        let out = render(&mut state, 80, 16);
        assert!(!out.contains("echo"), "{out}");
    }

    #[test]
    fn the_frame_shows_a_header_transcript_composer_and_status() {
        let mut state = AppState::new();
        state.push(Entry::User("build the thing".into()));
        state.push(Entry::Assistant("working on it".into()));
        let out = render(&mut state, 80, 16);
        assert!(out.contains("SE"), "{out}");
        assert!(out.contains("build the thing"), "{out}");
        assert!(out.contains("working on it"), "{out}");
        assert!(out.contains("Enter Send"), "{out}");
    }

    #[test]
    fn a_pending_approval_is_visible_and_tells_the_user_the_keys() {
        let mut state = AppState::new();
        state.activity = Activity::AwaitingApproval {
            task: se_protocol::TaskId::from_raw(1),
            call: se_protocol::ToolCallId::from_raw(1),
        };
        state.push(Entry::Approval {
            name: "run_shell".into(),
            summary: "cargo test".into(),
            decision: None,
        });
        let out = render(&mut state, 80, 16);
        assert!(out.contains("run_shell"), "{out}");
        assert!(out.contains("y Allow"), "{out}");
        // The composer must say why it is not accepting text.
        assert!(out.contains("decision is waiting"), "{out}");
    }

    #[test]
    fn a_long_transcript_shows_its_newest_lines_by_default() {
        let mut state = AppState::new();
        for i in 0..200 {
            state.push(Entry::Assistant(format!("line {i}")));
        }
        let out = render(&mut state, 60, 12);
        assert!(out.contains("line 199"), "newest must be visible");
        assert!(!out.contains("line 0\n"), "oldest must be scrolled off");
    }

    #[test]
    fn scrolling_back_reveals_older_lines() {
        let mut state = AppState::new();
        for i in 0..200 {
            state.push(Entry::Assistant(format!("line {i}")));
        }
        state.scroll = 50;
        let out = render(&mut state, 60, 12);
        assert!(!out.contains("line 199"), "scrolled away from newest");
        assert!(out.contains("line 14") || out.contains("line 15"), "{out}");
    }

    #[test]
    fn a_gap_in_events_is_rendered_as_a_warning_not_hidden() {
        let mut state = AppState::new();
        state.dropped_events = 3;
        let out = render(&mut state, 80, 10);
        assert!(out.contains("not received"), "{out}");
    }

    #[test]
    fn diagnostics_show_stage_timings_when_enabled() {
        let mut state = AppState::new();
        let mut timings = Timings::default();
        timings.record(se_protocol::Span {
            stage: Stage::TimeToFirstToken,
            duration: std::time::Duration::from_millis(120),
        });
        state.last_timings = Some(timings);

        let hidden = render(&mut state, 80, 12);
        assert!(!hidden.contains("ttft"), "off by default");

        state.show_diagnostics = true;
        let shown = render(&mut state, 80, 12);
        assert!(shown.contains("ttft"), "{shown}");
        assert!(shown.contains("120ms"), "{shown}");
    }

    #[test]
    fn rendering_a_narrow_terminal_does_not_panic() {
        // Terminals get resized to silly sizes; drawing must survive it.
        let mut state = AppState::new();
        state.push(Entry::User("a fairly long line of text here".into()));
        state.composer.set("typing something".into());
        for (w, h) in [(20, 6), (10, 5), (120, 40), (40, 4)] {
            let _ = render(&mut state, w, h);
        }
    }

    #[test]
    fn a_tool_entry_shows_its_outcome_distinctly() {
        let mut state = AppState::new();
        state.push(Entry::Tool {
            name: "read_file".into(),
            summary: "src/main.rs".into(),
            state: ToolState::Ok,
        });
        state.push(Entry::Tool {
            name: "run_shell".into(),
            summary: "exit 1".into(),
            state: ToolState::Failed,
        });
        state.push(Entry::Tool {
            name: "write_file".into(),
            summary: "not run".into(),
            state: ToolState::Denied,
        });
        let out = render(&mut state, 80, 14);
        assert!(out.contains("✓"), "{out}");
        assert!(out.contains("✗"), "{out}");
        assert!(out.contains("⊘"), "{out}");
    }

    #[test]
    fn long_tool_summaries_are_truncated_rather_than_wrapping_the_layout() {
        assert_eq!(truncate("short", 10), "short");
        let long = "x".repeat(100);
        let cut = truncate(&long, 20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.ends_with('…'));
        // Newlines in a tool summary must not break the single-line row.
        assert_eq!(truncate("a\nb", 10), "a b");
    }
}

#[cfg(test)]
mod wrapping {
    use super::*;
    use crate::state::{AppState, Entry};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(state: &mut AppState, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// A model's answer is one logical line however long it is. Counting
    /// logical lines pushed everything after it off the bottom with no
    /// way to scroll back, so the end of a long answer was unreachable.
    #[test]
    fn the_end_of_a_very_long_answer_is_visible() {
        let mut state = AppState::new();
        let long: String = (0..200).map(|i| format!("word{i} ")).collect::<String>();
        state.push(Entry::Assistant(format!("{long}FINALWORD")));

        let out = render(&mut state, 60, 20);
        assert!(
            out.contains("FINALWORD"),
            "the end of a long answer must be on screen:\n{out}"
        );
    }

    #[test]
    fn scrolling_back_through_a_long_answer_reaches_its_beginning() {
        let mut state = AppState::new();
        let long: String = (0..200).map(|i| format!("word{i} ")).collect();
        state.push(Entry::Assistant(long));

        state.scroll = 400;
        let out = render(&mut state, 60, 20);
        assert!(
            out.contains("word0"),
            "scrolling back must reach the start:\n{out}"
        );
    }

    #[test]
    fn a_word_longer_than_the_terminal_is_broken_not_dropped() {
        let mut state = AppState::new();
        state.push(Entry::Assistant("x".repeat(300)));
        let out = render(&mut state, 30, 12);
        // Every rendered row is within the width, and the content is
        // present rather than silently truncated to one row.
        let rows: Vec<&str> = out.lines().filter(|l| l.contains('x')).collect();
        assert!(rows.len() > 5, "a 300-char word must occupy several rows");
    }

    #[test]
    fn wrapping_preserves_text_content_exactly() {
        let line = Line::from(vec![Span::raw("hello world this is a test of wrapping")]);
        let wrapped = wrap_lines(vec![line], 10);
        let joined: String = wrapped
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join("");
        // Spaces at wrap points are consumed, so compare words.
        let original: Vec<&str> = "hello world this is a test of wrapping"
            .split_whitespace()
            .collect();
        let after: Vec<&str> = joined.split_whitespace().collect();
        assert_eq!(original, after, "no word may be lost or duplicated");
    }

    #[test]
    fn a_short_line_is_left_untouched() {
        let line = Line::from(vec![Span::raw("short")]);
        let wrapped = wrap_lines(vec![line], 40);
        assert_eq!(wrapped.len(), 1);
    }

    fn with_picker(query: &str) -> AppState {
        let mut state = AppState::new();
        for c in query.chars() {
            crate::input::apply_local(&mut state, &crate::input::Action::InsertChar(c));
        }
        state
    }

    #[test]
    fn an_open_picker_is_actually_drawn_with_its_candidates() {
        let mut state = with_picker("/mod");
        assert!(state.picker.is_open());
        let screen = render(&mut state, 80, 20);
        assert!(
            screen.contains("/models"),
            "the picker must show the matching command:\n{screen}"
        );
    }

    #[test]
    fn a_closed_picker_draws_nothing_over_the_transcript() {
        let mut state = AppState::new();
        let screen = render(&mut state, 80, 20);
        assert!(!screen.contains("/models"));
    }

    #[test]
    fn the_picker_shows_a_message_when_nothing_matches() {
        // An empty result set must not render as a blank box.
        let mut state = with_picker("/zzzznope");
        let screen = render(&mut state, 80, 20);
        assert!(screen.contains("no command matches"), "got:\n{screen}");
    }

    #[test]
    fn the_picker_does_not_cover_the_composer() {
        // It anchors to the transcript area; the draft must stay
        // visible while the user is choosing a command.
        let mut state = with_picker("/mod");
        let screen = render(&mut state, 80, 20);
        let lines: Vec<&str> = screen.lines().collect();
        let composer_row = lines
            .iter()
            .rposition(|l| l.contains("/mod"))
            .expect("the draft must be on screen");
        // The last occurrence of the typed text is the composer, below
        // the picker rows.
        assert!(
            composer_row >= lines.len() - 3,
            "composer at {composer_row} of {}",
            lines.len()
        );
    }

    #[test]
    fn the_help_bar_advertises_the_picker_keys_while_it_is_open() {
        let mut state = with_picker("/mod");
        let screen = render(&mut state, 100, 20);
        assert!(screen.contains("Esc Cancel"), "got:\n{screen}");
        assert!(screen.contains("Enter Run"));
    }

    #[test]
    fn the_help_bar_advertises_slash_for_commands_when_idle() {
        let mut state = AppState::new();
        let screen = render(&mut state, 100, 20);
        assert!(screen.contains("/ Commands"), "got:\n{screen}");
    }

    #[test]
    fn a_narrow_terminal_does_not_panic_with_the_picker_open() {
        // The picker computes its own rect; a terminal too short for a
        // bordered box must skip it rather than underflow.
        let mut state = with_picker("/mod");
        for (w, h) in [(20u16, 6u16), (10, 5), (40, 4)] {
            let _ = render(&mut state, w, h);
        }
    }
}
