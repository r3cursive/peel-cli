//! Filesystem helpers. Every tree traversal here is iterative, so an archive
//! that unpacks into thousands of nested directories cannot exhaust the stack.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub(crate) fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// Remove `p` (a file, or a directory and everything in it) without following
/// symlinks. Only ever called on paths this run created.
pub(crate) fn remove(p: &Path) {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => {
            if fs::remove_dir_all(p).is_err() {
                // Extracted entries can be read-only; open them up and try again.
                make_writable(p);
                let _ = fs::remove_dir_all(p);
            }
        }
        Ok(_) => {
            let _ = fs::remove_file(p);
        }
        Err(_) => {}
    }
}

fn make_writable(dir: &Path) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let _ = fs::set_permissions(&d, fs::Permissions::from_mode(0o700));
        if let Ok(entries) = fs::read_dir(&d) {
            for e in entries.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(e.path());
                }
            }
        }
    }
}

/// What an extraction has written so far.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub(crate) struct Usage {
    /// Apparent size of every non-directory entry.
    pub(crate) bytes: u64,
    /// Files, directories and links below the top.
    pub(crate) entries: u64,
}

/// Apparent size of everything under `p`, without following symlinks. `None`
/// when some directory in it cannot be read, so callers enforcing a cap fail
/// closed instead of seeing zero.
#[cfg(test)]
pub(crate) fn tree_size(p: &Path) -> Option<u64> {
    tree_usage(p).map(|u| u.bytes)
}

/// Bytes and entries under `p`, without following symlinks. `None` when some
/// directory in it cannot be read, so callers enforcing a cap fail closed.
pub(crate) fn tree_usage(p: &Path) -> Option<Usage> {
    let m = match fs::symlink_metadata(p) {
        Ok(m) => m,
        // Nothing there yet (the extractor has not started writing).
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Some(Usage::default()),
        Err(_) => return None,
    };
    if !m.is_dir() {
        return Some(Usage {
            bytes: m.len(),
            entries: 1,
        });
    }
    let mut total: u64 = 0;
    let mut entries: u64 = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let listing = match fs::read_dir(&d) {
            Ok(listing) => listing,
            // An extractor removed this directory between listing and reading
            // (binwalk deletes a failed extractor's dir): gone, not unreadable.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        for e in listing {
            let e = e.ok()?;
            match e.file_type() {
                Ok(t) if t.is_dir() => {
                    entries += 1;
                    stack.push(e.path());
                }
                Ok(_) => {
                    entries += 1;
                    total = total.saturating_add(e.metadata().map_or(0, |m| m.len()));
                }
                // Entries vanish while an extractor works (its own temp files).
                Err(_) => {}
            }
        }
    }
    Some(Usage {
        bytes: total,
        entries,
    })
}

/// True when `p` holds at least one entry that is not a directory.
pub(crate) fn has_content(p: &Path) -> bool {
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(_) => return true,
                Err(_) => {}
            }
        }
    }
    false
}

/// The only entry in `dir`, if it is a single regular file.
pub(crate) fn sole_file(dir: &Path) -> Option<PathBuf> {
    let mut it = fs::read_dir(dir).ok()?.flatten();
    let first = it.next()?;
    (it.next().is_none() && first.file_type().ok()?.is_file()).then(|| first.path())
}

fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Ok(mut fa), Ok(mut fb)) = (fs::File::open(a), fs::File::open(b)) else {
        return false;
    };
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let (Ok(na), Ok(nb)) = (read_full(&mut fa, &mut ba), read_full(&mut fb, &mut bb)) else {
            return false;
        };
        if na != nb || ba[..na] != bb[..nb] {
            return false;
        }
        if na == 0 {
            return true;
        }
    }
}

fn read_full(f: &mut fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Delete carved files that are byte-for-byte copies of the input, so a carver
/// that re-emits its input cannot loop until the depth cap.
pub(crate) fn prune_copies(dir: &Path, input: &Path) {
    let Ok(len) = fs::metadata(input).map(|m| m.len()) else {
        return;
    };
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(t) = e.file_type() else { continue };
            let p = e.path();
            if t.is_dir() {
                stack.push(p);
            } else if t.is_file()
                && e.metadata().is_ok_and(|m| m.len() == len)
                && same_bytes(&p, input)
            {
                let _ = fs::remove_file(&p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_size_fails_closed_on_unreadable_dirs() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tmp")
            .join(format!("fsutil-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("locked")).unwrap();
        fs::write(dir.join("a"), b"12345").unwrap();
        assert_eq!(tree_size(&dir), Some(5));
        assert_eq!(tree_size(&dir.join("missing")), Some(0));
        fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        // Root can read anything, so the unreadable case only holds for a normal user.
        let readable_anyway = fs::read_dir(dir.join("locked")).is_ok();
        if !readable_anyway {
            assert_eq!(tree_size(&dir), None);
        }
        fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o700)).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
