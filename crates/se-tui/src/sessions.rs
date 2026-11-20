//! The `/resume` picker: this project's saved conversations (Tab: every
//! recent project's), resumed in place with Enter and arranged with
//! r (rename), p (pin), K/J or Shift+Up/Down (move), d (delete, to
//! `.trash`) and u (undo the last delete). The files themselves are
//! handled by [`se_core::sessions`].

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use se_core::sessions as store;

use crate::state::{AppState, Entry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Browse,
    /// Typing a new title for the highlighted row.
    Rename(String),
    /// `d` was pressed: `y` deletes, anything else cancels.
    ConfirmDelete,
}

#[derive(Debug, Clone)]
pub struct Item {
    /// The project it belongs to, when that is not this one.
    pub elsewhere: Option<PathBuf>,
    pub row: store::Row,
}

#[derive(Debug, Clone)]
pub struct Overlay {
    /// Every recent project, not just this one.
    pub all: bool,
    pub items: Vec<Item>,
    pub selected: usize,
    pub mode: Mode,
    /// The last thing a key did (or why it could not).
    pub message: Option<String>,
}

/// Opens the picker on this project's conversations.
pub fn open(state: &mut AppState) {
    if state.sessions_dir.is_none() {
        state.push(Entry::Notice(
            "no sessions folder here: resume with `se --resume=<id>`".to_string(),
        ));
        return;
    }
    state.picker.close();
    state.sessions = Some(Overlay {
        all: false,
        items: Vec::new(),
        selected: 0,
        mode: Mode::Browse,
        message: None,
    });
    reload(state);
}

/// Re-reads the rows, keeping the highlight on `keep` if given.
fn reload_keeping(state: &mut AppState, keep: Option<&str>) {
    let Some(dir) = state.sessions_dir.clone() else {
        return;
    };
    let Some(ov) = state.sessions.as_mut() else {
        return;
    };
    let here = project_of(&dir);
    let (rows, notice) = store::list(&dir);
    let mut items: Vec<Item> = rows
        .into_iter()
        .map(|row| Item {
            elsewhere: None,
            row,
        })
        .collect();
    if ov.all {
        let recent = state
            .recent_projects
            .as_deref()
            .map(store::recent_projects)
            .unwrap_or_default();
        for p in recent.into_iter().filter(|p| !same_path(&p.path, &here)) {
            let dir = se_core::session_store::default_sessions_dir(&p.path);
            items.extend(store::list(&dir).0.into_iter().map(|row| Item {
                elsewhere: Some(p.path.clone()),
                row,
            }));
        }
    }
    if notice.is_some() {
        ov.message = notice;
    }
    if let Some(id) = keep {
        if let Some(i) = items
            .iter()
            .position(|it| it.elsewhere.is_none() && it.row.id == id)
        {
            ov.selected = i;
        }
    }
    ov.selected = ov.selected.min(items.len().saturating_sub(1));
    ov.items = items;
}

fn reload(state: &mut AppState) {
    reload_keeping(state, None);
}

/// `<project>/.se/sessions` -> `<project>`.
fn project_of(dir: &Path) -> PathBuf {
    dir.parent()
        .and_then(Path::parent)
        .map_or_else(|| dir.to_path_buf(), Path::to_path_buf)
}

fn same_path(a: &Path, b: &Path) -> bool {
    let (a, b) = (a.to_string_lossy(), b.to_string_lossy());
    if cfg!(windows) {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

fn say(state: &mut AppState, text: impl Into<String>) {
    if let Some(ov) = state.sessions.as_mut() {
        ov.message = Some(text.into());
    }
}

/// Text pasted while renaming goes into the title; otherwise it is dropped.
pub fn paste(state: &mut AppState, text: &str) {
    if let Some(Overlay {
        mode: Mode::Rename(buf),
        ..
    }) = state.sessions.as_mut()
    {
        buf.extend(text.chars().filter(|c| !c.is_control()));
    }
}

/// One key while the picker is open.
pub fn key(state: &mut AppState, key: KeyEvent) {
    let Some(ov) = state.sessions.as_mut() else {
        return;
    };
    let Some(dir) = state.sessions_dir.clone() else {
        return;
    };
    match &mut ov.mode {
        Mode::Rename(buf) => {
            match key.code {
                KeyCode::Esc => ov.mode = Mode::Browse,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => buf.push(c),
                KeyCode::Enter => {
                    let title = buf.clone();
                    ov.mode = Mode::Browse;
                    let id = ov.items[ov.selected].row.id.clone();
                    match store::rename(&dir, &id, &title) {
                        Ok(()) => {
                            reload_keeping(state, Some(&id));
                            say(state, "renamed");
                        }
                        Err(e) => say(state, format!("rename failed: {e}")),
                    }
                }
                _ => {}
            }
            return;
        }
        Mode::ConfirmDelete => {
            ov.mode = Mode::Browse;
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                let id = ov.items[ov.selected].row.id.clone();
                match store::trash(&dir, &id, state.session_id.as_deref()) {
                    Ok(t) => {
                        state.trashed.push((dir.clone(), t));
                        reload(state);
                        say(
                            state,
                            format!("moved {id} to .se/sessions/.trash — u to undo"),
                        );
                    }
                    Err(e) => say(state, format!("not deleted: {e}")),
                }
            } else {
                ov.message = Some("delete cancelled".to_string());
            }
            return;
        }
        Mode::Browse => {}
    }

    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let current = ov.items.get(ov.selected).cloned();
    let arranging = matches!(key.code, KeyCode::Char('r' | 'p' | 'd' | 'K' | 'J'))
        || (shift && matches!(key.code, KeyCode::Up | KeyCode::Down));
    if arranging && (ov.all || current.is_none()) {
        ov.message = Some(if ov.all {
            "arrange in this project's view (Tab)".to_string()
        } else {
            "nothing to arrange".to_string()
        });
        return;
    }
    match key.code {
        KeyCode::Esc => state.sessions = None,
        KeyCode::Tab | KeyCode::BackTab => {
            ov.all = !ov.all;
            ov.selected = 0;
            ov.message = None;
            reload(state);
        }
        KeyCode::Up | KeyCode::Char('K') | KeyCode::Down | KeyCode::Char('J') if arranging => {
            let up = matches!(key.code, KeyCode::Up | KeyCode::Char('K'));
            let rows: Vec<store::Row> = ov.items.iter().map(|it| it.row.clone()).collect();
            let id = rows[ov.selected].id.clone();
            match store::move_row(&dir, &rows, ov.selected, up) {
                Ok(Some(_)) => reload_keeping(state, Some(&id)),
                Ok(None) => {
                    ov.message = Some("cannot move further (pinned rows stay on top)".to_string())
                }
                Err(e) => ov.message = Some(format!("move failed: {e}")),
            }
        }
        KeyCode::Up => ov.selected = ov.selected.saturating_sub(1),
        KeyCode::Down => ov.selected = (ov.selected + 1).min(ov.items.len().saturating_sub(1)),
        KeyCode::Char('r') => {
            ov.mode = Mode::Rename(current.map(|it| it.row.title).unwrap_or_default())
        }
        KeyCode::Char('p') => {
            let id = current.map(|it| it.row.id).unwrap_or_default();
            match store::toggle_pin(&dir, &id) {
                Ok(on) => {
                    reload_keeping(state, Some(&id));
                    say(state, if on { "pinned" } else { "unpinned" });
                }
                Err(e) => ov.message = Some(format!("pin failed: {e}")),
            }
        }
        KeyCode::Char('d') => {
            let id = current.map(|it| it.row.id).unwrap_or_default();
            if state.session_id.as_deref() == Some(id.as_str()) {
                ov.message = Some(
                    "that is the open conversation: switch away from it before deleting"
                        .to_string(),
                );
            } else {
                ov.mode = Mode::ConfirmDelete;
                ov.message = Some(format!(
                    "delete {id}? y to move it to .trash, any other key cancels"
                ));
            }
        }
        KeyCode::Char('u') => match state.trashed.pop() {
            None => ov.message = Some("nothing deleted this run to undo".to_string()),
            Some((from, t)) => match store::restore(&from, &t) {
                Ok(()) => {
                    reload_keeping(state, Some(&t.id));
                    say(state, format!("restored {}", t.id));
                }
                Err(e) => {
                    ov.message = Some(format!("undo failed: {e}"));
                    state.trashed.push((from, t));
                }
            },
        },
        KeyCode::Enter => match current {
            None => {}
            Some(Item {
                elsewhere: Some(path),
                row,
            }) => {
                ov.message = Some(format!(
                    "start SE in {} to resume it: `se --resume={}`",
                    path.display(),
                    row.id
                ));
            }
            Some(Item { row, .. }) if state.session_id.as_deref() == Some(row.id.as_str()) => {
                state.sessions = None;
            }
            Some(Item { row, .. }) => {
                state.sessions = None;
                state.switch_to = Some(Some(row.id));
            }
        },
        _ => {}
    }
}

/// "3 h ago" for a last-saved time.
pub fn age(when: std::time::SystemTime) -> String {
    let mins = when.elapsed().map_or(0, |d| d.as_secs() / 60);
    match mins {
        0 => "just now".to_string(),
        m if m < 60 => format!("{m} min ago"),
        m if m < 60 * 24 => format!("{} h ago", m / 60),
        m => format!("{} days ago", m / (60 * 24)),
    }
}
