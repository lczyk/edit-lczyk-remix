# Overview

Knowledge base for the `edit` terminal editor -- a divergent fork of [microsoft/edit](https://github.com/microsoft/edit), living at [lczyk/edit-lczyk-remix](https://github.com/lczyk/edit-lczyk-remix).

This book collects notes that don't belong in the source tree itself: terminal interop quirks, per-terminal configuration, design notes that grew past a single comment. Source-of-truth for code conventions still lives in [`AGENTS.md`](https://github.com/lczyk/edit-lczyk-remix/blob/lczyk-remix/AGENTS.md) at the repo root, and the [`README.md`](https://github.com/lczyk/edit-lczyk-remix/blob/lczyk-remix/README.md) covers what the fork has added and dropped.

## Sections

- [eat](./eat.md) -- the `bat`-like `cat` persona: output paths, options, paging, exec and live modes, viewer keys.
- [Quirks](./quirks.md) -- `--quirks=...` / `EDIT_QUIRKS=...` behaviour toggles. Positive canonical names (`safe-filenames`, `unicode`, `color`, `animations`, `create`); `NAME` enables, `-NAME` disables.
- [Keybindings](./keybindings.md) -- config file format, what's bindable, chord syntax, the Cmd modifier on macOS.
- [Terminal Keyboard](./terminal-keyboard.md) -- why some chords (`Cmd+C`, `Option+Backspace`, ...) don't reach `edit` out of the box, and how to fix it per terminal.
  - [Alacritty](./alacritty.md) -- concrete config snippets for Alacritty on macOS.
- [Dev Input Log](./dev-input-log.md) -- `--logfile` JSONL stream for the "I pressed X, expected Y" feedback loop.

## Build

```sh
make docs-serve   # live-reload at http://localhost:3000
make docs-build   # one-shot HTML into doc/book/
```

`mdbook` must be on `$PATH` (`cargo install mdbook`).
