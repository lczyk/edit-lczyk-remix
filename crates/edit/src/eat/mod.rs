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
pub mod route;
pub mod source;
pub mod stream;
pub mod theme;
pub mod viewer;
pub mod views;

use std::process::ExitCode;
use std::time::Duration;

use cli::{Cli, LineRange, parse_cli, parse_line_range, prog_name, resolve_use_color};
use detect::{resolve_language, resolve_language_with};
use route::{Env, Plan, SourceSpec};

fn run_viewer(spec: SourceSpec, watch: Option<Duration>, cli: &Cli) -> ExitCode {
    let lang = match &spec {
        SourceSpec::File(path) => match resolve_language(path, cli.language.as_deref()) {
            Ok(lang) => Some(lang),
            Err(name) => return unknown_language(&name),
        },
        // `-l` or nothing: the output is sniffed once it is there.
        SourceSpec::Command(_) => match cli.language.as_deref() {
            Some(name) => match resolve_language_with(None, Some(name), Vec::new) {
                Ok(lang) => Some(lang),
                Err(name) => return unknown_language(&name),
            },
            None => None,
        },
        // A listing paints its own colours; no language gets a say.
        SourceSpec::Dir(_) => Some(lsh_defs::PLAIN),
        SourceSpec::Stdin => unreachable!("route keeps stdin off the viewer"),
    };
    // A command is a spawn per poll, not a stat, and so is a listing (its
    // git column): fifty of these at 50ms would be a core. The floor is a
    // quarter second, said in the header.
    let floor = match &spec {
        SourceSpec::Command(_) | SourceSpec::Dir(_) => Duration::from_millis(250),
        SourceSpec::File(_) | SourceSpec::Stdin => Duration::ZERO,
    };
    let watch = watch.map(|w| w.max(floor));
    let source = source::Source::new(spec, false);
    let use_color = resolve_use_color(cli.color, true);
    match views::run_snapshot(source, lang, cli.number, use_color, cli.wrap.resolve(), watch) {
        Ok(()) => ExitCode::from(0),
        Err(e) => {
            eprintln!("{}: {e}", prog_name());
            ExitCode::from(1)
        }
    }
}

fn unknown_language(name: &str) -> ExitCode {
    eprintln!("{}: unknown language '{name}'", prog_name());
    ExitCode::from(2)
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

    let line_range: Option<LineRange> = match cli.line_range.as_deref().map(parse_line_range) {
        Some(Ok(r)) => Some(r),
        Some(Err(e)) => {
            eprintln!("{}: invalid --line-range: {e}", prog_name());
            return ExitCode::from(2);
        }
        None => None,
    };

    let plan = match route::route(&cli, &Env::detect()) {
        Ok(plan) => plan,
        Err(usage) => {
            let prog = prog_name();
            eprintln!("{prog}: {}", usage.message(&prog));
            return ExitCode::from(2);
        }
    };

    // An unknown `-l` is refused whatever the plan; the viewer resolves it
    // again on the way in.
    if let Some(name) = cli.language.as_deref()
        && lsh_defs::detect::find_language(name).is_none()
    {
        return unknown_language(name);
    }

    match plan {
        Plan::Help => cli::print_short_help(),
        Plan::Viewer { source, watch } => run_viewer(source, watch, &cli),
        Plan::Stream { sources } => stream::run(&sources, &cli, line_range),
    }
}
