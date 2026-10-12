//! What eat reads, behind one surface: a file, stdin, a command's stdout
//! or a directory listing. A [`Source`] is built from a [`SourceSpec`] and
//! produces a [`Load`] on request. The stream asks once and waits
//! ([`Source::load`]); the viewer starts a load and polls for it
//! ([`Source::start`] / [`Source::poll`]), so a slow command never holds
//! up the screen, and asks [`Source::probe`] between loads whether there
//! is anything new to fetch.
//!
//! Nothing here knows about a text buffer or a terminal: content comes
//! back as a reader, as bytes or as listing rows, and the sink decides
//! what to do with it.

use std::io::{self, BufRead as _, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use crate::watch::FileStat;

use super::exec;
use super::listing::{self, Row};
use super::route::SourceSpec;

/// A source, with whatever state its kind keeps between loads.
pub(crate) struct Source {
    kind: Kind,
    /// A synchronous kind's load, parked between `start` and `poll`.
    ready: Option<Result<Load, String>>,
}

enum Kind {
    File {
        path: PathBuf,
        /// What the file looked like when it was last loaded, for `probe`.
        stat: Option<FileStat>,
    },
    Stdin,
    Command {
        argv: Vec<String>,
        /// Length and hash of the previous stdout, so the same output
        /// again is `Unchanged` without keeping a copy of it.
        last: Option<(usize, u64)>,
        run: Option<exec::Run>,
    },
    Dir {
        path: PathBuf,
        plain: bool,
        /// The previous listing, so the same rows again are `Unchanged`.
        last: Option<Vec<Row>>,
    },
}

/// What a load produced.
pub(crate) enum Load {
    New(Snapshot),
    /// Nothing to show again: the same bytes or rows as last time, or a
    /// command that failed without producing anything.
    Unchanged(Option<Failure>),
}

pub(crate) struct Snapshot {
    pub(crate) content: Content,
    /// A command that did not exit 0, output or not.
    pub(crate) failure: Option<Failure>,
}

pub(crate) enum Content {
    /// Streamed; `size_hint` is the total length when it is known.
    Reader {
        reader: Box<dyn Read>,
        size_hint: Option<usize>,
    },
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

    /// One line for a header: the exit code and the last stderr line.
    pub(crate) fn note(&self) -> String {
        exec::failure_note_of(self.status, &self.stderr)
    }
}

/// What a cheap look at the source says, between loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Probe {
    Changed,
    Unchanged,
    /// The source has no cheap probe; the only way to know is a load.
    Unknown,
}

impl Content {
    /// The content as lines, `\n` stripped, as `BufRead::lines` gives
    /// them: invalid utf-8 is an error, not a replacement character.
    pub(crate) fn lines(self) -> io::Result<Vec<String>> {
        match self {
            Content::Reader { reader, .. } => {
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
            SourceSpec::File(path) => Kind::File { path, stat: None },
            SourceSpec::Stdin => Kind::Stdin,
            SourceSpec::Command(argv) => Kind::Command { argv, last: None, run: None },
            SourceSpec::Dir(path) => Kind::Dir { path, plain, last: None },
        };
        Source { kind, ready: None }
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

    /// Whether a load is in flight.
    pub(crate) fn is_running(&self) -> bool {
        matches!(&self.kind, Kind::Command { run: Some(_), .. })
    }

    /// Has the source changed since it was last loaded? A file compares
    /// its stat; a vanished file, or one that has appeared, counts as a
    /// change. Everything else has to be loaded to find out.
    pub(crate) fn probe(&self) -> Probe {
        match &self.kind {
            Kind::File { path, stat } => match (stat, FileStat::from_path(path).ok()) {
                (Some(a), Some(b)) if *a == b => Probe::Unchanged,
                (None, None) => Probe::Unchanged,
                _ => Probe::Changed,
            },
            Kind::Stdin | Kind::Command { .. } | Kind::Dir { .. } => Probe::Unknown,
        }
    }

    /// Read everything, now. `Err` is why nothing could be read: a file
    /// that would not open, a command that would not spawn.
    pub(crate) fn load(&mut self) -> Result<Load, String> {
        match &mut self.kind {
            Kind::Command { argv, last, .. } => {
                let out = exec::run(argv).map_err(|e| e.to_string())?;
                Ok(finish_command(out, last))
            }
            _ => self.load_now(),
        }
    }

    /// Begin a load. A command is spawned and lands through [`poll`]; every
    /// other kind is read here and handed back by the next `poll`. `Err` is
    /// a spawn failure. A command already running is left to finish.
    ///
    /// [`poll`]: Source::poll
    pub(crate) fn start(&mut self) -> Result<(), String> {
        match &mut self.kind {
            Kind::Command { argv, run, .. } => {
                if run.is_none() {
                    *run = Some(exec::start(argv).map_err(|e| e.to_string())?);
                }
                Ok(())
            }
            _ => {
                self.ready = Some(self.load_now());
                Ok(())
            }
        }
    }

    /// `Some` once a started load has landed.
    pub(crate) fn poll(&mut self) -> Option<Result<Load, String>> {
        match &mut self.kind {
            Kind::Command { run, last, .. } => {
                let out = match run.as_mut()?.poll() {
                    Ok(None) => return None,
                    Ok(Some(out)) => Ok(out),
                    Err(e) => Err(e.to_string()),
                };
                *run = None;
                Some(out.map(|out| finish_command(out, last)))
            }
            _ => self.ready.take(),
        }
    }

    /// [`poll`], giving a command up to `grace` to return first, so a quick
    /// one lands before the next paint. A synchronous kind lands at once.
    ///
    /// [`poll`]: Source::poll
    pub(crate) fn wait_up_to(&mut self, grace: Duration) -> Option<Result<Load, String>> {
        let until = std::time::Instant::now() + grace;
        loop {
            if let Some(load) = self.poll() {
                return Some(load);
            }
            if !self.is_running() || std::time::Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn load_now(&mut self) -> Result<Load, String> {
        let content = match &mut self.kind {
            Kind::File { path, stat } => {
                // The stat is taken whatever the open says: a file that is
                // gone is `None`, and stays `Unchanged` until it is back.
                *stat = FileStat::from_path(path).ok();
                let file = std::fs::File::open(&*path).map_err(|e| e.to_string())?;
                let size_hint = file.metadata().ok().map(|m| m.len() as usize);
                Content::Reader { reader: Box::new(file), size_hint }
            }
            Kind::Stdin => {
                Content::Reader { reader: Box::new(io::stdin().lock()), size_hint: None }
            }
            Kind::Dir { path, plain, last } => {
                let rows = listing::render(path, *plain).map_err(|e| e.to_string())?;
                if last.as_ref() == Some(&rows) {
                    return Ok(Load::Unchanged(None));
                }
                *last = Some(rows.clone());
                Content::Rows(rows)
            }
            Kind::Command { .. } => unreachable!("a command is loaded through its run"),
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
    fn a_file_is_a_reader_with_a_stat_to_probe_against() {
        let dir = std::env::temp_dir().join(format!("eat-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let mut source = Source::new(SourceSpec::File(path.clone()), false);
        assert_eq!(source.path(), Some(path.as_path()));
        assert_eq!(source.label(), path.display().to_string());
        assert_eq!(source.probe(), Probe::Changed, "never loaded, so anything is news");
        let Ok(Load::New(snap)) = source.load() else { panic!("a readable file loads") };
        assert!(snap.failure.is_none());
        let Content::Reader { size_hint, reader } = snap.content else { panic!("a file streams") };
        assert_eq!(size_hint, Some(8));
        assert_eq!(Content::Reader { reader, size_hint }.lines().unwrap(), ["one", "two"]);
        assert_eq!(source.probe(), Probe::Unchanged);
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        assert_eq!(source.probe(), Probe::Changed);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(source.probe(), Probe::Changed, "a vanished file is a change");
        assert!(source.load().is_err());
        assert_eq!(source.probe(), Probe::Unchanged, "and stays gone until it is back");
        std::fs::write(&path, "back\n").unwrap();
        assert_eq!(source.probe(), Probe::Changed);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_synchronous_source_lands_on_the_poll_after_start() {
        let mut source = Source::new(SourceSpec::File(PathBuf::from("/nonexistent/x")), false);
        assert!(source.poll().is_none(), "nothing started yet");
        source.start().unwrap();
        assert!(!source.is_running());
        assert!(matches!(source.poll(), Some(Err(_))));
        assert!(source.poll().is_none(), "taken once");
    }

    #[test]
    fn a_command_is_polled_until_it_lands() {
        let mut source = command(&["sh", "-c", "sleep 0.2; echo late; exit 2"]);
        assert_eq!(source.probe(), Probe::Unknown);
        source.start().unwrap();
        assert!(source.is_running());
        assert!(source.poll().is_none(), "polled as finished before it could be");
        source.start().unwrap();
        assert!(source.is_running(), "a second start leaves the run alone");
        let out = loop {
            if let Some(out) = source.poll() {
                break out;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(!source.is_running());
        let Ok(Load::New(snap)) = out else { panic!("output with a failure loads") };
        assert_eq!(snap.failure.unwrap().note(), "exit 2");
        assert!(matches!(snap.content, Content::Bytes(ref b) if b == b"late\n"));
        // a quick command lands inside the grace.
        let mut quick = command(&["echo", "now"]);
        quick.start().unwrap();
        let out = quick.wait_up_to(Duration::from_secs(2)).expect("echo returns within 2s");
        assert_eq!(bytes(out), b"now\n");
        assert!(!quick.is_running());
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
        assert!(command(&["/nonexistent/eat-test-program"]).start().is_err());
        assert!(command(&[]).load().is_err());
    }

    #[test]
    fn a_directory_is_rows_and_the_same_listing_again_is_unchanged() {
        let dir = std::env::temp_dir().join(format!("eat-source-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.rs"), "").unwrap();
        let mut source = Source::new(SourceSpec::Dir(dir.clone()), true);
        assert_eq!(source.probe(), Probe::Unknown, "a listing has to be made to compare");
        let Ok(Load::New(snap)) = source.load() else { panic!("a readable dir loads") };
        assert_eq!(snap.content.lines().unwrap(), ["sub/", "a.rs"]);
        assert!(matches!(source.load(), Ok(Load::Unchanged(None))));
        std::fs::write(dir.join("b.rs"), "").unwrap();
        let Ok(Load::New(snap)) = source.load() else { panic!("a new entry is a new listing") };
        assert_eq!(snap.content.lines().unwrap(), ["sub/", "a.rs", "b.rs"]);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(source.load().is_err());
    }
}
