//! The peeler: the job queue, the worker threads, and what happens to one file.

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::binwalk::{Binwalk, Carve};
use crate::cli::Opts;
use crate::exec::{Limits, RunBudget, Share};
use crate::fsutil::{has_content, remove, sole_file, tree_usage};
use crate::naming::{Names, archive_ext};
use crate::provenance::{
    PROVENANCE_MARKER, mark_temp, marker_in_use, provenance, set_provenance, temp_mark,
};
use crate::report::{Reporter, Stats, human};
use crate::sevenzip::{Outcome, Probe, STREAM_TYPES, SevenZip};
use crate::sys::{process_alive, stopping};

const TEMP_PREFIX: &str = ".peel-tmp";

pub(crate) enum Job {
    Walk(PathBuf, usize),
    /// `owned`: sibling directories this file made on an earlier run. The file's
    /// job walks them, one layer deeper when they are its output.
    File {
        path: PathBuf,
        level: usize,
        owned: Vec<PathBuf>,
    },
}

#[derive(Default)]
pub(crate) struct Queue {
    jobs: VecDeque<Job>,
    active: usize,
}

pub(crate) struct Peeler {
    pub(crate) opts: Opts,
    pub(crate) sevenzip: SevenZip,
    pub(crate) binwalk: Option<Binwalk>,
    /// Output names taken in this run, between the claim and the rename.
    pub(crate) claims: Mutex<HashSet<PathBuf>>,
    pub(crate) queue: Mutex<Queue>,
    pub(crate) wake: Condvar,
    pub(crate) stats: Stats,
    pub(crate) reporter: Reporter,
    pub(crate) temp_seq: AtomicU64,
    /// Random per run (from /dev/urandom), part of every temp directory's name.
    pub(crate) temp_token: u64,
    /// The `--total-size` budget, when one was given.
    pub(crate) budget: Option<RunBudget>,
}

/// An extraction's output, ready to be moved into place by `Peeler::commit`.
struct Staged<'a> {
    dir: &'a Path,
    label: &'a str,
    carved: bool,
    share: Share<'a>,
}

/// The pid and run token in a name of exactly the form `make_temp` writes:
/// `.peel-tmp-<pid>-<16 hex>-<seq>`. Anything else is not a peel leftover.
fn leftover_of(name: &OsStr) -> Option<(i32, &str)> {
    let s = name
        .to_str()?
        .strip_prefix(TEMP_PREFIX)?
        .strip_prefix('-')?;
    let mut parts = s.split('-');
    let (pid, token, seq) = (parts.next()?, parts.next()?, parts.next()?);
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    let hex = token.len() == 16 && token.bytes().all(|b| b.is_ascii_hexdigit());
    if !(parts.next().is_none() && digits(pid) && hex && digits(seq)) {
        return None;
    }
    Some((pid.parse().ok()?, token))
}

impl Peeler {
    pub(crate) fn push(&self, jobs: Vec<Job>) {
        if jobs.is_empty() {
            return;
        }
        self.queue.lock().unwrap().jobs.extend(jobs);
        self.wake.notify_all();
    }

    /// Create a fresh hidden working directory in `parent`. The name carries this
    /// run's random token, so nothing in the tree can plant something at it ahead
    /// of time, and `create_dir` fails on anything already there, a symlink
    /// included. Only a directory this returns is ever handed to an extractor or
    /// removed.
    fn make_temp(&self, parent: &Path) -> io::Result<PathBuf> {
        loop {
            let seq = self.temp_seq.fetch_add(1, Ordering::SeqCst);
            let name = format!(
                "{TEMP_PREFIX}-{}-{:016x}-{seq}",
                std::process::id(),
                self.temp_token
            );
            let path = parent.join(name);
            match fs::create_dir(&path) {
                Ok(()) => {
                    // The ownership proof a later run checks before it removes
                    // this dir as a leftover (an archive cannot ship one).
                    mark_temp(&path, &format!("{:016x}", self.temp_token));
                    return Ok(path);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// What one extraction may do, from the options.
    fn limits(&self) -> Limits<'_> {
        Limits {
            bytes: self.opts.max_size,
            files: self.opts.max_files,
            timeout: self.opts.timeout,
            budget: self.budget.as_ref(),
        }
    }

    fn temp_failed(&self, path: &Path, e: &io::Error) {
        self.reporter.problem(
            &self.stats.failed,
            "failed",
            path,
            &format!(": cannot create work directory: {e}"),
        );
    }

    pub(crate) fn run(&self) {
        thread::scope(|s| {
            for _ in 0..self.opts.jobs {
                s.spawn(|| self.worker());
            }
        });
    }

    fn worker(&self) {
        loop {
            let job = {
                let mut q = self.queue.lock().unwrap();
                loop {
                    if stopping() {
                        q.jobs.clear();
                    }
                    if let Some(job) = q.jobs.pop_front() {
                        q.active += 1;
                        break Some(job);
                    }
                    if q.active == 0 {
                        break None;
                    }
                    q = self
                        .wake
                        .wait_timeout(q, Duration::from_millis(200))
                        .unwrap()
                        .0;
                }
            };
            let Some(job) = job else {
                self.wake.notify_all();
                return;
            };
            let result = panic::catch_unwind(AssertUnwindSafe(|| match &job {
                Job::Walk(dir, level) => self.walk(dir, *level),
                Job::File { path, level, owned } => self.process_file(path, *level, owned),
            }));
            if result.is_err() {
                let path = match &job {
                    Job::Walk(p, _) | Job::File { path: p, .. } => p,
                };
                self.reporter.problem(
                    &self.stats.failed,
                    "failed",
                    path,
                    ": internal error (please report)",
                );
            }
            let mut q = self.queue.lock().unwrap();
            q.active -= 1;
            if q.active == 0 && q.jobs.is_empty() {
                self.wake.notify_all();
            }
        }
    }

    fn walk(&self, dir: &Path, level: usize) {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                return self.reporter.problem(
                    &self.stats.unreadable,
                    "unreadable",
                    dir,
                    &format!(": {e}"),
                );
            }
        };
        // Collect before queueing: an extraction finishing mid-listing must not show
        // up in this listing and get walked twice.
        let mut files: Vec<(PathBuf, OsString)> = Vec::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            // Names peel reserves for itself are never opened. A `.peel-src` that is
            // this directory's own provenance record passes quietly; anything else
            // at a reserved name (a crash leftover, or something an archive
            // shipped) is reported, so it cannot hide content silently.
            if name.as_bytes().starts_with(TEMP_PREFIX.as_bytes()) || name == PROVENANCE_MARKER {
                if name == PROVENANCE_MARKER && marker_in_use(dir) {
                    continue;
                }
                // A work dir left by a run that died (SIGKILL, OOM, power loss) is
                // removed, but only a real directory with
                // exactly peel's name format, carrying the ownership mark peel
                // wrote with the same run token, whose run is confirmed gone. A
                // folder that came out of an archive has no mark, so it is only
                // reported. Symlinks and look-alike names are left alone too.
                if let Some((pid, token)) = leftover_of(&name)
                    && entry.file_type().is_ok_and(|t| t.is_dir())
                    && temp_mark(&entry.path()).as_deref() == Some(token)
                    && !process_alive(pid)
                {
                    let path = entry.path();
                    if self.opts.dry_run {
                        self.reporter.say(
                            "would remove",
                            &path,
                            ": leftover of an interrupted run",
                            false,
                        );
                    } else {
                        remove(&path);
                        self.reporter.problem(
                            &self.stats.cleaned,
                            "removed",
                            &path,
                            ": leftover of an interrupted run",
                        );
                    }
                    continue;
                }
                self.reporter.problem(
                    &self.stats.skipped,
                    "skipped",
                    &entry.path(),
                    ": name reserved by peel, not opened",
                );
                continue;
            }
            // DirEntry::file_type does not follow symlinks: links, fifos, sockets and
            // devices are never touched.
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                dirs.push(entry.path());
            } else if ft.is_file() {
                files.push((entry.path(), name));
            }
        }
        // A subdirectory made by a sibling file on an earlier run belongs to that
        // file's job, which walks it one layer deeper, so the depth cap holds across
        // re-runs.
        let names: HashSet<&OsStr> = files.iter().map(|(_, n)| n.as_os_str()).collect();
        let mut owned: HashMap<OsString, Vec<PathBuf>> = HashMap::new();
        let mut jobs = Vec::new();
        for d in dirs {
            match provenance(&d) {
                Some(src) if names.contains(src.as_os_str()) => {
                    owned.entry(src).or_default().push(d)
                }
                _ => jobs.push(Job::Walk(d, level)),
            }
        }
        for (path, name) in files {
            let owned = owned.remove(&name).unwrap_or_default();
            jobs.push(Job::File { path, level, owned });
        }
        self.push(jobs);
    }

    fn process_file(&self, path: &Path, level: usize, owned: &[PathBuf]) {
        let walked = self.handle_file(path, level);
        // The walk already matched each owned dir's provenance to this file. One that
        // sits at one of this file's output names is its output and goes one layer
        // deeper, even when the file no longer opens this run; any other is walked
        // like a normal folder.
        let outputs = path
            .file_name()
            .map(|name| Names::new(path, name).candidates)
            .unwrap_or_default();
        for dir in owned {
            if walked.as_ref() == Some(dir) {
                continue;
            }
            let depth = if outputs.contains(dir) {
                level + 1
            } else {
                level
            };
            self.push(vec![Job::Walk(dir.clone(), depth)]);
        }
    }

    /// Returns the earlier output directory it descended into, if any.
    fn handle_file(&self, path: &Path, level: usize) -> Option<PathBuf> {
        let name = path.file_name()?;
        let names = Names::new(path, name);
        let probed = self.sevenzip.probe(path);
        if stopping() {
            return None;
        }
        match probed {
            Err(e) => self.reporter.problem(
                &self.stats.failed,
                "failed",
                path,
                &format!(": cannot run 7-Zip: {e}"),
            ),
            Ok(Probe::Archive {
                kind,
                size,
                entries,
            }) => {
                if let Some(done) = self.already_done(&names, level) {
                    return Some(done);
                }
                self.open_archive(path, &names, level, &kind, size, entries);
            }
            Ok(Probe::Locked) => {
                if let Some(done) = self.already_done(&names, level) {
                    return Some(done);
                }
                self.reporter
                    .problem(&self.stats.locked, "locked", path, "");
            }
            Ok(Probe::Broken(msg)) => {
                self.reporter
                    .problem(&self.stats.failed, "failed", path, &format!(": {msg}"))
            }
            Ok(Probe::TimedOut) => {
                let after = self.opts.timeout.unwrap_or_default();
                self.reporter.problem(
                    &self.stats.timed_out,
                    "timed out",
                    path,
                    &format!(": probe stopped after {after:?}"),
                );
            }
            Ok(Probe::NotArchive) => {
                if let Some(done) = self.already_done(&names, level) {
                    return Some(done);
                }
                self.try_carve(path, &names, level);
            }
        }
        None
    }

    /// An earlier run's output for this input: count it and walk it one layer deeper.
    fn already_done(&self, names: &Names, level: usize) -> Option<PathBuf> {
        let dir = names.existing()?;
        self.stats.done.fetch_add(1, Ordering::SeqCst);
        self.push(vec![Job::Walk(dir.clone(), level + 1)]);
        Some(dir)
    }

    fn open_archive(
        &self,
        path: &Path,
        names: &Names,
        level: usize,
        kind: &str,
        size: Option<u64>,
        entries: u64,
    ) {
        let cap = self.opts.max_size;
        let limits = self.limits();
        if level > self.opts.depth {
            return self.reporter.problem(
                &self.stats.too_deep,
                "too deep",
                path,
                &format!(" (layer {level})"),
            );
        }
        // Up-front checks against what the listing says. Single-stream
        // compressors list one entry with an unreliable size, so they are left
        // to the watchdog.
        let stream = STREAM_TYPES.contains(&kind);
        let refuse = |detail: String| {
            self.reporter
                .problem(&self.stats.too_big, "too big", path, &detail)
        };
        if let (Some(n), false, true) = (size, stream, cap > 0)
            && n > cap
        {
            return refuse(format!(": lists {} > {}", human(n), human(cap)));
        }
        if !stream && self.opts.max_files > 0 && entries > self.opts.max_files {
            return refuse(format!(
                ": lists {entries} entries > {}",
                self.opts.max_files
            ));
        }
        if let Some(b) = &self.budget {
            let left = b.remaining();
            if left == 0 {
                return refuse(format!(": the {} run budget is used up", human(b.limit)));
            }
            if let (Some(n), false) = (size, stream)
                && n > left
            {
                return refuse(format!(
                    ": lists {}, only {} of the {} run budget left",
                    human(n),
                    human(left),
                    human(b.limit)
                ));
            }
        }
        if self.opts.dry_run {
            self.stats.would.fetch_add(1, Ordering::SeqCst);
            return self
                .reporter
                .say("would open", path, &format!("  [{kind}]"), false);
        }

        // This job's bytes stay counted against --total-size from here until
        // commit (or until the output is removed and the share dropped). The
        // listed size is reserved first, so of jobs racing for the last of the
        // budget, one goes ahead instead of all of them giving up.
        let mut share = Share::new(self.budget.as_ref());
        if let (Some(b), Some(n), false) = (&self.budget, size, stream)
            && !share.reserve(n)
        {
            return refuse(format!(
                ": lists {}, only {} of the {} run budget left",
                human(n),
                human(b.remaining()),
                human(b.limit)
            ));
        }
        let mut out = match self.make_temp(&names.parent) {
            Ok(dir) => dir,
            Err(e) => return self.temp_failed(path, &e),
        };
        let mut label = kind.to_owned();
        match self.sevenzip.extract(path, &out, &limits, &mut share) {
            Outcome::Ok => {}
            other => {
                remove(&out);
                return self.report_outcome(path, other);
            }
        }

        // gzip/xz/zstd/... around a tar: unpack the tar in the same step. The cap
        // applies to the final output; the intermediate .tar is gone right after.
        if stream
            && let Some(inner) = sole_file(&out)
            && let Ok(Probe::Archive {
                kind: inner_kind, ..
            }) = self.sevenzip.probe(&inner)
            && inner_kind == "tar"
        {
            let flat = match self.make_temp(&names.parent) {
                Ok(dir) => dir,
                Err(e) => {
                    remove(&out);
                    return self.temp_failed(path, &e);
                }
            };
            let mut flat_share = Share::new(self.budget.as_ref());
            match self
                .sevenzip
                .extract(&inner, &flat, &limits, &mut flat_share)
            {
                Outcome::Ok => {
                    remove(&out);
                    out = flat;
                    // The .tar is gone: its bytes leave the budget with the old share.
                    share = flat_share;
                    label = format!("tar+{kind}");
                }
                Outcome::Failed(_) | Outcome::Locked => remove(&flat), // keep the .tar
                other => {
                    remove(&flat);
                    remove(&out);
                    return self.report_outcome(path, other);
                }
            }
        }

        if !has_content(&out) {
            // Opened, but nothing inside worth a directory.
            remove(&out);
            return;
        }
        self.commit(
            path,
            names,
            level,
            Staged {
                dir: &out,
                label: &label,
                carved: false,
                share,
            },
        );
    }

    fn report_outcome(&self, path: &Path, outcome: Outcome) {
        let cap = human(self.opts.max_size);
        match outcome {
            Outcome::Ok | Outcome::Stopped => {}
            Outcome::Locked => self
                .reporter
                .problem(&self.stats.locked, "locked", path, ""),
            Outcome::TooBig { size, killed: true } => {
                let detail = format!(": stopped after passing {cap} ({} written)", human(size));
                self.reporter
                    .problem(&self.stats.too_big, "too big", path, &detail);
            }
            Outcome::TooBig {
                size,
                killed: false,
            } => {
                let detail = format!(": unpacked to {} > {cap}", human(size));
                self.reporter
                    .problem(&self.stats.too_big, "too big", path, &detail);
            }
            Outcome::TooMany { entries, killed } => {
                let max = self.opts.max_files;
                let detail = if killed {
                    format!(": stopped after {entries} files > {max}")
                } else {
                    format!(": unpacked {entries} files > {max}")
                };
                self.reporter
                    .problem(&self.stats.too_big, "too big", path, &detail);
            }
            Outcome::OverBudget => {
                let limit = self.budget.as_ref().map_or(0, |b| b.limit);
                let detail = format!(": stopped, the {} run budget is used up", human(limit));
                self.reporter
                    .problem(&self.stats.too_big, "too big", path, &detail);
            }
            Outcome::TimedOut => {
                let after = self.opts.timeout.unwrap_or_default();
                let detail = format!(": stopped after {after:?}");
                self.reporter
                    .problem(&self.stats.timed_out, "timed out", path, &detail);
            }
            Outcome::Failed(msg) => {
                self.reporter
                    .problem(&self.stats.failed, "failed", path, &format!(": {msg}"))
            }
        }
    }

    fn try_carve(&self, path: &Path, names: &Names, level: usize) {
        // binwalk goes no deeper than `--binwalk-depth` (1 by default: the paths
        // given and what is loose under them), and never past `--depth`.
        if let Some(binwalk) = &self.binwalk
            && level <= self.opts.depth.min(self.opts.binwalk_depth)
        {
            if self.opts.dry_run {
                if self.dry_carve(binwalk, path, names) {
                    self.stats.would.fetch_add(1, Ordering::SeqCst);
                    return self.reporter.say("would carve", path, "  [binwalk]", false);
                }
            } else {
                let work = match self.make_temp(&names.parent) {
                    Ok(dir) => dir,
                    Err(e) => return self.temp_failed(path, &e),
                };
                let stage = match self.make_temp(&names.parent) {
                    Ok(dir) => dir,
                    Err(e) => {
                        remove(&work);
                        return self.temp_failed(path, &e);
                    }
                };
                let mut share = Share::new(self.budget.as_ref());
                match binwalk.carve(path, &names.name, &work, &stage, &self.limits(), &mut share) {
                    Carve::Got(stage) => {
                        let made = Staged {
                            dir: &stage,
                            label: "binwalk",
                            carved: true,
                            share,
                        };
                        return self.commit(path, names, level, made);
                    }
                    Carve::Limited(outcome) => {
                        remove(&stage);
                        return self.report_outcome(path, outcome);
                    }
                    Carve::Stopped => return remove(&stage),
                    Carve::Failed(msg) if archive_ext(&names.name).is_some() => {
                        remove(&stage);
                        return self.reporter.problem(
                            &self.stats.failed,
                            "failed",
                            path,
                            &format!(": {msg}"),
                        );
                    }
                    Carve::Failed(_) | Carve::Nothing => remove(&stage),
                }
            }
        }
        if let Some(ext) = archive_ext(&names.name)
            && !stopping()
        {
            let why = if ext == "rar" {
                ": no backend could open it (RAR needs a 7-Zip build with RAR support, e.g. 7zz)"
            } else {
                ": no backend could open it"
            };
            self.reporter
                .problem(&self.stats.failed, "failed", path, why);
        }
    }

    /// Carve into a fresh private directory under the system temp dir, so a dry run
    /// leaves the tree untouched. Returns whether anything would be carved.
    fn dry_carve(&self, binwalk: &Binwalk, path: &Path, names: &Names) -> bool {
        let seq = self.temp_seq.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let scratch =
            env::temp_dir().join(format!("peel-dry-{}-{seq}-{nanos}", std::process::id()));
        // create_dir fails on anything already there, a planted symlink included.
        if fs::create_dir(&scratch).is_err() {
            return false;
        }
        let _ = fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700));
        let (work, stage) = (scratch.join("w"), scratch.join("s"));
        let got = fs::create_dir(&work).is_ok()
            && fs::create_dir(&stage).is_ok()
            && matches!(
                binwalk.carve(
                    path,
                    &names.name,
                    &work,
                    &stage,
                    &self.limits(),
                    &mut Share::new(None)
                ),
                Carve::Got(_)
            );
        remove(&scratch);
        got
    }

    fn commit(&self, path: &Path, names: &Names, level: usize, made: Staged) {
        let Staged {
            dir: staged,
            label,
            carved,
            share,
        } = made;
        // Provenance first, so the directory is never in place without its record.
        if let Err(e) = set_provenance(staged, &names.name) {
            remove(staged);
            return self.reporter.problem(
                &self.stats.failed,
                "failed",
                path,
                &format!(": cannot record provenance: {e}"),
            );
        }
        let Some(target) = names.claim(&self.claims) else {
            remove(staged);
            return self.reporter.problem(
                &self.stats.failed,
                "failed",
                path,
                ": no free output name",
            );
        };
        if let Err(e) = fs::rename(staged, &target) {
            remove(staged);
            return self.reporter.problem(
                &self.stats.failed,
                "failed",
                path,
                &format!(": cannot move output into place: {e}"),
            );
        }
        share.commit(tree_usage(&target).map_or(0, |u| u.bytes));
        self.stats.extracted.fetch_add(1, Ordering::SeqCst);
        let verb = if carved {
            self.stats.carved.fetch_add(1, Ordering::SeqCst);
            "carved"
        } else {
            "extracted"
        };
        self.reporter.say(
            verb,
            path,
            &format!(" -> {}  [{label}]", target.display()),
            false,
        );
        self.push(vec![Job::Walk(target, level + 1)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_leftover_names_parse() {
        let pid = |s: &str| leftover_of(OsStr::new(s)).map(|(p, _)| p);
        assert_eq!(pid(".peel-tmp-4242-0123456789abcdef-7"), Some(4242));
        assert_eq!(pid(".peel-tmp-4242-0123456789ABCDEF-0"), Some(4242));
        assert_eq!(
            pid(".peel-tmp-4242-0"),
            None,
            "old two-part names are not ours now"
        );
        assert_eq!(pid(".peel-tmp-4242-notours"), None);
        assert_eq!(pid(".peel-tmp-4242-0123456789abcdef-7-extra"), None);
        assert_eq!(pid(".peel-tmp-x-0123456789abcdef-7"), None);
        assert_eq!(pid(".peel-tmpfoo"), None);
        assert_eq!(pid(".peel-src"), None);
    }
}
