//! A project's saved conversations (`.se/sessions`) as a list the user can
//! arrange: titles, pins and manual order kept in `index.json`, deletion as a
//! move to `.trash/<id>/` that can be undone, and the small
//! `recent-projects.json` list. Every path is passed in, so nothing here
//! decides where a home directory is.
//!
//! A session's files are every top-level file named `<id>.*`:
//! `<id>.conversation.json`, `<id>.specialist`, `<id>.model` and
//! `<id>.<ms>.events.jsonl`.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

const INDEX: &str = "index.json";
const TRASH: &str = ".trash";
const CONVERSATION: &str = ".conversation.json";
/// How many projects `recent-projects.json` keeps.
pub const RECENT_CAP: usize = 50;

/// The user's arrangement of one session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Arrangement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<u32>,
}

/// `index.json`: arrangements by session id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub sessions: BTreeMap<String, Arrangement>,
}

/// One saved conversation as the picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    /// The user's title, else the first thing they asked.
    pub title: String,
    /// When the conversation was last saved.
    pub modified: SystemTime,
    /// `provider/model` of its last turn, when recorded (`<id>.model`).
    pub model: Option<String>,
    pub messages: usize,
    pub pinned: bool,
}

/// Reads `index.json`. Missing is empty; unreadable is empty plus a notice
/// for the user (the bad file is kept, see [`save_index`]).
pub fn load_index(dir: &Path) -> (Index, Option<String>) {
    let path = dir.join(INDEX);
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(index) => (index, None),
            Err(e) => (
                Index::default(),
                Some(format!(
                    "{} is unreadable ({e}): titles, pins and order are ignored; the next change keeps it as {INDEX}.bad and starts a new one",
                    path.display()
                )),
            ),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Index::default(), None),
        Err(e) => (
            Index::default(),
            Some(format!("cannot read {} ({e}): titles, pins and order are ignored", path.display())),
        ),
    }
}

pub fn save_index(dir: &Path, index: &Index) -> io::Result<()> {
    write_json(&dir.join(INDEX), index)
}

/// Writes via a temp file and rename. A file already there that does not
/// parse as `T` is kept as `<name>.bad` first, never silently replaced.
fn write_json<T: Serialize + for<'de> Deserialize<'de>>(path: &Path, value: &T) -> io::Result<()> {
    if let Ok(old) = std::fs::read_to_string(path) {
        if serde_json::from_str::<T>(&old).is_err() {
            std::fs::rename(path, path.with_extension("json.bad"))?;
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// The conversations in `dir`: pinned first, then manually ordered, then
/// the rest newest first. A notice comes back if `index.json` was unusable.
pub fn list(dir: &Path) -> (Vec<Row>, Option<String>) {
    let (index, notice) = load_index(dir);
    let mut rows: Vec<(Option<u32>, Row)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let id = e
                .file_name()
                .into_string()
                .ok()?
                .strip_suffix(CONVERSATION)?
                .to_string();
            let modified = e.metadata().ok()?.modified().ok()?;
            let saved = crate::load_conversation(&e.path()).unwrap_or_default();
            let asked = saved
                .iter()
                .find(|m| {
                    m.role == se_provider::Role::User
                        && !m.content.starts_with(crate::provider_runner::MEMORY_PREFIX)
                })
                .map(|m| {
                    m.content
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .trim()
                })
                .filter(|t| !t.is_empty())
                .map(|t| t.chars().take(80).collect::<String>());
            let arrangement = index.sessions.get(&id).cloned().unwrap_or_default();
            let model = std::fs::read_to_string(dir.join(format!("{id}.model")))
                .ok()
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty());
            Some((
                arrangement.order,
                Row {
                    title: arrangement
                        .title
                        .or(asked)
                        .unwrap_or_else(|| "(no messages)".to_string()),
                    modified,
                    model,
                    messages: saved.len(),
                    pinned: arrangement.pinned,
                    id,
                },
            ))
        })
        .collect();
    rows.sort_by(|(ao, a), (bo, b)| {
        (
            !a.pinned,
            ao.unwrap_or(u32::MAX),
            std::cmp::Reverse(a.modified),
            &a.id,
        )
            .cmp(&(
                !b.pinned,
                bo.unwrap_or(u32::MAX),
                std::cmp::Reverse(b.modified),
                &b.id,
            ))
    });
    (rows.into_iter().map(|(_, r)| r).collect(), notice)
}

fn arrange(dir: &Path, change: impl FnOnce(&mut Index)) -> io::Result<()> {
    let (mut index, _) = load_index(dir);
    change(&mut index);
    index.sessions.retain(|_, a| *a != Arrangement::default());
    save_index(dir, &index)
}

/// Sets the title shown for `id`; a blank title goes back to the first message.
pub fn rename(dir: &Path, id: &str, title: &str) -> io::Result<()> {
    let title = title.trim();
    arrange(dir, |index| {
        index.sessions.entry(id.to_string()).or_default().title =
            (!title.is_empty()).then(|| title.to_string());
    })
}

/// Pins or unpins `id`; returns whether it is now pinned.
pub fn toggle_pin(dir: &Path, id: &str) -> io::Result<bool> {
    let mut now = false;
    arrange(dir, |index| {
        let a = index.sessions.entry(id.to_string()).or_default();
        a.pinned = !a.pinned;
        now = a.pinned;
    })?;
    Ok(now)
}

/// Moves `rows[at]` one place up or down, within its pinned/unpinned group,
/// and saves the whole visible order. Returns its new position, or `None`
/// if it cannot move that way.
pub fn move_row(dir: &Path, rows: &[Row], at: usize, up: bool) -> io::Result<Option<usize>> {
    let to = if up { at.checked_sub(1) } else { Some(at + 1) };
    let Some(to) = to.filter(|t| *t < rows.len() && at < rows.len()) else {
        return Ok(None);
    };
    if rows[at].pinned != rows[to].pinned {
        return Ok(None);
    }
    let mut ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    ids.swap(at, to);
    arrange(dir, |index| {
        for (n, id) in ids.iter().enumerate() {
            index.sessions.entry((*id).to_string()).or_default().order = Some(n as u32);
        }
    })?;
    Ok(Some(to))
}

/// A deleted session, enough to put it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trashed {
    pub id: String,
    pub files: Vec<String>,
    pub arrangement: Option<Arrangement>,
}

/// Every top-level file of session `id` in `dir`.
pub fn sidecars(dir: &Path, id: &str) -> Vec<String> {
    let prefix = format!("{id}.");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(&prefix))
        .collect();
    names.sort();
    names
}

/// Moves every file of `id` to `.trash/<id>/` and drops its index entry.
/// Refuses the conversation that is open (`open`), and never overwrites an
/// earlier trash of the same id.
pub fn trash(dir: &Path, id: &str, open: Option<&str>) -> io::Result<Trashed> {
    if open == Some(id) || id.is_empty() {
        return Err(io::Error::other(format!(
            "{id} is the open conversation: switch away from it first"
        )));
    }
    let files = sidecars(dir, id);
    if files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no files for session {id}"),
        ));
    }
    let bin = dir.join(TRASH).join(id);
    if bin.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists; move it away first", bin.display()),
        ));
    }
    std::fs::create_dir_all(&bin)?;
    move_all(&files, dir, &bin)?;
    let (index, _) = load_index(dir);
    let arrangement = index.sessions.get(id).cloned();
    if arrangement.is_some() {
        arrange(dir, |index| {
            index.sessions.remove(id);
        })?;
    }
    Ok(Trashed {
        id: id.to_string(),
        files,
        arrangement,
    })
}

/// Puts a [`trash`]ed session back, byte for byte, with its index entry.
pub fn restore(dir: &Path, trashed: &Trashed) -> io::Result<()> {
    let bin = dir.join(TRASH).join(&trashed.id);
    if let Some(clash) = trashed.files.iter().find(|f| dir.join(f).exists()) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{clash} exists again in {}; not overwriting it",
                dir.display()
            ),
        ));
    }
    move_all(&trashed.files, &bin, dir)?;
    let _ = std::fs::remove_dir(&bin);
    if let Some(a) = &trashed.arrangement {
        arrange(dir, |index| {
            index.sessions.insert(trashed.id.clone(), a.clone());
        })?;
    }
    Ok(())
}

/// Moves `files` from `from` to `to`; on a failure, moves back what moved.
fn move_all(files: &[String], from: &Path, to: &Path) -> io::Result<()> {
    for (n, f) in files.iter().enumerate() {
        if let Err(e) = std::fs::rename(from.join(f), to.join(f)) {
            for done in &files[..n] {
                let _ = std::fs::rename(to.join(done), from.join(done));
            }
            return Err(e);
        }
    }
    Ok(())
}

/// One entry of `recent-projects.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentProject {
    pub path: PathBuf,
    /// Seconds since the Unix epoch.
    pub last_used: u64,
}

/// The recent projects, most recently used first; missing or unreadable is empty.
pub fn recent_projects(file: &Path) -> Vec<RecentProject> {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Records that SE started in `project` at `now`, keeping the newest
/// [`RECENT_CAP`]. No disk scanning: this list is the only source.
pub fn touch_recent(file: &Path, project: &Path, now: u64) -> io::Result<()> {
    let same = |a: &Path| {
        let (a, b) = (a.to_string_lossy(), project.to_string_lossy());
        if cfg!(windows) {
            a.eq_ignore_ascii_case(&b)
        } else {
            a == b
        }
    };
    let mut list = recent_projects(file);
    list.retain(|p| !same(&p.path));
    list.insert(
        0,
        RecentProject {
            path: project.to_path_buf(),
            last_used: now,
        },
    );
    list.sort_by_key(|p| std::cmp::Reverse(p.last_used));
    list.truncate(RECENT_CAP);
    write_json(file, &list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("se-sessions-test-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A conversation saved `age` seconds ago, asking `first`.
    fn save(dir: &Path, id: &str, first: &str, age: u64) {
        use se_provider::{Message, Role};
        let json = serde_json::to_string(&[
            Message::text(Role::User, first),
            Message::text(Role::Assistant, "ok"),
        ])
        .unwrap();
        let path = dir.join(format!("{id}{CONVERSATION}"));
        std::fs::write(&path, json).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    fn ids(dir: &Path) -> Vec<String> {
        list(dir).0.into_iter().map(|r| r.id).collect()
    }

    #[test]
    fn rows_are_pinned_then_manual_order_then_newest_first() {
        let d = TempDir::new();
        save(&d.0, "a", "oldest", 300);
        save(&d.0, "b", "middle", 200);
        save(&d.0, "c", "newest", 100);
        save(&d.0, "d", "older still", 400);
        assert_eq!(ids(&d.0), ["c", "b", "a", "d"], "no index: newest first");

        let rows = list(&d.0).0;
        assert_eq!(rows[0].title, "newest");
        assert_eq!(rows[0].messages, 2);
        // Move `b` above `c`: the visible order is saved.
        assert_eq!(move_row(&d.0, &rows, 1, true).unwrap(), Some(0));
        assert_eq!(ids(&d.0), ["b", "c", "a", "d"]);
        // A session with no index entry goes after the ordered ones.
        save(&d.0, "e", "brand new", 1);
        assert_eq!(ids(&d.0), ["b", "c", "a", "d", "e"]);
        // Pinned first, whatever the order says.
        assert!(toggle_pin(&d.0, "d").unwrap());
        assert_eq!(ids(&d.0), ["d", "b", "c", "a", "e"]);
        // A move never crosses the pinned/unpinned boundary.
        let rows = list(&d.0).0;
        assert_eq!(move_row(&d.0, &rows, 1, true).unwrap(), None);
        assert_eq!(move_row(&d.0, &rows, 0, true).unwrap(), None);
        assert!(!toggle_pin(&d.0, "d").unwrap());
        assert_eq!(ids(&d.0), ["b", "c", "a", "d", "e"]);
    }

    #[test]
    fn a_rename_persists_and_a_blank_one_goes_back_to_the_first_message() {
        let d = TempDir::new();
        save(&d.0, "a", "what is rust\nsecond line", 10);
        assert_eq!(list(&d.0).0[0].title, "what is rust");
        rename(&d.0, "a", "  Rust notes ").unwrap();
        assert_eq!(list(&d.0).0[0].title, "Rust notes");
        assert!(std::fs::read_to_string(d.0.join(INDEX))
            .unwrap()
            .contains("Rust notes"));
        rename(&d.0, "a", "").unwrap();
        assert_eq!(list(&d.0).0[0].title, "what is rust");
    }

    #[test]
    fn a_corrupt_index_still_lists_everything_and_is_kept_before_replacing() {
        let d = TempDir::new();
        save(&d.0, "a", "one", 20);
        save(&d.0, "b", "two", 10);
        std::fs::write(d.0.join(INDEX), "{ not json").unwrap();
        let (rows, notice) = list(&d.0);
        assert_eq!(rows.len(), 2);
        assert!(notice.unwrap().contains("unreadable"));
        // Listing never rewrites it.
        assert_eq!(
            std::fs::read_to_string(d.0.join(INDEX)).unwrap(),
            "{ not json"
        );
        // A change keeps the bad file aside, then starts fresh.
        rename(&d.0, "a", "kept").unwrap();
        assert_eq!(
            std::fs::read_to_string(d.0.join("index.json.bad")).unwrap(),
            "{ not json"
        );
        let (rows, notice) = list(&d.0);
        assert!(notice.is_none());
        assert!(rows.iter().any(|r| r.title == "kept"));
    }

    #[test]
    fn delete_moves_every_file_to_trash_and_undo_restores_them_byte_identical() {
        let d = TempDir::new();
        save(&d.0, "17", "keep me", 10);
        save(&d.0, "170", "a different session", 5);
        std::fs::write(d.0.join("17.specialist"), "backend-lead").unwrap();
        std::fs::write(d.0.join("17.model"), "nvidia/x").unwrap();
        std::fs::write(d.0.join("17.99.events.jsonl"), [0u8, 159, 146, 150, b'\n']).unwrap();
        rename(&d.0, "17", "titled").unwrap();
        toggle_pin(&d.0, "17").unwrap();
        let before: Vec<(String, Vec<u8>)> = sidecars(&d.0, "17")
            .into_iter()
            .map(|f| (f.clone(), std::fs::read(d.0.join(&f)).unwrap()))
            .collect();
        assert_eq!(before.len(), 4);

        let t = trash(&d.0, "17", Some("170")).unwrap();
        assert!(sidecars(&d.0, "17").is_empty());
        assert_eq!(ids(&d.0), ["170"], "the other session is untouched");
        for (f, _) in &before {
            assert!(d.0.join(".trash").join("17").join(f).exists());
        }
        assert!(!load_index(&d.0).0.sessions.contains_key("17"));

        restore(&d.0, &t).unwrap();
        for (f, bytes) in &before {
            assert_eq!(&std::fs::read(d.0.join(f)).unwrap(), bytes, "{f}");
        }
        let rows = list(&d.0).0;
        assert_eq!(
            (rows[0].id.as_str(), rows[0].title.as_str(), rows[0].pinned),
            ("17", "titled", true)
        );
        assert!(!d.0.join(".trash").join("17").exists());
    }

    #[test]
    fn the_open_conversation_cannot_be_deleted() {
        let d = TempDir::new();
        save(&d.0, "5", "open one", 10);
        assert!(trash(&d.0, "5", Some("5")).is_err());
        assert_eq!(sidecars(&d.0, "5").len(), 1);
    }

    #[test]
    fn recent_projects_dedupe_newest_first_and_cap() {
        let d = TempDir::new();
        let file = d.0.join("home").join(".se").join("recent-projects.json");
        touch_recent(&file, Path::new("/p/one"), 1).unwrap();
        touch_recent(&file, Path::new("/p/two"), 2).unwrap();
        touch_recent(&file, Path::new("/p/one"), 3).unwrap();
        let list = recent_projects(&file);
        assert_eq!(
            list.iter().map(|p| p.path.clone()).collect::<Vec<_>>(),
            [PathBuf::from("/p/one"), PathBuf::from("/p/two")]
        );
        for n in 0..60 {
            touch_recent(&file, &PathBuf::from(format!("/q/{n}")), 10 + n).unwrap();
        }
        let list = recent_projects(&file);
        assert_eq!(list.len(), RECENT_CAP);
        assert_eq!(list[0].path, PathBuf::from("/q/59"));
    }
}
