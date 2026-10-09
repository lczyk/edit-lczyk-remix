mod apperr;
mod cli;
#[cfg(debug_assertions)]
mod devlog;
mod document;
mod draw_editor;
mod draw_menubar;
mod draw_statusbar;
mod keybindings;
mod minimap;
mod modals;
mod settings;
mod state;

use std::borrow::Cow;
use std::time::Duration;
use std::{env, process};

use draw_editor::*;
use draw_menubar::*;
use draw_statusbar::*;
use edit::framebuffer::IndexedColor;
use edit::helpers::*;
use edit::input::{self, vk};
use edit::sys;
use edit::tui::*;
use edit::vt;
use state::*;
use stdext::arena::{self, Arena, scratch_arena};
use stdext::collections::BString;

use crate::settings::Settings;

#[cfg(target_pointer_width = "32")]
const SCRATCH_ARENA_CAPACITY: usize = 128 * MEBI;
#[cfg(target_pointer_width = "64")]
const SCRATCH_ARENA_CAPACITY: usize = 512 * MEBI;

// NOTE: Before our main() gets called, Rust initializes its stdlib. This pulls in the entire
// std::io::{stdin, stdout, stderr} machinery, and probably some more, which amounts to about 20KB.
// It can technically be avoided nowadays with `#![no_main]`. Maybe a fun project for later? :)
fn main() -> process::ExitCode {
    let argv0 = env::args_os().next();
    let name = argv0
        .as_deref()
        .and_then(|p| std::path::Path::new(p).file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("edit");

    // --eat wins over --help so `edit --eat --help` prints eat's help.
    // Symlink (name == "eat") also takes priority.
    if name == "eat" {
        return edit::eat::main();
    }
    if env::args_os().skip(1).take_while(|a| a != "--").any(|a| a == "--eat") {
        // SAFETY: single-threaded at this point in main.
        unsafe { std::env::set_var("EDIT_EAT_VIA_FLAG", "1") };
        return edit::eat::main();
    }

    // --help/-h anywhere in remaining args prints edit's help.
    if env::args_os().any(|a| a == "-h" || a == "--help") {
        cli::print_help();
        return process::ExitCode::SUCCESS;
    }

    if cfg!(debug_assertions) {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            drop(edit::term::RestoreModes);
            drop(sys::Deinit);
            hook(info);
        }));
    }

    match run() {
        Ok(()) => process::ExitCode::SUCCESS,
        Err(err) => {
            sys::write_stdout(&format!("{}\n", FormatApperr::from(err)));
            process::ExitCode::FAILURE
        }
    }
}

fn run() -> apperr::Result<()> {
    let _sys_deinit = sys::init();
    arena::init(SCRATCH_ARENA_CAPACITY)?;

    let Some(path) = cli::parse_args()? else {
        return Ok(());
    };
    let document = document::Document::open(&path)?;
    let mut state = State::new(document)?;

    if let Err(err) = Settings::reload() {
        state.add_error(err);
    }
    if let Err(err) = keybindings::load_or_create() {
        state.add_error(err);
    }
    // The textarea handles these six itself, so that they also work in a
    // modal's input field. Hand it the configured chords, or rebinding them
    // would relabel the menubar and change nothing else.
    {
        use keybindings::{Action, chord};
        edit::tui::set_textarea_chords(edit::tui::TextareaChords {
            cut: chord(Action::Cut),
            copy: chord(Action::Copy),
            paste: chord(Action::Paste),
            undo: chord(Action::Undo),
            redo: chord(Action::Redo),
            select_all: chord(Action::SelectAll),
        });
    }
    // This will reopen stdin if it's redirected (which may fail) and switch
    // the terminal to raw mode which prevents the user from pressing Ctrl+C.
    // `handle_args` may want to print a help message (must not fail),
    // and reads files (may hang; should be cancellable with Ctrl+C).
    // As such, we call this after `handle_args`.
    sys::switch_modes()?;

    let mut vt_parser = vt::Parser::new();
    let mut input_parser = input::Parser::new();
    let mut tui = Tui::new()?;

    let _restore = {
        // Palette: use the terminal's reported OSC 4 / 10 / 11 colours
        // (so themes the user picked at the terminal level apply
        // naturally), falling back to the baked-in DEFAULT_THEME for
        // any slot the terminal doesn't report. opt into indexed
        // emission so terminals which drop OSC 4 (e.g. tmux) still
        // render via their own palette.
        let (probe, restore) = edit::term::setup(&mut vt_parser, edit::framebuffer::DEFAULT_THEME);
        if probe.ambiguous_width == 2 {
            edit::unicode::setup_ambiguous_width(2);
            state.document.buffer.borrow_mut().reflow();
        }
        tui.setup_indexed_colors(probe.indexed_colors);
        tui.setup_emit_indexed_codes(true);
        restore
    };

    edit::notify::set_handler(state::push_warning);

    state.menubar_color_bg = tui.indexed(IndexedColor::Background).oklab_blend(tui.indexed_alpha(
        IndexedColor::BrightBlue,
        1,
        2,
    ));
    state.menubar_color_fg = tui.contrasted(state.menubar_color_bg);
    let floater_bg = tui
        .indexed_alpha(IndexedColor::Background, 2, 3)
        .oklab_blend(tui.indexed_alpha(IndexedColor::Foreground, 1, 3));
    let floater_fg = tui.contrasted(floater_bg);
    tui.setup_modifier_translations(ModifierTranslations {
        ctrl: "Ctrl",
        alt: "Alt",
        shift: "Shift",
        cmd: "Cmd",
    });
    tui.set_floater_default_bg(floater_bg);
    tui.set_floater_default_fg(floater_fg);
    tui.set_modal_default_bg(floater_bg);
    tui.set_modal_default_fg(floater_fg);

    sys::inject_window_size_into_stdin();

    const GUTTER_REDIFF_DEBOUNCE: Duration = Duration::from_millis(300);
    const MINIMAP_REBUILD_DEBOUNCE: Duration = Duration::from_millis(300);
    const LANGUAGE_REDETECT_DEBOUNCE: Duration = Duration::from_millis(500);
    const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(2);

    loop {
        // Process a batch of input.
        {
            let scratch = scratch_arena(None);
            let read_timeout = vt_parser.read_timeout().min(tui.read_timeout());
            let Some(input) = sys::read_stdin(&scratch, read_timeout) else {
                break;
            };

            let vt_iter = vt_parser.parse(&input);
            let mut input_iter = input_parser.parse(vt_iter);
            let mut absorbed = false;
            let mut drew = false;

            while {
                let input = input_iter.next();
                let more = input.is_some();

                if input.as_ref().is_some_and(|i| tui.absorb_idle_motion(i)) {
                    absorbed = true;
                } else if more || drew || !absorbed {
                    drew = true;

                    #[cfg(debug_assertions)]
                    let logged_input = if devlog::is_enabled() {
                        input.as_ref().map(devlog::describe)
                    } else {
                        None
                    };

                    let mut ctx = tui.create_context(input);

                    draw(&mut ctx, &mut state);

                    #[cfg(debug_assertions)]
                    if let Some(desc) = logged_input {
                        let snapshot = state.document.buffer.borrow();
                        devlog::log(&desc, Some(&snapshot));
                    }
                }

                more
            } {}

            // A batch of nothing but idle mouse motion needs no frame at all.
            if absorbed && !drew {
                continue;
            }
        }

        // Continue rendering until the layout has settled.
        // This can take >1 frame, if the input focus is tossed between different controls.
        while tui.needs_settling() {
            let mut ctx = tui.create_context(None);

            draw(&mut ctx, &mut state);
        }

        if state.exit {
            break;
        }

        // Refresh the git-baseline gutter marks if the buffer changed and
        // the debounce window has elapsed. Synchronous; the diff is fast
        // and the git subprocess only runs on first refresh.
        state.document.gutter_check_dirty();
        if state.document.gutter_should_rebuild(GUTTER_REDIFF_DEBOUNCE) {
            state.document.gutter_refresh();
        }

        // Skip width updates until a real size is known -- the initial
        // {0,0} would compute width=0 and hide the rail until next resize.
        if tui.size().width > 0 {
            state.document.set_minimap_target_width(desired_minimap_width(tui.size().width));
        }
        state.document.minimap_check_dirty();
        if state.document.minimap_should_rebuild(MINIMAP_REBUILD_DEBOUNCE) {
            state.document.minimap_refresh();
        }

        state.document.check_disk_fingerprint(DISK_CHECK_INTERVAL);

        // Re-run language auto-detect on plain docs as their content grows.
        // No-op once a language is found or the user picks one explicitly.
        state.document.language_check_dirty();
        if state.document.language_should_redetect(LANGUAGE_REDETECT_DEBOUNCE) {
            state.document.language_redetect();
        }

        // Render the UI and write it to the terminal.
        {
            let scratch = scratch_arena(None);
            let mut output = tui.render(&scratch);

            write_terminal_title(&scratch, &mut output, &mut state);
            edit::mount::flush_clipboard_to_host(&scratch, &mut output, &mut tui);

            sys::write_stdout(&output);
        }
    }

    Ok(())
}

fn draw(ctx: &mut Context, state: &mut State) {
    handle_global_shortcuts(ctx, state);

    draw_menubar(ctx, state);
    draw_editor(ctx, state);
    draw_statusbar(ctx, state);

    modals::raise_errors_if_any(state);
    modals::draw_active(ctx, state);

    if let Some(key) = ctx.keyboard_input()
        && key == vk::F3
    {
        search_execute(ctx, state, SearchAction::Search);
        ctx.needs_rerender();
        ctx.set_input_consumed();
    }
}

/// Lines moved per "small jump" action.
const SMALL_JUMP_LINES: CoordType = 3;

/// Where an action's effect lands, which decides whether focus can veto it.
///
/// Window actions reach the frame around the document -- dialogs, panels, view
/// toggles -- and stay available whatever holds the keyboard. Document actions
/// reach into the buffer, so a text field must be able to shadow them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scope {
    Window,
    Document,
}

/// Every chord the keybinding table dispatches, in priority order.
///
/// Kept as data so the mapping can be examined without a terminal: which
/// actions are reachable at all, in what order they win, and which a focused
/// field shadows. Three actions sat in the table for a long time with no
/// dispatch behind them at all, advertised by the menubar and accepted by the
/// config file; a list is checkable, a chain of `else if` is not.
const DISPATCH: &[(keybindings::Action, Scope)] = {
    use keybindings::Action::*;
    &[
        (Exit, Scope::Window),
        (Save, Scope::Window),
        (GoToLine, Scope::Window),
        (ToggleColumnGuides, Scope::Window),
        (ToggleWordWrap, Scope::Window),
        (FocusStatusbar, Scope::Window),
        (OpenAbout, Scope::Window),
        (Find, Scope::Window),
        (Replace, Scope::Window),
        (MoveLineUp, Scope::Document),
        (MoveLineDown, Scope::Document),
        (DeleteLine, Scope::Document),
        (ToggleLineComment, Scope::Document),
        (SmallJumpUpSelect, Scope::Document),
        (SmallJumpDownSelect, Scope::Document),
        (SmallJumpUp, Scope::Document),
        (SmallJumpDown, Scope::Document),
        (LineStart, Scope::Document),
        (LineEnd, Scope::Document),
        (LineStartSelect, Scope::Document),
        (LineEndSelect, Scope::Document),
        (DeleteToLineStart, Scope::Document),
        (DeleteToLineEnd, Scope::Document),
        (JumpDocumentStart, Scope::Document),
        (JumpDocumentEnd, Scope::Document),
    ]
};

/// Which action a keystroke means. Pure: no tui, no buffer, no terminal.
///
/// `search_enabled` and `focus_in_field` are the only live state that changes
/// the answer, and both arrive as plain values so the whole table can be
/// exercised from a unit test.
fn resolve(
    key: edit::input::InputKey,
    focus_in_field: bool,
    search_enabled: bool,
) -> Option<keybindings::Action> {
    use keybindings::Action;

    if key == edit::input::vk::NULL {
        return None; // an unbound action, which no keystroke should match
    }
    for &(action, scope) in DISPATCH {
        if keybindings::chord(action) != key {
            continue;
        }
        if scope == Scope::Document && focus_in_field {
            return None;
        }
        if !search_enabled && matches!(action, Action::Find | Action::Replace) {
            return None;
        }
        return Some(action);
    }
    None
}

fn handle_global_shortcuts(ctx: &mut Context, state: &mut State) {
    use edit::buffer::MoveLineDirection;
    use keybindings::Action;

    let Some(key) = ctx.keyboard_input() else {
        return;
    };
    let search_enabled = state.wants_search.kind != StateSearchKind::Disabled;
    let Some(action) = resolve(key, ctx.focus_is_in_text_field(), search_enabled) else {
        return;
    };

    match action {
        Action::Exit => state.modal = Some(modals::Modal::ConfirmExit),
        Action::Save => save_document(ctx, state),
        Action::GoToLine => state.modal = Some(modals::Modal::GoToLine),
        Action::ToggleColumnGuides => {
            let mut tb = state.document.buffer.borrow_mut();
            let on = tb.is_column_guides_enabled();
            tb.set_column_guides_enabled(!on);
        }
        Action::ToggleWordWrap => {
            let mut tb = state.document.buffer.borrow_mut();
            let on = tb.is_word_wrap_enabled();
            tb.set_word_wrap(!on);
        }
        Action::FocusStatusbar => state.wants_statusbar_focus = true,
        Action::OpenAbout => state.modal = Some(modals::Modal::About),
        Action::Find => {
            state.wants_search.kind = StateSearchKind::Search;
            state.wants_search.focus = true;
        }
        Action::Replace => {
            state.wants_search.kind = StateSearchKind::Replace;
            state.wants_search.focus = true;
        }
        Action::MoveLineUp => {
            state.document.buffer.borrow_mut().move_selected_lines(MoveLineDirection::Up)
        }
        Action::MoveLineDown => {
            state.document.buffer.borrow_mut().move_selected_lines(MoveLineDirection::Down)
        }
        Action::DeleteLine => state.document.buffer.borrow_mut().delete_lines(),
        Action::ToggleLineComment => toggle_line_comment(state),
        Action::SmallJumpUpSelect => edit::buffer::small_jump_select(
            &mut state.document.buffer.borrow_mut(),
            -SMALL_JUMP_LINES,
        ),
        Action::SmallJumpDownSelect => edit::buffer::small_jump_select(
            &mut state.document.buffer.borrow_mut(),
            SMALL_JUMP_LINES,
        ),
        Action::SmallJumpUp => {
            edit::buffer::small_jump(&mut state.document.buffer.borrow_mut(), -SMALL_JUMP_LINES)
        }
        Action::SmallJumpDown => {
            edit::buffer::small_jump(&mut state.document.buffer.borrow_mut(), SMALL_JUMP_LINES)
        }
        Action::LineStart => {
            edit::buffer::smart_line_start(&mut state.document.buffer.borrow_mut(), false)
        }
        Action::LineEnd => edit::buffer::line_end(&mut state.document.buffer.borrow_mut(), false),
        Action::LineStartSelect => {
            edit::buffer::smart_line_start(&mut state.document.buffer.borrow_mut(), true)
        }
        Action::LineEndSelect => {
            edit::buffer::line_end(&mut state.document.buffer.borrow_mut(), true)
        }
        Action::DeleteToLineStart => state.document.buffer.borrow_mut().delete_to_line_edge(false),
        Action::DeleteToLineEnd => state.document.buffer.borrow_mut().delete_to_line_edge(true),
        Action::JumpDocumentStart => {
            let mut tb = state.document.buffer.borrow_mut();
            tb.cursor_move_to_logical(Point { x: 0, y: 0 });
            tb.set_preferred_column(0);
            tb.make_cursor_visible();
        }
        Action::JumpDocumentEnd => {
            let mut tb = state.document.buffer.borrow_mut();
            tb.cursor_move_to_logical(Point::MAX);
            let x = tb.cursor_visual_pos().x;
            tb.set_preferred_column(x);
            tb.make_cursor_visible();
        }
        // Handled by the menubar or the textarea, never by this table.
        Action::Undo
        | Action::Redo
        | Action::Cut
        | Action::Copy
        | Action::Paste
        | Action::SelectAll
        | Action::FocusMenubar => return,
    }

    ctx.set_input_consumed();
    ctx.needs_rerender();
}

fn toggle_line_comment(state: &mut State) {
    // TODO: when we have a user-facing warning/toast system, surface a
    // "no comment syntax for this file" hint instead of silent noop.
    let lang = state.document.buffer.borrow().language();
    let line_tok = lang.line_comment;
    let block_tok = lang.block_comment;
    if let Some(tok) = line_tok.or_else(|| document::fallback_line_comment(&state.document.path)) {
        state.document.buffer.borrow_mut().toggle_line_comment(tok);
    } else if let Some((open, close)) = block_tok {
        state.document.buffer.borrow_mut().toggle_per_line_block_comment(open, close);
    }
}

/// Pick minimap cell width for a terminal `terminal_width` cells wide. 0
/// disables the rail (terminal too narrow); same thresholds apply in both
/// unicode and ascii-quirk modes.
fn desired_minimap_width(terminal_width: CoordType) -> u8 {
    const NARROW_FLOOR: CoordType = 30;
    const TWO_CELL_THRESHOLD: CoordType = 60;
    if terminal_width < NARROW_FLOOR {
        return 0;
    }
    if terminal_width < TWO_CELL_THRESHOLD {
        return 1;
    }
    2
}

fn write_terminal_title<'a>(arena: &'a Arena, output: &mut BString<'a>, state: &mut State) {
    let filename = state.document.filename.as_str();
    let dirty = state.document.buffer.borrow().is_dirty();

    if filename == state.osc_title_file_status.filename
        && dirty == state.osc_title_file_status.dirty
    {
        return;
    }

    output.push_str(arena, "\x1b]0;");
    if !filename.is_empty() {
        if dirty {
            output.push_str(arena, edit::glyphs::modified_dot());
        }
        output.push_str(arena, &sanitize_control_chars(filename));
        output.push_str(arena, " - ");
    }
    output.push_str(arena, "edit\x1b\\");

    state.osc_title_file_status.filename = filename.to_string();
    state.osc_title_file_status.dirty = dirty;
}

/// Strips all C0 control characters from the string and replaces them with "_".
///
/// Jury is still out on whether this should also strip C1 control characters.
/// That requires parsing UTF8 codepoints, which is annoying.
fn sanitize_control_chars(text: &str) -> Cow<'_, str> {
    if let Some(off) = text.bytes().position(|b| (..0x20).contains(&b)) {
        let mut sanitized = text.to_string();
        // SAFETY: We only search for ASCII and replace it with ASCII.
        let vec = unsafe { sanitized.as_bytes_mut() };

        for i in &mut vec[off..] {
            *i = if (..0x20).contains(i) { b'_' } else { *i }
        }

        Cow::Owned(sanitized)
    } else {
        Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod dispatch_tests {
    use edit::input::{kbmod, vk};
    use keybindings::Action;

    use super::*;

    /// Serialises tests that read the keybinding table.
    ///
    /// `keybindings::chord` borrows a process-wide `SemiRefCell` that is only
    /// `Sync` by assertion -- the editor is single-threaded, so nothing there
    /// contends. The test harness is not, and two threads borrowing it at once
    /// panics with "RefCell already mutably borrowed".
    fn with_bindings<R>(f: impl FnOnce() -> R) -> R {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        f()
    }

    /// Actions this table deliberately does not own. Undo/Redo/Cut/Copy/Paste/
    /// SelectAll belong to the textarea, so that they work in input fields too;
    /// FocusMenubar is consumed by the menubar itself.
    const NOT_OURS: &[Action] = &[
        Action::Undo,
        Action::Redo,
        Action::Cut,
        Action::Copy,
        Action::Paste,
        Action::SelectAll,
        Action::FocusMenubar,
    ];

    #[test]
    fn every_action_is_either_dispatched_here_or_deliberately_elsewhere() {
        // The bug this pins: toggle_word_wrap, focus_statusbar and open_about
        // each had a table entry, a config key, docs and a menu item showing
        // their chord -- and no dispatch at all. Nothing failed, because
        // nothing was checking that the table and the handler agreed.
        for &(action, _) in keybindings::ACTION_KEYS.iter() {
            let dispatched = DISPATCH.iter().any(|&(a, _)| a == action);
            let excused = NOT_OURS.contains(&action);
            assert!(
                dispatched != excused,
                "{action:?} is {} -- every action must be dispatched here or listed as owned elsewhere, not both or neither",
                if dispatched {
                    "both dispatched and excused"
                } else {
                    "neither dispatched nor excused"
                }
            );
        }
    }

    #[test]
    fn a_field_shadows_document_actions_but_not_window_ones() {
        with_bindings(|| {
            let delete_line = keybindings::chord(Action::DeleteLine);
            let exit = keybindings::chord(Action::Exit);

            assert_eq!(resolve(delete_line, false, true), Some(Action::DeleteLine));
            assert_eq!(resolve(delete_line, true, true), None, "a field must shadow it");

            assert_eq!(resolve(exit, false, true), Some(Action::Exit));
            assert_eq!(
                resolve(exit, true, true),
                Some(Action::Exit),
                "exit is not the field's to eat"
            );
        });
    }

    #[test]
    fn search_actions_need_search_enabled() {
        with_bindings(|| {
            let find = keybindings::chord(Action::Find);
            assert_eq!(resolve(find, false, true), Some(Action::Find));
            assert_eq!(resolve(find, false, false), None);
        });
    }

    #[test]
    fn an_unbound_action_is_not_matched_by_a_null_keystroke() {
        with_bindings(|| {
            // An unbound entry parses to vk::NULL. Without the guard every unbound
            // action would answer to the same non-keystroke.
            assert_eq!(resolve(vk::NULL, false, true), None);
        });
    }

    #[test]
    fn an_unmapped_chord_resolves_to_nothing() {
        with_bindings(|| {
            assert_eq!(resolve(kbmod::CTRL | kbmod::ALT | vk::F7, false, true), None);
        });
    }
}
