"""Reload in the eat snapshot view keeps the viewport.

`r` re-reads the file. The reader's place in it is the viewport's top
row, and a reload must not move it: the same row stays on top, or, when
the file shrank past it, the viewport is pulled back just far enough to
keep the last row on the bottom edge. The cursor is not the reader's
place -- the viewer never shows one.
"""

import os
import tempfile

from framework import (
    Edit,
    expect,
    test,
)


def _write(path, lines):
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


def _numbered(n, tag):
    return ["line %03d %s" % (i, tag) for i in range(n)]


@test
def reload_keeps_the_top_row():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "doc.txt")
        _write(path, _numbered(60, "v1"))
        with Edit(["--eat", "--color", "never", path], cols=40, rows=8) as ed:
            for _ in range(20):
                ed.send(b"j")
            expect(b"line 020 v1" in ed.plain, "did not scroll down before the reload")

            # every visible row changes, so the frame after `r` is the
            # whole viewport rather than a diff of nothing.
            _write(path, _numbered(60, "v2"))
            mark = ed.mark()
            ed.send(b"r")
            frame = ed.plain_since(mark)
            expect(b"line 020 v2" in frame, "the top row moved on reload")
            expect(b"line 000 v2" not in frame, "reload jumped to the top")
            expect(b"line 019 v2" not in frame, "reload drifted up a row")
            ed.send(b"q")


@test
def reload_clamps_the_top_row_to_a_shorter_file():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "doc.txt")
        _write(path, _numbered(60, "v1"))
        with Edit(["--eat", "--color", "never", path], cols=40, rows=8) as ed:
            for _ in range(20):
                ed.send(b"j")
            expect(b"line 020 v1" in ed.plain, "did not scroll down before the reload")

            _write(path, _numbered(5, "v3"))
            mark = ed.mark()
            ed.send(b"r")
            frame = ed.plain_since(mark)
            expect(b"line 004 v3" in frame, "the last row of the shorter file is not on screen")
            expect(b"line 020" not in frame, "stale rows survived the reload")
            ed.send(b"q")


@test
def reload_pulls_back_to_fill_the_screen_when_the_file_shrinks():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "doc.txt")
        _write(path, _numbered(60, "v1"))
        with Edit(["--eat", "--color", "never", path], cols=40, rows=8) as ed:
            for _ in range(30):
                ed.send(b"j")
            expect(b"line 030 v1" in ed.plain, "did not scroll down before the reload")

            # 25 lines plus the empty one after the final newline, on a
            # 7-row body: the top row can be no lower than line 019.
            _write(path, _numbered(25, "v3"))
            mark = ed.mark()
            ed.send(b"r")
            frame = ed.plain_since(mark)
            expect(b"line 019 v3" in frame, "the viewport did not pull back to the tail")
            expect(b"line 024 v3" in frame, "the last row is not on screen")
            expect(b"line 018 v3" not in frame, "the viewport pulled back too far")
            ed.send(b"q")
