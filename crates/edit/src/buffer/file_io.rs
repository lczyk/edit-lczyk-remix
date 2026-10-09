//! File I/O: reading a file into the buffer (with encoding and
//! line-ending detection) and writing it back out.

use std::io::Read;

use super::*;

impl TextBuffer {
    /// Replaces the entire buffer contents with the given `text`.
    /// Assumes that the line count doesn't change.
    pub fn copy_from_str(&mut self, text: &dyn ReadableDocument) {
        if self.buffer.copy_from(text) {
            self.recalc_after_content_swap();
            self.cursor_move_to_logical(Point { x: CoordType::MAX, y: 0 });

            let delete = self.buffer.len() - self.cursor.offset;
            if delete != 0 {
                self.buffer
                    .apply(Edit::Delete { range: self.cursor.offset..self.cursor.offset + delete });
            }
        }
    }

    fn recalc_after_content_swap(&mut self) {
        // If the buffer was changed, nothing we previously saved can be relied upon.
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_history_type = HistoryType::Other;
        self.cursor = Default::default();
        self.set_selection(None);
        self.mark_as_clean();
        self.reflow();
        self.highlighter_cache.invalidate_from(0);
        self.gutter_marks.clear();
    }

    /// Copies the contents of the buffer into a string.
    pub fn save_as_string(&mut self, dst: &mut dyn WriteableDocument) {
        self.buffer.copy_into(dst);
        self.mark_as_clean();
    }

    /// Reads a UTF-8 file from disk into the text buffer.
    pub fn read_file(&mut self, file: &mut File) -> IoResult<()> {
        let size_hint = file.metadata().ok().map(|m| m.len() as usize);
        self.read_from(file, size_hint)
    }

    /// Reads UTF-8 text from any reader into the text buffer, replacing
    /// what was there. `size_hint` is the total length when known; a pipe
    /// or a socket passes `None`.
    pub fn read_from(&mut self, reader: &mut dyn Read, size_hint: Option<usize>) -> IoResult<()> {
        // TODO: Since reading the file can fail, we should ensure that we also reset the cursor here.
        // I don't do it, so that `recalc_after_content_swap()` works.
        self.buffer.clear();
        self.read_as_utf8(reader, size_hint)?;

        // Figure out
        // * the logical line count
        // * the newline type (LF or CRLF)
        // * the indentation type (tabs or spaces)
        // * whether there's a final newline
        {
            let chunk = self.read_forward(0);
            let mut offset = 0;
            let mut lines = 0;
            // Number of lines ending in CRLF.
            let mut crlf_count = 0;
            // Number of lines starting with a tab.
            let mut tab_indentations = 0;
            // Number of lines starting with a space.
            let mut space_indentations = 0;
            // Histogram of the indentation depth of lines starting with between 2 and 8 spaces.
            // In other words, `space_indentation_sizes[0]` is the number of lines starting with 2 spaces.
            let mut space_indentation_sizes = [0; 7];

            loop {
                // Check if the line starts with a tab.
                if offset < chunk.len() && chunk[offset] == b'\t' {
                    tab_indentations += 1;
                } else {
                    // Otherwise, check how many spaces the line starts with. Searching for >8 spaces
                    // allows us to reject lines that have more than 1 level of indentation.
                    let space_indentation =
                        chunk[offset..].iter().take(9).take_while(|&&c| c == b' ').count();

                    // We'll also reject lines starting with 1 space, because that's too fickle as a heuristic.
                    if (2..=8).contains(&space_indentation) {
                        space_indentations += 1;

                        // If we encounter an indentation depth of 6, it may either be a 6-space indentation,
                        // two 3-space indentation or 3 2-space indentations. To make this work, we increment
                        // all 3 possible histogram slots.
                        //   2 -> 2
                        //   3 -> 3
                        //   4 -> 4 2
                        //   5 -> 5
                        //   6 -> 6 3 2
                        //   7 -> 7
                        //   8 -> 8 4 2
                        space_indentation_sizes[space_indentation - 2] += 1;
                        if space_indentation & 4 != 0 {
                            space_indentation_sizes[0] += 1;
                        }
                        if space_indentation == 6 || space_indentation == 8 {
                            space_indentation_sizes[space_indentation / 2 - 2] += 1;
                        }
                    }
                }

                (offset, lines) = simd::lines_fwd(chunk, offset, lines, lines + 1);

                // Check if the preceding line ended in CRLF.
                if offset >= 2 && &chunk[offset - 2..offset] == b"\r\n" {
                    crlf_count += 1;
                }

                // We'll limit our heuristics to the first 1000 lines.
                // That should hopefully be enough in practice.
                if offset >= chunk.len() || lines >= 1000 {
                    break;
                }
            }

            // Assume CRLF if more than half the lines end in CRLF; empty file defaults to LF.
            let newlines_are_crlf = lines != 0 && crlf_count > lines / 2;

            // We'll assume tabs if there are more lines starting with tabs than with spaces.
            let indent_with_tabs = tab_indentations > space_indentations;
            let tab_size = if indent_with_tabs {
                // Tabs will get a visual size of 4 spaces by default.
                4
            } else {
                // Otherwise, we'll assume the most common indentation depth.
                // If there are conflicting indentation depths, we'll prefer the maximum, because in the loop
                // above we incremented the histogram slot for 2-spaces when encountering 4-spaces and so on.
                let mut max = 1;
                let mut tab_size = 4;
                for (i, &count) in space_indentation_sizes.iter().enumerate() {
                    if count >= max {
                        max = count;
                        tab_size = i as CoordType + 2;
                    }
                }
                tab_size
            };

            // If the file has more than 1000 lines, figure out how many are remaining.
            if offset < chunk.len() {
                (_, lines) = simd::lines_fwd(chunk, offset, lines, CoordType::MAX);
            }

            let final_newline = chunk.ends_with(b"\n");

            // Add 1, because the last line doesn't end in a newline (it ends in the literal end).
            self.stats.logical_lines = lines + 1;
            self.stats.visual_lines = self.stats.logical_lines;
            self.newlines_are_crlf = newlines_are_crlf;
            self.insert_final_newline = final_newline;
            self.indent_with_tabs = indent_with_tabs;
            self.tab_size = tab_size;
        }

        self.recalc_after_content_swap();
        Ok(())
    }

    fn read_as_utf8(&mut self, reader: &mut dyn Read, size_hint: Option<usize>) -> io::Result<()> {
        // Without a size hint the input may be a pipe or a socket.
        // Every read will have the same size until we hit the end.
        let mut chunk_size = 128 * KIBI;
        let mut extra_chunk_size = 128 * KIBI;

        if let Some(len) = size_hint {
            // Usually the next read of size `chunk_size` will read the entire input,
            // but if the size has changed for some reason, then `extra_chunk_size`
            // should be large enough to read the rest of it.
            // 4KiB is not too large and not too slow.
            chunk_size = len;
            extra_chunk_size = 4 * KIBI;
        }

        loop {
            let gap = self.buffer.reserve_gap(self.text_length(), chunk_size, 0);
            if gap.is_empty() {
                break;
            }

            let read = reader.read(gap)?;
            if read == 0 {
                break;
            }

            self.buffer.commit_gap(read);
            chunk_size = extra_chunk_size;
        }

        Ok(())
    }

    /// Writes the text buffer contents as UTF-8.
    pub fn write_file(&mut self, file: &mut File) -> IoResult<()> {
        let mut offset = 0;
        loop {
            let chunk = self.read_forward(offset);
            if chunk.is_empty() {
                break;
            }
            file.write_all(chunk)?;
            offset += chunk.len();
        }
        self.mark_as_clean();
        Ok(())
    }
}
