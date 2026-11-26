//! Roots.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use crate::peeler::Job;

/// Resolve the roots, drop duplicates and any root inside another one. peel never
/// follows symlinks while walking, so a root inside another is already covered.
/// Returns the jobs and the roots that could not be read.
pub(crate) fn plan_roots(paths: &[PathBuf]) -> (Vec<Job>, Vec<(PathBuf, String)>) {
    let mut bad = Vec::new();
    let mut dirs: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for given in paths {
        // Roots are followed even when they are symlinks: they were named.
        match fs::canonicalize(given).and_then(|c| fs::metadata(&c).map(|m| (c, m))) {
            Ok((c, m)) if m.is_dir() => dirs.push((given.clone(), c)),
            Ok((c, m)) if m.is_file() => files.push((given.clone(), c)),
            Ok(_) => bad.push((given.clone(), "not a file or folder".into())),
            Err(e) => bad.push((given.clone(), e.to_string())),
        }
    }
    dirs.sort_by(|a, b| a.1.cmp(&b.1));
    let mut kept_dirs: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (given, canon) in dirs {
        if !kept_dirs.iter().any(|(_, k)| canon.starts_with(k)) {
            kept_dirs.push((given, canon));
        }
    }
    let mut seen_files: HashSet<PathBuf> = HashSet::new();
    let mut jobs: Vec<Job> = kept_dirs
        .iter()
        .map(|(g, _)| Job::Walk(g.clone(), 1))
        .collect();
    for (given, canon) in files {
        if kept_dirs.iter().any(|(_, k)| canon.starts_with(k)) || !seen_files.insert(canon) {
            continue;
        }
        jobs.push(Job::File {
            path: given,
            level: 1,
            owned: Vec::new(),
        });
    }
    (jobs, bad)
}
