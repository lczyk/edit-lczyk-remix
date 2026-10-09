"""`-` names stdin, wherever it stands among the paths."""

import os
import subprocess
import tempfile

from framework import EDIT_BIN, expect, test


def _run_cli(argv, stdin=b"", timeout=5.0):
    proc = subprocess.run([EDIT_BIN] + argv, input=stdin, capture_output=True,
                          timeout=timeout)
    return proc.returncode, proc.stdout, proc.stderr


@test
def a_bare_dash_reads_stdin():
    rc, out, err = _run_cli(["--eat", "-"], stdin=b"hi\n")
    expect(rc == 0, f"exit {rc}: {err!r}")
    expect(out == b"hi\n", f"got {out!r}")


@test
def a_bare_dash_takes_its_place_among_files():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "a.txt")
        with open(path, "w") as f:
            f.write("file\n")
        rc, out, err = _run_cli(["--eat", "-p", path, "-", path], stdin=b"stdin\n")
        expect(rc == 0, f"exit {rc}: {err!r}")
        expect(out == b"file\nstdin\nfile\n", f"got {out!r}")
