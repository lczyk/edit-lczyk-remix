//! What eat reads, behind one surface: a file, stdin, a command's stdout
//! or a directory listing. A [`Source`] is built from a [`SourceSpec`] and
//! produces a [`Load`] on request ([`Source::load`]).
//!
//! Nothing here knows about a text buffer or a terminal: content comes
//! back as a reader, as bytes or as listing rows, and the sink decides
//! what to do with it.

use std::io::{self, BufRead as _, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use super::exec;
use super::listing::{self, Row};
use super::route::SourceSpec;

/// A source, with whatever state its kind keeps between loads.
pub(crate) struct Source {
    kind: Kind,
}

enum Kind {
    File {
        path: PathBuf,
    },
    Stdin,
    Command {
        argv: Vec<String>,
        /// Length and hash of the previous stdout, so the same output
        /// again is `Unchanged` without keeping a copy of it.
        last: Option<(usize, u64)>,
    },
    Dir {
        path: PathBuf,
        plain: bool,
    },
}

/// What a load produced.
pub(crate) enum Load {
    New(Snapshot),
    /// Nothing to show again: the same bytes as last time, or a command
    /// that failed without producing anything.
    Unchanged(Option<Failure>),
}

pub(crate) struct Snapshot {
    pub(crate) content: Content,
    /// A command that did not exit 0, output or not.
    pub(crate) failure: Option<Failure>,
}

pub(crate) enum Content {
    Reader(Box<dyn Read>),
    Bytes(Vec<u8>),
    /// A directory listing, one row per entry.
    Rows(Vec<Row>),
}

/// What a command's nonzero exit left behind.
pub(crate) struct Failure {
    pub(crate) status: ExitStatus,
    pub(crate) stderr: Vec<u8>,
}

impl Failure {
    fn of(out: &exec::Output) -> Option<Failure> {
        (!out.status.success()).then(|| Failure { status: out.status, stderr: out.stderr.clone() })
    }
}

impl Content {
    /// The content as lines, `\n` stripped, as `BufRead::lines` gives
    /// them: invalid utf-8 is an error, not a replacement character.
    pub(crate) fn lines(self) -> io::Result<Vec<String>> {
        match self {
            Content::Reader(reader) => {
                BufReader::with_capacity(128 * 1024, reader).lines().collect()
            }
            Content::Bytes(bytes) => bytes.lines().collect(),
            Content::Rows(rows) => Ok(rows.into_iter().map(|r| r.text).collect()),
        }
    }
}

impl Source {
    /// `plain` is the listing's names-only form; nothing else reads it.
    pub(crate) fn new(spec: SourceSpec, plain: bool) -> Source {
        let kind = match spec {
            SourceSpec::File(path) => Kind::File { path },
            SourceSpec::Stdin => Kind::Stdin,
            SourceSpec::Command(argv) => Kind::Command { argv, last: None },
            SourceSpec::Dir(path) => Kind::Dir { path, plain },
        };
        Source { kind }
    }

    /// The source as a header or an error message names it.
    pub(crate) fn label(&self) -> String {
        match &self.kind {
            Kind::File { path, .. } | Kind::Dir { path, .. } => path.display().to_string(),
            Kind::Stdin => "stdin".to_string(),
            Kind::Command { argv, .. } => exec::label(argv),
        }
    }

    /// The path on disk, for language detection and the gutter baseline.
    pub(crate) fn path(&self) -> Option<&Path> {
        match &self.kind {
            Kind::File { path, .. } | Kind::Dir { path, .. } => Some(path),
            Kind::Stdin | Kind::Command { .. } => None,
        }
    }

    /// Read everything, now. `Err` is why nothing could be read: a file
    /// that would not open, a command that would not spawn.
    pub(crate) fn load(&mut self) -> Result<Load, String> {
        let content = match &mut self.kind {
            Kind::File { path } => {
                let file = std::fs::File::open(&*path).map_err(|e| e.to_string())?;
                Content::Reader(Box::new(file))
            }
            Kind::Stdin => Content::Reader(Box::new(io::stdin().lock())),
            Kind::Command { argv, last } => {
                let out = exec::run(argv).map_err(|e| e.to_string())?;
                return Ok(finish_command(out, last));
            }
            Kind::Dir { path, plain } => {
                Content::Rows(listing::render(path, *plain).map_err(|e| e.to_string())?)
            }
        };
        Ok(Load::New(Snapshot { content, failure: None }))
    }
}

/// A returned command into a load. A failure without output is not a
/// load: the previous content stands and the failure is the note. One
/// that exits nonzero with output (`diff`, `grep`) is loaded, and still
/// carries the failure. The same bytes as last time are `Unchanged`, so
/// a live view does not repaint for nothing.
fn finish_command(out: exec::Output, last: &mut Option<(usize, u64)>) -> Load {
    let failure = Failure::of(&out);
    if out.stdout.is_empty() && failure.is_some() {
        return Load::Unchanged(failure);
    }
    let fingerprint = (out.stdout.len(), crate::hash::hash(0, &out.stdout));
    if *last == Some(fingerprint) {
        return Load::Unchanged(failure);
    }
    *last = Some(fingerprint);
    Load::New(Snapshot { content: Content::Bytes(out.stdout), failure })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn command(parts: &[&str]) -> Source {
        Source::new(SourceSpec::Command(argv(parts)), false)
    }

    fn bytes(load: Result<Load, String>) -> Vec<u8> {
        match load {
            Ok(Load::New(Snapshot { content: Content::Bytes(b), .. })) => b,
            _ => panic!("not a new load of bytes"),
        }
    }

    #[test]
    fn a_file_is_a_reader_and_a_missing_one_is_an_error() {
        let dir = std::env::temp_dir().join(format!("eat-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let mut source = Source::new(SourceSpec::File(path.clone()), false);
        assert_eq!(source.path(), Some(path.as_path()));
        assert_eq!(source.label(), path.display().to_string());
        let Ok(Load::New(snap)) = source.load() else { panic!("a readable file loads") };
        assert!(snap.failure.is_none());
        assert_eq!(snap.content.lines().unwrap(), ["one", "two"]);
        std::fs::remove_file(&path).unwrap();
        assert!(source.load().is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_command_loads_with_its_failure_and_the_same_output_again_is_unchanged() {
        let mut source = command(&["sh", "-c", "echo out; echo err >&2; exit 3"]);
        assert_eq!(source.label(), "sh -c 'echo out; echo err >&2; exit 3'");
        assert!(source.path().is_none());
        let Ok(Load::New(snap)) = source.load() else { panic!("output with a failure loads") };
        let failure = snap.failure.expect("exit 3 is a failure");
        assert_eq!(failure.status.code(), Some(3));
        assert_eq!(failure.stderr, b"err\n");
        assert!(matches!(snap.content, Content::Bytes(ref b) if b == b"out\n"));
        // the second run: same bytes, so nothing new, but the failure
        // is still reported.
        match source.load() {
            Ok(Load::Unchanged(Some(f))) => assert_eq!(f.status.code(), Some(3)),
            _ => panic!("the same output again is unchanged"),
        }
    }

    #[test]
    fn a_failure_without_output_does_not_replace_what_was_there() {
        let flag = std::env::temp_dir().join(format!("eat-source-flag-{}", std::process::id()));
        let _ = std::fs::remove_file(&flag);
        let script = format!("if [ -e '{}' ]; then exit 4; else echo hi; fi", flag.display());
        let mut source = command(&["sh", "-c", &script]);
        assert_eq!(bytes(source.load()), b"hi\n");
        std::fs::write(&flag, "").unwrap();
        let again = source.load();
        std::fs::remove_file(&flag).unwrap();
        match again {
            Ok(Load::Unchanged(Some(f))) => assert_eq!(f.status.code(), Some(4)),
            _ => panic!("an empty failure is unchanged with the failure"),
        }
    }

    #[test]
    fn a_missing_program_is_an_error() {
        assert!(command(&["/nonexistent/eat-test-program"]).load().is_err());
        assert!(command(&[]).load().is_err());
    }

    #[test]
    fn a_directory_is_rows() {
        let dir = std::env::temp_dir().join(format!("eat-source-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.rs"), "").unwrap();
        let mut source = Source::new(SourceSpec::Dir(dir.clone()), true);
        let Ok(Load::New(snap)) = source.load() else { panic!("a readable dir loads") };
        assert_eq!(snap.content.lines().unwrap(), ["sub/", "a.rs"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
