"""A directory argument.

edit refuses one with a cat-style `edit: PATH: Is a directory` and a
failing exit code, before it reaches the TUI. eat lists it instead, in
the place the file's contents would go.
"""

import os
import subprocess
import tempfile

from framework import EDIT_BIN, Edit, expect, test


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


def _eat_fixture(tmp):
    os.mkdir(os.path.join(tmp, "sub"))
    with open(os.path.join(tmp, "main.rs"), "w") as f:
        f.write("x" * 1234)
    with open(os.path.join(tmp, ".dotfile"), "w") as f:
        f.write("abc")


@test
def eat_lists_a_directory():
    with tempfile.TemporaryDirectory() as tmp:
        _eat_fixture(tmp)
        rc, out = _run_cli(["--eat", "--color", "never", tmp])
        expect(rc == 0, f"expected success, got {rc}: {out!r}")
        # The temp dir sits outside any repo, so there is no git column.
        expect(out == b"   - sub/\n   3 .dotfile\n1.2k main.rs\n",
               f"unexpected listing: {out!r}")


@test
def eat_plain_lists_names_only():
    with tempfile.TemporaryDirectory() as tmp:
        _eat_fixture(tmp)
        rc, out = _run_cli(["--eat", "-p", tmp])
        expect(rc == 0, f"expected success, got {rc}: {out!r}")
        expect(out == b"sub/\n.dotfile\nmain.rs\n", f"unexpected listing: {out!r}")


@test
def eat_lists_a_directory_among_files():
    with tempfile.TemporaryDirectory() as tmp:
        _eat_fixture(tmp)
        hello = os.path.join(tmp, "hello.txt")
        with open(hello, "w") as f:
            f.write("hello\n")
        rc, out = _run_cli(["--eat", "-p", hello, os.path.join(tmp, "sub"), hello])
        expect(rc == 0, f"expected success, got {rc}: {out!r}")
        # An empty directory lists as nothing, between the two files.
        expect(out == b"hello\nhello\n", f"unexpected output: {out!r}")
        rc, out = _run_cli(["--eat", "-p", hello, tmp])
        expect(out.startswith(b"hello\nsub/\n"), f"unexpected output: {out!r}")


@test
def eat_on_a_tty_pages_the_listing_rather_than_opening_the_viewer():
    with tempfile.TemporaryDirectory() as tmp:
        _eat_fixture(tmp)
        with Edit(["--eat", tmp], env={"PAGER": "cat"}) as ed:
            expect(b"main.rs" in ed.plain, f"expected the listing, got: {ed.plain!r}")
            expect(b"Is a directory" not in ed.plain,
                   f"the viewer tried to read the directory: {ed.plain!r}")
