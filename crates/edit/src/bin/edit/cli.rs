//! Command-line surface: argument parsing, the quirks table, filename
//! vetting, and the help / version output.
//!
//! Split out from `main.rs` so the runtime concerns there (terminal
//! bootstrap, the event loop, draw dispatch) aren't interleaved with a
//! few hundred lines of argv handling. Nothing here touches the tui.

use std::collections::HashSet;
use std::env;
#[cfg(debug_assertions)]
use std::path::Path;

use edit::sys;

use crate::apperr;
#[cfg(debug_assertions)]
use crate::devlog;
use crate::document;
#[cfg(debug_assertions)]
use crate::keybindings;

/// Toggleable behaviours. Parsed from `--quirks=a,b,c`.
///
/// Canonical spellings are positive (`color`, `animations`). Entries
/// marked `default` are on at startup and disabled with
/// `--quirks=-NAME`; entries without `default` are off until enabled
/// with `--quirks=NAME`. `polyflag::defaults` seeds the set from this
/// table so it doubles as schema and policy.
const KNOWN_QUIRKS: &[polyflag::KnownToken] = &[
    polyflag::token!(default "unicode"),
    polyflag::token!(default "color"; "colour"),
    polyflag::token!(default "animations"),
    polyflag::token!(default "safe-filenames"),
    polyflag::token!("create"; "allow-create", "allowcreate"),
];

pub(crate) fn parse_args() -> apperr::Result<Option<std::path::PathBuf>> {
    let mut path: Option<std::path::PathBuf> = None;
    let mut accept_flags = true;
    let mut quirks: HashSet<&'static str> = polyflag::defaults(KNOWN_QUIRKS);
    let mut seen_quirks_flag = false;

    // EDIT_QUIRKS layers in before any cli flag, so a `--quirks=-name`
    // override can negate an env-provided default. The env var name is
    // derived by polyflag from the prefix and flag, so cli surface and
    // env surface stay in lock-step.
    debug_assert!(check_quirks_table(), "KNOWN_QUIRKS has duplicate or empty spelling");
    let warn_deprecated = |spelling: &str, canonical: &'static str| {
        sys::write_stdout(&format!(
            "edit: warning: --quirks={spelling} is deprecated, use {canonical}\n"
        ));
    };
    if let Err(e) = polyflag::apply_env_for_flag_with_callback(
        "edit",
        "quirks",
        KNOWN_QUIRKS,
        &mut quirks,
        warn_deprecated,
    ) {
        sys::write_stdout(&format!(
            "edit: {} contains unknown quirk {:?}\nknown quirks: {}\n",
            polyflag::env_var_name("edit", "quirks"),
            e.0,
            known_quirks_for_help(),
        ));
        return Ok(None);
    }

    for arg in env::args_os().skip(1) {
        if accept_flags {
            if arg == "--" {
                accept_flags = false;
                continue;
            }
            if arg == "-h" || arg == "--help" {
                print_help();
                return Ok(None);
            }
            if arg == "-v" || arg == "--version" {
                print_version();
                return Ok(None);
            }
            if arg == "-L" || arg == "--list-languages" {
                edit::langlist::list_languages(edit::langlist::ListFormat::Pretty);
                return Ok(None);
            }
            if let Some(value) = arg
                .to_str()
                .and_then(|s| s.strip_prefix("--list-languages=").or_else(|| s.strip_prefix("-L=")))
            {
                match edit::langlist::ListFormat::parse(value) {
                    Ok(fmt) => {
                        edit::langlist::list_languages(fmt);
                        return Ok(None);
                    }
                    Err(e) => {
                        sys::write_stdout(&format!("edit: {e}\n"));
                        return Ok(None);
                    }
                }
            }
            if let Some(list) = arg.to_str().and_then(|s| s.strip_prefix("--quirks=")) {
                if seen_quirks_flag {
                    sys::write_stdout("edit: --quirks may only be passed once\n");
                    return Ok(None);
                }
                seen_quirks_flag = true;
                if let Err(e) =
                    polyflag::apply_with_callback(list, KNOWN_QUIRKS, &mut quirks, warn_deprecated)
                {
                    sys::write_stdout(&format!(
                        "edit: unknown quirk {:?}\nknown quirks: {}\n",
                        e.0,
                        known_quirks_for_help(),
                    ));
                    return Ok(None);
                }
                continue;
            }
            #[cfg(debug_assertions)]
            if let Some(p) = arg.to_str().and_then(|s| s.strip_prefix("--logfile=")) {
                if let Err(e) = devlog::open(Path::new(p)) {
                    sys::write_stdout(&format!("failed to open logfile: {e}\n"));
                }
                continue;
            }
            #[cfg(debug_assertions)]
            if arg == "--force-reset-config" {
                if let Err(e) = keybindings::force_reset() {
                    sys::write_stdout(&format!("failed to reset config: {e:?}\n"));
                    return Ok(None);
                }
                sys::write_stdout("config reset\n");
                continue;
            }

            // Unknown flag: anything starting with `-` that survived the
            // checks above. Refuse rather than silently treating it as a
            // path. Use `--` to open files whose names start with `-`.
            if arg.to_str().is_some_and(|s| s.starts_with('-') && s != "-") {
                let arg = arg.to_string_lossy();
                sys::write_stdout(&format!(
                    "edit: unknown option {arg:?}\n\
                     try 'edit --help', or 'edit -- {arg}' to open a file with that name\n"
                ));
                return Ok(None);
            }
        }

        if path.is_some() {
            sys::write_stdout("edit: only one file argument is supported\n");
            return Ok(None);
        }
        path = Some(std::path::PathBuf::from(&arg));
    }

    // Apply quirks that affect global rendering state. NO_COLOR env var
    // (per https://no-color.org) is honoured the same way as
    // `--quirks=-color` -- either turns colour off across the editor.
    edit::glyphs::set_ascii_only(!quirks.contains("unicode"));
    edit::glyphs::set_no_color(!quirks.contains("color") || edit::glyphs::env_disables_color());
    edit::glyphs::set_no_animations(!quirks.contains("animations"));
    document::set_allow_create(quirks.contains("create"));

    match path {
        Some(p) => {
            // Refuse weird filenames (see [`is_safe_filename`]). Catches
            // creation of new files with surprising names and rare-but-real
            // existing files with weird names (e.g. left behind by a buggy
            // tool). Disable with `--quirks=-safe-filenames`.
            let (file_path, _) = document::parse_filename_goto(&p);
            // The raw path first: a directory named `x:5` is not `x` at line 5.
            if let Some(dir) = [p.as_path(), file_path].into_iter().find(|p| p.is_dir()) {
                let msg = format!("edit: {}: Is a directory", dir.display());
                return Err(std::io::Error::new(std::io::ErrorKind::IsADirectory, msg).into());
            }
            let name = file_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if quirks.contains("safe-filenames") && !is_safe_filename(name) {
                sys::write_stdout(&format!(
                    "edit: refusing filename {name:?}\n\
                     pass `--quirks=-safe-filenames` to allow\n"
                ));
                return Ok(None);
            }
            // Refuse to create new files unless `--quirks=create`.
            // edit never creates directories regardless of this quirk.
            if !quirks.contains("create") && !file_path.exists() {
                let display = file_path.display();
                sys::write_stdout(&format!(
                    "edit: refusing to create new file: {display}\n\
                     pass `--quirks=create` to allow\n"
                ));
                return Ok(None);
            }
            Ok(Some(p))
        }
        None => {
            print_help();
            Ok(None)
        }
    }
}

/// Safe-filename gate. A filename must:
/// - be non-empty.
/// - not be `.` or `..` (those name directories, not files).
/// - have at most one leading dot (`.gitignore` ok, `..tilde` not).
/// - not start with `-` (would be confused with a cli flag downstream).
/// - contain at least one ASCII letter (`123` is weird).
/// - be ASCII only (no unicode -- emoji, accented chars, CJK are weird).
/// - have a stem from `[A-Za-z0-9_+-]` and only alphanumeric extension
///   parts. Splitting on `.` after stripping any single leading dot:
///   the first segment is the stem; the rest are extensions and must be
///   plain alphanumerics (so `foo.tar.gz` is fine, `foo.b-c~d` is not).
///
/// Disable with `--quirks=-safe-filenames`.
fn is_safe_filename(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." || name.starts_with('-') {
        return false;
    }
    if name.starts_with("..") {
        return false;
    }
    if !name.bytes().any(|b| b.is_ascii_alphabetic()) {
        return false;
    }

    let body = name.strip_prefix('.').unwrap_or(name);
    let mut parts = body.split('.');
    let Some(stem) = parts.next() else {
        return false;
    };
    if stem.is_empty()
        || !stem.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+'))
    {
        return false;
    }
    parts.all(|ext| !ext.is_empty() && ext.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Render a comma-separated list of accepted quirk spellings for use in
/// error messages. Canonicals are listed with any non-`Hidden` aliases
/// shown parenthetically -- `--quirks=` accepts both forms.
fn known_quirks_for_help() -> String {
    use polyflag::AliasStatus;
    let mut out = String::new();
    for kt in KNOWN_QUIRKS {
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(kt.canonical);
        let mut first_alt = true;
        for alias in kt.aliases {
            if matches!(alias.status, AliasStatus::Alternative | AliasStatus::Deprecated) {
                out.push_str(if first_alt { " (" } else { ", " });
                out.push_str(alias.spelling);
                first_alt = false;
            }
        }
        if !first_alt {
            out.push(')');
        }
    }
    out
}

/// Wrap [`polyflag::check_known`] so the call site is one line. Returns
/// `true` if the table is well-formed; in debug builds a malformed table
/// panics inside `check_known` before we ever return.
fn check_quirks_table() -> bool {
    polyflag::check_known(KNOWN_QUIRKS);
    true
}

pub(crate) fn print_help() {
    sys::write_stdout(concat!(
        "Usage: edit [OPTIONS] [--] FILE[:LINE[:COLUMN]]\n",
        "Options:\n",
        "    --eat            Act as eat: read stdin if piped, or files from\n",
        "                     arguments, and page through them.\n",
        "                     Equivalent to running the eat binary directly.\n",
        "    -h, --help       Print this help message\n",
        "    -v, --version    Print the version number\n",
        "    -L, --list-languages[=FORMAT]\n",
        "                     Print known syntax-highlighting languages and exit.\n",
        "                     FORMAT is pretty (default), plain, or json.\n",
        "                     Use --list-languages=FORMAT or -L=FORMAT to pick.\n",
        "    --               End of options. Subsequent arguments are treated as\n",
        "                     file names even if they start with `-`.\n",
        "                     Example: `edit -- --version` opens a file called `--version`.\n",
        "    --quirks=LIST    Comma-separated toggles. `NAME` enables, `-NAME` disables.\n",
        "                     Defaults shown in [brackets]. Known quirks:\n",
        "                       [on]  unicode        -- render UI with unicode glyphs\n",
        "                                              (box-drawing etc). disable for\n",
        "                                              ASCII-only output.\n",
        "                       [on]  color          -- emit SGR colour. disable to drop\n",
        "                                              colour while keeping attributes.\n",
        "                                              alias: colour\n",
        "                       [on]  animations     -- cursor / scroll / floater motion.\n",
        "                                              logic stays instant when disabled;\n",
        "                                              only visible interpolation is\n",
        "                                              suppressed.\n",
        "                       [on]  safe-filenames -- refuse weird filenames. names\n",
        "                                              must be ASCII-only, contain a\n",
        "                                              letter, not start with `-`, have\n",
        "                                              at most one leading dot, not be\n",
        "                                              `.` or `..`; stem is [A-Za-z0-9_+-],\n",
        "                                              extension(s) alphanumeric only.\n",
        "                                              disable to permit weird names.\n",
        "                       [off] create         -- allow creating new files. without\n",
        "                                              this, edit refuses to open / save a\n",
        "                                              missing path. edit never creates\n",
        "                                              directories regardless.\n",
        "                                              aliases: allow-create, allowcreate\n",
        "\n",
        "Arguments:\n",
        "    FILE[:LINE[:COLUMN]]    The file to open, optionally with line and column (e.g., foo.txt:123:45)\n",
        "\n",
        "Environment:\n",
        "    EDIT_QUIRKS    Comma-separated quirks applied before any --quirks flag.\n",
        "                   Use --quirks=-NAME to negate an entry from EDIT_QUIRKS.\n",
    ));
    #[cfg(debug_assertions)]
    sys::write_stdout(concat!(
        "\nDebug-build options:\n",
        "    --logfile=PATH          Log inputs + buffer state as JSONL\n",
        "    --force-reset-config    Wipe config dir and rewrite defaults, then continue\n",
    ));
}

pub(crate) fn print_version() {
    sys::write_stdout(&format!("edit {}\n", version::version!()));
}

#[cfg(test)]
mod tests {
    use super::is_safe_filename;

    #[test]
    fn safe_filenames_accepted() {
        for name in [
            "foo",
            "foo.txt",
            "Foo_Bar.tar.gz",
            "_underscore",
            "v2",
            "1.txt",
            "foo+bar.tar.gz",
            "my-file_v2.tar.gz",
            ".gitignore",
            ".env.local",
        ] {
            assert!(is_safe_filename(name), "expected {name:?} to be safe");
        }
    }

    #[test]
    fn unsafe_filenames_rejected() {
        for name in [
            "",
            ".",
            "..",
            "...foo",
            "..tilde",
            "tilde~mid",
            "trailing~",
            "foo.b-c_d",
            "foo..txt",
            "-leading-dash.txt",
            "--double-dash",
            "with space.txt",
            "a/b",
            "a\\b",
            "foo:bar",
            "foo\"bar",
            "foo|bar",
            "foo$bar",
            "foo#bar",
            "foo,bar",
            "foo(bar)",
            "key=value.conf",
            "user@host.txt",
            "0123",
            "42",
            "h\u{e9}llo.txt",                   // hello with an acute e
            "r\u{e9}sum\u{e9}.pdf",             // resume with acutes
            "\u{444}\u{430}\u{439}\u{43b}.txt", // cyrillic
            "\u{4f60}\u{597d}.md",              // cjk
            "rocket\u{1f680}.txt",              // emoji
            "\u{1f480}",                        // emoji
            "newline\n",
        ] {
            assert!(!is_safe_filename(name), "expected {name:?} to be unsafe");
        }
    }
}
