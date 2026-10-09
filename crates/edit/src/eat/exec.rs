//! `eat -x -- CMD ARGS...`: the source is a command's stdout rather than a
//! file. The command runs to completion once per load, with stdin closed
//! and stderr captured; what a failure means for the view is the caller's
//! call. The viewer runs it in the background ([`start`] / [`Run`]) so a
//! slow command never freezes the screen; the stream path waits ([`run`]).

use std::io::{self, Read as _};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;

pub(crate) struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: ExitStatus,
}

/// Run `argv` to completion. `Err` is a spawn failure (not found, not
/// executable, empty argv); a nonzero exit is `Ok` with the status set.
pub(crate) fn run(argv: &[String]) -> io::Result<Output> {
    let out = spawn(argv)?.wait_with_output()?;
    Ok(Output { stdout: out.stdout, stderr: out.stderr, status: out.status })
}

/// The command gets its own process group, so that killing it later
/// takes its children too: `sh -c 'sleep 30; ...'` is a shell and a
/// sleep, and the sleep holds the pipes open after the shell is gone.
fn spawn(argv: &[String]) -> io::Result<Child> {
    use std::os::unix::process::CommandExt as _;
    let Some((prog, args)) = argv.split_first() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no command"));
    };
    Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
}

/// A command running in the background. Its two pipes are drained on
/// threads so a chatty command cannot fill one and stall; [`Run::poll`]
/// is a non-blocking `try_wait`. Dropping it kills the child, so a
/// command that outlives the viewer does not keep running into a closed
/// pipe.
pub(crate) struct Run {
    child: Child,
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
}

/// Start `argv` in the background. `Err` is a spawn failure, as for
/// [`run`]; what the command does after that is read off [`Run::poll`].
pub(crate) fn start(argv: &[String]) -> io::Result<Run> {
    let mut child = spawn(argv)?;
    let stdout = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    });
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    });
    Ok(Run { child, stdout, stderr })
}

impl Run {
    /// Give a quick command the chance to return before the first paint,
    /// so `cat` never flashes `[running]`. A slow one is left to `poll`.
    pub(crate) fn wait_up_to(&mut self, grace: std::time::Duration) -> io::Result<Option<Output>> {
        let until = std::time::Instant::now() + grace;
        loop {
            if let Some(out) = self.poll()? {
                return Ok(Some(out));
            }
            if std::time::Instant::now() >= until {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// `Some` once the command has exited, with everything it wrote.
    pub(crate) fn poll(&mut self) -> io::Result<Option<Output>> {
        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };
        let stdout = self.stdout.take().and_then(|h| h.join().ok()).unwrap_or_default();
        let stderr = self.stderr.take().and_then(|h| h.join().ok()).unwrap_or_default();
        Ok(Some(Output { stdout, stderr, status }))
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        // The whole group, or a grandchild keeps the pipes open and the
        // reader threads blocked in read(2), which on macOS holds up the
        // process exit until it is done.
        // SAFETY: a plain signal to the group this process created.
        unsafe {
            libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
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
    fn a_background_run_is_polled_not_awaited() {
        let mut run = start(&argv(&["sh", "-c", "sleep 0.3; echo late; exit 2"])).unwrap();
        assert!(run.poll().unwrap().is_none(), "polled as finished before it could be");
        let started = std::time::Instant::now();
        let out = loop {
            if let Some(out) = run.poll().unwrap() {
                break out;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(5), "never finished");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(out.stdout, b"late\n");
        assert_eq!(out.status.code(), Some(2));
    }

    #[test]
    fn dropping_a_run_kills_the_command_and_its_children() {
        // The shell is the child; the sleep is its child and would outlive
        // a kill of the shell alone, holding the pipes open.
        let run = start(&argv(&["sh", "-c", "sleep 30 & echo $!; wait"])).unwrap();
        let shell = run.child.id();
        let sleeper = loop {
            // the pid line is all the shell prints before it waits.
            if let Some(h) = run.stdout.as_ref()
                && h.is_finished()
            {
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            let probe = std::process::Command::new("pgrep")
                .args(["-P", &shell.to_string()])
                .output()
                .unwrap();
            let s = String::from_utf8_lossy(&probe.stdout);
            if let Some(pid) = s.lines().next().and_then(|l| l.trim().parse::<u32>().ok()) {
                break Some(pid);
            }
        };
        let sleeper = sleeper.expect("the shell forked its sleep");
        drop(run);
        let alive = |pid: u32| {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(!alive(shell), "shell {shell} outlived its Run");
        // The sleep is reparented and dies on the group signal; give init
        // a moment to notice.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!alive(sleeper), "sleep {sleeper} outlived its Run");
    }

    #[test]
    fn a_missing_program_is_a_spawn_error() {
        assert!(run(&argv(&["/nonexistent/eat-test-program"])).is_err());
        assert!(run(&[]).is_err());
    }
}
