//! Golden snapshot tests for the lsh highlighter.
//!
//! Layout: `tests/fixtures/<lang>/<case>.<ext>`. Each fixture has a sibling
//! `<case>.<ext>.snap` capturing the highlighter output. To accept new or
//! changed snapshots after a deliberate fix, rerun with
//! `UPDATE_GOLDEN=1 cargo test -p lsh --test golden`.
//!
//! The `Language` enum is generated from `definitions/*.lsh` by `build.rs`.
//! Adding a new lsh def adds a variant; the match in `fixture_subdir`
//! becomes non-exhaustive, forcing a fixture-dir entry at compile time.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use lsh::compiler::{Generator, SerializedCharset};
use lsh::runtime::{ConflictTag, Runtime};
use stdext::arena::scratch_arena;
use stdext::glob::glob_match;

include!(concat!(env!("OUT_DIR"), "/language_enum.rs"));

/// Subdirectory under `tests/fixtures/` that holds a language's fixtures.
/// Adding an lsh definition extends `Language`; this match must grow with
/// it (compile error otherwise).
fn fixture_subdir(lang: Language) -> &'static str {
    match lang {
        Language::Awk => "awk",
        Language::C => "c",
        Language::Cast => "cast",
        Language::Css => "css",
        Language::Csv => "csv",
        Language::Diff => "diff",
        Language::Dockerfile => "dockerfile",
        Language::Erb => "erb",
        Language::Fish => "fish",
        Language::GemfileLock => "gemfile_lock",
        Language::GitCommit => "git_commit",
        Language::GitRebase => "git_rebase",
        Language::Gleam => "gleam",
        Language::Go => "go",
        Language::GoMod => "go_mod",
        Language::GoSum => "go_sum",
        Language::Glsl => "glsl",
        Language::Hcl => "hcl",
        Language::Ignore => "ignore",
        Language::Javascript => "javascript",
        Language::Jq => "jq",
        Language::Json => "json",
        Language::Justfile => "justfile",
        Language::Lsh => "lsh",
        Language::Makefile => "makefile",
        Language::Man => "man",
        Language::Markdown => "markdown",
        Language::Objc => "objc",
        Language::Plain => "plain",
        Language::Powershell => "powershell",
        Language::Properties => "properties",
        Language::Python => "python",
        Language::Ruby => "ruby",
        Language::Rust => "rust",
        Language::Sed => "sed",
        Language::Shellscript => "shellscript",
        Language::SliceYaml => "slice_yaml",
        Language::Sps => "sps",
        Language::Toml => "toml",
        Language::Xml => "xml",
        Language::Yaml => "yaml",
    }
}

const SNAP_SUFFIX: &str = ".jsonl";

/// Suffixes that other test crates (e.g. `eat`) drop next to lsh fixtures.
/// Files named `<fixture>.<suffix>` for any of these are not lsh fixtures
/// and must be skipped during discovery.
const SIBLING_SNAP_SUFFIXES: &[&str] = &[".snap.ansi"];

/// A file is treated as a snapshot iff its name is `<fixture-name>.<suffix>`
/// for one of our recognised snapshot suffixes and a sibling `<fixture-name>`
/// exists. The sibling check keeps `.jsonl` usable as a regular fixture
/// extension (e.g. `sample.jsonl`) while still recognising real snapshots
/// like `kitchen_sink.md.jsonl` and `sample.jsonl.jsonl`.
fn is_snap(p: &Path) -> bool {
    let Some(s) = p.to_str() else { return false };
    if let Some(stem) = s.strip_suffix(SNAP_SUFFIX)
        && Path::new(stem).exists()
    {
        return true;
    }
    for suffix in SIBLING_SNAP_SUFFIXES {
        if let Some(stem) = s.strip_suffix(suffix)
            && Path::new(stem).exists()
        {
            return true;
        }
    }
    false
}

fn discover_fixtures(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            discover_fixtures(&p, out);
        } else if !is_snap(&p) {
            out.push(p);
        }
    }
}

fn snap_path(fixture: &Path) -> PathBuf {
    let mut p = fixture.to_path_buf();
    let mut name = OsString::from(fixture.file_name().unwrap());
    name.push(SNAP_SUFFIX);
    p.set_file_name(name);
    p
}

/// Append `s` as a JSON string literal (with surrounding quotes) onto `out`.
/// Bytes are decoded as UTF-8 with lossy replacement; invalid sequences
/// become U+FFFD before being JSON-encoded.
fn json_str(out: &mut String, s: &[u8]) {
    out.push('"');
    for c in String::from_utf8_lossy(s).chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn snap_line(out: &mut String, lineno: usize, kind: &str, text: &[u8], conflict: ConflictTag) {
    out.push_str(&format!("{{\"line\":{lineno},\"kind\":"));
    json_str(out, kind.as_bytes());
    out.push_str(",\"text\":");
    json_str(out, text);
    // Only lines inside a merge conflict carry the field, so snapshots of
    // conflict-free fixtures are unchanged by its existence.
    let conflict = match conflict {
        ConflictTag::None => None,
        ConflictTag::Marker => Some("marker"),
        ConflictTag::Ours => Some("ours"),
        ConflictTag::Base => Some("base"),
        ConflictTag::Theirs => Some("theirs"),
    };
    if let Some(conflict) = conflict {
        out.push_str(&format!(",\"conflict\":\"{conflict}\""));
    }
    out.push_str("}\n");
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn every_language_has_fixtures() {
    let root = fixtures_root();
    for &lang in ALL {
        let dir = root.join(fixture_subdir(lang));
        assert!(dir.exists(), "missing fixture dir for {lang:?}: {}", dir.display());
        let has_file = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.path().is_file() && !is_snap(&e.path()));
        assert!(has_file, "no fixture file in {}", dir.display());
    }
}

#[test]
fn golden() {
    let _ = stdext::arena::init(128 * 1024 * 1024);

    let outer = scratch_arena(None);
    let mut generator = Generator::new(&outer);
    generator.read_directory(lsh::compiler::builtin_definitions_path()).unwrap();
    let assembly = generator.assemble().unwrap();

    let charsets: Vec<SerializedCharset> =
        assembly.charsets.iter().map(|cs| cs.serialize()).collect();

    let max_id = assembly.highlight_kinds.iter().map(|hk| hk.value).max().unwrap_or(0);
    let mut kind_names: Vec<&str> = vec![""; max_id as usize + 1];
    for hk in &assembly.highlight_kinds {
        kind_names[hk.value as usize] = hk.identifier;
    }

    let root = fixtures_root();
    let mut fixtures = Vec::new();
    discover_fixtures(&root, &mut fixtures);
    fixtures.sort();
    assert!(!fixtures.is_empty(), "no fixtures found under {}", root.display());

    let update = env::var_os("UPDATE_GOLDEN").is_some();
    let mut failures: Vec<String> = Vec::new();

    for fixture in &fixtures {
        let path_bytes = fixture.as_os_str().as_encoded_bytes();
        let candidates: Vec<_> = assembly
            .entrypoints
            .iter()
            .filter(|ep| ep.paths.iter().any(|pat| glob_match(pat.as_bytes(), path_bytes)))
            .collect();

        let entrypoint = match candidates.as_slice() {
            // Plain claims no glob, so only its own fixture dir falls back to
            // it; anywhere else an unclaimed fixture is a broken glob.
            [] if fixture.parent().and_then(|p| p.file_name()) == Some(OsStr::new("plain")) => {
                assembly.entrypoints.iter().find(|ep| ep.name == "plain").unwrap()
            }
            [] => {
                failures.push(format!("no entrypoint for {}", fixture.display()));
                continue;
            }
            [only] => *only,
            _ => {
                // Multiple definitions claim this path (e.g. yaml + dialects).
                // Run each candidate's detector against the fixture head and
                // pick the first that returns true; fall back to the first
                // candidate w/o a detector (the base language).
                let src = fs::read(fixture).unwrap();
                let head = &src[..src.len().min(4096)];
                let mut runtime =
                    Runtime::new(&assembly.instructions, &assembly.strings, &charsets, 0);
                let mut base = None;
                let mut decided = None;
                for cand in &candidates {
                    match cand.detect_address {
                        Some(addr) => {
                            if runtime.detect(head, addr as u32) {
                                decided = Some(*cand);
                                break;
                            }
                        }
                        None => {
                            if base.is_none() {
                                base = Some(*cand);
                            }
                        }
                    }
                }
                decided.or(base).unwrap_or(candidates[0])
            }
        };

        let mut runtime = Runtime::new(
            &assembly.instructions,
            &assembly.strings,
            &charsets,
            entrypoint.address as u32,
        );

        let src = fs::read(fixture).unwrap();
        let mut snap = String::new();
        for (lineno, line) in src.split(|&b| b == b'\n').enumerate() {
            let line = match line.last() {
                Some(b'\r') => &line[..line.len() - 1],
                _ => line,
            };
            let scratch = scratch_arena(Some(&outer));
            runtime.set_line_number(lineno as u32 + 1);
            let parsed = runtime.parse_next_line::<u32>(&scratch, line);
            for w in parsed.spans.windows(2) {
                let curr = &w[0];
                let next = &w[1];
                let text = &line[curr.start..next.start];
                if text.is_empty() {
                    continue;
                }
                let kind = kind_names.get(curr.kind as usize).copied().unwrap_or("?");
                snap_line(&mut snap, lineno + 1, kind, text, parsed.conflict);
            }
        }

        let snap_file = snap_path(fixture);
        if update {
            fs::write(&snap_file, &snap).unwrap();
            continue;
        }
        // A missing snapshot is a failure, not something to fill in
        // silently. Writing one here would mean a new fixture blesses
        // whatever the highlighter happens to do, pass on the very same
        // run, and never be looked at by anyone.
        if !snap_file.exists() {
            failures.push(format!(
                "no snapshot for {} -- rerun with UPDATE_GOLDEN=1 and review the result",
                fixture.display()
            ));
            continue;
        }
        let existing = fs::read_to_string(&snap_file).unwrap_or_default();
        if existing != snap {
            let mut actual = snap_file.clone();
            let mut name = OsString::from(snap_file.file_name().unwrap());
            name.push(".actual");
            actual.set_file_name(name);
            fs::write(&actual, &snap).unwrap();
            failures.push(format!(
                "snapshot mismatch: {} (wrote {})",
                snap_file.display(),
                actual.display()
            ));
        }
    }

    if !failures.is_empty() {
        panic!(
            "{} golden failure(s); rerun with UPDATE_GOLDEN=1 to refresh:\n  {}",
            failures.len(),
            failures.join("\n  ")
        );
    }
}
