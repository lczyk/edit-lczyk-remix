"""`eat -w`: the live view.

The viewer polls the source and loads it again when it changed, as if
`r` had been pressed: the viewport stays, the marker says what the last
load had to say. It needs a tty and one file or a command, and refuses
everything else with exit 2 rather than half-working.
"""

import os
import subprocess
import tempfile

from framework import EDIT_BIN, Edit, expect, pause, test


def _write(path, lines):
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


def _numbered(n, tag):
    return ["line %03d %s" % (i, tag) for i in range(n)]


def _run_cli(argv, timeout=5.0):
    proc = subprocess.run([EDIT_BIN] + argv, capture_output=True, timeout=timeout)
    return proc.returncode, proc.stdout, proc.stderr


def _settles_on(ed, needle, tries=10):
    for _ in range(tries):
        pause(0.2)
        ed.drain()
        if needle in ed.plain:
            return True
    return False


@test
def a_changed_file_is_loaded_without_a_keypress():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "doc.txt")
        _write(path, _numbered(40, "v1"))
        with Edit(["--eat", "--color", "never", "-w", "200ms", path], cols=120, rows=8) as ed:
            expect(b"live 200ms" in ed.plain, "the header does not say it is live")
            for _ in range(10):
                ed.send(b"j")
            expect(b"line 010 v1" in ed.plain, "did not scroll before the change")
            _write(path, _numbered(40, "v2"))
            expect(_settles_on(ed, b"line 010 v2"), "the change was not picked up")
            expect(b"line 000 v2" not in ed.plain, "the reload lost the viewport")
            ed.send(b"q")


@test
def a_changed_command_output_is_loaded_without_a_keypress():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "doc.txt")
        _write(path, _numbered(40, "v1"))
        env = {"EAT_WATCH_INTERVAL_MS": "200"}
        with Edit(["--eat", "--color", "never", "-w", "-x", "--", "cat", path],
                  cols=120, rows=8, env=env) as ed:
            expect(b"live 200ms" in ed.plain, "the env default did not reach the header")
            for _ in range(10):
                ed.send(b"j")
            _write(path, _numbered(40, "v2"))
            expect(_settles_on(ed, b"line 010 v2"), "the change was not picked up")
            expect(b"line 000 v2" not in ed.plain, "the reload lost the viewport")
            ed.send(b"q")


@test
def a_vanished_file_keeps_the_screen_and_says_so():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "doc.txt")
        _write(path, _numbered(10, "v1"))
        with Edit(["--eat", "--color", "never", "-w", "200ms", path], cols=80, rows=8) as ed:
            os.remove(path)
            expect(_settles_on(ed, b"[No such file"), "no note in the header")
            screen = ed.screen()
            expect(b"line 000 v1" in screen, "the buffer was blanked")
            _write(path, _numbered(10, "v3"))
            expect(_settles_on(ed, b"line 000 v3"), "the file coming back was not picked up")
            ed.send(b"q")


@test
def watch_refuses_what_the_viewer_cannot_show():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "doc.txt")
        _write(path, ["x"])
        cases = [
            (["--eat", "-w", path], b"needs a tty"),
            (["--eat", "-w", "-x", "--", "cat", path], b"needs a tty"),
            (["--eat", "-w"], b"needs a file"),
            (["--eat", "-w", "-"], b"cannot watch stdin"),
            (["--eat", "-w", path, path], b"takes a single file"),
            (["--eat", "-w", d], b"cannot watch a directory"),
            (["--eat", "-w", "--plain", path], b"--plain"),
        ]
        for argv, why in cases:
            rc, out, err = _run_cli(argv)
            expect(rc == 2, f"{argv}: exit {rc}, want 2: {err!r}")
            expect(why in err, f"{argv}: got {err!r}")
