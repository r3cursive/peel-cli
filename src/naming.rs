//! Naming outputs.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::fsutil::is_real_dir;
use crate::provenance::provenance;

/// `<name>_x`, then `<name>_x2` ... `<name>_x9` when the stem name is taken.
const FALLBACK_NAMES: usize = 9;

/// Tarball suffixes stripped whole when naming the output directory.
const COMPOUND_SUFFIXES: &[&str] = &[
    ".tar.gz",
    ".tar.bz2",
    ".tar.xz",
    ".tar.zst",
    ".tar.lz4",
    ".tar.lzma",
    ".tar.lz",
    ".tar.z",
    ".tgz",
    ".tbz2",
    ".tbz",
    ".txz",
    ".tzst",
    ".taz",
    ".tlz",
];

/// Extensions that promise an archive. When no backend opens such a file, peel
/// reports it instead of passing over it in silence.
const ARCHIVE_EXTS: &[&str] = &[
    "zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "tbz", "tbz2", "xz", "txz", "zst", "tzst",
    "lz4", "lzma", "lz", "lzo", "cab", "arj", "lzh", "cpio", "rpm", "deb", "iso", "z", "001",
];

pub(crate) struct Names {
    pub(crate) parent: PathBuf,
    pub(crate) name: OsString,
    /// `foo.zip` -> `foo`, then `foo.zip_x`, `foo.zip_x2` ... in order of preference.
    pub(crate) candidates: Vec<PathBuf>,
}

impl Names {
    pub(crate) fn new(path: &Path, name: &OsStr) -> Self {
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let mut candidates: Vec<PathBuf> =
            stem_of(name).map(|s| parent.join(s)).into_iter().collect();
        for n in 1..=FALLBACK_NAMES {
            let mut alt = name.to_os_string();
            alt.push(if n == 1 {
                "_x".to_owned()
            } else {
                format!("_x{n}")
            });
            candidates.push(parent.join(alt));
        }
        Names {
            parent,
            name: name.to_os_string(),
            candidates,
        }
    }

    /// This input's output from an earlier run: a candidate directory whose
    /// provenance names this input.
    pub(crate) fn existing(&self) -> Option<PathBuf> {
        self.candidates
            .iter()
            .find(|c| is_real_dir(c) && provenance(c).as_deref() == Some(self.name.as_os_str()))
            .cloned()
    }

    /// Reserve a free output name for this run.
    pub(crate) fn claim(&self, claims: &Mutex<HashSet<PathBuf>>) -> Option<PathBuf> {
        let mut claims = claims.lock().unwrap();
        self.candidates
            .iter()
            .find(|c| fs::symlink_metadata(c).is_err() && claims.insert((*c).clone()))
            .cloned()
    }
}

fn stem_of(name: &OsStr) -> Option<OsString> {
    let b = name.as_bytes();
    let lower = b.to_ascii_lowercase();
    let stem = match COMPOUND_SUFFIXES
        .iter()
        .find(|s| lower.ends_with(s.as_bytes()))
    {
        Some(suffix) => &b[..b.len() - suffix.len()],
        None => {
            let dot = b.iter().rposition(|&c| c == b'.')?;
            let ext_len = b.len() - dot - 1;
            if dot == 0 || !(1..=10).contains(&ext_len) {
                return None;
            }
            &b[..dot]
        }
    };
    // `..zip` would otherwise name the parent directory itself.
    (!stem.is_empty() && stem != b"." && stem != b"..").then(|| OsString::from_vec(stem.to_vec()))
}

pub(crate) fn archive_ext(name: &OsStr) -> Option<String> {
    let b = name.as_bytes();
    let dot = b.iter().rposition(|&c| c == b'.')?;
    let ext = String::from_utf8_lossy(&b[dot + 1..]).to_ascii_lowercase();
    ARCHIVE_EXTS.contains(&ext.as_str()).then_some(ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stem(s: &str) -> Option<String> {
        stem_of(OsStr::new(s)).map(|o| o.to_string_lossy().into_owned())
    }

    #[test]
    fn stems() {
        assert_eq!(stem("foo.zip").as_deref(), Some("foo"));
        assert_eq!(stem("a.tar.gz").as_deref(), Some("a"));
        assert_eq!(stem("A.TGZ").as_deref(), Some("A"));
        assert_eq!(stem("notes.txt.gz").as_deref(), Some("notes.txt"));
        assert_eq!(stem("a.7z.001").as_deref(), Some("a.7z"));
        assert_eq!(stem("blob"), None);
        assert_eq!(stem(".bashrc"), None);
        assert_eq!(stem("weird.extension-too-long"), None);
        assert_eq!(stem(".tar.gz"), None);
        assert_eq!(stem("..zip"), None);
        assert_eq!(stem("...zip"), None);
        assert_eq!(stem("..tar.gz"), None);
    }
}
