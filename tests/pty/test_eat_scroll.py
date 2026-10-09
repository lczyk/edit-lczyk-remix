"""Horizontal scroll in the eat viewer.

End re-anchors on the tail and must not reset the columns the reader
scrolled to: a tail snap only moves the vertical offset. Scrolling right
is bounded by the text's own width, so a fast flick crosses a wide line
in one go and stops at its end.
"""

import os
import tempfile

from framework import (
    Edit,
    expect,
    pause,
    test,
)


def _numbered_lines(n):
    # Column 24 (three Right presses at 8 columns each) lands inside the
    # per-line tag, so a row that still shows its tag is one whose
    # horizontal offset was kept.
    return ["line %03d " % i + ("L%02d-" % i) * 30 for i in range(n)]


WHEEL_RIGHT = b"\x1b[<67;10;5M"


def _write(path, lines):
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


@test
def end_keeps_columns():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "long.txt")
        _write(path, _numbered_lines(30))
        with Edit(["--eat", "--color", "never", "--wrap", "never", path],
                  cols=40, rows=8) as ed:
            for _ in range(3):
                ed.send(b"l")
            ed.send(b"g")
            expect(b"L00-L00" in ed.plain, "not scrolled right at the top")
            mark = ed.mark()
            ed.send(b"G")
            frame = ed.plain_since(mark)
            expect(b"L29-L29" in frame, "End did not reach the tail")
            expect(b"line 029" not in frame, "End reset the horizontal offset")
            ed.send(b"q")


def _wide_file(path):
    """A wide line, a block of short ones, then a second wide line.

    Column ~185 carries a distinct tag on each wide line, so what is on
    screen says which columns the viewport is showing.
    """
    _write(path,
           ["start " + "." * 180 + " END"]
           + ["short %d" % i for i in range(20)]
           + ["begin " + "-" * 180 + " TAIL"])


@test
def a_fast_flick_crosses_a_wide_line_in_one_go():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "wide.txt")
        _wide_file(path)
        with Edit(["--eat", "--color", "never", "--wrap", "never", path],
                  cols=40, rows=10) as ed:
            mark = ed.mark()
            # one write, as a trackpad flick arrives: the events queue up
            # faster than frames are painted.
            os.write(ed.fd, WHEEL_RIGHT * 80)
            pause(1.2)
            ed.drain()
            frame = ed.plain_since(mark)
            expect(b"END" in frame, "the flick stalled short of the line's end")


@test
def moving_down_onto_short_lines_keeps_the_columns():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "wide.txt")
        _wide_file(path)
        with Edit(["--eat", "--color", "never", "--wrap", "never", path],
                  cols=40, rows=10) as ed:
            os.write(ed.fd, WHEEL_RIGHT * 80)
            pause(1.2)
            ed.drain()
            expect(b"END" in ed.plain, "did not reach the right edge")
            mark = ed.mark()
            # down through the short block onto the second wide line: the
            # short rows are all left of the viewport, and their width must
            # not drag it back.
            for _ in range(14):
                ed.send(b"j")
            frame = ed.plain_since(mark)
            expect(b"TAIL" in frame, "moving down dragged the viewport back")
            expect(b"begin" not in frame, "viewport landed at column 0")


@test
def a_resize_keeps_the_viewport():
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "long.txt")
        _write(path, _numbered_lines(60))
        with Edit(["--eat", "--color", "never", "--wrap", "never", path],
                  cols=40, rows=8) as ed:
            for _ in range(20):
                ed.send(b"j")
            expect(b"line 020" in ed.plain, "did not scroll down before the resize")
            # the viewer never shows a cursor, so the reader's place is the
            # viewport, and a new width must not pull it to the cursor's row.
            screen = ed.screen()
            expect(b"line 020" in screen, f"the resize moved the viewport: {screen[:200]!r}")
            expect(b"line 000" not in screen, "the resize jumped to the top")
            ed.send(b"q")
