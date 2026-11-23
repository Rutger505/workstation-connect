//! Outdated-install check + auto-update (daemon) and `wsconnect update` (CLI).
//! Installed = commit embedded by build.rs; latest = GitHub main. Only laptop/ changes count.
//! Check: `git ls-remote` for main's sha (no API quota), GitHub compare API only for a sha not
//! seen before (60 requests/h unauthenticated, shared per IP).
//! Auto-update: `wsconnect update` in its own transient unit (`systemd-run --user`), outside the
//! daemon's cgroup: the update restarts the daemon unit, which would kill a child of it.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
const EVERY: Duration = Duration::from_secs(5 * 60);
const RETRY: Duration = Duration::from_secs(5 * 60);
/// A failed auto-update of a commit isn't retried for that commit within this time.
pub const FAILED_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// Transient unit running the auto-update.
pub const UNIT: &str = "wsconnect-update";
/// Poll interval while the update unit runs; give up (stop it) after MAX_UPDATE.
const UPDATE_POLL: Duration = Duration::from_secs(5);
const MAX_UPDATE: Duration = Duration::from_secs(60 * 60);
/// Wake-up granularity: monotonic sleeps stop during suspend, wall clock doesn't.
const TICK: Duration = Duration::from_secs(60);

/// Settle time after a (re)connect before checking: DNS/route and NM's own connectivity check.
const AFTER_RECONNECT: Duration = Duration::from_secs(10);

/// Set in the daemon status only when outdated.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Update {
    pub installed: String,
    pub latest: String,
    pub state: UpdateState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    /// Waiting for a manual `wsconnect update` (auto_update = false).
    Available,
    /// Auto-update running (unit wsconnect-update).
    Updating,
    /// Auto-update of `latest` failed; retried after FAILED_BACKOFF (or a newer commit).
    Failed,
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

/// main's sha from `git ls-remote <url> refs/heads/main` output.
pub fn parse_ls_remote(out: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let (sha, name) = l.split_once('\t')?;
        (name.trim() == "refs/heads/main"
            && sha.len() == 40
            && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| sha.to_ascii_lowercase())
    })
}

/// main's sha via the git protocol: no GitHub API quota.
async fn latest_main() -> Result<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new("git")
            .args(["ls-remote", GIT_URL, "refs/heads/main"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("git ls-remote: no answer in 30 s")?
    .context("run git")?;
    if !out.status.success() {
        bail!(
            "git ls-remote: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_ls_remote(&String::from_utf8_lossy(&out.stdout)).context("git ls-remote: no main")
}

/// GitHub compare API: does `latest` have laptop/ changes the install lacks? Err: unknown
/// (network, rate limit, commit not on GitHub).
fn compare_blocking(installed: &str, latest: &str) -> Result<bool> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .user_agent(concat!("wsconnect/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let compare: Value = serde_json::from_str(&get(
        &agent,
        &format!("https://api.github.com/repos/{REPO}/compare/{installed}...{latest}"),
        "application/vnd.github+json",
    )?)?;
    Ok(outdated(&compare))
}

/// A failed auto-update, kept in ~/.local/state/wsconnect/update.failed ("<sha> <unix secs>")
/// so a daemon restart doesn't retry it right away.
#[derive(Debug, Clone, PartialEq)]
pub struct Failed {
    pub target: String,
    pub at: SystemTime,
}

impl Failed {
    pub fn parse(text: &str) -> Option<Self> {
        let mut it = text.split_whitespace();
        let target = it.next()?.to_string();
        let secs: u64 = it.next()?.parse().ok()?;
        Some(Self {
            target,
            at: UNIX_EPOCH + Duration::from_secs(secs),
        })
    }

    fn format(&self) -> String {
        let secs = self.at.duration_since(UNIX_EPOCH).unwrap_or_default();
        format!("{} {}\n", self.target, secs.as_secs())
    }
}

fn failed_path() -> PathBuf {
    crate::config::state_dir().join("update.failed")
}

fn load_failed() -> Option<Failed> {
    Failed::parse(&std::fs::read_to_string(failed_path()).ok()?)
}

fn save_failed(f: &Failed) {
    let path = failed_path();
    let res = std::fs::create_dir_all(crate::config::state_dir())
        .and_then(|()| std::fs::write(&path, f.format()));
    if let Err(e) = res {
        warn!("write {}: {e}", path.display());
    }
}

/// Auto-install `target` now? Not when its last try failed less than FAILED_BACKOFF ago.
pub fn may_install(target: &str, failed: Option<&Failed>, now: SystemTime) -> bool {
    !failed.is_some_and(|f| {
        f.target == target
            && now
                .duration_since(f.at)
                .is_ok_and(|age| age < FAILED_BACKOFF)
    })
}

/// The running binary; after `cargo install` replaced it, /proc reports "<path> (deleted)".
fn current_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| "wsconnect".into());
    match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(s) => s.into(),
        None => exe,
    }
}

async fn unit_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", UNIT])
        .status()
        .await
        .is_ok_and(|s| s.success())
}

/// Starts `wsconnect update` as transient user unit; returns once queued.
async fn start_unit() -> Result<()> {
    let mut cmd = Command::new("systemd-run");
    cmd.args(["--user", "--unit", UNIT, "--collect", "--quiet"])
        .arg("--description=wsconnect auto-update");
    // The user manager's environment may lack the toolchain the daemon was started with.
    for var in ["PATH", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"] {
        if std::env::var_os(var).is_some() {
            cmd.arg(format!("--setenv={var}"));
        }
    }
    let out = cmd
        .arg("--")
        .arg(current_exe())
        .arg("update")
        .output()
        .await
        .context("start systemd-run")?;
    if !out.status.success() {
        bail!(
            "systemd-run: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Waits for the update unit to end. A successful update restarts the daemon, so returning at
/// all means it failed. Gives up (stops the unit) after MAX_UPDATE.
async fn wait_unit() -> Result<()> {
    let start = std::time::Instant::now();
    while unit_active().await {
        if start.elapsed() > MAX_UPDATE {
            let _ = Command::new("systemctl")
                .args(["--user", "stop", UNIT])
                .status()
                .await;
            bail!(
                "still running after {} min: stopped",
                MAX_UPDATE.as_secs() / 60
            );
        }
        tokio::time::sleep(UPDATE_POLL).await;
    }
    bail!("ended without restarting the daemon (journalctl --user -u {UNIT})")
}

/// Daemon task: reports the outdated state on every successful check (None: up to date).
/// Errors keep the old result. Never runs for builds without a known commit. `reconnected` =
/// check now (after a short settle), on top of the fixed interval. `auto`: install right away
/// in unit wsconnect-update; `on_failed` runs after a failed auto-update.
pub async fn watch(
    auto: bool,
    report: impl Fn(Option<Update>),
    on_failed: impl Fn(),
    reconnected: &Notify,
) {
    if COMMIT == "unknown" {
        info!("update check off: build commit unknown");
        return;
    }
    if unit_active().await {
        info!("auto-update already running ({UNIT}): waiting for it");
        report(Some(Update {
            installed: COMMIT_SHORT.into(),
            latest: "?".into(),
            state: UpdateState::Updating,
        }));
        if let Err(e) = wait_unit().await {
            warn!("auto-update: {e:#}");
            on_failed();
        }
        report(None);
    } else {
        tokio::time::sleep(FIRST_CHECK).await;
    }
    // Last sha the compare API was asked about, and its answer.
    let mut checked: Option<(String, bool)> = None;
    let mut failed = load_failed();
    loop {
        let res = match latest_main().await {
            Ok(latest) if latest == COMMIT => Ok((latest, false)),
            Ok(latest) => match &checked {
                Some((sha, out)) if *sha == latest => Ok((latest, *out)),
                _ => {
                    let l = latest.clone();
                    match tokio::task::spawn_blocking(move || compare_blocking(COMMIT, &l)).await {
                        Ok(Ok(out)) => {
                            checked = Some((latest.clone(), out));
                            Ok((latest, out))
                        }
                        Ok(Err(e)) => Err(e),
                        Err(e) => Err(e.into()),
                    }
                }
            },
            Err(e) => Err(e),
        };
        let wait = match res {
            Ok((_, false)) => {
                debug!("wsconnect up to date ({COMMIT_SHORT})");
                report(None);
                EVERY
            }
            Ok((latest, true)) => {
                let mut u = Update {
                    installed: COMMIT_SHORT.into(),
                    latest: short(&latest),
                    state: UpdateState::Available,
                };
                if !auto {
                    info!("update available: {} → {}", u.installed, u.latest);
                } else if !may_install(&latest, failed.as_ref(), SystemTime::now()) {
                    info!(
                        "update {} → {}: failed before, retrying after {} min or a newer commit",
                        u.installed,
                        u.latest,
                        FAILED_BACKOFF.as_secs() / 60
                    );
                    u.state = UpdateState::Failed;
                } else {
                    info!("auto-update: {} → {}, installing", u.installed, u.latest);
                    u.state = UpdateState::Updating;
                    report(Some(u.clone()));
                    let res = match start_unit().await {
                        Ok(()) => wait_unit().await,
                        Err(e) => Err(e),
                    };
                    if let Err(e) = res {
                        warn!("auto-update to {} failed: {e:#}", u.latest);
                        let f = Failed {
                            target: latest.clone(),
                            at: SystemTime::now(),
                        };
                        save_failed(&f);
                        failed = Some(f);
                        u.state = UpdateState::Failed;
                        on_failed();
                    }
                }
                report(Some(u));
                EVERY
            }
            Err(e) => {
                warn!("update check: {e:#}");
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

/// Held for the whole install: an auto-update and one from the menu must not build at once.
fn lock(cache: &std::path::Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(cache)?;
    let path = cache.join("update.lock");
    let f = std::fs::File::create(&path).with_context(|| format!("open {}", path.display()))?;
    // SAFETY: plain flock on an fd we own; released when the file closes.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another wsconnect update is running ({})", path.display());
    }
    Ok(f)
}

async fn install() -> Result<()> {
    let cache = cache_dir();
    let _lock = lock(&cache)?;
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
    fn parses_ls_remote() {
        let sha = "0123456789abcdef0123456789ABCDEF01234567";
        assert_eq!(
            parse_ls_remote(&format!("{sha}\trefs/heads/main\n")),
            Some(sha.to_ascii_lowercase())
        );
        assert_eq!(parse_ls_remote(&format!("{sha}\trefs/heads/mainx\n")), None);
        assert_eq!(parse_ls_remote("abc\trefs/heads/main"), None);
        assert_eq!(parse_ls_remote(""), None);
    }

    #[test]
    fn failed_commit_waits_backoff() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let f = Failed {
            target: "aaa".into(),
            at: now - Duration::from_secs(10 * 60),
        };
        assert!(!may_install("aaa", Some(&f), now));
        assert!(
            may_install("bbb", Some(&f), now),
            "newer commit: try at once"
        );
        assert!(may_install("aaa", Some(&f), now + FAILED_BACKOFF));
        assert!(may_install("aaa", None, now));
        assert_eq!(Failed::parse(&f.format()), Some(f));
        assert_eq!(Failed::parse("garbage"), None);
    }

    #[test]
    fn version_has_commit() {
        assert!(VERSION.ends_with(&format!("({COMMIT_SHORT})")));
        assert!(COMMIT == "unknown" || COMMIT.len() == 40);
    }
}
