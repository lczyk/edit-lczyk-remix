//! Eat's alt-screen view, mounted through `edit::mount`.
//!
//! The keymap and the terminal session live in [`super::viewer`]; what
//! stays here is the view itself: one [`Source`] in a read-only
//! `TextBuffer` in edit's textarea (cursor / scroll / selection /
//! mouse-wheel native). `r` loads the source again, `q` exits. A tick
//! drives the poll: static, it only raises the `[modified on disk]` marker
//! when the source's probe says so; live (`-w`), it loads whenever the
//! source may have changed. The view never asks what kind of source it
//! has: a load is started and polled, and lands when it lands. The model
//! of both modes is in `doc/spec/eat-viewer.fizz`.
//!
//! Long lines wrap by default (`--wrap`); `w` toggles, and Left/Right
//! (or `h`/`l`) scroll horizontally while wrap is off.

use std::io;
use std::time::{Duration, Instant};

use lsh::runtime::Language;

use crate::buffer::{RcTextBuffer, TextBuffer};
use crate::eat::source::{Content, Load, Probe, Source};
use crate::eat::viewer::{self, ViewerKey};

/// The header's one marker slot. `Modified` is the probe noticing a
/// change behind the viewer; `Note` is what the last load had to say --
/// a command that did not exit 0, or a source that could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Marker {
    None,
    Modified,
    Note(String),
}

/// How often the loop wakes while a load is in flight, so its result
/// lands promptly. Idle, the poll interval rules.
const RUN_POLL: Duration = Duration::from_millis(100);

/// How long a freshly started load is given to land before the view
/// paints `[running]`: a `cat` lands inside this and never flashes.
const RUN_GRACE: Duration = Duration::from_millis(40);

/// Run the snapshot tui pager over `source`, mounted via
/// [`crate::mount::mount`]. Textarea handles cursor, scroll, selection
/// natively. `q` exits, `r` reloads, `w` toggles wrap, Left/Right scroll
/// horizontally while wrap is off.
///
/// `lang` is `None` when nothing but the content can settle the language
/// (a command's output with no `-l`); it is then sniffed off the first
/// load.
///
/// `watch` makes the view live: every interval the source is probed and,
/// unless the probe says nothing changed, loaded again as if `r` had been
/// pressed. Static, the probe only raises the `[modified on disk]`
/// marker, and a source without a probe is not touched until `r`.
///
/// A load may take time (a command runs in the background): the view
/// paints at once with `[running]` in the header, keeps answering keys,
/// and shows the result when it lands. One load at a time; a poll or `r`
/// that finds one in flight does nothing.
///
/// Scope dropped (TODO(lczyk)):
/// - `--color=never` override. edit's tui has no plain-mode toggle yet.
pub(crate) fn run_snapshot(
    mut source: Source,
    lang: Option<&'static Language>,
    show_numbers: bool,
    _use_color: bool,
    wrap: bool,
    watch: Option<Duration>,
) -> io::Result<()> {
    use std::ops::ControlFlow;

    use crate::helpers::{CoordType, Point, Size};
    use crate::mount;

    let buf =
        TextBuffer::new_rc(false).map_err(|e| io::Error::other(format!("text buffer: {e:?}")))?;
    let path_label = source.label();
    let mut lang = lang;
    {
        let mut b = buf.borrow_mut();
        if let Some(lang) = lang {
            b.set_language(lang);
        }
        b.set_margin_enabled(show_numbers);
        b.set_word_wrap(wrap);
        b.set_read_only(true);
    }

    // The first load. A source that cannot be started or read at all is
    // an error to exit with; one that is slow lands later, under
    // `[running]`.
    source.start().map_err(|e| io::Error::other(format!("{path_label}: {e}")))?;
    let mut marker = match source.wait_up_to(RUN_GRACE) {
        Some(Err(note)) => return Err(io::Error::other(format!("{path_label}: {note}"))),
        Some(Ok(load)) => apply(&buf, Ok(load), &mut lang, show_numbers, wrap),
        None => Marker::None,
    };

    let mut wrap = wrap;
    let mut last_poll = Instant::now();
    let mut header = snapshot_header(&path_label, &marker, watch, source.is_running());
    let poll_interval = watch.unwrap_or(Duration::from_secs(2));
    let tick_override =
        std::rc::Rc::new(std::cell::Cell::new(source.is_running().then_some(RUN_POLL)));

    let _session = viewer::ViewerSession::begin()?;

    let opts = mount::MountOpts {
        tick_interval: Some(poll_interval),
        tick_override: Some(tick_override.clone()),
        on_probe: Some(reflow_on_ambiguous_width(buf.clone())),
        ..Default::default()
    };
    mount::mount(opts, |ctx| -> ControlFlow<()> {
        // What this frame changed, so the header is rebuilt once at the end.
        let mut touched = false;

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
                // A load already in flight is the reload; nothing more.
                ViewerKey::Reload if source.is_running() => {}
                ViewerKey::Reload => {
                    if let Some(load) = begin_load(&mut source, RUN_GRACE) {
                        marker = apply(&buf, load, &mut lang, show_numbers, wrap);
                    }
                    last_poll = Instant::now();
                    touched = true;
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

        // A load in flight: has it landed? Checked every frame; the tick
        // override keeps frames coming at RUN_POLL while one is out.
        if let Some(load) = source.poll() {
            marker = apply(&buf, load, &mut lang, show_numbers, wrap);
            last_poll = Instant::now();
            touched = true;
        }

        // The poll. Fires at most every `poll_interval`; mount's
        // tick_interval keeps the loop awake to hit this branch even with
        // no user input. Live, anything but a clean "unchanged" is loaded
        // again; static, the probe only moves the marker, and a source
        // without one is left alone (there is nothing cheap to ask it).
        let now = Instant::now();
        if !source.is_running() && now.duration_since(last_poll) >= poll_interval {
            last_poll = now;
            let next = match (watch.is_some(), source.probe()) {
                (true, Probe::Changed | Probe::Unknown) => {
                    touched = true;
                    match begin_load(&mut source, Duration::ZERO) {
                        Some(load) => apply(&buf, load, &mut lang, show_numbers, wrap),
                        None => marker.clone(),
                    }
                }
                (true, Probe::Unchanged) | (false, Probe::Unknown) => marker.clone(),
                (false, probe) => match (&marker, probe) {
                    (Marker::Note(_), _) => marker.clone(),
                    (_, Probe::Changed) => Marker::Modified,
                    (_, Probe::Unchanged) => Marker::None,
                    (_, Probe::Unknown) => unreachable!("matched above"),
                },
            };
            if next != marker {
                marker = next;
                touched = true;
            }
        }

        if touched {
            tick_override.set(source.is_running().then_some(RUN_POLL));
            header = snapshot_header(&path_label, &marker, watch, source.is_running());
            ctx.needs_rerender();
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

/// Start a load and give it `grace` to land. `Some` is a result to apply
/// now; `None` means it is still running. A source that cannot be started
/// is a result too: the note to show.
fn begin_load(source: &mut Source, grace: Duration) -> Option<Result<Load, String>> {
    if let Err(note) = source.start() {
        return Some(Err(note));
    }
    source.wait_up_to(grace)
}

/// A landed load into the buffer, keeping the viewport. Returns the
/// marker to show: what the load had to say, or none. An unchanged load
/// leaves the buffer alone, it already shows the content.
fn apply(
    buf: &RcTextBuffer,
    load: Result<Load, String>,
    lang: &mut Option<&'static Language>,
    show_numbers: bool,
    wrap: bool,
) -> Marker {
    let snapshot = match load {
        Err(note) => return Marker::Note(note),
        Ok(Load::Unchanged(failure)) => return note_of(failure),
        Ok(Load::New(snapshot)) => snapshot,
    };
    let mut b = buf.borrow_mut();
    b.set_read_only(false);
    let read = match snapshot.content {
        Content::Reader { mut reader, size_hint } => b.read_from(&mut *reader, size_hint),
        Content::Bytes(bytes) => b.read_from(&mut &bytes[..], Some(bytes.len())),
        // The rows bring their own colours; the load clears the previous
        // set, so they go on after it.
        Content::Rows(rows) => {
            let mut text = String::new();
            for row in &rows {
                text.push_str(&row.text);
                text.push('\n');
            }
            let read = b.read_from(&mut text.as_bytes(), Some(text.len()));
            b.set_pinned_highlights(Some(rows.iter().map(|r| r.pinned()).collect()));
            read
        }
    };
    let marker = match read {
        Err(e) => Marker::Note(e.to_string()),
        Ok(()) => {
            b.set_margin_enabled(show_numbers);
            b.set_word_wrap(wrap);
            b.request_scroll_bound_to_tail();
            if lang.is_none() {
                *lang = Some(settle_language(&mut b));
            }
            note_of(snapshot.failure)
        }
    };
    b.set_read_only(true);
    marker
}

fn note_of(failure: Option<crate::eat::source::Failure>) -> Marker {
    match failure {
        Some(f) => Marker::Note(f.note()),
        None => Marker::None,
    }
}

/// Sniff the language off the buffer's head: a command's output has no
/// path to go by.
fn settle_language(b: &mut TextBuffer) -> &'static Language {
    let head = b.read_forward(0);
    let head = head[..head.len().min(4096)].to_vec();
    let lang = lsh_defs::detect::resolve(None, lsh_defs::detect::NO_USER_ASSOCIATIONS, || head);
    b.set_language(lang);
    lang
}

/// The marker leads so a long path cannot push it off a narrow screen.
fn snapshot_header(
    path_label: &str,
    marker: &Marker,
    watch: Option<Duration>,
    running: bool,
) -> String {
    let run = if running { "[running] " } else { "" };
    let lead = match marker {
        Marker::None => run.to_string(),
        Marker::Modified => format!("{run}[modified on disk] "),
        Marker::Note(note) => format!("{run}[{note}] "),
    };
    let live = match watch {
        Some(every) => format!("live {}ms; ", every.as_millis()),
        None => String::new(),
    };
    format!(
        "{lead}{path_label} @ {}  ({live}q exit, r reload, w wrap, arrows/g/G/PgUp/PgDn scroll)",
        format_clock(),
    )
}

/// `MountOpts::on_probe` callback: reflow the buffer iff the terminal
/// reported ambiguous-width 2.
///
/// The view reads the source into a `TextBuffer` before mounting, so the
/// initial measurement ran at width 1. `mount` applies the probed width
/// globally, but already-measured content keeps its stale wrap points
/// until something recomputes them.
fn reflow_on_ambiguous_width(buf: RcTextBuffer) -> crate::mount::ProbeCallback {
    Box::new(move |probe| {
        if probe.ambiguous_width == 2 {
            buf.borrow_mut().reflow();
        }
    })
}

/// approximate hh:mm:ss using system time. avoids a chrono dep.
fn format_clock() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{h:02}:{m:02}:{s:02}")
}
