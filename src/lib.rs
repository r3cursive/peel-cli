//! peel: take apart every archive and compressed file under a folder, layer by layer.
//!
//! 7-Zip opens anything it recognizes by content. With `--binwalk`, binwalk carves
//! files 7-Zip cannot open, down to `--binwalk-depth` layers (1 by default). Each
//! input lands in `<stem>/` beside it (or `<name>_x/` when that name is taken), and
//! whatever comes out goes back on the queue until nothing new appears. Every
//! extraction runs in a hidden temp directory that is renamed into place only on
//! success, so a failed, locked, oversized or interrupted one leaves nothing. Each output directory records which input made it (an xattr,
//! or a marker file where the filesystem has no xattrs), so re-runs know what is done.

mod backend;
mod binwalk;
mod cli;
mod exec;
mod fsutil;
mod naming;
mod peeler;
mod provenance;
mod report;
mod roots;
mod sevenzip;
mod sys;

use std::env;
use std::process::ExitCode;
use std::sync::atomic::AtomicU64;
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use backend::find_program;
use binwalk::Binwalk;
use cli::{Cli, USAGE, parse_args};
use peeler::Peeler;
use report::{Reporter, Stats, count};
use roots::plan_roots;
use sevenzip::SevenZip;
use sys::{install_signal_handlers, stopping};

pub fn run_cli() -> ExitCode {
    let opts = match parse_args(env::args_os().skip(1)) {
        Ok(Cli::Run(o)) => o,
        Ok(Cli::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Cli::Version) => {
            println!("peel {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("peel: {e}\nTry 'peel --help'.");
            return ExitCode::from(2);
        }
    };
    // A bare `peel` must never start taking apart whatever folder it was typed in.
    if opts.paths.is_empty() {
        eprintln!("peel: no path given (use `peel .` for the current folder)\nTry 'peel --help'.");
        return ExitCode::from(2);
    }
    let Some(sevenz) = find_program("PEEL_7Z", &["7zz", "7z"]) else {
        eprintln!("peel: 7-Zip not found (install the 7zip package, or set PEEL_7Z)");
        return ExitCode::from(2);
    };
    let binwalk = if opts.binwalk {
        // Asked for explicitly, so a missing binwalk is an error, not a shrug.
        let Some(found) = find_program("PEEL_BINWALK", &["binwalk"]) else {
            eprintln!(
                "peel: --binwalk given but binwalk not found (install binwalk, or set PEEL_BINWALK)"
            );
            return ExitCode::from(2);
        };
        Some(Binwalk { bin: found })
    } else {
        None
    };
    install_signal_handlers();

    let (jobs, bad_roots) = plan_roots(&opts.paths);
    let spod = sevenzip::needs_spod(&sevenz);
    let sevenzip = SevenZip {
        bin: sevenz,
        password: opts.password.clone(),
        timeout: opts.timeout,
        spod,
    };
    let reporter = Reporter { quiet: opts.quiet };
    let (total_size, strict) = (opts.total_size, opts.strict);
    let peeler = Peeler {
        opts,
        sevenzip,
        binwalk,
        claims: Mutex::default(),
        queue: Mutex::default(),
        wake: Condvar::new(),
        stats: Stats::default(),
        reporter,
        temp_seq: AtomicU64::new(0),
        temp_token: random_token(),
        budget: (total_size > 0).then(|| exec::RunBudget::new(total_size)),
    };
    for (root, why) in &bad_roots {
        peeler
            .reporter
            .problem(&peeler.stats.failed, "failed", root, &format!(": {why}"));
    }
    peeler.push(jobs);

    let start = Instant::now();
    peeler.run();

    let summary = peeler.stats.summary(peeler.opts.dry_run);
    let interrupted = stopping();
    eprintln!(
        "peel: {}{summary} in {:.1}s",
        if interrupted { "interrupted; " } else { "" },
        start.elapsed().as_secs_f64()
    );
    if interrupted {
        ExitCode::from(130)
    } else if count(&peeler.stats.failed) > 0 || (strict && peeler.stats.unopened() > 0) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// 64 random bits for temp directory names, so nothing in the tree can plant
/// something at a name peel is about to use. Falls back to the clock and pid only
/// if /dev/urandom cannot be read.
fn random_token() -> u64 {
    use std::io::Read;
    let mut buf = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok()
    {
        return u64::from_ne_bytes(buf);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    nanos ^ (u64::from(std::process::id()) << 32)
}
