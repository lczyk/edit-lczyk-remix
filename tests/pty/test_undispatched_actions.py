"""Actions the menubar advertises must actually respond to their chord.

toggle_word_wrap, focus_statusbar and open_about all had an Action entry, a
keybindings.yaml key, a line in doc/src/keybindings.md and a menu item that
displayed their configured chord -- but no code path ever consumed that chord.
Rebinding them, or using the shipped default, did nothing. Word wrap toggled
only via a hardcoded Alt+Z inside the textarea, which no config could move.
"""

import contextlib
import os

from framework import ESC, Edit, config_home, csi_u, expect, fixture, test


def _config_path() -> str:
    return os.path.join(config_home(), "edit", "keybindings.yaml")


@contextlib.contextmanager
def bindings(**overrides):
    """Overlay `overrides` on the config, restoring it afterwards."""
    path = _config_path()
    os.makedirs(os.path.dirname(path), exist_ok=True)
    previous = None
    if os.path.exists(path):
        with open(path, "rb") as f:
            previous = f.read()
    body = "keybindings:\n" + "".join(f'  {k}: "{v}"\n' for k, v in overrides.items())
    with open(path, "w", encoding="utf-8") as f:
        f.write(body)
    try:
        yield
    finally:
        if previous is None:
            os.unlink(path)
        else:
            with open(path, "wb") as f:
                f.write(previous)


# Chords nothing else binds, so a pass cannot come from another handler.
CTRL_G = csi_u(ord("g"), ctrl=True)
CTRL_B = csi_u(ord("b"), ctrl=True)


# A wrapped line's continuation rows carry this in the gutter instead of a line
# number, so its presence is a direct read of whether wrap is on.
WRAP_MARKER = "\u2219".encode()


@test
def a_rebound_word_wrap_chord_toggles_wrap():
    with bindings(toggle_word_wrap="Ctrl+G"):
        with Edit([fixture("longword.txt")], cols=40, rows=12) as ed:
            expect(WRAP_MARKER in ed.screen(), "wrap should be on by default")
            ed.send(CTRL_G)
            expect(WRAP_MARKER not in ed.screen(), "Ctrl+G did not turn wrap off")
            ed.send(CTRL_G)
            expect(WRAP_MARKER in ed.screen(), "Ctrl+G did not turn wrap back on")


@test
def a_rebound_about_chord_opens_the_dialog():
    with bindings(open_about="Ctrl+B"):
        with Edit([fixture("hello.txt")], cols=60, rows=14) as ed:
            # The dialog is titled lowercase and shows a "version:" line.
            expect(b"version:" not in ed.screen(), "the dialog was already open")
            ed.send(CTRL_B)
            expect(b"version:" in ed.screen(), "Ctrl+B did not open the about dialog")
            ed.send(ESC)


@test
def the_default_alt_z_still_toggles_wrap():
    # The textarea's own Alt+Z stays as an alias, so the shipped default keeps
    # working for anyone who never touches the config.
    alt_z = csi_u(ord("z"), alt=True)
    with Edit([fixture("longword.txt")], cols=40, rows=12) as ed:
        expect(WRAP_MARKER in ed.screen(), "wrap should be on by default")
        ed.send(alt_z)
        expect(WRAP_MARKER not in ed.screen(), "Alt+Z stopped toggling wrap")
