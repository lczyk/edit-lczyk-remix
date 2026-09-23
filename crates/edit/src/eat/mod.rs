//! eat -- a bat-like syntax-highlighting cat persona for edit.
//!
//! Reached by argv0 (a symlink named `eat`) or `edit --eat`. Three output
//! paths, picked in [`main`]:
//!
//! - **non-tty** -- highlight to ansi and write through, in [`stream`].
//!   Pipes, redirects, and the pager all land here.
//! - **tty snapshot** -- a read-only alt-screen viewer, in [`views`].
//! - **tty follow** -- the same viewer over a growing file.
//!
//! A directory argument goes down the non-tty path whatever stdout is,
//! as a [`listing`] in place of the file body.
//!
//! The two viewers share their keymap and terminal setup via [`viewer`];
//! everything shares language detection via [`detect`].

pub mod cli;
pub mod detect;
pub mod follow;
pub mod gutter_view;
pub mod listing;
pub mod stream;
pub mod theme;
pub mod viewer;
pub mod views;

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;

use cli::{Cli, FollowDuration, parse_cli, parse_line_range, prog_name, resolve_use_color};
use detect::resolve_language;
use stream::run;

/// validate --follow combos, resolve language, dispatch to `follow::run`.
/// `--paging` is silently ignored in follow mode (forced off); paging-while-
/// following is a deliberate v2 once we have a scrollback ux for it.
fn run_follow_cli(cli: &Cli, has_line_range: bool) -> ExitCode {
    // disallowed combos
    if cli.files.is_empty() || cli.files.iter().any(|f| f == "-") {
        eprintln!("{}: --follow requires a file path (stdin is not supported)", prog_name());
        return ExitCode::from(2);
    }
    if cli.files.len() > 1 {
        eprintln!(
            "{}: --follow takes a single file (multi-file follow is not supported)",
            prog_name()
        );
        return ExitCode::from(2);
    }
    if has_line_range {
        eprintln!("{}: --follow cannot be combined with --line-range", prog_name());
        return ExitCode::from(2);
    }
    if cli.plain {
        // plain + follow could work (just write raw bytes), but we'd need a
        // separate path; v1 keeps the surface tight. error rather than
        // silently doing one of the two.
        eprintln!("{}: --follow cannot be combined with --plain", prog_name());
        return ExitCode::from(2);
    }

    let path = PathBuf::from(&cli.files[0]);

    let lang = match resolve_language(&path, cli.language.as_deref()) {
        Ok(lang) => lang,
        Err(name) => {
            eprintln!("{}: unknown language '{name}'", prog_name());
            return ExitCode::from(2);
        }
    };

    let use_color = resolve_use_color(cli.color, io::stdout().is_terminal());

    // poll interval comes from the parsed `--follow` value (already defaulted
    // by parse_cli's bare-`-f` rewrite, which also honours EAT_FOLLOW_INTERVAL_MS).
    let poll = cli.follow.map(|fd| fd.0).unwrap_or(FollowDuration::DEFAULT);

    // tty -> live alt-screen pager (mount-based, phase C.1/C.2/C.4);
    // non-tty (piped) -> stream lines as before.
    // EAT_FOLLOW_NO_TUI=1 forces streaming even on a tty (debug / scripting).
    let force_no_tui = std::env::var("EAT_FOLLOW_NO_TUI").is_ok_and(|v| !v.is_empty());
    let result = if io::stdout().is_terminal() && !force_no_tui {
        views::run_follow_mount(path, lang, cli.number, use_color, poll, cli.wrap.resolve())
    } else {
        follow::run(path, lang, cli.number, use_color, poll)
    };

    match result {
        Ok(()) => ExitCode::from(0),
        Err(e) => {
            eprintln!("{}: {e}", prog_name());
            ExitCode::from(1)
        }
    }
}

/// main entry point for eat. called from edit's argv0 dispatch -- either via a
/// symlink named `eat` or via `edit --eat`. there is no separate cargo target.
pub fn main() -> ExitCode {
    stdext::arena::init(128 * 1024 * 1024).unwrap();

    let cli: Cli = parse_cli();

    if let Some(fmt) = cli.list_languages {
        return crate::langlist::list_languages(fmt.0);
    }

    if cli.version {
        println!("{} {}", prog_name(), version::version!());
        return ExitCode::from(0);
    }

    let line_range = if let Some(ref s) = cli.line_range {
        match parse_line_range(s) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("{}: invalid --line-range: {e}", prog_name());
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };

    if cli.follow.is_some() {
        return run_follow_cli(&cli, line_range.is_some());
    }

    // single file, tty, not plain, no line-range -> snapshot TUI pager
    let use_snapshot_tui = io::stdout().is_terminal()
        && !cli.plain
        && line_range.is_none()
        && cli.files.len() == 1
        && cli.files[0] != "-"
        && !std::path::Path::new(&cli.files[0]).is_dir();

    if use_snapshot_tui {
        let path = PathBuf::from(&cli.files[0]);
        let lang = match resolve_language(&path, cli.language.as_deref()) {
            Ok(lang) => lang,
            Err(name) => {
                eprintln!("{}: unknown language '{name}'", prog_name());
                return ExitCode::from(2);
            }
        };
        let use_color = resolve_use_color(cli.color, true);
        return match views::run_snapshot(path, lang, cli.number, use_color, cli.wrap.resolve()) {
            Ok(()) => ExitCode::from(0),
            Err(e) => {
                eprintln!("{}: {e}", prog_name());
                ExitCode::from(1)
            }
        };
    }

    run(
        &cli.files,
        cli.language.as_deref(),
        cli.plain,
        cli.number,
        line_range,
        cli.color,
        cli.paging,
        cli.wrap,
    )
}
