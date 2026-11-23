//! Log upload to a private S3 bucket (SeaweedFS on cdn.rutgerpronk.com, path-style, SigV4):
//! the daemon's journal since the last successful upload (+ wsconnect-update, kdeconnectd and
//! best effort the system's Bluetooth lines), so debugging needs no log collecting by hand.
//! Key: write-only, from ~/.config/workstation-connect/logs.env (never in the repo); missing
//! file = upload off.

use crate::sigv4::{self, Credentials};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

const ENDPOINT_HOST: &str = "cdn.rutgerpronk.com";
const BUCKET: &str = "wsconnect-logs";
const REGION: &str = "us-east-1";
/// Body cap: the newest lines win.
const MAX_BODY: usize = 1_000_000;
/// First upload without a cursor: this much journal.
const FIRST_SINCE: &str = "-1h";
/// Triggers within this window become one upload.
const DEBOUNCE: Duration = Duration::from_secs(5);
const PUT_TIMEOUT: Duration = Duration::from_secs(30);
/// Whole upload incl. journalctl.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const ACCESS_VAR: &str = "WSCONNECT_LOGS_ACCESS_KEY";
const SECRET_VAR: &str = "WSCONNECT_LOGS_SECRET_KEY";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Periodic,
    LinkOk,
    LinkFail,
    HotspotOk,
    HotspotFail,
    Update,
    Start,
    Selftest,
}

impl Reason {
    pub const ALL: [Reason; 8] = [
        Reason::Periodic,
        Reason::LinkOk,
        Reason::LinkFail,
        Reason::HotspotOk,
        Reason::HotspotFail,
        Reason::Update,
        Reason::Start,
        Reason::Selftest,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Periodic => "periodic",
            Reason::LinkOk => "link-ok",
            Reason::LinkFail => "link-fail",
            Reason::HotspotOk => "hotspot-ok",
            Reason::HotspotFail => "hotspot-fail",
            Reason::Update => "update",
            Reason::Start => "start",
            Reason::Selftest => "selftest",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == s)
    }
}

pub fn env_path() -> PathBuf {
    crate::config::dir().join("logs.env")
}

fn cursor_path() -> PathBuf {
    crate::config::state_dir().join("upload.cursor")
}

/// `KEY=value` lines (optional quotes, `export`, comments).
pub fn parse_env(text: &str) -> Option<Credentials> {
    let (mut access, mut secret) = (None, None);
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        match k.trim() {
            ACCESS_VAR => access = Some(v),
            SECRET_VAR => secret = Some(v),
            _ => {}
        }
    }
    Some(Credentials {
        access: access.filter(|s| !s.is_empty())?,
        secret: secret.filter(|s| !s.is_empty())?,
    })
}

/// Ok(None): no logs.env, upload off.
pub fn load_credentials() -> Result<Option<Credentials>> {
    let path = env_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    parse_env(&text)
        .map(Some)
        .with_context(|| format!("{}: needs {ACCESS_VAR}= and {SECRET_VAR}=", path.display()))
}

/// `wsconnect logs-key`: writes logs.env (mode 600). The daemon reads it per upload.
pub fn write_key(access: &str, secret: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if [access, secret]
        .iter()
        .any(|s| s.is_empty() || s.contains(char::is_whitespace))
    {
        bail!("access and secret key must be non-empty, without spaces");
    }
    let path = env_path();
    std::fs::create_dir_all(crate::config::dir())?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    // mode() only applies to new files.
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    writeln!(f, "{ACCESS_VAR}={access}\n{SECRET_VAR}={secret}")?;
    println!(
        "wrote {} (mode 600); the daemon uses it from the next upload",
        path.display()
    );
    Ok(())
}

/// (year, month, day, hour, minute, second) in UTC.
pub fn utc(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem / 60 % 60) as u32,
        (rem % 60) as u32,
    )
}

/// YYYYMMDDTHHMMSSZ
pub fn amz_date(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = utc(secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

fn iso(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = utc(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC")
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

/// laptop/<hostname>/<YYYY-MM-DD>/<HHMMSS>-<reason>.log (UTC)
pub fn object_key(host: &str, reason: Reason, secs: u64) -> String {
    let host: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let (y, mo, d, h, mi, s) = utc(secs);
    format!(
        "laptop/{host}/{y:04}-{mo:02}-{d:02}/{h:02}{mi:02}{s:02}-{}.log",
        reason.as_str()
    )
}

/// Splits journalctl --show-cursor output into (entries, cursor of the last entry).
pub fn split_cursor(out: &str) -> (String, Option<String>) {
    let mut lines: Vec<&str> = out.lines().collect();
    let cursor = match lines.last().and_then(|l| l.strip_prefix("-- cursor: ")) {
        Some(c) => {
            let c = c.trim().to_string();
            lines.pop();
            Some(c)
        }
        None => None,
    };
    let mut body = lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    (body, cursor)
}

/// Keeps the newest `max` bytes, cut at a line start.
pub fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let start = text[start..].find('\n').map_or(start, |i| start + i + 1);
    format!("[… {} older bytes cut]\n{}", start, &text[start..])
}

async fn run(cmd: &mut Command) -> Result<(bool, String, String)> {
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.kill_on_drop(true).output())
        .await
        .context("timeout")??;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// User journal of wsconnect, the update unit and kdeconnectd after `cursor`.
async fn user_journal(cursor: Option<&str>) -> Result<(String, Option<String>)> {
    let mut cmd = Command::new("journalctl");
    cmd.args([
        "--user",
        "-u",
        "wsconnect",
        "-u",
        crate::update::UNIT,
        "-u",
        "app-org.kde.kdeconnect.daemon@*",
        "-o",
        "short-precise",
        "--no-pager",
        "-q",
        "--show-cursor",
    ]);
    match cursor {
        Some(c) => cmd.arg(format!("--after-cursor={c}")),
        None => cmd.arg(format!("--since={FIRST_SINCE}")),
    };
    let (ok, out, err) = run(&mut cmd).await.context("journalctl --user")?;
    if !ok {
        bail!("journalctl --user: {}", err.trim());
    }
    Ok(split_cursor(&out))
}

/// Best effort: system journal (bluetoothd + Bluetooth kernel lines) since `since`; empty when
/// not allowed (user not in systemd-journal/wheel).
async fn system_journal(since: &str) -> String {
    let mut out = String::new();
    let queries: [(&str, Vec<&str>); 2] = [
        ("bluetooth.service", vec!["-b", "-u", "bluetooth"]),
        (
            "kernel (bluetooth)",
            vec![
                "-k",
                "--grep",
                "bluetooth|hci|btusb|mt79|rfcomm|l2cap",
                "--case-sensitive=false",
            ],
        ),
    ];
    for (name, args) in queries {
        let mut cmd = Command::new("journalctl");
        cmd.args(&args)
            .args(["-o", "short-precise", "--no-pager", "-q"])
            .arg(format!("--since={since}"));
        if let Ok((_, lines, _)) = run(&mut cmd).await
            && !lines.trim().is_empty()
        {
            out.push_str(&format!("\n===== system journal: {name} =====\n{lines}"));
        }
    }
    out
}

struct Payload {
    body: String,
    cursor: Option<String>,
    new_lines: bool,
}

async fn build(reasons: &[Reason], extra: Option<&str>, now: u64) -> Result<Payload> {
    let path = cursor_path();
    let saved = std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // Cursor file mtime = last successful upload: the system journal starts there.
    let since = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .filter(|_| saved.is_some())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(FIRST_SINCE.to_string(), |d| format!("@{}", d.as_secs()));
    let (user, cursor) = match user_journal(saved.as_deref()).await {
        Ok(j) => j,
        // A cursor journald no longer knows (rotated/vacuumed): start fresh.
        Err(e) if saved.is_some() => {
            warn!("log upload: {e:#}: reading the last hour instead");
            user_journal(None).await?
        }
        Err(e) => return Err(e),
    };
    let new_lines = !user.trim().is_empty();
    let system = system_journal(&since).await;
    let names: Vec<_> = reasons.iter().map(|r| r.as_str()).collect();
    let mut head = format!(
        "host: {}\nwsconnect: {}\nkernel: {}\nreason: {}\ntime: {}\njournal since: {}\n",
        hostname(),
        crate::update::VERSION,
        kernel(),
        names.join(", "),
        iso(now),
        if saved.is_some() {
            "last upload"
        } else {
            FIRST_SINCE
        },
    );
    if let Some(x) = extra {
        head.push('\n');
        head.push_str(x);
        if !x.ends_with('\n') {
            head.push('\n');
        }
    }
    let logs = format!("\n===== user journal =====\n{user}{system}");
    let room = MAX_BODY.saturating_sub(head.len()).max(64 * 1024);
    Ok(Payload {
        body: head + &tail(&logs, room),
        cursor,
        new_lines,
    })
}

fn put_blocking(creds: &Credentials, key: &str, body: String) -> Result<()> {
    let uri = sigv4::encode_path(&format!("/{BUCKET}/{key}"));
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let date = amz_date(secs);
    let hash = sigv4::sha256_hex(body.as_bytes());
    let ctype = "text/plain; charset=utf-8";
    let auth = sigv4::authorization(
        creds,
        REGION,
        "s3",
        &date,
        &sigv4::Request {
            method: "PUT",
            uri: &uri,
            query: "",
            headers: &[
                ("content-type", ctype),
                ("host", ENDPOINT_HOST),
                ("x-amz-content-sha256", &hash),
                ("x-amz-date", &date),
            ],
            payload_sha256: &hash,
        },
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(PUT_TIMEOUT))
        .http_status_as_error(false)
        .user_agent(concat!("wsconnect/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut res = agent
        .put(&format!("https://{ENDPOINT_HOST}{uri}"))
        .header("Content-Type", ctype)
        .header("x-amz-content-sha256", &hash)
        .header("x-amz-date", &date)
        .header("Authorization", &auth)
        .send(body)
        .context("PUT")?;
    let status = res.status();
    if !status.is_success() {
        let text = res.body_mut().read_to_string().unwrap_or_default();
        bail!(
            "PUT {key}: HTTP {status}: {}",
            text.chars().take(300).collect::<String>()
        );
    }
    Ok(())
}

/// One upload. Ok(None): skipped (no logs.env, or periodic without new lines). The cursor only
/// moves after a successful PUT.
pub async fn upload(reasons: &[Reason], extra: Option<&str>) -> Result<Option<String>> {
    let Some(creds) = load_credentials()? else {
        return Ok(None);
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let p = build(reasons, extra, now).await?;
    if reasons == [Reason::Periodic] && !p.new_lines {
        debug!("log upload: nothing new");
        return Ok(None);
    }
    let reason = reasons.first().copied().unwrap_or(Reason::Periodic);
    let key = object_key(&hostname(), reason, now);
    let size = p.body.len();
    let k = key.clone();
    tokio::task::spawn_blocking(move || put_blocking(&creds, &k, p.body))
        .await
        .context("upload task")??;
    if let Some(c) = p.cursor {
        let res = std::fs::create_dir_all(crate::config::state_dir())
            .and_then(|()| std::fs::write(cursor_path(), c));
        if let Err(e) = res {
            warn!("log upload: write {}: {e}", cursor_path().display());
        }
    }
    // debug, not info: an info line per upload would be "new lines" for the next periodic one.
    debug!("log upload: {key} ({size} bytes)");
    Ok(Some(key))
}

enum Job {
    Trigger(Reason),
    /// Retry pending reasons only (network back).
    Retry,
    Now {
        reason: Reason,
        extra: Option<String>,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
}

/// Daemon side: debounced, one upload at a time, never blocks callers.
#[derive(Clone)]
pub struct Uploader {
    tx: mpsc::Sender<Job>,
}

impl Uploader {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(serve(rx));
        Self { tx }
    }

    /// Upload soon (debounced), also when nothing new happened since the last one (except periodic).
    pub fn trigger(&self, reason: Reason) {
        let _ = self.tx.try_send(Job::Trigger(reason));
    }

    /// Network back: retry uploads that failed (if any).
    pub fn retry(&self) {
        let _ = self.tx.try_send(Job::Retry);
    }

    /// Upload right away with `extra` after the header; returns the object key.
    pub async fn now(&self, reason: Reason, extra: Option<String>) -> Result<Option<String>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job::Now {
                reason,
                extra,
                reply,
            })
            .await
            .map_err(|_| anyhow::anyhow!("uploader gone"))?;
        rx.await?
    }
}

/// Merges a reason into the pending list: periodic only when alone, no duplicates.
pub fn merge(pending: &mut Vec<Reason>, r: Reason) {
    if r == Reason::Periodic && !pending.is_empty() {
        return;
    }
    pending.retain(|p| *p != Reason::Periodic);
    if !pending.contains(&r) {
        pending.push(r);
    }
}

async fn attempt(
    reasons: &[Reason],
    extra: Option<&str>,
    off_logged: &mut bool,
) -> Result<Option<String>> {
    match tokio::time::timeout(UPLOAD_TIMEOUT, upload(reasons, extra)).await {
        Err(_) => bail!("log upload: no result after {}s", UPLOAD_TIMEOUT.as_secs()),
        Ok(Ok(None)) if load_credentials().ok().flatten().is_none() => {
            if !*off_logged {
                info!("log upload off: no {}", env_path().display());
                *off_logged = true;
            }
            Ok(None)
        }
        Ok(r) => {
            *off_logged = false;
            r
        }
    }
}

async fn serve(mut rx: mpsc::Receiver<Job>) {
    // Reasons whose upload hasn't succeeded yet.
    let mut pending: Vec<Reason> = Vec::new();
    let mut off_logged = false;
    while let Some(job) = rx.recv().await {
        match job {
            Job::Now {
                reason,
                extra,
                reply,
            } => {
                let r = attempt(&[reason], extra.as_deref(), &mut off_logged).await;
                if let Err(e) = &r {
                    warn!("log upload ({}): {e:#}", reason.as_str());
                }
                let _ = reply.send(r);
                continue;
            }
            Job::Retry if pending.is_empty() => continue,
            Job::Retry => {}
            Job::Trigger(r) => merge(&mut pending, r),
        }
        // Collect the burst (connect outcome + mode change + …) into one upload.
        let deadline = tokio::time::Instant::now() + DEBOUNCE;
        let mut now_jobs = Vec::new();
        while let Ok(Some(job)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            match job {
                Job::Trigger(r) => merge(&mut pending, r),
                Job::Retry => {}
                j @ Job::Now { .. } => now_jobs.push(j),
            }
        }
        match attempt(&pending, None, &mut off_logged).await {
            Ok(_) => pending.clear(),
            Err(e) => {
                let names: Vec<_> = pending.iter().map(|r| r.as_str()).collect();
                warn!(
                    "log upload ({}): {e:#}; retrying at the next trigger",
                    names.join(", ")
                );
                pending.retain(|r| *r != Reason::Periodic);
            }
        }
        for job in now_jobs {
            if let Job::Now {
                reason,
                extra,
                reply,
            } = job
            {
                let _ = reply.send(attempt(&[reason], extra.as_deref(), &mut off_logged).await);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(utc(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(utc(1_795_466_724), (2026, 11, 23, 20, 45, 24));
        assert_eq!(amz_date(1_369_353_600), "20130524T000000Z");
    }

    #[test]
    fn object_keys() {
        assert_eq!(
            object_key("my laptop", Reason::LinkFail, 1_795_466_724),
            "laptop/my_laptop/2026-11-23/204524-link-fail.log"
        );
        for r in Reason::ALL {
            assert_eq!(Reason::parse(r.as_str()), Some(r));
        }
    }

    #[test]
    fn parses_env() {
        let c = parse_env(
            "# key\nexport WSCONNECT_LOGS_ACCESS_KEY=\"ak\"\nWSCONNECT_LOGS_SECRET_KEY=s/k+\n",
        )
        .unwrap();
        assert_eq!((c.access.as_str(), c.secret.as_str()), ("ak", "s/k+"));
        assert_eq!(parse_env("WSCONNECT_LOGS_ACCESS_KEY=ak\n"), None);
        assert_eq!(
            parse_env("WSCONNECT_LOGS_ACCESS_KEY=\nWSCONNECT_LOGS_SECRET_KEY=x"),
            None
        );
    }

    #[test]
    fn splits_cursor() {
        let (body, c) = split_cursor("a\nb\n-- cursor: s=1;i=2\n");
        assert_eq!(body, "a\nb\n");
        assert_eq!(c.as_deref(), Some("s=1;i=2"));
        assert_eq!(split_cursor(""), (String::new(), None));
    }

    #[test]
    fn tails_at_line_start() {
        assert_eq!(tail("abc\n", 10), "abc\n");
        let t = tail("line one\nline two\nline three\n", 15);
        assert!(t.ends_with("\nline three\n"), "{t}");
        assert!(!t.contains("two"), "{t}");
        assert!(t.starts_with("[… "), "{t}");
    }

    #[test]
    fn merges_reasons() {
        let mut p = vec![];
        merge(&mut p, Reason::Periodic);
        assert_eq!(p, [Reason::Periodic]);
        merge(&mut p, Reason::LinkOk);
        merge(&mut p, Reason::LinkOk);
        merge(&mut p, Reason::Periodic);
        assert_eq!(p, [Reason::LinkOk]);
        merge(&mut p, Reason::HotspotFail);
        assert_eq!(p, [Reason::LinkOk, Reason::HotspotFail]);
    }
}
