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

    // A long banner wraps (up to 3 rows) rather than running off-screen.
    let banner_height = state.banner.as_ref().map_or(0, |b| {
        let width = usize::from(area.width.max(2)) - 1;
        (b.chars().count() + 1).div_ceil(width).clamp(1, 3) as u16
    });
    let composer_height = composer_height(state, area.width.saturating_sub(2));
    let strip_height = strip_height(&state.agents);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),               // header
            Constraint::Length(banner_height),   // optional warning banner
            Constraint::Min(3),                  // transcript
            Constraint::Length(composer_height), // composer
            Constraint::Length(strip_height),    // agent strip (0 rows: none)
            Constraint::Length(1),               // help bar
        ])
        .split(area);

    draw_header(f, chunks[0], state);
    if let Some(banner) = &state.banner {
        draw_banner(f, chunks[1], banner);
    }
    match state.agents.viewing() {
        // The agent's own transcript in the transcript area, with the same
        // wrapping and scroll handling, by lending it the slot.
        Some(i) => {
            std::mem::swap(
                &mut state.transcript,
                &mut state.agents.agents[i].transcript,
            );
            draw_transcript(f, chunks[2], state);
            std::mem::swap(
                &mut state.transcript,
                &mut state.agents.agents[i].transcript,
            );
        }
        None => draw_transcript(f, chunks[2], state),
    }
    draw_composer(f, chunks[3], state);
    draw_strip(f, chunks[4], state);
    draw_help_bar(f, chunks[5], state);
    // Drawn last, over the transcript, so opening the picker never
    // reflows the conversation or moves the composer under the user's
    // cursor.
    if state.picker.is_open() {
        draw_picker(f, chunks[2], state);
    }
    if let Some(ov) = &state.sessions {
        draw_sessions(f, chunks[2], ov, state.session_id.as_deref());
    }
}

/// The `/resume` picker, over the whole transcript area.
fn draw_sessions(f: &mut Frame, area: Rect, ov: &crate::sessions::Overlay, open: Option<&str>) {
    if area.height < 4 {
        return;
    }
    let scope = if ov.all {
        "all recent projects"
    } else {
        "this project"
    };
    let title = format!(" conversations · {scope} · {} ", ov.items.len());
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::LAVENDER).bg(theme::CHARCOAL))
            .title(Span::styled(title, theme::accent()))
            .style(Style::default().bg(theme::CHARCOAL)),
        area,
    );
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    // The last inner row is the prompt / message line.
    let room = usize::from(inner.height.saturating_sub(1)).max(1);
    let width = usize::from(inner.width);
    let mut lines: Vec<Line> = Vec::new();
    if ov.items.is_empty() {
        lines.push(Line::from(Span::styled(
            "no saved conversations yet (one is saved after its first reply)",
            theme::muted(),
        )));
    }
    let first = ov.selected.saturating_sub(room - 1);
    for (i, it) in ov.items.iter().enumerate().skip(first).take(room) {
        let selected = i == ov.selected;
        let r = &it.row;
        let mut meta = format!(
            "  {} · {} · {} msgs",
            crate::sessions::age(r.modified),
            r.model.as_deref().unwrap_or("model ?"),
            r.messages
        );
        if let Some(p) = &it.elsewhere {
            meta.push_str(&format!(" · {}", p.display()));
        }
        if it.elsewhere.is_none() && open == Some(r.id.as_str()) {
            meta.push_str(" · open");
        }
        let pin = if r.pinned { "* " } else { "  " };
        let title_room = width.saturating_sub(meta.chars().count() + 4).max(10);
        lines.push(Line::from(vec![
            Span::styled(
                if selected { "▌" } else { " " },
                Style::default().fg(theme::LAVENDER),
            ),
            Span::styled(pin, theme::accent()),
            Span::styled(
                truncate(&r.title, title_room),
                if selected {
                    theme::accent_bold()
                } else {
                    theme::base()
                },
            ),
            Span::styled(meta, theme::muted()),
        ]));
    }
    f.render_widget(Paragraph::new(lines).style(theme::base()), inner);
    let prompt = match &ov.mode {
        crate::sessions::Mode::Rename(buf) => {
            format!("new title: {buf}▏  (Enter save · Esc cancel · empty = first message)")
        }
        _ => ov.message.clone().unwrap_or_default(),
    };
    let last = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate(&prompt, width),
            theme::accent(),
        )))
        .style(theme::base()),
        last,
    );
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
    // How many match and where the highlight is, so a long list (100+
    // models) shows that there is more below and that typing filters it.
    let what = match state.picker.scope() {
        crate::picker::PickerScope::Commands => "commands",
        crate::picker::PickerScope::Agents => "agents",
        crate::picker::PickerScope::Models => "models",
        crate::picker::PickerScope::Providers => "providers",
    };
    let title = if candidates.is_empty() {
        String::new()
    } else {
        format!(
            " {what} {} of {} \u{b7} type to filter ",
            state.picker.selected().min(candidates.len() - 1) + 1,
            candidates.len()
        )
    };
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::LAVENDER).bg(theme::CHARCOAL))
            .title(Span::styled(title, theme::accent()))
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
    // One column width for the whole list, so names and descriptions
    // line up and a long name never runs into its tag.
    let name_width = candidates
        .iter()
        .map(|c| c.path.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(12, 40);
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
            Span::styled(format!(" /{:<name_width$}  ", spec.path), name_style),
        ];
        // Agents carry their department; a model's provider is already
        // the first part of its name, so it gets no tag.
        if matches!(
            state.picker.scope(),
            crate::picker::PickerScope::Agents | crate::picker::PickerScope::Providers
        ) {
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

fn strip_height(strip: &crate::agents::Strip) -> u16 {
    if strip.is_empty() {
        0
    } else if strip.collapsed() {
        1
    } else {
        strip.agents.len().min(crate::agents::MAX_ROWS) as u16
    }
}

/// One line per agent below the composer, or one summary line when there
/// are many and the strip is not focused. Records rows for mouse clicks.
fn draw_strip(f: &mut Frame, area: Rect, state: &mut AppState) {
    use ratatui::style::Color;
    use se_protocol::AgentState as S;
    let strip = &state.agents;
    let mut hit = Vec::new();
    let mut lines = Vec::new();
    let dot = |s: S| {
        let c = match s {
            S::Running => theme::LAVENDER,
            S::Background => Color::Rgb(0x7a, 0xa8, 0xe0),
            S::WaitingApproval => theme::WARN,
            S::Done => theme::OK,
            S::Failed => theme::ERROR,
            S::Cancelled => theme::MUTED,
        };
        Span::styled(" \u{25cf} ", Style::default().fg(c).bg(theme::CHARCOAL))
    };
    if strip.collapsed() {
        let active = strip
            .agents
            .iter()
            .filter(|a| !a.state.is_terminal())
            .count();
        let state_of_all = if active > 0 { S::Running } else { S::Done };
        lines.push(Line::from(vec![
            dot(state_of_all),
            Span::styled(
                format!(
                    "{} agents · {active} running · {} finished  (↓ to expand)",
                    strip.agents.len(),
                    strip.agents.len() - active
                ),
                theme::base(),
            ),
        ]));
        hit.push((area.y, None));
    } else if !strip.is_empty() {
        let now = std::time::Instant::now();
        let selected = strip.selected();
        let rows = usize::from(area.height);
        // Keep the selection in the window.
        let first = selected.map_or(0, |s| s.saturating_sub(rows.saturating_sub(1)));
        for (i, a) in strip.agents.iter().enumerate().skip(first).take(rows) {
            let name = if a.role == a.id {
                a.id.clone()
            } else {
                format!("{} [{}]", a.role, a.id)
            };
            let tokens = a.tokens.map_or(String::new(), |t| format!(" · {t} tok"));
            let text = format!(
                "{name} · {} · {}{tokens}",
                a.state.label(),
                crate::agents::elapsed_label(a.elapsed(now))
            );
            let style = if selected == Some(i) {
                theme::accent_bold().add_modifier(Modifier::REVERSED)
            } else {
                theme::base()
            };
            hit.push((area.y + (i - first) as u16, Some(i)));
            lines.push(Line::from(vec![
                dot(a.state),
                Span::styled(text, style),
                Span::styled(format!("  {}", truncate(&a.task, 60)), theme::muted()),
            ]));
        }
    }
    f.render_widget(Paragraph::new(lines).style(theme::base()), area);
    state.agents.hit = hit;
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
        .style(theme::base())
        .wrap(Wrap { trim: false }),
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
    let sep = || Span::styled("  \u{b7}  ", theme::muted());
    spans.push(sep());
    match state.current_model() {
        Some(m) => spans.push(Span::styled(m, theme::accent())),
        None => spans.push(Span::styled(
            "no model chosen",
            Style::default().fg(theme::WARN).bg(theme::CHARCOAL),
        )),
    }
    spans.push(sep());
    spans.push(Span::styled(
        format!("{} mode", state.mode.name()),
        if state.mode == se_protocol::WorkMode::Auto {
            Style::default().fg(theme::WARN).bg(theme::CHARCOAL)
        } else {
            theme::muted()
        },
    ));
    if let Some(agent) = &state.agent {
        spans.push(sep());
        spans.push(Span::styled(agent.clone(), theme::base()));
    }
    if !state.folder.is_empty() {
        spans.push(sep());
        spans.push(Span::styled(state.folder.clone(), theme::muted()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(theme::base()), area);
}

/// Light Markdown for answers, with no new dependency: fenced code as a
/// marked block (fence lines hidden, language shown), headings and
/// `**bold**` emphasised, `inline code` highlighted, list bullets as dots.
fn markdown_lines(text: &str) -> Vec<Line<'static>> {
    let code = Style::default().fg(theme::LAVENDER).bg(theme::CHARCOAL);
    let mut out = Vec::new();
    let mut in_code = false;
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            in_code = !in_code;
            let lang = rest.trim();
            if in_code && !lang.is_empty() {
                out.push(Line::from(Span::styled(
                    format!("  {lang}"),
                    theme::muted(),
                )));
            }
            continue;
        }
        if in_code {
            out.push(Line::from(vec![
                Span::styled("  \u{2502} ", theme::muted()),
                Span::styled(raw.to_string(), code),
            ]));
            continue;
        }
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            out.push(Line::from(Span::styled(
                trimmed[hashes..].trim().to_string(),
                theme::accent_bold(),
            )));
            continue;
        }
        let indent = raw.len() - trimmed.len();
        let (prefix, body) = match trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            Some(rest) => (format!("{}\u{2022} ", " ".repeat(indent)), rest),
            None => (" ".repeat(indent), trimmed),
        };
        let mut spans = vec![Span::styled(prefix, theme::base())];
        spans.extend(inline_spans(body, code));
        out.push(Line::from(spans));
    }
    out
}

/// `**bold**` and `` `code` `` within one line. An unclosed marker is
/// shown as typed rather than swallowing the rest of the line.
fn inline_spans(line: &str, code: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = line;
    while !rest.is_empty() {
        let next = [("**", rest.find("**")), ("`", rest.find('`'))]
            .into_iter()
            .filter_map(|(m, at)| at.map(|a| (a, m)))
            .min();
        let Some((at, marker)) = next else {
            spans.push(Span::styled(rest.to_string(), theme::base()));
            break;
        };
        let after = &rest[at + marker.len()..];
        let Some(end) = after.find(marker) else {
            spans.push(Span::styled(rest.to_string(), theme::base()));
            break;
        };
        if at > 0 {
            spans.push(Span::styled(rest[..at].to_string(), theme::base()));
        }
        let style = if marker == "`" {
            code
        } else {
            theme::base().add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(after[..end].to_string(), style));
        rest = &after[end + marker.len()..];
    }
    spans
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
            Entry::Assistant(text) => lines.extend(markdown_lines(text)),
            Entry::Speaker(who) => {
                lines.push(Line::from(Span::styled(
                    format!("\u{25cf} {who}"),
                    theme::accent(),
                )));
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
            // Multi-line notices (`/help`, `/providers`, ...) keep their
            // line breaks instead of collapsing into one paragraph.
            Entry::Notice(text) => {
                for l in text.lines() {
                    lines.push(Line::from(Span::styled(format!("  {l}"), theme::muted())));
                }
            }
            Entry::Error(text) => {
                for l in text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  {l}"),
                        Style::default().fg(theme::ERROR).bg(theme::CHARCOAL),
                    )));
                }
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

    // A draft typed before the prompt arrived is drawn as usual so it can be
    // cleared; the help bar says why y/n do not answer yet.
    if state.awaiting_approval().is_some() && state.composer.is_empty() {
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

    // /connect fields: each with its own title, so it is obvious the text goes to
    // secure storage and not to the model. The key is drawn as dots only.
    if let Some(step) = &state.connect {
        let typed = state.composer.text();
        let (shown, title) = match step {
            crate::state::ConnectStep::Key { name, .. } => {
                let dots = "\u{2022}".repeat(typed.chars().count().min(64));
                (
                    if dots.is_empty() {
                        "paste the key, then Enter (Esc cancels)".to_string()
                    } else {
                        dots
                    },
                    format!(" API key for {name} \u{2014} hidden, saved to secure storage "),
                )
            }
            crate::state::ConnectStep::Address { name, .. } => (
                if typed.is_empty() {
                    "paste the API address (https://\u{2026}/v1), then Enter (Esc cancels)"
                        .to_string()
                } else {
                    typed.to_string()
                },
                format!(" API address for {name} \u{2014} the key comes next "),
            ),
        };
        f.render_widget(
            Paragraph::new(shown).style(theme::base()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::accent_bold())
                    .title(title),
            ),
            area,
        );
        let col = (state.composer.text().chars().count().min(64) as u16)
            .min(area.width.saturating_sub(3));
        f.set_cursor_position(ratatui::layout::Position::new(area.x + 1 + col, area.y + 1));
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
            // Names the newline chord this terminal is known to deliver,
            // the same one the help bar shows -- never an unproven one.
            Paragraph::new(format!(
                "describe the work  (Enter to send, {} for a new line)",
                state.keys.newline_hint()
            ))
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
    let hint: String = if state.awaiting_approval().is_some() && !state.composer.is_empty() {
        "a decision is waiting: clear the draft (Ctrl+U) to answer   Esc Interrupt".to_string()
    } else if state.awaiting_approval().is_some() {
        "y Allow   n Deny   Ctrl+C Exit".to_string()
    } else if state.sessions.is_some() {
        "↑↓ Select   Enter Resume   Tab All projects   r Rename   p Pin   K/J Move   d Delete   u Undo   Esc Close"
            .to_string()
    } else if state.picker.is_open() {
        // While the picker owns the keyboard, advertise its keys rather
        // than the composer's -- the keybinding hints and the actual
        // keymap must not disagree (charter §4).
        "↑↓ Select   Enter Run   Tab Complete   Esc Cancel".to_string()
    } else if let Some(i) = state.agents.viewing() {
        format!(
            "agent {}   ↑↓ PgUp/PgDn Scroll   Esc Back to agents",
            state.agents.agents[i].id
        )
    } else if matches!(state.agents.focus, crate::agents::Focus::Strip(_)) {
        "↑↓ Select   Enter Open transcript   Esc Back to input".to_string()
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
    let mouse = if state.mouse_capture {
        "   F2 mouse:on   Shift+Tab mode"
    } else {
        "   F2 mouse:off   Shift+Tab mode"
    };
    let mut spans = vec![
        Span::styled(hint, theme::muted()),
        Span::styled(mouse, theme::muted()),
    ];
    if state.agents.focus == crate::agents::Focus::Composer
        && crate::agents::wants(
            state,
            &crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Down),
        )
    {
        spans.insert(1, Span::styled("   ↓ Agents", theme::muted()));
    }
    if !state.status.is_empty() {
        spans.push(Span::styled("   ", theme::muted()));
        spans.push(Span::styled(state.status.clone(), theme::accent()));
    }
    if state.last_timings.is_some() || state.is_busy() {
        spans.push(Span::styled("   ", theme::muted()));
        spans.push(Span::styled(state.usage_line(), theme::muted()));
    }
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
    fn strip_lines(out: &str) -> Vec<&str> {
        out.lines()
            .filter(|l| l.contains('\u{25cf}') && l.contains("task "))
            .collect()
    }

    #[test]
    fn the_agent_strip_is_hidden_with_no_agents() {
        let mut state = AppState::new();
        let before = render(&mut state, 80, 20);
        assert!(!before.contains('\u{25cf}'));
        assert!(!before.contains("agents"));
        assert!(state.agents.hit.is_empty());
    }

    #[test]
    fn the_agent_strip_shows_one_line_per_agent_below_the_composer() {
        let mut state = AppState::new();
        crate::agents::tests::spawn(&mut state, &["a", "b", "c", "d"]);
        state.agents.apply(&se_protocol::AgentEvent::Tokens {
            agent_id: "b".into(),
            used: 1234,
        });
        state.agents.apply(&se_protocol::AgentEvent::Failed {
            agent_id: "c".into(),
            error: "x".into(),
        });
        let out = render(&mut state, 100, 24);
        let lines = strip_lines(&out);
        assert_eq!(lines.len(), 4, "{out}");
        assert!(lines[0].contains("auto [a] · running · 0s"), "{out}");
        assert!(lines[1].contains("1234 tok"), "{out}");
        assert!(lines[2].contains("failed"), "{out}");
        // Below the composer's bottom border, above the help bar.
        let rows: Vec<&str> = out.lines().collect();
        let first = rows.iter().position(|l| l.contains("auto [a]")).unwrap();
        assert!(rows[first - 1].contains('└'), "{out}");
        assert_eq!(state.agents.hit.len(), 4);
        assert_eq!(state.agents.hit[0], (first as u16, Some(0)));
    }

    #[test]
    fn more_than_four_agents_collapse_to_a_summary_until_focused() {
        let mut state = AppState::new();
        crate::agents::tests::spawn(&mut state, &["a", "b", "c", "d", "e"]);
        state.agents.apply(&se_protocol::AgentEvent::Done {
            agent_id: "e".into(),
            summary: "ok".into(),
        });
        let out = render(&mut state, 100, 24);
        assert!(strip_lines(&out).is_empty(), "{out}");
        assert!(
            out.contains("5 agents · 4 running · 1 finished  (↓ to expand)"),
            "{out}"
        );
        assert_eq!(state.agents.hit.len(), 1);
        state.agents.focus = crate::agents::Focus::Strip(0);
        let out = render(&mut state, 100, 24);
        assert_eq!(strip_lines(&out).len(), 5, "{out}");
        assert!(out.contains("Enter Open transcript"), "{out}");
    }

    #[test]
    fn an_open_agent_view_shows_that_agents_transcript_not_the_chat() {
        let mut state = AppState::new();
        state.push(Entry::Assistant("main chat answer".into()));
        crate::agents::tests::spawn(&mut state, &["a"]);
        state.agents.apply(&se_protocol::AgentEvent::Activity {
            agent_id: "a".into(),
            payload: Box::new(se_protocol::Payload::Delta {
                kind: se_protocol::DeltaKind::Text,
                text: "agent says hi".into(),
            }),
        });
        crate::agents::set_focus(&mut state, crate::agents::Focus::View(0));
        let out = render(&mut state, 80, 20);
        assert!(out.contains("agent says hi"), "{out}");
        assert!(!out.contains("main chat answer"), "{out}");
        assert!(out.contains("Esc Back to agents"), "{out}");
        // Swapped back after drawing.
        assert_eq!(
            state.transcript,
            vec![Entry::Assistant("main chat answer".into())]
        );
        crate::agents::set_focus(&mut state, crate::agents::Focus::Composer);
        assert!(render(&mut state, 80, 20).contains("main chat answer"));
    }

    #[test]
    fn a_click_on_a_strip_row_opens_that_agent_only_when_reported() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut state = AppState::new();
        crate::agents::tests::spawn(&mut state, &["a", "b"]);
        render(&mut state, 80, 20);
        let (y, _) = state.agents.hit[1];
        let click = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            crate::input::action_for_mouse(&state, click(0)),
            crate::Action::Nothing
        );
        let a = crate::input::action_for_mouse(&state, click(y));
        crate::input::apply_local(&mut state, &a);
        assert_eq!(state.agents.focus, crate::agents::Focus::View(1));
    }

    #[test]
    fn a_banner_is_shown_when_there_is_no_real_model_connected() {
        let mut state = AppState::new();
        state.banner = Some("no model connected — replies below are a local echo".into());
        let out = render(&mut state, 80, 16);
        assert!(out.contains("local echo"), "{out}");
    }

    /// The screenshot bug: a long banner was cut off at the screen edge.
    #[test]
    fn a_long_banner_wraps_instead_of_running_off_screen() {
        let mut state = AppState::new();
        state.banner = Some(format!("{} END-OF-BANNER", "word ".repeat(20)));
        let out = render(&mut state, 60, 16);
        assert!(out.contains("END-OF-BANNER"), "{out}");
    }

    /// The screenshot bug: `/connect` and `/help` lost their line breaks.
    #[test]
    fn a_multi_line_notice_keeps_its_line_breaks() {
        let mut state = AppState::new();
        state.push(Entry::Notice("first line\nsecond line".into()));
        let out = render(&mut state, 80, 16);
        let rows: Vec<&str> = out.lines().collect();
        let first = rows
            .iter()
            .position(|r| r.contains("first line"))
            .expect("drawn");
        assert!(rows[first + 1].contains("second line"), "{out}");
        assert!(!rows[first].contains("second line"), "{out}");
    }

    /// The screenshot bug: the composer advertised Shift+Enter while the
    /// help bar (correctly) said Alt+Enter.
    #[test]
    fn the_composer_hint_names_the_same_newline_key_as_the_help_bar() {
        let mut state = AppState::new();
        let out = render(&mut state, 100, 16);
        let hint = state.keys.newline_hint();
        assert!(out.contains(&format!("{hint} for a new line")), "{out}");
        assert!(!out.contains("Shift+Enter for a new line") || hint == "Shift+Enter");
    }

    /// Answers are Markdown: fences, `**bold**` and inline code markers
    /// must not reach the screen as raw characters.
    #[test]
    fn markdown_in_an_answer_is_rendered_not_shown_raw() {
        let mut state = AppState::new();
        state.push(Entry::Assistant(
            "## Plan\n```sql\nCREATE TABLE orders (id bigint);\n```\n- **Note:** use `cents`"
                .into(),
        ));
        let out = render(&mut state, 80, 16);
        assert!(out.contains("CREATE TABLE orders"), "{out}");
        assert!(out.contains("Note: use cents"), "{out}");
        assert!(
            !out.contains("```") && !out.contains("**") && !out.contains("## "),
            "{out}"
        );
        assert!(out.contains("sql"), "the language label stays: {out}");
    }

    /// Every turn runs as an agent; the user must see which one, and on
    /// which model.
    #[test]
    fn the_answering_agent_and_model_are_shown() {
        let mut state = AppState::new();
        state.default_model = Some(se_protocol::ModelRef {
            provider: "claude-code".into(),
            model: "sonnet".into(),
        });
        state.agent = Some("api-architect".into());
        state.push(Entry::Speaker(
            "api-architect \u{b7} claude-code/sonnet".into(),
        ));
        let out = render(&mut state, 100, 16);
        let header = out.lines().next().unwrap();
        assert!(
            header.contains("claude-code/sonnet") && header.contains("api-architect"),
            "{header}"
        );
        assert!(out.contains("\u{25cf} api-architect"), "{out}");
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
