//! Running the backends under the watchdog.

use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::fsutil::{Usage, tree_usage};
use crate::sys::{GroupGuard, isolate_child, kill_group, stopping};

/// Read at most `cap` bytes of a pipe, then drain the rest so the writer never blocks.
pub(crate) fn drain_capped(
    mut pipe: impl Read + Send + 'static,
    cap: u64,
) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut pipe).take(cap).read_to_end(&mut buf);
        let _ = io::copy(&mut pipe, &mut io::sink());
        buf
    })
}

/// The `--total-size` budget, shared by every job in the run. Bytes count as
/// in flight while an extraction writes them, and as committed once its output
/// is in place.
pub(crate) struct RunBudget {
    pub(crate) limit: u64,
    committed: AtomicU64,
    in_flight: AtomicU64,
}

impl RunBudget {
    pub(crate) fn new(limit: u64) -> RunBudget {
        RunBudget {
            limit,
            committed: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
        }
    }

    fn used(&self) -> u64 {
        self.committed
            .load(Ordering::SeqCst)
            .saturating_add(self.in_flight.load(Ordering::SeqCst))
    }

    pub(crate) fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used())
    }

    pub(crate) fn commit(&self, bytes: u64) {
        self.committed.fetch_add(bytes, Ordering::SeqCst);
    }

    /// Atomically claim `n` bytes of what is left. False when they do not fit, so
    /// of several jobs racing for the last of the budget, the first one wins
    /// instead of all of them seeing each other and giving up.
    fn reserve(&self, n: u64) -> bool {
        let mut current = self.in_flight.load(Ordering::SeqCst);
        loop {
            let committed = self.committed.load(Ordering::SeqCst);
            if committed.saturating_add(current).saturating_add(n) > self.limit {
                return false;
            }
            match self.in_flight.compare_exchange(
                current,
                current + n,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(now) => current = now,
            }
        }
    }
}

/// What one extraction may do. Zero or `None` means no limit.
#[derive(Clone, Copy)]
pub(crate) struct Limits<'a> {
    pub(crate) bytes: u64,
    pub(crate) files: u64,
    pub(crate) timeout: Option<Duration>,
    pub(crate) budget: Option<&'a RunBudget>,
}

pub(crate) enum RunEnd {
    Exited(ExitStatus, String),
    /// Output passed the byte cap: `killed` while running, or found after it exited.
    TooBig {
        size: u64,
        killed: bool,
    },
    /// Output passed the file cap.
    TooMany {
        entries: u64,
        killed: bool,
    },
    /// The run's `--total-size` budget ran out.
    OverBudget,
    TimedOut,
    /// The output could not be measured, so the caps could not be enforced.
    Unmeasurable,
    Stopped,
}

/// One job's share of the run budget. It holds a reservation (the listed size,
/// claimed before extracting) and grows to the most the job has written, so its
/// bytes stay counted from the start until the caller either commits them
/// (output in place) or drops the share (output removed). There is no moment
/// where another job could see them counted nowhere.
pub(crate) struct Share<'a> {
    budget: Option<&'a RunBudget>,
    counted: u64,
}

impl<'a> Share<'a> {
    pub(crate) fn new(budget: Option<&'a RunBudget>) -> Share<'a> {
        Share { budget, counted: 0 }
    }

    /// Claim `n` bytes up front. True when they fit, or when there is no budget.
    pub(crate) fn reserve(&mut self, n: u64) -> bool {
        let Some(b) = self.budget else { return true };
        if n <= self.counted {
            return true;
        }
        let more = n - self.counted;
        if b.reserve(more) {
            self.counted = n;
            true
        } else {
            false
        }
    }

    /// The output is in place: move its bytes from in flight to committed.
    pub(crate) fn commit(mut self, bytes: u64) {
        if let Some(b) = self.budget {
            b.commit(bytes);
        }
        self.release();
    }

    /// Count `bytes` written so far. Only ever grows, so a reservation is never
    /// handed back early while the job is still writing.
    fn grow(&mut self, bytes: u64) {
        if let Some(b) = self.budget
            && bytes > self.counted
        {
            b.in_flight
                .fetch_add(bytes - self.counted, Ordering::SeqCst);
            self.counted = bytes;
        }
    }

    fn release(&mut self) {
        if let Some(b) = self.budget {
            b.in_flight.fetch_sub(self.counted, Ordering::SeqCst);
            self.counted = 0;
        }
    }
}

impl Drop for Share<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

/// Check `watch` against the limits. `None` when everything is within them.
fn violation(watch: &Path, limits: &Limits, share: &mut Share, killed: bool) -> Option<RunEnd> {
    let Some(Usage { bytes, entries }) = tree_usage(watch) else {
        // Fails closed: an unreadable directory in the output counts as over.
        return Some(RunEnd::Unmeasurable);
    };
    share.grow(bytes);
    if limits.bytes > 0 && bytes > limits.bytes {
        return Some(RunEnd::TooBig {
            size: bytes,
            killed,
        });
    }
    if limits.files > 0 && entries > limits.files {
        return Some(RunEnd::TooMany { entries, killed });
    }
    if let Some(b) = limits.budget
        && b.used() > b.limit
    {
        return Some(RunEnd::OverBudget);
    }
    None
}

/// Run an extractor in its own process group with a closed stdin, watching
/// `watch` against `limits`. Past a limit, when the output cannot be measured,
/// or on a stop signal, the whole group is killed. The group is also killed once
/// the extractor exits, so no helper it left behind keeps writing. `share` is
/// the caller's, so the bytes stay counted after this returns.
pub(crate) fn run_capped(
    mut cmd: Command,
    watch: &Path,
    limits: &Limits,
    share: &mut Share,
) -> io::Result<RunEnd> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    isolate_child(&mut cmd);
    let start = Instant::now();
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let _group = GroupGuard::register(pid);
    let reader = drain_capped(child.stderr.take().expect("stderr is piped"), 64 * 1024);
    let finish = |child: &mut Child, reader: thread::JoinHandle<Vec<u8>>| {
        kill_group(pid);
        let _ = child.wait();
        reader.join().unwrap_or_default()
    };
    let measured = limits.bytes > 0 || limits.files > 0 || limits.budget.is_some();
    let mut delay = Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait()? {
            let err = finish(&mut child, reader);
            if stopping() {
                return Ok(RunEnd::Stopped);
            }
            if measured && let Some(end) = violation(watch, limits, share, false) {
                return Ok(end);
            }
            return Ok(RunEnd::Exited(
                status,
                String::from_utf8_lossy(&err).into_owned(),
            ));
        }
        if stopping() {
            finish(&mut child, reader);
            return Ok(RunEnd::Stopped);
        }
        if limits.timeout.is_some_and(|t| start.elapsed() > t) {
            finish(&mut child, reader);
            return Ok(RunEnd::TimedOut);
        }
        if measured && let Some(end) = violation(watch, limits, share, true) {
            finish(&mut child, reader);
            return Ok(end);
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(250));
    }
}
