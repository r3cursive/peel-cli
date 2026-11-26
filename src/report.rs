//! Counters, human-readable sizes, and the one place that prints to the user.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
pub(crate) struct Stats {
    pub(crate) extracted: AtomicUsize,
    pub(crate) carved: AtomicUsize,
    pub(crate) would: AtomicUsize,
    pub(crate) done: AtomicUsize,
    pub(crate) locked: AtomicUsize,
    pub(crate) too_big: AtomicUsize,
    pub(crate) too_deep: AtomicUsize,
    pub(crate) failed: AtomicUsize,
    pub(crate) unreadable: AtomicUsize,
    pub(crate) skipped: AtomicUsize,
    pub(crate) timed_out: AtomicUsize,
    /// Work dirs of dead runs that were removed.
    pub(crate) cleaned: AtomicUsize,
}

pub(crate) fn count(c: &AtomicUsize) -> usize {
    c.load(Ordering::SeqCst)
}

impl Stats {
    /// The summary line without its `peel: ` prefix and timing suffix.
    pub(crate) fn summary(&self, dry_run: bool) -> String {
        let head = if dry_run {
            format!("{} would be opened", count(&self.would))
        } else {
            format!(
                "{} extracted ({} carved)",
                count(&self.extracted),
                count(&self.carved)
            )
        };
        let mut summary = format!(
            "{head}, {} already done, {} locked, {} too big, {} too deep, {} failed",
            count(&self.done),
            count(&self.locked),
            count(&self.too_big),
            count(&self.too_deep),
            count(&self.failed),
        );
        if count(&self.unreadable) > 0 {
            summary.push_str(&format!(", {} unreadable", count(&self.unreadable)));
        }
        if count(&self.timed_out) > 0 {
            summary.push_str(&format!(", {} timed out", count(&self.timed_out)));
        }
        if count(&self.skipped) > 0 {
            summary.push_str(&format!(", {} skipped", count(&self.skipped)));
        }
        if count(&self.cleaned) > 0 {
            summary.push_str(&format!(", {} leftovers removed", count(&self.cleaned)));
        }
        summary
    }

    /// Everything left unopened, for `--strict`.
    pub(crate) fn unopened(&self) -> usize {
        [
            &self.failed,
            &self.locked,
            &self.too_big,
            &self.too_deep,
            &self.timed_out,
            &self.unreadable,
            &self.skipped,
        ]
        .into_iter()
        .map(count)
        .sum()
    }
}

pub(crate) fn human(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Bidirectional-text controls: they reorder how a line displays, so a name could
/// make `evil.zip` read as something else.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Control and bidi characters in a file name (or in 7-Zip's message about one)
/// are shown escaped, so a name cannot forge an output line, drive the terminal,
/// or reorder what the line appears to say.
pub(crate) fn clean(s: &str) -> Cow<'_, str> {
    if !s.chars().any(|c| c.is_control() || is_bidi_control(c)) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\x{:02x}", u32::from(c));
            }
            c if is_bidi_control(c) => {
                let _ = write!(out, "\\u{{{:04x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

pub(crate) struct Reporter {
    pub(crate) quiet: bool,
}

impl Reporter {
    pub(crate) fn say(&self, label: &str, path: &Path, detail: &str, problem: bool) {
        if problem || !self.quiet {
            let shown = path.display().to_string();
            let mut out = io::stdout().lock();
            let _ = writeln!(out, "{label:<10} {}{}", clean(&shown), clean(detail));
        }
    }

    pub(crate) fn problem(&self, counter: &AtomicUsize, label: &str, path: &Path, detail: &str) {
        counter.fetch_add(1, Ordering::SeqCst);
        self.say(label, path, detail, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(clean("plain.zip"), "plain.zip");
        assert_eq!(clean("a\nfailed  x.zip"), "a\\nfailed  x.zip");
        assert_eq!(clean("b\x1b[31m.zip"), "b\\x1b[31m.zip");
        assert_eq!(clean("caf\u{e9}"), "caf\u{e9}");
        assert_eq!(clean("photo\u{202E}gpj.exe"), "photo\\u{202e}gpj.exe");
    }
}
