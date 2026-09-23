"""A directory argument.

edit refuses one with a cat-style `edit: PATH: Is a directory` and a
failing exit code, before it reaches the TUI.
"""

import subprocess
import tempfile

from framework import EDIT_BIN, expect, test


def _run_cli(argv, cwd=None, timeout=5.0):
    proc = subprocess.run(
        [EDIT_BIN] + argv,
        capture_output=True,
        cwd=cwd,
        timeout=timeout,
    )
    return proc.returncode, proc.stdout + proc.stderr


@test
def edit_refuses_a_directory():
    with tempfile.TemporaryDirectory() as tmp:
        rc, out = _run_cli([tmp])
        expect(rc != 0, f"expected a failing exit code, got {rc}")
        expect(f"edit: {tmp}: Is a directory".encode() in out,
               f"expected a cat-style message, got: {out!r}")


@test
def edit_refuses_the_current_directory():
    with tempfile.TemporaryDirectory() as tmp:
        rc, out = _run_cli(["."], cwd=tmp)
        expect(rc != 0, f"expected a failing exit code, got {rc}")
        expect(b"edit: .: Is a directory" in out,
               f"expected a cat-style message, got: {out!r}")
