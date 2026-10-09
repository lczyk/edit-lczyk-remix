//! Eat's alt-screen view, mounted through `edit::mount`.
//!
//! The keymap and the terminal session live in [`super::viewer`]; what
//! stays here is the view itself: `eat <file>` or `eat -x -- <cmd>` on a
//! tty. A read-only `TextBuffer` in edit's textarea (cursor / scroll /
//! selection / mouse-wheel native). A 2s `tick_interval` drives the
//! `[modified on disk]` marker for a file; `r` reloads, `q` exits.
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

    /// Replace the buffer with the source. `Err` is the note to show and
    /// means the buffer was left alone: a file that could not be opened, or
    /// a command that failed without producing anything. A command that
    /// exits nonzero with output (`diff`, `grep`) is loaded, and the note
    /// still says so.
    fn load(&self, b: &mut crate::buffer::TextBuffer) -> Result<Option<String>, String> {
        match self {
            Source::File(path) => {
                let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
                b.read_file(&mut f).map_err(|e| e.to_string())?;
                Ok(None)
            }
            Source::Command(argv) => {
                let out = exec::run(argv).map_err(|e| e.to_string())?;
                let note = (!out.status.success()).then(|| exec::failure_note(&out));
                if out.stdout.is_empty()
                    && let Some(note) = note
                {
                    return Err(note);
                }
                b.read_from(&mut &out.stdout[..], Some(out.stdout.len()))
                    .map_err(|e| e.to_string())?;
                Ok(note)
            }
        }
    }
}

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
/// Scope dropped (TODO(lczyk)):
/// - `--color=never` override. edit's tui has no plain-mode toggle yet.
pub fn run_snapshot(
    source: Source,
    lang: Option<&'static Language>,
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
    let mut marker = Marker::None;
    {
        let mut b = buf.borrow_mut();
        match source.load(&mut b) {
            Ok(None) => {}
            Ok(Some(note)) => marker = Marker::Note(note),
            Err(note) => return Err(io::Error::other(format!("{}: {note}", source.label()))),
        }
        let lang = lang.unwrap_or_else(|| {
            let head = b.read_forward(0);
            let head = head[..head.len().min(4096)].to_vec();
            lsh_defs::detect::resolve(None, lsh_defs::detect::NO_USER_ASSOCIATIONS, || head)
        });
        b.set_language(lang);
        b.set_margin_enabled(show_numbers);
        b.set_word_wrap(wrap);
        b.set_read_only(true);
    }

    let mut wrap = wrap;
    let path_label = source.label();
    let mut captured_stat = source.stat();
    let mut last_disk_check = Instant::now();
    let mut header = snapshot_header(&path_label, Instant::now(), &marker);
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
                    {
                        let mut b = buf.borrow_mut();
                        b.set_read_only(false);
                        marker = match source.load(&mut b) {
                            Ok(None) => Marker::None,
                            Ok(Some(note)) | Err(note) => Marker::Note(note),
                        };
                        b.set_margin_enabled(show_numbers);
                        b.set_word_wrap(wrap);
                        b.set_read_only(true);
                        b.request_scroll_bound_to_tail();
                    }
                    captured_stat = source.stat();
                    last_disk_check = Instant::now();
                    header = snapshot_header(&path_label, Instant::now(), &marker);
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

        // disk-change poll, for a file source. fires at most every
        // `disk_check_interval`; mount's tick_interval keeps the loop awake
        // to hit this branch even with no user input.
        let now = Instant::now();
        if captured_stat.is_some() && now.duration_since(last_disk_check) >= disk_check_interval {
            last_disk_check = now;
            let changed = match (&captured_stat, source.stat()) {
                (Some(a), Some(b)) => *a != b,
                (Some(_), None) => true,
                _ => false,
            };
            let next = match (&marker, changed) {
                (Marker::Modified, false) => Marker::None,
                (Marker::Note(_), _) => marker.clone(),
                (_, true) => Marker::Modified,
                (_, false) => Marker::None,
            };
            if next != marker {
                marker = next;
                header = snapshot_header(&path_label, Instant::now(), &marker);
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

/// The marker leads so a long path cannot push it off a narrow screen.
fn snapshot_header(path_label: &str, at: Instant, marker: &Marker) -> String {
    let lead = match marker {
        Marker::None => String::new(),
        Marker::Modified => "[modified on disk] ".to_string(),
        Marker::Note(note) => format!("[{note}] "),
    };
    format!(
        "{lead}{path_label} @ {}  (q exit, r reload, w wrap, arrows/g/G/PgUp/PgDn scroll)",
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
