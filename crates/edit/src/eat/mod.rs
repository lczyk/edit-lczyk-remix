//! eat -- a bat-like syntax-highlighting cat persona for edit.
//!
//! Reached by argv0 (a symlink named `eat`) or `edit --eat`. Three output
//! paths, picked in [`main`]:
//!
//! - **non-tty** -- highlight to ansi and write through, in [`stream`].
//!   Pipes, redirects, and the pager all land here.
//! - **tty snapshot** -- a read-only alt-screen viewer, in [`views`].
//!
//! `-x` swaps the file for a command's stdout ([`exec`]) and takes the
//! same two paths: the viewer on a tty, the stream otherwise.
//!
//! A directory argument goes down the non-tty path whatever stdout is,
//! as a [`listing`] in place of the file body.
//!
//! The viewer's keymap and terminal setup are in [`viewer`]; everything
//! shares language detection via [`detect`].

pub mod cli;
pub mod detect;
pub mod exec;
pub mod gutter_view;
pub mod listing;
pub mod stream;
pub mod theme;
pub mod viewer;
pub mod views;

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;

use cli::{Cli, LineRange, parse_cli, parse_line_range, prog_name, resolve_use_color};
use detect::{resolve_language, resolve_language_with};
use stream::run;

/// `-x -- CMD ARGS...`: the positionals are the command. A tty gets the
/// snapshot viewer over its stdout, with `r` running it again; anything
/// else streams it like a file. `-l` names the language; without it the
/// output is sniffed, since there is no path to go by.
fn run_exec_cli(cli: &Cli, line_range: Option<LineRange>) -> ExitCode {
    if cli.files.is_empty() {
        eprintln!("{}: -x needs a command: {} -x -- CMD [ARGS...]", prog_name(), prog_name());
        return ExitCode::from(2);
    }
    let use_viewer = io::stdout().is_terminal() && !cli.plain && line_range.is_none();
    if use_viewer {
        let lang = match cli.language.as_deref() {
            Some(name) => match resolve_language_with(None, Some(name), Vec::new) {
                Ok(lang) => Some(lang),
                Err(name) => {
                    eprintln!("{}: unknown language '{name}'", prog_name());
                    return ExitCode::from(2);
                }
            },
            None => None,
        };
        return run_snapshot(views::Source::Command(cli.files.clone()), lang, cli);
    }
    run(
        &cli.files,
        true,
        cli.language.as_deref(),
        cli.plain,
        cli.number,
        line_range,
        cli.color,
        cli.paging,
        cli.wrap,
    )
}

fn run_snapshot(
    source: views::Source,
    lang: Option<&'static lsh::runtime::Language>,
    cli: &Cli,
) -> ExitCode {
    let use_color = resolve_use_color(cli.color, true);
    match views::run_snapshot(source, lang, cli.number, use_color, cli.wrap.resolve()) {
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

    if cli.exec {
        return run_exec_cli(&cli, line_range);
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
        return run_snapshot(views::Source::File(path), Some(lang), &cli);
    }

    run(
        &cli.files,
        false,
        cli.language.as_deref(),
        cli.plain,
        cli.number,
        line_range,
        cli.color,
        cli.paging,
        cli.wrap,
    )
}
