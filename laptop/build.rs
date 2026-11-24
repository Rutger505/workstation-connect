//! Embeds the build's git commit as WSCONNECT_COMMIT(_SHORT) for the update check, and the
//! user-facing version WSCONNECT_VERSION = "v<n>", n = first-parent commits since
//! [`VERSION_BASE`] that touch laptop/. Builds run in a real git repo (the updater's full clone,
//! cargo's git checkout). No git, no commit or base missing (shallow clone): "unknown", the update
//! check is skipped then. Override with env WSCONNECT_COMMIT / WSCONNECT_VERSION.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Last commit before versioning; the first commit after it touching laptop/ is v1.
const VERSION_BASE: &str = "f0400b0ea30fdb87cc12aeab924df0611c70a169";

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

/// First-parent commits in VERSION_BASE..HEAD touching this dir (laptop/). None without git or
/// when the base isn't an ancestor of HEAD (missing in a shallow clone, unrelated history).
fn count_since_base(dir: &Path) -> Option<u32> {
    let ancestor = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["merge-base", "--is-ancestor", VERSION_BASE, "HEAD"])
        .output()
        .ok()?;
    if !ancestor.status.success() {
        return None;
    }
    let range = format!("{VERSION_BASE}..HEAD");
    git(
        dir,
        &["rev-list", "--count", "--first-parent", &range, "--", "."],
    )?
    .parse()
    .ok()
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=WSCONNECT_COMMIT");
    println!("cargo:rerun-if-env-changed=WSCONNECT_VERSION");
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

    let version = std::env::var("WSCONNECT_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| count_since_base(&dir).map(|n| format!("v{n}")))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=WSCONNECT_VERSION={version}");
}
