//! The textarea widget.
//!
//! Much the largest widget in the tui, and the only one with its own
//! input handling: mouse hit-testing and drag-scrolling, plus a keyboard
//! map covering navigation, selection, editing and the standard chords.
//!
//! Painting is not here. `crate::buffer::render` builds the per-row IR
//! and `crate::paint` consumes it; what this module owns is the widget
//! shell -- layout negotiation, scroll offset, and turning input into
//! calls on the underlying `TextBuffer`.

use super::*;

impl Tui {
    /// Renders the body of a `NodeContent::Textarea` node into the
    /// framebuffer. Encapsulates the orchestration that was inline
    /// in [`Tui::render_node`]:
    ///
    /// 1. Compute minimap / scrollbar widths, narrow destination.
    /// 2. Dispatch on `no_animations()` -- snap or advance lerps.
    /// 3. Seed line-move trail from buffer event if fresh.
    /// 4. Call `tb.layout()` + orchestrate the two-pass paint via
    ///    `paint::draw::textarea_lines` + `textarea_overlays`.
    /// 5. Paint the in-flight line-move trail overlay (if any).
    /// 6. Paint the minimap rail (if visible).
    /// 7. Paint the scrollbar (if visible, mutually exclusive w/
    ///    the minimap).
    pub(super) fn render_textarea_content(
        &mut self,
        tc: &mut TextareaContent,
        node_id: u64,
        inner: Rect,
        inner_clipped: Rect,
    ) {
        let mut tb = tc.buffer.borrow_mut();
        let mut destination = Rect {
            left: inner_clipped.left,
            top: inner_clipped.top,
            right: inner_clipped.right,
            bottom: inner_clipped.bottom,
        };

        let minimap_w = paint::physics::textarea_minimap_width(tc.single_line, tb.minimap_cells());
        let scrollbar_w = paint::physics::textarea_scrollbar_width(tc.single_line, minimap_w);

        destination.right -= scrollbar_w + minimap_w;

        // Per-textarea anim state lives in Tui::anim.textareas keyed
        // by node id. Pull a Copy of the entry, mutate freely, write
        // back at the end -- avoids holding a borrow on
        // self.anim.textareas across the &mut self.framebuffer paint
        // calls below.
        let mut anim_state = *self.anim.textareas.entry(node_id).or_default();

        // Single no_animations() dispatch: when animations are disabled,
        // snap everything to target and skip the per-feature lerps. The
        // inner advance_* fns no longer check the killswitch themselves.
        let (visual_offset, cursor_override) = if crate::glyphs::no_animations() {
            anim_state.scroll_visual = (tc.scroll_offset.x as f32, tc.scroll_offset.y as f32);
            anim_state.cursor_visual = {
                let c = tb.caret_visual_pos();
                Some((c.x as f32, c.y as f32))
            };
            anim_state.last_buffer_generation = tb.generation();
            // Bump the line-move gen so re-enabling animations doesn't
            // re-trigger an old trail.
            anim_state.last_line_move_gen = tb.peek_pending_line_move().1;
            (tc.scroll_offset, tb.caret_visual_pos())
        } else {
            paint::anim::snap_on_buffer_edit(
                &mut anim_state.scroll_visual,
                &mut anim_state.cursor_visual,
                &mut anim_state.last_buffer_generation,
                tb.generation(),
                tc.scroll_offset,
                tb.caret_visual_pos(),
            );

            let (line_move_ev, line_move_gen) = tb.peek_pending_line_move();
            paint::anim::seed_line_move_trail(
                &mut anim_state.line_move,
                &mut anim_state.last_line_move_gen,
                line_move_ev,
                line_move_gen,
                time::Instant::now(),
            );

            // Toggling wrap on pins the scroll target's x to 0 without
            // touching the buffer generation, so nothing snaps the lerp and it
            // spends the next few frames feeding a horizontal offset to a
            // layout that has no horizontal scroll.
            if tb.word_wrap_column() > 0 {
                anim_state.scroll_visual.0 = tc.scroll_offset.x as f32;
            }

            let visual_offset = paint::anim::advance_scroll(
                &mut anim_state.scroll_visual,
                tc.scroll_offset,
                self.anim.dt_secs,
            );
            let cursor_target = tb.caret_visual_pos();
            let cursor_override = paint::anim::advance_cursor(
                &mut anim_state.cursor_visual,
                cursor_target,
                self.anim.dt_secs,
            );
            let still_animating =
                visual_offset != tc.scroll_offset || cursor_override != cursor_target;
            if still_animating {
                self.request_animation_frame();
            }
            (visual_offset, cursor_override)
        };

        // Orchestrate layout + paint here: tb.layout returns an owned
        // TextareaLayout; paint runs in two passes (text rows + lsh +
        // overlays) with tb borrowed mutably only where it must be
        // (cursor seed, lsh). Formerly lived inside the now-deleted
        // TextBuffer::render.
        if let Some(layout) = tb.layout(visual_offset, destination, Some(cursor_override)) {
            tb.set_cursor_for_rendering(layout.start_cursor);
            let selection_rects = paint::draw::textarea_lines(
                &mut self.framebuffer,
                &layout,
                destination.left,
                destination.right,
                tb.margin_width(),
                tc.has_focus,
            );
            paint::draw::textarea_overlays(
                &mut self.framebuffer,
                paint::draw::TextareaOverlayOpts {
                    dest: destination,
                    origin: visual_offset,
                    margin_width: tb.margin_width(),
                    ruler_column: tb.ruler(),
                    selection_rects: &selection_rects,
                    gutter_marks: &layout.gutter_marks,
                    focused: tc.has_focus,
                    cursor_visual: layout.cursor_visual_render,
                    word_wrap_column: tb.word_wrap_column(),
                    overtype: tb.is_overtype(),
                    line_highlight: tb.is_line_highlight_enabled() && layout.selection_empty,
                    column_guides: tb.is_column_guides_enabled(),
                },
            );
            tc.scroll_offset_x_max = layout.visual_pos_x_max;
        }

        // Trail-flash overlay for the line-move animation. Runs
        // after the textarea paint pass so it sits on top of the
        // text. Expires once duration elapsed. Per-row band shape
        // (which columns
        // get tinted) comes from `tb.line_move_bands()`. We narrow
        // `destination` to exclude the buffer's left margin (line
        // numbers / gutter marks) so the band stays in the text area.
        if let Some(t) =
            paint::anim::advance_line_move_trail(&mut anim_state.line_move, time::Instant::now())
        {
            let line_move = anim_state.line_move.expect("just advanced past None");
            let text_dest = Rect { left: destination.left + tb.margin_width(), ..destination };
            paint::draw::line_move_trail(
                &mut self.framebuffer,
                text_dest,
                visual_offset,
                line_move.to_y,
                line_move.height,
                t,
                tb.line_move_bands(),
            );
            self.request_animation_frame();
        }

        if minimap_w > 0 {
            let track = Rect {
                left: inner_clipped.right - scrollbar_w - minimap_w,
                top: inner_clipped.top,
                right: inner_clipped.right - scrollbar_w,
                bottom: inner_clipped.bottom,
            };
            paint::draw::minimap_rail(
                &mut self.framebuffer,
                track,
                tb.minimap_cells(),
                tb.minimap_content_rows(),
                visual_offset.y,
                inner.height(),
            );
        }

        if scrollbar_w > 0 {
            // Render the scrollbar.
            let track = Rect {
                left: inner_clipped.right - 1,
                top: inner_clipped.top,
                right: inner_clipped.right,
                bottom: inner_clipped.bottom,
            };
            tc.thumb_height = self.framebuffer.draw_scrollbar(
                inner_clipped,
                track,
                visual_offset.y,
                tb.visual_line_count() + inner.height() - 1,
            );
        }

        // Write back the per-textarea anim state. Read-modify-write
        // pattern avoids holding a borrow on `self.anim.textareas`
        // across the &mut self.framebuffer paint calls above.
        self.anim.textareas.insert(node_id, anim_state);
    }
}

impl Context<'_, '_> {
    /// Creates a text area.
    pub fn textarea(&mut self, classname: &'static str, tb: RcTextBuffer) {
        self.textarea_internal(classname, TextBufferPayload::Textarea(tb));
    }

    pub(super) fn textarea_internal(
        &mut self,
        classname: &'static str,
        payload: TextBufferPayload,
    ) -> bool {
        self.block_begin(classname);
        self.block_end();

        let mut node = self.tree.last_node.borrow_mut();
        let node = &mut *node;
        let single_line = match &payload {
            TextBufferPayload::Editline(_) => true,
            TextBufferPayload::Textarea(_) => false,
        };

        let buffer = {
            let buffers = &mut self.tui.cached_text_buffers;

            let cached = match buffers.iter_mut().find(|t| t.node_id == node.id) {
                Some(cached) => {
                    if let TextBufferPayload::Textarea(tb) = &payload {
                        cached.editor = tb.clone();
                    };
                    cached.seen = true;
                    cached
                }
                None => {
                    // If the node is not in the cache, we need to create a new one.
                    buffers.push(CachedTextBuffer {
                        node_id: node.id,
                        editor: match &payload {
                            TextBufferPayload::Editline(_) => TextBuffer::new_rc(true).unwrap(),
                            TextBufferPayload::Textarea(tb) => tb.clone(),
                        },
                        seen: true,
                    });
                    buffers.last_mut().unwrap()
                }
            };

            // SAFETY: *Assuming* that there are no duplicate node IDs in the tree that
            // would cause this cache slot to be overwritten, then this operation is safe.
            // The text buffer cache will keep the buffer alive for us long enough.
            unsafe { mem::transmute(&*cached.editor) }
        };

        node.content = NodeContent::Textarea(TextareaContent {
            buffer,
            scroll_offset: Default::default(),
            scroll_offset_y_drag_start: CoordType::MIN,
            scroll_offset_x_max: 0,
            thumb_height: 0,
            single_line,
            has_focus: self.tui.is_node_focused(node.id),
        });

        let content = match node.content {
            NodeContent::Textarea(ref mut content) => content,
            _ => unreachable!(),
        };

        if let TextBufferPayload::Editline(text) = &payload {
            content.buffer.borrow_mut().copy_from_str(*text);
        }

        // The offset this frame started at. The horizontal bound applies to
        // advancing only, so a narrower row set can't drag the viewport back.
        let mut scroll_x_before = content.scroll_offset.x;

        if let Some(node_prev) = self.tui.prev_node_map.get(node.id) {
            let node_prev = node_prev.borrow();
            if let NodeContent::Textarea(content_prev) = &node_prev.content {
                content.scroll_offset = content_prev.scroll_offset;
                scroll_x_before = content_prev.scroll_offset.x;
                content.scroll_offset_y_drag_start = content_prev.scroll_offset_y_drag_start;
                content.scroll_offset_x_max = content_prev.scroll_offset_x_max;
                content.thumb_height = content_prev.thumb_height;

                let mut text_width = node_prev.inner.width();
                {
                    let tb = content.buffer.borrow();
                    let minimap_w = paint::physics::textarea_minimap_width(
                        content.single_line,
                        tb.minimap_cells(),
                    );
                    text_width -=
                        paint::physics::textarea_scrollbar_width(content.single_line, minimap_w)
                            + minimap_w;
                }

                let mut make_cursor_visible;
                let scroll_delta_x;
                let scroll_delta_y;
                let scroll_to_tail;
                let scroll_bound_to_tail;
                {
                    let mut tb = content.buffer.borrow_mut();
                    make_cursor_visible = tb.take_cursor_visibility_request();
                    // A reflow follows the cursor only where the cursor is the
                    // reader's place. Unfocused, the viewport is: eat's viewer
                    // keeps its cursor parked at the top and must not snap to
                    // it on a resize.
                    make_cursor_visible |= tb.set_width(text_width) && content.has_focus;
                    scroll_delta_x = tb.take_scroll_delta_x_request();
                    scroll_delta_y = tb.take_scroll_delta_y_request();
                    scroll_to_tail = tb.take_scroll_to_tail_request();
                    scroll_bound_to_tail = tb.take_scroll_bound_to_tail_request();
                }

                make_cursor_visible |= self.textarea_handle_input(content, &node_prev, single_line);

                content.scroll_offset.x += scroll_delta_x;
                content.scroll_offset.y += scroll_delta_y;
                if scroll_to_tail {
                    // Last line on the bottom edge; the horizontal offset is
                    // the reader's and stays put.
                    let lines = content.buffer.borrow().visual_line_count();
                    content.scroll_offset.y = (lines - node_prev.inner.height()).max(0);
                } else if scroll_bound_to_tail {
                    let lines = content.buffer.borrow().visual_line_count();
                    let tail = (lines - node_prev.inner.height()).max(0);
                    content.scroll_offset.y = content.scroll_offset.y.min(tail);
                } else if scroll_delta_x == 0 && scroll_delta_y == 0 && make_cursor_visible {
                    self.textarea_make_cursor_visible(content, &node_prev);
                }
            } else {
                crate::sanity_assert!(textarea_prev_node_kind, false, "prev node not a textarea");
            }
        }

        let dirty;
        {
            let mut tb = content.buffer.borrow_mut();
            dirty = tb.is_dirty();
            if dirty && let TextBufferPayload::Editline(text) = payload {
                tb.save_as_string(text);
            }
        }

        self.textarea_adjust_scroll_offset(content, scroll_x_before);

        if single_line {
            node.attributes.fg = self.indexed(IndexedColor::Foreground);
            node.attributes.bg = self.indexed(IndexedColor::Background);
            if !content.has_focus {
                node.attributes.fg = self.contrasted(node.attributes.bg);
                node.attributes.bg = self.indexed_alpha(IndexedColor::Background, 1, 2);
            }
        }

        node.attributes.focusable = true;
        node.intrinsic_size.height = content.buffer.borrow().visual_line_count();
        node.intrinsic_size_set = true;

        dirty
    }

    fn textarea_handle_input(
        &mut self,
        tc: &mut TextareaContent,
        node_prev: &Node,
        single_line: bool,
    ) -> bool {
        if self.input_consumed {
            return false;
        }

        let mut tb = tc.buffer.borrow_mut();
        let tb = &mut *tb;
        let mut make_cursor_visible = false;
        let mut change_preferred_column = false;

        // Scrolling works even if the node isn't focused.
        if self.input_scroll_delta != Point::default()
            && node_prev.inner_clipped.contains(self.tui.mouse_position)
        {
            tc.scroll_offset.x += self.input_scroll_delta.x;
            tc.scroll_offset.y += self.input_scroll_delta.y;
            self.set_input_consumed();
            return make_cursor_visible;
        } else if self.tui.mouse_state != InputMouseState::None
            && self.tui.is_node_focused(node_prev.id)
        {
            let mouse = self.tui.mouse_position;
            let inner = node_prev.inner;
            let minimap_w =
                paint::physics::textarea_minimap_width(tc.single_line, tb.minimap_cells());
            let scrollbar_w = paint::physics::textarea_scrollbar_width(tc.single_line, minimap_w);
            let text_rect = Rect {
                left: inner.left + tb.margin_width(),
                top: inner.top,
                right: inner.right - scrollbar_w - minimap_w,
                bottom: inner.bottom,
            };
            let minimap_rect = Rect {
                left: text_rect.right,
                top: inner.top,
                right: text_rect.right + minimap_w,
                bottom: inner.bottom,
            };
            let track_rect = Rect {
                left: minimap_rect.right,
                top: inner.top,
                right: minimap_rect.right + scrollbar_w,
                bottom: inner.bottom,
            };
            let pos = Point {
                x: mouse.x - inner.left - tb.margin_width() + tc.scroll_offset.x,
                y: mouse.y - inner.top + tc.scroll_offset.y,
            };

            if text_rect.contains(self.tui.mouse_down_position) {
                // Freeze input during follow-up frames of a multi-click
                // sequence (motion/release after the 2nd+ mouse-down). The
                // initial multi-click frame carries `input_mouse_click >= 2`
                // and is handled by the dispatch below; later frames have
                // `input_mouse_click == 0` and would otherwise clobber the
                // word/line selection via drag or cursor-move.
                if self.tui.mouse_click_counter >= 2 && self.input_mouse_click == 0 {
                    // no-op: keep multi-click selection intact
                } else if self.tui.mouse_is_drag {
                    tb.selection_update_visual(pos);
                    tb.set_preferred_column(tb.cursor_visual_pos().x);

                    let height = inner.height();

                    // If the editor is only 1 line tall we can't possibly scroll up or down.
                    if height >= 2 {
                        fn calc(min: CoordType, max: CoordType, mouse: CoordType) -> CoordType {
                            // Otherwise, the scroll zone is up to 3 lines at the top/bottom.
                            let zone_height = ((max - min) / 2).min(3);

                            // The .y positions where the scroll zones begin:
                            // Mouse coordinates above top and below bottom respectively.
                            let scroll_min = min + zone_height;
                            let scroll_max = max - zone_height - 1;

                            // Calculate the delta for scrolling up or down.
                            let delta_min = (mouse - scroll_min).clamp(-zone_height, 0);
                            let delta_max = (mouse - scroll_max).clamp(0, zone_height);

                            // If I didn't mess up my logic here, only one of the two values can possibly be !=0.
                            let idx = 3 + delta_min + delta_max;

                            const SPEEDS: [CoordType; 7] = [-9, -3, -1, 0, 1, 3, 9];
                            let idx = idx.clamp(0, SPEEDS.len() as CoordType) as usize;
                            SPEEDS[idx]
                        }

                        let delta_x = calc(text_rect.left, text_rect.right, mouse.x);
                        let delta_y = calc(text_rect.top, text_rect.bottom, mouse.y);

                        tc.scroll_offset.x += delta_x;
                        tc.scroll_offset.y += delta_y;

                        if delta_x != 0 || delta_y != 0 {
                            self.tui.read_timeout = time::Duration::from_millis(25);
                        }
                    }
                } else {
                    match self.input_mouse_click {
                        5.. => {}
                        4 => tb.select_all(),
                        3 => {
                            tb.cursor_move_to_visual(pos);
                            tb.select_line();
                        }
                        2 => {
                            tb.cursor_move_to_visual(pos);
                            tb.select_word();
                        }
                        _ => match self.tui.mouse_state {
                            InputMouseState::Left => {
                                if self.input_mouse_modifiers.contains(kbmod::SHIFT) {
                                    // TODO: Untested because Windows Terminal surprisingly doesn't support Shift+Click.
                                    tb.selection_update_visual(pos);
                                } else {
                                    tb.cursor_move_to_visual(pos);
                                }
                                tb.set_preferred_column(tb.cursor_visual_pos().x);
                                make_cursor_visible = true;
                            }
                            _ => return false,
                        },
                    }
                }
            } else if minimap_rect.contains(self.tui.mouse_down_position) {
                // Rail row maps via the cell list (1:1 when rail >= n_cells,
                // scaled otherwise) so each braille glyph corresponds to a
                // contiguous source-row slice.
                //
                // - Pure click+release (no drag): jump so the clicked cell's
                //   first source row becomes the topmost visible line. Lets
                //   a misclick be aborted by moving off the rail first.
                // - Drag: scroll proportionally with mouse delta so the
                //   initial grab point on the band stays under the cursor
                //   (real-scrollbar feel). No re-snap on release.
                let content_rows = tb.minimap_content_rows() as i64;
                let rail_h = minimap_rect.height() as i64;
                if content_rows > 0 && rail_h > 0 {
                    let rows_per_cell = MINIMAP_SOURCE_ROWS_PER_CELL as i64;
                    let n_cells = (content_rows + rows_per_cell - 1) / rows_per_cell;
                    let rail_used = n_cells.min(rail_h);
                    let max_scroll = (tb.visual_line_count() - 1).max(0);

                    if self.tui.mouse_state == InputMouseState::Release {
                        if !self.tui.mouse_is_drag {
                            let local_y = (mouse.y - minimap_rect.top).max(0) as i64;
                            let cell_idx = if rail_h >= n_cells {
                                local_y
                            } else {
                                local_y * n_cells / rail_h
                            };
                            let target_row = (cell_idx * rows_per_cell) as CoordType;
                            tc.scroll_offset.y = target_row.clamp(0, max_scroll);
                        }
                        tc.scroll_offset_y_drag_start = CoordType::MIN;
                    } else if self.tui.mouse_is_drag && rail_used > 0 {
                        if tc.scroll_offset_y_drag_start == CoordType::MIN {
                            tc.scroll_offset_y_drag_start = tc.scroll_offset.y;
                        }
                        let delta_y = (mouse.y - self.tui.mouse_down_position.y) as i64;
                        // Source rows per rail row: cells_capacity / rail_used.
                        let cells_capacity = n_cells * rows_per_cell;
                        let delta_rows = (delta_y * cells_capacity / rail_used) as CoordType;
                        tc.scroll_offset.y =
                            (tc.scroll_offset_y_drag_start + delta_rows).clamp(0, max_scroll);
                    }
                }
            } else if track_rect.contains(self.tui.mouse_down_position) {
                if self.tui.mouse_state == InputMouseState::Release {
                    tc.scroll_offset_y_drag_start = CoordType::MIN;
                } else if self.tui.mouse_is_drag {
                    if tc.scroll_offset_y_drag_start == CoordType::MIN {
                        tc.scroll_offset_y_drag_start = tc.scroll_offset.y;
                    }

                    // The textarea supports 1 height worth of "scrolling beyond the end".
                    // `track_height` is the same as the viewport height.
                    let scrollable_height = tb.visual_line_count() - 1;
                    tc.scroll_offset.y = scrollbar_drag_offset(
                        tc.scroll_offset_y_drag_start,
                        mouse.y - self.tui.mouse_down_position.y,
                        scrollable_height,
                        track_rect.height() - tc.thumb_height,
                    );
                }
            }

            self.set_input_consumed();
            return make_cursor_visible;
        }

        if !tc.has_focus {
            return false;
        }

        let mut write: &[u8] = &[];

        if let Some(input) = &self.input_text {
            write = input.as_bytes();
        } else if let Some(input) = &self.input_keyboard {
            let key = input.key();
            let modifiers = input.modifiers();

            make_cursor_visible = true;

            // The configured chords are matched first, on the whole
            // key+modifier combination -- a rebound action need not live on the
            // letter the default used, so the per-key match below could not see
            // it. An unbound action is `vk::NULL`, which no real keystroke is.
            let chords = textarea_chords();
            let chord = *input;
            let configured = chord != vk::NULL
                && if chord == chords.undo {
                    tb.undo();
                    true
                } else if chord == chords.redo {
                    tb.redo();
                    true
                } else if chord == chords.cut {
                    tb.cut(self.clipboard_mut());
                    true
                } else if chord == chords.copy {
                    tb.copy(self.clipboard_mut());
                    true
                } else if chord == chords.paste {
                    tb.paste(self.clipboard_ref());
                    true
                } else if chord == chords.select_all {
                    tb.select_all();
                    true
                } else {
                    false
                };

            if configured {
                change_preferred_column = true;
            } else {
                match key {
                    vk::BACK => {
                        let granularity = if modifiers == KBMOD_FOR_WORD_NAV {
                            CursorMovement::Word
                        } else {
                            CursorMovement::Grapheme
                        };
                        tb.delete(granularity, -1);
                    }
                    vk::TAB => {
                        if single_line {
                            // If this is just a simple input field, don't consume Tab (= early return).
                            return false;
                        }
                        tb.indent_change(if modifiers == kbmod::SHIFT { -1 } else { 1 });
                    }
                    vk::RETURN => {
                        if single_line {
                            // If this is just a simple input field, don't consume Enter (= early return).
                            return false;
                        }
                        write = b"\n";
                    }
                    vk::ESCAPE => {
                        // If there was a selection, clear it and show the cursor (= fallthrough).
                        if !tb.clear_selection() {
                            if single_line {
                                // If this is just a simple input field, don't consume the escape key
                                // (early return) and don't show the cursor (= return false).
                                return false;
                            }

                            // If this is a textarea, don't show the cursor if
                            // the escape key was pressed and nothing happened.
                            make_cursor_visible = false;
                        }
                    }
                    vk::PRIOR => {
                        let height = node_prev.inner.height() - 1;

                        // If the cursor was already on the first line,
                        // move it to the start of the buffer.
                        if tb.cursor_visual_pos().y == 0 {
                            tb.set_preferred_column(0);
                        }

                        if modifiers == kbmod::SHIFT {
                            tb.selection_update_visual(Point {
                                x: tb.preferred_column(),
                                y: tb.cursor_visual_pos().y - height,
                            });
                        } else {
                            tb.cursor_move_to_visual(Point {
                                x: tb.preferred_column(),
                                y: tb.cursor_visual_pos().y - height,
                            });
                        }
                    }
                    vk::NEXT => {
                        let height = node_prev.inner.height() - 1;

                        // If the cursor was already on the last line,
                        // move it to the end of the buffer.
                        if tb.cursor_visual_pos().y >= tb.visual_line_count() - 1 {
                            tb.set_preferred_column(CoordType::MAX);
                        }

                        if modifiers == kbmod::SHIFT {
                            tb.selection_update_visual(Point {
                                x: tb.preferred_column(),
                                y: tb.cursor_visual_pos().y + height,
                            });
                        } else {
                            tb.cursor_move_to_visual(Point {
                                x: tb.preferred_column(),
                                y: tb.cursor_visual_pos().y + height,
                            });
                        }

                        if tb.preferred_column() == CoordType::MAX {
                            tb.set_preferred_column(tb.cursor_visual_pos().x);
                        }
                    }
                    vk::END => {
                        let logical_before = tb.cursor_logical_pos();
                        let destination = if modifiers.contains(kbmod::CTRL) {
                            Point::MAX
                        } else {
                            Point { x: CoordType::MAX, y: tb.cursor_visual_pos().y }
                        };

                        if modifiers.contains(kbmod::SHIFT) {
                            tb.selection_update_visual(destination);
                        } else {
                            tb.cursor_move_to_visual(destination);
                        }

                        if !modifiers.contains(kbmod::CTRL) {
                            let logical_after = tb.cursor_logical_pos();

                            // If word-wrap is enabled and the user presses End the first time,
                            // it moves to the start of the visual line. The second time they
                            // press it, it moves to the start of the logical line.
                            if tb.is_word_wrap_enabled() && logical_after == logical_before {
                                if modifiers == kbmod::SHIFT {
                                    tb.selection_update_logical(Point {
                                        x: CoordType::MAX,
                                        y: tb.cursor_logical_pos().y,
                                    });
                                } else {
                                    tb.cursor_move_to_logical(Point {
                                        x: CoordType::MAX,
                                        y: tb.cursor_logical_pos().y,
                                    });
                                }
                            }
                        }
                    }
                    vk::HOME => {
                        let logical_before = tb.cursor_logical_pos();
                        let destination = if modifiers.contains(kbmod::CTRL) {
                            Default::default()
                        } else {
                            Point { x: 0, y: tb.cursor_visual_pos().y }
                        };

                        if modifiers.contains(kbmod::SHIFT) {
                            tb.selection_update_visual(destination);
                        } else {
                            tb.cursor_move_to_visual(destination);
                        }

                        if !modifiers.contains(kbmod::CTRL) {
                            let mut logical_after = tb.cursor_logical_pos();

                            // If word-wrap is enabled and the user presses Home the first time,
                            // it moves to the start of the visual line. The second time they
                            // press it, it moves to the start of the logical line.
                            if tb.is_word_wrap_enabled() && logical_after == logical_before {
                                if modifiers == kbmod::SHIFT {
                                    tb.selection_update_logical(Point {
                                        x: 0,
                                        y: tb.cursor_logical_pos().y,
                                    });
                                } else {
                                    tb.cursor_move_to_logical(Point {
                                        x: 0,
                                        y: tb.cursor_logical_pos().y,
                                    });
                                }
                                logical_after = tb.cursor_logical_pos();
                            }

                            // If the line has some indentation and the user pressed Home,
                            // the first time it'll stop at the indentation. The second time
                            // they press it, it'll move to the true start of the line.
                            //
                            // If the cursor is already at the start of the line,
                            // we move it back to the end of the indentation.
                            if logical_after.x == 0
                                && let indent_end = tb.indent_end_logical_pos()
                                && (logical_before > indent_end || logical_before.x == 0)
                            {
                                if modifiers == kbmod::SHIFT {
                                    tb.selection_update_logical(indent_end);
                                } else {
                                    tb.cursor_move_to_logical(indent_end);
                                }
                            }
                        }
                    }
                    vk::LEFT => {
                        let granularity = if modifiers.contains(KBMOD_FOR_WORD_NAV) {
                            CursorMovement::Word
                        } else {
                            CursorMovement::Grapheme
                        };
                        if modifiers.contains(kbmod::SHIFT) {
                            tb.selection_update_delta(granularity, -1);
                        } else if let Some((beg, _)) = tb.selection_range() {
                            unsafe { tb.set_cursor(beg) };
                        } else {
                            tb.cursor_move_delta(granularity, -1);
                        }
                    }
                    vk::UP => {
                        if single_line {
                            return false;
                        }
                        match modifiers {
                            kbmod::NONE => {
                                let mut x = tb.preferred_column();
                                let mut y = tb.cursor_visual_pos().y - 1;

                                // If there's a selection we put the cursor above it.
                                if let Some((beg, _)) = tb.selection_range() {
                                    x = beg.visual_pos.x;
                                    y = beg.visual_pos.y - 1;
                                    tb.set_preferred_column(x);
                                }

                                // If the cursor was already on the first line,
                                // move it to the start of the buffer.
                                if y < 0 {
                                    x = 0;
                                    tb.set_preferred_column(0);
                                }

                                tb.cursor_move_to_visual(Point { x, y });
                            }
                            kbmod::CTRL => {
                                tc.scroll_offset.y -= 1;
                                make_cursor_visible = false;
                            }
                            kbmod::SHIFT => {
                                // If the cursor was already on the first line,
                                // move it to the start of the buffer.
                                if tb.cursor_visual_pos().y == 0 {
                                    tb.set_preferred_column(0);
                                }

                                tb.selection_update_visual(Point {
                                    x: tb.preferred_column(),
                                    y: tb.cursor_visual_pos().y - 1,
                                });
                            }
                            _ => return false,
                        }
                    }
                    vk::RIGHT => {
                        let granularity = if modifiers.contains(KBMOD_FOR_WORD_NAV) {
                            CursorMovement::Word
                        } else {
                            CursorMovement::Grapheme
                        };
                        if modifiers.contains(kbmod::SHIFT) {
                            tb.selection_update_delta(granularity, 1);
                        } else if let Some((_, end)) = tb.selection_range() {
                            unsafe { tb.set_cursor(end) };
                        } else {
                            tb.cursor_move_delta(granularity, 1);
                        }
                    }
                    vk::DOWN => {
                        if single_line {
                            return false;
                        }
                        match modifiers {
                            kbmod::NONE => {
                                let mut x = tb.preferred_column();
                                let mut y = tb.cursor_visual_pos().y + 1;

                                // If there's a selection we put the cursor below it.
                                if let Some((_, end)) = tb.selection_range() {
                                    x = end.visual_pos.x;
                                    y = end.visual_pos.y + 1;
                                    tb.set_preferred_column(x);
                                }

                                // If the cursor was already on the last line,
                                // move it to the end of the buffer.
                                if y >= tb.visual_line_count() {
                                    x = CoordType::MAX;
                                }

                                tb.cursor_move_to_visual(Point { x, y });

                                // If we fell into the `if y >= tb.get_visual_line_count()` above, we wanted to
                                // update the `preferred_column` but didn't know yet what it was. Now we know!
                                if x == CoordType::MAX {
                                    tb.set_preferred_column(tb.cursor_visual_pos().x);
                                }
                            }
                            kbmod::CTRL => {
                                tc.scroll_offset.y += 1;
                                make_cursor_visible = false;
                            }
                            kbmod::SHIFT => {
                                // If the cursor was already on the last line,
                                // move it to the end of the buffer.
                                if tb.cursor_visual_pos().y >= tb.visual_line_count() - 1 {
                                    tb.set_preferred_column(CoordType::MAX);
                                }

                                tb.selection_update_visual(Point {
                                    x: tb.preferred_column(),
                                    y: tb.cursor_visual_pos().y + 1,
                                });

                                if tb.preferred_column() == CoordType::MAX {
                                    tb.set_preferred_column(tb.cursor_visual_pos().x);
                                }
                            }
                            _ => return false,
                        }
                    }
                    vk::INSERT => match modifiers {
                        kbmod::SHIFT => tb.paste(self.clipboard_ref()),
                        kbmod::CTRL => tb.copy(self.clipboard_mut()),
                        // Overtype in a one-line input field has nothing to
                        // mean, and the field has no way to show the mode --
                        // so bare Insert silently made the Find box overwrite.
                        // Its siblings above (Tab, Return, arrows, Escape) all
                        // bail out for single-line the same way.
                        _ if single_line => return false,
                        _ => tb.set_overtype(!tb.is_overtype()),
                    },
                    vk::DELETE => match modifiers {
                        kbmod::SHIFT => tb.cut(self.clipboard_mut()),
                        m if m == KBMOD_FOR_WORD_NAV => tb.delete(CursorMovement::Word, 1),
                        _ => tb.delete(CursorMovement::Grapheme, 1),
                    },
                    vk::B => match modifiers {
                        kbmod::ALT if cfg!(any(target_os = "macos", target_os = "ios")) => {
                            // On macOS, terminals commonly emit the Emacs style
                            // Alt+B (ESC b) sequence for Alt+Left.
                            tb.cursor_move_delta(CursorMovement::Word, -1);
                        }
                        _ => return false,
                    },
                    vk::F => match modifiers {
                        kbmod::ALT if cfg!(any(target_os = "macos", target_os = "ios")) => {
                            // On macOS, terminals commonly emit the Emacs style
                            // Alt+F (ESC f) sequence for Alt+Right.
                            tb.cursor_move_delta(CursorMovement::Word, 1);
                        }
                        _ => return false,
                    },
                    vk::H => match modifiers {
                        kbmod::CTRL => tb.delete(CursorMovement::Word, -1),
                        _ => return false,
                    },
                    vk::L => match modifiers {
                        kbmod::CTRL => tb.select_line(),
                        _ => return false,
                    },
                    vk::Z => match modifiers {
                        // Kept as an always-on alias: the config has a single
                        // `redo` entry, and this is the other spelling users reach
                        // for. Everything else on this key is configurable above.
                        m if m == KBMOD_PRIMARY | kbmod::SHIFT => tb.redo(),
                        kbmod::ALT => tb.set_word_wrap(!tb.is_word_wrap_enabled()),
                        _ => return false,
                    },
                    _ => return false,
                }

                change_preferred_column = !matches!(key, vk::PRIOR | vk::NEXT | vk::UP | vk::DOWN);
            }
        } else {
            return false;
        }

        if single_line && !write.is_empty() {
            let (end, _) = simd::lines_fwd(write, 0, 0, 1);
            write = unicode::strip_newline(&write[..end]);
        }
        if !write.is_empty() {
            tb.write_canon(write);
            change_preferred_column = true;
            make_cursor_visible = true;
        }

        if change_preferred_column {
            tb.set_preferred_column(tb.cursor_visual_pos().x);
        }

        self.set_input_consumed();
        make_cursor_visible
    }

    fn textarea_make_cursor_visible(&self, tc: &mut TextareaContent, node_prev: &Node) {
        let tb = tc.buffer.borrow();
        let mut scroll_x = tc.scroll_offset.x;
        let mut scroll_y = tc.scroll_offset.y;

        let text_width = tb.text_width();
        let cursor_x = tb.cursor_visual_pos().x;
        scroll_x = scroll_x.min(cursor_x - 10);
        scroll_x = scroll_x.max(cursor_x - text_width + 10);

        let viewport_height = node_prev.inner.height();
        let cursor_y = tb.cursor_visual_pos().y;
        // Scroll up if the cursor is above the visible area.
        scroll_y = scroll_y.min(cursor_y);
        // Scroll down if the cursor is below the visible area.
        scroll_y = scroll_y.max(cursor_y - viewport_height + 1);

        tc.scroll_offset.x = scroll_x;
        tc.scroll_offset.y = scroll_y;
    }

    fn textarea_adjust_scroll_offset(&self, tc: &mut TextareaContent, scroll_x_before: CoordType) {
        let tb = tc.buffer.borrow();
        let mut scroll_x = tc.scroll_offset.x;
        let mut scroll_y = tc.scroll_offset.y;

        // Keep the last columns of the widest visible row on screen. Bounds
        // advancing only: scrolling onto shorter rows leaves the offset where
        // the reader put it rather than yanking the text sideways.
        let bound = tc.scroll_offset_x_max.max(tb.cursor_visual_pos().x) - 10;
        scroll_x = scroll_x.min(bound.max(scroll_x_before));
        scroll_x = scroll_x.max(0);
        scroll_y = scroll_y.clamp(0, tb.visual_line_count() - 1);

        if tb.is_word_wrap_enabled() {
            scroll_x = 0;
        }

        tc.scroll_offset.x = scroll_x;
        tc.scroll_offset.y = scroll_y;
    }
}
