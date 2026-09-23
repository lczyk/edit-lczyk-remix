//! Subprocess wrapper around `git`: baselines for the gutter diff, and
//! per-path status for eat's directory listing. No libgit2; missing
//! binary degrades to a graceful noop.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{io, str};

#[derive(Debug, Clone)]
pub struct GitInfo {
    pub repo_root: PathBuf,
    pub rel_path: String,
}

/// Locate the git repository containing `path` and return its toplevel +
/// the path of `path` relative to that toplevel.
pub fn locate(path: &Path) -> io::Result<GitInfo> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let out = Command::new("git")
        .arg("-C")
        .arg(parent)
        .args(["rev-parse", "--show-toplevel"])
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "not a git repo"));
    }
    let root_str = str::from_utf8(&out.stdout)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 toplevel"))?
        .trim()
        .to_string();
    let repo_root = canonicalise(Path::new(&root_str))?;
    let abs = canonicalise(path)?;
    let rel = abs
        .strip_prefix(&repo_root)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "path outside repo"))?;
    let rel_path = rel
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 rel path"))?
        .to_string();
    Ok(GitInfo { repo_root, rel_path })
}

/// Read the baseline blob for `info`. Tries `HEAD:<rel>` first, then
/// `:<rel>` (the staged blob, useful for new-files-staged).
pub fn read_baseline(info: &GitInfo) -> io::Result<Vec<u8>> {
    if let Ok(bytes) = show_blob(&info.repo_root, &format!("HEAD:{}", info.rel_path)) {
        return Ok(bytes);
    }
    show_blob(&info.repo_root, &format!(":{}", info.rel_path))
}

fn show_blob(repo_root: &Path, spec: &str) -> io::Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["show", spec])
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("{spec} missing")));
    }
    Ok(out.stdout)
}

/// `git status --porcelain` for everything under `dir`, ignored paths
/// included, as `(XY, path relative to dir)`. An empty path means `dir`
/// itself carries the status: it sits in a wholly untracked or ignored
/// tree, which git reports as the tree's root alone.
pub fn dir_status(dir: &Path) -> io::Result<Vec<([u8; 2], String)>> {
    let prefix = git_stdout(dir, &["rev-parse", "--show-prefix"])?;
    let prefix = str::from_utf8(&prefix)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 prefix"))?
        .trim_end_matches('\n')
        .to_string();
    let out = git_stdout(dir, &["status", "--porcelain=v1", "-z", "--ignored", "--", "."])?;
    Ok(parse_porcelain_z(&out, &prefix))
}

fn git_stdout(dir: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).stderr(Stdio::null()).output()?;
    if !out.status.success() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "not a git repo"));
    }
    Ok(out.stdout)
}

/// Porcelain paths are relative to the toplevel whatever the cwd, so
/// `prefix` (the dir's own path from the toplevel) is stripped off.
fn parse_porcelain_z(out: &[u8], prefix: &str) -> Vec<([u8; 2], String)> {
    let mut entries = Vec::new();
    let mut records = out.split(|&b| b == 0);
    while let Some(rec) = records.next() {
        if rec.len() < 4 {
            continue;
        }
        let xy = [rec[0], rec[1]];
        // A rename or copy is followed by a second record holding the
        // source path.
        if matches!(xy[0], b'R' | b'C') {
            records.next();
        }
        let path = String::from_utf8_lossy(&rec[3..]);
        if let Some(rel) = path.strip_prefix(prefix) {
            entries.push((xy, rel.to_string()));
        } else if path.ends_with('/') && prefix.starts_with(&*path) {
            entries.push((xy, String::new()));
        }
    }
    entries
}

fn canonicalise(p: &Path) -> io::Result<PathBuf> {
    // canonicalize resolves /var -> /private/var on macos so the strip_prefix
    // in locate() lines up.
    std::fs::canonicalize(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path that cannot resolve. `locate` canonicalises both the file and
    /// the toplevel, so this fails whether or not `git` is on $PATH and
    /// whether or not the temp dir happens to sit inside a repository.
    fn missing_path() -> PathBuf {
        std::env::temp_dir().join(format!("gutter-no-such-file-{}", std::process::id()))
    }

    #[test]
    fn locating_a_missing_file_is_an_error_not_a_panic() {
        // The whole feature is best-effort: no repo, no git binary, or a path
        // outside the worktree all have to come back as a plain Err so the
        // caller can fall back to no marks.
        assert!(locate(&missing_path()).is_err());
    }

    #[test]
    fn a_baseline_for_a_missing_file_is_an_error() {
        let info = GitInfo {
            repo_root: std::env::temp_dir(),
            rel_path: format!("gutter-no-such-file-{}", std::process::id()),
        };
        assert!(read_baseline(&info).is_err());
    }

    #[test]
    fn loading_a_baseline_for_a_missing_file_yields_no_marks() {
        // The state constructor swallows every failure mode above; a `None`
        // here is what suppresses the margin rather than crashing the editor.
        let state = crate::gutter_diff::BaselineState::load(&missing_path());
        assert!(state.bytes.is_none());
    }

    #[test]
    fn porcelain_paths_come_back_relative_to_the_dir() {
        let out = b" M sub/a.rs\0?? sub/new/\0R  sub/b.rs\0sub/old.rs\0!! other/x\0";
        let got = parse_porcelain_z(out, "sub/");
        assert_eq!(
            got,
            vec![
                (*b" M", "a.rs".to_string()),
                (*b"??", "new/".to_string()),
                (*b"R ", "b.rs".to_string()),
            ]
        );
    }

    #[test]
    fn a_wholly_ignored_dir_reports_itself_with_an_empty_path() {
        // `git status -- .` from inside an ignored dir names the dir, not its
        // children.
        let got = parse_porcelain_z(b"!! target/\0", "target/");
        assert_eq!(got, vec![(*b"!!", String::new())]);
        let got = parse_porcelain_z(b"!! target/\0", "target/debug/deps/");
        assert_eq!(got, vec![(*b"!!", String::new())]);
    }

    #[test]
    fn a_dir_outside_any_repo_has_no_status() {
        assert!(dir_status(&missing_path()).is_err());
    }

    #[test]
    fn this_repo_locates_and_has_a_baseline() {
        // Positive direction, skipped rather than failed when the environment
        // cannot provide it -- a source tarball with no .git, or no git binary.
        let here = Path::new(file!());
        if !here.exists() {
            return;
        }
        let Ok(info) = locate(here) else {
            return;
        };
        assert!(info.repo_root.is_absolute());
        assert!(info.rel_path.ends_with("git.rs"), "rel_path={}", info.rel_path);
        assert!(!info.rel_path.starts_with('/'));
        // This file is committed, so HEAD:<rel> resolves.
        let bytes = read_baseline(&info).expect("committed file has a baseline");
        assert!(bytes.starts_with(b"//!"), "baseline did not look like this file");
    }
}
