//! Language detection for the eat persona: an explicit `-l` wins, then the
//! shared chain in `lsh_defs::detect::resolve` (globs with dialect
//! disambiguation, shebang, content sniff, plain text).

use std::fs::File;
use std::io::Read as _;
use std::path::Path;

use lsh::runtime::Language;
use lsh_defs::detect::{NO_USER_ASSOCIATIONS, find_language, resolve};

/// Join the first 64 lines of a `Vec<String>` (BufRead::lines() strips `\n`)
/// back into a contiguous byte buffer suitable for the shared `lsh_defs::detect`
/// fns, which take `head: &[u8]`. Allocates -- only called once per file on the
/// detection path.
pub(crate) fn head_bytes(lines: &[String]) -> Vec<u8> {
    let take = lines.iter().take(64);
    let mut out = Vec::with_capacity(take.clone().map(|l| l.len() + 1).sum());
    for l in take {
        out.extend_from_slice(l.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Read up to 4 KiB from the start of `path` for content-based language
/// detection. Best-effort: returns empty on any error.
pub(crate) fn read_head(path: &Path) -> Vec<u8> {
    let mut buf = vec![0u8; 4096];
    match File::open(path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => {
            buf.truncate(n);
            buf
        }
        Err(_) => Vec::new(),
    }
}

/// Resolve the language for a path: an explicit override, else the shared
/// detection chain ending in plain text.
///
/// Returns `Err` with the unknown name when `-l` names a language that
/// does not exist -- callers turn that into a usage error.
pub(crate) fn resolve_language(
    path: &Path,
    override_name: Option<&str>,
) -> Result<&'static Language, String> {
    resolve_language_with(Some(path), override_name, || read_head(path))
}

/// [`resolve_language`] for content that may have no path, such as a
/// command's output: `head` is only consulted when nothing else settles it.
pub(crate) fn resolve_language_with(
    path: Option<&Path>,
    override_name: Option<&str>,
    head: impl FnOnce() -> Vec<u8>,
) -> Result<&'static Language, String> {
    match override_name {
        Some(name) => find_language(name).ok_or_else(|| name.to_string()),
        None => Ok(resolve(path, NO_USER_ASSOCIATIONS, head)),
    }
}
