//! Outdated-install check (daemon) and `wsconnect update` (CLI).
//! Installed = commit embedded by build.rs; latest = GitHub main. Only laptop/ changes count.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::time::{Duration, SystemTime};
use tokio::process::Command;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

pub const COMMIT: &str = env!("WSCONNECT_COMMIT");
pub const COMMIT_SHORT: &str = env!("WSCONNECT_COMMIT_SHORT");
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("WSCONNECT_COMMIT_SHORT"),
    ")"
);

const REPO: &str = "Rutger505/workstation-connect";
const GIT_URL: &str = "https://github.com/Rutger505/workstation-connect";
const FIRST_CHECK: Duration = Duration::from_secs(30);
const EVERY: Duration = Duration::from_secs(15 * 60);
const RETRY: Duration = Duration::from_secs(5 * 60);
/// Wake-up granularity: monotonic sleeps stop during suspend, wall clock doesn't.
const TICK: Duration = Duration::from_secs(60);

/// Settle time after a (re)connect before checking: DNS/route and NM's own connectivity check.
const AFTER_RECONNECT: Duration = Duration::from_secs(10);

/// Set in the daemon status only when outdated.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Update {
    pub installed: String,
    pub latest: String,
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Decides from a GitHub compare `{installed}...{latest}` response. `ahead` = main has commits
/// the install lacks; `diverged` = also local-only commits (dev build). README-only: not outdated.
pub fn outdated(compare: &Value) -> bool {
    let newer = matches!(compare["status"].as_str(), Some("ahead" | "diverged"));
    let laptop = compare["files"].as_array().is_some_and(|f| {
        f.iter()
            .filter_map(|f| f["filename"].as_str())
            .any(|n| n.starts_with("laptop/"))
    });
    newer && laptop
}

fn get(agent: &ureq::Agent, url: &str, accept: &str) -> Result<String> {
    agent
        .get(url)
        .header("Accept", accept)
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .with_context(|| format!("GET {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("read {url}"))
}

/// Ok(None): up to date. Err: unknown (network, rate limit, commit not on GitHub).
fn check_blocking(installed: &str) -> Result<Option<Update>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .user_agent(concat!("wsconnect/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let api = format!("https://api.github.com/repos/{REPO}");
    let latest = get(
        &agent,
        &format!("{api}/commits/main"),
        "application/vnd.github.sha",
    )?;
    let latest = latest.trim();
    if latest == installed {
        return Ok(None);
    }
    let compare: Value = serde_json::from_str(&get(
        &agent,
        &format!("{api}/compare/{installed}...{latest}"),
        "application/vnd.github+json",
    )?)?;
    Ok(outdated(&compare).then(|| Update {
        installed: short(installed),
        latest: short(latest),
    }))
}

/// Daemon task: reports Some(update) / None on every successful check. Errors keep the old
/// result. Never runs for builds without a known commit. `reconnected` = check now (after a
/// short settle), on top of the fixed interval.
pub async fn watch(report: impl Fn(Option<Update>), reconnected: &Notify) {
    if COMMIT == "unknown" {
        info!("update check off: build commit unknown");
        return;
    }
    tokio::time::sleep(FIRST_CHECK).await;
    loop {
        let wait = match tokio::task::spawn_blocking(|| check_blocking(COMMIT)).await {
            Ok(Ok(u)) => {
                match &u {
                    Some(u) => info!("update available: {} → {}", u.installed, u.latest),
                    None => debug!("wsconnect up to date ({COMMIT_SHORT})"),
                }
                report(u);
                EVERY
            }
            Ok(Err(e)) => {
                warn!("update check: {e:#}");
                RETRY
            }
            Err(e) => {
                warn!("update check: {e}");
                RETRY
            }
        };
        let next = SystemTime::now() + wait;
        while SystemTime::now() < next {
            tokio::select! {
                _ = tokio::time::sleep(TICK.min(wait)) => {}
                _ = reconnected.notified() => {
                    // Connect events come in bursts (primary + connectivity): settle, then drop
                    // the stored permit so the burst gives one check.
                    tokio::time::sleep(AFTER_RECONNECT).await;
                    let _ = tokio::time::timeout(Duration::ZERO, reconnected.notified()).await;
                    debug!("network (re)connected: checking for updates");
                    break;
                }
            }
        }
    }
}

/// `wsconnect update`: reinstall from main, restart the daemon if it runs.
pub async fn run(pause: bool) -> Result<()> {
    let r = install().await;
    if let Err(e) = &r {
        eprintln!("update failed: {e:#}");
    }
    if pause {
        // Started from the menu in a fresh terminal: keep it open to read the result.
        println!("\nPress Enter to close");
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
    }
    r
}

/// Persistent clone + build dir under ~/.cache/wsconnect. `cargo install --git` with a kept
/// target dir reused the old wsconnect build after a new commit (git sources aren't fingerprinted
/// by content), so update builds a local clone with `--path`: mtime fingerprints catch every change.
/// Safe to delete (next update is a full build again).
fn cache_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        })
        .join("wsconnect")
}

async fn run_ok(cmd: &mut Command, what: &str) -> Result<()> {
    let status = cmd
        .status()
        .await
        .with_context(|| format!("start {what}"))?;
    if !status.success() {
        bail!("{what}: {status}");
    }
    Ok(())
}

async fn git_out(src: &std::path::Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(src)
        .args(args)
        .output()
        .await?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn install() -> Result<()> {
    let cache = cache_dir();
    let src = cache.join("src");
    if !src.join(".git").exists() {
        if src.exists() {
            std::fs::remove_dir_all(&src).with_context(|| format!("remove {}", src.display()))?;
        }
        std::fs::create_dir_all(&cache)?;
        run_ok(
            Command::new("git")
                .args(["clone", "--quiet", GIT_URL])
                .arg(&src),
            "git clone",
        )
        .await?;
    } else {
        run_ok(
            Command::new("git")
                .arg("-C")
                .arg(&src)
                .args(["fetch", "--quiet", "origin", "main"]),
            "git fetch",
        )
        .await?;
        run_ok(
            Command::new("git").arg("-C").arg(&src).args([
                "checkout",
                "--quiet",
                "--force",
                "-B",
                "main",
                "FETCH_HEAD",
            ]),
            "git checkout",
        )
        .await?;
    }
    let head = git_out(&src, &["rev-parse", "HEAD"]).await?;
    let head = head.get(..7).unwrap_or(&head).to_string();
    run_ok(
        Command::new("cargo")
            .args(["install", "--force", "--locked", "--path"])
            .arg(src.join("laptop"))
            .arg("--target-dir")
            .arg(cache.join("target")),
        "cargo install",
    )
    .await?;
    // Guard against a stale build ever again: the installed binary must report the fetched commit.
    let out = Command::new("wsconnect")
        .arg("--version")
        .output()
        .await
        .context("run wsconnect")?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !version.contains(&format!("({head})")) {
        bail!(
            "installed `{version}`, expected commit {head} (PATH has another wsconnect, or stale build: delete {})",
            cache.display()
        );
    }
    println!("installed {version}");
    let active = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "wsconnect"])
        .status()
        .await
        .is_ok_and(|s| s.success());
    if active {
        let status = Command::new("systemctl")
            .args(["--user", "restart", "wsconnect"])
            .status()
            .await
            .context("start systemctl")?;
        if !status.success() {
            bail!("systemctl --user restart wsconnect: {status}");
        }
        println!("daemon restarted");
    }
    println!("wsconnect updated");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cmp(status: &str, files: &[&str]) -> Value {
        let files: Vec<_> = files.iter().map(|f| json!({"filename": f})).collect();
        json!({"status": status, "files": files})
    }

    #[test]
    fn outdated_only_for_newer_laptop_changes() {
        assert!(outdated(&cmp("ahead", &["README.md", "laptop/src/ctl.rs"])));
        assert!(outdated(&cmp("diverged", &["laptop/Cargo.lock"])));
        assert!(!outdated(&cmp("ahead", &["README.md", "android/x.kt"])));
        assert!(!outdated(&cmp("behind", &["laptop/src/ctl.rs"])));
        assert!(!outdated(&cmp("identical", &[])));
        assert!(!outdated(&json!({"message": "Not Found"})));
    }

    #[test]
    fn version_has_commit() {
        assert!(VERSION.ends_with(&format!("({COMMIT_SHORT})")));
        assert!(COMMIT == "unknown" || COMMIT.len() == 40);
    }
}
