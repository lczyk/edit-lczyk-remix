"""`eat -x -- CMD`: a command's stdout in place of a file.

On a tty the snapshot viewer shows it and `r` runs the command again;
a failed rerun keeps what was on screen and says so in the header.
Off a tty the output streams through like a file, and the command's own
exit code comes back out. `edit --eat` and the `eat` name are the same
program, so both forms are driven.
"""

import os
import subprocess
import tempfile

from framework import EDIT_BIN, Edit, expect, test


def _write(path, lines):
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


def _numbered(n, tag):
    return ["line %03d %s" % (i, tag) for i in range(n)]


def _run_cli(argv, timeout=5.0):
    proc = subprocess.run([EDIT_BIN] + argv, capture_output=True, timeout=timeout)
    return proc.returncode, proc.stdout, proc.stderr


@test
def exec_shows_the_output_and_r_runs_it_again():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "doc.txt")
        for argv0 in (None, "eat"):
            _write(path, _numbered(30, "v1"))
            argv = ["--color", "never", "-x", "--", "cat", path]
            if argv0 is None:
                argv = ["--eat"] + argv
            with Edit(argv, cols=60, rows=8, argv0=argv0) as ed:
                expect(b"line 000 v1" in ed.plain,
                       f"the command's output is not on screen: {ed.plain[:200]!r}")
                expect(b"cat " in ed.plain, "the header does not name the command")
                for _ in range(10):
                    ed.send(b"j")
                _write(path, _numbered(30, "v2"))
                mark = ed.mark()
                ed.send(b"r")
                frame = ed.plain_since(mark)
                expect(b"line 010 v2" in frame, "r did not run the command again")
                expect(b"line 000 v2" not in frame, "the rerun lost the viewport")
                ed.send(b"q")


@test
def a_failed_rerun_keeps_the_screen_and_says_why():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "doc.txt")
        _write(path, _numbered(10, "v1"))
        with Edit(["--eat", "--color", "never", "-x", "--", "cat", path],
                  cols=60, rows=8) as ed:
            expect(b"line 000 v1" in ed.plain,
                   f"the command's output is not on screen: {ed.plain[:200]!r}")
            os.remove(path)
            mark = ed.mark()
            ed.send(b"r")
            frame = ed.plain_since(mark)
            expect(b"[exit 1: cat:" in frame, f"no failure note in the header: {frame!r}")
            screen = ed.screen()
            expect(b"line 000 v1" in screen, "the failed rerun blanked the buffer")
            ed.send(b"q")


@test
def exec_off_a_tty_streams_the_output():
    rc, out, err = _run_cli(["--eat", "-x", "--", "printf", "a\\nb\\n"])
    expect(rc == 0, f"exit {rc}: {err!r}")
    expect(out == b"a\nb\n", f"got {out!r}")


@test
def exec_off_a_tty_passes_the_exit_code_through():
    rc, out, err = _run_cli(["--eat", "-x", "--", "sh", "-c", "echo out; echo err >&2; exit 3"])
    expect(rc == 3, f"exit {rc}, want the command's 3")
    expect(out == b"out\n", f"stdout {out!r}")
    expect(b"err" in err, f"stderr not forwarded: {err!r}")


@test
def exec_without_a_command_is_a_usage_error():
    rc, out, err = _run_cli(["--eat", "-x"])
    expect(rc == 2, f"exit {rc}, want 2")
    expect(b"-x needs a command" in err, f"got {err!r}")


@test
def a_separator_keeps_the_command_s_own_flags():
    rc, out, err = _run_cli(["--eat", "-x", "--", "printf", "--", "-n\\n"])
    expect(rc == 0, f"exit {rc}: {err!r}")
    expect(out == b"-n\n", f"got {out!r}")
