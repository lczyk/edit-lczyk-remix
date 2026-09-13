//! Language detection -- path glob, shebang, content sniffing.
//!
//! Single shared impl for every consumer of the bundled lsh definitions.
//! [`resolve`] runs the whole chain and ends in [`PLAIN`]; the pieces it
//! is built from are public for callers that need one step on its own:
//!
//! - [`match_file_associations`] -- glob a path against an association table,
//!   collect every unique candidate. Used together with [`disambiguate_language`]
//!   when two definitions share an extension (e.g. yaml + yaml dialect).
//! - [`language_from_shebang`] -- pull the interpreter token from a `#!...`
//!   line, resolve it via each language's `shebangs` attribute, fall back to
//!   a hardcoded alias map (covers interpreters whose def is the parent
//!   dialect: `dash`/`ksh`/`ash` -> shellscript, `node`/`deno`/`bun` ->
//!   javascript).
//! - [`language_from_content`] -- last-resort sniff. Currently identifies markdown
//!   (extensionless `README` / `NOTES`) and unified/git diff (`git diff | eat`).

use std::path::Path;

use lsh::runtime::{Language, Runtime};
use stdext::glob::glob_match;

use crate::{ASSEMBLY, CHARSETS, FILE_ASSOCIATIONS, LANGUAGES, PLAIN, STRINGS};

/// For callers with no associations of their own.
pub const NO_USER_ASSOCIATIONS: &[(&str, &Language)] = &[];

/// Resolve the language of a buffer. In order: the user's own associations,
/// the bundled globs (a glob several dialects share is settled by their
/// detectors against the head), a shebang, a content sniff, and finally
/// [`PLAIN`]. The one place that fallback is decided. `head` is the first
/// few KiB of the buffer and is only fetched when the path alone does not
/// decide.
pub fn resolve<T: AsRef<[u8]>>(
    path: Option<&Path>,
    user_associations: &[(T, &'static Language)],
    head: impl FnOnce() -> Vec<u8>,
) -> &'static Language {
    let mut candidates = Vec::new();
    if let Some(path) = path {
        // A user association settles the path by itself; the bundled
        // dialects and their detectors never get to overrule it.
        candidates = match_file_associations(user_associations, path);
        if candidates.is_empty() {
            candidates = match_file_associations(FILE_ASSOCIATIONS, path);
        }
    }
    if let [only] = candidates.as_slice() {
        return only;
    }
    let head = head();
    if let Some(lang) = disambiguate_language(&candidates, &head) {
        return lang;
    }
    language_from_shebang(&head).or_else(|| language_from_content(&head)).unwrap_or(PLAIN)
}

/// Walk an association table, return the first language whose glob matches.
/// Single-match shortcut for callers that don't care about dialect ambiguity.
pub fn process_file_associations<T>(
    associations: &[(T, &'static Language)],
    path: &Path,
) -> Option<&'static Language>
where
    T: AsRef<[u8]>,
{
    let path = path.as_os_str().as_encoded_bytes();
    for a in associations {
        if glob_match(a.0.as_ref(), path) {
            return Some(a.1);
        }
    }
    None
}

/// Collect every entry whose glob matches `path`, in iteration order, with
/// duplicates filtered out. Used for content-aware dialect disambiguation:
/// when two definitions share a glob (e.g. plain yaml and a yaml dialect both
/// register `**/*.yaml`), the caller needs the full candidate set so it can
/// run each candidate's [`Language::detect_entrypoint`] against the buffer
/// head.
pub fn match_file_associations<T>(
    associations: &[(T, &'static Language)],
    path: &Path,
) -> Vec<&'static Language>
where
    T: AsRef<[u8]>,
{
    let path_bytes = path.as_os_str().as_encoded_bytes();
    let mut hits: Vec<&'static Language> = Vec::new();
    for a in associations {
        if glob_match(a.0.as_ref(), path_bytes) && !hits.iter().any(|l| std::ptr::eq(*l, a.1)) {
            hits.push(a.1);
        }
    }
    hits
}

/// Pick the right language from a candidate set, using content-based
/// disambiguation when the set has more than one entry. Rules:
///
/// - Single candidate -- returned as-is, no buffer read.
/// - Multiple candidates -- each with a [`Language::detect_entrypoint`] is
///   run against `head`. First detector that returns true wins.
/// - If no detector matches, the first candidate w/out a `detect_entrypoint`
///   is returned (the "base" fallback). If every candidate has a detector
///   and none matched, the first listed candidate is returned to keep the
///   call non-failing.
pub fn disambiguate_language(
    candidates: &[&'static Language],
    head: &[u8],
) -> Option<&'static Language> {
    match candidates {
        [] => None,
        [only] => Some(only),
        _ => {
            let mut runtime = Runtime::new(&ASSEMBLY, &STRINGS, &CHARSETS, 0);
            let mut base: Option<&'static Language> = None;
            for &cand in candidates {
                match cand.detect_entrypoint {
                    Some(detect_ep) => {
                        if runtime.detect(head, detect_ep) {
                            return Some(cand);
                        }
                    }
                    None => {
                        if base.is_none() {
                            base = Some(cand);
                        }
                    }
                }
            }
            base.or_else(|| candidates.first().copied())
        }
    }
}

/// Try to identify the language of a buffer from its shebang (`#!...`) line.
/// Used as a fallback for files whose extension didn't match any association --
/// executable scripts w/out a file extension are the main motivating case.
///
/// `head` should be the first chunk of the buffer; only the first line is
/// inspected. Returns `None` if there's no shebang or the interpreter isn't
/// recognised.
pub fn language_from_shebang(head: &[u8]) -> Option<&'static Language> {
    let line = head.split(|&b| b == b'\n').next()?;
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if !line.starts_with(b"#!") {
        return None;
    }
    let body = &line[2..];

    // Walk whitespace-separated tokens. Skip flags (`-S`, `-u`, ...) and the
    // `env` wrapper itself; the first remaining token names the interpreter.
    let mut interpreter: Option<&[u8]> = None;
    for tok in body.split(|&b| b == b' ' || b == b'\t') {
        if tok.is_empty() || tok.starts_with(b"-") {
            continue;
        }
        let base = basename(tok);
        if base == b"env" {
            continue;
        }
        interpreter = Some(base);
        break;
    }
    let token = strip_version_suffix(interpreter?);

    // First: data-driven match against each language's `shebangs` attribute.
    // This is the source of truth -- definitions declare what they answer to.
    for lang in LANGUAGES {
        for sb in lang.shebangs {
            if token == sb.as_bytes() {
                return Some(lang);
            }
        }
    }

    // Fallback: hardcoded alias map. Covers interpreters whose dialect is
    // registered under a different name -- e.g. `dash` is shellscript, `node`
    // is javascript. Avoids forcing every alias into the def file's shebangs.
    let id = interpreter_to_language_id(token)?;
    LANGUAGES.iter().find(|l| l.id == id)
}

/// Try to identify the language of a buffer from its content alone, when
/// neither path nor shebang detection produced a hit. Recognises:
///
/// - Markdown -- common case is `README` / `NOTES` / `LICENSE` w/out a `.md`
///   suffix, or a brand-new buffer where the user has started typing markdown.
/// - Unified/git diff -- common case is `git diff | eat`, or an extensionless
///   patch file dropped into the buffer.
///
/// `head` should be a chunk of the buffer (a few KB is plenty); only the
/// first ~64 lines are inspected.
pub fn language_from_content(head: &[u8]) -> Option<&'static Language> {
    if looks_like_diff(head) {
        return LANGUAGES.iter().find(|l| l.id == "diff");
    }
    // Properties is checked before markdown: a `#`-comment-heavy ini file
    // (e.g. `default-params`) scores as markdown headings otherwise. The
    // properties sniff is strict -- any prose line fails it -- so genuine
    // markdown still falls through.
    if looks_like_properties(head) {
        return LANGUAGES.iter().find(|l| l.id == "properties");
    }
    if looks_like_markdown(head) {
        return LANGUAGES.iter().find(|l| l.id == "markdown");
    }
    None
}

/// Sniff extensionless ini / conf / properties files (e.g. a `default-params`
/// config with no suffix). Deliberately strict to avoid stealing shell scripts
/// or makefiles: EVERY nonempty line must be a comment, a `[section]` header,
/// or a space-padded `key = value`. The padding requirement (` = `) is what
/// separates ini assignments from shell `VAR=val`. Unpadded ini still relies on
/// the `*.ini` path glob.
fn looks_like_properties(head: &[u8]) -> bool {
    let mut nonempty = 0u32;
    let mut kv = 0u32;
    let mut sections = 0u32;

    for raw in head.split(|&b| b == b'\n').take(64) {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        let t = trim_left_ws(line);
        if t.is_empty() {
            continue;
        }
        nonempty += 1;

        if t[0] == b'#' || t[0] == b';' {
            continue;
        }
        if t[0] == b'[' && t.last() == Some(&b']') {
            sections += 1;
            continue;
        }
        if is_padded_kv(t) {
            kv += 1;
            continue;
        }
        // non-conforming line -> not a properties file.
        return false;
    }

    nonempty >= 2 && (kv >= 2 || (sections >= 1 && kv >= 1))
}

/// `key = value`: a `[\w.-]+` key, then ` = ` (at least one space each side),
/// then a non-empty value.
fn is_padded_kv(line: &[u8]) -> bool {
    let key_end = line
        .iter()
        .position(|&b| !(b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-'))
        .unwrap_or(line.len());
    if key_end == 0 {
        return false;
    }
    let rest = &line[key_end..];
    // require ` = ` (space, equals, space) at the key boundary.
    rest.starts_with(b" = ") && rest.len() > 3
}

fn looks_like_diff(head: &[u8]) -> bool {
    let mut prev_minus = false;
    for raw in head.split(|&b| b == b'\n').take(16) {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.starts_with(b"diff --git ") || line.starts_with(b"Index: ") {
            return true;
        }
        if line.starts_with(b"@@ -") {
            return true;
        }
        if prev_minus && line.starts_with(b"+++ ") {
            return true;
        }
        prev_minus = line.starts_with(b"--- ");
    }
    false
}

fn looks_like_markdown(head: &[u8]) -> bool {
    let mut score: i32 = 0;
    let mut nonempty_lines: u32 = 0;
    let mut in_code_fence = false;
    let mut first = true;

    for raw in head.split(|&b| b == b'\n').take(64) {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        let trimmed = trim_left_ws(line);

        if first {
            first = false;
            // YAML frontmatter delimiter at very start of file
            if line == b"---" {
                score += 3;
            }
        }

        if trimmed.is_empty() {
            continue;
        }
        nonempty_lines += 1;

        // fenced code block toggles -- count the fence itself, but skip
        // scoring lines inside the fence (they look like prose / code, not md).
        if trimmed.starts_with(b"```") || trimmed.starts_with(b"~~~") {
            score += 2;
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence {
            continue;
        }

        // ATX heading: 1-6 `#`s followed by a space
        if trimmed.starts_with(b"#") {
            let hashes = trimmed.iter().take_while(|&&b| b == b'#').count();
            if (1..=6).contains(&hashes) && trimmed.get(hashes).copied() == Some(b' ') {
                score += 3;
                continue;
            }
        }

        // bullet list marker `- `, `* `, `+ `
        if matches!(trimmed.first(), Some(b'-') | Some(b'*') | Some(b'+'))
            && trimmed.get(1).copied() == Some(b' ')
        {
            score += 1;
        }

        // numbered list `1. `
        let digits = trimmed.iter().take_while(|&&b| b.is_ascii_digit()).count();
        if digits > 0
            && trimmed.get(digits).copied() == Some(b'.')
            && trimmed.get(digits + 1).copied() == Some(b' ')
        {
            score += 1;
        }

        // blockquote
        if trimmed.starts_with(b"> ") || trimmed == b">" {
            score += 1;
        }

        // setext heading underline
        if trimmed.len() >= 3
            && (trimmed.iter().all(|&b| b == b'=') || trimmed.iter().all(|&b| b == b'-'))
        {
            score += 2;
        }

        // inline link `[text](url)` -- crude but effective
        if let Some(p) = find_subseq(trimmed, b"](")
            && trimmed[..p].contains(&b'[')
        {
            score += 2;
        }

        // reference-style link definition `[label]: url`
        if trimmed.starts_with(b"[") && find_subseq(trimmed, b"]: ").is_some() {
            score += 2;
        }
    }

    nonempty_lines >= 2 && score >= 4
}

/// Find a language by name (case-insensitive match on id and display name).
pub fn find_language(name: &str) -> Option<&'static Language> {
    let name_lower = name.to_ascii_lowercase();
    LANGUAGES.iter().find(|lang| {
        lang.id.to_ascii_lowercase() == name_lower || lang.name.to_ascii_lowercase() == name_lower
    })
}

fn trim_left_ws(line: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
        i += 1;
    }
    &line[i..]
}

fn find_subseq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn basename(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Drop a trailing version suffix made of digits and dots, so `python3.11` and
/// `python3` both map to `python`.
fn strip_version_suffix(name: &[u8]) -> &[u8] {
    let mut end = name.len();
    while end > 0 {
        let c = name[end - 1];
        if c.is_ascii_digit() || c == b'.' {
            end -= 1;
        } else {
            break;
        }
    }
    if end == 0 { name } else { &name[..end] }
}

fn interpreter_to_language_id(name: &[u8]) -> Option<&'static str> {
    Some(match name {
        b"dash" | b"ksh" | b"ash" | b"mksh" => "shellscript",
        b"fish" => "fish",
        b"node" | b"deno" | b"bun" => "javascript",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(line: &[u8]) -> Option<&'static str> {
        language_from_shebang(line).map(|l| l.id)
    }

    #[test]
    fn shebang_shellscript() {
        assert_eq!(id(b"#!/bin/sh\n"), Some("shellscript"));
        assert_eq!(id(b"#!/bin/bash"), Some("shellscript"));
        assert_eq!(id(b"#!/usr/bin/env bash\n"), Some("shellscript"));
        assert_eq!(id(b"#!/usr/bin/env -S bash -e\n"), Some("shellscript"));
        assert_eq!(id(b"#!/usr/bin/zsh\n"), Some("shellscript"));
        assert_eq!(id(b"#!/usr/bin/env fish\n"), Some("fish"));
        assert_eq!(id(b"#!/bin/sh\r\n"), Some("shellscript"));
        assert_eq!(id(b"#!/bin/sh\nfollowed by other text"), Some("shellscript"));
        assert_eq!(id(b"#!\t/bin/bash\n"), Some("shellscript"));
        // dash/ksh/ash/mksh go via the alias map, not lang.shebangs
        assert_eq!(id(b"#!/bin/dash\n"), Some("shellscript"));
        assert_eq!(id(b"#!/usr/bin/env ksh\n"), Some("shellscript"));
    }

    #[test]
    fn shebang_other_interpreters() {
        assert_eq!(id(b"#!/usr/bin/env python3\n"), Some("python"));
        assert_eq!(id(b"#!/usr/bin/python3.11\n"), Some("python"));
        assert_eq!(id(b"#!/usr/bin/env -S python3 -u\n"), Some("python"));
        // javascript dialects go via the alias map
        assert_eq!(id(b"#!/usr/bin/env node\n"), Some("javascript"));
        assert_eq!(id(b"#!/usr/bin/env pwsh\n"), Some("powershell"));
    }

    #[test]
    fn shebang_none() {
        assert_eq!(id(b""), None);
        assert_eq!(id(b"hello world\n"), None);
        assert_eq!(id(b"# not a shebang\n"), None);
        assert_eq!(id(b"#!/usr/bin/env\n"), None);
        assert_eq!(id(b"#!/usr/bin/env nonesuch\n"), None);
        assert_eq!(id(b"  #!/bin/sh\n"), None);
    }

    fn content_id(text: &[u8]) -> Option<&'static str> {
        language_from_content(text).map(|l| l.id)
    }

    #[test]
    fn content_markdown_obvious() {
        let s = b"# Title\n\nsome intro text.\n\n## Section\n\nsee [here](https://x).\n";
        assert_eq!(content_id(s), Some("markdown"));
    }

    #[test]
    fn content_markdown_frontmatter() {
        let s = b"---\ntitle: hi\n---\n\n# heading\n\nbody.\n";
        assert_eq!(content_id(s), Some("markdown"));
    }

    #[test]
    fn content_markdown_list_and_quote() {
        let s = b"# notes\n\n- one\n- two\n- three\n\n> quoted bit\n";
        assert_eq!(content_id(s), Some("markdown"));
    }

    #[test]
    fn content_diff_git() {
        let s = b"diff --git a/foo b/foo\nindex 0..1\n--- a/foo\n+++ b/foo\n";
        assert_eq!(content_id(s), Some("diff"));
    }

    #[test]
    fn content_diff_unified() {
        let s = b"--- a/foo\n+++ b/foo\n@@ -1,2 +1,2 @@\n-old\n+new\n";
        assert_eq!(content_id(s), Some("diff"));
    }

    #[test]
    fn content_diff_hunk_first() {
        let s = b"@@ -1,2 +1,2 @@\n-old\n+new\n";
        assert_eq!(content_id(s), Some("diff"));
    }

    #[test]
    fn content_not_markdown() {
        assert_eq!(content_id(b"hello world\n"), None);
        assert_eq!(content_id(b"- one item\n"), None);
        assert_eq!(
            content_id(b"this is just prose with no markdown markers in it at all.\n"),
            None
        );
        assert_eq!(content_id(b""), None);
    }

    #[test]
    fn content_properties() {
        // extensionless config like pixel-goo's `default-params`.
        let s = b"# display\nwidth = 800\nheight = 600\nfps-cap = 60\n";
        assert_eq!(content_id(s), Some("properties"));
        // with a section header.
        let s = b"[core]\nname = goo\n";
        assert_eq!(content_id(s), Some("properties"));
    }

    #[test]
    fn content_not_properties() {
        // shell-style unpadded assignment must NOT sniff as properties.
        assert_eq!(content_id(b"FOO=bar\nBAZ=qux\n"), None);
        // a real script line disqualifies the whole file.
        assert_eq!(content_id(b"width = 800\nif [ -z \"$x\" ]; then\n"), None);
        // single kv is not enough.
        assert_eq!(content_id(b"width = 800\n"), None);
    }

    fn resolved(path: Option<&str>, head: &[u8]) -> &'static str {
        resolve(path.map(Path::new), NO_USER_ASSOCIATIONS, || head.to_vec()).id
    }

    #[test]
    fn resolve_walks_the_chain_and_ends_in_plain() {
        // A single glob hit decides without reading the head.
        let lang = resolve(Some(Path::new("src/main.rs")), NO_USER_ASSOCIATIONS, || {
            panic!("head read for an unambiguous path")
        });
        assert_eq!(lang.id, "rust");
        assert_eq!(resolved(Some("x.yaml"), b"key: value\n"), "yaml");
        assert_eq!(resolved(Some("run"), b"#!/bin/sh\necho hi\n"), "shellscript");
        assert_eq!(resolved(Some("NOTES"), b"# Title\n\n- one\n- two\n- three\n"), "markdown");
        assert!(std::ptr::eq(
            resolve(Some(Path::new("NOTES")), NO_USER_ASSOCIATIONS, || b"just prose\n".to_vec()),
            PLAIN
        ));
        assert!(std::ptr::eq(
            resolve(None, NO_USER_ASSOCIATIONS, || b"just prose\n".to_vec()),
            PLAIN
        ));
    }

    #[test]
    fn rails_project_files_resolve_to_their_own_languages() {
        // `.html.erb` must not read as html, nor `Gemfile.lock` as a Gemfile.
        assert_eq!(resolved(Some("app/views/home/index.html.erb"), b""), "erb");
        assert_eq!(resolved(Some("Gemfile.lock"), b""), "gemfile-lock");
        assert_eq!(resolved(Some("Gemfile"), b""), "ruby");
        assert_eq!(resolved(Some("app/assets/stylesheets/application.css"), b""), "css");
        assert_eq!(resolved(Some(".craftignore"), b""), "ignore");
        assert_eq!(resolved(Some("bin/rails"), b"#!/usr/bin/env ruby\n"), "ruby");
    }

    #[test]
    fn a_user_association_wins_over_the_bundled_one() {
        let python = find_language("python").unwrap();
        let user = [("**/*.rs", python)];
        let lang = resolve(Some(Path::new("src/main.rs")), &user, Vec::new);
        assert_eq!(lang.id, "python");

        // Even against a glob that bundled dialects share and settle by
        // content: the user's choice is final and the head is never read.
        let user = [("**/*.h", python)];
        let lang =
            resolve(Some(Path::new("x.h")), &user, || panic!("head read despite a user match"));
        assert_eq!(lang.id, "python");
    }

    #[test]
    fn find_language_by_id_and_name() {
        assert!(find_language("rust").is_some());
        assert!(find_language("RUST").is_some());
        assert!(find_language("nonesuch").is_none());
    }
}
