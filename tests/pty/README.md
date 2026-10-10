# PTY tests for `edit`

Small test framework that drives the `edit` binary through a pseudo-terminal
and asserts against its rendered output.

## Running

Build first:

```sh
cargo build --release
```

Run everything:

```sh
python3 tests/pty/framework.py
```

Watch the editor live while tests run:

```sh
python3 tests/pty/framework.py --watch
```

`--watch` mirrors child-PTY output to your real terminal, sizes the PTY to
your current window, filters the startup probe sequences (so the terminal
doesn't reply to them and leak `rgb:...` bytes onto your shell afterwards), and
slows scripted `pause()` calls 2x so you can see what's happening. Tune pace
with `--pace 5`.

Run a subset:

```sh
python3 tests/pty/framework.py --filter prefill
python3 tests/pty/framework.py tests/pty/test_typing.py
```

Point at a debug build:

```sh
EDIT_BIN=target/debug/edit python3 tests/pty/framework.py
```

## Layout

- `framework.py` -- library + CLI entrypoint. Contains `Edit`, key constants,
  `expect`, `pause`, `fixture`, the `@test` registry, and the discovery runner.
- `fixtures/` -- sample input files, reached via `fixture("name")`. The
  highlighting tests instead pull from the lsh golden corpus at
  `crates/lsh/tests/fixtures/` (`LSH_FIXTURES_DIR`).
- `test_*.py` -- each defines `@test`-decorated functions.

Not part of `make verify` (that needs no Python and no built binary), but the
`pty` job in [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml) runs
them. The whole suite should pass; a failure is a real regression.

## Chords

**Send `UNDO`, not `CTRL_Z`.** Cut/Copy/Paste/Undo/Redo/SelectAll and the
menubar actions go through the platform's primary modifier -- Cmd on macOS,
Ctrl elsewhere. `framework.py` exposes ready-made `UNDO`, `REDO`, `CUT`,
`COPY`, `PASTE`, `SELECT_ALL`, `FIND`, `REPLACE`, `SAVE` and
`TOGGLE_COMMENT` that resolve per platform; build others with
`primary("k")` or `csi_u(codepoint, cmd=True)`.

`EXIT` is the exception and is Ctrl+Q everywhere: macOS terminals claim
Cmd+Q for their own quit, so the shipped macOS bindings leave Exit on Ctrl.

The raw `CTRL_*` byte constants are still there for chords that really are
Ctrl-only, but sending `CTRL_Z` on macOS matches nothing at all -- the editor
doesn't even redraw, so the test sees an empty frame rather than an obviously
wrong one.

## Sanity checks as an oracle

A binary built with `--features sanity` logs every tripped invariant to
`$TMPDIR/edit/log/sanity-YYYYMMDD.log`. Nothing in a passing test notices, so
point the suite at one and ask it to care:

```sh
cargo build --features sanity
EDIT_BIN=target/debug/edit python3 tests/pty/framework.py --strict-sanity
```

Any check tripped during a test now fails that test, naming it:

```
FAIL ( 0.62s)  oracle_probe::hits_the_hard_wrap_trip
        sanity checks tripped:
          cursor_visual_pos_drift: stored vp=Point { x: 1, y: 2 } ...
```

`--strict-sanity` is off by default: a binary built without the feature can
never trip, and a run against someone else's stale log would report their
findings as yours.

`EDIT_SANITY_PANIC=1` escalates a soft check to a panic instead, which kills
the editor mid-test. The harness recognises the resulting dead child and
reports the exit status plus the same trip list, rather than the bare
`OSError: [Errno 5]` the PTY hands it.

## Config isolation

Each run points the child at a throwaway `XDG_CONFIG_HOME`, so `edit` creates
a fresh `keybindings.yaml` from the shipped platform defaults. Without it the
tests would inherit whatever is in your `~/.config/edit`, and a stale local
config would quietly decide which chords work -- passing on your machine and
failing on CI, or worse, the other way around.

## Writing a test

```python
# tests/pty/test_example.py
from framework import FIND, Edit, SHIFT_RIGHT, expect, fixture, test


@test
def find_prefills_selection():
    with Edit([fixture("hello.txt")]) as ed:
        ed.send(SHIFT_RIGHT * 5)   # select 5 chars
        ed.send(FIND)              # open Find
        frame = ed.last_plain_frame(b"Find:")
        expect(b"Find: hello" in frame, f"got: {frame[:120]!r}")
```

`ed.send()` writes the bytes, lets `edit` settle, and drains the resulting
output -- so most tests need no explicit `pause` or `drain` calls. Override
per call: `ed.send(data, settle=0.3, drain=False)`.

Key pieces:

- `Edit(argv, cols=?, rows=?)` -- spawns the editor. Use as a context manager.
- `ed.send(bytes, settle=?, drain=?)` -- write + settle + drain.
- `ed.screen()` -- force a full repaint and return it. Reach for this whenever
  a test asserts that something is **not** on screen: the renderer only emits
  the lines that changed, so absence from a drained diff means "not redrawn",
  and a test built on it passes whenever the editor ignored the key.
- `ed.drain(timeout=?)` / `ed.plain` -- manual read + ANSI-stripped buffer.
- `ed.last_plain_frame(anchor)` -- stripped view from the last occurrence of
  `anchor` onward; use for assertions on the most recent frame.
- `ed.mark()` / `ed.plain_since(mark)` -- assert only on bytes received after
  a known point.
- `expect(cond, msg)` -- fail the test if `cond` is falsy.
- `pause(s)` -- `time.sleep(s * PACE)`; rarely needed.
- `fixture("name")` -- absolute path to a file in `fixtures/`.
- Key constants: `ESC`, `ENTER`, `BACKSPACE`, `TAB`, `F10`, arrows
  (`LEFT`/`RIGHT`/`UP`/`DOWN`), `HOME`/`END`, the `SHIFT_*` variants, and the
  platform-primary chords listed under [Chords](#chords).
- `csi_u(codepoint, shift=, alt=, ctrl=, cmd=)` / `primary("k", shift=)` --
  build a chord the constants don't cover.
- `config_home()` -- the throwaway config dir the children share.

## Output

Each test reports `PASS`/`FAIL` (colorized on TTY), its wall-clock duration,
and total runtime at the end:

```
PASS ( 0.46s)  test_checkbox_ascii::no_unicode_checkboxes_in_search_panel
...
8/8 passed in 5.99s
```

Set `NO_COLOR=1` to disable color.

## Caveats

- Timings use plain `time.sleep` under the hood. If CI is slow, bump pauses.
- `framework.py` answers only the startup probes that actually block `edit`.
  If you add new probes to the editor, extend `Edit._settle()`.
- `close()` sends Ctrl+Q then `n` to decline the "save changes" dialog;
  keybindings for Exit are assumed to be Ctrl+Q on both platforms.
