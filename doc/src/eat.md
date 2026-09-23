# eat

`eat` is `edit`'s second persona: a `bat`-flavoured syntax-highlighting `cat`.
It is the same binary, busybox-style. Two ways in:

- `eat FILE` -- via the `eat -> edit` symlink that `make install` creates.
- `edit --eat FILE` -- the flag form, for when the symlink isn't around.

There is no separate `eat` build target. Highlighting comes from the same `lsh`
definitions the editor uses, so a language that highlights in one highlights in
the other.

## Output paths

`eat` picks one of three renderers, and the choice is mostly about whether
stdout is a terminal:

| stdout | invocation | renderer |
|---|---|---|
| not a tty | anything | ansi stream -- highlight, write through. Pipes, redirects and the pager all land here. |
| tty | single file (not a directory), no `--plain`, no `--line-range` | alt-screen snapshot viewer |
| tty | `-f` / `--follow` | alt-screen follow viewer over a growing file |

`--plain` (`-p`) drops highlighting, decorations and paging -- it makes `eat`
behave like plain `cat`.

With no arguments and a tty on stdin, `eat` prints short help and exits rather
than blocking on stdin. With piped stdin and no arguments, it reads stdin. Use
`-` to name stdin explicitly.

## Options

```
-l, --language <LANG>   override syntax detection (needed for stdin without a shebang)
-p, --plain             no highlighting, decorations, or paging
-n, --number            show line numbers
    --line-range <R>    N | N: | :M | N:M
    --color <WHEN>      auto (default) | always | never
    --paging <WHEN>     auto (default) | always | never
    --wrap <WHEN>       auto (default) | always | never -- never chops instead
-f, --follow [<DUR>]    follow appends, like `tail -F`
-L, --list-languages    print known languages (pretty | plain | json)
    --version
```

Language detection order: `-l` override, then path/filename globs (a glob
several definitions share is settled by their content detectors), then a
shebang sniff of the first line, then a content sniff, then Plain Text. The
editor uses the same chain.

### Merge conflicts

Conflict-marker lines (`<<<<<<<`, `|||||||`, `=======`, `>>>>>>>`) are
recognised in every file, Plain Text included, and drawn in magenta; each
side of the conflict is highlighted as if it were the only one. With `-n`,
every line of the block carries a magenta separator (`!` without colour)
in place of the diff mark. The editor does the same in its margin.

### Colour

Precedence, highest first:

1. `--color always` / `--color never`.
2. `FORCE_COLOR` -- any non-empty value other than `0` forces colour on.
3. `NO_COLOR` -- any non-empty value forces colour off, per <https://no-color.org>.
4. Whether stdout is a tty.

The editor itself only consults `NO_COLOR`, not `FORCE_COLOR`.

### Paging

`--paging=auto` pages when stdout is a tty and a pager can be found. The pager
is resolved as `EAT_PAGER`, then `PAGER`, then a `$PATH` walk for `less`. When
the pager is `less`, `eat` passes `-R` (raw ansi through) and `-F` (quit if the
content fits one screen), plus a chop flag when `--wrap=never`.

Paging forces colour on: through the pipe to the pager, the tty check would
otherwise read false and strip it.

### Follow mode

`-f` polls the file for appends. The optional value sets the interval and
accepts `500ms`, `30s`, `1m`, `1.5s`, or a bare number meaning seconds. Bare
`-f` defaults to 250 ms, or to `EAT_FOLLOW_INTERVAL_MS` when that is set.
Intervals below 50 ms are clamped.

Follow mode is deliberately narrow -- these combinations error with exit code
2 rather than half-working:

- no path, or `-` (stdin is not supported)
- more than one path
- `--line-range`
- `--plain`

`--paging` is ignored while following.

## Viewer keys

Both tty viewers share one keymap. vi aliases mirror `less`.

| keys | action |
|---|---|
| `q`, `Escape` | quit |
| `j` / `k`, `Down` / `Up` | scroll a line |
| `PageDown` / `PageUp` | scroll a page |
| `h` / `l`, `Left` / `Right` | scroll horizontally |
| `g`, `Home` | jump to top |
| `G`, `End` | jump to bottom |
| `w` | toggle word wrap |
| `r` | reload from disk (snapshot view only) |
| `Cmd+C` / `Ctrl+C` | copy the selection |
| `Cmd+A` / `Ctrl+A` | select all |

The primary modifier is `Cmd` on macOS and `Ctrl` elsewhere, same as the
editor -- see [Terminal Keyboard](./terminal-keyboard.md) if a chord doesn't
reach the program.

## Directories

A directory argument is listed where a file's contents would go -- always via
the ansi stream, so on a tty a lone `eat DIR` goes to the pager rather than
the viewer:

```
   - -M src/
   - -I target/
   3 -- .gitignore
1.2k -M Cargo.toml
 340 -N new.rs
   - -- link -> elsewhere
```

- Directories first, then files, each sorted case-insensitively. Dotfiles
  are listed; `.` and `..` are not. A symlink to a directory groups with the
  directories and shows its target.
- Size in decimal units, like `eza`, at most four characters wide.
  Directories and symlinks show `-`.
- The git column is `eza --git`'s: staged then unstaged, `N` new, `M`
  modified, `D` deleted, `R` renamed, `T` type change, `U` conflicted, `I`
  ignored, `-` unchanged. Every unmerged pair (`AA`, `DD`, `UU`, ...) shows
  as `UU`. A directory shows the most notable status inside it
  (ignored files inside it don't count). Outside a repo, or with no `git` on
  `$PATH`, the column is left out.
- File names are coloured by the language their name matches -- globs only,
  nothing is opened -- hashed onto the six ansi-16 hues, so a language keeps
  its colour. Names that match no language stay uncoloured.
- Control characters in names are escaped as rust escapes (`\u{1b}`), like
  `eza`, so a filename cannot send escape sequences to the terminal.

`--plain` lists names only, with a trailing `/` on directories. `-n` and
`--line-range` apply to the rows as they would to lines.

## Multi-file output

With a tty and two or more files, each is preceded by a `--- <path> ---`
header, bold when colour is on. One file, or a non-tty stdout, gets a plain
concatenation with no headers. `--plain` never emits headers.

## Environment

- `EAT_PAGER` -- pager override, beats `PAGER`.
- `EAT_FOLLOW_INTERVAL_MS` -- default poll interval for bare `-f`.
- `EAT_FOLLOW_NO_TUI=1` -- force the streaming follow path even on a tty.
- `FORCE_COLOR` / `NO_COLOR` -- see [Colour](#colour).
