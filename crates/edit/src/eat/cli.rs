//! Argument parsing for the eat persona.
//!
//! argh does the work, with two hand-rolled adjustments in [`parse_cli`]:
//! bare `-L` and bare `-f` are rewritten to carry their default values,
//! since argh has no notion of an option whose value is optional.

use std::process::ExitCode;

use argh::FromArgs;

use crate::langlist::ListFormat;

/// argh glue for the shared [`ListFormat`]. The wrapper exists so
/// `langlist` stays free of the cli framework -- argh is eat's
/// dependency, not the library's.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ListFormatArg(pub(crate) ListFormat);

impl argh::FromArgValue for ListFormatArg {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        ListFormat::parse(value).map(ListFormatArg)
    }
}

/// eat -- a bat-like syntax-highlighting cat.
#[derive(FromArgs, PartialEq, Debug)]
#[argh(name = "eat")]
pub(crate) struct Cli {
    /// override syntax detection (required for stdin if no shebang)
    #[argh(option, short = 'l')]
    pub(crate) language: Option<String>,

    /// disable highlighting, decorations, and paging -- act like cat
    #[argh(switch, short = 'p')]
    pub(crate) plain: bool,

    /// show line numbers
    #[argh(switch, short = 'n')]
    pub(crate) number: bool,

    /// line range: N | N: | :M | N:M
    #[argh(option)]
    pub(crate) line_range: Option<String>,

    /// when to use colors: auto, always, never
    #[argh(option, default = "ColorMode::Auto")]
    pub(crate) color: ColorMode,

    /// when to use a pager: auto, always, never
    #[argh(option, default = "PagingMode::Auto")]
    pub(crate) paging: PagingMode,

    /// when to wrap long lines: auto, always, never (never chops instead)
    #[argh(option, default = "WrapMode::Auto")]
    pub(crate) wrap: WrapMode,

    /// live view: load the source again whenever it changes, polled every
    /// DUR (`-w 2s`, `-w 500ms`, `-w 2` = seconds; bare `-w` is 1s, or
    /// EAT_WATCH_INTERVAL_MS). needs a tty and one file or a command.
    #[argh(option, short = 'w')]
    pub(crate) watch: Option<PollInterval>,

    /// run a command and show its stdout instead of reading files. the
    /// positional arguments are the command, so put `--` before it:
    /// `eat -x -- git diff main..HEAD`
    #[argh(switch, short = 'x')]
    pub(crate) exec: bool,

    /// print known languages and exit (format: pretty, plain, json; defaults to pretty)
    #[argh(option, short = 'L')]
    pub(crate) list_languages: Option<ListFormatArg>,

    /// print version and exit
    #[argh(switch)]
    pub(crate) version: bool,

    /// files to read (use - for stdin); a directory is listed. with -x,
    /// the command and its arguments
    #[argh(positional)]
    pub(crate) files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorMode {
    Auto,
    Always,
    Never,
}

impl argh::FromArgValue for ColorMode {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(ColorMode::Auto),
            "always" => Ok(ColorMode::Always),
            "never" => Ok(ColorMode::Never),
            _ => Err(format!("invalid color mode: {value}. expected auto, always, or never")),
        }
    }
}

/// resolve whether to emit ansi colour, following the de-facto-standard
/// precedence (highest wins):
///
///   1. explicit cli flag (`--color always|never`).
///   2. `FORCE_COLOR` env var (any non-empty value other than `0`) -> on.
///   3. `NO_COLOR` env var (any non-empty value, per <https://no-color.org>) -> off.
///   4. fall back to whether the output stream is a tty.
///
/// kept as a free function so the various entry points (bulk render, follow,
/// list-languages) can all share it. testable via the `_with_env` variant
/// that takes the env values as parameters.
pub(crate) fn resolve_use_color(mode: ColorMode, output_is_tty: bool) -> bool {
    let force = std::env::var_os("FORCE_COLOR");
    let no = std::env::var_os("NO_COLOR");
    resolve_use_color_with_env(mode, output_is_tty, force.as_deref(), no.as_deref())
}

// The `NO_COLOR`-only half of this precedence lives in `crate::glyphs`,
// where the editor -- which has no `ColorMode` -- reads it.
pub(crate) fn resolve_use_color_with_env(
    mode: ColorMode,
    output_is_tty: bool,
    force_color: Option<&std::ffi::OsStr>,
    no_color: Option<&std::ffi::OsStr>,
) -> bool {
    // explicit cli flag wins.
    match mode {
        ColorMode::Always => return true,
        ColorMode::Never => return false,
        ColorMode::Auto => {}
    }
    // FORCE_COLOR overrides NO_COLOR + tty status. accept any non-empty value
    // except literal "0" (matches the convention used by chalk, supports-color,
    // and friends in the js ecosystem).
    if let Some(v) = force_color
        && !v.is_empty()
        && v != "0"
    {
        return true;
    }
    // NO_COLOR: any non-empty value disables, per https://no-color.org.
    if let Some(v) = no_color
        && !v.is_empty()
    {
        return false;
    }
    // fall back to terminal detection.
    output_is_tty
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PagingMode {
    Auto,
    Always,
    Never,
}

impl argh::FromArgValue for PagingMode {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(PagingMode::Auto),
            "always" => Ok(PagingMode::Always),
            "never" => Ok(PagingMode::Never),
            _ => Err(format!("invalid paging mode: {value}. expected auto, always, or never")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WrapMode {
    Auto,
    Always,
    Never,
}

impl argh::FromArgValue for WrapMode {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(WrapMode::Auto),
            "always" => Ok(WrapMode::Always),
            "never" => Ok(WrapMode::Never),
            _ => Err(format!("invalid wrap mode: {value}. expected auto, always, or never")),
        }
    }
}

impl WrapMode {
    /// Wrap state the tui views start in. `auto` wraps: the tui only ever runs
    /// on a tty, and clipping long lines by default hides content.
    pub(crate) fn resolve(self) -> bool {
        !matches!(self, WrapMode::Never)
    }
}

/// a poll interval parsed off the cli. accepts `30s`, `500ms`,
/// `1m`, `1.5s`, or a bare number (= seconds). minimum 50ms; smaller values
/// are silently clamped. zero / negative / non-finite values are rejected.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PollInterval(pub std::time::Duration);

impl PollInterval {
    /// minimum permitted poll interval. 50ms is enough headroom for a tui
    /// redraw + tick; anything below is just busy-looping w/out user value.
    pub const MIN: std::time::Duration = std::time::Duration::from_millis(50);

    /// default poll interval applied when `-w` is given w/out a value.
    pub const DEFAULT: std::time::Duration = std::time::Duration::from_secs(1);

    pub fn parse(value: &str) -> Result<Self, String> {
        let s = value.trim();
        if s.is_empty() {
            return Err("empty duration".into());
        }
        // split numeric prefix from unit suffix.
        let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
        let (num_part, unit) = s.split_at(split);
        let n: f64 = num_part.parse().map_err(|_| format!("invalid duration: {value}"))?;
        if !n.is_finite() || n < 0.0 {
            return Err(format!("invalid duration: {value}"));
        }
        let ms = match unit {
            "" => n * 1000.0, // bare number = seconds
            "s" => n * 1000.0,
            "ms" => n,
            "m" => n * 60_000.0,
            other => {
                return Err(format!("invalid duration unit: '{other}' (expected ms, s, or m)"));
            }
        };
        if ms <= 0.0 {
            return Err("duration must be positive".into());
        }
        let mut d = std::time::Duration::from_millis(ms.round() as u64);
        if d < Self::MIN {
            d = Self::MIN;
        }
        Ok(PollInterval(d))
    }
}

impl argh::FromArgValue for PollInterval {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        Self::parse(value)
    }
}

pub(crate) struct LineRange {
    pub(crate) start: Option<usize>,
    pub(crate) end: Option<usize>,
}

pub(crate) fn parse_line_range(s: &str) -> Result<LineRange, String> {
    if s.is_empty() {
        return Err("empty line range".into());
    }

    let (start_str, end_str) = match s.split_once(':') {
        Some((l, r)) => (l, r),
        None => (s, ""),
    };

    let start = if start_str.is_empty() {
        None
    } else {
        Some(start_str.parse::<usize>().map_err(|e| format!("invalid line number: {e}"))?)
    };
    let end = if end_str.is_empty() {
        None
    } else {
        Some(end_str.parse::<usize>().map_err(|e| format!("invalid line number: {e}"))?)
    };

    Ok(LineRange { start, end })
}

/// print short help and exit.
pub(crate) fn print_short_help() -> ExitCode {
    let name = prog_name();
    let eat_flag = if std::env::var("EDIT_EAT_VIA_FLAG").is_ok() { " --eat" } else { "" };
    eprintln!(
        "usage: {name}{eat_flag} [-l <lang>] [-p] [-n] [-L] [--line-range <RANGE>] [--color <WHEN>] [--paging <WHEN>] [--wrap <WHEN>] [-f [<DUR>]] [--version] [FILES...]"
    );
    eprintln!("try `{name}{eat_flag} --help` for more details.");
    ExitCode::from(0)
}

/// parse argv with one ergonomic adjustment: bare `-L` / `--list-languages` (no value
/// following, or a non-format value following) is rewritten to `-L pretty` so the user
/// can type `eat -L` and get the default pretty listing.
/// Print argh help output, inserting ` --eat` into the Usage line when invoked
/// via `edit --eat` so the user sees `Usage: edit --eat [options]`.
pub(crate) fn print_help_maybe_eat(help: &str, via_eat_flag: bool, argv0: &str) {
    if !via_eat_flag {
        println!("{help}");
        return;
    }
    if let Some((first, rest)) = help.split_once('\n') {
        let prefix = format!("Usage: {argv0}");
        if let Some(args) = first.strip_prefix(&prefix) {
            println!("Usage: {argv0} --eat{args}");
        } else {
            println!("{first}");
        }
        println!("{rest}");
    } else {
        println!("{help}");
    }
}

/// File stem of argv[0], used as the program name prefix in messages.
pub(crate) fn prog_name() -> String {
    std::env::args_os()
        .next()
        .as_deref()
        .and_then(|p| std::path::Path::new(p).file_stem())
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "eat".to_string())
}

pub(crate) fn parse_cli() -> Cli {
    use argh::FromArgs;
    let argv: Vec<String> = std::env::args().collect();
    if argv.is_empty() {
        eprintln!("{}: empty argv", prog_name());
        std::process::exit(1);
    }

    let via_eat_flag = std::env::var("EDIT_EAT_VIA_FLAG").is_ok();

    // --help/-h anywhere in args prints help and exits, position-independent.
    if argv.iter().skip(1).any(|a| a == "-h" || a == "--help") {
        let help_argv: [&str; 2] = [argv[0].as_str(), "--help"];
        match Cli::from_args(&[help_argv[0]], &help_argv[1..]) {
            Ok(_) => unreachable!(),
            Err(early_exit) => match early_exit.status {
                Ok(()) => {
                    print_help_maybe_eat(&early_exit.output, via_eat_flag, argv[0].as_str());
                    std::process::exit(0);
                }
                Err(()) => {
                    eprintln!("{}", early_exit.output);
                    std::process::exit(1);
                }
            },
        }
    }

    let watch_default = std::env::var("EAT_WATCH_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| format!("{ms}ms"))
        .unwrap_or_else(|| format!("{}ms", PollInterval::DEFAULT.as_millis()));
    match parse_argv(&argv, via_eat_flag, &watch_default) {
        Ok(c) => c,
        Err(early_exit) => match early_exit.status {
            Ok(()) => {
                print_help_maybe_eat(&early_exit.output, via_eat_flag, &argv[0]);
                std::process::exit(0);
            }
            Err(()) => {
                eprintln!(
                    "{}\nRun {}{} for more information.",
                    early_exit.output,
                    argv[0],
                    if via_eat_flag { " --eat --help" } else { " --help" },
                );
                std::process::exit(1);
            }
        },
    }
}

/// Stands in for a bare `-` on its way through argh, which reads anything
/// starting with a dash as an option. Mapped back once the positionals are
/// out the other side.
const STDIN_TOKEN: &str = "\u{1}stdin";

/// [`rewrite_argv`], argh, and the stdin stand-in undone.
fn parse_argv(
    argv: &[String],
    via_eat_flag: bool,
    watch_default: &str,
) -> Result<Cli, argh::EarlyExit> {
    use argh::FromArgs;
    let rewritten = rewrite_argv(argv, via_eat_flag, watch_default);
    let strs: Vec<&str> = rewritten.iter().map(|s| s.as_str()).collect();
    let mut cli = Cli::from_args(&[strs[0]], &strs[1..])?;
    for f in &mut cli.files {
        if f == STDIN_TOKEN {
            *f = "-".to_string();
        }
    }
    Ok(cli)
}

/// The argv as argh will see it: `--eat` stripped, a bare `-` stood in
/// for, and the two options with an optional value (`-L`, `-w`) given
/// their default when the next token is not one. Everything after `--`
/// passes through untouched, since from there on the tokens are paths or,
/// with `-x`, a command.
fn rewrite_argv(argv: &[String], via_eat_flag: bool, watch_default: &str) -> Vec<String> {
    let mut rewritten: Vec<String> = Vec::with_capacity(argv.len() + 1);
    rewritten.push(argv[0].clone());
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        if a == "--" {
            rewritten.extend(argv[i..].iter().cloned());
            break;
        } else if a == "-" {
            rewritten.push(STDIN_TOKEN.to_string());
            i += 1;
        } else if via_eat_flag && a == "--eat" {
            // strip --eat injected by `edit --eat` before argh sees it
            i += 1;
        } else if a == "-L" || a == "--list-languages" {
            rewritten.push(a.clone());
            let next_is_format =
                argv.get(i + 1).is_some_and(|n| matches!(n.as_str(), "pretty" | "plain" | "json"));
            if !next_is_format {
                rewritten.push("pretty".to_string());
            }
            i += 1;
        } else if a == "-w" || a == "--watch" {
            rewritten.push(a.clone());
            let next = argv.get(i + 1);
            if next.is_some_and(|n| PollInterval::parse(n).is_ok()) {
                rewritten.push(next.unwrap().clone());
                i += 2;
            } else {
                rewritten.push(watch_default.to_string());
                i += 1;
            }
        } else {
            rewritten.push(a.clone());
            i += 1;
        }
    }
    rewritten
}

// pre-existing layout: `mod tests` sits in the middle of the file, with
// more pub items following. didn't trigger clippy when this was a standalone
// crate; after the absorption (0f0d8f7) it now lives as an inner module of
// edit and the lint fires. moving the test block to the bottom of the file
// would be a big mechanical churn; allow until that cleanup pass.
#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    // shebang + content-sniff tests now live in `lsh_defs::detect::tests` --
    // the impls moved to the shared crate so both `eat` and `edit` consume the
    // same detection logic.

    // --- argv rewrite ---

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn rewrite(parts: &[&str]) -> Vec<String> {
        rewrite_argv(&argv(parts), false, "1000ms")
    }

    #[test]
    fn rewrite_gives_bare_list_languages_a_format() {
        assert_eq!(rewrite(&["eat", "-L"]), argv(&["eat", "-L", "pretty"]));
        assert_eq!(rewrite(&["eat", "-L", "json"]), argv(&["eat", "-L", "json"]));
    }

    #[test]
    fn rewrite_gives_bare_watch_the_default() {
        assert_eq!(rewrite(&["eat", "-w", "x.log"]), argv(&["eat", "-w", "1000ms", "x.log"]));
        assert_eq!(rewrite(&["eat", "-w", "2s", "x.log"]), argv(&["eat", "-w", "2s", "x.log"]));
        assert_eq!(rewrite(&["eat", "-w"]), argv(&["eat", "-w", "1000ms"]));
        assert_eq!(rewrite(&["eat", "-w", "--", "x"]), argv(&["eat", "-w", "1000ms", "--", "x"]));
    }

    #[test]
    fn rewrite_leaves_everything_after_the_separator_alone() {
        assert_eq!(
            rewrite(&["eat", "-x", "--", "ls", "-L"]),
            argv(&["eat", "-x", "--", "ls", "-L"])
        );
        assert_eq!(rewrite(&["eat", "--", "-w"]), argv(&["eat", "--", "-w"]));
    }

    #[test]
    fn a_bare_dash_is_a_positional() {
        let cli = parse_argv(&argv(&["eat", "-n", "-"]), false, "1000ms").unwrap();
        assert!(cli.number);
        assert_eq!(cli.files, argv(&["-"]));
        let cli = parse_argv(&argv(&["eat", "-x", "cat", "-", "-n"]), false, "1000ms").unwrap();
        assert_eq!(cli.files, argv(&["cat", "-"]));
        assert!(cli.number);
        let cli = parse_argv(&argv(&["eat", "--", "-"]), false, "1000ms").unwrap();
        assert_eq!(cli.files, argv(&["-"]));
    }

    #[test]
    fn rewrite_strips_the_eat_flag_only_when_injected() {
        let out = rewrite_argv(&argv(&["edit", "--eat", "a.txt"]), true, "1000ms");
        assert_eq!(out, argv(&["edit", "a.txt"]));
        let out = rewrite_argv(&argv(&["eat", "--", "--eat"]), true, "1000ms");
        assert_eq!(out, argv(&["eat", "--", "--eat"]));
    }

    /// Every argv up to four tokens over the alphabet the two-stage parse
    /// (rewrite, then argh) can trip over, checked against the rules the
    /// matrix promises rather than against expected outputs.
    #[test]
    fn every_short_argv_parses_by_the_rules() {
        const ALPHABET: [&str; 7] = ["-w", "2s", "-x", "--", "a.txt", "-", "-n"];
        let mut count = 0;
        let mut stack: Vec<Vec<&str>> = vec![vec![]];
        while let Some(seq) = stack.pop() {
            if seq.len() < 4 {
                for t in ALPHABET {
                    let mut next = seq.clone();
                    next.push(t);
                    stack.push(next);
                }
            }
            count += 1;
            let mut raw = vec!["eat"];
            raw.extend(&seq);
            let rewritten = rewrite_argv(&argv(&raw), false, "1000ms");
            let strs: Vec<&str> = rewritten.iter().map(String::as_str).collect();
            let before_sep: Vec<&str> = seq.iter().copied().take_while(|t| *t != "--").collect();
            let after_sep: Vec<&str> = match seq.iter().position(|t| *t == "--") {
                Some(i) => seq[i + 1..].to_vec(),
                None => vec![],
            };

            // the rewrite never touches the separator or anything after it.
            if seq.contains(&"--") {
                let tail: Vec<&str> =
                    strs.iter().rev().take(after_sep.len()).rev().copied().collect();
                assert_eq!(tail, after_sep, "{seq:?}: rewritten {strs:?}");
            }

            let Ok(cli) = parse_argv(&argv(&raw), false, "1000ms") else { continue };

            // -x and -w are only flags before the separator.
            assert_eq!(cli.exec, before_sep.contains(&"-x"), "{seq:?}");
            assert_eq!(cli.watch.is_some(), before_sep.contains(&"-w"), "{seq:?}");

            // bare -w takes the default; -w 2s takes 2s.
            if let Some(i) = before_sep.iter().position(|t| *t == "-w") {
                let want = match before_sep.get(i + 1) {
                    Some(&"2s") => std::time::Duration::from_secs(2),
                    _ => std::time::Duration::from_secs(1),
                };
                assert_eq!(cli.watch.map(|w| w.0), Some(want), "{seq:?}");
            }

            // everything after the separator is a positional, in order,
            // and so is a bare dash wherever it stands.
            let files: Vec<&str> = cli.files.iter().map(String::as_str).collect();
            assert!(files.ends_with(&after_sep), "{seq:?}: files {files:?}");
            let dashes = seq.iter().filter(|t| **t == "-").count();
            let dashes_after_w =
                before_sep.windows(2).filter(|w| w[0] == "-w" && w[1] == "-").count();
            assert_eq!(
                files.iter().filter(|f| **f == "-").count(),
                dashes,
                "{seq:?}: files {files:?}, {dashes_after_w} after -w"
            );
            // and nothing before it that is a flag ends up there.
            for f in &files[..files.len() - after_sep.len()] {
                assert!(!["-w", "-x", "-n"].contains(f), "{seq:?}: files {files:?}");
            }
        }
        assert_eq!(count, 1 + 7 + 49 + 343 + 2401);
    }

    // --- line range parsing ---

    #[test]
    fn line_range_single() {
        let r = parse_line_range("5").unwrap();
        assert_eq!(r.start, Some(5));
        assert_eq!(r.end, None);
    }

    #[test]
    fn line_range_start_only() {
        let r = parse_line_range("5:").unwrap();
        assert_eq!(r.start, Some(5));
        assert_eq!(r.end, None);
    }

    #[test]
    fn line_range_end_only() {
        let r = parse_line_range(":10").unwrap();
        assert_eq!(r.start, None);
        assert_eq!(r.end, Some(10));
    }

    #[test]
    fn line_range_both() {
        let r = parse_line_range("5:10").unwrap();
        assert_eq!(r.start, Some(5));
        assert_eq!(r.end, Some(10));
    }

    #[test]
    fn line_range_empty() {
        assert!(parse_line_range("").is_err());
    }

    // --- color mode parsing ---

    #[test]
    fn color_mode_parsing() {
        assert_eq!(
            <ColorMode as argh::FromArgValue>::from_arg_value("auto").unwrap(),
            ColorMode::Auto
        );
        assert_eq!(
            <ColorMode as argh::FromArgValue>::from_arg_value("always").unwrap(),
            ColorMode::Always
        );
        assert_eq!(
            <ColorMode as argh::FromArgValue>::from_arg_value("never").unwrap(),
            ColorMode::Never
        );
    }

    #[test]
    fn color_mode_invalid() {
        assert!(<ColorMode as argh::FromArgValue>::from_arg_value("nope").is_err());
    }

    // --- NO_COLOR / FORCE_COLOR resolution ---

    fn rc(mode: ColorMode, tty: bool, force: Option<&str>, no: Option<&str>) -> bool {
        use std::ffi::OsStr;
        resolve_use_color_with_env(mode, tty, force.map(OsStr::new), no.map(OsStr::new))
    }

    #[test]
    fn cli_always_wins_over_everything() {
        assert!(rc(ColorMode::Always, false, None, Some("1")));
        assert!(rc(ColorMode::Always, false, Some("0"), Some("yes")));
    }

    #[test]
    fn cli_never_wins_over_everything() {
        assert!(!rc(ColorMode::Never, true, Some("1"), None));
        assert!(!rc(ColorMode::Never, true, None, None));
    }

    #[test]
    fn force_color_overrides_no_color_and_tty() {
        assert!(rc(ColorMode::Auto, false, Some("1"), Some("1")));
        assert!(rc(ColorMode::Auto, false, Some("3"), None));
        assert!(rc(ColorMode::Auto, false, Some("true"), None));
    }

    #[test]
    fn force_color_zero_or_empty_does_not_force() {
        assert!(!rc(ColorMode::Auto, false, Some("0"), None));
        assert!(!rc(ColorMode::Auto, false, Some(""), None));
    }

    #[test]
    fn no_color_disables_for_any_non_empty_value() {
        assert!(!rc(ColorMode::Auto, true, None, Some("1")));
        assert!(!rc(ColorMode::Auto, true, None, Some("yes")));
        assert!(!rc(ColorMode::Auto, true, None, Some("anything")));
    }

    #[test]
    fn no_color_empty_does_not_disable() {
        // per the spec, only non-empty values count.
        assert!(rc(ColorMode::Auto, true, None, Some("")));
    }

    #[test]
    fn auto_falls_back_to_tty_when_env_silent() {
        assert!(rc(ColorMode::Auto, true, None, None));
        assert!(!rc(ColorMode::Auto, false, None, None));
    }

    // --- paging mode parsing ---

    #[test]
    fn paging_mode_parsing() {
        assert_eq!(
            <PagingMode as argh::FromArgValue>::from_arg_value("auto").unwrap(),
            PagingMode::Auto
        );
        assert_eq!(
            <PagingMode as argh::FromArgValue>::from_arg_value("always").unwrap(),
            PagingMode::Always
        );
        assert_eq!(
            <PagingMode as argh::FromArgValue>::from_arg_value("never").unwrap(),
            PagingMode::Never
        );
    }

    // --- list format ---

    #[test]
    fn list_format_arg_parsing() {
        let parse = <ListFormatArg as argh::FromArgValue>::from_arg_value;
        assert_eq!(parse("pretty").unwrap().0, ListFormat::Pretty);
        assert_eq!(parse("plain").unwrap().0, ListFormat::Plain);
        assert_eq!(parse("json").unwrap().0, ListFormat::Json);
        assert!(parse("xml").is_err());
    }

    // --- poll interval ---

    #[test]
    fn poll_interval_parsing() {
        use std::time::Duration;
        let p = |s: &str| PollInterval::parse(s).map(|d| d.0);
        assert_eq!(p("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(p("2"), Ok(Duration::from_secs(2)));
        assert_eq!(p("2s"), Ok(Duration::from_secs(2)));
        assert_eq!(p("1.5s"), Ok(Duration::from_millis(1500)));
        assert_eq!(p("1m"), Ok(Duration::from_secs(60)));
        assert_eq!(p("30s"), Ok(Duration::from_secs(30)));
        // clamping at MIN
        assert_eq!(p("10ms"), Ok(PollInterval::MIN));
        assert_eq!(p("0.001s"), Ok(PollInterval::MIN));
    }

    #[test]
    fn poll_interval_rejections() {
        assert!(PollInterval::parse("").is_err());
        assert!(PollInterval::parse("abc").is_err());
        assert!(PollInterval::parse("10x").is_err());
        assert!(PollInterval::parse("-5s").is_err());
        assert!(PollInterval::parse("0").is_err());
        assert!(PollInterval::parse("0ms").is_err());
    }

    // theme tests live in `lsh_defs::theme::tests` -- the colourmap impl
    // moved to the shared crate alongside the canonical
    // `HighlightKind::default_color` table.
}
