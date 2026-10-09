//! Eat's alt-screen view, mounted through `edit::mount`.
//!
//! The keymap and the terminal session live in [`super::viewer`]; what
//! stays here is the view itself: `eat <file>` or `eat -x -- <cmd>` on a
//! tty. A read-only `TextBuffer` in edit's textarea (cursor / scroll /
//! selection / mouse-wheel native). `r` reloads, `q` exits. A tick drives
//! the poll: static, it only raises the `[modified on disk]` marker for a
//! file; live (`-w`), it reloads whenever the source changed. The model
//! of both is in `doc/spec/eat-viewer.fizz`.
//!
//! Long lines wrap by default (`--wrap`); `w` toggles, and Left/Right
//! (or `h`/`l`) scroll horizontally while wrap is off.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lsh::runtime::Language;

use crate::eat::exec;
use crate::eat::viewer::{self, ViewerKey};
use crate::watch::FileStat;

// --- snapshot driver -----------------------------------------------------

/// What the snapshot view shows: a file re-read from disk, or the stdout
/// of a command re-run, on every load.
pub enum Source {
    File(PathBuf),
    Command(Vec<String>),
}

/// The header's one marker slot. `Modified` is the file poll noticing a
/// change behind the viewer; `Note` is what the last load had to say --
/// a command that did not exit 0, or a source that could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Marker {
    None,
    Modified,
    Note(String),
}

impl Source {
    fn label(&self) -> String {
        match self {
            Source::File(path) => path.display().to_string(),
            Source::Command(argv) => exec::label(argv),
        }
    }

    /// The file's stat for the change poll; a command has nothing to poll.
    fn stat(&self) -> Option<FileStat> {
        match self {
            Source::File(path) => FileStat::from_path(path).ok(),
            Source::Command(_) => None,
        }
    }

    /// Replace the buffer with the file. `Err` is the note to show and
    /// means the buffer was left alone. A command is not loaded here: it
    /// is started with [`exec::start`] and lands through [`Source::finish`]
    /// when it returns.
    fn load_file(&self, b: &mut crate::buffer::TextBuffer) -> Result<Load, String> {
        match self {
            Source::File(path) => {
                let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
                b.read_file(&mut f).map_err(|e| e.to_string())?;
                Ok(Load::Loaded(None))
            }
            Source::Command(_) => Ok(Load::Unchanged(None)),
        }
    }

    /// A command's output into the buffer. `Err` is the note to show and
    /// means the buffer was left alone: a command that failed without
    /// producing anything. A command that exits nonzero with output
    /// (`diff`, `grep`) is loaded, and the note still says so.
    ///
    /// `last` is the command's previous stdout: the same bytes again are
    /// `Unchanged`, so a live view does not repaint for nothing.
    fn finish(
        b: &mut crate::buffer::TextBuffer,
        out: exec::Output,
        last: &mut Vec<u8>,
    ) -> Result<Load, String> {
        let note = (!out.status.success()).then(|| exec::failure_note(&out));
        if out.stdout.is_empty()
            && let Some(note) = note
        {
            return Err(note);
        }
        if out.stdout == *last {
            return Ok(Load::Unchanged(note));
        }
        b.read_from(&mut &out.stdout[..], Some(out.stdout.len())).map_err(|e| e.to_string())?;
        *last = out.stdout;
        Ok(Load::Loaded(note))
    }
}

enum Load {
    Loaded(Option<String>),
    Unchanged(Option<String>),
}

/// How often the loop wakes while a command is in flight, so its result
/// lands promptly. Idle, the poll interval rules.
const RUN_POLL: Duration = Duration::from_millis(100);

/// How long a freshly started command is given to return before the view
/// paints `[running]`: a `cat` lands inside this and never flashes.
const RUN_GRACE: Duration = Duration::from_millis(40);

/// Run the snapshot tui pager. Loads the source into a [`TextBuffer`]
/// (marked read-only) and mounts edit's tui via [`crate::mount::mount`].
/// Textarea handles cursor, scroll, selection natively. `q` exits, `r`
/// reloads, `w` toggles wrap, Left/Right scroll horizontally while wrap is
/// off.
///
/// `lang` is `None` when nothing but the content can settle the language
/// (a command's output with no `-l`); it is then sniffed after the first
/// load.
///
/// `watch` makes the view live: every interval the source is checked
/// and, if it changed, loaded again as if `r` had been pressed. Static,
/// a file is still polled every 2s for the `[modified on disk]` marker;
/// a command is not run again until `r`.
///
/// A command runs in the background: the view paints at once with
/// `[running]` in the header, keeps answering keys, and loads the output
/// when the command returns. One run at a time; a poll or `r` that finds
/// one in flight does nothing. The model is `doc/spec/eat-viewer.fizz`.
///
/// Scope dropped (TODO(lczyk)):
/// - `--color=never` override. edit's tui has no plain-mode toggle yet.
pub fn run_snapshot(
    source: Source,
    lang: Option<&'static Language>,
    show_numbers: bool,
    _use_color: bool,
    wrap: bool,
    watch: Option<Duration>,
) -> io::Result<()> {
    use std::ops::ControlFlow;

    use crate::buffer::TextBuffer;
    use crate::helpers::{CoordType, Point, Size};

    use crate::mount;

    let buf =
        TextBuffer::new_rc(false).map_err(|e| io::Error::other(format!("text buffer: {e:?}")))?;
    let mut marker = Marker::None;
    let mut last_output = Vec::new();
    // The language is settled once there is content to sniff: now for a
    // file, when the first run returns for a command without `-l`.
    let mut lang = lang;
    let mut running: Option<exec::Run> = match &source {
        Source::File(_) => None,
        Source::Command(argv) => {
            let mut run = exec::start(argv)
                .map_err(|e| io::Error::other(format!("{}: {e}", source.label())))?;
            match run.wait_up_to(RUN_GRACE) {
                Ok(None) => Some(run),
                Ok(Some(out)) => {
                    let mut b = buf.borrow_mut();
                    marker = marker_of(Source::finish(&mut b, out, &mut last_output));
                    None
                }
                Err(e) => return Err(io::Error::other(format!("{}: {e}", source.label()))),
            }
        }
    };
    {
        let mut b = buf.borrow_mut();
        match source.load_file(&mut b) {
            Ok(Load::Loaded(None) | Load::Unchanged(None)) => {}
            Ok(Load::Loaded(Some(note)) | Load::Unchanged(Some(note))) => {
                marker = Marker::Note(note)
            }
            Err(note) => return Err(io::Error::other(format!("{}: {note}", source.label()))),
        }
        if let Some(lang) = lang {
            b.set_language(lang);
        } else if running.is_none() {
            lang = Some(settle_language(&mut b));
        }
        b.set_margin_enabled(show_numbers);
        b.set_word_wrap(wrap);
        b.set_read_only(true);
    }

    let mut wrap = wrap;
    let path_label = source.label();
    let mut captured_stat = source.stat();
    let mut last_disk_check = Instant::now();
    let mut header =
        snapshot_header(&path_label, Instant::now(), &marker, watch, running.is_some());
    let poll_interval = watch.unwrap_or(Duration::from_secs(2));
    let tick_override = std::rc::Rc::new(std::cell::Cell::new(running.as_ref().map(|_| RUN_POLL)));

    let _session = viewer::ViewerSession::begin()?;

    let opts = mount::MountOpts {
        tick_interval: Some(poll_interval),
        tick_override: Some(tick_override.clone()),
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
                ViewerKey::Reload => match &source {
                    Source::File(_) => {
                        marker = reload_file(&source, &buf, show_numbers, wrap);
                        captured_stat = source.stat();
                        last_disk_check = Instant::now();
                        header =
                            snapshot_header(&path_label, Instant::now(), &marker, watch, false);
                        ctx.needs_rerender();
                    }
                    // A run already in flight is the reload; nothing more.
                    Source::Command(argv) if running.is_none() => {
                        running = start_run(argv, &mut marker);
                        if let Some(run) = running.as_mut()
                            && let Ok(Some(out)) = run.wait_up_to(RUN_GRACE)
                        {
                            running = None;
                            marker = finish_run(&buf, out, &mut last_output, show_numbers, wrap);
                        }
                        tick_override.set(running.as_ref().map(|_| RUN_POLL));
                        last_disk_check = Instant::now();
                        header = snapshot_header(
                            &path_label,
                            Instant::now(),
                            &marker,
                            watch,
                            running.is_some(),
                        );
                        ctx.needs_rerender();
                    }
                    Source::Command(_) => {}
                },
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

        // A run in flight: has it returned? Checked every frame; the tick
        // override keeps frames coming at RUN_POLL while one is out.
        if let Some(run) = running.as_mut() {
            let done = match run.poll() {
                Ok(None) => None,
                Ok(Some(out)) => Some(Ok(out)),
                Err(e) => Some(Err(e.to_string())),
            };
            if let Some(result) = done {
                running = None;
                tick_override.set(None);
                marker = match result {
                    Ok(out) => {
                        let next = finish_run(&buf, out, &mut last_output, show_numbers, wrap);
                        if lang.is_none() {
                            lang = Some(settle_language(&mut buf.borrow_mut()));
                        }
                        next
                    }
                    Err(note) => Marker::Note(note),
                };
                last_disk_check = Instant::now();
                header = snapshot_header(&path_label, Instant::now(), &marker, watch, false);
                ctx.needs_rerender();
            }
        }

        // the poll. fires at most every `poll_interval`; mount's
        // tick_interval keeps the loop awake to hit this branch even with
        // no user input. a file is stat'ed; a live command is run again,
        // a static one is not (there is nothing cheap to ask it).
        let now = Instant::now();
        if running.is_none() && now.duration_since(last_disk_check) >= poll_interval {
            last_disk_check = now;
            match &source {
                Source::Command(argv) => {
                    if watch.is_some() {
                        running = start_run(argv, &mut marker);
                        tick_override.set(running.as_ref().map(|_| RUN_POLL));
                        header = snapshot_header(
                            &path_label,
                            Instant::now(),
                            &marker,
                            watch,
                            running.is_some(),
                        );
                        ctx.needs_rerender();
                    }
                }
                Source::File(_) => {
                    let changed = match (&captured_stat, source.stat()) {
                        (Some(a), Some(b)) => *a != b,
                        (Some(_), None) | (None, Some(_)) => true,
                        (None, None) => false,
                    };
                    let next = if watch.is_some() {
                        if changed {
                            let next = reload_file(&source, &buf, show_numbers, wrap);
                            captured_stat = source.stat();
                            next
                        } else {
                            marker.clone()
                        }
                    } else {
                        match (&marker, changed) {
                            (Marker::Note(_), _) => marker.clone(),
                            (_, true) => Marker::Modified,
                            (_, false) => Marker::None,
                        }
                    };
                    if next != marker {
                        marker = next;
                        header =
                            snapshot_header(&path_label, Instant::now(), &marker, watch, false);
                        ctx.needs_rerender();
                    }
                }
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

/// Read the file again, keeping the viewport. Returns the marker to show:
/// the load's note, or none.
fn reload_file(
    source: &Source,
    buf: &crate::buffer::RcTextBuffer,
    show_numbers: bool,
    wrap: bool,
) -> Marker {
    let mut b = buf.borrow_mut();
    b.set_read_only(false);
    let loaded = source.load_file(&mut b);
    settle_load(&mut b, &loaded, show_numbers, wrap);
    b.set_read_only(true);
    marker_of(loaded)
}

/// A returned command into the buffer, keeping the viewport. An
/// unchanged output is left alone, the buffer already shows it.
fn finish_run(
    buf: &crate::buffer::RcTextBuffer,
    out: exec::Output,
    last_output: &mut Vec<u8>,
    show_numbers: bool,
    wrap: bool,
) -> Marker {
    let mut b = buf.borrow_mut();
    b.set_read_only(false);
    let loaded = Source::finish(&mut b, out, last_output);
    settle_load(&mut b, &loaded, show_numbers, wrap);
    b.set_read_only(true);
    marker_of(loaded)
}

/// Start a command; a spawn failure becomes the marker, there is no run.
fn start_run(argv: &[String], marker: &mut Marker) -> Option<exec::Run> {
    match exec::start(argv) {
        Ok(run) => Some(run),
        Err(e) => {
            *marker = Marker::Note(e.to_string());
            None
        }
    }
}

fn settle_load(
    b: &mut crate::buffer::TextBuffer,
    loaded: &Result<Load, String>,
    show_numbers: bool,
    wrap: bool,
) {
    if let Ok(Load::Loaded(_)) = loaded {
        b.set_margin_enabled(show_numbers);
        b.set_word_wrap(wrap);
        b.request_scroll_bound_to_tail();
    }
}

fn marker_of(loaded: Result<Load, String>) -> Marker {
    match loaded {
        Ok(Load::Loaded(None) | Load::Unchanged(None)) => Marker::None,
        Ok(Load::Loaded(Some(note)) | Load::Unchanged(Some(note))) | Err(note) => {
            Marker::Note(note)
        }
    }
}

/// Sniff the language off the buffer's head: a command's output has no
/// path to go by.
fn settle_language(b: &mut crate::buffer::TextBuffer) -> &'static Language {
    let head = b.read_forward(0);
    let head = head[..head.len().min(4096)].to_vec();
    let lang = lsh_defs::detect::resolve(None, lsh_defs::detect::NO_USER_ASSOCIATIONS, || head);
    b.set_language(lang);
    lang
}

/// The marker leads so a long path cannot push it off a narrow screen.
fn snapshot_header(
    path_label: &str,
    at: Instant,
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
        format_clock(at),
    )
}

/// `MountOpts::on_probe` callback: reflow the buffer iff the terminal
/// reported ambiguous-width 2.
///
/// The view reads the source into a `TextBuffer` before mounting, so the
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

/// approximate hh:mm:ss using system time. avoids a chrono dep.
fn format_clock(_now: Instant) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{h:02}:{m:02}:{s:02}")
}
