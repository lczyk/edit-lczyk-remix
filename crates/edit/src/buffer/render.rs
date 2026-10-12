//! Render pass 1: walk the visible rows and build the per-row IR.
//!
//! Produces the owned types in [`super::layout`] -- body text, selection
//! and shadow-match rects, whitespace and control-character visualisers,
//! syntax-highlight spans clipped to each row. Nothing here paints; pass 2
//! (`crate::paint::draw`) consumes the IR and writes into the framebuffer.
//!
//! Splitting it this way is what lets the IR outlive the scratch arena
//! that built it, and lets pass 1 be tested without a framebuffer.

use super::*;

/// One coloured span of a logical line, in absolute byte offsets, from
/// whichever of the highlighter and the pinned spans is in charge.
struct MarkupSpan {
    start: usize,
    end: usize,
    color: Option<IndexedColor>,
    attr: Option<Attributes>,
}

/// The highlighter's spans of one line as markup, `Other` dropped.
fn lsh_markup(spans: &[Highlight<HighlightKind>], out: &mut Vec<MarkupSpan>) {
    for pair in spans.windows(2) {
        let (curr, next) = (&pair[0], &pair[1]);
        if curr.kind == HighlightKind::Other {
            continue;
        }
        let attr = match curr.kind {
            HighlightKind::MarkupBold => Some(Attributes::Bold),
            HighlightKind::MarkupItalic => Some(Attributes::Italic),
            HighlightKind::MarkupLink => Some(Attributes::Underlined),
            HighlightKind::MarkupStrikethrough => Some(Attributes::Strikethrough),
            _ => None,
        };
        out.push(MarkupSpan {
            start: curr.start,
            end: next.start,
            color: highlight_kind_color(curr.kind),
            attr,
        });
    }
}

/// How far out a row's width is measured for the horizontal scroll bound
/// (see [`TextareaLayout::visual_pos_x_max`]). One line can be the whole
/// file, so the walk stops here and the width is reported as unknown.
const MAX_MEASURED_COLUMN: CoordType = 4096;

impl TextBuffer {
    /// Builds the per-row margin prefix (line numbers + separator,
    /// or wrap-marker dots) into `line` and returns the per-row
    /// gutter classification: which gutter mark (if any) should be
    /// painted for this row, and whether the row's margin column
    /// needs the wrapped-continuation dim.
    ///
    /// `visual_line` is the document-visual y; `cursor_beg` is the
    /// row's start cursor; `in_conflict` is what the highlighter said about
    /// the row's logical line, and outranks the diff mark. Caller must have
    /// already gated on `line_number_width != 0`.
    fn build_gutter_margin<'arena>(
        &self,
        line: &mut BString<'arena>,
        scratch: &'arena Arena,
        line_number_width: usize,
        visual_line: CoordType,
        cursor_beg: &Cursor,
        in_conflict: bool,
    ) -> (Option<GutterMark>, bool) {
        let mark = if in_conflict {
            GutterMark::Conflict
        } else {
            self.gutter_mark(cursor_beg.logical_pos.y)
        };
        if visual_line >= self.stats.visual_lines {
            // Past the end of the buffer? Place "    | " in the margin.
            // Since we know that we won't see line numbers greater than i64::MAX (9223372036854775807)
            // any time soon, we can use a static string as the template (`MARGIN`) and slice it,
            // because `line_number_width` can't possibly be larger than 19.
            let off = 19 - line_number_width;
            let template = margin_template();
            unsafe { std::hint::assert_unchecked(off < template.len()) };
            line.push_str(scratch, &template[off..]);
            (None, false)
        } else if self.word_wrap_column <= 0 || cursor_beg.logical_pos.x == 0 {
            // Regular line? Place "123 | " in the margin.
            let sep = crate::glyphs::box_v();
            arena_write_fmt!(
                scratch,
                line,
                "{:1$} {2} ",
                cursor_beg.logical_pos.y + 1,
                line_number_width,
                sep
            );
            (if mark != GutterMark::None { Some(mark) } else { None }, false)
        } else {
            // Wrapped line? Place " ... | " in the margin.
            let number_width = (cursor_beg.logical_pos.y + 1).ilog10() as usize + 1;
            let sep = crate::glyphs::box_v();
            if crate::glyphs::ascii_only() {
                arena_write_fmt!(
                    scratch,
                    line,
                    "{0:1$}{0:.<2$} {3} ",
                    "",
                    line_number_width - number_width,
                    number_width,
                    sep
                );
            } else {
                arena_write_fmt!(
                    scratch,
                    line,
                    "{0:1$}{0:\u{2219}<2$} {3} ",
                    "",
                    line_number_width - number_width,
                    number_width,
                    sep
                );
            }
            // Added/Modified/Conflict colour the whole logical line's bar,
            // and the boundary DeletedBelow flag sits under the line's last
            // row -- the caller walks it down. DeletedAbove is the one arrow
            // that belongs to the first row alone.
            let mark = match mark {
                m @ (GutterMark::Added
                | GutterMark::Modified
                | GutterMark::DeletedBelow
                | GutterMark::Conflict) => Some(m),
                GutterMark::DeletedAbove | GutterMark::None => None,
            };
            (mark, true)
        }
    }

    /// Computes the per-row selection geometry: the byte-offset
    /// sub-range of the line that is selected (used as a visualiser
    /// mask for whitespace inside selections), and the visual rect
    /// to paint as the selection band.
    ///
    /// Returns `(selection_off, Some(rect))` when the selection
    /// touches this row, or `(0..0, None)` otherwise. `rect` is in
    /// framebuffer coordinates.
    ///
    /// `cursor_override` + `cursor_visual_render` pin the active
    /// edge of the selection to the animated cursor's exact x on
    /// the cursor's row -- without that the selection ends at the
    /// line's last logical column when the animated x is past it,
    /// lagging the visible cursor on short lines.
    #[allow(clippy::too_many_arguments)]
    fn build_selection_row(
        &self,
        cursor_beg: &Cursor,
        cursor_end: &Cursor,
        selection_beg: Point,
        selection_end: Point,
        visual_line: CoordType,
        cursor_override: Option<Point>,
        cursor_visual_render: Point,
        selection_active_is_end: bool,
        destination: Rect,
        origin: Point,
        text_width: CoordType,
        y: CoordType,
    ) -> (Range<usize>, Option<Rect>) {
        let mut selection_off = 0..0;
        if !(cursor_beg.visual_pos.y == visual_line
            && selection_beg <= cursor_end.logical_pos
            && selection_end >= cursor_beg.logical_pos)
        {
            return (selection_off, None);
        }

        let mut cursor = *cursor_beg;

        // By default, we assume the entire line is selected.
        let mut selection_pos_beg = 0;
        let mut selection_pos_end = COORD_TYPE_SAFE_MAX;
        selection_off.start = cursor_beg.offset;
        selection_off.end = cursor_end.offset;

        // The start of the selection is within this line. We need to update selection_beg.
        if selection_beg <= cursor_end.logical_pos && selection_beg >= cursor_beg.logical_pos {
            cursor = self.cursor_move_to_logical_internal(cursor, selection_beg);
            selection_off.start = cursor.offset;
            selection_pos_beg = cursor.visual_pos.x;
        }

        // The end of the selection is within this line. We need to update selection_end.
        if selection_end <= cursor_end.logical_pos && selection_end >= cursor_beg.logical_pos {
            cursor = self.cursor_move_to_logical_internal(cursor, selection_end);
            selection_off.end = cursor.offset;
            selection_pos_end = cursor.visual_pos.x;
        }

        // On the cursor's animated row, pin the active edge to the
        // anim cursor's exact visual x. Without this, selection ends
        // at the line's last logical column when anim x is past it,
        // which lags the visible cursor on short lines.
        if cursor_override.is_some() && cursor_visual_render.y == visual_line {
            if selection_active_is_end {
                selection_pos_end = cursor_visual_render.x;
            } else {
                selection_pos_beg = cursor_visual_render.x;
            }
        }

        let left = destination.left + self.margin_width - origin.x;
        let top = destination.top + y;
        let rect = Rect {
            left: left + selection_pos_beg.max(origin.x),
            top,
            right: left + selection_pos_end.min(origin.x + text_width),
            bottom: top + 1,
        };
        (selection_off, Some(rect))
    }

    /// Finds literal occurrences of the current selection's text
    /// that fall on this visual row, and returns their visual rects
    /// for the caller to paint as shadow-match highlights.
    ///
    /// Excludes the user's actual selection (matched by absolute
    /// offset, not by content) so the real selection paints over
    /// just once. Returns an empty vec when there are no matches
    /// on this row, or when the row sits outside the line that
    /// holds the active selection.
    #[allow(clippy::too_many_arguments)]
    fn build_shadow_matches_row(
        &self,
        needle: &[u8],
        sel_beg: usize,
        sel_end: usize,
        cursor_beg: &Cursor,
        cursor_end: &Cursor,
        visual_line: CoordType,
        destination: Rect,
        origin: Point,
        y: CoordType,
    ) -> Vec<Rect> {
        let mut out = Vec::new();
        if !(cursor_beg.visual_pos.y == visual_line && cursor_beg.offset < cursor_end.offset) {
            return out;
        }
        let line_beg = cursor_beg.offset;
        let line_end = cursor_end.offset;
        // Extend the scan window to catch matches that cross the left edge.
        let scan_beg = line_beg.saturating_sub(needle.len().saturating_sub(1));
        let mut haystack = Vec::new();
        self.buffer.extract_raw(scan_beg..line_end, &mut haystack, 0);

        let mut i = 0;
        while i + needle.len() <= haystack.len() {
            if &haystack[i..i + needle.len()] != needle {
                i += 1;
                continue;
            }
            let match_beg = scan_beg + i;
            let match_end = match_beg + needle.len();
            i += 1;

            // Keep only matches whose visible portion lies on this line.
            if match_end <= line_beg || match_beg >= line_end {
                continue;
            }
            // Skip the user's actual selection.
            if match_beg == sel_beg && match_end == sel_end {
                continue;
            }

            let mb = self.cursor_move_to_offset_internal(*cursor_beg, match_beg.max(line_beg));
            let me = self.cursor_move_to_offset_internal(mb, match_end.min(line_end));
            let left = destination.left + self.margin_width - origin.x;
            let top = destination.top + y;
            out.push(Rect {
                left: left + mb.visual_pos.x,
                top,
                right: left + me.visual_pos.x,
                bottom: top + 1,
            });
        }
        out
    }

    /// Build per-row markup rects: foreground colours + attribute blits
    /// (bold / italic / underline / strikethrough), each clipped to the
    /// visual row's actual text extent. Intersects every span with
    /// `[cursor_beg.offset, cursor_end.offset)` and emits one rect per
    /// intersecting span -- so wrapped continuation rows never paint past
    /// their wrap column (the bug the old `render_apply_highlights` path
    /// had with `COORD_TYPE_SAFE_MAX`).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::type_complexity)]
    fn build_markup_row(
        &self,
        cursor_beg: &Cursor,
        cursor_end: &Cursor,
        visual_line: CoordType,
        spans: &[MarkupSpan],
        destination: Rect,
        origin: Point,
        y: CoordType,
    ) -> (Vec<(Rect, IndexedColor)>, Vec<(Rect, Attributes)>) {
        let mut fg_rects: Vec<(Rect, IndexedColor)> = Vec::new();
        let mut attr_rects: Vec<(Rect, Attributes)> = Vec::new();

        if cursor_beg.visual_pos.y != visual_line || cursor_beg.offset >= cursor_end.offset {
            return (fg_rects, attr_rects);
        }
        let line_beg = cursor_beg.offset;
        let line_end = cursor_end.offset;
        let row_end_x = cursor_end.visual_pos.x;
        let text_left = destination.left + self.margin_width;
        let text_right = destination.right;
        let top = destination.top + y;

        for span in spans {
            // Drop spans entirely outside this row's visible byte range.
            if span.end <= line_beg || span.start >= line_end {
                continue;
            }
            let clip_beg = span.start.max(line_beg);
            let clip_end = span.end.min(line_end);
            if clip_beg >= clip_end {
                continue;
            }

            let mb = self.cursor_move_to_offset_internal(*cursor_beg, clip_beg);
            let me = self.cursor_move_to_offset_internal(mb, clip_end);
            // If a clamped cursor walks past the wrap boundary onto the
            // next visual row, treat its visual_x as this row's actual
            // end -- the rect must stay on `visual_line`.
            let mb_x = if mb.visual_pos.y == visual_line { mb.visual_pos.x } else { row_end_x };
            let me_x = if me.visual_pos.y == visual_line { me.visual_pos.x } else { row_end_x };

            let screen_left = (text_left + mb_x - origin.x).max(text_left);
            let screen_right = (text_left + me_x - origin.x).min(text_right);
            if screen_left >= screen_right {
                continue;
            }
            let rect = Rect { left: screen_left, top, right: screen_right, bottom: top + 1 };

            if let Some(color) = span.color {
                fg_rects.push((rect, color));
            }
            if let Some(attr) = span.attr {
                attr_rects.push((rect, attr));
            }
        }

        (fg_rects, attr_rects)
    }

    /// The pinned spans of logical line `logical_y` as absolute byte
    /// spans. `cursor` is any cursor on that line.
    fn pinned_markup(
        &self,
        pinned: &[Vec<PinnedSpan>],
        cursor: &Cursor,
        logical_y: CoordType,
        out: &mut Vec<MarkupSpan>,
    ) {
        let Some(spans) = pinned.get(logical_y as usize) else { return };
        let line_start = self.goto_line_start(*cursor, logical_y).offset;
        out.extend(spans.iter().map(|s| MarkupSpan {
            start: line_start + s.range.start,
            end: line_start + s.range.end,
            color: s.color,
            attr: (s.attr != Attributes::None).then_some(s.attr),
        }));
    }

    /// Encodes the body text for a single visual row into `line`:
    /// the on-screen text after the gutter margin. Handles the
    /// left-edge wide-glyph overlap, tab expansion, whitespace
    /// visualisers for the selected region, and U+2400-range
    /// pictures for C0 / C1 control bytes. Paints the per-cell
    /// visualiser rects directly into `fb` via `paint::draw`.
    ///
    /// `selection_off` is the byte sub-range produced by
    /// `build_selection_row`; used as a mask for whitespace
    /// visualisers.
    #[allow(clippy::too_many_arguments)]
    fn build_body_text<'arena>(
        &self,
        line: &mut BString<'arena>,
        scratch: &'arena Arena,
        cursor_beg_in: Cursor,
        cursor_end: &Cursor,
        selection_off: Range<usize>,
        destination: Rect,
        origin: Point,
    ) -> BodyTextRects {
        let mut whitespace_visualizers: Vec<Rect> = Vec::new();
        let mut control_chars: Vec<Rect> = Vec::new();
        let mut cursor_beg = cursor_beg_in;
        // If we couldn't reach the left edge, we may have stopped short due to a wide glyph.
        // In that case we'll try to find the next character and then compute by how many
        // columns it overlaps the left edge (can be anything between 1 and 7).
        if cursor_beg.visual_pos.x < origin.x {
            let cursor_next = self.cursor_move_to_logical_internal(
                cursor_beg,
                Point { x: cursor_beg.logical_pos.x + 1, y: cursor_beg.logical_pos.y },
            );

            if cursor_next.visual_pos.x > origin.x {
                let overlap = cursor_next.visual_pos.x - origin.x;
                crate::sanity_check!(
                    tab_overlap_range,
                    (1..=7).contains(&overlap),
                    "overlap={} (expected 1..=7)",
                    overlap
                );
                line.push_str(scratch, &tab_whitespace()[..overlap as usize]);
                cursor_beg = cursor_next;
            }
        }

        let mut visualizer_buf = [0xE2, 0x90, 0x80]; // U+2400 in UTF8
        let mut global_off = cursor_beg.offset;
        let mut cursor_line = cursor_beg;

        while global_off < cursor_end.offset {
            let chunk = self.read_forward(global_off);
            let chunk = &chunk[..chunk.len().min(cursor_end.offset - global_off)];
            let mut it = Utf8Chars::new(chunk, 0);

            // TODO(perf): Looping char-by-char is bad for performance.
            // >25% of the total rendering time is spent here.
            loop {
                let chunk_off = it.offset();
                let global_off = global_off + chunk_off;
                let Some(ch) = it.next() else {
                    break;
                };

                if ch == ' ' || ch == '\t' {
                    let is_tab = ch == '\t';
                    let visualize = selection_off.contains(&global_off);
                    let mut whitespace = tab_whitespace();
                    let mut prefix_add = 0;

                    if is_tab || visualize {
                        // We need the character's visual position in order to either compute the tab size,
                        // or set the foreground color of the visualizer, respectively.
                        // TODO(perf): Doing this char-by-char is of course also bad for performance.
                        cursor_line = self.cursor_move_to_offset_internal(cursor_line, global_off);
                    }

                    let tab_size = if is_tab { self.tab_size_eval(cursor_line.column) } else { 1 };

                    if visualize {
                        // If the whitespace is part of the selection,
                        // we replace " " with the central dot and "\t" with the rightward arrow.
                        (whitespace, prefix_add) =
                            if is_tab { visual_tab() } else { visual_space() };

                        // Make the visualized characters slightly gray.
                        let visualizer_rect = {
                            let left =
                                destination.left + self.margin_width + cursor_line.visual_pos.x
                                    - origin.x;
                            let top = destination.top + cursor_line.visual_pos.y - origin.y;
                            Rect { left, top, right: left + 1, bottom: top + 1 }
                        };
                        whitespace_visualizers.push(visualizer_rect);
                    }

                    line.push_str(scratch, &whitespace[..prefix_add + tab_size as usize]);
                } else if ch <= '\x1f' || ('\u{7f}'..='\u{9f}').contains(&ch) {
                    // Append a Unicode representation of the C0 or C1 control character.
                    visualizer_buf[2] = if ch <= '\x1f' {
                        0x80 | ch as u8 // U+2400..=U+241F
                    } else if ch == '\x7f' {
                        0xA1 // U+2421
                    } else {
                        0xA6 // U+2426, because there are no pictures for C1 control characters.
                    };

                    // Our manually constructed UTF8 is never going to be invalid. Trust.
                    line.push_str(scratch, unsafe { str::from_utf8_unchecked(&visualizer_buf) });

                    // Highlight the control character yellow.
                    cursor_line = self.cursor_move_to_offset_internal(cursor_line, global_off);
                    let visualizer_rect = {
                        let left = destination.left + self.margin_width + cursor_line.visual_pos.x
                            - origin.x;
                        let top = destination.top + cursor_line.visual_pos.y - origin.y;
                        Rect { left, top, right: left + 1, bottom: top + 1 }
                    };
                    control_chars.push(visualizer_rect);
                } else {
                    line.push(scratch, ch);
                }
            }

            global_off += chunk.len();
        }

        BodyTextRects { whitespace_visualizers, control_chars }
    }

    /// Build the per-row layout outputs (text, dim-margin flag,
    /// selection rect, shadow-match rects, visualiser rects) into a
    /// [`TextareaLayout`]. Pure
    /// `&self` -- no buffer mutation. The caller writes the
    /// returned `start_cursor` back into `cursor_for_rendering`
    /// before invoking the lsh pass.
    pub fn layout(
        &mut self,
        origin: Point,
        destination: Rect,
        cursor_override: Option<Point>,
    ) -> Option<TextareaLayout> {
        if destination.is_empty() {
            return None;
        }

        // Wrapped rows have no horizontal scroll to honour, and a nonzero x
        // starts each row mid-text -- which reads as a continuation row, so
        // the margin shows dots and the wrong numbers for the rest of the
        // viewport.
        let origin = if self.word_wrap_column > 0 { Point { x: 0, y: origin.y } } else { origin };

        let width = destination.width();
        let height = destination.height();
        let line_number_width = self.margin_width.max(3) as usize - 3;
        let text_width = width - self.margin_width;
        let right_edge = origin.x + text_width;
        let mut visual_pos_x_max = 0;
        let mut visual_pos_x_truncated = false;
        let mut gutter_paint: Vec<(CoordType, GutterMark)> = Vec::new();
        let mut deleted_below: Option<(usize, CoordType)> = None;

        // Pick the cursor closer to the `origin.y`.
        let mut cursor = {
            let a = self.cursor;
            let b = self.cursor_for_rendering.unwrap_or_default();
            let da = (a.visual_pos.y - origin.y).abs();
            let db = (b.visual_pos.y - origin.y).abs();
            if da < db { a } else { b }
        };

        let [mut selection_beg, mut selection_end] = match self.selection {
            None => [Point::MIN, Point::MIN],
            Some(TextBufferSelection { beg, end }) => minmax(beg, end),
        };
        // The "active" end of the selection (the one the cursor is currently
        // anchored to). When the cursor's visible position is animated, the
        // visible selection extends to the animated cursor so anchor->cursor
        // reads consistently. The logical selection (`self.selection`, used
        // by Copy/Cut/Delete and friends) is unchanged -- only the visible
        // bounds shift.
        let selection_active_is_end = self.selection.map(|s| s.end >= s.beg).unwrap_or(false);
        // `caret_visual_pos`, not the raw cursor: at a word-wrap row break those
        // are two different cells and this value is painted as the caret. The
        // override is always supplied in production, so only a caller passing
        // None sees this -- which is exactly how the same mistake survived in
        // the physics IR until it was found.
        let cursor_visual_render = cursor_override.unwrap_or_else(|| self.caret_visual_pos());
        if cursor_override.is_some() && self.selection.is_some() {
            let anim_cursor =
                self.cursor_move_to_visual_internal(self.cursor, cursor_visual_render);
            let anim_logical = anim_cursor.logical_pos;
            if selection_active_is_end {
                selection_end = anim_logical.max(selection_beg);
            } else {
                selection_beg = anim_logical.min(selection_end);
            }
        }

        // Shadow-highlight every other literal occurrence of the selected text.
        // Only when the selection is non-empty, fits on one logical line, and
        // contains no newline bytes.
        let shadow_match = self.selection_range().and_then(|(b, e)| {
            if b.logical_pos.y != e.logical_pos.y {
                return None;
            }
            let mut needle = Vec::new();
            self.buffer.extract_raw(b.offset..e.offset, &mut needle, 0);
            if needle.is_empty() || needle.contains(&b'\n') {
                return None;
            }
            Some((needle, b.offset, e.offset))
        });

        let mut decors: Vec<VisualLine> = Vec::with_capacity(height.max(0) as usize);
        let mut start_cursor: Option<Cursor> = None;

        // Per-frame viewport row index, built only under word-wrap (it's the
        // only case `cursor_move_to_visual_internal` consults it). See
        // [`WrappedView`].
        let mut view_rows: Vec<Cursor> = if self.word_wrap_column > 0 {
            Vec::with_capacity(height.max(0) as usize)
        } else {
            Vec::new()
        };

        // lsh markup is computed inside the layout loop, intersected per
        // visual row, so wrapped rows don't paint attrs (underline / bold /
        // ...) past their actual text extent. Skipped when colour output is
        // suppressed (no-color mode); then markup rects stay empty and the
        // draw step paints nothing extra.
        let lsh_enabled = !crate::glyphs::no_color();
        let mut highlighter = Highlighter::new(&self.buffer, self.language);
        let mut hl_logical_y: Option<CoordType> = None;
        let mut hl_buf: Vec<MarkupSpan> = Vec::new();
        let mut hl_conflict = false;

        for y in 0..height {
            let scratch = scratch_arena(None);
            let mut line = BString::empty();
            line.reserve(&*scratch, width as usize * 2);
            let mut decor = VisualLine {
                fb_y: destination.top + y,
                text: String::new(),
                dim_wrapped_margin: false,
                selection_rect: None,
                shadow_match_rects: Vec::new(),
                whitespace_visualizers: Vec::new(),
                control_chars: Vec::new(),
                markup_fg_rects: Vec::new(),
                markup_attr_rects: Vec::new(),
            };

            let visual_line = origin.y + y;
            let cursor_beg =
                self.cursor_move_to_visual_internal(cursor, Point { x: origin.x, y: visual_line });
            let cursor_end = self.cursor_move_to_visual_internal(
                cursor_beg,
                Point { x: origin.x + text_width, y: visual_line },
            );

            // Capture the y==0 cursor as the seed for the next render's
            // cursor walk + the start cursor for the lsh pass. Caller
            // writes it back to `self.cursor_for_rendering`.
            if y == 0 {
                start_cursor = Some(cursor_beg);
            }

            // Capture the row start for the viewport index. Stop once we walk
            // past document end -- there `cursor_beg` stops advancing and its
            // visual row no longer equals `visual_line`, so skipping keeps
            // `rows` contiguous from `origin.y` (target row indexes directly).
            if self.word_wrap_column > 0 && cursor_beg.visual_pos.y == visual_line {
                view_rows.push(cursor_beg);
            }

            // Parse the row's logical line before the gutter is built: the
            // conflict mark comes from the highlighter, not the diff. Rows
            // past the end keep the last line's parse, since the cursor
            // stops advancing there. Wrapped continuation rows share it.
            let logical_y = cursor_beg.logical_pos.y;
            if hl_logical_y != Some(logical_y) {
                hl_buf.clear();
                hl_conflict = match &self.pinned_highlights {
                    Some(pinned) => {
                        self.pinned_markup(pinned, &cursor_beg, logical_y, &mut hl_buf);
                        false
                    }
                    None => {
                        let scratch_hl = scratch_arena(None);
                        let parsed = self.highlighter_cache.parse_line(
                            &scratch_hl,
                            &mut highlighter,
                            logical_y,
                        );
                        lsh_markup(&parsed.spans, &mut hl_buf);
                        parsed.conflict != ConflictTag::None
                    }
                };
                hl_logical_y = Some(logical_y);
            }

            if line_number_width != 0 {
                let (mark, dim) = self.build_gutter_margin(
                    &mut line,
                    &scratch,
                    line_number_width,
                    visual_line,
                    &cursor_beg,
                    hl_conflict,
                );
                if let Some(mark) = mark {
                    let row = destination.top + y;
                    let logical_y = cursor_beg.logical_pos.y;
                    // DeletedBelow points at the gap under the whole logical
                    // line, so it rides down to that line's last visible row
                    // instead of claiming a gap at every wrap.
                    if mark == GutterMark::DeletedBelow
                        && let Some((idx, pending_y)) = deleted_below
                        && pending_y == logical_y
                    {
                        gutter_paint[idx].0 = row;
                    } else {
                        if mark == GutterMark::DeletedBelow {
                            deleted_below = Some((gutter_paint.len(), logical_y));
                        }
                        gutter_paint.push((row, mark));
                    }
                }
                decor.dim_wrapped_margin = dim;
            }

            let (selection_off, sel_rect) = self.build_selection_row(
                &cursor_beg,
                &cursor_end,
                selection_beg,
                selection_end,
                visual_line,
                cursor_override,
                cursor_visual_render,
                selection_active_is_end,
                destination,
                origin,
                text_width,
                y,
            );
            decor.selection_rect = sel_rect;

            // Shadow-highlight matches of the current selection on this visual line.
            if let Some((needle, sel_beg, sel_end)) = &shadow_match {
                decor.shadow_match_rects = self.build_shadow_matches_row(
                    needle,
                    *sel_beg,
                    *sel_end,
                    &cursor_beg,
                    &cursor_end,
                    visual_line,
                    destination,
                    origin,
                    y,
                );
            }

            // How wide this row's content actually is. `cursor_end` stops at
            // the viewport's right edge, so a row that reaches it says nothing
            // about where its text ends -- and a scroll bound derived from
            // that is a function of the scroll offset rather than the content.
            // The walk is bounded, because one line can be the whole file.
            if self.word_wrap_column <= 0 {
                let row_end_x = if cursor_end.visual_pos.x < right_edge {
                    cursor_end.visual_pos.x
                } else if right_edge >= MAX_MEASURED_COLUMN {
                    MAX_MEASURED_COLUMN
                } else {
                    self.cursor_move_to_visual_internal(
                        cursor_end,
                        Point { x: MAX_MEASURED_COLUMN, y: visual_line },
                    )
                    .visual_pos
                    .x
                };
                if row_end_x >= MAX_MEASURED_COLUMN {
                    visual_pos_x_truncated = true;
                } else {
                    visual_pos_x_max = visual_pos_x_max.max(row_end_x);
                }
            }

            // Nothing to do if the entire line is empty.
            if cursor_beg.offset != cursor_end.offset {
                let body_rects = self.build_body_text(
                    &mut line,
                    &scratch,
                    cursor_beg,
                    &cursor_end,
                    selection_off,
                    destination,
                    origin,
                );
                decor.whitespace_visualizers = body_rects.whitespace_visualizers;
                decor.control_chars = body_rects.control_chars;
            }

            // Compute per-row lsh markup rects, clipped to this visual
            // row's actual text extent. Done here (during linewrap) so
            // wrapped continuation rows don't smear attrs across the
            // trailing blanks past the wrap column.
            if lsh_enabled && cursor_beg.offset != cursor_end.offset {
                let (fg_rects, attr_rects) = self.build_markup_row(
                    &cursor_beg,
                    &cursor_end,
                    visual_line,
                    &hl_buf,
                    destination,
                    origin,
                    y,
                );
                decor.markup_fg_rects = fg_rects;
                decor.markup_attr_rects = attr_rects;
            }

            // Copy the per-iter arena-backed BString into the
            // owned-text slot on `decor` so the layout output
            // outlives the scratch arena. ~80 bytes per row; not a
            // hot path.
            decor.text.push_str(line.as_str());
            decors.push(decor);

            cursor = cursor_end;
        }

        // Publish the viewport row index for the next frame's cursor motion.
        // Under word-wrap only; otherwise the field stays as-is (unused, and a
        // wrap toggle reflows + clears it anyway).
        if self.word_wrap_column > 0 {
            self.wrapped_view = Some(WrappedView {
                rows: view_rows,
                origin_y: origin.y,
                word_wrap_column: self.word_wrap_column,
                generation: self.buffer.generation(),
            });
        }

        let logical_y_beg = start_cursor.map_or(0, |c| c.logical_pos.y);
        let logical_y_end = cursor.logical_pos.y + 1;
        Some(TextareaLayout {
            lines: decors,
            gutter_marks: gutter_paint,
            visual_pos_x_max: if visual_pos_x_truncated {
                CoordType::MAX
            } else {
                visual_pos_x_max
            },
            cursor_visual_render,
            selection_empty: selection_beg >= selection_end,
            highlight_logical_y_range: logical_y_beg..logical_y_end,
            start_cursor,
        })
    }

    /// Extracts a rectangular region of the text buffer and writes it to the framebuffer.
    /// The `destination` rect is framebuffer coordinates. The extracted region within this
    /// text buffer has the given `origin` and the same size as the `destination` rect.
    /// Per-source-line dominant `IndexedColor`. Picks the `HighlightKind`
    /// with the most byte coverage on that line (excluding `Other`) and maps
    /// it via [`highlight_kind_color`]. Returns an empty Vec if no language
    /// is set.
    pub fn dominant_color_per_line(&self) -> Vec<Option<IndexedColor>> {
        if crate::glyphs::no_color() {
            return Vec::new();
        }
        let line_count = self.logical_line_count() as usize;
        let mut out = vec![None; line_count];
        if let Some(pinned) = &self.pinned_highlights {
            for (slot, spans) in out.iter_mut().zip(pinned) {
                let mut tally: Vec<(IndexedColor, usize)> = Vec::new();
                for s in spans {
                    let Some(color) = s.color else { continue };
                    match tally.iter_mut().find(|(c, _)| *c == color) {
                        Some(t) => t.1 += s.range.len(),
                        None => tally.push((color, s.range.len())),
                    }
                }
                *slot = tally.into_iter().max_by_key(|(_, len)| *len).map(|(c, _)| c);
            }
            return out;
        }
        let mut highlighter = Highlighter::new(&self.buffer, self.language);
        for slot in out.iter_mut() {
            let scratch = scratch_arena(None);
            let Some(parsed) = highlighter.parse_next_line(&scratch) else { break };
            // Tally byte coverage per kind across this line's spans.
            let mut tally: Vec<(HighlightKind, usize)> = Vec::new();
            for w in parsed.spans.windows(2) {
                let kind = w[0].kind;
                if matches!(kind, HighlightKind::Other) {
                    continue;
                }
                let len = w[1].start.saturating_sub(w[0].start);
                if let Some(s) = tally.iter_mut().find(|(k, _)| *k == kind) {
                    s.1 += len;
                } else {
                    tally.push((kind, len));
                }
            }
            *slot = tally
                .into_iter()
                .max_by_key(|(_, len)| *len)
                .and_then(|(k, _)| highlight_kind_color(k));
        }
        out
    }
}
