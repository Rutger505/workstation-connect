//! TRANSITIONAL shim for the rename wsconnect → workstation-connect (Nov 2026).
//! Pre-rename updaters run `wsconnect --version` after `cargo install` and restart
//! wsconnect.service (`env wsconnect daemon`); this execs the real binary so both still work and
//! the daemon can migrate itself (src/migrate.rs, which also deletes this file once unused).
//! Remove (with migrate.rs) once every laptop runs workstation-connect.service.

use std::os::unix::process::CommandExt;
use std::process::Command;

fn main() {
    let mut args = std::env::args_os();
    // argv[0] stays "wsconnect": migrate.rs detects old callers by it.
    let arg0 = args.next().unwrap_or_else(|| "wsconnect".into());
    let exe = std::env::current_exe().unwrap_or_default();
    let exe = match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(s) => s.into(),
        None => exe,
    };
    let target = exe.with_file_name("workstation-connect");
    let err = Command::new(&target).arg0(arg0).args(args).exec();
    eprintln!("wsconnect: exec {}: {err}", target.display());
    std::process::exit(127);
}
