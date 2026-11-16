//! Embeds the git commit of the build as WSCONNECT_COMMIT(_SHORT) for the update check.
//! `cargo install --git` builds inside cargo's git checkout, which is a real git repo. No git or
//! no commit: "unknown", the update check is skipped then. Override with env WSCONNECT_COMMIT.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=WSCONNECT_COMMIT");
    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));

    // Rebuild when HEAD moves: HEAD itself, the branch it points at, packed refs.
    if let Some(git_dir) = git(&dir, &["rev-parse", "--absolute-git-dir"]) {
        let git_dir = PathBuf::from(git_dir);
        let common = git(
            &dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .map(PathBuf::from)
        .unwrap_or_else(|| git_dir.clone());
        let mut watch = vec![git_dir.join("HEAD"), common.join("packed-refs")];
        if let Some(branch) = git(&dir, &["symbolic-ref", "-q", "HEAD"]) {
            watch.push(common.join(branch));
        }
        // A missing file would make cargo rerun this script on every build.
        for p in watch.iter().filter(|p| p.exists()) {
            println!("cargo:rerun-if-changed={}", p.display());
        }
    }

    let commit = std::env::var("WSCONNECT_COMMIT")
        .ok()
        .or_else(|| git(&dir, &["rev-parse", "HEAD"]))
        .filter(|c| c.len() == 40 && c.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(|c| c.to_ascii_lowercase())
        .unwrap_or_else(|| "unknown".into());
    let short = if commit == "unknown" {
        commit.clone()
    } else {
        commit[..7].to_string()
    };
    println!("cargo:rustc-env=WSCONNECT_COMMIT={commit}");
    println!("cargo:rustc-env=WSCONNECT_COMMIT_SHORT={short}");
}
