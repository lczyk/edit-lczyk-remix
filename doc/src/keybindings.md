# Keybindings

User-editable config file controls a subset of `edit`'s shortcuts. The rest (dialog-internal keys: Return / Escape / Arrows / Backspace) are hardcoded.

## Config file location

`<config_dir>/keybindings.toml`. Auto-created on first run from a platform-specific embedded default.

- `$XDG_CONFIG_HOME/edit/keybindings.toml`
- Fallback: `~/.config/edit/keybindings.toml`

Same path on Linux and macOS. Embedded defaults live at `crates/edit/src/bin/edit/keybindings.macos.toml` and `crates/edit/src/bin/edit/keybindings.linux.toml` respectively.

## What's bindable

The `Action` enum in `crates/edit/src/bin/edit/keybindings.rs` lists every bindable command. Roughly:

- Menubar items: `exit`, `save`, `undo`, `redo`, `cut`, `copy`, `paste`, `select_all`, `find`, `replace`, `go_to_line`, `toggle_word_wrap`, `open_about`, `focus_menubar`, `focus_statusbar`.
- Editor commands: `move_line_up` / `_down`, `delete_line`, `toggle_line_comment`, `delete_to_line_start` / `_end`.
- Cursor motion (mostly relevant on macOS where Cmd is the natural modifier): `small_jump_up` / `_down` (+ `_select` variants), `line_start` / `_end` (+ `_select`).

Anything not in the `Action` enum is hardcoded -- dialog-internal keys (Return, Escape, arrows, Backspace) and the textarea's own navigation and selection map.

`undo`, `redo`, `cut`, `copy`, `paste` and `select_all` are a special case worth knowing about. They are handled inside the textarea widget rather than by the global shortcut dispatch, so that they work in a modal's input field as well as in the document -- and so a rebind applies in both. The editor hands the widget your configured chords at startup; when nothing configures them (the `eat` viewer, say) they fall back to a `KBMOD_PRIMARY` constant that resolves to `Cmd` on macOS and `Ctrl` elsewhere.

Two consequences:

- Rebinding one of the six replaces the platform default rather than adding to it. `undo = "Ctrl+U"` on macOS means `Cmd+Z` no longer undoes.
- `Cmd`/`Ctrl+Shift+Z` stays wired to redo whatever you bind, since the table has one `redo` entry and this is the other spelling people reach for.

## Chord syntax

```toml
undo            = "Cmd+Z"
redo            = "Cmd+Shift+Z"
focus_menubar   = "F10"
select_all      = "Cmd+A"
focus_statusbar = ""           # unbound
exit            = ["Ctrl+Q", "Ctrl+W"]
```

- **Modifier names:** `Ctrl`, `Alt`, `Shift`, `Cmd` (alias `Super`).
- **Key names:** letter `A`-`Z`, digit `0`-`9`, `Up`/`Down`/`Left`/`Right`, `Home`/`End`/`PageUp`/`PageDown`, `Insert`/`Delete`, `Tab`/`Back`/`Return`/`Escape`/`Space`, `F1`..`F24`, `Numpad0`..`Numpad9`.
- **Empty string** (`""`) leaves an action unbound. This is the linux default for several macOS-only chords (e.g. `delete_to_line_start`, `line_start`).
- **A list binds several chords** to one action; any of them triggers it, and the menubar shows the first. `[]` leaves the action unbound, same as `""`. The six textarea actions above and `focus_menubar` only honour the first chord in a list.

## Cmd modifier on macOS

`Cmd` is recognised via the [kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/). `edit` pushes flag 1 (`CSI > 1 u`) on startup so terminals that support the protocol encode `Cmd+anything` as a CSI-u sequence the parser can decode.

The chord still has to **reach** `edit` -- terminals frequently claim popular Cmd chords for native actions (copy, find, ...) before forwarding. See [Terminal Keyboard](./terminal-keyboard.md) for the diagnosis pattern and [Alacritty](./alacritty.md) for a worked config.

## Reloading

`edit` reads `keybindings.toml` once at startup. To pick up changes, restart the editor.
