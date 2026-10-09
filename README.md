# edit (lczyk remix)

A simple editor for simple needs -- forked, then bent to fit one pair of hands.

This is a divergent fork of [microsoft/edit](https://github.com/microsoft/edit).

It shares the ancestry and the MIT licence (see [LICENSE](LICENSE)) but is no
longer the same program: Windows support, localization, packaging, benchmarks
and the multi-document surface are gone, while syntax highlighting, a diff
gutter, a minimap and a second `bat`-like persona have arrived. There is no
intent to contribute back upstream, and nothing here is published to crates.io
or to any distro.

## What's in the box

- **`edit FILE`** -- a single-file, single-buffer terminal editor with
  VS Code-ish input controls, undo/redo, find/replace, a git diff gutter, a
  minimap, and merge-conflict markers highlighted in any file.
- **`eat [FILES...]`** -- a `bat`-flavoured syntax-highlighting `cat`. Same
  binary, busybox-style: `make install` drops an `eat -> edit` symlink, and
  `edit --eat` works too. Handles paging, line ranges, line numbers, wrapping,
  a command's output in place of a file (`eat -x -- git diff`) and a live
  view of either (`-w`). See [doc/src/eat.md](doc/src/eat.md).
- **`lsh`** -- the in-tree syntax-highlighting compiler and runtime, with a few
  dozen language definitions in [crates/lsh/definitions/](crates/lsh/definitions/).
  `lsh-bin` is the CLI for debugging them.

Notable differences from upstream, beyond the removals:

- **Linux and macOS only.** No `#[cfg(windows)]`, no drive pickers.
- **One file per invocation.** No `New`/`Open`/`Close`, no file picker tree.
  `Save As` renames in place.
- **English only.** No `i18n/`, no `loc()` indirection.
- **`Cmd` is a first-class modifier** via the kitty keyboard protocol, so macOS
  chords feel native. See [doc/src/terminal-keyboard.md](doc/src/terminal-keyboard.md).
- **Configurable keybindings** in `<config_dir>/keybindings.toml`. See
  [doc/src/keybindings.md](doc/src/keybindings.md).
- **`--quirks=` / `EDIT_QUIRKS=`** behaviour toggles rather than scattered
  flags. See [doc/src/quirks.md](doc/src/quirks.md).

## Build

Needs a stable Rust toolchain (MSRV `1.90`, edition 2024). Everything goes
through the [Makefile](Makefile) -- `make help` lists the targets.

```sh
make build     # release build into target/release/edit
make install   # debug build + `eat` symlink into ~/.cargo/bin
make verify    # fmt-check + clippy + test (the pre-commit gate)
```

Release builds are compressed with `upx` when it happens to be installed; the
step is skipped otherwise.

### PTY tests

`make verify` covers the Rust suite. The end-to-end tests drive the built
binary through a pseudo-terminal and run separately (CI runs both):

```sh
make build
python3 tests/pty/framework.py
```

See [tests/pty/README.md](tests/pty/README.md) for filters, `--watch` mode,
and how to write new ones.

### Documentation

The knowledge base under [doc/](doc/) is an mdBook (`cargo install mdbook`):

```sh
make docs-serve   # live reload at http://localhost:3000
make docs-build   # one-shot HTML into doc/book/
```

Design notes and proposals live in [meanderings/](meanderings/); run
[meanderings/index.sh](meanderings/index.sh) for a status-grouped index. Repo
conventions, for humans and agents alike, live in [AGENTS.md](AGENTS.md).

## ICU library configuration

Search and Replace optionally use the ICU library, loaded via `dlopen` at
runtime. When it can't be loaded, both degrade gracefully.

By default, the following library names are tried:

 Variable | macOS | Linux / Other
----------|-------|---------------
`EDIT_CFG_ICUUC_SONAME` | `libicucore.dylib` | `libicuuc.so`
`EDIT_CFG_ICUI18N_SONAME` | `libicucore.dylib` | `libicui18n.so`

The unversioned `libicuuc.so` is a symlink that ships in the development
package, not the runtime one. On a machine with only `libicuuc.so.76`,
either install that package (`sudo apt install libicu-dev`, or your
distribution's equivalent) or point at the versioned library directly:

```sh
EDIT_CFG_ICUUC_SONAME=libicuuc.so.76 EDIT_CFG_ICUI18N_SONAME=libicui18n.so.76 make build
```

Setting the SONAME does not disable renaming auto-detection, so on Linux
that is normally the only thing you need to set.

This project assumes that ICU exports symbols without `_` prefix and without
version suffix, such as `u_errorName`. If your installation uses versioned
exports, set:

* `EDIT_CFG_ICU_CPP_EXPORTS=true` -- look for C++ symbols such as `_u_errorName`. Enabled by default on macOS.
* `EDIT_CFG_ICU_RENAMING_VERSION=76` -- look for symbols such as `u_errorName_76`.
* `EDIT_CFG_ICU_RENAMING_AUTO_DETECT=true` -- detect the version at runtime. Enabled by default on Linux unless `EDIT_CFG_ICU_RENAMING_VERSION` is set.

To check that ICU is actually wired up, `make test-icu` finds an installed ICU,
builds against it, and fails rather than skipping if the search tests can't run.
