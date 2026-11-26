//! 7-Zip: probing what a file is and extracting it.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::exec::{Limits, RunEnd, Share, drain_capped, run_capped};
use crate::sys::{GroupGuard, isolate_light, kill_group};

/// 7-Zip types that wrap one stream. Their listed size comes from an unreliable
/// trailer, and a tar inside one is unpacked in the same step.
pub(crate) const STREAM_TYPES: &[&str] = &[
    "gzip", "bzip2", "xz", "zstd", "lzma", "lzma86", "Z", "lz4", "lzip", "brotli",
];

/// The 7-Zip binary, the password to try on encrypted archives, and the
/// `--timeout`, which bounds probes as well as extractions.
pub(crate) struct SevenZip {
    pub(crate) bin: PathBuf,
    pub(crate) password: OsString,
    pub(crate) timeout: Option<Duration>,
    /// Whether to pass `-spod`, which only 7-Zip 26.01 and newer accept.
    pub(crate) spod: bool,
}

/// 7-Zip's version as `(major, minor)`, from its banner: `7-Zip 26.04 (x64) : ...`
/// or p7zip's `7-Zip [64] 16.02 : ...`.
pub(crate) fn banner_version(text: &str) -> Option<(u32, u32)> {
    let line = text.lines().find(|l| l.trim_start().starts_with("7-Zip"))?;
    line.split_whitespace().find_map(|word| {
        let (major, minor) = word.split_once('.')?;
        Some((major.parse().ok()?, minor.parse().ok()?))
    })
}

/// Whether this 7-Zip needs `-spod` (26.01 and newer, where a `*` in the `-o`
/// path otherwise means "archive name"). Builds before 26.01 reject the switch.
/// When the banner cannot be read, the answer is yes: on a new build, leaving it
/// out would let a `*` in a directory name redirect output, and an old build
/// then fails loudly instead.
pub(crate) fn needs_spod(bin: &Path) -> bool {
    banner_of(bin)
        .and_then(|text| banner_version(&text))
        .is_none_or(|v| v >= (26, 1))
}

/// What the binary prints when run with no arguments, within 3 seconds. A
/// binary that hangs instead is killed and counts as unreadable.
fn banner_of(bin: &Path) -> Option<String> {
    let mut cmd = Command::new(bin);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    isolate_light(&mut cmd);
    let mut child = cmd.spawn().ok()?;
    let reader = drain_capped(child.stdout.take()?, 64 * 1024);
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(3) => {
                thread::sleep(Duration::from_millis(10));
            }
            _ => {
                kill_group(child.id());
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    }
    reader
        .join()
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

pub(crate) enum Probe {
    /// `size` sums the listed entry sizes, `entries` counts the listed entries.
    Archive {
        kind: String,
        size: Option<u64>,
        entries: u64,
    },
    Locked,
    NotArchive,
    Broken(String),
    /// The listing ran past `--timeout` and was killed.
    TimedOut,
}

pub(crate) enum Failure {
    Locked,
    NotArchive,
    Other(String),
}

pub(crate) enum Outcome {
    Ok,
    Locked,
    TooBig { size: u64, killed: bool },
    TooMany { entries: u64, killed: bool },
    OverBudget,
    TimedOut,
    Stopped,
    Failed(String),
}

impl Outcome {
    /// A watchdog verdict as an outcome, for anything other than a normal exit.
    pub(crate) fn from_limit(end: RunEnd) -> Outcome {
        match end {
            RunEnd::TooBig { size, killed } => Outcome::TooBig { size, killed },
            RunEnd::TooMany { entries, killed } => Outcome::TooMany { entries, killed },
            RunEnd::OverBudget => Outcome::OverBudget,
            RunEnd::TimedOut => Outcome::TimedOut,
            RunEnd::Stopped => Outcome::Stopped,
            RunEnd::Unmeasurable => {
                Outcome::Failed("output has an unreadable directory, so its size is unknown".into())
            }
            RunEnd::Exited(_, err) => Outcome::Failed(first_error(&err)),
        }
    }
}

impl SevenZip {
    fn password_arg(&self) -> OsString {
        let mut a = OsString::from("-p");
        a.push(&self.password);
        a
    }

    /// Ask 7-Zip what `path` is: `7z l -slt`, read the first `Type =` of the archive
    /// header and sum the entry sizes. The listing is parsed as it streams, so a huge
    /// archive's listing is never held in memory. The probe runs isolated and
    /// registered like an extractor, so a stop signal ends a hung one.
    pub(crate) fn probe(&self, path: &Path) -> io::Result<Probe> {
        let mut cmd = Command::new(&self.bin);
        // -spd: a name like `?.zip` is a file name, never a wildcard.
        cmd.args(["l", "-slt", "-spd"])
            .arg(self.password_arg())
            .arg("--")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        isolate_light(&mut cmd);
        let mut child = cmd.spawn()?;
        let pid = child.id();
        let _group = GroupGuard::register(pid);
        // The listing is read on this thread, so a hung probe is timed from a
        // watcher thread that kills its group when --timeout passes.
        let finished = Arc::new(AtomicBool::new(false));
        let watcher = self.timeout.map(|limit| {
            let finished = Arc::clone(&finished);
            thread::spawn(move || {
                let start = Instant::now();
                while !finished.load(Ordering::SeqCst) {
                    if start.elapsed() > limit {
                        kill_group(pid);
                        return true;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                false
            })
        });
        let errors = drain_capped(child.stderr.take().expect("stderr is piped"), 16 * 1024);
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let (mut header, mut entries) = (false, false);
        let mut kind = None;
        let mut size: Option<u64> = None;
        let mut listed: u64 = 0;
        let mut head = String::new();
        let mut raw = Vec::new();
        loop {
            raw.clear();
            if stdout.read_until(b'\n', &mut raw)? == 0 {
                break;
            }
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\n', '\r']);
            if head.len() < 16 * 1024 {
                head.push_str(line);
                head.push('\n');
            }
            if line == "----------" {
                entries = true;
            } else if !entries {
                if line == "--" {
                    header = true;
                } else if header && kind.is_none() {
                    kind = line.strip_prefix("Type = ").map(str::to_owned);
                }
            } else if line.starts_with("Path = ") {
                listed += 1;
            } else if let Some(n) = line
                .strip_prefix("Size = ")
                .and_then(|s| s.trim().parse::<u64>().ok())
            {
                size = Some(size.unwrap_or(0).saturating_add(n));
            }
        }
        let status = child.wait()?;
        finished.store(true, Ordering::SeqCst);
        let errors = errors.join().unwrap_or_default();
        if watcher.is_some_and(|w| w.join().unwrap_or(false)) {
            return Ok(Probe::TimedOut);
        }
        if !status.success() {
            let text = format!("{head}\n{}", String::from_utf8_lossy(&errors));
            return Ok(match failure_kind(&text, path) {
                Failure::Locked => Probe::Locked,
                Failure::NotArchive => Probe::NotArchive,
                Failure::Other(msg) => Probe::Broken(msg),
            });
        }
        Ok(Probe::Archive {
            kind: kind.unwrap_or_else(|| "?".into()),
            size,
            entries: listed,
        })
    }

    /// Extract `archive` into `out`, which the caller has already created,
    /// exclusively, at a name no one else can predict.
    pub(crate) fn extract(
        &self,
        archive: &Path,
        out: &Path,
        limits: &Limits,
        share: &mut Share,
    ) -> Outcome {
        // 7-Zip reads a `*` in the -o path as "archive name" (on 26.01 and newer
        // unless -spod is passed, and older builds may offer no way to turn it
        // off), and the folders above a work dir can carry names from the tree.
        // So 7-Zip runs from the folder that holds the work dir, the -o value is
        // only the work dir's own name (which peel generated and never contains
        // a `*`), and the archive goes in as an absolute path, kept literal by -spd.
        let (Some(dir), Some(name)) = (out.parent(), out.file_name()) else {
            return Outcome::Failed("work directory has no parent".into());
        };
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        let archive = match std::path::absolute(archive) {
            Ok(p) => p,
            Err(e) => return Outcome::Failed(format!("cannot resolve the archive's path: {e}")),
        };
        let archive = archive.as_path();
        let mut o = OsString::from("-o");
        o.push(name);
        let mut cmd = Command::new(&self.bin);
        cmd.current_dir(dir);
        // -spod: since 7-Zip 26.01 a `*` in the -o path means "archive name";
        // older builds reject the switch, so it goes only where it exists.
        // -spd: a name like `?.zip` is a file name, never a wildcard.
        // -snld is never passed, so 7-Zip keeps refusing symlinks that point outside
        // the output directory.
        cmd.args(["x", "-y"]);
        if self.spod {
            cmd.arg("-spod");
        }
        cmd.args(["-spd", "-bso0", "-bsp0"])
            .arg(self.password_arg())
            .arg(o)
            .arg("--")
            .arg(archive);
        match run_capped(cmd, out, limits, share) {
            // 1 is 7-Zip's "warning": the archive came out, with complaints.
            Ok(RunEnd::Exited(st, _)) if matches!(st.code(), Some(0 | 1)) => Outcome::Ok,
            Ok(RunEnd::Exited(_, err)) => match failure_kind(&err, archive) {
                Failure::Locked => Outcome::Locked,
                Failure::NotArchive => {
                    Outcome::Failed("7-Zip listed it but could not open it".into())
                }
                Failure::Other(msg) => Outcome::Failed(msg),
            },
            Ok(end) => Outcome::from_limit(end),
            Err(e) => Outcome::Failed(format!("cannot run 7-Zip: {e}")),
        }
    }
}

/// 7-Zip's per-entry messages that mean "the password is wrong". They open the
/// `ERROR:` payload and are followed by ` : <entry name>`.
const LOCKED_ENTRY_MESSAGES: &[&str] = &[
    "Wrong password",
    "CRC Failed in encrypted file. Wrong password?",
    "Data Error in encrypted file. Wrong password?",
];
/// 7-Zip's whole-archive message for an encrypted header: `<input> : <this>`.
const LOCKED_ARCHIVE_MESSAGE: &str = "Cannot open encrypted archive. Wrong password?";
/// 7-Zip's messages for "this is not an archive", alone on a line or as
/// `<input> : <message>`.
const NOT_ARCHIVE_MESSAGES: &[&str] = &[
    "Cannot open the file as archive",
    "Can not open the file as archive",
    "Is not archive",
];

/// Classify 7-Zip's complaint. Entry names (`Path = ...` lines, and the text after
/// ` : ` in per-entry errors) and the input's own name are attacker-chosen, so
/// only 7-Zip's complete message forms count: a file called `Wrong password.zip`
/// or an entry named `Cannot open encrypted archive` must not steer the verdict.
/// Anything that does not match a whole form reads as a failure, never as locked
/// or as not-an-archive.
fn failure_kind(text: &str, input: &Path) -> Failure {
    let shown = input.to_string_lossy();
    // 7-Zip prints control characters in a path as `_`.
    let printed: String = shown
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect();
    let lines: Vec<String> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("Path = "))
        .map(|l| {
            let mut l = l.trim().to_owned();
            for name in [shown.as_ref(), printed.as_str()] {
                if !name.is_empty() {
                    l = l.replace(name, "<input>");
                }
            }
            l
        })
        .collect();
    let payloads = || {
        lines
            .iter()
            .filter_map(|l| l.strip_prefix("ERROR:").map(str::trim_start))
    };
    let locked = payloads().any(|p| {
        LOCKED_ENTRY_MESSAGES
            .iter()
            .any(|m| p == *m || p.starts_with(&format!("{m} : ")))
            || p == format!("<input> : {LOCKED_ARCHIVE_MESSAGE}")
    });
    if locked {
        return Failure::Locked;
    }
    let not_archive = lines
        .iter()
        .any(|l| NOT_ARCHIVE_MESSAGES.contains(&l.as_str()))
        || payloads().any(|p| {
            NOT_ARCHIVE_MESSAGES
                .iter()
                .any(|m| p == format!("<input> : {m}"))
        });
    if not_archive {
        Failure::NotArchive
    } else {
        Failure::Other(first_error(&lines.join("\n")))
    }
}

pub(crate) fn first_error(text: &str) -> String {
    let lines = || text.lines().map(str::trim).filter(|l| !l.is_empty());
    let line = lines()
        .find(|l| l.starts_with("ERROR") || l.contains("rror"))
        .or_else(|| lines().next())
        .unwrap_or("unknown error");
    let line = line.strip_prefix("ERROR:").unwrap_or(line).trim();
    line.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(text: &str, input: &str) -> &'static str {
        match failure_kind(text, Path::new(input)) {
            Failure::Locked => "locked",
            Failure::NotArchive => "not-archive",
            Failure::Other(_) => "other",
        }
    }

    #[test]
    fn errors() {
        assert_eq!(kind("ERROR: Wrong password : f.txt", "a.zip"), "locked");
        assert_eq!(
            kind(
                "ERROR: h.7z : Cannot open encrypted archive. Wrong password?",
                "h.7z"
            ),
            "locked"
        );
        assert_eq!(
            kind("ERROR: x\nCannot open the file as archive", "x"),
            "not-archive"
        );
        match failure_kind("ERROR: Unexpected end of data : p.txt", Path::new("c.gz")) {
            Failure::Other(m) => assert_eq!(m, "Unexpected end of data : p.txt"),
            _ => panic!("should be Other"),
        }
    }

    #[test]
    fn attacker_chosen_names_do_not_steer_the_verdict() {
        // An input named after a lock message is still just not an archive.
        let input = "./Wrong password Cannot open encrypted archive.txt";
        let text = format!("ERROR: {input} : Cannot open the file as archive");
        assert_eq!(kind(&text, input), "not-archive");
        // An entry named after a lock message does not make a broken archive locked.
        let listing = "Path = Wrong password\nPath = Cannot open encrypted archive\nERROR: Data Error : Wrong password";
        assert_eq!(kind(listing, "b.zip"), "other");
        // Nor does an entry named after a not-an-archive message.
        let entry = "ERROR: Data Error : Cannot open the file as archive";
        assert_eq!(kind(entry, "b.zip"), "other");
        // 7-Zip prints a newline in the input's name as `_`; that name must still
        // be recognized as the input, not as a lock message.
        let input = "steer/Cannot open encrypted archive\nnl.txt";
        let text =
            "ERROR: steer/Cannot open encrypted archive_nl.txt : Cannot open the file as archive";
        assert_eq!(kind(text, input), "not-archive");
    }

    #[test]
    fn banner_versions() {
        let v = |s: &str| banner_version(s);
        assert_eq!(
            v("\n7-Zip 26.04 (x64) : Copyright (c) 1999-2026 Igor Pavlov : 2026-10-05"),
            Some((26, 4))
        );
        assert_eq!(
            v("7-Zip 23.01 (x64) : Copyright (c) 1999-2023 Igor Pavlov"),
            Some((23, 1))
        );
        assert_eq!(
            v("7-Zip [64] 16.02 : Copyright (c) 1999-2016 Igor Pavlov"),
            Some((16, 2))
        );
        assert_eq!(v("not 7-zip at all"), None);
        assert!(v("7-Zip 26.04").unwrap() >= (26, 1));
        assert!(v("7-Zip 23.01").unwrap() < (26, 1));
    }

    #[test]
    fn every_wrong_password_form_is_locked() {
        for text in [
            "ERROR: Wrong password : p.txt",
            "ERROR: CRC Failed in encrypted file. Wrong password? : p.txt",
            "ERROR: Data Error in encrypted file. Wrong password? : p.txt",
            "ERROR: ./e.7z : Cannot open encrypted archive. Wrong password?",
        ] {
            assert_eq!(kind(text, "./e.7z"), "locked", "{text}");
        }
    }
}
