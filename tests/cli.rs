//! End-to-end tests: each one builds a small tree with the system's own archivers,
//! runs the real `peel` binary over it, and checks the tree afterwards. Each test's
//! name says the behavior it pins down.

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const PEEL: &str = env!("CARGO_BIN_EXE_peel");
const PAYLOAD: &[u8] = b"hello from peel\n";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::create_dir_all(dir.join("root")).unwrap();
        fs::write(dir.join("src/p.txt"), PAYLOAD).unwrap();
        Scratch(dir)
    }
    fn src(&self) -> PathBuf {
        self.0.join("src")
    }
    fn root(&self) -> PathBuf {
        self.0.join("root")
    }
    fn at(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn tool(cwd: &Path, argv: &[&str]) {
    let st = Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", argv[0]));
    assert!(st.success(), "{argv:?} failed");
}

/// Run a compressor as `cmd -c src > dest`.
fn compress_to(cwd: &Path, argv: &[&str], dest: &Path) {
    let out = fs::File::create(dest).unwrap();
    let st = Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdout(out)
        .status()
        .unwrap();
    assert!(st.success(), "{argv:?} failed");
}

/// `7z a <dest> <files...>` from the scratch src dir.
fn zip(s: &Scratch, dest: &Path, files: &[&str], extra: &[&str]) {
    let dest = dest.to_str().unwrap().to_owned();
    let mut argv = vec!["7z", "a", "-bso0", "-bsp0"];
    argv.extend_from_slice(extra);
    argv.push(&dest);
    argv.extend_from_slice(files);
    tool(&s.src(), &argv);
}

fn peel(cwd: &Path, args: &[&str]) -> Output {
    peel_env(cwd, args, &[])
}

fn peel_env(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(PEEL);
    cmd.args(args).current_dir(cwd).stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn has_binwalk() -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|d| d.join("binwalk").is_file())
}

/// Every path under `dir`, relative and sorted, without following symlinks.
fn listing(dir: &Path) -> Vec<String> {
    fn go(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            out.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
            if e.file_type().unwrap().is_dir() {
                go(base, &p, out);
            }
        }
    }
    let mut out = Vec::new();
    go(dir, dir, &mut out);
    out.sort();
    out
}

fn no_temp_dirs(dir: &Path) -> bool {
    listing(dir)
        .iter()
        .all(|p| !p.split('/').any(|c| c.starts_with(".peel-tmp")))
}

#[test]
fn no_args_refuses() {
    let s = Scratch::new("noargs");
    zip(&s, &s.at("a.zip"), &["p.txt"], &[]);
    let before = listing(&s.root());
    let out = peel(&s.root(), &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("peel ."),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(before, listing(&s.root()), "a bare peel must touch nothing");
}

#[test]
fn dot_uses_cwd() {
    let s = Scratch::new("dot");
    zip(&s, &s.at("a.zip"), &["p.txt"], &[]);
    let out = peel(&s.root(), &["."]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(fs::read(s.at("a/p.txt")).unwrap(), PAYLOAD);
}

#[test]
fn multiple_dirs() {
    let s = Scratch::new("multi");
    fs::create_dir_all(s.at("one")).unwrap();
    fs::create_dir_all(s.at("two")).unwrap();
    zip(&s, &s.at("one/a.zip"), &["p.txt"], &[]);
    zip(&s, &s.at("two/b.zip"), &["p.txt"], &[]);
    let out = peel(&s.root(), &["one", "two"]);
    assert!(out.status.success());
    assert_eq!(fs::read(s.at("one/a/p.txt")).unwrap(), PAYLOAD);
    assert_eq!(fs::read(s.at("two/b/p.txt")).unwrap(), PAYLOAD);
}

#[test]
fn single_file_arg() {
    let s = Scratch::new("single");
    zip(&s, &s.at("some.zip"), &["p.txt"], &[]);
    zip(&s, &s.at("other.zip"), &["p.txt"], &[]);
    let out = peel(&s.root(), &["some.zip"]);
    assert!(out.status.success());
    assert_eq!(fs::read(s.at("some/p.txt")).unwrap(), PAYLOAD);
    assert!(!s.at("other").exists(), "only the named file is opened");
}

#[test]
fn common_formats() {
    let s = Scratch::new("formats");
    let src = s.src();
    zip(&s, &s.at("fz.zip"), &["p.txt"], &[]);
    zip(&s, &s.at("f7.7z"), &["p.txt"], &[]);
    tool(
        &src,
        &["tar", "cf", s.at("ft.tar").to_str().unwrap(), "p.txt"],
    );
    for (name, argv) in [
        ("pg.txt", ["gzip", "-c"]),
        ("pb.txt", ["bzip2", "-c"]),
        ("px.txt", ["xz", "-c"]),
        ("pz.txt", ["zstd", "-qc"]),
    ] {
        fs::write(src.join(name), PAYLOAD).unwrap();
        let ext = match argv[0] {
            "gzip" => "gz",
            "bzip2" => "bz2",
            "xz" => "xz",
            _ => "zst",
        };
        compress_to(
            &src,
            &[argv[0], argv[1], name],
            &s.at(&format!("{name}.{ext}")),
        );
    }
    let out = peel(&s.root(), &["."]);
    assert!(out.status.success(), "{}", text(&out.stdout));
    for want in [
        "fz/p.txt",
        "f7/p.txt",
        "ft/p.txt",
        "pg.txt/pg.txt",
        "pb.txt/pb.txt",
        "px.txt/px.txt",
        "pz.txt/pz.txt",
    ] {
        assert_eq!(
            fs::read(s.at(want)).unwrap_or_default(),
            PAYLOAD,
            "{want}\n{}",
            text(&out.stdout)
        );
    }
}

#[test]
fn extensionless_zip() {
    let s = Scratch::new("noext");
    zip(&s, &s.at("tmp.zip"), &["p.txt"], &[]);
    fs::rename(s.at("tmp.zip"), s.at("blob")).unwrap();
    assert!(peel(&s.root(), &["."]).status.success());
    assert_eq!(fs::read(s.at("blob_x/p.txt")).unwrap(), PAYLOAD);
}

#[test]
fn nested_layers() {
    let s = Scratch::new("nested");
    tool(&s.src(), &["tar", "czf", "t.tar.gz", "p.txt"]);
    zip(&s, &s.at("n.zip"), &["t.tar.gz"], &[]);
    assert!(peel(&s.root(), &["."]).status.success());
    assert_eq!(fs::read(s.at("n/t/p.txt")).unwrap(), PAYLOAD);
}

#[test]
fn depth_cap() {
    let s = Scratch::new("depth");
    zip(&s, &s.src().join("inner.zip"), &["p.txt"], &[]);
    zip(&s, &s.at("outer.zip"), &["inner.zip"], &[]);
    let out = peel(&s.root(), &["--depth", "1", "."]);
    assert!(out.status.success());
    assert!(s.at("outer/inner.zip").is_file());
    assert!(!s.at("outer/inner").exists());
    assert!(
        text(&out.stdout).contains("too deep"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn encrypted_zip_locked() {
    let s = Scratch::new("enc");
    zip(&s, &s.at("enc.zip"), &["p.txt"], &["-psecret"]);
    // Hold peel's stdin open, like a terminal: if 7-Zip inherited it and asked
    // for a password, it would wait forever instead of failing fast.
    let mut child = Command::new(PEEL)
        .arg(".")
        .current_dir(s.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let held = child.stdin.take();
    let status = wait_for(&mut child, Duration::from_secs(10));
    drop(held);
    assert!(status.success());
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut stdout).unwrap();
    assert!(
        stdout.contains("locked") && stdout.contains("enc.zip"),
        "{stdout}"
    );
    assert!(!s.at("enc").exists() && !s.at("enc.zip_x").exists());
    assert!(no_temp_dirs(&s.root()));
}

#[test]
fn header_encrypted_7z_locked() {
    let s = Scratch::new("hdr");
    zip(&s, &s.at("h.7z"), &["p.txt"], &["-psecret", "-mhe=on"]);
    let out = peel(&s.root(), &["."]);
    assert!(out.status.success());
    assert!(
        text(&out.stdout).contains("locked"),
        "{}",
        text(&out.stdout)
    );
    assert!(!s.at("h").exists() && !s.at("h.7z_x").exists());
}

#[test]
fn password_flag() {
    let s = Scratch::new("pw");
    zip(&s, &s.at("s.zip"), &["p.txt"], &["-pinfected"]);
    assert!(peel(&s.root(), &["-p", "infected", "."]).status.success());
    assert_eq!(fs::read(s.at("s/p.txt")).unwrap(), PAYLOAD);
}

#[test]
fn idempotent_rerun() {
    let s = Scratch::new("rerun");
    zip(&s, &s.at("a.zip"), &["p.txt"], &[]);
    tool(
        &s.src(),
        &["tar", "czf", s.at("b.tar.gz").to_str().unwrap(), "p.txt"],
    );
    assert!(peel(&s.root(), &["."]).status.success());
    let before = listing(&s.root());
    let out = peel(&s.root(), &["."]);
    assert!(out.status.success());
    assert!(
        !text(&out.stdout).contains("extracted"),
        "{}",
        text(&out.stdout)
    );
    let err = text(&out.stderr);
    assert!(
        err.contains("0 extracted") && err.contains("2 already done"),
        "{err}"
    );
    assert_eq!(before, listing(&s.root()));
}

#[test]
fn corrupt_archive_fails() {
    let s = Scratch::new("corrupt");
    fs::write(s.src().join("c.txt"), PAYLOAD.repeat(200)).unwrap();
    compress_to(
        &s.src(),
        &["gzip", "-c", "c.txt"],
        &s.src().join("c.txt.gz"),
    );
    let whole = fs::read(s.src().join("c.txt.gz")).unwrap();
    fs::write(s.at("c.txt.gz"), &whole[..20]).unwrap();
    let out = peel(&s.root(), &["."]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(text(&out.stdout).contains("failed"));
    assert!(!s.at("c.txt").exists() && !s.at("c.txt.gz_x").exists());
    assert!(no_temp_dirs(&s.root()));
}

#[test]
fn summary_and_exit() {
    let s = Scratch::new("summary");
    zip(&s, &s.at("a.zip"), &["p.txt"], &[]);
    let out = peel(&s.root(), &["."]);
    assert_eq!(out.status.code(), Some(0));
    let err = text(&out.stderr);
    for word in [
        "1 extracted",
        "already done",
        "locked",
        "too big",
        "too deep",
        "failed",
    ] {
        assert!(err.contains(word), "summary lacks {word:?}: {err}");
    }
}

#[test]
fn odd_names() {
    let s = Scratch::new("names");
    fs::create_dir_all(s.at("my files")).unwrap();
    zip(&s, &s.at("my files/a b.zip"), &["p.txt"], &[]);
    zip(&s, &s.at("tmp.zip"), &["p.txt"], &[]);
    let weird = OsString::from_vec(b"caf\xe9.zip".to_vec());
    fs::rename(s.at("tmp.zip"), s.root().join(&weird)).unwrap();
    let out = peel(&s.root(), &["."]);
    assert!(out.status.success(), "{}", text(&out.stdout));
    assert_eq!(fs::read(s.at("my files/a b/p.txt")).unwrap(), PAYLOAD);
    let stem = OsString::from_vec(b"caf\xe9".to_vec());
    assert_eq!(
        fs::read(s.root().join(stem).join("p.txt")).unwrap(),
        PAYLOAD
    );
}

#[test]
fn dry_run() {
    let s = Scratch::new("dry");
    zip(&s, &s.at("a.zip"), &["p.txt"], &[]);
    let before = listing(&s.root());
    let out = peel(&s.root(), &["--dry-run", "."]);
    assert!(out.status.success());
    assert!(
        text(&out.stdout).contains("would open"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(before, listing(&s.root()));
}

#[test]
fn missing_backend() {
    let s = Scratch::new("nobackend");
    let out = peel_env(&s.root(), &["."], &[("PEEL_7Z", "/nonexistent/7z")]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("7-Zip not found"));
}

#[test]
fn help_and_bad_flag() {
    let s = Scratch::new("help");
    let help = peel(&s.root(), &["--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(text(&help.stdout).contains("Usage: peel"));
    assert_eq!(peel(&s.root(), &["--bogus"]).status.code(), Some(2));
}

#[test]
fn help_states_output_and_exit_status() {
    let s = Scratch::new("help-output");
    let help = peel(&s.root(), &["--help"]);
    assert_eq!(help.status.code(), Some(0));
    let out = text(&help.stdout);
    assert!(out.contains("summary line on stderr"), "{out}");
    assert!(
        out.contains("timed out, unreadable and skipped are reported but stay 0 unless"),
        "{out}"
    );
}

#[test]
fn version_prints_name_and_version() {
    let s = Scratch::new("version");
    let v = peel(&s.root(), &["--version"]);
    assert_eq!(v.status.code(), Some(0));
    assert_eq!(
        text(&v.stdout),
        format!("peel {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn many_plain_files_fast() {
    let s = Scratch::new("many");
    for d in 0..20 {
        let dir = s.at(&format!("d{d}"));
        fs::create_dir_all(&dir).unwrap();
        for f in 0..100 {
            fs::write(
                dir.join(format!("note{f}.txt")),
                format!("plain text {d} {f}\n"),
            )
            .unwrap();
        }
    }
    let before = listing(&s.root());
    let start = Instant::now();
    let out = peel(&s.root(), &["."]);
    let took = start.elapsed();
    assert!(out.status.success());
    assert_eq!(before, listing(&s.root()));
    // 5 s is the budget on a fast many-core workstation, where this takes about
    // 0.4 s. Slower machines such as 4-vCPU CI runners set their own budget
    // through PEEL_PERF_BUDGET_SECS; it still catches a several-fold slowdown.
    let budget = std::env::var("PEEL_PERF_BUDGET_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    assert!(
        took < Duration::from_secs(budget),
        "2,000 plain files took {took:?} (budget {budget} s)"
    );
    eprintln!("2,000 plain files: {took:?}");
}

#[test]
fn symlinks_not_followed() {
    let s = Scratch::new("links");
    let outside = s.0.join("outside");
    fs::create_dir_all(&outside).unwrap();
    zip(&s, &outside.join("x.zip"), &["p.txt"], &[]);
    std::os::unix::fs::symlink(&outside, s.at("link")).unwrap();
    std::os::unix::fs::symlink(outside.join("x.zip"), s.at("zl.zip")).unwrap();
    assert!(peel(&s.root(), &["."]).status.success());
    assert!(!outside.join("x").exists(), "followed a symlinked dir");
    assert!(!s.at("zl").exists(), "followed a symlinked file");
}

#[test]
fn originals_untouched() {
    let s = Scratch::new("originals");
    zip(&s, &s.at("a.zip"), &["p.txt"], &[]);
    tool(
        &s.src(),
        &["tar", "czf", s.at("b.tar.gz").to_str().unwrap(), "p.txt"],
    );
    let (a, b) = (
        fs::read(s.at("a.zip")).unwrap(),
        fs::read(s.at("b.tar.gz")).unwrap(),
    );
    assert!(peel(&s.root(), &["."]).status.success());
    assert_eq!(a, fs::read(s.at("a.zip")).unwrap());
    assert_eq!(b, fs::read(s.at("b.tar.gz")).unwrap());
}

#[test]
fn no_dependencies() {
    let manifest =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    let deps: Vec<&str> = manifest
        .lines()
        .skip_while(|l| l.trim() != "[dependencies]")
        .skip(1)
        .take_while(|l| !l.trim_start().starts_with('['))
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .collect();
    assert!(deps.is_empty(), "dependencies crept in: {deps:?}");
}

#[test]
fn elf_opened_by_default() {
    let s = Scratch::new("elf");
    fs::copy("/usr/bin/true", s.at("truebin")).unwrap();
    let out = peel(&s.root(), &["."]);
    assert!(out.status.success(), "{}", text(&out.stdout));
    assert!(s.at("truebin_x").is_dir(), "{}", text(&out.stdout));
}

/// Write an executable `/bin/sh` stand-in for 7-Zip into the scratch dir.
fn stub(s: &Scratch, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = s.0.join(name);
    // Real 7-Zip prints its banner and exits when run with no arguments, and
    // peel reads that once to pick its switches. Stand-ins answer the same way,
    // unless the body handles that call itself.
    let banner = if body.starts_with("if [ $# -eq 0 ]") {
        ""
    } else {
        "if [ $# -eq 0 ]; then echo '7-Zip 26.04 (x64) : test stand-in'; exit 0; fi\n"
    };
    fs::write(&path, format!("#!/bin/sh\n{banner}{body}")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

/// A 7-Zip that cannot open anything, so every file falls through to binwalk.
/// Used with a stock ELF binary, which binwalk 3.1 carves on this host.
fn stub_7z(s: &Scratch) -> String {
    stub(
        s,
        "no-7z",
        "echo 'ERROR: stub' >&2\necho 'Cannot open the file as archive' >&2\nexit 2\n",
    )
}

/// Shell that pulls the `-o<dir>` argument out of a 7-Zip command line.
const STUB_OUT: &str =
    "out=\nfor a in \"$@\"; do case \"$a\" in -o*) out=\"${a#-o}\" ;; esac; done\n";

/// True while a process with this pid exists (zombies included).
fn alive(pid_file: &Path) -> bool {
    let pid = fs::read_to_string(pid_file).unwrap_or_default();
    let pid = pid.trim();
    !pid.is_empty() && Path::new("/proc").join(pid).exists()
}

/// Wait up to `limit` for `child` to exit.
fn wait_for(child: &mut std::process::Child, limit: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st;
        }
        assert!(
            start.elapsed() < limit,
            "peel did not exit within {limit:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn binwalk_carves_what_7z_cannot() {
    if !has_binwalk() {
        eprintln!("binwalk not installed, skipping");
        return;
    }
    let s = Scratch::new("carve");
    fs::copy("/usr/bin/true", s.at("fw.bin")).unwrap();
    let stub = stub_7z(&s);
    let out = peel_env(
        &s.root(),
        &["--binwalk", "--depth", "1", "."],
        &[("PEEL_7Z", &stub)],
    );
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(
        text(&out.stdout).contains("carved"),
        "{}",
        text(&out.stdout)
    );
    let carved = listing(&s.at("fw"));
    assert!(
        carved.iter().any(|p| p.ends_with("decompressed.bin")),
        "{carved:?}"
    );
    assert!(no_temp_dirs(&s.root()));
}

#[test]
fn binwalk_off_by_default() {
    let s = Scratch::new("nocarve");
    fs::copy("/usr/bin/true", s.at("fw.bin")).unwrap();
    let stub = stub_7z(&s);
    let out = peel_env(&s.root(), &["."], &[("PEEL_7Z", &stub)]);
    assert!(out.status.success());
    assert!(!s.at("fw").exists() && !s.at("fw.bin_x").exists());
}

#[test]
fn binwalk_flag_without_binwalk() {
    let s = Scratch::new("nobinwalk");
    zip(&s, &s.at("a.zip"), &["p.txt"], &[]);
    let before = listing(&s.root());
    let out = peel_env(
        &s.root(),
        &["--binwalk", "."],
        &[("PEEL_BINWALK", "/nonexistent/binwalk")],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("binwalk not found"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(before, listing(&s.root()));
}

/// A binwalk that "carves" one small file out of anything it is given, so the
/// binwalk depth is tested without binwalk installed. It writes what binwalk 3
/// writes (`<work>/<name>.extracted/<offset>/...`), and its bytes differ from
/// every input, so `prune_copies` keeps them.
fn stub_binwalk(s: &Scratch) -> String {
    stub(
        s,
        "fake-binwalk",
        "work=\nin=\nwhile [ $# -gt 0 ]; do\n\
         \x20 case \"$1\" in\n\
         \x20   -C) shift; work=\"$1\" ;;\n\
         \x20   --) shift; in=\"$1\"; break ;;\n\
         \x20 esac\n\
         \x20 shift\n\
         done\n\
         d=\"$work/${in##*/}.extracted/0\"\n\
         mkdir -p \"$d\" && echo 'carved by the binwalk stand-in' > \"$d/x\"\n",
    )
}

/// A plain file loose at layer 1, and another inside a zip at layer 2.
fn layered_plain_files(s: &Scratch) {
    fs::write(s.at("q.txt"), b"loose plain file\n").unwrap();
    zip(s, &s.at("a.zip"), &["p.txt"], &[]);
}

#[test]
fn binwalk_stops_at_layer_one_by_default() {
    let s = Scratch::new("bw-layer1");
    layered_plain_files(&s);
    let bw = stub_binwalk(&s);
    let out = peel_env(&s.root(), &["-b", "."], &[("PEEL_BINWALK", &bw)]);
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(s.at("q/0/x").is_file(), "{:?}", listing(&s.root()));
    assert_eq!(fs::read(s.at("a/p.txt")).unwrap(), PAYLOAD);
    assert!(
        !s.at("a/p").exists() && !s.at("a/p.txt_x").exists(),
        "a layer-2 file was carved: {:?}",
        listing(&s.root())
    );
    assert!(no_temp_dirs(&s.root()));
}

#[test]
fn binwalk_depth_reaches_inside_archives() {
    let s = Scratch::new("bw-layer2");
    layered_plain_files(&s);
    let bw = stub_binwalk(&s);
    let out = peel_env(
        &s.root(),
        &["--binwalk-depth", "2", "."],
        &[("PEEL_BINWALK", &bw)],
    );
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(s.at("a/p/0/x").is_file(), "{:?}", listing(&s.root()));
    // The carved file sits at layer 3, past the limit, so it is left alone.
    assert_eq!(listing(&s.at("a/p/0")), ["x"]);
    assert!(no_temp_dirs(&s.root()));
}

#[test]
fn targz_flattened() {
    let s = Scratch::new("flat");
    tool(
        &s.src(),
        &["tar", "czf", s.at("a.tar.gz").to_str().unwrap(), "p.txt"],
    );
    assert!(peel(&s.root(), &["."]).status.success());
    assert_eq!(listing(&s.at("a")), ["p.txt"]);
}

#[test]
fn max_size_precheck() {
    let s = Scratch::new("cap-list");
    fs::write(s.src().join("big.txt"), vec![b'a'; 4096]).unwrap();
    zip(&s, &s.at("big.zip"), &["big.txt"], &[]);
    let out = peel(&s.root(), &["--max-size", "1K", "."]);
    assert!(out.status.success());
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("too big") && stdout.contains(": lists"),
        "{stdout}"
    );
    assert!(!s.at("big").exists());
}

#[test]
fn max_size_after_exit() {
    let s = Scratch::new("cap-after");
    fs::write(s.src().join("big.txt"), vec![b'a'; 4096]).unwrap();
    compress_to(&s.src(), &["gzip", "-c", "big.txt"], &s.at("big.txt.gz"));
    let out = peel(&s.root(), &["--max-size", "1K", "."]);
    assert!(out.status.success());
    let stdout = text(&out.stdout);
    // A 4 KiB gunzip can finish before the first watchdog poll ("unpacked to")
    // or be caught mid-write ("stopped"): either way it is refused and nothing
    // stays behind. watchdog_kills_runaway_extractor pins the kill path.
    assert!(stdout.contains("too big"), "{stdout}");
    assert!(
        stdout.contains("unpacked to") || stdout.contains("stopped"),
        "{stdout}"
    );
    assert!(!s.at("big.txt").exists() && no_temp_dirs(&s.root()));
}

#[test]
fn watchdog_kills_runaway_extractor() {
    let s = Scratch::new("cap-kill");
    let pid_file = s.0.join("stub.pid");
    // Lists a gzip (no trusted size, so no pre-check), then writes forever.
    let body = format!(
        "case \"$1\" in\n  l) printf -- '--\\nPath = x\\nType = gzip\\n\\n----------\\nPath = y\\n'; exit 0 ;;\nesac\n{STUB_OUT}echo $$ > '{}'\nwhile :; do head -c 65536 /dev/zero >> \"$out/runaway\"; done\n",
        pid_file.display()
    );
    let slow = stub(&s, "runaway-7z", &body);
    fs::write(s.at("x.gz"), b"stand-in").unwrap();
    let start = Instant::now();
    let out = peel_env(
        &s.root(),
        &["--max-size", "256K", "."],
        &[("PEEL_7Z", &slow)],
    );
    assert!(start.elapsed() < Duration::from_secs(10));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("too big") && stdout.contains("stopped"),
        "{stdout}"
    );
    assert!(!alive(&pid_file), "the runaway extractor survived");
    assert!(
        no_temp_dirs(&s.root()) && !s.at("x").exists(),
        "{:?}",
        listing(&s.root())
    );
}

#[test]
fn same_stem_collision() {
    let s = Scratch::new("stem");
    zip(&s, &s.at("a.zip"), &["p.txt"], &[]);
    zip(&s, &s.at("a.7z"), &["p.txt"], &[]);
    assert!(peel(&s.root(), &["."]).status.success());
    assert_eq!(fs::read(s.at("a/p.txt")).unwrap(), PAYLOAD);
    let other = if s.at("a.zip_x").exists() {
        "a.zip_x"
    } else {
        "a.7z_x"
    };
    assert_eq!(fs::read(s.at(other).join("p.txt")).unwrap(), PAYLOAD);
    let again = peel(&s.root(), &["."]);
    let err = text(&again.stderr);
    assert!(
        err.contains("0 extracted") && err.contains("2 already done"),
        "{err}"
    );
}

#[test]
fn unopenable_rar_reported() {
    let s = Scratch::new("rar");
    fs::write(s.at("fake.rar"), b"this is not really a rar archive\n").unwrap();
    let out = peel(&s.root(), &["."]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("failed") && stdout.contains("fake.rar"),
        "{stdout}"
    );
    assert!(stdout.contains("no backend could open it"), "{stdout}");
}

#[test]
fn ctrl_c_stops_cleanly() {
    let s = Scratch::new("sigint");
    let pid_file = s.0.join("stub.pid");
    // Lists everything as a zip, writes a partial file, then hangs like a slow
    // multi-gigabyte extraction would.
    let body = format!(
        "case \"$1\" in\n  l) printf -- '--\\nPath = x\\nType = zip\\n\\n----------\\nPath = p.txt\\nSize = 3\\n'; exit 0 ;;\nesac\n{STUB_OUT}echo partial > \"$out/partial\"\necho $$ > '{}'\nexec sleep 30\n",
        pid_file.display()
    );
    let slow = stub(&s, "slow-7z", &body);
    zip(&s, &s.at("slow.zip"), &["p.txt"], &[]);
    let mut child = Command::new(PEEL)
        .arg(".")
        .current_dir(s.root())
        .env("PEEL_7Z", &slow)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !alive(&pid_file) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "stub never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    tool(&s.root(), &["kill", "-INT", &child.id().to_string()]);
    let status = wait_for(&mut child, Duration::from_secs(2));
    assert_eq!(status.code(), Some(130));
    assert!(!alive(&pid_file), "the extractor survived Ctrl-C");
    assert!(no_temp_dirs(&s.root()), "{:?}", listing(&s.root()));
    assert!(!s.at("slow").exists());
}

#[test]
fn no_temp_left() {
    let s = Scratch::new("mixed");
    zip(&s, &s.at("good.zip"), &["p.txt"], &[]);
    zip(&s, &s.at("enc.zip"), &["p.txt"], &["-psecret"]);
    fs::write(s.src().join("big.txt"), vec![b'a'; 4096]).unwrap();
    compress_to(&s.src(), &["gzip", "-c", "big.txt"], &s.at("big.txt.gz"));
    let whole = fs::read(s.at("big.txt.gz")).unwrap();
    fs::write(s.at("cut.gz"), &whole[..20]).unwrap();
    let out = peel(&s.root(), &["--max-size", "1K", "."]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let stdout = text(&out.stdout);
    for kind in ["extracted", "locked", "too big", "failed"] {
        assert!(stdout.contains(kind), "no {kind:?} case ran: {stdout}");
    }
    assert!(s.at("good/p.txt").is_file());
    assert!(no_temp_dirs(&s.root()), "{:?}", listing(&s.root()));
}

#[test]
fn wildcard_names_are_literal() {
    let s = Scratch::new("glob");
    for (name, body) in [("star.txt", "star\n"), ("A.txt", "A\n"), ("B.txt", "B\n")] {
        fs::write(s.src().join(name), body).unwrap();
    }
    zip(&s, &s.at("?.zip"), &["star.txt"], &[]);
    zip(&s, &s.at("a.zip"), &["A.txt"], &[]);
    zip(&s, &s.at("b.zip"), &["B.txt"], &[]);
    let out = peel(&s.root(), &["."]);
    assert!(out.status.success(), "{}", text(&out.stdout));
    assert_eq!(listing(&s.at("?")), ["star.txt"]);
    assert_eq!(listing(&s.at("a")), ["A.txt"]);
}

#[test]
fn overlapping_roots_open_once() {
    let s = Scratch::new("overlap");
    fs::create_dir_all(s.at("sub")).unwrap();
    zip(&s, &s.at("a.zip"), &["p.txt"], &[]);
    zip(&s, &s.at("sub/b.zip"), &["p.txt"], &[]);
    let out = peel(&s.root(), &[".", ".", "sub", "a.zip", "./sub/b.zip"]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", text(&out.stderr));
    assert_eq!(stdout.matches("extracted").count(), 2, "{stdout}");
    assert!(!s.at("a.zip_x").exists() && !s.at("sub/b.zip_x").exists());
}

#[test]
fn depth_cap_survives_rerun() {
    let s = Scratch::new("depth-rerun");
    zip(&s, &s.src().join("inner.zip"), &["p.txt"], &[]);
    zip(&s, &s.at("outer.zip"), &["inner.zip"], &[]);
    assert!(peel(&s.root(), &["-d", "1", "."]).status.success());
    let again = peel(&s.root(), &["-d", "1", "."]);
    assert!(again.status.success());
    assert!(
        !s.at("outer/inner").exists(),
        "a re-run peeled past the depth cap"
    );
    assert!(
        text(&again.stdout).contains("too deep"),
        "{}",
        text(&again.stdout)
    );
}

#[test]
fn unmarked_dir_not_trusted() {
    let s = Scratch::new("unmarked");
    fs::create_dir_all(s.at("foo")).unwrap();
    fs::write(s.at("foo/mine.txt"), b"not from any archive\n").unwrap();
    zip(&s, &s.at("foo.zip"), &["p.txt"], &[]);
    assert!(peel(&s.root(), &["."]).status.success());
    assert_eq!(fs::read(s.at("foo.zip_x/p.txt")).unwrap(), PAYLOAD);
    assert_eq!(listing(&s.at("foo")), ["mine.txt"]);
}

#[test]
fn new_sibling_after_run_is_opened() {
    let s = Scratch::new("sibling");
    zip(&s, &s.at("a.zip"), &["p.txt"], &[]);
    assert!(peel(&s.root(), &["."]).status.success());
    zip(&s, &s.at("a.7z"), &["p.txt"], &[]);
    let out = peel(&s.root(), &["."]);
    assert!(out.status.success());
    assert_eq!(fs::read(s.at("a.7z_x/p.txt")).unwrap(), PAYLOAD);
    let err = text(&out.stderr);
    assert!(
        err.contains("1 extracted") && err.contains("1 already done"),
        "{err}"
    );
}

#[test]
fn locked_sibling_still_reported() {
    let s = Scratch::new("locked-sibling");
    zip(&s, &s.at("a.zip"), &["p.txt"], &[]);
    zip(&s, &s.at("a.7z"), &["p.txt"], &["-psecret", "-mhe=on"]);
    assert!(peel(&s.root(), &["."]).status.success());
    let again = peel(&s.root(), &["."]);
    assert!(
        text(&again.stdout).contains("locked"),
        "{}",
        text(&again.stdout)
    );
    assert!(
        text(&again.stderr).contains("1 locked"),
        "{}",
        text(&again.stderr)
    );
}

#[test]
fn extensionless_beside_x_zip() {
    let s = Scratch::new("x-zip");
    zip(&s, &s.at("tmp.zip"), &["p.txt"], &[]);
    fs::rename(s.at("tmp.zip"), s.at("foo")).unwrap();
    fs::write(s.src().join("q.txt"), b"other\n").unwrap();
    zip(&s, &s.at("foo_x.zip"), &["q.txt"], &[]);
    let out = peel(&s.root(), &["."]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert_eq!(stdout.matches("extracted").count(), 2, "{stdout}");
    let again = text(&peel(&s.root(), &["."]).stderr);
    assert!(
        again.contains("0 extracted") && again.contains("2 already done"),
        "{again}"
    );
}

#[test]
fn dotdot_zip_named_safely() {
    let s = Scratch::new("dotdot");
    zip(&s, &s.at("..zip"), &["p.txt"], &[]);
    let out = peel(&s.root(), &["."]);
    assert!(out.status.success(), "{}", text(&out.stdout));
    assert_eq!(fs::read(s.at("..zip_x/p.txt")).unwrap(), PAYLOAD);
    assert!(!s.root().join("p.txt").exists() && !s.0.join("p.txt").exists());
}

#[test]
fn targz_budget_counts_final_output() {
    let s = Scratch::new("tar-budget");
    fs::write(s.src().join("seven.txt"), vec![b'z'; 7000]).unwrap();
    tool(
        &s.src(),
        &[
            "tar",
            "czf",
            s.at("t.tar.gz").to_str().unwrap(),
            "seven.txt",
        ],
    );
    let out = peel(&s.root(), &["-m", "16K", "."]);
    assert!(out.status.success(), "{}", text(&out.stdout));
    assert_eq!(
        fs::read(s.at("t/seven.txt")).unwrap().len(),
        7000,
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn carved_rerun_counts_done() {
    if !has_binwalk() {
        eprintln!("binwalk not installed, skipping");
        return;
    }
    let s = Scratch::new("carve-rerun");
    fs::copy("/usr/bin/true", s.at("fw.bin")).unwrap();
    let stub = stub_7z(&s);
    let env = [("PEEL_7Z", stub.as_str())];
    assert!(
        peel_env(&s.root(), &["-b", "-d", "1", "."], &env)
            .status
            .success()
    );
    let again = text(&peel_env(&s.root(), &["-b", "-d", "1", "."], &env).stderr);
    assert!(
        again.contains("0 extracted") && again.contains("1 already done"),
        "{again}"
    );
}

#[test]
fn dry_run_carve_writes_nothing() {
    if !has_binwalk() {
        eprintln!("binwalk not installed, skipping");
        return;
    }
    let s = Scratch::new("dry-carve");
    fs::copy("/usr/bin/true", s.at("fw.bin")).unwrap();
    let stub = stub_7z(&s);
    let before = listing(&s.root());
    let out = peel_env(&s.root(), &["-b", "-n", "."], &[("PEEL_7Z", &stub)]);
    assert!(out.status.success());
    assert!(
        text(&out.stdout).contains("would carve"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(before, listing(&s.root()));
}

#[test]
fn depth_holds_when_archive_breaks_later() {
    let s = Scratch::new("depth-broken");
    zip(&s, &s.src().join("inner.zip"), &["p.txt"], &[]);
    zip(&s, &s.at("outer.zip"), &["inner.zip"], &[]);
    assert!(peel(&s.root(), &["-d", "1", "."]).status.success());
    // The archive is damaged after the first run, so this time it fails to open.
    let whole = fs::read(s.at("outer.zip")).unwrap();
    fs::write(s.at("outer.zip"), &whole[..whole.len() / 3]).unwrap();
    let again = peel(&s.root(), &["-d", "1", "."]);
    assert!(!s.at("outer/inner").exists(), "{}", text(&again.stdout));
}

/// Run `peel` through `sh -c`, so the script can plant names that use peel's own
/// pid (`$$` survives `exec`) before peel starts.
fn peel_after(cwd: &Path, setup: &str, args: &str, env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(format!("{setup}\nexec '{PEEL}' {args}"))
        .current_dir(cwd)
        .stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

/// Entries in `dir` (not recursive) whose names start with `prefix`.
fn entries_starting(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .map(|e| e.path())
        .collect()
}

#[test]
fn planted_temp_dirs_are_never_touched() {
    let s = Scratch::new("planted-dir");
    zip(&s, &s.at("a.zip"), &["p.txt"], &[]);
    // Directories at the names the old scheme would have picked for this run.
    let setup =
        "for n in 0 1 2 3; do mkdir .peel-tmp-$$-$n && echo keep > .peel-tmp-$$-$n/keep.txt; done";
    let out = peel_after(&s.root(), setup, ".", &[]);
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert_eq!(fs::read(s.at("a/p.txt")).unwrap(), PAYLOAD);
    let planted = entries_starting(&s.root(), ".peel-tmp-");
    assert_eq!(planted.len(), 4, "{planted:?}");
    for dir in planted {
        assert_eq!(
            fs::read(dir.join("keep.txt")).unwrap(),
            b"keep\n",
            "{dir:?} was touched"
        );
    }
}

#[test]
fn planted_symlink_cannot_redirect_binwalk() {
    if !has_binwalk() {
        eprintln!("binwalk not installed, skipping");
        return;
    }
    let s = Scratch::new("planted-link");
    let victim = s.0.join("victim");
    fs::create_dir_all(&victim).unwrap();
    fs::copy("/usr/bin/true", s.at("fw.bin")).unwrap();
    let stub = stub_7z(&s);
    let setup = "for n in 0 1 2 3; do ln -s ../victim .peel-tmp-$$-$n; done";
    let out = peel_after(&s.root(), setup, "--binwalk -d 1 .", &[("PEEL_7Z", &stub)]);
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(
        listing(&victim).is_empty(),
        "binwalk wrote through a planted link: {:?}",
        listing(&victim)
    );
}

#[test]
fn reserved_names_are_reported() {
    let s = Scratch::new("reserved");
    fs::create_dir_all(s.at(".peel-tmp-x")).unwrap();
    fs::create_dir_all(s.at("sub")).unwrap();
    zip(&s, &s.at(".peel-tmp-x/inner.zip"), &["p.txt"], &[]);
    zip(&s, &s.at("tmp1.zip"), &["p.txt"], &[]);
    fs::rename(s.at("tmp1.zip"), s.at(".peel-src")).unwrap();
    zip(&s, &s.at("tmp2.zip"), &["p.txt"], &[]);
    fs::rename(s.at("tmp2.zip"), s.at("sub/.peel-tmpfoo")).unwrap();
    let out = peel(&s.root(), &["."]);
    assert!(out.status.success(), "{}", text(&out.stdout));
    assert_eq!(
        text(&out.stdout).matches("skipped").count(),
        3,
        "{}",
        text(&out.stdout)
    );
    assert!(
        text(&out.stderr).contains("3 skipped"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn control_chars_in_names_are_escaped() {
    let s = Scratch::new("ctrl");
    zip(&s, &s.at("tmp1.zip"), &["p.txt"], &[]);
    fs::rename(s.at("tmp1.zip"), s.at("a\nfailed     forged.zip")).unwrap();
    zip(&s, &s.at("tmp2.zip"), &["p.txt"], &[]);
    fs::rename(s.at("tmp2.zip"), s.at("b\x1b[31mred.zip")).unwrap();
    let out = peel(&s.root(), &["."]);
    assert!(out.status.success());
    let stdout = text(&out.stdout);
    assert_eq!(
        stdout.lines().count(),
        2,
        "a name forged an output line: {stdout:?}"
    );
    assert!(
        !stdout.contains('\x1b'),
        "raw escape sequence reached the terminal: {stdout:?}"
    );
}

#[test]
fn second_signal_kills_extractor_grandchildren() {
    let s = Scratch::new("sig2");
    let pid_file = s.0.join("grandchild.pid");
    // The extractor leaves a grandchild, which PDEATHSIG alone does not cover.
    let body = format!(
        "case \"$1\" in\n  l) printf -- '--\\nPath = x\\nType = zip\\n\\n----------\\nPath = p.txt\\nSize = 3\\n'; exit 0 ;;\nesac\nsleep 30 &\necho $! > '{}'\nwait\n",
        pid_file.display()
    );
    let slow = stub(&s, "slow-7z", &body);
    zip(&s, &s.at("slow.zip"), &["p.txt"], &[]);
    let mut child = Command::new(PEEL)
        .arg(".")
        .current_dir(s.root())
        .env("PEEL_7Z", &slow)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !alive(&pid_file) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "stub never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid = child.id().to_string();
    tool(
        &s.root(),
        &["sh", "-c", &format!("kill -INT {pid}; kill -INT {pid}")],
    );
    let status = wait_for(&mut child, Duration::from_secs(2));
    assert_eq!(status.code(), Some(130));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!alive(&pid_file), "an extractor's grandchild outlived peel");
}

#[test]
fn stop_signal_ends_a_hung_probe() {
    let s = Scratch::new("probe-hang");
    let pid_file = s.0.join("probe.pid");
    let body = format!(
        "case \"$1\" in\n  l) sleep 30 &\n     echo $! > '{}'\n     wait ;;\nesac\nexit 2\n",
        pid_file.display()
    );
    let hang = stub(&s, "hang-7z", &body);
    fs::write(s.at("x.zip"), b"stand-in").unwrap();
    let mut child = Command::new(PEEL)
        .arg(".")
        .current_dir(s.root())
        .env("PEEL_7Z", &hang)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !alive(&pid_file) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "stub never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    tool(&s.root(), &["kill", "-INT", &child.id().to_string()]);
    let status = wait_for(&mut child, Duration::from_secs(2));
    assert_eq!(status.code(), Some(130));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!alive(&pid_file), "the probe's child outlived peel");
}

#[test]
fn deep_tree_extracts_without_crashing() {
    let s = Scratch::new("deep");
    let mut deep = s.src().join("deep");
    for _ in 0..1000 {
        deep.push("d");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("bottom.txt"), PAYLOAD).unwrap();
    zip(&s, &s.at("deep.zip"), &["deep"], &[]);
    let out = peel(&s.root(), &["."]);
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(
        text(&out.stdout).contains("extracted"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn path_lookup_ignores_relative_entries() {
    let s = Scratch::new("path-rel");
    zip(&s, &s.at("a.zip"), &["p.txt"], &[]);
    // A `7zz` planted in the folder being peeled, which would win the lookup
    // through an empty PATH entry ("." by POSIX convention) if it counted.
    let marker = s.0.join("planted-ran");
    let planted = s.at("7zz");
    fs::write(
        &planted,
        format!("#!/bin/sh\ntouch '{}'\nexit 2\n", marker.display()),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&planted, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!("{}:", std::env::var("PATH").unwrap_or_default());
    let out = peel_env(&s.root(), &["."], &[("PATH", &path)]);
    assert!(
        !marker.exists(),
        "peel ran a 7zz from the tree being peeled"
    );
    assert_eq!(
        fs::read(s.at("a/p.txt")).unwrap(),
        PAYLOAD,
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn strict_flag_fails_on_anything_unopened() {
    let s = Scratch::new("strict");
    zip(&s, &s.at("enc.zip"), &["p.txt"], &["-psecret"]);
    assert_eq!(peel(&s.root(), &["."]).status.code(), Some(0));
    assert_eq!(peel(&s.root(), &["--strict", "."]).status.code(), Some(1));
}

#[test]
fn max_files_precheck() {
    let s = Scratch::new("files-list");
    let names: Vec<String> = (0..5).map(|i| format!("f{i}.txt")).collect();
    for n in &names {
        fs::write(s.src().join(n), b"x").unwrap();
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    zip(&s, &s.at("many.zip"), &refs, &[]);
    let out = peel(&s.root(), &["--max-files", "3", "."]);
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("too big") && stdout.contains("5 entries"),
        "{stdout}"
    );
    assert!(!s.at("many").exists());
}

#[test]
fn max_files_watchdog_stops_a_file_flood() {
    let s = Scratch::new("files-flood");
    let pid_file = s.0.join("stub.pid");
    let body = format!(
        "case \"$1\" in\n  l) printf -- '--\\nPath = x\\nType = gzip\\n\\n----------\\nPath = y\\n'; exit 0 ;;\nesac\n{STUB_OUT}echo $$ > '{}'\ni=0\nwhile :; do : > \"$out/f$i\"; i=$((i+1)); done\n",
        pid_file.display()
    );
    let flood = stub(&s, "flood-7z", &body);
    fs::write(s.at("x.gz"), b"stand-in").unwrap();
    let out = peel_env(
        &s.root(),
        &["--max-files", "200", "."],
        &[("PEEL_7Z", &flood)],
    );
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("too big") && stdout.contains("files"),
        "{stdout}"
    );
    assert!(!alive(&pid_file));
    assert!(no_temp_dirs(&s.root()) && !s.at("x").exists());
}

#[test]
fn timeout_stops_a_hung_extraction() {
    let s = Scratch::new("timeout");
    let pid_file = s.0.join("stub.pid");
    let body = format!(
        "case \"$1\" in\n  l) printf -- '--\\nPath = x\\nType = zip\\n\\n----------\\nPath = p.txt\\nSize = 3\\n'; exit 0 ;;\nesac\necho $$ > '{}'\nexec sleep 30\n",
        pid_file.display()
    );
    let slow = stub(&s, "slow-7z", &body);
    zip(&s, &s.at("slow.zip"), &["p.txt"], &[]);
    let start = Instant::now();
    let out = peel_env(&s.root(), &["--timeout", "1s", "."], &[("PEEL_7Z", &slow)]);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "took {:?}",
        start.elapsed()
    );
    let stdout = text(&out.stdout);
    assert!(stdout.contains("timed out"), "{stdout}");
    assert!(
        text(&out.stderr).contains("1 timed out"),
        "{}",
        text(&out.stderr)
    );
    assert!(!alive(&pid_file));
    assert!(no_temp_dirs(&s.root()));
}

#[test]
fn total_size_caps_the_whole_run() {
    let s = Scratch::new("total");
    fs::write(s.src().join("big.txt"), vec![b'a'; 4096]).unwrap();
    for n in ["a.zip", "b.zip", "c.zip"] {
        zip(&s, &s.at(n), &["big.txt"], &[]);
    }
    let out = peel(&s.root(), &["-j", "1", "--total-size", "6K", "."]);
    let stdout = text(&out.stdout);
    assert_eq!(stdout.matches("extracted").count(), 1, "{stdout}");
    assert_eq!(stdout.matches("run budget").count(), 2, "{stdout}");
}

#[test]
fn crash_leftovers_are_removed_only_when_their_run_is_dead() {
    let s = Scratch::new("leftovers");
    zip(&s, &s.at("a.zip"), &["p.txt"], &[]);
    // A pid that has certainly exited: a short child we already reaped.
    let dead = Command::new("true").spawn().unwrap();
    let dead_pid = dead.id();
    let mut dead = dead;
    dead.wait().unwrap();
    let live_pid = std::process::id();
    let stale = s.at(&format!(".peel-tmp-{dead_pid}-0123456789abcdef-0"));
    let live = s.at(&format!(".peel-tmp-{live_pid}-0123456789abcdef-0"));
    let lookalike = s.at(&format!(".peel-tmp-{dead_pid}-notours"));
    for d in [&stale, &live, &lookalike] {
        fs::create_dir_all(d).unwrap();
        fs::write(d.join("partial"), b"half").unwrap();
        // The ownership proof peel's own make_temp writes: its run token.
        mark_as_peel_temp(d, "0123456789abcdef");
    }
    let victim = s.0.join("victim");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("keep.txt"), b"keep").unwrap();
    let linked = s.at(&format!(".peel-tmp-{dead_pid}-fedcba9876543210-1"));
    std::os::unix::fs::symlink(&victim, &linked).unwrap();

    // A dry run removes nothing.
    assert!(peel(&s.root(), &["-n", "."]).status.success());
    assert!(stale.exists());

    let out = peel(&s.root(), &["."]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(!stale.exists(), "a dead run's leftover was kept: {stdout}");
    assert!(stdout.contains("removed"), "{stdout}");
    assert!(
        live.join("partial").exists(),
        "a live run's work dir was removed"
    );
    assert!(
        lookalike.join("partial").exists(),
        "a look-alike name was removed"
    );
    assert!(
        fs::symlink_metadata(&linked).is_ok(),
        "a planted symlink was removed"
    );
    assert_eq!(fs::read(victim.join("keep.txt")).unwrap(), b"keep");
    assert_eq!(fs::read(s.at("a/p.txt")).unwrap(), PAYLOAD);
}

#[test]
fn children_get_only_absolute_path_entries() {
    let s = Scratch::new("child-path");
    fs::copy("/usr/bin/true", s.at("fw.bin")).unwrap();
    let seen = s.0.join("binwalk-path.txt");
    // A binwalk stand-in that records the PATH it was given and carves nothing.
    let recorder = stub(
        &s,
        "record-binwalk",
        &format!("printf '%s' \"$PATH\" > '{}'\nexit 0\n", seen.display()),
    );
    let no7z = stub_7z(&s);
    let path = format!("{}::rel/bin:.", std::env::var("PATH").unwrap_or_default());
    let out = peel_env(
        &s.root(),
        &["--binwalk", "."],
        &[
            ("PATH", &path),
            ("PEEL_7Z", &no7z),
            ("PEEL_BINWALK", &recorder),
        ],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let recorded = fs::read_to_string(&seen).expect("binwalk stand-in never ran");
    assert!(!recorded.is_empty());
    for entry in recorded.split(':') {
        assert!(
            entry.starts_with('/'),
            "child got PATH entry {entry:?} in {recorded:?}"
        );
    }
}

#[test]
fn child_gets_no_path_when_none_is_absolute() {
    let s = Scratch::new("child-no-path");
    fs::write(s.at("plain.txt"), b"not an archive").unwrap();
    // Each stand-in records "ran" and then only the PATH line of the
    // environment it was given, if there is one. It reads that environment
    // through /usr/bin/env, by absolute path, because the shell's own `$PATH`
    // shows a built-in default when none was passed, and that would hide the
    // difference. The filter uses shell builtins only, so it needs no lookup.
    let record = |file: &Path| {
        format!(
            "printf 'ran\\n' > '{0}'\n/usr/bin/env | while IFS= read -r l; do \
             case \"$l\" in PATH=*) printf '%s\\n' \"$l\" ;; esac; done >> '{0}'\n",
            file.display()
        )
    };
    let probe_seen = s.0.join("probe-path.txt");
    let carve_seen = s.0.join("binwalk-path.txt");
    let no7z = stub(
        &s,
        "record-7z",
        &format!(
            "{}echo 'Cannot open the file as archive' >&2\nexit 2\n",
            record(&probe_seen)
        ),
    );
    let nocarve = stub(
        &s,
        "record-binwalk",
        &format!("{}exit 0\n", record(&carve_seen)),
    );
    // Not one absolute entry: an empty PATH passed on would mean the current
    // directory, the tree being peeled. The probe gets `isolate_light`, and
    // binwalk, as an extractor, gets `isolate_child`.
    let out = peel_env(
        &s.root(),
        &["--binwalk", "."],
        &[
            ("PATH", "rel/bin:."),
            ("PEEL_7Z", &no7z),
            ("PEEL_BINWALK", &nocarve),
        ],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    for (who, seen) in [("probe", &probe_seen), ("binwalk", &carve_seen)] {
        let got = fs::read_to_string(seen).unwrap_or_default();
        assert!(got.starts_with("ran\n"), "{who} stand-in never ran");
        let path = got.lines().find_map(|l| l.strip_prefix("PATH="));
        assert_eq!(path, None, "{who} got PATH={path:?}");
    }
}

#[test]
fn timeout_also_stops_a_hung_probe() {
    let s = Scratch::new("probe-timeout");
    let pid_file = s.0.join("probe.pid");
    let body = format!(
        "case \"$1\" in\n  l) echo $$ > '{}'\n     exec sleep 30 ;;\nesac\nexit 2\n",
        pid_file.display()
    );
    let hang = stub(&s, "hang-7z", &body);
    fs::write(s.at("x.zip"), b"stand-in").unwrap();
    let start = Instant::now();
    let out = peel_env(&s.root(), &["--timeout", "1s", "."], &[("PEEL_7Z", &hang)]);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "took {:?}",
        start.elapsed()
    );
    assert!(
        text(&out.stdout).contains("timed out"),
        "{}",
        text(&out.stdout)
    );
    assert!(!alive(&pid_file), "the hung probe survived");
}

#[test]
fn total_size_holds_with_parallel_jobs() {
    let s = Scratch::new("total-par");
    fs::write(s.src().join("big.txt"), vec![b'a'; 4096]).unwrap();
    for n in ["a.zip", "b.zip", "c.zip", "d.zip", "e.zip", "f.zip"] {
        zip(&s, &s.at(n), &["big.txt"], &[]);
    }
    for _ in 0..5 {
        let _ = fs::remove_dir_all(s.root().join("a"));
        for n in ["a", "b", "c", "d", "e", "f"] {
            let _ = fs::remove_dir_all(s.at(n));
        }
        let out = peel(&s.root(), &["-j", "6", "--total-size", "6K", "."]);
        let stdout = text(&out.stdout);
        assert_eq!(stdout.matches("extracted").count(), 1, "{stdout}");
    }
}

#[test]
fn old_7zip_without_spod_still_extracts() {
    let s = Scratch::new("old7z");
    // A 7-Zip 23.01 stand-in: refuses -spod, as builds before 26.01 do.
    let body = format!(
        "if [ $# -eq 0 ]; then echo; echo '7-Zip 23.01 (x64) : Copyright (c) 1999-2023 Igor Pavlov : 2023-06-20'; exit 0; fi\ncase \"$1\" in\n  l) printf -- '--\\nPath = x\\nType = zip\\n\\n----------\\nPath = p.txt\\nSize = 3\\n'; exit 0 ;;\nesac\nfor a in \"$@\"; do [ \"$a\" = -spod ] && {{ echo 'Command Line Error: Unsupported switch: -spod' >&2; exit 7; }}; done\n{STUB_OUT}echo hi > \"$out/p.txt\"\nexit 0\n"
    );
    let old = stub(&s, "old-7z", &body);
    fs::write(s.at("a.zip"), b"stand-in").unwrap();
    let out = peel_env(&s.root(), &["."], &[("PEEL_7Z", &old)]);
    assert!(out.status.success(), "{}", text(&out.stdout));
    assert_eq!(fs::read(s.at("a/p.txt")).unwrap(), b"hi\n");
}

/// Give `dir` the ownership proof peel's `make_temp` writes on its work dirs.
fn mark_as_peel_temp(dir: &Path, token: &str) {
    tool(
        dir.parent().unwrap(),
        &[
            "setfattr",
            "-n",
            "user.peel.tmp",
            "-v",
            token,
            dir.to_str().unwrap(),
        ],
    );
}

#[test]
fn archive_shipped_leftover_name_is_not_removed() {
    let s = Scratch::new("shipped-leftover");
    // An ordinary zip that happens to hold a folder in peel's work-dir format,
    // with a pid that does not exist. Content from an archive carries no
    // ownership proof, so it must be reported, never deleted.
    let shipped = ".peel-tmp-999999999-0123456789abcdef-0";
    fs::create_dir_all(s.src().join(shipped)).unwrap();
    fs::write(s.src().join(shipped).join("inside.txt"), PAYLOAD).unwrap();
    zip(&s, &s.at("carrier.zip"), &[shipped], &[]);
    let out = peel(&s.root(), &["."]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(!stdout.contains("removed"), "{stdout}");
    assert!(stdout.contains("skipped"), "{stdout}");
    assert_eq!(
        fs::read(s.at("carrier").join(shipped).join("inside.txt")).unwrap(),
        PAYLOAD
    );
}

#[test]
fn star_in_parent_name_cannot_redirect_output() {
    let s = Scratch::new("star-parent");
    // Reports 23.01, so peel leaves out -spod, but runs the installed 7-Zip,
    // which then reads a `*` in an -o path as "archive name".
    let real = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join("7z"))
        .find(|p| p.is_file())
        .expect("7z on PATH");
    let wrapper = stub(
        &s,
        "old-banner-7z",
        &format!(
            "if [ $# -eq 0 ]; then echo '7-Zip 23.01 (x64) : wrapper'; exit 0; fi\nexec '{}' \"$@\"\n",
            real.display()
        ),
    );
    fs::create_dir_all(s.at("w*x")).unwrap();
    zip(&s, &s.at("tmp.zip"), &["p.txt"], &[]);
    fs::rename(s.at("tmp.zip"), s.at("w*x/a.zip")).unwrap();
    let out = peel_env(&s.root(), &["."], &[("PEEL_7Z", &wrapper)]);
    assert!(out.status.success(), "{}", text(&out.stdout));
    assert_eq!(
        fs::read(s.at("w*x/a/p.txt")).unwrap_or_default(),
        PAYLOAD,
        "{:?}",
        listing(&s.root())
    );
    let top: Vec<String> = fs::read_dir(s.root())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(top, ["w*x"], "output escaped next to the folder: {top:?}");
}

#[test]
fn relative_backend_path_is_fixed_before_extraction() {
    let s = Scratch::new("rel-backend");
    let real = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join("7z"))
        .find(|p| p.is_file())
        .expect("7z on PATH");
    // peel starts in the scratch dir, where the backend the user named sits.
    stub(
        &s,
        "relbin-7z",
        &format!("exec '{}' \"$@\"\n", real.display()),
    );
    // A file at the same relative name inside the tree being peeled.
    fs::create_dir_all(s.at("sub")).unwrap();
    stub(&s, "root/sub/relbin-7z", "touch PLANTED_RAN\nexit 2\n");
    zip(&s, &s.at("tmp.zip"), &["p.txt"], &[]);
    fs::rename(s.at("tmp.zip"), s.at("sub/a.zip")).unwrap();
    let out = peel_env(&s.0, &["root/sub"], &[("PEEL_7Z", "./relbin-7z")]);
    assert!(
        !s.at("sub/PLANTED_RAN").exists(),
        "the planted backend ran: {}",
        text(&out.stdout)
    );
    assert!(out.status.success(), "{}", text(&out.stdout));
    assert_eq!(fs::read(s.at("sub/a/p.txt")).unwrap_or_default(), PAYLOAD);
}

#[test]
fn unknown_option_is_escaped() {
    let s = Scratch::new("unknown-escaped");
    // The argument holds an ESC, a bracket sequence and a newline. Printed raw,
    // it would put a live escape and a forged second line on stderr.
    let out = peel(&s.root(), &["--x\u{1b}[31mred\nforged"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = text(&out.stderr);
    assert_eq!(
        stderr.lines().next(),
        Some("peel: unknown option: --x\\x1b[31mred\\nforged"),
        "{stderr}"
    );
    let plain = peel(&s.root(), &["--nope"]);
    assert_eq!(plain.status.code(), Some(2));
    assert_eq!(
        text(&plain.stderr).lines().next(),
        Some("peel: unknown option: --nope")
    );
}
