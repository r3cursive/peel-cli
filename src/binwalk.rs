//! binwalk: carving files 7-Zip cannot open.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::exec::{Limits, RunEnd, Share, run_capped};
use crate::fsutil::{has_content, prune_copies, remove};
use crate::sevenzip::{Outcome, first_error};

/// The binwalk binary.
pub(crate) struct Binwalk {
    pub(crate) bin: PathBuf,
}

pub(crate) enum Carve {
    Got(PathBuf),
    Nothing,
    /// A watchdog limit stopped it (too big, too many files, budget, timeout).
    Limited(Outcome),
    Stopped,
    Failed(String),
}

impl Binwalk {
    /// One layer of binwalk carving into `work`. binwalk writes
    /// `<work>/<name>.extracted/<offset>/...` plus a symlink to the input; the
    /// `.extracted` tree is moved onto `stage` and returned. The caller creates
    /// both directories, exclusively, before the call, so binwalk never follows
    /// something planted at either name. `work` is removed afterwards; `stage` is
    /// left for the caller, empty when nothing was carved.
    pub(crate) fn carve(
        &self,
        input: &Path,
        name: &OsStr,
        work: &Path,
        stage: &Path,
        limits: &Limits,
        share: &mut Share,
    ) -> Carve {
        let mut cmd = Command::new(&self.bin);
        cmd.args(["-q", "-e", "-C"]).arg(work).arg("--").arg(input);
        let result = match run_capped(cmd, work, limits, share) {
            // binwalk 3 exits 0 even when every extractor fails, so the output decides.
            Ok(RunEnd::Exited(st, _)) if st.success() => {
                let mut dir = name.to_os_string();
                dir.push(".extracted");
                let extracted = work.join(dir);
                prune_copies(&extracted, input);
                if !has_content(&extracted) {
                    Carve::Nothing
                } else if let Err(e) = fs::rename(&extracted, stage) {
                    // `stage` is an empty directory this run made, which rename replaces.
                    Carve::Failed(format!("cannot stage binwalk output: {e}"))
                } else {
                    Carve::Got(stage.to_path_buf())
                }
            }
            Ok(RunEnd::Exited(_, err)) => Carve::Failed(format!("binwalk: {}", first_error(&err))),
            Ok(RunEnd::Stopped) => Carve::Stopped,
            Ok(RunEnd::Unmeasurable) => {
                Carve::Failed("output has an unreadable directory, so its size is unknown".into())
            }
            Ok(end) => Carve::Limited(Outcome::from_limit(end)),
            Err(e) => Carve::Failed(format!("cannot run binwalk: {e}")),
        };
        remove(work);
        result
    }
}
