"""What keybindings.yaml says about the six textarea chords is what happens.

Regression: cut/copy/paste/undo/redo/select_all were hardcoded to the
platform's primary modifier inside the textarea widget, and no code path
consulted the config for them. Rebinding one relabelled the menubar and
changed nothing else.
"""

import contextlib
import os

from framework import END, UNDO, Edit, config_home, csi_u, expect, fixture, test

# Chords nothing binds by default, so a pass cannot come from the built-ins.
CTRL_U = csi_u(ord("u"), ctrl=True)
CTRL_J = csi_u(ord("j"), ctrl=True)


def _config_path() -> str:
    return os.path.join(config_home(), "edit", "keybindings.yaml")


@contextlib.contextmanager
def bindings(**overrides):
    """Overlay `overrides` on the config for the duration of the block.

    `merge_file` applies the file on top of the shipped defaults, so a mapping
    holding only these entries leaves every other action alone. The previous
    contents are restored on the way out -- the config dir is shared by the
    whole run.
    """
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


@test
def a_rebound_undo_and_redo_are_honoured():
    with bindings(undo="Ctrl+U", redo="Ctrl+J"):
        with Edit([fixture("hello.txt")]) as ed:
            ed.send(END)
            ed.send(b"ZZZ")
            expect(b"helloZZZ" in ed.screen(), "insert not visible before undo")

            ed.send(CTRL_U)
            after_undo = ed.screen()
            expect(b"hello world" in after_undo, f"line missing after undo: {after_undo[:120]!r}")
            expect(b"helloZZZ" not in after_undo, "Ctrl+U did not undo")

            ed.send(CTRL_J)
            expect(b"helloZZZ" in ed.screen(), "Ctrl+J did not redo")


@test
def the_platform_default_stops_working_once_rebound():
    # The other half of the contract: the built-in chord must give way, or
    # "rebinding" would just be "adding".
    with bindings(undo="Ctrl+U"):
        with Edit([fixture("hello.txt")]) as ed:
            ed.send(END)
            ed.send(b"ZZZ")
            ed.send(UNDO)
            expect(b"helloZZZ" in ed.screen(), "the old chord still undid the edit")


@test
def an_empty_binding_leaves_the_action_unbound():
    with bindings(undo=""):
        with Edit([fixture("hello.txt")]) as ed:
            ed.send(END)
            ed.send(b"ZZZ")
            ed.send(UNDO)
            ed.send(CTRL_U)
            expect(b"helloZZZ" in ed.screen(), "an unbound undo still undid the edit")
