//! Which way an invocation goes: the viewer, the stream, the short help,
//! or a usage error. One pure function over the parsed [`Cli`] and what
//! the environment says about it, the code form of `dispatch()` in
//! `doc/spec/eat-cli.fizz`. Nothing here opens a file or writes a byte.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::cli::Cli;

/// What the environment contributes to routing. The directory probe is
/// injected so the routing table can be tested without a filesystem.
pub(crate) struct Env {
    pub(crate) stdout_is_tty: bool,
    pub(crate) stdin_is_tty: bool,
    pub(crate) is_dir: fn(&Path) -> bool,
}

impl Env {
    pub(crate) fn detect() -> Env {
        use std::io::IsTerminal as _;
        Env {
            stdout_is_tty: std::io::stdout().is_terminal(),
            stdin_is_tty: std::io::stdin().is_terminal(),
            is_dir: Path::is_dir,
        }
    }
}

/// One thing to show, before anything has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceSpec {
    File(PathBuf),
    Dir(PathBuf),
    Stdin,
    Command(Vec<String>),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// A tty with nothing to read: the short usage line.
    Help,
    /// The alt-screen viewer over one source, live when `watch` is set.
    Viewer { source: SourceSpec, watch: Option<Duration> },
    /// The ansi stream over the sources in order.
    Stream { sources: Vec<SourceSpec> },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Usage {
    ExecNeedsCommand,
    /// What keeps `-w` out of the viewer, as the message after `-w `.
    Watch(&'static str),
}

impl Usage {
    pub(crate) fn message(&self, prog: &str) -> String {
        match self {
            Usage::ExecNeedsCommand => format!("-x needs a command: {prog} -x -- CMD [ARGS...]"),
            Usage::Watch(why) => format!("-w {why}"),
        }
    }
}

pub(crate) fn route(cli: &Cli, env: &Env) -> Result<Plan, Usage> {
    let watch = cli.watch.map(|w| w.0);
    let viewer_ok = env.stdout_is_tty && !cli.plain && cli.line_range.is_none();

    if cli.exec {
        if cli.files.is_empty() {
            return Err(Usage::ExecNeedsCommand);
        }
        let source = SourceSpec::Command(cli.files.clone());
        if viewer_ok {
            return Ok(Plan::Viewer { source, watch });
        }
        if watch.is_some() {
            return Err(Usage::Watch(viewer_blocker(cli, env)));
        }
        return Ok(Plan::Stream { sources: vec![source] });
    }

    let sources: Vec<SourceSpec> = if cli.files.is_empty() {
        vec![SourceSpec::Stdin]
    } else {
        cli.files.iter().map(|f| spec_of(f, env)).collect()
    };

    if viewer_ok && let [source @ SourceSpec::File(_)] = sources.as_slice() {
        return Ok(Plan::Viewer { source: source.clone(), watch });
    }
    if watch.is_some() {
        let why = match (cli.files.len(), sources.as_slice()) {
            (0, _) => "needs a file",
            (1, [SourceSpec::Stdin]) => "cannot watch stdin",
            (1, [SourceSpec::Dir(_)]) => "cannot watch a directory",
            (1, _) => viewer_blocker(cli, env),
            _ => "takes a single file",
        };
        return Err(Usage::Watch(why));
    }
    if cli.files.is_empty() && env.stdin_is_tty {
        return Ok(Plan::Help);
    }
    Ok(Plan::Stream { sources })
}

fn spec_of(arg: &str, env: &Env) -> SourceSpec {
    if arg == "-" {
        return SourceSpec::Stdin;
    }
    let path = PathBuf::from(arg);
    if (env.is_dir)(&path) { SourceSpec::Dir(path) } else { SourceSpec::File(path) }
}

/// Which of the viewer's preconditions failed, for a `-w` that could not
/// take it. Only called when at least one did.
fn viewer_blocker(cli: &Cli, env: &Env) -> &'static str {
    if cli.plain {
        "cannot be combined with --plain"
    } else if cli.line_range.is_some() {
        "cannot be combined with --line-range"
    } else if !env.stdout_is_tty {
        "needs a tty"
    } else {
        unreachable!("the viewer was open to this invocation")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(flags: &[&str], files: &[&str]) -> Cli {
        use argh::FromArgs as _;
        let mut cli = Cli::from_args(&["eat"], flags).unwrap();
        cli.files = files.iter().map(|s| s.to_string()).collect();
        cli
    }

    fn env(stdout_is_tty: bool, stdin_is_tty: bool) -> Env {
        Env { stdout_is_tty, stdin_is_tty, is_dir: |p| p.to_str().is_some_and(|s| s == "DIR") }
    }

    const TTY: bool = true;
    const PIPE: bool = false;

    fn file(name: &str) -> SourceSpec {
        SourceSpec::File(PathBuf::from(name))
    }

    fn viewer(source: SourceSpec, watch_ms: Option<u64>) -> Result<Plan, Usage> {
        Ok(Plan::Viewer { source, watch: watch_ms.map(Duration::from_millis) })
    }

    fn stream(sources: Vec<SourceSpec>) -> Result<Plan, Usage> {
        Ok(Plan::Stream { sources })
    }

    /// flags, positionals, stdout tty, stdin tty, and where that goes.
    type Case = (&'static [&'static str], &'static [&'static str], bool, bool, Result<Plan, Usage>);

    /// The matrix from `doc/src/eat.md`, one row per cell.
    #[test]
    fn the_routing_matrix() {
        let cmd = SourceSpec::Command(vec!["ls".into()]);
        let dir = SourceSpec::Dir(PathBuf::from("DIR"));
        let cases: Vec<Case> = vec![
            // a tty: one file takes the viewer, anything else streams
            (&[], &["a.txt"], TTY, TTY, viewer(file("a.txt"), None)),
            (&[], &["a.txt", "b.txt"], TTY, TTY, stream(vec![file("a.txt"), file("b.txt")])),
            (&[], &["-"], TTY, PIPE, stream(vec![SourceSpec::Stdin])),
            (&[], &["DIR"], TTY, TTY, stream(vec![dir.clone()])),
            (&[], &["a.txt", "DIR"], TTY, TTY, stream(vec![file("a.txt"), dir.clone()])),
            (&["-p"], &["a.txt"], TTY, TTY, stream(vec![file("a.txt")])),
            (&["--line-range", "1:2"], &["a.txt"], TTY, TTY, stream(vec![file("a.txt")])),
            // not a tty: everything streams
            (&[], &["a.txt"], PIPE, TTY, stream(vec![file("a.txt")])),
            (&[], &[], PIPE, PIPE, stream(vec![SourceSpec::Stdin])),
            (&[], &[], TTY, PIPE, stream(vec![SourceSpec::Stdin])),
            // nothing to read on a tty is the short help
            (&[], &[], TTY, TTY, Ok(Plan::Help)),
            (&[], &[], PIPE, TTY, Ok(Plan::Help)),
            (&["-p"], &[], TTY, TTY, Ok(Plan::Help)),
            // -x: the positionals are the command
            (&["-x"], &["ls"], TTY, TTY, viewer(cmd.clone(), None)),
            (&["-x"], &["ls"], PIPE, TTY, stream(vec![cmd.clone()])),
            (&["-x", "-p"], &["ls"], TTY, TTY, stream(vec![cmd.clone()])),
            (&["-x"], &[], TTY, TTY, Err(Usage::ExecNeedsCommand)),
            (&["-x"], &[], PIPE, PIPE, Err(Usage::ExecNeedsCommand)),
            // -w: the viewer, or a usage error; it never streams
            (&["-w", "2s"], &["a.txt"], TTY, TTY, viewer(file("a.txt"), Some(2000))),
            (&["-w", "2s", "-x"], &["ls"], TTY, TTY, viewer(cmd.clone(), Some(2000))),
            (&["-w", "2s"], &["a.txt"], PIPE, TTY, Err(Usage::Watch("needs a tty"))),
            (&["-w", "2s", "-x"], &["ls"], PIPE, TTY, Err(Usage::Watch("needs a tty"))),
            (&["-w", "2s"], &[], TTY, TTY, Err(Usage::Watch("needs a file"))),
            (&["-w", "2s"], &[], TTY, PIPE, Err(Usage::Watch("needs a file"))),
            (&["-w", "2s"], &["-"], TTY, PIPE, Err(Usage::Watch("cannot watch stdin"))),
            (&["-w", "2s"], &["DIR"], TTY, TTY, Err(Usage::Watch("cannot watch a directory"))),
            (&["-w", "2s"], &["a.txt", "b"], TTY, TTY, Err(Usage::Watch("takes a single file"))),
            (
                &["-w", "2s", "-p"],
                &["a.txt"],
                TTY,
                TTY,
                Err(Usage::Watch("cannot be combined with --plain")),
            ),
            (
                &["-w", "2s", "--line-range", "1"],
                &["a.txt"],
                TTY,
                TTY,
                Err(Usage::Watch("cannot be combined with --line-range")),
            ),
            (
                &["-w", "2s", "-x", "-p"],
                &["ls"],
                TTY,
                TTY,
                Err(Usage::Watch("cannot be combined with --plain")),
            ),
            (&["-w", "2s", "-x"], &[], TTY, TTY, Err(Usage::ExecNeedsCommand)),
        ];
        for (flags, files, stdout_tty, stdin_tty, want) in cases {
            let got = route(&cli(flags, files), &env(stdout_tty, stdin_tty));
            assert_eq!(got, want, "flags {flags:?} files {files:?} tty {stdout_tty}/{stdin_tty}");
        }
    }

    /// The shape of the positionals is reported before the flags that
    /// keep the viewer closed, so `-w -p a b` says "a single file".
    #[test]
    fn the_watch_shape_outranks_the_viewer_blockers() {
        let got = route(&cli(&["-w", "1s", "-p"], &["a", "b"]), &env(TTY, TTY));
        assert_eq!(got, Err(Usage::Watch("takes a single file")));
        let got = route(&cli(&["-w", "1s", "-p"], &["DIR"]), &env(PIPE, TTY));
        assert_eq!(got, Err(Usage::Watch("cannot watch a directory")));
        let got = route(&cli(&["-w", "1s", "-p", "--line-range", "1"], &["a"]), &env(PIPE, TTY));
        assert_eq!(got, Err(Usage::Watch("cannot be combined with --plain")));
    }

    #[test]
    fn usage_messages_name_the_flag() {
        assert_eq!(
            Usage::ExecNeedsCommand.message("eat"),
            "-x needs a command: eat -x -- CMD [ARGS...]"
        );
        assert_eq!(Usage::Watch("needs a tty").message("eat"), "-w needs a tty");
    }
}
