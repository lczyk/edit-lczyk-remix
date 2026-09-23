//! Eat's two alt-screen views, both mounted through `edit::mount`.
//!
//! Shared scaffolding -- the keymap and the terminal session -- lives in
//! [`super::viewer`]; what stays here is what the two views genuinely do
//! differently.
//!
//! - **snapshot view** (`run_snapshot`) -- `eat <file>` on a tty.
//!   Read-only `TextBuffer` + edit's textarea (cursor / scroll /
//!   selection / mouse-wheel native). A 2s `tick_interval` drives the
//!   `[modified on disk]` flag; `r` reloads, `q` exits.
//! - **follow view** (`run_follow_mount`) -- `eat -f <file>`.
//!   Funnel-modelled drain into the same `TextBuffer`: stat each tick,
//!   on growth append only the new bytes via `write_raw`; on rotation
//!   reload via `read_file`. Funnel-style `pause_offset` tracks
//!   follow/paused; wheel-up or Up/PgUp/g/Home pauses, End/G or
//!   wheel-down to the bottom resumes follow. `q/esc` exit.
//!
//! Both views go through `edit::mount::mount`; the bespoke alt-screen
//! driver (own vt parser, own viewport state, own line ring) is gone
//! as of phase C.5.
//!
//! Both wrap long lines by default (`--wrap`); `w` toggles, and
//! Left/Right (or `h`/`l`) scroll horizontally while wrap is off. Line
//! counts shown in the headers are always logical lines -- wrap is a
//! display concern and must not move them.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lsh::runtime::Language;

use crate::eat::viewer::{self, ViewerKey};
use crate::watch::{self, FileDelta, FileStat};

// --- snapshot driver -----------------------------------------------------

/// Run the snapshot tui pager. Reads the file into a [`TextBuffer`]
/// (marked read-only) and mounts edit's tui via [`crate::mount::mount`].
/// Textarea handles cursor, scroll, selection natively. `q` exits, `w`
/// toggles wrap, Left/Right scroll horizontally while wrap is off.
///
/// Scope dropped (TODO(lczyk)):
/// - `--color=never` override. edit's tui has no plain-mode toggle yet.
pub fn run_snapshot(
    path: PathBuf,
    lang: &'static Language,
    show_numbers: bool,
    _use_color: bool,
    wrap: bool,
) -> io::Result<()> {
    use std::ops::ControlFlow;

    use crate::buffer::TextBuffer;
    use crate::helpers::{CoordType, Point, Size};

    use crate::mount;

    let buf =
        TextBuffer::new_rc(false).map_err(|e| io::Error::other(format!("text buffer: {e:?}")))?;
    {
        let mut b = buf.borrow_mut();
        let with_path = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
        let mut f =
            std::fs::File::open(&path).map_err(|e| io::Error::new(e.kind(), with_path(&e)))?;
        b.read_file(&mut f).map_err(|e| io::Error::other(with_path(&e)))?;
        b.set_language(lang);
        b.set_margin_enabled(show_numbers);
        b.set_word_wrap(wrap);
        b.set_read_only(true);
    }

    let mut wrap = wrap;
    let path_label = path.display().to_string();
    let mut captured_stat = FileStat::from_path(&path).ok();
    let mut file_changed = false;
    let mut last_disk_check = Instant::now();
    let mut header = snapshot_header(&path_label, Instant::now(), file_changed);
    let disk_check_interval = Duration::from_secs(2);

    let _session = viewer::ViewerSession::begin()?;

    let opts = mount::MountOpts {
        tick_interval: Some(disk_check_interval),
        on_probe: Some(reflow_on_ambiguous_width(buf.clone())),
        ..Default::default()
    };
    mount::mount(opts, |ctx| -> ControlFlow<()> {
        // The textarea is mounted unfocused (no cursor block painted), so
        // it ignores keys and we translate them to scroll requests here.
        if let Some(k) = ctx.keyboard_input()
            && let Some(action) = viewer::classify(k)
        {
            let body_h = (ctx.size().height - 1).max(1) as CoordType;
            ctx.set_input_consumed();
            match action {
                ViewerKey::Quit => return ControlFlow::Break(()),
                ViewerKey::Copy => buf.borrow_mut().copy(ctx.clipboard_mut()),
                ViewerKey::SelectAll => buf.borrow_mut().select_all(),
                ViewerKey::ToggleWrap => {
                    wrap = !wrap;
                    buf.borrow_mut().set_word_wrap(wrap);
                    ctx.needs_rerender();
                }
                ViewerKey::Reload => {
                    if let Ok(mut f) = std::fs::File::open(&path) {
                        let mut b = buf.borrow_mut();
                        b.set_read_only(false);
                        let _ = b.read_file(&mut f);
                        b.set_margin_enabled(show_numbers);
                        b.set_word_wrap(wrap);
                        b.set_read_only(true);
                    }
                    captured_stat = FileStat::from_path(&path).ok();
                    file_changed = false;
                    last_disk_check = Instant::now();
                    header = snapshot_header(&path_label, Instant::now(), file_changed);
                    ctx.needs_rerender();
                }
                ViewerKey::ScrollLines(d) => buf.borrow_mut().request_scroll_delta_y(d),
                ViewerKey::ScrollPages(p) => {
                    buf.borrow_mut().request_scroll_delta_y(p * (body_h - 1).max(1));
                }
                ViewerKey::ScrollColumns(d) => buf.borrow_mut().request_scroll_delta_x(d),
                ViewerKey::ToTop => {
                    let n = buf.borrow().visual_line_count();
                    buf.borrow_mut().request_scroll_delta_y(-n);
                }
                ViewerKey::ToBottom => {
                    let mut b = buf.borrow_mut();
                    b.cursor_move_to_logical(Point::MAX);
                    b.request_scroll_to_tail();
                }
            }
        }

        // disk-change poll. fires at most every `disk_check_interval`;
        // mount's tick_interval keeps the loop awake to hit this branch
        // even with no user input.
        let now = Instant::now();
        if now.duration_since(last_disk_check) >= disk_check_interval {
            last_disk_check = now;
            let now_stat = FileStat::from_path(&path).ok();
            let changed = match (&captured_stat, &now_stat) {
                (Some(a), Some(b)) => a != b,
                (Some(_), None) => true,
                _ => false,
            };
            if changed != file_changed {
                file_changed = changed;
                header = snapshot_header(&path_label, Instant::now(), file_changed);
                ctx.needs_rerender();
            }
        }

        let size = ctx.size();
        ctx.label("snapshot-header", &header);

        // NB: no `inherit_focus()` -- unfocused textarea suppresses the
        // terminal cursor. mouse-wheel scroll still works (handled
        // pre-focus-check in textarea_handle_input).
        ctx.textarea("snapshot-body", buf.clone());
        let body_h = (size.height - 1).max(1) as CoordType;
        ctx.attr_intrinsic_size(Size { width: 0, height: body_h });

        ControlFlow::Continue(())
    })
}

fn snapshot_header(path_label: &str, at: Instant, file_changed: bool) -> String {
    let delta = if file_changed { "  [modified on disk]" } else { "" };
    format!(
        "{path_label} @ {}{delta}  (q exit, r reload, w wrap, arrows/g/G/PgUp/PgDn scroll)",
        format_clock(at),
    )
}

/// Mount-based follow view -- the default `eat -f` tty pager and the
/// only path exercised by `edit --follow`.
///
/// Drain loop is modelled on funnel's: stat the file each tick (cheap),
/// branch on (rotated || size unchanged || grown), read only the new
/// bytes from `last_size..current_size`, append via
/// `TextBuffer::write_raw` so the rope grows in place and the
/// highlighter cache only invalidates from the modified line down.
/// Rotation (inode change or size shrink) falls back to a full reload
/// via `read_file`.
///
/// Funnel-style follow/paused: `pause_offset` mirrors funnel's
/// `display_offset` (lines above the live bottom). Transitions through
/// 0 re-enter follow. Wheel-up enters pause; wheel-down toward the
/// bottom resumes. Key bindings: Up/Dn, j/k, Left/Right, h/l, g/G, Home,
/// End, PgUp, PgDn, w, q/esc.
///
/// The textarea is mounted **without** focus. Unfocused textareas:
/// - do not paint the terminal cursor (eat is a viewer, not an editor),
/// - still accept mouse-wheel scroll (handled before the focus check),
/// - ignore keyboard input -- we translate keys to
///   `request_scroll_delta_y` calls in this callback instead.
pub fn run_follow_mount(
    path: PathBuf,
    lang: &'static Language,
    show_numbers: bool,
    _use_color: bool,
    poll_interval: Duration,
    wrap: bool,
) -> io::Result<()> {
    use std::ops::ControlFlow;

    use crate::buffer::TextBuffer;
    use crate::helpers::{CoordType, Point, Size};

    use crate::mount;

    // Tail snap: the last line lands on the bottom edge and the horizontal
    // offset is left alone, so a reader scrolled right stays there as the
    // file grows. The cursor rides along so a copy still takes the tail.
    fn snap_to_tail(b: &mut TextBuffer) {
        b.cursor_move_to_logical(Point::MAX);
        b.request_scroll_to_tail();
    }

    // initial load: full read via `read_file` so encoding / line-ending
    // detection runs once.
    let buf =
        TextBuffer::new_rc(false).map_err(|e| io::Error::other(format!("text buffer: {e:?}")))?;
    {
        let mut b = buf.borrow_mut();
        let with_path = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
        let mut f =
            std::fs::File::open(&path).map_err(|e| io::Error::new(e.kind(), with_path(&e)))?;
        b.read_file(&mut f).map_err(|e| io::Error::other(with_path(&e)))?;
        b.set_language(lang);
        b.set_margin_enabled(show_numbers);
        b.set_word_wrap(wrap);
        b.set_read_only(true);
        snap_to_tail(&mut b);
    }

    let path_label = path.display().to_string();

    // funnel-style follow/paused state. See module-level docs for the
    // pause_offset mirror semantics; body_h is recomputed per frame and
    // pause_offset is clamped against `visual_line_count - body_h` so
    // wheel-up past the top doesn't accumulate phantom offset that the
    // textarea won't honour.
    let mut following = true;
    let mut pause_offset: CoordType = 0;

    // `w` flips wrap, but the reflow only lands when the textarea calls
    // `set_width` later in the same frame -- so the re-anchor is deferred to
    // the next frame, when the new visual geometry is readable.
    let mut wrap = wrap;
    let mut pending_rewrap: Option<Rewrap> = None;
    let mut top_line_cache = TopLineCache::default();

    let mut wheel_accel = WheelAccel::default();

    let _session = viewer::ViewerSession::begin()?;

    let mut drain_state = DrainState::new(&path);

    // tick at ~30fps regardless of the user's poll_interval. stat is
    // cheap; reload still gated on stat change. Decoupling the wake
    // rate from the reload rate is what makes a fast-growing log read
    // as smooth scrolling rather than 4Hz chunk-jumps. honour the
    // user's `-f <interval>` only as a floor (don't wake faster than
    // requested if they explicitly want less frequent polling).
    let tick = poll_interval.min(Duration::from_millis(33));
    let opts = mount::MountOpts {
        tick_interval: Some(tick),
        on_probe: Some(reflow_on_ambiguous_width(buf.clone())),
        ..Default::default()
    };
    mount::mount(opts, |ctx| -> ControlFlow<()> {
        let body_h = (ctx.size().height - 1).max(1) as CoordType;

        // deferred re-anchor after a wrap toggle. runs before anything reads
        // the geometry, now that last frame's textarea pass has reflowed.
        match pending_rewrap.take() {
            Some(Rewrap::Tail) => {
                snap_to_tail(&mut buf.borrow_mut());
                pause_offset = 0;
                following = true;
                ctx.needs_rerender();
            }
            Some(Rewrap::Line { top_logical, old_scroll_y }) => {
                let mut b = buf.borrow_mut();
                b.cursor_move_to_logical(Point { x: 0, y: top_logical - 1 });
                let new_top = b.cursor_visual_pos().y;
                let new_total = b.visual_line_count();
                // the textarea kept its old `scroll_offset.y` across the
                // reflow, clamped to the new row count; steer from there.
                let cur_scroll_y = old_scroll_y.clamp(0, (new_total - 1).max(0));
                b.request_scroll_delta_y(new_top - cur_scroll_y);
                pause_offset = (new_total - body_h - new_top).max(0);
                following = pause_offset == 0;
                ctx.needs_rerender();
            }
            None => {}
        }

        let max_offset = (buf.borrow().visual_line_count() - body_h).max(0);

        // helper: apply a scroll delta and update pause_offset / follow
        // state in lockstep. `delta` is the same value passed to the
        // textarea: positive = scroll down (toward bottom).
        let mut apply_scroll = |delta: CoordType| {
            let (new_off, foll) = apply_scroll_pure(pause_offset, delta, max_offset);
            pause_offset = new_off;
            following = foll;
        };

        // wheel-input is applied by the textarea on its own, but we
        // still need to mirror its effect on pause_offset so the
        // follow/paused state stays in sync.
        let raw_wheel = ctx.scroll_delta().y;
        if raw_wheel != 0 {
            // ramp ride: fast spinning bumps the wheel-line multiplier
            // (funnel WheelAccel ported verbatim). only used to update
            // our shadow -- textarea has already applied raw_wheel to
            // its own scroll_offset.
            let dir = if raw_wheel < 0 { ScrollDir::Up } else { ScrollDir::Down };
            let factor = wheel_accel.lines(Instant::now(), dir) as CoordType;
            // the textarea only knows the raw delta; we apply the same
            // raw delta to pause_offset so they stay in sync. accel is
            // ignored on the textarea side b/c we don't have a way to
            // boost its scroll from out here. revisit if wheel feel
            // becomes a problem.
            let _ = factor;
            apply_scroll(raw_wheel);
        }

        if let Some(k) = ctx.keyboard_input()
            && let Some(action) = viewer::classify(k)
        {
            ctx.set_input_consumed();
            match action {
                ViewerKey::Quit => return ControlFlow::Break(()),
                ViewerKey::Copy => buf.borrow_mut().copy(ctx.clipboard_mut()),
                ViewerKey::SelectAll => buf.borrow_mut().select_all(),
                // Reload is snapshot-only; the drain already tracks the file.
                ViewerKey::Reload => {}
                ViewerKey::ToggleWrap => {
                    wrap = !wrap;
                    // Re-wrapping moves every visual line, so remember what
                    // to re-anchor on: the tail if following, otherwise the
                    // logical line currently at the top of the viewport.
                    pending_rewrap = Some(if following {
                        Rewrap::Tail
                    } else {
                        let b = buf.borrow();
                        let old_scroll_y = (b.visual_line_count() - body_h - pause_offset).max(0);
                        drop(b);
                        Rewrap::Line {
                            top_logical: top_line_cache.get(&buf, old_scroll_y),
                            old_scroll_y,
                        }
                    });
                    buf.borrow_mut().set_word_wrap(wrap);
                    ctx.needs_rerender();
                }
                // Vertical movement is mirrored onto pause_offset so the
                // follow/paused state stays in lockstep with the textarea.
                ViewerKey::ScrollLines(d) => {
                    buf.borrow_mut().request_scroll_delta_y(d);
                    apply_scroll(d);
                }
                ViewerKey::ScrollPages(p) => {
                    let d = p * (body_h - 1).max(1);
                    buf.borrow_mut().request_scroll_delta_y(d);
                    apply_scroll(d);
                }
                ViewerKey::ScrollColumns(d) => buf.borrow_mut().request_scroll_delta_x(d),
                ViewerKey::ToTop => {
                    let n = buf.borrow().visual_line_count();
                    buf.borrow_mut().request_scroll_delta_y(-n);
                    apply_scroll(-n);
                }
                ViewerKey::ToBottom => {
                    snap_to_tail(&mut buf.borrow_mut());
                    pause_offset = 0;
                    following = true;
                }
            }
        }

        // drain: stat, branch on (rotated, idle, grown), feed
        // append-bytes through TextBuffer::write_raw. mirror of funnel's
        // `drain` minus the per-line emit (we feed the buffer in one
        // chunk; the textarea handles wrapping/highlighting at paint
        // time).
        let outcome = drain_into_buffer(&path, &buf, &mut drain_state, show_numbers);
        if !matches!(outcome, DrainOutcome::Idle) {
            if following {
                snap_to_tail(&mut buf.borrow_mut());
            }
            // paused branch is a no-op: scroll_offset.y is a top-line
            // index that stays put as the buffer grows, so already-
            // visible content stays anchored.
            ctx.needs_rerender();
        }

        // counts shown to the user are logical lines -- wrap is a display
        // concern, so "line 40/120" must not change when `w` is pressed.
        // the scroll bookkeeping above stays in visual rows.
        let scroll_y = {
            let b = buf.borrow();
            (b.visual_line_count() - body_h - pause_offset).max(0)
        };
        let counts = HeaderCounts {
            top_line: top_line_cache.get(&buf, scroll_y),
            total_lines: buf.borrow().logical_line_count(),
        };
        let header =
            follow_mount_header(&path_label, Instant::now(), following, poll_interval, counts);

        let size = ctx.size();
        ctx.label("follow-header", &header);

        // NB: no `inherit_focus()` -- unfocused textarea suppresses the
        // terminal cursor. mouse-wheel scroll still works (handled
        // pre-focus-check in textarea_handle_input).
        ctx.textarea("follow-body", buf.clone());
        let body_h = (size.height - 1).max(1) as CoordType;
        ctx.attr_intrinsic_size(Size { width: 0, height: body_h });

        ControlFlow::Continue(())
    })
}

/// Mouse-wheel acceleration ported from funnel. Slow spins return 1
/// line; sustained fast spinning ramps to 2. Streak counts up on
/// FAST_THRESHOLD ticks, decays per medium tick, resets on direction
/// flip or after RESET_MS of quiet. Kept as a struct (and exported
/// internally as `ScrollDir`) so the C.3 work that adds wheel-boost on
/// the textarea side has somewhere to plug in.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum ScrollDir {
    Up,
    Down,
}

#[derive(Default)]
struct WheelAccel {
    last_tick: Option<Instant>,
    last_dir: Option<ScrollDir>,
    fast_streak: u32,
}

const WHEEL_FAST_THRESHOLD_MS: u128 = 60;
const WHEEL_RESET_MS: u128 = 250;
const WHEEL_STREAK_MAX: u32 = 12;

impl WheelAccel {
    fn lines(&mut self, now: Instant, dir: ScrollDir) -> usize {
        let dt = self.last_tick.map(|t| now.duration_since(t).as_millis()).unwrap_or(u128::MAX);
        let dir_flipped = self.last_dir.is_some_and(|d| d != dir);
        self.last_tick = Some(now);
        self.last_dir = Some(dir);
        if dir_flipped || dt > WHEEL_RESET_MS {
            self.fast_streak = 0;
        } else if dt < WHEEL_FAST_THRESHOLD_MS {
            self.fast_streak = (self.fast_streak + 1).min(WHEEL_STREAK_MAX);
        } else {
            self.fast_streak = self.fast_streak.saturating_sub(1);
        }
        match self.fast_streak {
            0..=3 => 1,
            _ => 2,
        }
    }
}

struct HeaderCounts {
    top_line: crate::helpers::CoordType,
    total_lines: crate::helpers::CoordType,
}

/// What a pending wrap toggle should re-anchor the viewport to once the
/// reflow has landed. See `pending_rewrap` in `run_follow_mount`.
enum Rewrap {
    Tail,
    Line { top_logical: crate::helpers::CoordType, old_scroll_y: crate::helpers::CoordType },
}

/// Memo for the header's top-visible logical line. Resolving a visual row to a
/// logical one is a measurement walk and the header is rebuilt every tick
/// (~30fps), so only recompute when the viewport or the content moved.
#[derive(Default)]
struct TopLineCache {
    key: Option<(crate::helpers::CoordType, u32)>,
    line: crate::helpers::CoordType,
}

impl TopLineCache {
    fn get(
        &mut self,
        buf: &crate::buffer::RcTextBuffer,
        scroll_y: crate::helpers::CoordType,
    ) -> crate::helpers::CoordType {
        let b = buf.borrow();
        let key = (scroll_y, b.generation());
        if self.key != Some(key) {
            self.key = Some(key);
            self.line = b.resolve_visual_pos(crate::helpers::Point { x: 0, y: scroll_y }).0.y + 1;
        }
        self.line
    }
}

/// `MountOpts::on_probe` callback: reflow the buffer iff the terminal
/// reported ambiguous-width 2.
///
/// Both views read the file into a `TextBuffer` before mounting, so the
/// initial measurement ran at width 1. `mount` applies the probed width
/// globally, but already-measured content keeps its stale wrap points
/// until something recomputes them.
fn reflow_on_ambiguous_width(buf: crate::buffer::RcTextBuffer) -> crate::mount::ProbeCallback {
    Box::new(move |probe| {
        if probe.ambiguous_width == 2 {
            buf.borrow_mut().reflow();
        }
    })
}

fn follow_mount_header(
    path_label: &str,
    at: Instant,
    following: bool,
    poll: Duration,
    counts: HeaderCounts,
) -> String {
    let poll_ms = poll.as_millis();
    let mode = if following { "following" } else { "paused" };
    let HeaderCounts { top_line, total_lines } = counts;
    format!(
        "{path_label} [{mode}] line {top_line}/{total_lines} @ {}  ({poll_ms}ms arrows/g/G/PgUp/PgDn scroll, w wrap, q)",
        format_clock(at),
    )
}

/// approximate hh:mm:ss using system time. avoids a chrono dep.
fn format_clock(_now: Instant) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{h:02}:{m:02}:{s:02}")
}

// --- testable extractions ------------------------------------------------

/// Last-observed stat plus the head sample backing the rewrite check.
pub(crate) struct DrainState {
    pub last: FileStat,
    pub head: Vec<u8>,
}

impl DrainState {
    pub fn new(path: &Path) -> Self {
        let last = FileStat::from_path(path).unwrap_or(FileStat { size: 0, id: 0, mtime_ns: 0 });
        Self { head: watch::read_head(path), last }
    }
}

/// What [`drain_into_buffer`] decided this tick.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DrainOutcome {
    /// stat failed or file unchanged.
    Idle,
    /// file grew; new bytes appended to the buffer.
    Grew,
    /// contents replaced; buffer reloaded from scratch.
    Rotated,
}

/// Drain one tick: stat the file, classify, act.
///
/// Classification is [`watch::classify`], shared with the streaming
/// follow path so both agree on what counts as a rotation -- notably
/// including the same-length in-place rewrite that size and inode alone
/// cannot see.
pub(crate) fn drain_into_buffer(
    path: &Path,
    buf: &crate::buffer::RcTextBuffer,
    state: &mut DrainState,
    show_numbers: bool,
) -> DrainOutcome {
    use std::io::{Read as _, Seek as _};

    let Ok(curr) = FileStat::from_path(path) else {
        return DrainOutcome::Idle;
    };

    // Cheap gate: skip the head read on the common idle tick.
    if !curr.differs(&state.last) {
        return DrainOutcome::Idle;
    }

    let curr_head = watch::read_head(path);
    let head_changed = watch::head_changed(&state.head, &curr_head);
    let delta = watch::classify(&state.last, &curr, head_changed);

    // Commit the observation before acting: even if the read below fails
    // we've seen this state, and re-reporting it every tick would spin.
    state.last = curr;
    state.head = curr_head;

    match delta {
        FileDelta::Idle => DrainOutcome::Idle,
        FileDelta::Rotated => {
            if let Ok(mut f) = std::fs::File::open(path) {
                let mut b = buf.borrow_mut();
                b.set_read_only(false);
                let _ = b.read_file(&mut f);
                b.set_margin_enabled(show_numbers);
                b.set_read_only(true);
            }
            DrainOutcome::Rotated
        }
        FileDelta::Appended { from } => {
            let mut chunk = Vec::with_capacity(curr.size.saturating_sub(from) as usize);
            if let Ok(mut f) = std::fs::File::open(path)
                && f.seek(std::io::SeekFrom::Start(from)).is_ok()
                && f.read_to_end(&mut chunk).is_ok()
            {
                let mut b = buf.borrow_mut();
                b.set_read_only(false);
                b.cursor_move_to_logical(crate::helpers::Point::MAX);
                b.write_raw(&chunk);
                b.set_read_only(true);
                return DrainOutcome::Grew;
            }
            DrainOutcome::Idle
        }
    }
}

/// Funnel-style scroll-delta application. `delta` is the value passed to
/// the textarea (positive = scroll down toward the bottom). Returns the
/// new `pause_offset` and the new `following` flag.
pub(crate) fn apply_scroll_pure(
    pause_offset: crate::helpers::CoordType,
    delta: crate::helpers::CoordType,
    max_offset: crate::helpers::CoordType,
) -> (crate::helpers::CoordType, bool) {
    let new_off = (pause_offset - delta).clamp(0, max_offset);
    (new_off, new_off == 0)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;
    use crate::buffer::TextBuffer;

    // --- apply_scroll_pure ---

    #[test]
    fn scroll_up_from_follow_enters_pause() {
        let (off, foll) = apply_scroll_pure(0, -1, 100);
        assert_eq!(off, 1);
        assert!(!foll);
    }

    #[test]
    fn scroll_down_to_zero_resumes_follow() {
        let (off, foll) = apply_scroll_pure(1, 1, 100);
        assert_eq!(off, 0);
        assert!(foll);
    }

    #[test]
    fn scroll_clamps_at_max() {
        // user wheel-up past content top -- offset pegs at max_offset.
        let (off, foll) = apply_scroll_pure(50, -1000, 50);
        assert_eq!(off, 50);
        assert!(!foll);
    }

    #[test]
    fn scroll_clamps_at_zero() {
        // user wheel-down past tail -- offset pegs at 0, follow re-enters.
        let (off, foll) = apply_scroll_pure(3, 100, 100);
        assert_eq!(off, 0);
        assert!(foll);
    }

    #[test]
    fn scroll_zero_delta_is_noop() {
        let (off, foll) = apply_scroll_pure(7, 0, 100);
        assert_eq!(off, 7);
        assert!(!foll);
    }

    // --- drain_into_buffer ---

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        dir.join(format!("edit-follow-mount-{}-{}", std::process::id(), name))
    }

    fn write_file(path: &std::path::Path, content: &[u8]) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content).unwrap();
        f.sync_all().unwrap();
    }

    fn append_file(path: &std::path::Path, more: &[u8]) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(more).unwrap();
        f.sync_all().unwrap();
    }

    fn fresh_buf_with_file(path: &std::path::Path) -> (crate::buffer::RcTextBuffer, DrainState) {
        let buf = TextBuffer::new_rc(false).unwrap();
        {
            let mut b = buf.borrow_mut();
            let mut f = std::fs::File::open(path).unwrap();
            b.read_file(&mut f).unwrap();
            b.set_read_only(true);
        }
        (buf, DrainState::new(path))
    }

    #[test]
    fn drain_idle_when_file_unchanged() {
        let path = tmp_path("idle");
        write_file(&path, b"a\nb\nc\n");
        let (buf, mut state) = fresh_buf_with_file(&path);
        let lines_before = buf.borrow().visual_line_count();
        let outcome = drain_into_buffer(&path, &buf, &mut state, false);
        assert_eq!(outcome, DrainOutcome::Idle);
        assert_eq!(buf.borrow().visual_line_count(), lines_before);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn drain_grew_appends_new_bytes() {
        let path = tmp_path("grew");
        write_file(&path, b"a\nb\nc\n");
        let (buf, mut state) = fresh_buf_with_file(&path);
        let lines_before = buf.borrow().visual_line_count();
        append_file(&path, b"d\ne\n");
        let outcome = drain_into_buffer(&path, &buf, &mut state, false);
        assert_eq!(outcome, DrainOutcome::Grew);
        let lines_after = buf.borrow().visual_line_count();
        assert!(
            lines_after - lines_before >= 2,
            "expected 2+ new lines, got {lines_before} -> {lines_after}",
        );
        // state has caught up.
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(state.last.size, meta.len());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn drain_rotated_on_shrink() {
        let path = tmp_path("rot-shrink");
        write_file(&path, b"old1\nold2\nold3\n");
        let (buf, mut state) = fresh_buf_with_file(&path);
        // truncate + write smaller content: inode usually preserved, size
        // shrinks. drain should reload.
        write_file(&path, b"new\n");
        let outcome = drain_into_buffer(&path, &buf, &mut state, false);
        assert_eq!(outcome, DrainOutcome::Rotated);
        assert_eq!(state.last.size, 4);
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn drain_rotated_on_inode_change() {
        let path = tmp_path("rot-inode");
        write_file(&path, b"old1\nold2\n");
        let (buf, mut state) = fresh_buf_with_file(&path);
        // unlink + recreate: new inode even if same / similar size.
        std::fs::remove_file(&path).unwrap();
        write_file(&path, b"old1\nold2\n");
        let outcome = drain_into_buffer(&path, &buf, &mut state, false);
        assert_eq!(outcome, DrainOutcome::Rotated);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn drain_idle_when_file_missing() {
        let path = tmp_path("missing");
        let _ = std::fs::remove_file(&path);
        // bootstrap state against a path that doesn't exist; drain should
        // not panic.
        let buf = TextBuffer::new_rc(false).unwrap();
        let mut state = DrainState::new(&path);
        let outcome = drain_into_buffer(&path, &buf, &mut state, false);
        assert_eq!(outcome, DrainOutcome::Idle);
    }

    #[test]
    fn drain_rotated_on_same_size_in_place_rewrite() {
        // The case size + inode alone cannot see, and which the streaming
        // follow path has always caught: `> file` with the same byte count
        // leaves both unchanged, so only the head sample reveals that the
        // old contents are gone. Draining this as an append would splice
        // nothing and leave the viewer showing stale text forever.
        let path = tmp_path("rewrite-same-size");
        write_file(&path, b"aaa\nbbb\n");
        let (buf, mut state) = fresh_buf_with_file(&path);
        write_file(&path, b"xxx\nyyy\n"); // identical length
        let outcome = drain_into_buffer(&path, &buf, &mut state, false);
        assert_eq!(outcome, DrainOutcome::Rotated);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn drain_rotated_on_truncate_then_longer_rewrite() {
        // Grew, so it looks like an append -- but the leading bytes are
        // different, so appending from the old size would graft new content
        // onto a prefix that no longer exists.
        let path = tmp_path("rewrite-longer");
        write_file(&path, b"aaa\n");
        let (buf, mut state) = fresh_buf_with_file(&path);
        write_file(&path, b"xxx\nyyy\nzzz\n");
        let outcome = drain_into_buffer(&path, &buf, &mut state, false);
        assert_eq!(outcome, DrainOutcome::Rotated);
        let _ = std::fs::remove_file(&path);
    }

    // --- WheelAccel ---

    #[test]
    fn wheel_accel_slow_returns_one() {
        let mut w = WheelAccel::default();
        let now = Instant::now();
        // single tick after long quiet: streak stays 0, return 1.
        assert_eq!(w.lines(now, ScrollDir::Up), 1);
    }

    #[test]
    fn wheel_accel_fast_streak_ramps_to_two() {
        let mut w = WheelAccel::default();
        let mut t = Instant::now();
        // first call seeds last_tick; subsequent fast ticks accrue streak.
        w.lines(t, ScrollDir::Up);
        for _ in 0..5 {
            t += Duration::from_millis(30); // < WHEEL_FAST_THRESHOLD_MS=60
            w.lines(t, ScrollDir::Up);
        }
        t += Duration::from_millis(30);
        assert_eq!(w.lines(t, ScrollDir::Up), 2);
    }

    #[test]
    fn wheel_accel_dir_flip_resets() {
        let mut w = WheelAccel::default();
        let mut t = Instant::now();
        w.lines(t, ScrollDir::Up);
        for _ in 0..10 {
            t += Duration::from_millis(20);
            w.lines(t, ScrollDir::Up);
        }
        // mid-spin direction flip -> streak resets, back to 1.
        t += Duration::from_millis(20);
        assert_eq!(w.lines(t, ScrollDir::Down), 1);
    }

    #[test]
    fn wheel_accel_quiet_gap_resets() {
        let mut w = WheelAccel::default();
        let mut t = Instant::now();
        w.lines(t, ScrollDir::Up);
        for _ in 0..10 {
            t += Duration::from_millis(20);
            w.lines(t, ScrollDir::Up);
        }
        // long quiet (> WHEEL_RESET_MS=250) -> streak resets.
        t += Duration::from_millis(500);
        assert_eq!(w.lines(t, ScrollDir::Up), 1);
    }

    // --- follow_mount_header ---

    #[test]
    fn header_following_shows_logical_line() {
        let h = follow_mount_header(
            "/tmp/foo.log",
            Instant::now(),
            true,
            Duration::from_millis(250),
            HeaderCounts { top_line: 77, total_lines: 100 },
        );
        assert!(h.contains("[following]"), "got: {h}");
        assert!(h.contains("250ms"));
        assert!(h.contains("line 77/100"), "got: {h}");
    }

    #[test]
    fn header_paused_shows_logical_line() {
        let h = follow_mount_header(
            "/tmp/foo.log",
            Instant::now(),
            false,
            Duration::from_millis(100),
            HeaderCounts { top_line: 12, total_lines: 100 },
        );
        assert!(h.contains("[paused]"), "got: {h}");
        assert!(h.contains("line 12/100"), "got: {h}");
    }

    // --- wrap plumbing ---

    #[test]
    fn top_line_cache_recomputes_on_scroll_and_content() {
        let path = tmp_path("cache");
        write_file(&path, b"aaa\nbbb\nccc\nddd\n");
        let buf = TextBuffer::new_rc(false).unwrap();
        {
            let mut b = buf.borrow_mut();
            let mut f = std::fs::File::open(&path).unwrap();
            b.read_file(&mut f).unwrap();
        }

        let mut cache = TopLineCache::default();
        assert_eq!(cache.get(&buf, 0), 1);
        assert_eq!(cache.get(&buf, 2), 3);
        // repeat hit: same key, same answer, no recompute path taken.
        assert_eq!(cache.get(&buf, 2), 3);

        // appending bumps the generation, so a stale entry can't survive.
        let gen_before = buf.borrow().generation();
        {
            let mut b = buf.borrow_mut();
            b.set_read_only(false);
            b.cursor_move_to_logical(crate::helpers::Point::MAX);
            b.write_raw(b"eee\n");
        }
        assert_ne!(buf.borrow().generation(), gen_before);
        assert_eq!(cache.get(&buf, 4), 5);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn wrap_toggle_keeps_logical_line_count() {
        let path = tmp_path("wrapcount");
        // one long line that wraps into several rows at a narrow width.
        write_file(&path, format!("{}\nshort\n", "x".repeat(200)).as_bytes());
        let buf = TextBuffer::new_rc(false).unwrap();
        {
            let mut b = buf.borrow_mut();
            let mut f = std::fs::File::open(&path).unwrap();
            b.read_file(&mut f).unwrap();
            b.set_width(40);
        }

        let logical = buf.borrow().logical_line_count();
        buf.borrow_mut().set_word_wrap(true);
        buf.borrow_mut().set_width(40);
        assert!(buf.borrow().visual_line_count() > logical, "expected the long line to wrap");
        assert_eq!(buf.borrow().logical_line_count(), logical);

        buf.borrow_mut().set_word_wrap(false);
        buf.borrow_mut().set_width(40);
        assert_eq!(buf.borrow().visual_line_count(), logical);
        assert_eq!(buf.borrow().logical_line_count(), logical);

        std::fs::remove_file(&path).ok();
    }
}
