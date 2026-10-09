//! Thin external-mount surface for edit's [`Tui`]. Bundles the boilerplate
//! that every embedder of edit's tui repeats: [`Tui::new`], the
//! [`term::setup`] probe, the input/render loop, the alt-screen restore.
//!
//! The editor's own `bin/edit/main.rs` does **not** use this. Its loop
//! interleaves work no embedder wants: debounced gutter / minimap /
//! language rebuilds, a disk-fingerprint poll, the terminal title, and a
//! debug input log that needs the raw [`input::Input`] before the context
//! consumes it. What the two loops genuinely shared -- the clipboard
//! flush -- is [`flush_clipboard_to_host`], which both call; the rest is
//! about a dozen lines of read/parse/settle skeleton that is cheaper to
//! read twice than to invert behind callbacks.
//!
//! [`mount`] is therefore purely additive, for callers that want a
//! textarea and nothing else (eat's snapshot + follow views).
//!
//! ## Caller responsibilities
//!
//! Before calling [`mount`]:
//!
//! - `stdext::arena::init(...)` -- scratch arenas are single-init; the
//!   caller picks the capacity.
//! - `edit::sys::init()` -- raw mode + signal wiring. Hold the returned
//!   `Deinit` for the lifetime of the mount.
//! - `edit::sys::switch_modes()` -- enable raw-mode keypress reads.
//!
//! [`mount`] installs its own panic hook for the duration of the mount
//! (debug builds only; release uses `panic=abort`, which skips drops
//! anyway) so an unwinding panic restores the terminal *before* the
//! message prints -- otherwise the alt-screen leave scrolls it away. The
//! previous hook is restored on return.
//!
//! [`mount`] returns when the draw callback returns
//! [`ControlFlow::Break`] or stdin closes.

use std::io;
use std::ops::ControlFlow;
use std::time::Duration;

use stdext::arena::{Arena, scratch_arena};
use stdext::collections::BString;

use crate::framebuffer::{DEFAULT_THEME, INDEXED_COLORS_COUNT};
use crate::oklab::StraightRgba;
use crate::tui::{Context, Tui};
use crate::{base64, input, sys, term, vt};

/// One-shot callback handed the terminal probe before the first draw.
/// See [`MountOpts::on_probe`].
pub type ProbeCallback = Box<dyn FnOnce(&term::TerminalProbe)>;

/// Knobs for [`mount`]. Defaults match the editor's own setup.
pub struct MountOpts {
    /// Seeds the probe's palette. OSC 4/10/11 responses overwrite slots
    /// the terminal reports; unreported slots stay at the fallback.
    pub fallback_palette: [StraightRgba; INDEXED_COLORS_COUNT],
    /// Forwarded to [`Tui::setup_emit_indexed_codes`] after the probe.
    /// On (default) lets terminals which drop OSC 4 (e.g. tmux) still
    /// render via their own palette; off forces exact RGB everywhere.
    pub emit_indexed_codes: bool,
    /// If set, caps the input read timeout so the draw callback fires at
    /// least every `tick_interval` even with no user input. Use for periodic
    /// refreshes (clock, disk-change poll) that can't be input-driven.
    /// `None` (default) means: block on input indefinitely (or until the
    /// vt parser / tui animation requests a shorter timeout).
    pub tick_interval: Option<Duration>,
    /// A shorter tick the draw callback can switch on and off between
    /// frames, for a stretch where something is pending (a background
    /// command) and the result should land promptly without the idle
    /// cadence paying for it. Read before every wait; `None` inside means
    /// `tick_interval` applies.
    pub tick_override: Option<std::rc::Rc<std::cell::Cell<Option<Duration>>>>,
    /// Invoked once, after [`term::setup`] has probed the terminal and
    /// [`mount`] has applied the process-global ambiguous width, but
    /// before the first draw.
    ///
    /// Callers that built a [`crate::buffer::TextBuffer`] *before*
    /// mounting need this: an ambiguous width of 2 changes how already-
    /// buffered text measures, so the buffer has to be reflowed. Callers
    /// with nothing to reflow can leave it `None` (the default).
    pub on_probe: Option<ProbeCallback>,
}

impl Default for MountOpts {
    fn default() -> Self {
        Self {
            fallback_palette: DEFAULT_THEME,
            emit_indexed_codes: true,
            tick_interval: None,
            tick_override: None,
            on_probe: None,
        }
    }
}

#[cfg(debug_assertions)]
type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// Restores the previous panic hook when the mount returns. Debug-only:
/// release builds are `panic=abort`, so no unwinding drop runs anyway.
#[cfg(debug_assertions)]
struct PanicHookGuard(Option<std::sync::Arc<PanicHook>>);

#[cfg(debug_assertions)]
impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        // Drop our hook first so the Arc clone it captured is released;
        // that leaves us as sole owner and lets the original move back out.
        let _ = std::panic::take_hook();
        if let Some(arc) = self.0.take()
            && let Ok(prev) = std::sync::Arc::try_unwrap(arc)
        {
            std::panic::set_hook(prev);
        }
    }
}

/// Install a panic hook that restores the terminal before delegating to
/// the previous hook. Without this the message prints while the alt-screen
/// is still up, and the alt-screen leave during unwind scrolls it away.
#[cfg(debug_assertions)]
fn install_panic_hook() -> PanicHookGuard {
    let prev = std::sync::Arc::new(std::panic::take_hook());
    let prev_for_hook = std::sync::Arc::clone(&prev);
    std::panic::set_hook(Box::new(move |info| {
        drop(term::RestoreModes);
        drop(sys::Deinit);
        prev_for_hook(info);
    }));
    PanicHookGuard(Some(prev))
}

/// Append any pending clipboard copy to `out` as an OSC 52 sequence, so
/// the copy reaches the host terminal rather than staying inside this
/// process.
///
/// Call once per frame, after [`Tui::render`] and before the write. Both
/// the editor's loop and [`mount`]'s use this; it lives here because
/// getting the reserve-then-encode wrong on a large copy is the kind of
/// thing that should only be written once.
pub fn flush_clipboard_to_host<'a>(arena: &'a Arena, out: &mut BString<'a>, tui: &mut Tui) {
    let clipboard = tui.clipboard_mut();
    if !clipboard.wants_host_sync() {
        return;
    }

    let data = clipboard.read();
    if !data.is_empty() {
        // Reserve up front: BString doubles on growth, so a really large
        // copy would otherwise double `out` from e.g. 100MB to 200MB.
        out.reserve_exact(arena, base64::encode_len(data.len()) + 16);
        out.push_str(arena, "\x1b]52;c;");
        base64::encode(arena, out, data);
        out.push_str(arena, "\x1b\\");
    }

    clipboard.mark_as_synchronized();
}

/// Mount edit's [`Tui`] and run the input/render loop until `draw` returns
/// [`ControlFlow::Break`] or stdin closes.
///
/// `draw` is invoked once per input event and once per settle pass. It
/// composes the node tree (typically a single [`Context::textarea`] +
/// chrome) and signals exit via the [`ControlFlow`] return.
pub fn mount<F>(opts: MountOpts, mut draw: F) -> io::Result<()>
where
    F: FnMut(&mut Context) -> ControlFlow<()>,
{
    let mut tui = Tui::new()?;
    let mut vt_parser = vt::Parser::new();
    let mut input_parser = input::Parser::new();

    #[cfg(debug_assertions)]
    let _panic_hook = install_panic_hook();

    let (probe, _restore) = term::setup(&mut vt_parser, opts.fallback_palette);
    tui.setup_indexed_colors(probe.indexed_colors);
    if opts.emit_indexed_codes {
        tui.setup_emit_indexed_codes(true);
    }

    // Ambiguous-width is process-global and read at measure time, so it
    // has to land before the first draw. Buffers built before the mount
    // still carry measurements taken at the old width -- that's what
    // `on_probe` is for.
    if probe.ambiguous_width == 2 {
        crate::unicode::setup_ambiguous_width(2);
    }
    if let Some(on_probe) = opts.on_probe {
        on_probe(&probe);
    }

    sys::inject_window_size_into_stdin();

    let mut exit = false;
    while !exit {
        {
            let scratch = scratch_arena(None);
            let mut timeout = vt_parser.read_timeout().min(tui.read_timeout());
            if let Some(tick) = opts.tick_interval {
                timeout = timeout.min(tick);
            }
            if let Some(tick) = opts.tick_override.as_ref().and_then(|t| t.get()) {
                timeout = timeout.min(tick);
            }
            let Some(inp) = sys::read_stdin(&scratch, timeout) else {
                break;
            };
            let vt_iter = vt_parser.parse(&inp);
            let mut iter = input_parser.parse(vt_iter);
            let mut absorbed = false;
            let mut drew = false;
            while {
                let event = iter.next();
                let more = event.is_some();
                if event.as_ref().is_some_and(|e| tui.absorb_idle_motion(e)) {
                    absorbed = true;
                } else if more || drew || !absorbed {
                    drew = true;
                    let mut ctx = tui.create_context(event);
                    if draw(&mut ctx).is_break() {
                        exit = true;
                    }
                }
                more
            } {}

            // A batch of nothing but idle mouse motion needs no frame at all.
            if absorbed && !drew {
                continue;
            }
        }

        while tui.needs_settling() {
            let mut ctx = tui.create_context(None);
            if draw(&mut ctx).is_break() {
                exit = true;
            }
        }

        let scratch = scratch_arena(None);
        let mut out = tui.render(&scratch);
        flush_clipboard_to_host(&scratch, &mut out, &mut tui);
        sys::write_stdout(&out);
    }

    Ok(())
}
