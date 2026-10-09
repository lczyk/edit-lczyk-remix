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
import time

from framework import EDIT_BIN, Edit, expect, pause, test


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
                # the rerun lands when the command returns, usually inside
                # the startup grace, under load a tick later.
                ed.send(b"r", settle=0.3)
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
            ed.send(b"r", settle=0.3)
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


@test
def a_static_command_view_does_not_poll():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "doc.txt")
        _write(path, _numbered(5, "v1"))
        with Edit(["--eat", "--color", "never", "-x", "--", "cat", path],
                  cols=100, rows=8) as ed:
            # the file poll's marker is 2s in; a command has no cheap probe
            # and must not run or claim a change on its own.
            pause(2.6)
            ed.drain(0.2)
            expect(b"modified on disk" not in ed.plain, "a static command view raised the marker")
            expect(b"modified on disk" not in ed.screen(), "a static command view raised the marker")
            ed.send(b"q")


@test
def a_slow_command_does_not_block_the_viewer():
    with Edit(["--eat", "--color", "never", "-w", "300ms", "-x", "--",
               "sh", "-c", "sleep 2; echo v1"], cols=100, rows=8) as ed:
        expect(b"[running]" in ed.plain, f"no running marker at startup: {ed.plain[:200]!r}")
        expect(b"live 300ms" in ed.plain, "the header does not say it is live")
        # keys are answered while the command runs: q quits at once. the
        # pty has to be drained meanwhile, or the exit blocks on the tty.
        ed.send(b"q", drain=False)
        deadline = time.monotonic() + 1.0
        exited = False
        while not exited and time.monotonic() < deadline:
            ed.drain(0.05)
            exited = ed._wait_exit(0.05)
        expect(exited, "q did not quit while the command was running")


@test
def a_slow_command_lands_when_it_returns():
    with Edit(["--eat", "--color", "never", "-x", "--",
               "sh", "-c", "sleep 1; echo land$((40 + 2))"], cols=100, rows=8) as ed:
        expect(b"[running]" in ed.plain, "no running marker at startup")
        expect(b"land42" not in ed.plain, "output before the command returned")
        pause(1.6)
        ed.drain(0.2)
        expect(b"land42" in ed.plain, "the output did not land")
        expect(b"[running]" not in ed.screen(), "the running marker outlived the run")
        ed.send(b"q")


@test
def a_command_interval_has_a_floor():
    rc, out, err = _run_cli(["--eat", "-w", "50ms", "-x", "--", "true"])
    expect(rc == 2, f"non-tty should refuse -w: {err!r}")
    with Edit(["--eat", "--color", "never", "-w", "50ms", "-x", "--", "true"],
              cols=100, rows=8) as ed:
        expect(b"live 250ms" in ed.plain, f"the floor did not apply: {ed.plain[:160]!r}")
        ed.send(b"q")
