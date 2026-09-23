//! The non-tty output pipeline: highlight to ansi and write through.
//!
//! Kept deliberately separate from the alt-screen views. This half serves
//! pipes, redirects and pagers, where a framebuffer is meaningless --
//! `eat foo.go | grep` cannot consume one -- so it composes escape
//! sequences directly instead.

use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use gutter::GutterMark;
use lsh::runtime::{ConflictTag, Runtime};
use lsh_defs::{ASSEMBLY, CHARSETS, STRINGS};
use stdext::arena::scratch_arena;

use super::cli::{
    ColorMode, LineRange, PagingMode, WrapMode, print_short_help, prog_name, resolve_use_color,
};
use super::detect::head_bytes;
use super::{gutter_view, listing, theme};
use lsh_defs::detect::{NO_USER_ASSOCIATIONS, find_language, resolve};

/// resolve the pager binary path.
fn resolve_pager() -> Option<String> {
    // EAT_PAGER > PAGER > less
    if let Ok(p) = std::env::var("EAT_PAGER")
        && !p.is_empty()
    {
        return Some(p);
    }
    if let Ok(p) = std::env::var("PAGER")
        && !p.is_empty()
    {
        return Some(p);
    }

    // manual $PATH walk for less
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = Path::new(dir).join("less");
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }

    None
}

/// render the highlighted body bytes for a single line into `out`. no gutter
/// prefix and no trailing newline -- `write_highlighted_line` composes those
/// around it. returns where the line sits relative to a merge conflict, for
/// the gutter.
pub(crate) fn render_body(
    runtime: &mut Runtime,
    color_map: &[&str],
    line: &str,
    use_color: bool,
    out: &mut Vec<u8>,
) -> ConflictTag {
    use std::io::Write as _;
    let scratch = scratch_arena(None);
    let parsed = runtime.parse_next_line::<u32>(&scratch, line.as_bytes());
    let highlights = parsed.spans;
    // NOTE: lsh emits byte indices that may not land on utf-8 char
    // boundaries, so slice via as_bytes() and write_all -- string
    // slicing would panic on multi-byte codepoints (e.g. man pages
    // with em-dashes / smart quotes).
    let line_bytes = line.as_bytes();
    for w in highlights.windows(2) {
        let curr = &w[0];
        let next = &w[1];
        let start = curr.start;
        let end = next.start.min(line_bytes.len());
        let kind = curr.kind;
        let text = &line_bytes[start..end];

        if use_color
            && let Some(color) = color_map.get(kind as usize)
            && !color.is_empty()
        {
            let _ = write!(out, "{color}");
            out.extend_from_slice(text);
            out.extend_from_slice(b"\x1b[m");
        } else {
            out.extend_from_slice(text);
        }
    }
    parsed.conflict
}

/// write one line to `writer`, optionally with a leading line number and ansi
/// colour escapes from `color_map`. when `gutter` is `Some`, prepend the
/// gutter prefix (right-aligned line number + separator) using mark
/// information from it, with a line inside a merge conflict outranking the
/// diff mark. used by the bulk path (`print_highlighted`) and the streaming
/// follow path.
pub(crate) fn write_highlighted_line(
    writer: &mut dyn Write,
    runtime: &mut Runtime,
    color_map: &[&str],
    line_no: usize,
    line: &str,
    gutter: Option<&gutter_view::Gutter>,
    use_color: bool,
) -> io::Result<()> {
    // Position-sensitive constructs (a line-1 frontmatter fence) read the
    // line number from the vm, and a top-level return clears it.
    runtime.set_line_number(line_no as u32);
    // The body is rendered first: the gutter needs to know whether the
    // highlighter put this line inside a conflict.
    let mut body = Vec::with_capacity(line.len() + 16);
    let conflict = render_body(runtime, color_map, line, use_color, &mut body);
    if let Some(g) = gutter {
        let mark =
            if conflict != ConflictTag::None { GutterMark::Conflict } else { g.mark(line_no) };
        gutter_view::write_prefix(writer, line_no, g.width, mark, use_color)?;
    }
    writer.write_all(&body)?;
    writeln!(writer)?;
    Ok(())
}

/// write one file's highlighted lines (plus optional header) to a writer.
/// caller owns sink + pager lifecycle so a multi-file run shares one pager.
/// `first_line` is the 1-based file line number of `lines[0]`: a line range
/// shows a slice, and the numbers the gutter prints and the vm sees stay
/// the file's own.
#[allow(clippy::too_many_arguments)]
fn print_highlighted(
    writer: &mut dyn Write,
    runtime: &mut Runtime,
    lines: &[String],
    first_line: usize,
    color_map: &[&str],
    show_numbers: bool,
    header: Option<&str>,
    use_color: bool,
    gutter: Option<&gutter_view::Gutter>,
) -> io::Result<()> {
    write_header(writer, header, use_color)?;

    for (i, line) in lines.iter().enumerate() {
        let g = if show_numbers { gutter } else { None };
        write_highlighted_line(writer, runtime, color_map, first_line + i, line, g, use_color)?;
    }

    Ok(())
}

/// write a directory listing's rows, which arrive already coloured, with
/// the same header and line-number prefix a file would get.
fn write_listing(
    writer: &mut dyn Write,
    rows: &[String],
    first_line: usize,
    number_width: Option<usize>,
    header: Option<&str>,
    use_color: bool,
) -> io::Result<()> {
    write_header(writer, header, use_color)?;
    for (i, row) in rows.iter().enumerate() {
        if let Some(width) = number_width {
            gutter_view::write_prefix(writer, first_line + i, width, GutterMark::None, use_color)?;
        }
        writeln!(writer, "{row}")?;
    }
    Ok(())
}

fn write_header(writer: &mut dyn Write, header: Option<&str>, use_color: bool) -> io::Result<()> {
    match header {
        Some(hdr) if use_color => writeln!(writer, "\x1b[1m--- {hdr} ---\x1b[m"),
        Some(hdr) => writeln!(writer, "--- {hdr} ---"),
        None => Ok(()),
    }
}

/// the slice of `all` a line range selects, plus the 1-based line number
/// of its first row.
fn apply_line_range<'a>(all: &'a [String], range: Option<&LineRange>) -> (&'a [String], usize) {
    let first_line = range.map_or(1, |r| r.start.unwrap_or(1));
    let Some(range) = range else {
        return (all, first_line);
    };
    let start = first_line.saturating_sub(1).min(all.len());
    let end = range.end.unwrap_or(all.len()).clamp(start, all.len());
    (&all[start..end], first_line)
}

/// spawn the pager (if any) and return its stdin + child handle.
/// caller must drop the writer to signal EOF, then wait on the child.
fn open_pager_sink(
    pager_path: &str,
    wrap: bool,
) -> io::Result<(Box<dyn Write>, std::process::Child)> {
    let mut args: Vec<&str> = Vec::new();
    let pager_name = Path::new(pager_path).file_name().and_then(|n| n.to_str()).unwrap_or("");

    // -R: pass ansi colour escapes through raw. -F: exit if content fits one screen.
    // we deliberately do NOT pass -X (--no-init): without alt-screen, terminals
    // can't forward mouse-wheel events to less via xterm alternate-scroll, so the
    // page won't scroll under the cursor. less >=530 fixed the old -F-clears-screen
    // bug that motivated -X; older less is rare enough not to chase.
    if pager_name == "less" {
        args.extend_from_slice(&["-R", "-F"]);
        // less wraps by default, so only the chop case needs a flag.
        if !wrap {
            args.push("-S");
        }
    }

    let mut child = std::process::Command::new(pager_path)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().expect("piped");
    Ok((Box::new(stdin), child))
}

/// read a file into a Vec of lines.
pub(crate) fn read_file(path: &Path) -> io::Result<Vec<String>> {
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(128 * 1024, file);
    reader.lines().collect()
}

/// read stdin into a Vec of lines.
fn read_stdin() -> io::Result<Vec<String>> {
    let stdin = io::stdin();
    let reader = BufReader::with_capacity(128 * 1024, stdin.lock());
    reader.lines().collect()
}

/// run the cat-like path over a list of files and optional stdin.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    files: &[String],
    language_override: Option<&str>,
    plain: bool,
    show_numbers: bool,
    line_range: Option<LineRange>,
    color_mode: ColorMode,
    paging_mode: PagingMode,
    wrap_mode: WrapMode,
) -> ExitCode {
    let mut has_error = false;

    // resolve overridden language
    let lang_override = language_override.and_then(find_language);
    if let Some(name) = language_override
        && lang_override.is_none()
    {
        eprintln!("{}: unknown language '{name}'", prog_name());
        return ExitCode::from(2);
    }

    let color_map = theme::color_map();

    let stdin_is_tty = io::stdin().is_terminal();

    // tty + no args -> short help
    if stdin_is_tty && files.is_empty() {
        return print_short_help();
    }

    // collect inputs: expand "-" to stdin at its position
    let mut inputs: Vec<EatInput> = Vec::new();
    if files.is_empty() {
        // no args, stdin piped
        inputs.push(EatInput::Stdin);
    } else {
        for f in files {
            if f == "-" {
                inputs.push(EatInput::Stdin);
            } else {
                inputs.push(EatInput::File(PathBuf::from(f)));
            }
        }
    }

    // filter to actual files (not stdin) for multi-file header logic
    let file_count = inputs.iter().filter(|i| matches!(i, EatInput::File(_))).count();

    // decide paging + colour once for the whole run so a multi-file invocation
    // shares one pager rather than spawning one per file.
    let stdout_is_tty = io::stdout().is_terminal();
    let want_page = match paging_mode {
        PagingMode::Always => true,
        PagingMode::Never => false,
        PagingMode::Auto => stdout_is_tty,
    };
    let pager_path = if want_page { resolve_pager() } else { None };
    let use_color = match color_mode {
        ColorMode::Never => false,
        // through the pager pipe, stdout-is-tty would read false; treat paging
        // as a tty for colour purposes. otherwise consult env + tty.
        _ => pager_path.is_some() || resolve_use_color(color_mode, stdout_is_tty),
    };

    let mut pager_child: Option<std::process::Child> = None;
    let mut sink: Box<dyn Write> = match pager_path.as_deref() {
        Some(path) => match open_pager_sink(path, wrap_mode.resolve()) {
            Ok((w, c)) => {
                pager_child = Some(c);
                w
            }
            Err(_) => Box::new(io::stdout()),
        },
        None => Box::new(io::stdout()),
    };

    // tracks whether the pager (or downstream consumer) has hung up; once it
    // has, subsequent writes are pointless -- stop iterating.
    let mut sink_closed = false;

    for input in &inputs {
        if sink_closed {
            break;
        }
        if let EatInput::File(path) = input
            && path.is_dir()
        {
            let all = match listing::render(path, plain, use_color && !plain) {
                Ok(rows) => rows,
                Err(e) => {
                    eprintln!("{}: {}: {e}", prog_name(), path.display());
                    has_error = true;
                    continue;
                }
            };
            let (rows, first_line) = apply_line_range(&all, line_range.as_ref());
            let label = path.display().to_string();
            let header = (!plain && stdout_is_tty && inputs.len() > 1).then_some(label.as_str());
            let number_width = (show_numbers && !plain).then(|| all.len().to_string().len());
            match write_listing(sink.as_mut(), rows, first_line, number_width, header, use_color) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => sink_closed = true,
                Err(e) => {
                    eprintln!("{}: {e}", prog_name());
                    has_error = true;
                }
            }
            continue;
        }
        let (lines, path_for_detection, header_label) = match input {
            EatInput::File(path) => match read_file(path) {
                Ok(lines) => {
                    let label = if file_count > 1
                        || inputs.len() > 1
                        || inputs.iter().any(|i| matches!(i, EatInput::Stdin))
                    {
                        Some(path.display().to_string())
                    } else {
                        None
                    };
                    (lines, Some(path.as_path()), label)
                }
                Err(e) => {
                    eprintln!("{}: {}: {e}", prog_name(), path.display());
                    has_error = true;
                    continue;
                }
            },
            EatInput::Stdin => match read_stdin() {
                Ok(lines) => (lines, None, None),
                Err(e) => {
                    eprintln!("{}: stdin: {e}", prog_name());
                    has_error = true;
                    continue;
                }
            },
        };

        // apply line range: `shown` is the slice, `all` stays around so the
        // gutter's marks are indexed by the file's own line numbers.
        let all = lines;
        let (lines, first_line) = apply_line_range(&all, line_range.as_ref());

        let lang = lang_override.unwrap_or_else(|| {
            resolve(path_for_detection, NO_USER_ASSOCIATIONS, || head_bytes(lines))
        });

        if plain {
            // plain mode: cat to shared sink. no header, no decorations.
            for line in lines {
                if let Err(e) = writeln!(sink, "{line}") {
                    if e.kind() == io::ErrorKind::BrokenPipe {
                        sink_closed = true;
                        break;
                    }
                    eprintln!("{}: {e}", prog_name());
                    has_error = true;
                    sink_closed = true;
                    break;
                }
            }
            continue;
        }

        let header = if stdout_is_tty && inputs.len() > 1 { header_label.as_deref() } else { None };

        // build the gutter once per file when -n is on and we have a path to
        // resolve a baseline against. for stdin or with -n off, no gutter.
        let gutter = if show_numbers && let Some(p) = path_for_detection {
            // re-join the lines into a contiguous byte buffer for diffing.
            // BufRead::lines() already stripped \n, so we need to put them back.
            let mut bytes = Vec::with_capacity(all.iter().map(|l| l.len() + 1).sum());
            for l in &all {
                bytes.extend_from_slice(l.as_bytes());
                bytes.push(b'\n');
            }
            Some(gutter_view::Gutter::compute(p, &bytes, 1))
        } else {
            None
        };

        let mut runtime = Runtime::new(&ASSEMBLY, &STRINGS, &CHARSETS, lang.entrypoint);
        match print_highlighted(
            sink.as_mut(),
            &mut runtime,
            lines,
            first_line,
            &color_map,
            show_numbers,
            header,
            use_color,
            gutter.as_ref(),
        ) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                sink_closed = true;
            }
            Err(e) => {
                eprintln!("{}: {e}", prog_name());
                has_error = true;
            }
        }
    }

    // close sink (drops pager stdin so it sees EOF), then wait on pager.
    drop(sink);
    if let Some(mut c) = pager_child {
        let _ = c.wait();
    }

    if has_error { ExitCode::from(1) } else { ExitCode::from(0) }
}

enum EatInput {
    File(PathBuf),
    Stdin,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines inside a merge conflict get the conflict gutter mark even when
    /// the diff has nothing to say about them; the line after it is back to
    /// the diff's own mark.
    #[test]
    fn the_gutter_marks_every_line_of_a_conflict() {
        let mut runtime = Runtime::new(&ASSEMBLY, &STRINGS, &CHARSETS, lsh_defs::PLAIN.entrypoint);
        let gutter = gutter_view::Gutter { width: 1, marks: vec![GutterMark::None; 6] };
        let lines = ["a", "<<<<<<< HEAD", "ours", "=======", "theirs", ">>>>>>> b", "c"];
        let mut out = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            write_highlighted_line(&mut out, &mut runtime, &[], i + 1, line, Some(&gutter), false)
                .unwrap();
        }
        let text = String::from_utf8(out).unwrap();
        let prefixes: Vec<&str> = text.lines().map(|l| &l[..4]).collect();
        assert_eq!(prefixes, ["1 | ", "2 ! ", "3 ! ", "4 ! ", "5 ! ", "6 ! ", "7 | "]);
    }

    /// A slice that starts mid-file keeps the file's line numbers, so a
    /// `---` at the top of the slice is not mistaken for line-1 frontmatter.
    #[test]
    fn a_line_range_keeps_the_file_line_numbers() {
        let markdown = lsh_defs::detect::find_language("markdown").unwrap();
        let mut runtime = Runtime::new(&ASSEMBLY, &STRINGS, &CHARSETS, markdown.entrypoint);
        let color_map = theme::color_map();
        let lines: Vec<String> = ["---", "title: x", "---"].iter().map(|s| s.to_string()).collect();
        let gutter = gutter_view::Gutter { width: 1, marks: vec![GutterMark::None; 10] };
        let mut out = Vec::new();
        print_highlighted(
            &mut out,
            &mut runtime,
            &lines,
            6,
            &color_map,
            true,
            None,
            true,
            Some(&gutter),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("6\x1b[m"), "numbering starts at the slice's file line: {text:?}");
        assert!(!text.contains("\x1b[94m---"), "a mid-file --- is not frontmatter: {text:?}");
    }
}
