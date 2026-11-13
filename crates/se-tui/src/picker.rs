//! The `/` command picker, as composer state.
//!
//! Ported from the structure upstream OpenCode uses
//! (`packages/tui/src/component/prompt/autocomplete.tsx@9bbb8ce`): the
//! picker is *inside* the composer rather than a separate dialog, it
//! remembers the byte offset where the trigger character sits, and the
//! live query is simply the text between that trigger and the cursor.
//!
//! What is deliberately *not* ported is upstream's signal-copying
//! workaround. Upstream copies the query through a reactive effect
//! because its reactive text and non-reactive cursor can be briefly out
//! of sync on a keypress; SE's composer owns its buffer synchronously,
//! so the query is derived fresh from (trigger offset, cursor) and there
//! is no intermediate state that can disagree.
//!
//! The picker holds no command data of its own. It stores a trigger
//! offset and a selected row; the candidate list is recomputed from the
//! registry each frame, which is what keeps "what is on screen" and
//! "what Enter will run" from ever drifting apart.

/// Whether the picker is open, and if so where its trigger sits.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Picker {
    #[default]
    Closed,
    Open {
        /// Byte offset of the `/` that opened the picker. The query is
        /// the text from `trigger + 1` to the cursor.
        trigger: usize,
        /// Index into the current candidate list. Clamped on use, not
        /// on store, because the list length changes as the user types
        /// and a stored index would go stale between frames.
        selected: usize,
    },
}

impl Picker {
    pub fn is_open(&self) -> bool {
        matches!(self, Picker::Open { .. })
    }

    /// Opens the picker at `trigger`, the offset of the `/` just typed.
    pub fn open(&mut self, trigger: usize) {
        *self = Picker::Open {
            trigger,
            selected: 0,
        };
    }

    pub fn close(&mut self) {
        *self = Picker::Closed;
    }

    pub fn trigger(&self) -> Option<usize> {
        match self {
            Picker::Open { trigger, .. } => Some(*trigger),
            Picker::Closed => None,
        }
    }

    pub fn selected(&self) -> usize {
        match self {
            Picker::Open { selected, .. } => *selected,
            Picker::Closed => 0,
        }
    }

    /// Moves the highlight, wrapping, against a list of `len` rows.
    /// A zero-length list leaves the selection at 0 so an empty result
    /// set cannot produce an out-of-range highlight.
    pub fn move_selection(&mut self, delta: i32, len: usize) {
        if let Picker::Open { selected, .. } = self {
            if len == 0 {
                *selected = 0;
                return;
            }
            let n = len as i32;
            *selected = (((*selected as i32 + delta) % n + n) % n) as usize;
        }
    }

    /// The live query: the text between the trigger and the cursor.
    ///
    /// Returns `None` when the picker should close because the user has
    /// edited away from it -- the cursor moved behind the trigger, or
    /// the trigger character itself was deleted.
    pub fn query<'t>(&self, text: &'t str, cursor: usize) -> Option<&'t str> {
        let trigger = self.trigger()?;
        // The trigger must still be a `/`; if it was deleted or
        // overwritten, the picker is stale.
        if text.as_bytes().get(trigger) != Some(&b'/') {
            return None;
        }
        let start = trigger + 1;
        if cursor < start || cursor > text.len() {
            return None;
        }
        // A query never spans a line or a space: typing `/models then
        // more words` is a prompt, not a command, so the picker closes
        // once the user moves past the command word.
        let slice = text.get(start..cursor)?;
        if slice.contains(char::is_whitespace) {
            return None;
        }
        Some(slice)
    }

    /// Whether a `/` typed at `cursor` in `text` should open the
    /// picker. True only at the very start of the buffer or directly
    /// after whitespace, so a `/` inside a path (`src/lib.rs`) or a
    /// closing tag never opens it.
    pub fn should_open(text: &str, cursor: usize) -> bool {
        if cursor == 0 {
            return true;
        }
        text[..cursor]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slash_at_the_start_of_an_empty_composer_opens_the_picker() {
        assert!(Picker::should_open("", 0));
    }

    #[test]
    fn a_slash_after_a_space_opens_the_picker() {
        assert!(Picker::should_open("hello ", 6));
    }

    #[test]
    fn a_slash_inside_a_path_does_not_open_the_picker() {
        // The defect this prevents: typing `src/lib.rs` popping a
        // command picker mid-word.
        assert!(!Picker::should_open("src", 3));
        assert!(!Picker::should_open("look at src", 11));
    }

    #[test]
    fn a_slash_after_a_newline_opens_the_picker() {
        assert!(Picker::should_open("line one\n", 9));
    }

    #[test]
    fn the_query_is_the_text_between_the_trigger_and_the_cursor() {
        let mut p = Picker::Closed;
        p.open(0);
        assert_eq!(p.query("/mod", 4), Some("mod"));
        // Cursor mid-word: the query is only what is behind it, so
        // filtering tracks the cursor rather than the whole word.
        assert_eq!(p.query("/models", 4), Some("mod"));
    }

    #[test]
    fn an_empty_query_is_the_state_right_after_the_slash() {
        let mut p = Picker::Closed;
        p.open(0);
        assert_eq!(p.query("/", 1), Some(""));
    }

    #[test]
    fn the_picker_goes_stale_when_the_trigger_is_deleted() {
        let mut p = Picker::Closed;
        p.open(0);
        // The user backspaced over the `/`; what is left is a plain
        // prompt and the picker must not keep filtering it.
        assert_eq!(p.query("mod", 3), None);
    }

    #[test]
    fn the_picker_goes_stale_when_the_cursor_moves_behind_the_trigger() {
        let mut p = Picker::Closed;
        p.open(5);
        assert_eq!(p.query("hello /mod", 3), None);
    }

    #[test]
    fn a_query_containing_whitespace_closes_the_picker() {
        // `/models then a sentence` is a prompt the user is writing,
        // not a command still being chosen.
        let mut p = Picker::Closed;
        p.open(0);
        assert_eq!(p.query("/models and more", 16), None);
    }

    #[test]
    fn the_trigger_offset_is_respected_when_the_slash_is_not_at_zero() {
        let mut p = Picker::Closed;
        p.open(6);
        assert_eq!(p.query("hello /mod", 10), Some("mod"));
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut p = Picker::Closed;
        p.open(0);
        p.move_selection(1, 3);
        assert_eq!(p.selected(), 1);
        p.move_selection(-1, 3);
        assert_eq!(p.selected(), 0);
        // Up from the top goes to the bottom.
        p.move_selection(-1, 3);
        assert_eq!(p.selected(), 2);
        // Down from the bottom returns to the top.
        p.move_selection(1, 3);
        assert_eq!(p.selected(), 0);
    }

    #[test]
    fn selection_on_an_empty_result_set_stays_in_range() {
        // An empty candidate list must not leave a highlight pointing
        // at a row that does not exist.
        let mut p = Picker::Closed;
        p.open(0);
        p.move_selection(1, 0);
        assert_eq!(p.selected(), 0);
    }

    #[test]
    fn a_closed_picker_has_no_query_and_no_trigger() {
        let p = Picker::Closed;
        assert_eq!(p.trigger(), None);
        assert_eq!(p.query("/mod", 4), None);
        assert!(!p.is_open());
    }

    #[test]
    fn a_multibyte_query_does_not_panic_or_split_a_char() {
        // `get()` returning None on a non-boundary is the guard; this
        // pins it so a later refactor to slicing would fail here.
        let mut p = Picker::Closed;
        p.open(0);
        let text = "/日本語";
        assert_eq!(p.query(text, text.len()), Some("日本語"));
        // Offset 2 is inside the first multibyte char.
        assert_eq!(p.query(text, 2), None);
    }
}
