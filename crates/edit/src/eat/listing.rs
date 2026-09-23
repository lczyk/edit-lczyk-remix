//! `eat DIR`: an eza-flavoured long listing in place of file contents.
//!
//! One row per entry -- size, git status, name -- with directories first
//! and dotfiles included. File names are coloured by the language their
//! name resolves to (globs only; no entry is opened to sniff it), hashed
//! onto the six ansi-16 hues so a language keeps its colour across runs.

use std::collections::HashMap;
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
}

impl Entry {
    /// A real directory, not a link to one: gets the trailing `/`.
    fn is_real_dir(&self) -> bool {
        self.is_dir && self.link_target.is_none()
    }
}

/// Render the listing of `dir`, one string per row. `plain` drops
/// everything but the names.
pub(crate) fn render(dir: &Path, plain: bool, use_color: bool) -> io::Result<Vec<String>> {
    let mut entries = read_entries(dir)?;
    sort_entries(&mut entries);

    if plain {
        return Ok(entries
            .iter()
            .map(|e| if e.is_real_dir() { format!("{}/", e.name) } else { e.name.clone() })
            .collect());
    }

    let git = gutter::git::dir_status(dir).ok().map(|status| {
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        git_columns(&names, &status)
    });

    let rows = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut row = String::new();
            if e.is_real_dir() {
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
        let dirent = dirent?;
        let path = dirent.path();
        let meta = fs::symlink_metadata(&path)?;
        let link_target = meta
            .file_type()
            .is_symlink()
            .then(|| fs::read_link(&path).map(|t| t.to_string_lossy().into_owned()))
            .transpose()?;
        entries.push(Entry {
            name: dirent.file_name().to_string_lossy().into_owned(),
            // Follows symlinks, so a link to a directory groups with them.
            is_dir: path.is_dir(),
            link_target,
            size: meta.len(),
        });
    }
    Ok(entries)
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
    let shown = if e.is_real_dir() { format!("{}/", e.name) } else { e.name.clone() };
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
        row.push_str(target);
    }
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

/// Decimal units, like eza: `340`, `1.2k`, `12k`, `3.4M`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["k", "M", "G", "T", "P"];
    if bytes < 1000 {
        return bytes.to_string();
    }
    let mut value = bytes as f64;
    let mut unit = "";
    for u in UNITS {
        value /= 1000.0;
        unit = u;
        if value < 1000.0 {
            break;
        }
    }
    if value < 10.0 { format!("{value:.1}{unit}") } else { format!("{value:.0}{unit}") }
}

/// The two-column status shown for each of `names`, eza-style: staged,
/// then unstaged, `-` for unchanged. A directory shows the most notable
/// status among its contents; ignored files deeper down don't count, so
/// only a directory that is itself ignored shows as one.
fn git_columns(names: &[&str], status: &[([u8; 2], String)]) -> Vec<[char; 2]> {
    let index: HashMap<&str, usize> = names.iter().enumerate().map(|(i, n)| (*n, i)).collect();
    let mut cols = vec![['-', '-']; names.len()];
    for (xy, rel) in status {
        let shown = display_status(*xy);
        let (head, deeper) = match rel.split_once('/') {
            Some((head, rest)) => (head, !rest.is_empty()),
            None => (rel.as_str(), false),
        };
        if rel.is_empty() {
            cols.iter_mut().for_each(|c| merge(c, shown));
        } else if let Some(&i) = index.get(head)
            && !(deeper && xy == b"!!")
        {
            merge(&mut cols[i], shown);
        }
    }
    cols
}

fn display_status(xy: [u8; 2]) -> [char; 2] {
    match &xy {
        b"??" => ['-', 'N'],
        b"!!" => ['-', 'I'],
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
        assert_eq!(human_size(999_999_999), "1000M");
    }

    #[test]
    fn git_status_lands_on_the_entry_it_names() {
        let st = vec![
            (*b" M", "a.rs".to_string()),
            (*b"A ", "b.rs".to_string()),
            (*b"??", "c.rs".to_string()),
            (*b"!!", "target/".to_string()),
        ];
        let cols = git_columns(&["a.rs", "b.rs", "c.rs", "target", "clean.rs"], &st);
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
        let cols = git_columns(&["src", "docs"], &st);
        // An ignored file inside a directory doesn't make the directory ignored.
        assert_eq!(cols, [['-', 'M'], ['-', '-']]);
    }

    #[test]
    fn a_wholly_ignored_dir_marks_every_entry() {
        let st = vec![(*b"!!", String::new())];
        assert_eq!(git_columns(&["a", "b"], &st), [['-', 'I'], ['-', 'I']]);
    }
}
