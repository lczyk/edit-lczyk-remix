//! Comment toggling.
//!
//! Mirrors vscode's behaviour: line comments align to the least-indented
//! non-blank line in the range and leave blank lines untouched; block
//! comments wrap the selection.

use super::*;

impl TextBuffer {
    /// Toggle line comments over the selection (or current line if no
    /// selection) using `token` as the line-comment marker (e.g. `"//"`).
    ///
    /// Mirrors vscode's `editor.action.commentLine`: if every non-blank line
    /// in the range already starts with `token`, strips it (plus one trailing
    /// space if present); otherwise inserts `token + " "` at the column of the
    /// least-indented non-blank line. Blank lines are left untouched.
    pub fn toggle_line_comment(&mut self, token: &str) {
        if self.read_only || token.is_empty() {
            return;
        }

        let token_bytes = token.as_bytes();
        let token_chars = token.chars().count() as CoordType;

        let saved_selection = self.selection;
        let saved_cursor = self.cursor;

        let (sel_beg_y, sel_end_y, mut sel_beg, mut sel_end) = match saved_selection {
            Some(s) => {
                let [y0, y1] = minmax(s.beg.y, s.end.y);
                (y0, y1, s.beg, s.end)
            }
            None => {
                let p = saved_cursor.logical_pos;
                (p.y, p.y, p, p)
            }
        };

        // Pass 1: walk the line range, decide direction + min indent _column_
        // (tab-aware visible column, mirroring vscode `_normalizeInsertionPoint`).
        // Don't mutate yet -- just measure.
        let mut any_non_blank = false;
        let mut all_commented = true;
        let mut min_indent_cols = CoordType::MAX;
        for y in sel_beg_y..=sel_end_y {
            self.cursor_move_to_logical(Point { x: 0, y });
            if self.cursor.logical_pos.y != y {
                break;
            }
            let line_start = self.cursor.offset;
            let (indent_chars, indent_cols) =
                self.measure_indent_internal(line_start, CoordType::MAX);
            self.cursor_move_to_logical(Point { x: indent_chars, y });
            let off = self.cursor.offset;
            if line_is_blank_after(self.read_forward(off)) {
                continue;
            }
            any_non_blank = true;
            if indent_cols < min_indent_cols {
                min_indent_cols = indent_cols;
            }
            if !self.starts_with_at(off, token_bytes) {
                all_commented = false;
            }
        }
        if !any_non_blank {
            return;
        }
        // Floor to the indent grid so insertion lands on a tab boundary even
        // for files with mixed tab+space leading whitespace.
        let insert_cols = min_indent_cols / self.tab_size * self.tab_size;

        // Clear the selection while we mutate so `write_canon`/`delete` don't
        // try to delete the whole selected range. Restored (with shifted xs)
        // at the end.
        self.set_selection(None);

        self.edit_begin_grouping();
        for y in sel_beg_y..=sel_end_y {
            self.cursor_move_to_logical(Point { x: 0, y });
            if self.cursor.logical_pos.y != y {
                break;
            }
            let line_start = self.cursor.offset;
            let (indent_chars, _) = self.measure_indent_internal(line_start, CoordType::MAX);
            self.cursor_move_to_logical(Point { x: indent_chars, y });
            let off = self.cursor.offset;
            if line_is_blank_after(self.read_forward(off)) {
                continue;
            }

            let delta;
            if all_commented {
                self.delete(CursorMovement::Grapheme, token_chars);
                let trailing_space = self.read_forward(self.cursor.offset).first() == Some(&b' ');
                if trailing_space {
                    self.delete(CursorMovement::Grapheme, 1);
                    delta = -(token_chars + 1);
                } else {
                    delta = -token_chars;
                }
            } else {
                // Per-line char offset that lands at `insert_cols` visible col.
                // `measure_indent_internal` stops before any tab that would
                // straddle the boundary, matching vscode's back-off branch.
                let (insert_chars, _) = self.measure_indent_internal(line_start, insert_cols);
                self.cursor_move_to_logical(Point { x: insert_chars, y });
                let mut buf = Vec::with_capacity(token_bytes.len() + 1);
                buf.extend_from_slice(token_bytes);
                buf.push(b' ');
                self.write_canon(&buf);
                delta = token_chars + 1;
            }

            if y == sel_beg.y {
                sel_beg.x = (sel_beg.x + delta).max(0);
            }
            if y == sel_end.y {
                sel_end.x = (sel_end.x + delta).max(0);
            }
        }
        self.edit_end_grouping();

        // Restore selection (shifted) and place the cursor at its end, mirroring
        // `indent_change`.
        let restored_cursor_pos = if saved_cursor.logical_pos.y == sel_end.y {
            sel_end
        } else if saved_cursor.logical_pos.y == sel_beg.y {
            sel_beg
        } else {
            saved_cursor.logical_pos
        };
        self.set_cursor_internal(
            self.cursor_move_to_logical_internal(self.cursor, restored_cursor_pos),
        );
        self.set_selection(
            saved_selection.map(|_| TextBufferSelection { beg: sel_beg, end: sel_end }),
        );
    }

    /// Toggle per-line block comments over the selection (or current line).
    /// Used as a `Cmd+/` fallback for languages with no line-comment syntax
    /// (markdown, html, xml). Each non-blank line gets `open content close`
    /// inserted/stripped individually.
    pub fn toggle_per_line_block_comment(&mut self, open: &str, close: &str) {
        if self.read_only || open.is_empty() || close.is_empty() {
            return;
        }

        let open_bytes = open.as_bytes();
        let close_bytes = close.as_bytes();
        let open_chars = open.chars().count() as CoordType;
        let close_chars = close.chars().count() as CoordType;

        let saved_selection = self.selection;
        let saved_cursor = self.cursor;

        let (sel_beg_y, sel_end_y, mut sel_beg, mut sel_end) = match saved_selection {
            Some(s) => {
                let [y0, y1] = minmax(s.beg.y, s.end.y);
                (y0, y1, s.beg, s.end)
            }
            None => {
                let p = saved_cursor.logical_pos;
                (p.y, p.y, p, p)
            }
        };

        // Pass 1: decide direction. A line is "wrapped" iff its content (after
        // leading ws, before trailing ws) starts with `open` and ends with
        // `close`.
        let mut any_non_blank = false;
        let mut all_wrapped = true;
        for y in sel_beg_y..=sel_end_y {
            let Some(info) = self.scan_line_extents(y) else {
                continue;
            };
            any_non_blank = true;
            // NOTE: length guard keeps the open/close matches from
            // overlapping (e.g. "<!-->"), which would make the strip path
            // delete past the end of the line.
            if info.content_end - info.content_start < open_bytes.len() + close_bytes.len()
                || !self.range_starts_with(info.content_start, info.content_end, open_bytes)
                || !self.range_ends_with(info.content_start, info.content_end, close_bytes)
            {
                all_wrapped = false;
            }
        }
        if !any_non_blank {
            return;
        }

        self.set_selection(None);
        self.edit_begin_grouping();

        for y in sel_beg_y..=sel_end_y {
            let Some(info) = self.scan_line_extents(y) else {
                continue;
            };

            let (beg_delta, end_delta) = if all_wrapped {
                // Strip trailing close (+ optional preceding space).
                self.cursor_move_to_logical(Point { x: info.content_end_chars, y });
                self.delete(CursorMovement::Grapheme, -close_chars);
                let mut close_strip = close_chars;
                if self.read_backward(self.cursor.offset).last() == Some(&b' ') {
                    self.delete(CursorMovement::Grapheme, -1);
                    close_strip += 1;
                }
                // Strip leading open (+ optional trailing space).
                self.cursor_move_to_logical(Point { x: info.indent_chars, y });
                self.delete(CursorMovement::Grapheme, open_chars);
                let mut open_strip = open_chars;
                if self.read_forward(self.cursor.offset).first() == Some(&b' ') {
                    self.delete(CursorMovement::Grapheme, 1);
                    open_strip += 1;
                }
                (-open_strip, -(open_strip + close_strip))
            } else {
                // Skip tokens this line already has (e.g. a line straddling
                // a pre-existing multi-line comment) so toggling never
                // doubles them up.
                let has_open =
                    self.range_starts_with(info.content_start, info.content_end, open_bytes);
                let has_close =
                    self.range_ends_with(info.content_start, info.content_end, close_bytes);

                let mut end_delta_local = 0;
                if !has_close {
                    // Append " close" at end of content.
                    self.cursor_move_to_logical(Point { x: info.content_end_chars, y });
                    let mut tail = Vec::with_capacity(close_bytes.len() + 1);
                    tail.push(b' ');
                    tail.extend_from_slice(close_bytes);
                    self.write_canon(&tail);
                    end_delta_local += close_chars + 1;
                }
                let mut beg_delta_local = 0;
                if !has_open {
                    // Insert "open " at indent end.
                    self.cursor_move_to_logical(Point { x: info.indent_chars, y });
                    let mut head = Vec::with_capacity(open_bytes.len() + 1);
                    head.extend_from_slice(open_bytes);
                    head.push(b' ');
                    self.write_canon(&head);
                    beg_delta_local = open_chars + 1;
                }
                (beg_delta_local, beg_delta_local + end_delta_local)
            };

            if y == sel_beg.y {
                sel_beg.x = (sel_beg.x + beg_delta).max(0);
            }
            if y == sel_end.y {
                sel_end.x = (sel_end.x + end_delta).max(0);
            }
        }
        self.edit_end_grouping();

        let restored_cursor_pos = if saved_cursor.logical_pos.y == sel_end.y {
            sel_end
        } else if saved_cursor.logical_pos.y == sel_beg.y {
            sel_beg
        } else {
            saved_cursor.logical_pos
        };
        self.set_cursor_internal(
            self.cursor_move_to_logical_internal(self.cursor, restored_cursor_pos),
        );
        self.set_selection(
            saved_selection.map(|_| TextBufferSelection { beg: sel_beg, end: sel_end }),
        );
    }

    /// Toggle a single block-comment pair around the current selection (or
    /// current line if no selection). Triggered from the Edit menu only --
    /// no keyboard shortcut.
    pub fn toggle_block_comment(&mut self, open: &str, close: &str) {
        if self.read_only || open.is_empty() || close.is_empty() {
            return;
        }

        let open_bytes = open.as_bytes();
        let close_bytes = close.as_bytes();
        let open_chars = open.chars().count() as CoordType;
        let close_chars = close.chars().count() as CoordType;

        let saved_selection = self.selection;
        let saved_cursor = self.cursor;

        // Determine the byte range we'll toggle around. If there's a
        // selection use it; otherwise span the full content of the current
        // line.
        let (mut beg_pos, mut end_pos) = match saved_selection {
            Some(s) => {
                let [b, e] = minmax(s.beg, s.end);
                (b, e)
            }
            None => {
                let y = saved_cursor.logical_pos.y;
                let Some(info) = self.scan_line_extents(y) else {
                    return;
                };
                (Point { x: info.indent_chars, y }, Point { x: info.content_end_chars, y })
            }
        };

        self.cursor_move_to_logical(beg_pos);
        let beg_off = self.cursor.offset;
        self.cursor_move_to_logical(end_pos);
        let end_off = self.cursor.offset;
        if beg_off >= end_off {
            return;
        }

        // Toggle: if the selected range is exactly `open ... close` (allowing
        // one optional space on each inner side), strip it. Otherwise wrap.
        // NOTE: length guard keeps the open/close matches from overlapping
        // (e.g. "<!-->"), which would make the strip path delete past the
        // end of the range.
        let has_open = self.range_starts_with(beg_off, end_off, open_bytes);
        let has_close = self.range_ends_with(beg_off, end_off, close_bytes);
        let wrapped =
            end_off - beg_off >= open_bytes.len() + close_bytes.len() && has_open && has_close;

        self.set_selection(None);
        self.edit_begin_grouping();

        if wrapped {
            // Strip trailing close (+ optional space).
            self.cursor_move_to_logical(end_pos);
            self.delete(CursorMovement::Grapheme, -close_chars);
            let mut close_strip = close_chars;
            if self.read_backward(self.cursor.offset).last() == Some(&b' ') {
                self.delete(CursorMovement::Grapheme, -1);
                close_strip += 1;
            }
            // Strip leading open (+ optional space).
            self.cursor_move_to_logical(beg_pos);
            self.delete(CursorMovement::Grapheme, open_chars);
            let mut open_strip = open_chars;
            if self.read_forward(self.cursor.offset).first() == Some(&b' ') {
                self.delete(CursorMovement::Grapheme, 1);
                open_strip += 1;
            }
            beg_pos.x = (beg_pos.x).max(0);
            end_pos.x = if beg_pos.y == end_pos.y {
                (end_pos.x - (open_strip + close_strip)).max(beg_pos.x)
            } else {
                (end_pos.x - close_strip).max(0)
            };
        } else {
            // Skip tokens the range already has on one side so toggling
            // never doubles them up.
            if !has_close {
                // Append close + space at end.
                self.cursor_move_to_logical(end_pos);
                let mut tail = Vec::with_capacity(close_bytes.len() + 1);
                tail.push(b' ');
                tail.extend_from_slice(close_bytes);
                self.write_canon(&tail);
            }
            if !has_open {
                // Insert open + space at start.
                self.cursor_move_to_logical(beg_pos);
                let mut head = Vec::with_capacity(open_bytes.len() + 1);
                head.extend_from_slice(open_bytes);
                head.push(b' ');
                self.write_canon(&head);
                // Shift end if it sits on the same line as beg.
                if beg_pos.y == end_pos.y {
                    end_pos.x += open_chars + 1;
                }
            }
        }
        self.edit_end_grouping();

        let restored_cursor_pos =
            if saved_cursor.logical_pos == end_pos || saved_cursor.logical_pos.y == end_pos.y {
                end_pos
            } else {
                beg_pos
            };
        self.set_cursor_internal(
            self.cursor_move_to_logical_internal(self.cursor, restored_cursor_pos),
        );
        self.set_selection(
            saved_selection.map(|_| TextBufferSelection { beg: beg_pos, end: end_pos }),
        );
    }

    /// Returns the indent + trimmed-content extents for line `y`, or `None`
    /// if the line is blank (zero non-whitespace characters) or out of range.
    fn scan_line_extents(&mut self, y: CoordType) -> Option<LineExtents> {
        self.cursor_move_to_logical(Point { x: 0, y });
        if self.cursor.logical_pos.y != y {
            return None;
        }
        let line_start = self.cursor.offset;
        let (indent_chars, _) = self.measure_indent_internal(line_start, CoordType::MAX);
        self.cursor_move_to_logical(Point { x: indent_chars, y });
        let content_start = self.cursor.offset;
        if line_is_blank_after(self.read_forward(content_start)) {
            return None;
        }
        // Walk to end of line and back-trim trailing whitespace.
        self.cursor_move_to_logical(Point { x: CoordType::MAX, y });
        let mut content_end = self.cursor.offset;
        let mut content_end_chars = self.cursor.logical_pos.x;
        while content_end > content_start {
            let chunk = self.read_backward(content_end);
            let Some(&last) = chunk.last() else { break };
            if last == b' ' || last == b'\t' {
                content_end -= 1;
                content_end_chars -= 1;
            } else {
                break;
            }
        }
        Some(LineExtents { indent_chars, content_start, content_end, content_end_chars })
    }

    fn range_starts_with(&self, beg: usize, end: usize, needle: &[u8]) -> bool {
        if end - beg < needle.len() {
            return false;
        }
        let mut off = beg;
        let mut i = 0;
        while i < needle.len() {
            let chunk = self.read_forward(off);
            if chunk.is_empty() {
                return false;
            }
            let take = chunk.len().min(needle.len() - i);
            if chunk[..take] != needle[i..i + take] {
                return false;
            }
            i += take;
            off += take;
        }
        true
    }

    fn range_ends_with(&self, beg: usize, end: usize, needle: &[u8]) -> bool {
        if end - beg < needle.len() {
            return false;
        }
        let mut off = end;
        let mut i = needle.len();
        while i > 0 {
            let chunk = self.read_backward(off);
            if chunk.is_empty() {
                return false;
            }
            let take = chunk.len().min(i);
            if chunk[chunk.len() - take..] != needle[i - take..i] {
                return false;
            }
            i -= take;
            off -= take;
        }
        true
    }

    fn starts_with_at(&self, mut offset: usize, needle: &[u8]) -> bool {
        let mut i = 0;
        while i < needle.len() {
            let chunk = self.read_forward(offset);
            if chunk.is_empty() {
                return false;
            }
            let take = chunk.len().min(needle.len() - i);
            if chunk[..take] != needle[i..i + take] {
                return false;
            }
            i += take;
            offset += take;
        }
        true
    }

    pub(super) fn measure_indent_internal(
        &self,
        mut offset: usize,
        max_columns: CoordType,
    ) -> (CoordType, CoordType) {
        let mut chars = 0;
        let mut columns = 0;

        'outer: loop {
            let chunk = self.read_forward(offset);
            if chunk.is_empty() {
                break;
            }

            for &c in chunk {
                let next = match c {
                    b' ' => columns + 1,
                    b'\t' => columns + self.tab_size_eval(columns),
                    _ => break 'outer,
                };
                if next > max_columns {
                    break 'outer;
                }
                chars += 1;
                columns = next;
            }

            offset += chunk.len();

            // No need to do another round if we
            // already got the exact right amount.
            if columns >= max_columns {
                break;
            }
        }

        (chars, columns)
    }

    /// Builds the tinted column spans for one logical line of the
    /// line-move sweep band. One span per line, running from the first
    /// non-whitespace column to just past the last -- leading indent
    /// and trailing whitespace stay untinted. Mid-line whitespace
    /// (gaps between tokens) is included; those cells have no
    /// explicit fg so the renderer tints them with the theme's
    /// foreground colour. Empty / whitespace-only lines collapse to a
    /// single full-width span so blank rows still flash. Whitespace
    /// recognised: ASCII space + tab (not unicode U+00A0 NBSP etc).
    pub(super) fn compute_line_band(&self, y: CoordType) -> RowBand {
        let line_start = self.goto_line_start(self.cursor, y);
        let next_line = self.cursor_move_to_logical_internal(line_start, Point { x: 0, y: y + 1 });
        let line_off = line_start.offset;
        let line_end = next_line.offset;

        // Read the line into a contiguous buffer. Strips the line
        // terminator(s) so spans stop at the last content byte rather
        // than running into the newline column.
        let mut bytes: Vec<u8> = Vec::new();
        let mut o = line_off;
        while o < line_end {
            let chunk = self.read_forward(o);
            if chunk.is_empty() {
                break;
            }
            let take = (line_end - o).min(chunk.len());
            bytes.extend_from_slice(&chunk[..take]);
            o += take;
        }
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }

        let is_ws = |b: u8| b == b' ' || b == b'\t';
        // Empty / whitespace-only line -> single full-width span. The
        // cells have no explicit fg, so the renderer's tint falls back
        // to `IndexedColor::Foreground` and the row flashes in the
        // default text colour. `text_width()` already accounts for the
        // gutter / margin.
        if bytes.iter().all(|&b| is_ws(b)) {
            return Box::new([(0, self.text_width())]);
        }

        // First and last non-whitespace byte offsets in the line.
        // Whitespace checks only fire on ASCII bytes (0x20 / 0x09); UTF-8
        // continuation bytes (0x80..0xBF) are non-ws, so the bounds
        // never land inside a multi-byte glyph.
        let first = bytes.iter().position(|&b| !is_ws(b)).unwrap();
        let last = bytes.iter().rposition(|&b| !is_ws(b)).unwrap();

        // Convert byte offsets to visual columns. The right bound steps
        // one grapheme past the last non-ws byte so the band covers the
        // whole final glyph (incl. multi-byte / wide chars).
        let at_first = self.cursor_move_to_offset_internal(line_start, line_off + first);
        let at_last = self.cursor_move_to_offset_internal(at_first, line_off + last);
        let past_last = self.cursor_move_delta_internal(at_last, CursorMovement::Grapheme, 1);
        let l = at_first.visual_pos.x;
        let r = past_last.visual_pos.x;
        if r > l { Box::new([(l, r)]) } else { Box::new([]) }
    }
}
