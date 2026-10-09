
# AGENTS.md

Project-level guidance for AI coding assistants working on this codebase.

User-facing reference (terminal interop, keybinding syntax, debug-log format) lives in the knowledge base under [`doc/`](doc/) -- build with `make docs-serve`. This file stays focused on what the agent needs to know to act correctly in the repo.

## Context

Divergent fork of Microsoft's `edit` terminal editor, trimmed down for personal use and then grown in other directions (syntax highlighting, diff gutter, minimap, the `eat` persona). Lives at [lczyk/edit-lczyk-remix](https://github.com/lczyk/edit-lczyk-remix); default branch is `lczyk-remix`, not `main`. Not published, not packaged, no upstream contributions. Ignore anything on the internet that frames this as a Microsoft/MSDOS product -- upstream's docs describe a different program by now. [README.md](README.md) summarises what the fork added and dropped.

## Scope and platform

- **Targets:** Linux and macOS. Windows support has been removed -- do not reintroduce `#[cfg(windows)]`, `windows-sys`, `winresource`, drive pickers, `\\` path handling, or `EDIT_CFG_*` Windows SONAMEs.
- **Single-file, single-buffer.** Invoke as `edit FILE`. No path -> print help and exit. More than one path -> error. No `New`/`Open`/`Close` menu items, no file picker tree, no fuzzy cross-document navigation, no stdin-redirect scratch buffer. `Save As` exists only as a plain path-input modal (renames in place).
- **Language:** English only. The `i18n/` directory, `localization` module, `LocId` enum, and `loc()` function have all been deleted. Use plain string literals. Do not add `gettext`-style indirection.
- **No crates.io publish, no distro packaging.** No `categories`, no `repository` URL, no package-maintainer notes, no install scripts, no snap/desktop files.
- **No benchmarks, no fuzzing in-tree.** The `benches/`, `fuzz/`, and `editing-traces/` dirs are gone. Don't add `criterion`, `libfuzzer-sys`, or similar.

## Build and test

Use the [Makefile](Makefile) -- do not invoke `cargo` directly in routine work. Run `make help` to list targets. Common ones:

- `make build` -- release build. `make install` -- debug build with the `sanity` feature plus the `eat -> edit` symlink.
- `make check` / `make clippy` / `make test` -- individual checks, across the whole workspace. All three share the `FEATURES` variable: every feature except stdext's `single-threaded`, which the threaded test harness cannot survive. Don't reach for `--all-features`.
- `make format` / `make fmt-check` -- format the workspace / verify formatting.
- `make verify` -- full pre-commit gate (fmt-check + clippy + test). Run this before reporting a task as done.
- `make test-icu` -- test suite with ICU wired up; fails rather than skipping the search tests.
- `make cover` / `make cover-open` -- coverage via `cargo-llvm-cov`.
- `make docs-serve` / `make docs-build` -- knowledge base under [`doc/`](doc/) (mdBook).
- `make spec-check` -- model-check the FizzBee specs under [`doc/spec/`](doc/spec/) (needs docker).

`make verify` does **not** run the PTY tests -- those drive the built binary through a pseudo-terminal and need Python plus a fresh `make build`:

```sh
make build && python3 tests/pty/framework.py
```

CI runs them twice, the second time against a `--features sanity` build with `--strict-sanity`, which fails a test that tripped an invariant check. Run them yourself when a change touches rendering, input handling, or modal flow; all of them should pass. Three things to know before writing one:

- Send `UNDO`, not `CTRL_Z`. Chords go through the platform primary modifier, and a raw Ctrl byte matches nothing on macOS -- the editor doesn't even redraw, so an assertion on absence passes for the wrong reason.
- Assert on `ed.screen()`, not `ed.plain`, when a test cares that something is *not* on screen. The renderer only emits changed lines, so a drained diff is not a screen.
- Each run gets a throwaway `XDG_CONFIG_HOME`, so the shipped keybinding defaults apply rather than your own `~/.config/edit`.

See [tests/pty/README.md](tests/pty/README.md).

ICU is loaded via `dlopen` at runtime. If missing, Search/Replace degrades gracefully and the search tests skip. `make test-icu` builds against an installed ICU and makes that skip a failure, so a run meant to cover search can't pass having covered nothing. See [README.md](README.md) for `EDIT_CFG_ICU*` env vars -- note the default SONAME is the unversioned one, which only exists with the distro's ICU *development* package.

## Sanity checks

Implementation in [crates/stdext/src/sanity.rs](crates/stdext/src/sanity.rs); call sites are scattered through the buffer, measurement, render and highlighting layers. Two macros, both at crate root:

- `sanity_check!(name, cond, "fmt", args...)` -- soft. On failure logs a line to `$TMPDIR/edit/log/sanity-YYYYMMDD.log`, flashes a statusbar warning, and carries on. Deduplicated per call site over a 1s window.
- `sanity_assert!(name, cond, ...)` -- hard. Same logging, then panics with the logfile path in the message. Without the `sanity` feature it degrades to a plain `debug_assert!`.

Both compile to nothing without the feature, so release builds carry zero cost. `EDIT_SANITY_PANIC=1` promotes a soft trip to a panic, which is how you bisect one.

A sanity check is not a unit test. It is a **standing hypothesis about a property that must hold at a chokepoint**, evaluated against every input the program ever sees -- the fuzzing counterpart to the test suite's fixed cases. That shapes how they are written:

- One property per check, named as the hypothesis it asserts (`cursor_visual_pos_drift`, `gutter_marks_cover_the_buffer`), not as the bug that motivated it.
- Put it where the property must hold -- the setter, the publish point, the boundary the value crosses -- not at the site that happened to break it once. `set_cursor_internal` in [crates/edit/src/buffer/mod.rs](crates/edit/src/buffer/mod.rs) is the model: every cursor in the program passes through it, so a check there covers every path that could produce a bad one.
- The message carries the operands, not the diagnosis. Whoever reads the log wants the numbers.
- A check that re-derives a value must re-derive it by a *different* route than the code under test. A remeasure that calls back into the same function it is checking agrees with the bug and reports nothing.

Unit-test the check itself with `stdext::sanity::capture::trips`, which returns the trips a closure produced. End to end, the PTY suite has `--strict-sanity`, which fails any test that tripped a check:

```sh
cargo build --features sanity
EDIT_BIN=target/debug/edit python3 tests/pty/framework.py --strict-sanity
```

## The `tdd+sanity` flow

Default working style for **non-fatal behaviour bugs whose misbehaviour can be stated as an invariant** -- the wrong thing renders, the cursor lands somewhere it shouldn't, a mark attaches to the wrong row. Such a report is two defects, and they get fixed in this order:

1. **A gap in the sanity system.** The editor did something it must never do and nothing noticed.
2. **The editor bug itself.**

So: write (or repair) the sanity check first, and prove it trips on the *unfixed* build -- via `capture::trips` in a unit test, or a PTY test under `--strict-sanity`. Only then fix the editor, and watch the same check go quiet.

The ordering is the whole point. Sanity coverage is close to untestable after the fact: patch the editor first and the check never fires, so you cannot tell a check that would have caught the bug from one that is blind to it. A check written against a green build is a guess. Both defects want their own commit -- the check, then the fix.

Phrase the hypothesis in terms of observable behaviour rather than the mechanism you are about to change. "The cursor stays on the same visual row when Left is pressed at the end of a line" outlives whichever measurement path currently gets it wrong; "goto_line_start returns the right offset" does not.

Not every bug fits. Skip straight to the fix for panics and crashes (a plain test is the right oracle), for one-off logic errors with no invariant behind them, and for anything where the check would only restate the implementation line-for-line.

## Cmd modifier and terminal interop

`kbmod::CMD` exists alongside `CTRL`/`ALT`/`SHIFT` and maps to Super in the [kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/). The editor pushes flag 1 on startup (`CSI > 1 u` in `edit::term::setup`) and pops on exit (`CSI < u` from `edit::term::RestoreModes`). Word-nav-on-backspace/delete uses `KBMOD_FOR_WORD_NAV` (Alt on macOS, Ctrl elsewhere).

The textarea standard chords (Cut/Copy/Paste/Undo/Redo/SelectAll) are handled by the widget, not the global dispatch, so they work in modal input fields too. `bin/edit/main.rs` passes the configured chords down via `tui::set_textarea_chords`; absent that they default to `KBMOD_PRIMARY` (Cmd on macOS, Ctrl elsewhere). Don't move them into `handle_global_shortcuts` -- that runs before every other consumer and would take Cmd+C away from the search field.

User-facing detail (terminal compat, alacritty `option_as_alt`, `Cmd+C` swallowing, etc.) lives in the knowledge base: [doc/src/terminal-keyboard.md](doc/src/terminal-keyboard.md) and [doc/src/alacritty.md](doc/src/alacritty.md). When a "this chord doesn't work" report lands, point there before changing code.

## Keybindings

Config: `<config_dir>/keybindings.toml`, auto-created on first run from [keybindings.macos.toml](crates/edit/src/bin/edit/keybindings.macos.toml) / [keybindings.linux.toml](crates/edit/src/bin/edit/keybindings.linux.toml). The `Action` enum in [crates/edit/src/bin/edit/keybindings.rs](crates/edit/src/bin/edit/keybindings.rs) lists what's bindable -- menubar items and a few editor commands. Dialog-internal keys (Return/Escape/Arrows/Backspace) stay hardcoded.

Full chord syntax + bindable-action reference: [doc/src/keybindings.md](doc/src/keybindings.md).

## Dev input log (`--logfile`)

Debug builds accept `--logfile=PATH` and append a JSONL stream of `Input` events + post-frame `TextBuffer` snapshots. Implementation in [crates/edit/src/bin/edit/devlog.rs](crates/edit/src/bin/edit/devlog.rs), gated by `#[cfg(debug_assertions)]`. When the user pastes excerpts, treat `cursor`, `selection`, `dirty`, `lines` as ground truth for what actually happened.

Record shape + field reference: [doc/src/dev-input-log.md](doc/src/dev-input-log.md).

## Architecture

- **Text buffer ([crates/edit/src/buffer/](crates/edit/src/buffer/))** does not track line breaks. Only the current cursor position is kept; navigation seeks `O(n)` through the document. Every other perf decision flows from this:
  - [crates/edit/src/simd/](crates/edit/src/simd/) -- `memchr2` line-break scanners (>100 GB/s).
  - [crates/edit/src/unicode/](crates/edit/src/unicode/) -- `Utf8Chars` iterator (4 GB/s, transparently inserts U+FFFD) and `MeasurementConfig` grapheme/width measurement (600 MB/s).
  - Without word-wrap, `memchr2` drives all line navigation -- 1 GB files feel like 1 MB.
- **Rendering is two passes.** [crates/edit/src/buffer/render.rs](crates/edit/src/buffer/render.rs) walks the visible rows and builds the owned per-row IR in [buffer/layout.rs](crates/edit/src/buffer/layout.rs); [crates/edit/src/paint/](crates/edit/src/paint/) consumes it and writes into the framebuffer. `buffer` owns the IR because it produces it -- `paint` depends downward on `buffer`, never the reverse. Inside `paint/`, only `anim.rs` is animation; `draw.rs` and `physics.rs` are time-free by contract (see their module docs).
- **[crates/edit/src/framebuffer.rs](crates/edit/src/framebuffer.rs)** -- video-game-style framebuffer. UI draws into a buffer; diff against the previous frame is sent to the terminal.
- **[crates/edit/src/tui/](crates/edit/src/tui/)** -- immediate-mode UI. Read `mod.rs`'s module doc. `node.rs` is the arena tree + box layout, `textarea.rs` the biggest widget (and the only one with its own input handling).
- **[crates/edit/src/input/](crates/edit/src/input/)** -- `keys.rs` is the wire-format-free vocabulary (`InputKey`, `vk`, `kbmod`); `vt_decode.rs` turns [vt.rs](crates/edit/src/vt.rs)'s tokens into it. A non-VT backend would be a sibling of the decoder, not a change to the vocabulary.
- **[crates/edit/src/vt.rs](crates/edit/src/vt.rs)** -- VT parser.
- **[crates/edit/src/sys/](crates/edit/src/sys/)** -- platform abstractions (unix only): terminal i/o (raw mode, sigwinch resize injection, polling stdin reader, `write_stdout`) plus the fs + ICU helpers. Absorbed the former `tty` crate; don't reintroduce it.
- **[crates/edit/src/term.rs](crates/edit/src/term.rs)** -- alt-screen mode switch, OSC 4/10/11 palette probe, ambiguous-width probe, kitty kbd proto push; `RestoreModes` is the inverse-on-drop guard. Used by `bin/edit/main.rs` and by `edit::mount`.
- **[crates/edit/src/mount.rs](crates/edit/src/mount.rs)** -- thin external mount api for the tui: `mount(opts, draw_fn)` owns `Tui::new` + `term::setup` + the input/render loop + alt-screen restore. Used by `eat`'s alt-screen views; `bin/edit/main.rs` keeps its own loop (its module doc says why). `flush_clipboard_to_host` is shared by both.
- **[crates/edit/src/watch.rs](crates/edit/src/watch.rs)** -- poll-based file-change detection: a `FileStat` (size, inode, mtime) compared for equality on each tick, by the editor's modified-on-disk flag and eat's viewer poll.
- **[crates/edit/src/eat/](crates/edit/src/eat/)** -- the `eat` persona: `cli.rs` args, `detect.rs` language resolution, `stream.rs` the non-tty ansi pipeline, `exec.rs` the `-x` command runner, `views.rs` the alt-screen view over `mount` (static, or live with `-w`), `viewer.rs` its keymap + terminal session. The view's reload states and the argv rules are modelled in [doc/spec/](doc/spec/); `make spec-check` runs the model checker. Reachable via argv0 dispatch in `bin/edit/main.rs` (`name == "eat"` or `--eat`); the `eat` binary is a `make install`-time symlink to `edit`, not a separate cargo target.
- **[crates/edit/src/langlist.rs](crates/edit/src/langlist.rs)** -- the `-L` listing, shared by both personas rather than living under `eat`.
- **[crates/edit/src/bin/edit/](crates/edit/src/bin/edit/)** -- the binary. ~90% UI and business logic. `cli.rs` is the argv surface, `modals.rs` the global dialogs (one `Option<Modal>`, so two can't paint at once).

Terminal issues: check `vt.rs`, `sys/unix.rs`, and `edit::term::setup` first.

## Crates

- `edit` -- main binary and library. Includes `edit::eat` (busybox-style multicall: when invoked as `eat` via symlink, or with `--eat`, acts as a `bat`-like syntax-highlighting cat). User-facing surface documented in [doc/src/eat.md](doc/src/eat.md).
- `lsh` -- syntax-highlighting compiler and runtime. Language definitions in [crates/lsh/definitions/](crates/lsh/definitions/); `plain.lsh` is the empty fallback. `lsh::conflict` recognises merge-conflict markers ahead of the bytecode and forks the vm state per side; `lsh::kind` pins the builtin kinds the runtime emits itself. See [crates/lsh/README.md](crates/lsh/README.md).
- `lsh-bin` -- CLI for debugging LSH output.
- `lsh-defs` -- bundled lsh language defs codegen + detection helpers + ansi-16 colourmap. Shared by `edit` and `edit::eat`. `detect::resolve` is the one detection chain (user associations, globs, shebang, content sniff, `PLAIN`); a buffer always has a language. The canonical highlight-kind colour table is `lsh::compiler::default_ansi16`; consumers map it via `Ansi16::sgr()` rather than transcribing it.
- `gutter` -- per-line gutter mark computation + render (git-diff overlays). `GutterMark::Conflict` is the exception: it comes from the highlighter's per-line conflict tag, not the diff, and outranks it.
- `stdext` -- shared utilities (arena allocator, collections, SIMD helpers, sys shims).
- `unicode-gen` -- codegen for Unicode LUTs (only needed to regenerate tables; tables are checked in).

## Meanderings

Design notes, proposals, and comparisons live in [meanderings/](meanderings/) as `*.prop.md` files with YAML frontmatter (`status`, `date`, `description`). Run [meanderings/index.sh](meanderings/index.sh) for a status-grouped index. See [meanderings/README.md](meanderings/README.md) for the format. Files prefixed `_` are no longer active (implemented or shelved).

## Code conventions

- **Binary size matters.** Don't introduce dependencies lightly. Check whether stdlib or existing helpers already cover the use case.
- **[rustfmt.toml](rustfmt.toml):** stable rustfmt only -- `style_edition = "2024"`, `use_small_heuristics = "Max"`, `newline_style = "Unix"`, `use_field_init_shorthand = true`. Run `cargo fmt` before committing.
- **Clippy:** `--deny warnings` is the CI bar.
- **No comments explaining what well-named code already says.** Only comment hidden constraints, workarounds, or subtle invariants.
- **Rust edition:** 2024, MSRV `1.90` (see `rust-version` in [Cargo.toml](Cargo.toml)).

## Things to avoid

- Reintroducing Windows support, localization, or packaging surface.
- Adding features, dependencies, or abstractions beyond what the task requires.
- Mocking in tests where the real thing is cheap.
- Committing without running `make verify`.
