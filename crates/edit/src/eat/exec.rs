//! `eat -x -- CMD ARGS...`: the source is a command's stdout rather than a
//! file. The command runs to completion once per load, with stdin closed
//! and stderr captured; what a failure means for the view is the caller's
//! call.

use std::io;
use std::process::{Command, ExitStatus, Stdio};

pub(crate) struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: ExitStatus,
}

/// Run `argv` to completion. `Err` is a spawn failure (not found, not
/// executable, empty argv); a nonzero exit is `Ok` with the status set.
pub(crate) fn run(argv: &[String]) -> io::Result<Output> {
    let Some((prog, args)) = argv.split_first() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no command"));
    };
    let out = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    Ok(Output { stdout: out.stdout, stderr: out.stderr, status: out.status })
}

/// The command as a shell would show it: an argument with whitespace,
/// quotes or nothing in it is single-quoted.
pub(crate) fn label(argv: &[String]) -> String {
    let mut out = String::new();
    for (i, a) in argv.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let plain = !a.is_empty()
            && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c));
        if plain {
            out.push_str(a);
        } else {
            out.push('\'');
            out.push_str(&a.replace('\'', "'\\''"));
            out.push('\'');
        }
    }
    out
}

/// One line on a run that did not exit 0: the exit code (or the signal)
/// and the last non-empty stderr line, if there was one.
pub(crate) fn failure_note(out: &Output) -> String {
    let how = match out.status.code() {
        Some(code) => format!("exit {code}"),
        None => "killed by signal".to_string(),
    };
    let last = String::from_utf8_lossy(&out.stderr);
    match last.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
        Some(line) => format!("{how}: {line}"),
        None => how,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn label_quotes_only_what_a_shell_would_need() {
        assert_eq!(label(&argv(&["git", "diff", "a..b", "--stat"])), "git diff a..b --stat");
        assert_eq!(label(&argv(&["sh", "-c", "echo hi"])), "sh -c 'echo hi'");
        assert_eq!(label(&argv(&["printf", "it's", ""])), "printf 'it'\\''s' ''");
    }

    #[test]
    fn run_captures_both_streams_and_the_status() {
        let out = run(&argv(&["sh", "-c", "echo out; echo err >&2; exit 3"])).unwrap();
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(failure_note(&out), "exit 3: err");
    }

    #[test]
    fn failure_note_without_stderr_is_just_the_code() {
        let out = run(&argv(&["sh", "-c", "exit 7"])).unwrap();
        assert_eq!(failure_note(&out), "exit 7");
    }

    #[test]
    fn a_missing_program_is_a_spawn_error() {
        assert!(run(&argv(&["/nonexistent/eat-test-program"])).is_err());
        assert!(run(&[]).is_err());
    }
}
