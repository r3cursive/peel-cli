//! libc, declared by hand so the crate stays dependency-free, and the signal
//! handling built on it.

use std::ffi::{CStr, CString, c_char, c_int, c_ulong, c_void};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

unsafe extern "C" {
    fn signal(signum: c_int, handler: extern "C" fn(c_int)) -> usize;
    fn kill(pid: c_int, sig: c_int) -> c_int;
    fn prctl(option: c_int, ...) -> c_int;
    fn _exit(status: c_int) -> !;
    fn lsetxattr(
        path: *const c_char,
        name: *const c_char,
        value: *const c_void,
        size: usize,
        flags: c_int,
    ) -> c_int;
    fn lgetxattr(
        path: *const c_char,
        name: *const c_char,
        value: *mut c_void,
        size: usize,
    ) -> isize;
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::other("path holds a NUL byte"))
}

/// Set the extended attribute `name` on `path` itself, never on a symlink's
/// target.
pub(crate) fn set_xattr(path: &Path, name: &CStr, value: &[u8]) -> io::Result<()> {
    let p = c_path(path)?;
    // SAFETY: valid NUL-terminated strings and a live buffer of `value.len()`.
    let rc = unsafe {
        lsetxattr(
            p.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Read the extended attribute `name` of `path` itself into `buf`, and return the
/// value's length. An empty `buf` only asks for the size.
pub(crate) fn get_xattr(path: &Path, name: &CStr, buf: &mut [u8]) -> io::Result<usize> {
    let p = c_path(path)?;
    let value = if buf.is_empty() {
        std::ptr::null_mut()
    } else {
        buf.as_mut_ptr().cast()
    };
    // SAFETY: valid NUL-terminated strings, and `value` is null with size 0 or a
    // live buffer of `buf.len()`.
    let n = unsafe { lgetxattr(p.as_ptr(), name.as_ptr(), value, buf.len()) };
    usize::try_from(n).map_err(|_| io::Error::last_os_error())
}

const SIGHUP: c_int = 1;
const SIGINT: c_int = 2;
const SIGQUIT: c_int = 3;
const SIGKILL: c_int = 9;
const SIGTERM: c_int = 15;
const PR_SET_PDEATHSIG: c_int = 1;
/// "Operation not supported": the filesystem has no user xattrs.
pub(crate) const ENOTSUP: i32 = 95;

// ---------------------------------------------------------------------------
// Signals: every child peel starts (extractors and probes) runs in its own
// process group, so a size or stop kill reaches binwalk's helpers too, and that
// takes them out of the terminal's Ctrl-C group. Each live group is registered
// here. A stop signal kills every registered group at once (a hung probe or a
// grandchild included), a second one exits at once, and PDEATHSIG covers the
// direct child if peel dies some other way.

static STOP: AtomicBool = AtomicBool::new(false);

/// Process groups of the children running right now; 0 marks a free slot.
static GROUPS: [AtomicI32; 512] = [const { AtomicI32::new(0) }; 512];

extern "C" fn on_signal(_signum: c_int) {
    let again = STOP.swap(true, Ordering::SeqCst);
    for slot in &GROUPS {
        let pgid = slot.load(Ordering::SeqCst);
        if pgid > 0 {
            // SAFETY: kill is async-signal-safe.
            unsafe {
                kill(-pgid, SIGKILL);
            }
        }
    }
    if again {
        // SAFETY: _exit is async-signal-safe.
        unsafe { _exit(130) }
    }
}

pub(crate) fn install_signal_handlers() {
    for sig in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
        // SAFETY: the handler only touches atomics and calls kill and _exit.
        unsafe {
            signal(sig, on_signal);
        }
    }
}

/// A child's process group, registered so a stop signal kills it. Dropping the
/// guard unregisters it, so keep it alive until the child has been reaped.
pub(crate) struct GroupGuard(Option<usize>);

impl GroupGuard {
    /// Register the group led by `pid`. If a stop already arrived, kill it now:
    /// the signal may have come before the registration.
    pub(crate) fn register(pid: u32) -> GroupGuard {
        let pgid = i32::try_from(pid).unwrap_or(0);
        let slot = if pgid > 0 {
            GROUPS.iter().position(|s| {
                s.compare_exchange(0, pgid, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            })
        } else {
            None
        };
        if stopping() {
            kill_group(pid);
        }
        GroupGuard(slot)
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(i) = self.0 {
            GROUPS[i].store(0, Ordering::SeqCst);
        }
    }
}

pub(crate) fn kill_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: plain syscall; a negative pid addresses the child's group.
        unsafe {
            kill(-pid, SIGKILL);
        }
    }
}

pub(crate) fn stopping() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// "No such process": the pid is not in use.
const ESRCH: i32 = 3;

/// Whether a process with this pid exists. Signal 0 checks without sending
/// anything; only ESRCH counts as gone, so a pid owned by someone else (EPERM)
/// counts as alive and its work dir is left alone. Our own pid is always alive.
pub(crate) fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return true;
    }
    // SAFETY: plain syscall; signal 0 only checks for existence.
    let rc = unsafe { kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(ESRCH)
}

/// Run the child in its own process group, and have the kernel kill it when peel
/// dies. Applied to every extractor and probe command before it is spawned.
/// The child also gets a PATH of absolute entries only, or no PATH when none is
/// left: binwalk runs its helpers (7z, tar, unrar, ...) by name from inside a
/// directory of extracted content, so an empty or relative entry would let a
/// planted binary run there.
pub(crate) fn isolate_child(cmd: &mut Command) {
    isolate_light(cmd);
    // SAFETY: the hook runs between fork and exec and only makes one syscall.
    unsafe {
        cmd.pre_exec(|| {
            prctl(PR_SET_PDEATHSIG, SIGKILL as c_ulong);
            Ok(())
        });
    }
}

/// `isolate_child` without PDEATHSIG, for short-lived probes. A `pre_exec` hook
/// makes std fork and exec instead of using `posix_spawn`, which measured about
/// 4.5 times slower on a tree of plain files. A probe stays in its own registered
/// group, so a stop signal still kills it, and `--timeout` still bounds it.
pub(crate) fn isolate_light(cmd: &mut Command) {
    if let Some(path) = std::env::var_os("PATH") {
        let absolute: Vec<_> = std::env::split_paths(&path)
            .filter(|p| p.is_absolute())
            .collect();
        match std::env::join_paths(&absolute) {
            Ok(clean) if !absolute.is_empty() => {
                cmd.env("PATH", clean);
            }
            // No absolute entry is left (or, in theory, they cannot be joined).
            // An empty PATH means the current directory to the exec family, so
            // the child gets no PATH at all: the exec family's default applies
            // (glibc's `_CS_PATH`), or a shell's own built-in one.
            _ => {
                cmd.env_remove("PATH");
            }
        }
    }
    cmd.process_group(0);
}
