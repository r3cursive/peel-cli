//! Finding the backends.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub(crate) fn find_program(env_var: &str, names: &[&str]) -> Option<PathBuf> {
    if let Some(v) = env::var_os(env_var).filter(|v| !v.is_empty()) {
        let p = PathBuf::from(&v);
        return if p.components().count() > 1 {
            // Fixed to an absolute path now: 7-Zip runs from inside the tree
            // being peeled, where `./7zz` would name a planted file.
            let p = std::path::absolute(&p).ok()?;
            is_executable(&p).then_some(p)
        } else {
            search_path(&v)
        };
    }
    names.iter().find_map(|n| search_path(OsStr::new(n)))
}

/// Search only absolute PATH entries. An empty or relative entry means "the
/// current directory", and peel's current directory may be the untrusted tree
/// it is about to peel, where a planted `7zz` would otherwise win.
fn search_path(name: &OsStr) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
