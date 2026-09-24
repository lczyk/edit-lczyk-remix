//! `eat DIR`: an eza-flavoured long listing in place of file contents.
//!
//! One row per entry -- size, git status, name -- with directories first
//! and dotfiles included. File names are coloured by the language their
//! name resolves to (globs only; no entry is opened to sniff it), hashed
//! onto the six ansi-16 hues so a language keeps its colour across runs.
//! Control characters in names are escaped, so a hostile filename cannot
//! drive the terminal. A directory whose only entry is another directory
//! is shown with it, `foo/bar/`, down to the first that isn't.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;

use lsh_defs::detect::process_file_associations;
use lsh_defs::{FILE_ASSOCIATIONS, PLAIN};

use crate::hash::hash_str;

const ANSI_RESET: &str = "\x1b[m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_DIR: &str = "\x1b[1;34m";
const ANSI_SYMLINK: &str = "\x1b[36m";

struct Entry {
    name: String,
    is_dir: bool,
    /// `Some` for a symlink, holding where it points.
    link_target: Option<String>,
    size: u64,
    /// The sole-subdirectory chain below a real directory, shown with it.
    chain: Vec<String>,
}

impl Entry {
    /// A real directory, not a link to one: gets the trailing `/`.
    fn is_real_dir(&self) -> bool {
        self.is_dir && self.link_target.is_none()
    }

    /// The entry's path below the listed dir, `/`-joined, unescaped.
    fn rel_path(&self) -> String {
        let mut path = self.name.clone();
        for seg in &self.chain {
            path.push('/');
            path.push_str(seg);
        }
        path
    }

    fn shown_name(&self) -> String {
        let mut shown = escape_controls(&self.name).into_owned();
        for seg in &self.chain {
            shown.push('/');
            shown.push_str(&escape_controls(seg));
        }
        if self.is_real_dir() {
            shown.push('/');
        }
        shown
    }
}

/// Render the listing of `dir`, one string per row. `plain` drops
/// everything but the names.
pub(crate) fn render(dir: &Path, plain: bool, use_color: bool) -> io::Result<Vec<String>> {
    let mut entries = read_entries(dir)?;
    sort_entries(&mut entries);

    if plain {
        return Ok(entries.iter().map(Entry::shown_name).collect());
    }

    let git = gutter::git::dir_status(dir).ok().map(|status| {
        let paths: Vec<String> = entries.iter().map(Entry::rel_path).collect();
        git_columns(&paths, &status)
    });

    let rows = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut row = String::new();
            if e.is_dir || e.link_target.is_some() {
                push_painted(&mut row, "   -", ANSI_DIM, use_color);
            } else {
                row.push_str(&format!("{:>4}", human_size(e.size)));
            }
            row.push(' ');
            if let Some(cols) = &git {
                for &c in &cols[i] {
                    push_painted(&mut row, c.encode_utf8(&mut [0; 4]), git_color(c), use_color);
                }
                row.push(' ');
            }
            push_name(&mut row, dir, e, use_color);
            row
        })
        .collect();
    Ok(rows)
}

fn read_entries(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for dirent in fs::read_dir(dir)? {
        match read_entry(dirent) {
            Ok(entry) => entries.push(entry),
            // Removed between the readdir and the stat; it is simply gone.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(entries)
}

fn read_entry(dirent: io::Result<fs::DirEntry>) -> io::Result<Entry> {
    let dirent = dirent?;
    let path = dirent.path();
    let meta = fs::symlink_metadata(&path)?;
    let link_target = meta
        .file_type()
        .is_symlink()
        .then(|| fs::read_link(&path).map(|t| t.to_string_lossy().into_owned()))
        .transpose()?;
    let chain = if meta.is_dir() { sole_subdir_chain(&path) } else { Vec::new() };
    Ok(Entry {
        name: dirent.file_name().to_string_lossy().into_owned(),
        // Follows symlinks, so a link to a directory groups with them.
        is_dir: path.is_dir(),
        link_target,
        size: meta.len(),
        chain,
    })
}

/// The names below `dir` for as long as each directory holds nothing but
/// one real subdirectory. Links are never followed, so the walk can't
/// loop; an unreadable level just ends it.
fn sole_subdir_chain(dir: &Path) -> Vec<String> {
    let mut chain = Vec::new();
    let mut path = dir.to_path_buf();
    while let Some(name) = sole_subdir(&path) {
        path.push(&name);
        chain.push(name.to_string_lossy().into_owned());
    }
    chain
}

fn sole_subdir(dir: &Path) -> Option<OsString> {
    let mut it = fs::read_dir(dir).ok()?;
    let only = it.next()?.ok()?;
    if it.next().is_some() || !only.file_type().ok()?.is_dir() {
        return None;
    }
    Some(only.file_name())
}

fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn push_name(row: &mut String, dir: &Path, e: &Entry, use_color: bool) {
    let shown = e.shown_name();
    let color = if e.link_target.is_some() {
        ANSI_SYMLINK
    } else if e.is_dir {
        ANSI_DIR
    } else {
        language_color(&dir.join(&e.name))
    };
    push_painted(row, &shown, color, use_color);
    if let Some(target) = &e.link_target {
        row.push_str(" -> ");
        row.push_str(&escape_controls(target));
    }
}

/// Control characters as rust escapes (`\u{1b}`, `\n`), like eza.
fn escape_controls(s: &str) -> Cow<'_, str> {
    if !s.chars().any(char::is_control) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_debug());
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

fn push_painted(row: &mut String, text: &str, color: &str, use_color: bool) {
    if use_color && !color.is_empty() {
        row.push_str(color);
        row.push_str(text);
        row.push_str(ANSI_RESET);
    } else {
        row.push_str(text);
    }
}

/// The sgr for a file's language, or `""` when its name resolves to none.
fn language_color(path: &Path) -> &'static str {
    const HUES: [&str; 6] =
        ["\x1b[31m", "\x1b[32m", "\x1b[33m", "\x1b[34m", "\x1b[35m", "\x1b[36m"];
    match process_file_associations(FILE_ASSOCIATIONS, path) {
        Some(lang) if !std::ptr::eq(lang, PLAIN) => {
            HUES[(hash_str(0, lang.id) % HUES.len() as u64) as usize]
        }
        _ => "",
    }
}

fn git_color(c: char) -> &'static str {
    match c {
        'N' => "\x1b[32m",
        'M' => "\x1b[34m",
        'D' | 'U' => "\x1b[31m",
        'R' | 'T' => "\x1b[33m",
        _ => ANSI_DIM,
    }
}

/// Decimal units, like eza: `340`, `1.2k`, `12k`, `3.4M`. Never wider
/// than four characters, so the column stays aligned.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["k", "M", "G", "T", "P", "E"];
    if bytes < 1000 {
        return bytes.to_string();
    }
    let mut value = bytes as f64;
    for unit in UNITS {
        value /= 1000.0;
        // The cutoffs sit where rounding would carry into another digit:
        // 9.96 prints as `10`, and 999.5 moves up a unit rather than `1000`.
        if value < 9.95 {
            return format!("{value:.1}{unit}");
        }
        if value < 999.5 {
            return format!("{value:.0}{unit}");
        }
    }
    unreachable!("u64 tops out at 18E")
}

/// The two-column status shown for each of `paths` (an entry plus its
/// collapsed chain, `/`-joined), eza-style: staged, then unstaged, `-` for
/// unchanged. A directory shows the most notable status among its
/// contents; ignored files deeper down don't count, so only a row whose
/// path is itself ignored, or sits in an ignored directory, shows as one.
fn git_columns(paths: &[String], status: &[([u8; 2], String)]) -> Vec<[char; 2]> {
    fn head(p: &str) -> &str {
        p.split_once('/').map_or(p, |(h, _)| h)
    }
    let index: HashMap<&str, usize> = paths.iter().enumerate().map(|(i, p)| (head(p), i)).collect();
    let mut cols = vec![['-', '-']; paths.len()];
    for (xy, rel) in status {
        let shown = display_status(*xy);
        if rel.is_empty() {
            cols.iter_mut().for_each(|c| merge(c, shown));
            continue;
        }
        let Some(&i) = index.get(head(rel)) else { continue };
        if xy == b"!!" && !covers(rel.trim_end_matches('/'), &paths[i]) {
            continue;
        }
        merge(&mut cols[i], shown);
    }
    cols
}

/// Whether `path` is `ancestor` or lies below it.
fn covers(ancestor: &str, path: &str) -> bool {
    path.strip_prefix(ancestor).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn display_status(xy: [u8; 2]) -> [char; 2] {
    match &xy {
        b"??" => ['-', 'N'],
        b"!!" => ['-', 'I'],
        // The unmerged pairs; `AA` and `DD` carry no `U` of their own.
        b"DD" | b"AU" | b"UD" | b"UA" | b"DU" | b"AA" | b"UU" => ['U', 'U'],
        _ => xy.map(|c| match c {
            b' ' => '-',
            b'A' | b'C' => 'N',
            c => c as char,
        }),
    }
}

fn merge(into: &mut [char; 2], from: [char; 2]) {
    fn rank(c: char) -> u8 {
        match c {
            '-' => 0,
            'I' => 1,
            'R' => 2,
            'T' => 3,
            'N' => 4,
            'D' => 5,
            'M' => 6,
            _ => 7,
        }
    }
    for (a, b) in into.iter_mut().zip(from) {
        if rank(b) > rank(*a) {
            *a = b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eat-listing-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn directories_come_first_and_dotfiles_are_listed() {
        let dir = scratch_dir("order");
        fs::write(dir.join("b.txt"), "").unwrap();
        fs::write(dir.join("A.txt"), "").unwrap();
        fs::write(dir.join(".hidden"), "").unwrap();
        fs::create_dir(dir.join("zdir")).unwrap();
        fs::create_dir(dir.join(".git-like")).unwrap();
        let rows = render(&dir, true, false).unwrap();
        assert_eq!(rows, [".git-like/", "zdir/", ".hidden", "A.txt", "b.txt"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_symlink_shows_its_target_and_groups_by_what_it_points_at() {
        let dir = scratch_dir("links");
        fs::create_dir(dir.join("real")).unwrap();
        fs::write(dir.join("f.rs"), "fn main() {}\n").unwrap();
        std::os::unix::fs::symlink("real", dir.join("to-dir")).unwrap();
        std::os::unix::fs::symlink("f.rs", dir.join("to-file")).unwrap();
        let rows = render(&dir, false, false).unwrap();
        assert!(rows[0].ends_with("real/"), "{rows:?}");
        assert!(rows[1].ends_with("to-dir -> real"), "{rows:?}");
        assert!(rows[2].ends_with("f.rs"), "{rows:?}");
        assert!(rows[3].ends_with("to-file -> f.rs"), "{rows:?}");
        // A link's size is its own, not its target's, so neither shows one.
        assert!(rows[1].starts_with("   - "), "{rows:?}");
        assert!(rows[3].starts_with("   - "), "{rows:?}");
        assert_eq!(rows.len(), 4);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_row_carries_size_and_name_and_no_git_column_outside_a_repo() {
        let dir = scratch_dir("row");
        fs::write(dir.join("f.txt"), "x".repeat(1234)).unwrap();
        fs::create_dir(dir.join("d")).unwrap();
        let rows = render(&dir, false, false).unwrap();
        // temp_dir is outside any repo, so there is no status column.
        assert_eq!(rows, ["   - d/", "1.2k f.txt"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_language_keeps_its_colour_and_plain_text_gets_none() {
        let a = language_color(Path::new("x/one.rs"));
        let b = language_color(Path::new("y/two.rs"));
        assert!(!a.is_empty());
        assert_eq!(a, b);
        assert_eq!(language_color(Path::new("README")), "");
    }

    #[test]
    fn sizes_are_decimal_and_short() {
        assert_eq!(human_size(0), "0");
        assert_eq!(human_size(999), "999");
        assert_eq!(human_size(1000), "1.0k");
        assert_eq!(human_size(1234), "1.2k");
        assert_eq!(human_size(12_345), "12k");
        assert_eq!(human_size(3_400_000), "3.4M");
        assert_eq!(human_size(9_949), "9.9k");
        assert_eq!(human_size(9_960), "10k");
        assert_eq!(human_size(999_499), "999k");
        assert_eq!(human_size(999_500), "1.0M");
        assert_eq!(human_size(999_999_999), "1.0G");
        assert_eq!(human_size(u64::MAX), "18E");
    }

    #[test]
    fn a_size_never_outgrows_its_column() {
        let mut n = 1u64;
        while n < u64::MAX / 3 {
            for probe in [n - 1, n, n + n / 2, n * 3 - 1] {
                assert!(human_size(probe).len() <= 4, "{probe} -> {}", human_size(probe));
            }
            n *= 10;
        }
    }

    #[test]
    fn control_characters_in_names_are_escaped() {
        assert_eq!(escape_controls("plain.rs"), "plain.rs");
        assert_eq!(escape_controls("a\x1b[31mb"), "a\\u{1b}[31mb");
        assert_eq!(escape_controls("two\nlines"), "two\\nlines");
        let dir = scratch_dir("controls");
        fs::write(dir.join("evil\x1b]0;title\x07.txt"), "").unwrap();
        for plain in [true, false] {
            let rows = render(&dir, plain, true).unwrap();
            assert!(rows[0].contains("evil\\u{1b}]0;title\\u{7}.txt"), "{rows:?}");
            assert!(!rows[0].contains('\x07'), "{rows:?}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_unmerged_pair_shows_as_a_conflict() {
        for xy in [b"DD", b"AU", b"UD", b"UA", b"DU", b"AA", b"UU"] {
            assert_eq!(display_status(*xy), ['U', 'U'], "{}", String::from_utf8_lossy(xy));
        }
        // A plain staged add is not a conflict.
        assert_eq!(display_status(*b"A "), ['N', '-']);
    }

    #[test]
    fn git_status_lands_on_the_entry_it_names() {
        let st = vec![
            (*b" M", "a.rs".to_string()),
            (*b"A ", "b.rs".to_string()),
            (*b"??", "c.rs".to_string()),
            (*b"!!", "target/".to_string()),
        ];
        let cols = git_columns(&paths(&["a.rs", "b.rs", "c.rs", "target", "clean.rs"]), &st);
        assert_eq!(cols, [['-', 'M'], ['N', '-'], ['-', 'N'], ['-', 'I'], ['-', '-']]);
    }

    #[test]
    fn a_directory_shows_the_most_notable_status_inside_it() {
        let st = vec![
            (*b"??", "src/new.rs".to_string()),
            (*b" M", "src/lib.rs".to_string()),
            (*b"!!", "src/debug.log".to_string()),
            (*b"!!", "docs/out.html".to_string()),
        ];
        let cols = git_columns(&paths(&["src", "docs"]), &st);
        // An ignored file inside a directory doesn't make the directory ignored.
        assert_eq!(cols, [['-', 'M'], ['-', '-']]);
    }

    #[test]
    fn a_wholly_ignored_dir_marks_every_entry() {
        let st = vec![(*b"!!", String::new())];
        assert_eq!(git_columns(&paths(&["a", "b"]), &st), [['-', 'I'], ['-', 'I']]);
    }

    fn paths(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn a_chain_of_sole_subdirectories_collapses_into_one_row() {
        let dir = scratch_dir("chain");
        fs::create_dir_all(dir.join("foo/bar/baz")).unwrap();
        fs::write(dir.join("foo/bar/baz/x.rs"), "").unwrap();
        fs::write(dir.join("foo/bar/baz/y.rs"), "").unwrap();
        fs::create_dir_all(dir.join("a/b/c/d/e")).unwrap();
        fs::write(dir.join("f.txt"), "").unwrap();
        assert_eq!(render(&dir, true, false).unwrap(), ["a/b/c/d/e/", "foo/bar/baz/", "f.txt"]);
        let rows = render(&dir, false, false).unwrap();
        assert_eq!(rows, ["   - a/b/c/d/e/", "   - foo/bar/baz/", "   0 f.txt"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_chain_stops_at_a_directory_with_more_than_one_entry() {
        let dir = scratch_dir("chain-stop");
        fs::create_dir_all(dir.join("one/two")).unwrap();
        fs::create_dir(dir.join("one/two/three")).unwrap();
        fs::create_dir(dir.join("one/two/four")).unwrap();
        fs::create_dir_all(dir.join("dot/sub")).unwrap();
        fs::write(dir.join("dot/.hidden"), "").unwrap();
        fs::create_dir_all(dir.join("top/a")).unwrap();
        fs::create_dir(dir.join("top/b")).unwrap();
        assert_eq!(render(&dir, true, false).unwrap(), ["dot/", "one/two/", "top/"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_chain_ends_before_a_file_or_a_link() {
        let dir = scratch_dir("chain-end");
        fs::create_dir_all(dir.join("f/g")).unwrap();
        fs::write(dir.join("f/g/only.rs"), "").unwrap();
        fs::create_dir_all(dir.join("l/m")).unwrap();
        fs::create_dir(dir.join("target")).unwrap();
        std::os::unix::fs::symlink("../../target", dir.join("l/m/link")).unwrap();
        // A link to a dir holding a sole subdirectory is not expanded either.
        fs::create_dir_all(dir.join("real/inner")).unwrap();
        std::os::unix::fs::symlink("real", dir.join("to-real")).unwrap();
        let rows = render(&dir, true, false).unwrap();
        assert_eq!(rows, ["f/g/", "l/m/", "real/inner/", "target/", "to-real"]);
        let rows = render(&dir, false, false).unwrap();
        assert!(rows[4].ends_with(" to-real -> real"), "{rows:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unreadable_level_ends_the_chain() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("chain-perm");
        fs::create_dir_all(dir.join("p/q/r")).unwrap();
        fs::set_permissions(dir.join("p/q"), fs::Permissions::from_mode(0o000)).unwrap();
        let rows = render(&dir, true, false);
        fs::set_permissions(dir.join("p/q"), fs::Permissions::from_mode(0o755)).unwrap();
        // Root reads through a 000 dir, so the chain may run on to r.
        assert!(matches!(rows.unwrap().as_slice(), [s] if s == "p/q/" || s == "p/q/r/"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_segment_of_a_chain_is_escaped() {
        let dir = scratch_dir("chain-escape");
        fs::create_dir_all(dir.join("a\x1b/b\x07c")).unwrap();
        let rows = render(&dir, true, false).unwrap();
        assert_eq!(rows, ["a\\u{1b}/b\\u{7}c/"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_collapsed_row_takes_the_status_of_everything_inside_it() {
        let st = vec![
            (*b"??", "foo/bar/baz/new.rs".to_string()),
            (*b"!!", "foo/bar/baz/debug.log".to_string()),
            (*b"!!", "ign/sub/".to_string()),
            (*b"!!", "half/".to_string()),
        ];
        let cols = git_columns(&paths(&["foo/bar/baz", "ign/sub", "half/way", "clean/x"]), &st);
        // An ignored dir at or above the row's path covers the whole row;
        // an ignored file below it doesn't.
        assert_eq!(cols, [['-', 'N'], ['-', 'I'], ['-', 'I'], ['-', '-']]);
        let st = vec![(*b"!!", "foo/bar/baz/deeper/".to_string())];
        assert_eq!(git_columns(&paths(&["foo/bar/baz"]), &st), [['-', '-']]);
        // A sibling whose name merely extends the row's is not above it.
        assert!(!covers("foo/ba", "foo/bar"));
        assert!(covers("foo", "foo/bar"));
        assert!(covers("foo/bar", "foo/bar"));
    }
}
