//! Provenance: which input made an output directory.

use std::ffi::{CStr, OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use crate::sys::{ENOTSUP, get_xattr, set_xattr};

/// Extended attribute that names the input an output directory came from.
const PROVENANCE_XATTR: &CStr = c"user.peel.src";
/// The same record as a file, only on filesystems without user xattrs.
pub(crate) const PROVENANCE_MARKER: &str = ".peel-src";

/// Record on `dir` that it was made from the input called `source`. Falls back
/// to a marker file only when the filesystem has no user xattrs.
pub(crate) fn set_provenance(dir: &Path, source: &OsStr) -> io::Result<()> {
    let value = source.as_bytes();
    match set_xattr(dir, PROVENANCE_XATTR, value) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(ENOTSUP) => write_marker(dir, value),
        Err(e) => Err(e),
    }
}

/// Create the marker file. `create_new` fails on anything already at that name,
/// a symlink included, so an archive that shipped its own `.peel-src` cannot
/// supply the record or redirect the write.
fn write_marker(dir: &Path, value: &[u8]) -> io::Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(PROVENANCE_MARKER))?;
    f.write_all(value)
}

/// Which input `dir` was made from, if peel made it.
pub(crate) fn provenance(dir: &Path) -> Option<OsString> {
    let mut buf = vec![0u8; 4096];
    match get_xattr(dir, PROVENANCE_XATTR, &mut buf) {
        Ok(n) => {
            buf.truncate(n);
            return Some(OsString::from_vec(buf));
        }
        // The marker file counts only where the filesystem cannot hold xattrs. On
        // one that can, a `.peel-src` file could have come out of a hostile archive.
        Err(e) if e.raw_os_error() == Some(ENOTSUP) => {}
        Err(_) => return None,
    }
    let marker = dir.join(PROVENANCE_MARKER);
    let meta = fs::symlink_metadata(&marker).ok()?;
    (meta.is_file() && meta.len() <= 4096)
        .then(|| fs::read(&marker).ok().map(OsString::from_vec))?
}

/// Extended attribute that proves peel made a work dir: the run's token.
const TEMP_XATTR: &CStr = c"user.peel.tmp";

/// Mark a fresh work dir as peel's own, with the run token that is also in its
/// name. Best effort: on a filesystem without xattrs there is no proof, and a
/// leftover there is reported, never removed.
pub(crate) fn mark_temp(dir: &Path, token: &str) {
    let _ = set_xattr(dir, TEMP_XATTR, token.as_bytes());
}

/// The run token on a work dir, if peel marked it. 7-Zip and binwalk's
/// extractors do not restore user xattrs, so a directory that came out of an
/// archive never carries one.
pub(crate) fn temp_mark(dir: &Path) -> Option<String> {
    let mut buf = [0u8; 64];
    let n = get_xattr(dir, TEMP_XATTR, &mut buf).ok()?;
    String::from_utf8(buf[..n].to_vec()).ok()
}

/// True when `dir`'s provenance comes from its marker file, so the walk can stay
/// quiet about that one `.peel-src` and report any other.
pub(crate) fn marker_in_use(dir: &Path) -> bool {
    // A zero-length query only reports the value's size.
    matches!(get_xattr(dir, PROVENANCE_XATTR, &mut []), Err(e) if e.raw_os_error() == Some(ENOTSUP))
        && provenance(dir).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tmp")
            .join(format!("prov-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn provenance_round_trip() {
        let dir = scratch("rt");
        assert_eq!(provenance(&dir), None);
        set_provenance(&dir, OsStr::new("a.zip")).unwrap();
        assert_eq!(provenance(&dir).as_deref(), Some(OsStr::new("a.zip")));
        assert!(!marker_in_use(&dir));
        // On a filesystem with xattrs, a planted marker file is not believed.
        let other = dir.join("planted");
        fs::create_dir(&other).unwrap();
        fs::write(other.join(PROVENANCE_MARKER), "a.zip").unwrap();
        assert_eq!(provenance(&other), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_never_overwrites_or_follows() {
        let dir = scratch("marker");
        let target = dir.join("target.txt");
        fs::write(&target, b"untouched").unwrap();
        let shipped = dir.join("shipped");
        fs::create_dir(&shipped).unwrap();
        std::os::unix::fs::symlink(&target, shipped.join(PROVENANCE_MARKER)).unwrap();
        assert!(write_marker(&shipped, b"evil.zip").is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        let fresh = dir.join("fresh");
        fs::create_dir(&fresh).unwrap();
        write_marker(&fresh, b"a.zip").unwrap();
        assert!(write_marker(&fresh, b"b.zip").is_err());
        assert_eq!(fs::read(fresh.join(PROVENANCE_MARKER)).unwrap(), b"a.zip");
        let _ = fs::remove_dir_all(&dir);
    }
}
