//! Cursor-aware multi-line text editor backing the Chat input. Content is a
//! list of logical lines (split on `\n`); the cursor is a `(row, col)` pair in
//! char units. Editing/navigation operate on `char` boundaries so multi-byte
//! input never lands mid-codepoint. The renderer asks for the wrapped viewport
//! slice + cursor via [`TextArea::layout`] so what is shown and where the
//! caret sits always agree.

/// One operator input buffer: cursor-aware, multi-line, char-indexed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextArea {
    /// Logical lines (never empty: an empty buffer is one empty line).
    lines: Vec<String>,
    /// Cursor row (line index).
    row: usize,
    /// Cursor column, in chars, within `lines[row]`.
    col: usize,
}

impl Default for TextArea {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }
}

impl TextArea {
    /// Whole buffer joined with `\n`.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// True when the buffer holds nothing but a single empty line.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// Reset to a single empty line with the cursor at the origin.
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
    }

    #[cfg(test)]
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    #[cfg(test)]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    fn line_chars(&self, row: usize) -> Vec<char> {
        self.lines[row].chars().collect()
    }

    fn line_len(&self, row: usize) -> usize {
        self.lines[row].chars().count()
    }

    /// Insert one char at the cursor (typing). Newlines are handled by
    /// [`insert_newline`]; this never receives one.
    pub fn insert_char(&mut self, c: char) {
        let mut chars = self.line_chars(self.row);
        chars.insert(self.col, c);
        self.lines[self.row] = chars.into_iter().collect();
        self.col += 1;
    }

    /// Insert a string at the cursor, splitting on `\n` (paste / programmatic).
    pub fn insert_str(&mut self, text: &str) {
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                self.insert_newline();
            }
            for c in part.chars() {
                self.insert_char(c);
            }
        }
    }

    /// Split the current line at the cursor (Shift+Enter / pasted newline).
    pub fn insert_newline(&mut self) {
        let chars = self.line_chars(self.row);
        let (left, right): (Vec<char>, Vec<char>) = {
            let (l, r) = chars.split_at(self.col);
            (l.to_vec(), r.to_vec())
        };
        self.lines[self.row] = left.into_iter().collect();
        self.lines.insert(self.row + 1, right.into_iter().collect());
        self.row += 1;
        self.col = 0;
    }

    /// Backspace: delete the char before the cursor, joining lines at a line
    /// start.
    pub fn backspace(&mut self) {
        if self.col > 0 {
            let mut chars = self.line_chars(self.row);
            chars.remove(self.col - 1);
            self.lines[self.row] = chars.into_iter().collect();
            self.col -= 1;
        } else if self.row > 0 {
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len(self.row);
            self.lines[self.row].push_str(&current);
        }
    }

    /// Delete the char at the cursor, joining the next line in at a line end.
    pub fn delete_forward(&mut self) {
        let len = self.line_len(self.row);
        if self.col < len {
            let mut chars = self.line_chars(self.row);
            chars.remove(self.col);
            self.lines[self.row] = chars.into_iter().collect();
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    /// Kill from the cursor to end of line (`ctrl+k`); at a line end, pull the
    /// next line up instead.
    pub fn kill_to_line_end(&mut self) {
        let len = self.line_len(self.row);
        if self.col < len {
            let chars = self.line_chars(self.row);
            self.lines[self.row] = chars[..self.col].iter().collect();
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line_len(self.row);
        }
    }

    pub fn move_right(&mut self) {
        if self.col < self.line_len(self.row) {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.line_len(self.row));
        } else {
            self.col = 0;
        }
    }

    pub fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.line_len(self.row));
        } else {
            self.col = self.line_len(self.row);
        }
    }

    pub fn move_line_start(&mut self) {
        self.col = 0;
    }

    pub fn move_line_end(&mut self) {
        self.col = self.line_len(self.row);
    }

    /// Word-jump left (`alt+left`): skip whitespace then the word run.
    pub fn move_word_left(&mut self) {
        if self.col == 0 {
            self.move_left();
            return;
        }
        let chars = self.line_chars(self.row);
        let mut i = self.col;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.col = i;
    }

    /// Word-jump right (`alt+right`): skip the word run then trailing
    /// whitespace.
    pub fn move_word_right(&mut self) {
        let chars = self.line_chars(self.row);
        if self.col >= chars.len() {
            self.move_right();
            return;
        }
        let mut i = self.col;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        self.col = i;
    }

    /// Wrap the buffer to `width` and return the visible slice plus the cursor
    /// position within it. `width` is the inner content width (>= 1);
    /// `height` is how many visual rows fit. The returned cursor `(x, y)` is
    /// relative to the slice's top-left; `y` is always within `0..height`.
    pub fn layout(&self, width: usize, height: usize) -> EditorLayout {
        let width = width.max(1);
        // Build visual rows and remember where the cursor lands.
        let mut visual: Vec<String> = Vec::new();
        let mut cursor_visual = (0usize, 0usize); // (x, row index)
        for (r, line) in self.lines.iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();
            let mut chunks: Vec<Vec<char>> = if chars.is_empty() {
                vec![Vec::new()]
            } else {
                chars.chunks(width).map(<[char]>::to_vec).collect()
            };
            // When the line ends exactly on a wrap boundary (len is a positive
            // multiple of width), append an empty trailing row so the caret at
            // line end wraps to the next visual row instead of overlapping the
            // last char.
            if !chars.is_empty() && chars.len() % width == 0 {
                chunks.push(Vec::new());
            }
            let chunk_count = chunks.len();
            for (ci, chunk) in chunks.into_iter().enumerate() {
                if r == self.row {
                    let start = ci * width;
                    let end = start + chunk.len();
                    // The cursor belongs to this chunk when its col falls in
                    // [start, end), or at end when this is the line's last
                    // chunk (caret sits just past the final char).
                    let is_last = ci + 1 == chunk_count;
                    if (self.col >= start && self.col < end)
                        || (is_last && self.col >= start && self.col <= end)
                    {
                        cursor_visual = (self.col - start, visual.len());
                    }
                }
                visual.push(chunk.into_iter().collect());
            }
        }
        // Vertical scroll so the cursor row stays visible (bottom-anchored).
        let total = visual.len();
        let start = if cursor_visual.1 >= height {
            cursor_visual.1 + 1 - height
        } else {
            0
        };
        let rows: Vec<String> = visual.into_iter().skip(start).take(height).collect();
        EditorLayout {
            rows,
            cursor_x: cursor_visual.0,
            cursor_y: cursor_visual.1.saturating_sub(start),
            total_rows: total,
        }
    }
}

/// Wrapped viewport slice for the input renderer.
pub struct EditorLayout {
    /// Visible visual rows (already wrapped to width, height-capped).
    pub rows: Vec<String>,
    /// Cursor column within its visual row.
    pub cursor_x: usize,
    /// Cursor row within `rows`.
    pub cursor_y: usize,
    /// Total wrapped rows (for sizing the input box).
    pub total_rows: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> TextArea {
        let mut ta = TextArea::default();
        ta.insert_str(text);
        ta
    }

    #[test]
    fn typing_and_text_roundtrip() {
        let ta = typed("hello");
        assert_eq!(ta.text(), "hello");
        assert_eq!(ta.cursor(), (0, 5));
        assert!(!ta.is_empty());
    }

    #[test]
    fn newline_splits_and_grows_vertically() {
        let mut ta = typed("ab");
        ta.move_left(); // between a|b
        ta.insert_newline();
        assert_eq!(ta.text(), "a\nb");
        assert_eq!(ta.cursor(), (1, 0));
        assert_eq!(ta.lines().len(), 2);
    }

    #[test]
    fn paste_with_newlines_inserts_multiple_lines() {
        let ta = typed("one\ntwo\nthree");
        assert_eq!(ta.lines(), &["one", "two", "three"]);
        assert_eq!(ta.cursor(), (2, 5));
    }

    #[test]
    fn backspace_joins_lines_at_line_start() {
        let mut ta = typed("a\nb");
        ta.move_line_start(); // row 1, col 0
        ta.backspace();
        assert_eq!(ta.text(), "ab");
        assert_eq!(ta.cursor(), (0, 1));
    }

    #[test]
    fn delete_forward_joins_next_line_at_line_end() {
        let mut ta = typed("a\nb");
        ta.move_up();
        ta.move_line_end(); // row 0, col 1 (end of "a")
        ta.delete_forward();
        assert_eq!(ta.text(), "ab");
        assert_eq!(ta.cursor(), (0, 1));
    }

    #[test]
    fn kill_to_line_end_truncates_then_pulls_up() {
        let mut ta = typed("hello world");
        ta.move_line_start();
        for _ in 0..5 {
            ta.move_right(); // after "hello"
        }
        ta.kill_to_line_end();
        assert_eq!(ta.text(), "hello");
        // At the line end now, another kill with a following line pulls it up.
        let mut ta = typed("a\nb");
        ta.move_up();
        ta.move_line_end();
        ta.kill_to_line_end();
        assert_eq!(ta.text(), "ab");
    }

    #[test]
    fn line_start_and_end_jumps() {
        let mut ta = typed("hello");
        ta.move_line_start();
        assert_eq!(ta.cursor(), (0, 0));
        ta.move_line_end();
        assert_eq!(ta.cursor(), (0, 5));
    }

    #[test]
    fn word_jumps_skip_whitespace_and_word_runs() {
        let mut ta = typed("foo bar baz");
        ta.move_line_end(); // col 11
        ta.move_word_left();
        assert_eq!(ta.cursor(), (0, 8)); // start of "baz"
        ta.move_word_left();
        assert_eq!(ta.cursor(), (0, 4)); // start of "bar"
        ta.move_word_right();
        assert_eq!(ta.cursor(), (0, 8)); // past "bar " to "baz"
    }

    #[test]
    fn vertical_motion_clamps_column() {
        let mut ta = typed("longline\nx");
        ta.move_up(); // back to row 0, col stays 1 (cursor was at (1,1))
        assert_eq!(ta.cursor().0, 0);
        ta.move_line_end(); // col 8
        ta.move_down(); // row 1 has len 1, clamp col to 1
        assert_eq!(ta.cursor(), (1, 1));
    }

    #[test]
    fn left_wraps_to_previous_line_end() {
        let mut ta = typed("ab\ncd");
        ta.move_up();
        ta.move_line_end(); // (0,2)
        ta.move_right(); // wraps to (1,0)
        assert_eq!(ta.cursor(), (1, 0));
        ta.move_left(); // back to (0,2)
        assert_eq!(ta.cursor(), (0, 2));
    }

    #[test]
    fn clear_resets_to_single_empty_line() {
        let mut ta = typed("a\nb\nc");
        ta.clear();
        assert!(ta.is_empty());
        assert_eq!(ta.cursor(), (0, 0));
        assert_eq!(ta.lines().len(), 1);
    }

    #[test]
    fn layout_wraps_and_places_cursor() {
        let ta = typed("abcde"); // cursor at end, col 5 (not a boundary)
        let layout = ta.layout(3, 5);
        // 5 chars at width 3 -> "abc", "de"; cursor just past "de".
        assert_eq!(layout.rows, vec!["abc", "de"]);
        assert_eq!((layout.cursor_x, layout.cursor_y), (2, 1));
        assert_eq!(layout.total_rows, 2);
    }

    #[test]
    fn layout_wraps_caret_to_next_row_at_an_exact_boundary() {
        let ta = typed("abcdef"); // len 6, width 3: ends exactly on a boundary
        let layout = ta.layout(3, 5);
        // A trailing empty row carries the caret so it never overlaps 'f'.
        assert_eq!(layout.rows, vec!["abc", "def", ""]);
        assert_eq!((layout.cursor_x, layout.cursor_y), (0, 2));
        assert_eq!(layout.total_rows, 3);
    }

    #[test]
    fn layout_scrolls_to_keep_cursor_visible() {
        let mut ta = TextArea::default();
        for i in 0..10 {
            ta.insert_str(&format!("line{i}"));
            if i < 9 {
                ta.insert_newline();
            }
        }
        // Cursor on the last line; a 3-row window must show the tail.
        let layout = ta.layout(20, 3);
        assert_eq!(layout.rows.len(), 3);
        assert!(layout.rows.last().unwrap().contains("line9"));
        assert_eq!(layout.cursor_y, 2);
    }

    #[test]
    fn layout_empty_buffer_has_one_row_and_origin_cursor() {
        let ta = TextArea::default();
        let layout = ta.layout(10, 3);
        assert_eq!(layout.rows, vec![""]);
        assert_eq!((layout.cursor_x, layout.cursor_y), (0, 0));
    }
}
