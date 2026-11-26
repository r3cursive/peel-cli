# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

peel is a zero-dependency Rust CLI that takes apart every archive under the paths it is given, layer by layer. It drives 7-Zip, and binwalk when `--binwalk` is passed. `README.md` covers usage and the backends. If an `OWNER.md` exists at the root, it holds the maintainer's own working rules: read it first.

## Commands

```sh
cargo build
cargo test                                  # unit tests (in src/) + end-to-end suite (tests/cli.rs)
cargo test --test cli wildcard_names_are_literal   # one end-to-end test
cargo test --lib stems                      # one unit test
cargo clippy --all-targets -- -D warnings   # must be clean
cargo fmt --check                           # must be clean
cargo install --path . --locked             # installs ~/.cargo/bin/peel
peel .                                      # a bare `peel` refuses on purpose (exit 2)
```

The end-to-end tests build fixtures with the system's archivers, so `7z`, `tar`, `gzip`, `bzip2`, `xz`, `zstd` and `setfattr` must be on PATH, and the 7-Zip build must read zstd. The binwalk tests skip themselves when binwalk is missing. Fixtures live under `target/tmp` (`CARGO_TARGET_TMPDIR`). Tests that need an extractor to misbehave (hang, write forever, reject everything) point `PEEL_7Z` at a `/bin/sh` stand-in written by the test, which answers a no-argument call with a 7-Zip banner. Tests of the carve path that must run without binwalk (and so in CI) point `PEEL_BINWALK` at `stub_binwalk`, which writes binwalk's output layout. Do the same for new tests of that kind, rather than building hostile archives.

## Architecture

The binary is `src/main.rs`, which only calls `peel::run_cli()` in `src/lib.rs`. One run goes like this:

1. `cli::parse_args` reads the options.
2. `backend::find_program` resolves `$PEEL_7Z`, then `7zz`, then `7z` (absolute PATH entries only), plus binwalk. `sevenzip::needs_spod` reads the 7-Zip version once.
3. `roots::plan_roots` canonicalizes the roots and drops duplicates and roots nested in another root.
4. `peeler::Peeler::run` starts N worker threads over a shared queue of `Job::Walk` and `Job::File`.

- **Walk** (`Peeler::walk`) lists a directory without following symlinks. It never opens anything at a reserved name (`.peel-tmp*`, `.peel-src`). It removes a work dir left by a dead peel run, and reports anything else at a reserved name as `skipped`. When a subdirectory's provenance names a sibling file, the walk hands that directory to the file's job instead of walking it. That is how the depth cap holds across re-runs.
- **File** (`handle_file`) asks `SevenZip::probe` what the file is (`7z l -slt`, streamed). Then one of these happens:
  - An earlier output with matching provenance exists. It counts as done and is walked one layer deeper.
  - The file is an archive. `open_archive` extracts it into a fresh temp dir, flattens a tar that came out of a gzip/xz/zstd stream, and commits.
  - `--binwalk` is on and 7-Zip could not open it. `try_carve` runs `Binwalk::carve`, but only at layers up to `--binwalk-depth` (default 1, the paths given and what is loose under them), and never past `--depth`.
- **Commit** (`Peeler::commit`) sets provenance on the staged dir first (`provenance.rs`: a `user.peel.src` xattr, or a `.peel-src` file only where the filesystem returns ENOTSUP). It then claims a free name from `naming::Names` (`<stem>`, `<name>_x`, `<name>_x2` … `_x9`), renames into place, and queues `Walk(target, level + 1)`.
- **Children** run through `exec::run_capped` (extractors) or directly (probes). A watchdog polls the temp dir and kills the whole group past a limit or on a stop signal, and the group is killed again after exit to catch stragglers. Signals and every libc declaration live in `sys.rs`.
- `report.rs` holds the counters and the summary line. Its wording, like every user-visible string, is asserted by `tests/cli.rs`.

## Invariants

These hold everywhere, and the tests check each one:

- Never follow a symlink while walking, and never write outside the input's parent directory.
- Temp dirs are `.peel-tmp-<pid>-<token>-<seq>`, where the token is 64 random bits per run. `Peeler::make_temp` creates each one exclusively before any extractor sees it (binwalk's `-C` dir and stage included) and marks it with a `user.peel.tmp` xattr holding the token. Only a dir it returned is ever removed, with one exception: the walk removes a leftover of a dead run. That requires a real directory, a name `leftover_of` accepts, an ownership mark matching the token in that name, and `sys::process_alive` returning false. It never happens in a dry run, and an archive cannot ship the mark.
- The opt-in limits (`--max-files`, `--timeout`, `--total-size`) live in `exec::Limits`, and the watchdog in `exec::run_capped` enforces them. `--timeout` also bounds probes. All of them default to off.
- `--total-size` is an `exec::RunBudget` shared across jobs. `Share::reserve` claims an archive's listed size before extraction, and a share only grows until `Share::commit` or drop. Never release bytes early while a job is still writing.
- Backends are looked up only in absolute PATH entries (`backend::search_path`). An empty or relative entry would mean the tree being peeled. An explicit `$PEEL_7Z` or `$PEEL_BINWALK` path is made absolute at startup, because 7-Zip runs with `current_dir` set inside the tree. Every child also gets a PATH of absolute entries only, or no PATH when none is left (never an empty one, which means the current directory), because binwalk runs its helpers by name from inside extracted content.
- Extractors use `sys::isolate_child` (process group plus PDEATHSIG through `pre_exec`). Probes use `sys::isolate_light` (process group only), because a `pre_exec` hook disables std's `posix_spawn` path and made the plain-file walk 4.5 times slower. Every child is registered in `sys::GroupGuard`, and a stop signal kills every registered group from the handler, so neither a hung probe nor a grandchild survives.
- The cap fails closed: `fsutil::tree_usage` returns `None` on an unreadable directory, and that refuses the extraction. Every tree traversal is iterative.
- `sevenzip::failure_kind` accepts only 7-Zip's complete message forms, after taking out the input's name and the `Path =` lines, so attacker-chosen names cannot steer locked or not-an-archive. Printed names and messages go through `report::clean`, which escapes control and bidi characters.
- Only a directory whose provenance names the input counts as done. An unmarked `<stem>/` is not trusted: the output goes to `<name>_x/`.
- Every 7-Zip call gets `-spd` (wildcards off) and a `-p` password, a dummy one when none is given, so nothing ever prompts. Extraction also gets `-spod` on 7-Zip 26.01 and newer. Every input path comes after `--`. Never pass `-snld`.
- `Cargo.toml` has no dependencies, and the `no_dependencies` test enforces that. libc functions are declared by hand.

## Working on this repo

- **Security defaults are deliberate.** Bring changes to a default, or any hardening that limits what peel can do, as options with a recommendation. Never change them silently.
- **A refactor must be pure moves.** `tests/cli.rs` is black-box and stays untouched. Compare the old and new binaries on the same input.
- **Test the behavior, then build it.** A fix starts with a test that fails on the old code.
- **Don't build hostile fixtures inline.** Reason from the code instead, or use a stand-in backend through `PEEL_7Z`.
- `.claude/agents/peel-auditor.md` is a read-only reviewer pre-briefed with these rules. Use it for second looks, and never let the agent that wrote a change audit it.
