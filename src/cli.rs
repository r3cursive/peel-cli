//! The command line: usage text, options and argument parsing.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use crate::report::clean;

pub(crate) const USAGE: &str = "\
peel: take apart every archive and compressed file under a folder,
layer by layer

Usage: peel [OPTIONS] PATH...

Each PATH may be a folder (walked recursively) or a single file; use `peel .`
for the current folder. Anything 7-Zip can open is extracted into <stem>/
beside it (foo.zip -> foo/, or foo.zip_x/ when foo/ is taken). With --binwalk,
top-layer files 7-Zip cannot open are also carved (--binwalk-depth N reaches
deeper). Whatever comes out is walked again. Originals are never changed,
symlinks are never followed, nothing prompts.

Options:
  -p, --password PW    password to try on encrypted archives (default: none)
  -d, --depth N        most nested layers to peel (default: 8)
  -m, --max-size SIZE  skip or stop anything that unpacks past SIZE
                       (default: 10G; K/M/G/T suffixes; 0 = no limit)
  -j, --jobs N         parallel extractions (default: CPU count)
      --max-files N    skip or stop an extraction past N files (default: off)
      --timeout TIME   stop an extraction running longer than TIME, e.g. 30s,
                       10m, 2h (default: off)
      --total-size SIZE  stop extracting once the whole run has written SIZE
                       (default: off)
      --strict         exit 1 when anything was left unopened (locked, too big,
                       too deep, timed out, unreadable or skipped)
  -b, --binwalk        carve top-layer files 7-Zip cannot open, with binwalk
      --binwalk-depth N  carve down to layer N (default: 1; implies -b)
  -n, --dry-run        list what would be extracted, write nothing to the tree
                       (with -b, binwalk still runs, in a private temp dir)
  -q, --quiet          print only problems and the summary
  -h, --help           show this help
  -V, --version        show the version

Environment:
  PEEL_7Z       7-Zip binary (default: 7zz, then 7z, on PATH)
  PEEL_BINWALK  binwalk binary (default: binwalk on PATH)

Output: one line per item on stdout (a label, the path, then any detail) and a
summary line on stderr. Labels: extracted, carved, locked, too big, too deep,
timed out, failed, unreadable, skipped, removed; with -n: would open, would
carve, would remove. An item that is already done prints no line, only a count.

Exit status: 0 all good; 1 an archive failed to open (corrupt, or no backend
could read it) or a path given does not exist; 2 usage error or a missing
backend; 130 stopped by a signal (INT, TERM, HUP or QUIT). Locked, too big,
too deep, timed out, unreadable and skipped are reported but stay 0 unless
--strict.
";

/// Passed to 7-Zip when the user gave no password, so an encrypted archive fails
/// fast instead of prompting on the terminal.
const NO_PASSWORD: &str = "peel-no-password";
const DEFAULT_DEPTH: usize = 8;
/// binwalk carves only the files given and the files loose under them, unless
/// asked to go deeper, so a PDF that came out of a zip keeps its images.
const DEFAULT_BINWALK_DEPTH: usize = 1;
const DEFAULT_MAX_SIZE: u64 = 10 << 30;

#[derive(Clone)]
pub(crate) struct Opts {
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) password: OsString,
    pub(crate) depth: usize,
    pub(crate) max_size: u64,
    pub(crate) jobs: usize,
    pub(crate) binwalk: bool,
    /// Deepest layer binwalk carves (1 = the paths given). `--depth` still caps it.
    pub(crate) binwalk_depth: usize,
    pub(crate) dry_run: bool,
    pub(crate) quiet: bool,
    /// Opt-in limits, all off unless given.
    /// Most files and dirs one extraction may write (0 = no limit).
    pub(crate) max_files: u64,
    /// Longest one extraction may run.
    pub(crate) timeout: Option<Duration>,
    /// Most bytes the whole run may write (0 = no limit).
    pub(crate) total_size: u64,
    /// Exit 1 when anything was left unopened.
    pub(crate) strict: bool,
}

pub(crate) enum Cli {
    Run(Opts),
    Help,
    Version,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            paths: Vec::new(),
            password: OsString::from(NO_PASSWORD),
            depth: DEFAULT_DEPTH,
            max_size: DEFAULT_MAX_SIZE,
            jobs: thread::available_parallelism().map_or(4, |n| n.get()),
            binwalk: false,
            binwalk_depth: DEFAULT_BINWALK_DEPTH,
            dry_run: false,
            quiet: false,
            max_files: 0,
            timeout: None,
            total_size: 0,
            strict: false,
        }
    }
}

pub(crate) fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Cli, String> {
    let mut o = Opts::default();
    let mut args = args.into_iter();
    let mut only_paths = false;
    while let Some(arg) = args.next() {
        let bytes = arg.as_bytes();
        if only_paths || bytes.len() < 2 || bytes[0] != b'-' {
            o.paths.push(PathBuf::from(arg));
            continue;
        }
        // `--flag=value`, or 7-Zip style `-pPASSWORD` / `-d3` / `-m1G` / `-j4`.
        let (flag, inline) = if bytes.starts_with(b"--") {
            match bytes.iter().position(|&c| c == b'=') {
                Some(i) => (
                    &bytes[..i],
                    Some(OsString::from_vec(bytes[i + 1..].to_vec())),
                ),
                None => (bytes, None),
            }
        } else if bytes.len() > 2 && matches!(bytes[1], b'p' | b'd' | b'm' | b'j') {
            (&bytes[..2], Some(OsString::from_vec(bytes[2..].to_vec())))
        } else {
            (bytes, None)
        };
        let shown = String::from_utf8_lossy(flag).into_owned();
        let no_value = |set: &mut bool| {
            if inline.is_some() {
                return Err(format!("{shown} takes no value"));
            }
            *set = true;
            Ok(())
        };
        match flag {
            b"-h" | b"--help" => return Ok(Cli::Help),
            b"-V" | b"--version" => return Ok(Cli::Version),
            b"--" => only_paths = true,
            b"-p" | b"--password" => {
                let pw = value(&shown, inline, &mut args)?;
                if pw.is_empty() {
                    return Err("--password cannot be empty".into());
                }
                o.password = pw;
            }
            b"-d" | b"--depth" => o.depth = positive(&shown, &value(&shown, inline, &mut args)?)?,
            b"-j" | b"--jobs" => o.jobs = positive(&shown, &value(&shown, inline, &mut args)?)?,
            b"-m" | b"--max-size" => o.max_size = size(&shown, &value(&shown, inline, &mut args)?)?,
            b"--max-files" => {
                o.max_files = positive(&shown, &value(&shown, inline, &mut args)?)? as u64;
            }
            b"--timeout" => o.timeout = Some(time(&shown, &value(&shown, inline, &mut args)?)?),
            b"--total-size" => o.total_size = size(&shown, &value(&shown, inline, &mut args)?)?,
            b"--strict" => no_value(&mut o.strict)?,
            b"-b" | b"--binwalk" => no_value(&mut o.binwalk)?,
            b"--binwalk-depth" => {
                o.binwalk_depth = positive(&shown, &value(&shown, inline, &mut args)?)?;
                o.binwalk = true;
            }
            b"-n" | b"--dry-run" => no_value(&mut o.dry_run)?,
            b"-q" | b"--quiet" => no_value(&mut o.quiet)?,
            _ => return Err(format!("unknown option: {}", clean(&arg.to_string_lossy()))),
        }
    }
    Ok(Cli::Run(o))
}

fn value(
    flag: &str,
    inline: Option<OsString>,
    rest: &mut impl Iterator<Item = OsString>,
) -> Result<OsString, String> {
    inline
        .or_else(|| rest.next())
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn positive(flag: &str, v: &OsStr) -> Result<usize, String> {
    match v.to_str().and_then(|s| s.parse::<usize>().ok()) {
        Some(n) if n > 0 => Ok(n),
        _ => Err(format!(
            "{flag}: expected a positive number, got {:?}",
            v.to_string_lossy()
        )),
    }
}

fn size(flag: &str, v: &OsStr) -> Result<u64, String> {
    parse_size(&v.to_string_lossy())
        .ok_or_else(|| format!("{flag}: bad size {:?}", v.to_string_lossy()))
}

fn time(flag: &str, v: &OsStr) -> Result<Duration, String> {
    parse_duration(&v.to_string_lossy())
        .ok_or_else(|| format!("{flag}: bad time {:?}", v.to_string_lossy()))
}

/// `10G`, `512M`, `1.5G`, `4096`, `0`. Suffixes are binary (K = 1024). A nonzero
/// size that rounds to zero bytes is rejected, since 0 would mean "no limit".
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let s = s
        .strip_suffix("iB")
        .or_else(|| s.strip_suffix('B'))
        .unwrap_or(s);
    let (num, mult) = match s.chars().last()?.to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1u64 << 10),
        'M' => (&s[..s.len() - 1], 1 << 20),
        'G' => (&s[..s.len() - 1], 1 << 30),
        'T' => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    let n: f64 = num.trim().parse().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    let bytes = (n * mult as f64) as u64;
    (n == 0.0 || bytes > 0).then_some(bytes)
}

/// `30s`, `10m`, `2h`, `500ms`, or a bare number of seconds. Must be above zero.
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (num, scale) = if let Some(n) = s.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60.0)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600.0)
    } else {
        (s, 1.0)
    };
    let n: f64 = num.trim().parse().ok()?;
    (n.is_finite() && n > 0.0).then(|| Duration::from_secs_f64(n * scale))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("0s"), None);
        assert_eq!(parse_duration("soon"), None);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("0"), Some(0));
        assert_eq!(parse_size("4096"), Some(4096));
        assert_eq!(parse_size("1K"), Some(1024));
        assert_eq!(parse_size("10G"), Some(10 << 30));
        assert_eq!(parse_size("10GiB"), Some(10 << 30));
        assert_eq!(parse_size("1.5m"), Some(3 << 19));
        assert_eq!(
            parse_size("0.5"),
            None,
            "rounds to zero, which would mean no limit"
        );
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_size("-1"), None);
    }

    #[test]
    fn args() {
        let parse = |v: &[&str]| parse_args(v.iter().map(OsString::from));
        let Ok(Cli::Run(o)) = parse(&["-p", "infected", "--depth=3", "-m", "1G", "a", "--", "-b"])
        else {
            panic!("should parse");
        };
        assert_eq!(o.password, "infected");
        assert_eq!(o.depth, 3);
        assert_eq!(o.max_size, 1 << 30);
        assert_eq!(o.paths, [PathBuf::from("a"), PathBuf::from("-b")]);
        let Ok(Cli::Run(o)) = parse(&["-pinfected", "-d2", "-j3", "-m4K", "x"]) else {
            panic!("7-Zip style attached values should parse");
        };
        assert_eq!(
            (o.password.to_str(), o.depth, o.jobs, o.max_size),
            (Some("infected"), 2, 3, 4096)
        );
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["-p"]).is_err());
        assert!(parse(&["-p", ""]).is_err());
        assert!(parse(&["-d", "0"]).is_err());
        assert!(parse(&["-m", "0.5"]).is_err());
        assert!(parse(&["--dry-run=no"]).is_err());
        assert!(parse(&["--binwalk=yes"]).is_err());
        assert!(matches!(parse(&["-h"]), Ok(Cli::Help)));
        let Ok(Cli::Run(o)) = parse(&[
            "--max-files",
            "100",
            "--timeout=2m",
            "--total-size",
            "50G",
            "--strict",
            ".",
        ]) else {
            panic!("limit flags should parse");
        };
        assert_eq!(o.max_files, 100);
        assert_eq!(o.timeout, Some(Duration::from_secs(120)));
        assert_eq!(o.total_size, 50 << 30);
        assert!(o.strict);
        assert!(parse(&["--strict=yes"]).is_err());
        assert!(parse(&["--timeout", "0"]).is_err());
    }

    #[test]
    fn binwalk_depth_parses_like_depth() {
        let parse = |v: &[&str]| parse_args(v.iter().map(OsString::from));
        let Ok(Cli::Run(o)) = parse(&["-b", "."]) else {
            panic!("should parse");
        };
        assert!(o.binwalk);
        assert_eq!(o.binwalk_depth, 1, "-b alone carves layer 1 only");
        let Ok(Cli::Run(o)) = parse(&["--binwalk-depth=3", "."]) else {
            panic!("should parse");
        };
        assert!(o.binwalk, "--binwalk-depth turns binwalk on by itself");
        assert_eq!(o.binwalk_depth, 3);
        let Ok(Cli::Run(o)) = parse(&["--binwalk-depth", "2", "-b", "."]) else {
            panic!("should parse");
        };
        assert_eq!((o.binwalk, o.binwalk_depth), (true, 2));
        let Ok(Cli::Run(o)) = parse(&["."]) else {
            panic!("should parse");
        };
        assert!(!o.binwalk, "binwalk stays off without a flag");
        assert!(parse(&["--binwalk-depth", "0"]).is_err());
        assert!(parse(&["--binwalk-depth", "x"]).is_err());
        assert!(parse(&["--binwalk-depth"]).is_err());
    }

    #[test]
    fn size_and_time_helpers_name_the_flag_and_value() {
        assert_eq!(
            size("-m", OsStr::new("x")),
            Err("-m: bad size \"x\"".to_string())
        );
        assert_eq!(size("-m", OsStr::new("1G")), Ok(1 << 30));
        assert_eq!(
            time("--timeout", OsStr::new("9y")),
            Err("--timeout: bad time \"9y\"".to_string())
        );
        assert_eq!(
            time("--timeout", OsStr::new("10m")),
            Ok(Duration::from_secs(600))
        );
    }

    #[test]
    fn opts_default_holds_the_documented_defaults() {
        let o = Opts::default();
        assert_eq!(o.depth, 8);
        assert_eq!(o.binwalk_depth, 1);
        assert_eq!(o.max_size, 10 << 30);
        assert_eq!(o.password.to_str(), Some("peel-no-password"));
        assert_eq!(o.max_files, 0);
        assert_eq!(o.timeout, None);
        assert_eq!(o.total_size, 0);
        assert!(!o.strict);
        assert!(o.paths.is_empty());
        assert!(!o.binwalk && !o.dry_run && !o.quiet);
    }

    #[test]
    fn size_and_time_messages_reach_parse_args_unchanged() {
        let parse = |v: &[&str]| parse_args(v.iter().map(OsString::from));
        assert_eq!(
            parse(&["-m", "x"]).err().as_deref(),
            Some("-m: bad size \"x\"")
        );
        assert_eq!(
            parse(&["--total-size=1Q"]).err().as_deref(),
            Some("--total-size: bad size \"1Q\"")
        );
        assert_eq!(
            parse(&["--timeout", "9y"]).err().as_deref(),
            Some("--timeout: bad time \"9y\"")
        );
    }

    #[test]
    fn usage_fits_80_columns() {
        for line in USAGE.lines() {
            let columns = line.chars().count();
            assert!(
                columns <= 80,
                "USAGE line is {columns} columns, over 80: {line:?}"
            );
        }
    }
}
