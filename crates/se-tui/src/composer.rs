//! The message editor: text plus a cursor.
//!
//! The previous composer was an append-only `String` with `push`/`pop`,
//! so a typo three words back could only be fixed by deleting everything
//! after it. This is the smallest thing that edits like a terminal
//! should: a `String` and a cursor byte offset kept on a char boundary.
//!
//! Not a rope or a gap buffer -- a composed message is short, so the
//! cost of shifting a `String` on each edit is irrelevant, and a reader
//! understands a `String` at a glance.

/// A multi-line text buffer with a single cursor.
#[derive(Debug, Default, Clone)]
pub struct Composer {
    text: String,
    /// Byte offset of the cursor. Always at a `char` boundary and in
    /// `0..=text.len()`.
    cursor: usize,
}

impl Composer {
    pub fn new() -> Self {
        Composer::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replaces the whole buffer, cursor at the end. For seeding a
    /// refused submission back into the editor.
    pub fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Moves the cursor to a byte offset. Clamped into range and
    /// snapped to the next `char` boundary, because the invariant that
    /// the cursor is always on a boundary is what keeps every slicing
    /// operation in this type panic-free.
    pub fn set_cursor(&mut self, offset: usize) {
        let mut offset = offset.min(self.text.len());
        while offset < self.text.len() && !self.text.is_char_boundary(offset) {
            offset += 1;
        }
        self.cursor = offset;
    }

    /// Takes the contents, leaving the composer empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Deletes the char before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.prev_boundary(self.cursor);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    /// Deletes the char at the cursor.
    pub fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = self.next_boundary(self.cursor);
        self.text.replace_range(self.cursor..next, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
    }

    pub fn right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
    }

    /// Start of the current logical line.
    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map(|nl| nl + 1)
            .unwrap_or(0);
    }

    /// End of the current logical line.
    pub fn end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map(|rel| self.cursor + rel)
            .unwrap_or(self.text.len());
    }

    /// Cursor position as (row, column) once the text is wrapped at
    /// `width`, so the terminal cursor can be placed where the character
    /// visually is. Must use the same wrapping the renderer draws with,
    /// or the cursor drifts from the text.
    pub fn cursor_rowcol(&self, width: u16) -> (u16, u16) {
        let width = width.max(1) as usize;
        let mut row = 0usize;
        let mut col = 0usize;
        for (i, c) in self.text.char_indices() {
            if i >= self.cursor {
                break;
            }
            if c == '\n' {
                row += 1;
                col = 0;
            } else {
                col += 1;
                if col == width {
                    row += 1;
                    col = 0;
                }
            }
        }
        (row as u16, col as u16)
    }

    /// Visual rows the text occupies at `width`, at least one.
    pub fn rows(&self, width: u16) -> u16 {
        let width = width.max(1) as usize;
        let mut rows = 1usize;
        let mut col = 0usize;
        for c in self.text.chars() {
            if c == '\n' {
                rows += 1;
                col = 0;
            } else {
                col += 1;
                if col == width {
                    rows += 1;
                    col = 0;
                }
            }
        }
        rows as u16
    }

    fn prev_boundary(&self, from: usize) -> usize {
        let mut i = from.saturating_sub(1);
        while i > 0 && !self.text.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    fn next_boundary(&self, from: usize) -> usize {
        let mut i = (from + 1).min(self.text.len());
        while i < self.text.len() && !self.text.is_char_boundary(i) {
            i += 1;
        }
        i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Composer {
        let mut c = Composer::new();
        for ch in s.chars() {
            c.insert_char(ch);
        }
        c
    }

    #[test]
    fn typing_then_editing_in_the_middle_works() {
        let mut c = typed("helo world");
        // Fix "helo" -> "hello": move back to after "hel".
        for _ in 0..7 {
            c.left();
        }
        c.insert_char('l');
        assert_eq!(c.text(), "hello world");
    }

    #[test]
    fn backspace_and_delete_act_at_the_cursor_not_the_end() {
        let mut c = typed("abcd");
        c.left(); // between c and d
        c.backspace(); // removes c
        assert_eq!(c.text(), "abd");
        c.delete(); // removes d
        assert_eq!(c.text(), "ab");
    }

    #[test]
    fn home_and_end_move_within_the_current_line() {
        let mut c = typed("one\ntwo three");
        c.home();
        assert_eq!(c.cursor(), 4, "start of the second line");
        c.end();
        assert_eq!(c.cursor(), c.text().len(), "end of the second line");
    }

    #[test]
    fn the_cursor_never_lands_inside_a_multibyte_char() {
        let mut c = typed("aéb"); // é is two bytes
        c.home();
        c.right(); // past 'a'
        c.right(); // past 'é' (both bytes at once)
        assert!(c.text().is_char_boundary(c.cursor()));
        c.backspace(); // removes é whole, not half
        assert_eq!(c.text(), "ab");
    }

    #[test]
    fn cursor_rowcol_accounts_for_newlines_and_wrapping() {
        let mut c = typed("abc\nde");
        // cursor at end: row 1 (after the newline), col 2.
        assert_eq!(c.cursor_rowcol(80), (1, 2));

        // Wrapping: 5 chars at width 3 is col 0..3 then wraps.
        c.clear();
        for ch in "abcde".chars() {
            c.insert_char(ch);
        }
        assert_eq!(c.cursor_rowcol(3), (1, 2));
    }

    #[test]
    fn rows_counts_wrapped_and_explicit_lines() {
        assert_eq!(typed("").rows(80), 1);
        assert_eq!(typed("short").rows(80), 1);
        assert_eq!(typed("a\nb\nc").rows(80), 3);
        // 6 chars at width 3 exactly fills two rows; the cursor model
        // treats a line filled exactly to width the same way most
        // terminals do, by moving to a fresh row -- consistent with
        // cursor_rowcol's own wrap behaviour, tested above.
        assert_eq!(typed("abcdef").rows(3), 3);
    }

    #[test]
    fn take_empties_and_resets_the_cursor() {
        let mut c = typed("hello");
        assert_eq!(c.take(), "hello");
        assert!(c.is_empty());
        assert_eq!(c.cursor(), 0);
    }

    #[test]
    fn edits_at_the_boundaries_do_not_panic() {
        let mut c = Composer::new();
        c.backspace(); // empty
        c.delete(); // empty
        c.left();
        c.right();
        c.home();
        c.end();
        assert!(c.is_empty());
        assert_eq!(c.cursor(), 0);
    }
}
