//! `workstation-connect selftest`: step-by-step check of daemon, phone link, hotspot and KDE Connect with
//! timings. Phone link + hotspot run inside the daemon (ctl socket) so they don't fight its state
//! machine. Afterwards: report + logs to the bucket, report handed to Rocky via someclaw-thread.

use crate::config::Config;
use crate::kde::{KEY_PLUGINS, Kde};
use crate::logs::{self, Reason};
use crate::update::VERSION;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Res {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub name: String,
    pub res: Res,
    pub ms: Option<u64>,
    pub detail: String,
}

fn ms(since: Instant) -> Option<u64> {
    Some(since.elapsed().as_millis() as u64)
}

impl Step {
    pub fn new(name: &str, res: Res, ms: Option<u64>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            res,
            ms,
            detail: detail.into(),
        }
    }

    pub fn pass(name: &str, since: Option<Instant>, detail: impl Into<String>) -> Self {
        Self::new(name, Res::Pass, since.and_then(ms), detail)
    }

    pub fn fail(name: &str, since: Option<Instant>, detail: impl Into<String>) -> Self {
        Self::new(name, Res::Fail, since.and_then(ms), detail)
    }

    pub fn skip(name: &str, detail: impl Into<String>) -> Self {
        Self::new(name, Res::Skip, None, detail)
    }

    /// Pass with the Ok text, or fail with the full error chain, timed from `since`.
    pub fn timed(name: &str, since: Instant, r: Result<String>) -> Self {
        match r {
            Ok(d) => Self::pass(name, Some(since), d),
            Err(e) => Self::fail(name, Some(since), format!("{e:#}")),
        }
    }
}

pub fn render(title: &str, sections: &[(String, Vec<Step>)]) -> String {
    let mut out = format!("{title}\n");
    let (mut pass, mut fail, mut skip) = (0, 0, 0);
    for (name, steps) in sections {
        out.push_str(&format!("\n== {name} ==\n"));
        for s in steps {
            match s.res {
                Res::Pass => pass += 1,
                Res::Fail => fail += 1,
                Res::Skip => skip += 1,
            }
            let res = serde_json::to_string(&s.res).unwrap_or_default();
            let t = s.ms.map_or("-".to_string(), |m| format!("{m} ms"));
            out.push_str(&format!(
                "{:<4} {:>9}  {}: {}\n",
                res.trim_matches('"'),
                t,
                s.name,
                s.detail
            ));
        }
    }
    out.push_str(&format!(
        "\nsummary: {pass} pass, {fail} fail, {skip} skip\n"
    ));
    out
}

pub fn failures(sections: &[(String, Vec<Step>)]) -> usize {
    sections
        .iter()
        .flat_map(|(_, s)| s)
        .filter(|s| s.res == Res::Fail)
        .count()
}

/// Daemon op over the ctl socket (JSON request); `{"error": …}` becomes Err.
async fn daemon(req: Value) -> Result<Value> {
    let v: Value = serde_json::from_str(&crate::ctl::request(&req.to_string()).await?)?;
    if let Some(e) = v["error"].as_str() {
        bail!("{e}");
    }
    Ok(v)
}

async fn daemon_steps(req: Value) -> Vec<Step> {
    let t = Instant::now();
    match daemon(req).await {
        Ok(v) => serde_json::from_value(v["steps"].clone())
            .unwrap_or_else(|e| vec![Step::fail("daemon answer", Some(t), e.to_string())]),
        Err(e) => vec![Step::fail("daemon request", Some(t), format!("{e:#}"))],
    }
}

async fn nm_has_connection(name: &str) -> Result<bool> {
    let out = Command::new("nmcli")
        .args(["-t", "-f", "NAME", "connection", "show"])
        .output()
        .await?;
    if !out.status.success() {
        bail!("nmcli: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    // -t escapes ':' and '\' in names.
    let esc = name.replace('\\', "\\\\").replace(':', "\\:");
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l == esc))
}

async fn section_basics(
    cfg: &Option<Config>,
    cfg_err: &str,
    status: &mut Option<Value>,
) -> Vec<Step> {
    let mut steps = vec![Step::pass("workstation-connect", None, VERSION)];
    let t = Instant::now();
    match crate::ctl::request("status")
        .await
        .and_then(|s| Ok(serde_json::from_str::<Value>(&s)?))
    {
        Ok(v) => {
            let dv = v["version"]
                .as_str()
                .unwrap_or("unknown (older than selftest)");
            steps.push(Step::pass(
                "daemon reachable",
                Some(t),
                format!("daemon {dv}, mode {}", v["mode"].as_str().unwrap_or("?")),
            ));
            if dv != VERSION {
                steps.push(Step::fail(
                    "daemon version",
                    None,
                    format!("daemon runs {dv}, installed is {VERSION}: systemctl --user restart workstation-connect"),
                ));
            }
            match v["update"]["state"].as_str() {
                None => {}
                Some(s) => steps.push(Step::fail(
                    "up to date",
                    None,
                    format!(
                        "{} → {} ({s})",
                        v["update"]["installed"].as_str().unwrap_or("?"),
                        v["update"]["latest"].as_str().unwrap_or("?")
                    ),
                )),
            }
            *status = Some(v);
        }
        Err(e) => steps.push(Step::fail("daemon reachable", Some(t), format!("{e:#}"))),
    }
    match status
        .as_ref()
        .and_then(|v| v["phone"]["state"]["version"].as_u64())
    {
        Some(pv) => steps.push(Step::pass("phone app protocol", None, format!("v{pv}"))),
        None => steps.push(Step::skip(
            "phone app protocol",
            "phone link not connected yet: see phone state below",
        )),
    }
    match cfg {
        Some(c) => {
            steps.push(Step::pass(
                "config",
                None,
                format!(
                    "phone {}, hotspot_connection {:?}, auto_update {}",
                    c.phone, c.hotspot_connection, c.auto_update
                ),
            ));
            let t = Instant::now();
            steps.push(match nm_has_connection(&c.hotspot_connection).await {
                Ok(true) => Step::pass("hotspot connection in NM", Some(t), &c.hotspot_connection),
                Ok(false) => Step::fail(
                    "hotspot connection in NM",
                    Some(t),
                    format!(
                        "no NetworkManager connection {:?} (README: Setup)",
                        c.hotspot_connection
                    ),
                ),
                Err(e) => Step::fail("hotspot connection in NM", Some(t), format!("{e:#}")),
            });
        }
        None => steps.push(Step::fail("config", None, cfg_err)),
    }
    let env = logs::env_path();
    steps.push(match logs::load_credentials() {
        Ok(Some(_)) => Step::pass("logs.env", None, env.display().to_string()),
        Ok(None) => Step::fail(
            "logs.env",
            None,
            format!(
                "{} missing: log upload off (workstation-connect logs-key <access> <secret>)",
                env.display()
            ),
        ),
        Err(e) => Step::fail("logs.env", None, format!("{e:#}")),
    });
    steps
}

async fn section_kde(want: Option<&str>) -> Vec<Step> {
    let mut steps = Vec::new();
    let t = Instant::now();
    let kde = match Kde::connect().await {
        Ok(k) => k,
        Err(e) => return vec![Step::fail("session bus", Some(t), format!("{e:#}"))],
    };
    match kde.paired().await {
        Ok(ids) => steps.push(Step::pass(
            "kdeconnectd on D-Bus",
            Some(t),
            format!("{} paired device(s)", ids.len()),
        )),
        Err(e) => {
            steps.push(Step::fail(
                "kdeconnectd on D-Bus",
                Some(t),
                format!("{e:#}"),
            ));
            return steps;
        }
    }
    let t = Instant::now();
    let id = match kde.find_phone(want).await {
        Ok(id) => id,
        Err(e) => {
            steps.push(Step::fail("phone device", Some(t), format!("{e:#}")));
            return steps;
        }
    };
    let info = match kde.device_info(&id).await {
        Ok(i) => i,
        Err(e) => {
            steps.push(Step::fail("phone device", Some(t), format!("{id}: {e:#}")));
            return steps;
        }
    };
    steps.push(Step::pass(
        "phone device",
        Some(t),
        format!("{} ({id})", info.name),
    ));
    steps.push(if info.paired {
        Step::pass("paired", None, "yes")
    } else {
        Step::fail("paired", None, "no: pair from the phone app")
    });
    let links = format!("links {:?} ({})", info.providers, info.connection);
    steps.push(if info.reachable {
        Step::pass("reachable", None, links)
    } else {
        Step::fail("reachable", None, format!("no: {links}"))
    });
    for e in &info.errors {
        steps.push(Step::fail("D-Bus", None, e.clone()));
    }
    for (plugin, what) in KEY_PLUGINS {
        let name = format!(
            "plugin {} ({what})",
            plugin.trim_start_matches("kdeconnect_")
        );
        steps.push(if !info.reachable {
            Step::skip(
                &name,
                "phone not reachable: plugins only load while connected",
            )
        } else if info.plugins.iter().any(|p| p == plugin) {
            Step::pass(&name, None, "loaded")
        } else {
            Step::fail(&name, None, "not loaded (disabled on laptop or phone?)")
        });
    }
    steps.push(Step::pass("loaded plugins", None, info.plugins.join(", ")));
    steps.push(portal_step());
    if info.reachable {
        let t = Instant::now();
        steps.push(Step::timed(
            "kde connect ping",
            t,
            kde.ping(
                &id,
                &format!("workstation-connect selftest from {}", logs::hostname()),
            )
            .await
            .map(|()| "sent: it should show on the phone".to_string()),
        ));
    } else {
        steps.push(Step::skip("kde connect ping", "phone not reachable"));
    }
    steps
}

/// Trackpad input needs the RemoteDesktop portal routed to luminous (README: Setup).
fn portal_step() -> Step {
    let cfg = crate::config::xdg_config();
    let portals = cfg.join("xdg-desktop-portal/hyprland-portals.conf");
    let whitelist = cfg.join("xdg-desktop-portal-luminous/whitelist.json");
    let routed = std::fs::read_to_string(&portals).is_ok_and(|t| {
        t.lines()
            .any(|l| l.replace(' ', "") == "org.freedesktop.impl.portal.RemoteDesktop=luminous")
    });
    let listed =
        std::fs::read_to_string(&whitelist).is_ok_and(|t| t.contains("org.kde.kdeconnect.daemon"));
    match (routed, listed) {
        (true, true) => Step::pass(
            "trackpad portal config",
            None,
            "RemoteDesktop → luminous, KDE Connect whitelisted",
        ),
        _ => Step::fail(
            "trackpad portal config",
            None,
            format!(
                "RemoteDesktop=luminous in {}: {routed}; kdeconnect in {}: {listed}",
                portals.display(),
                whitelist.display()
            ),
        ),
    }
}

/// Uploads report + logs: through the daemon (shares its journal cursor), else directly.
async fn upload(report: &str) -> Result<Option<String>> {
    match daemon(json!({"cmd": "upload", "reason": "selftest", "report": report})).await {
        Ok(v) => Ok(v["key"].as_str().map(String::from)),
        Err(e) if crate::ctl::request("status").await.is_err() => {
            eprintln!("daemon not reachable ({e:#}): uploading directly");
            logs::upload(&[Reason::Selftest], Some(report)).await
        }
        Err(e) => Err(e),
    }
}

/// Hands the report to Rocky: `someclaw-thread send new -`, report on stdin. Returns its output.
async fn send_to_rocky(text: &str) -> Result<String> {
    let mut child = Command::new("someclaw-thread")
        .args(["send", "new", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("someclaw-thread: {e} (dev-env on PATH?)"))?;
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(text.as_bytes()).await?;
    drop(stdin);
    let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("someclaw-thread: no answer after 60 s"))??;
    if !out.status.success() {
        bail!(
            "someclaw-thread: {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// First http(s) URL in someclaw-thread's output.
pub fn find_url(out: &str) -> Option<&str> {
    out.split_whitespace()
        .find(|w| w.starts_with("https://") || w.starts_with("http://"))
}

pub async fn run(no_hotspot: bool, no_send: bool) -> Result<()> {
    let host = logs::hostname();
    let (cfg, cfg_err) = match Config::load() {
        Ok(c) => (Some(c), String::new()),
        Err(e) => (None, format!("{e:#}")),
    };
    let mut status = None;
    let mut sections: Vec<(String, Vec<Step>)> = Vec::new();

    eprintln!("a. versions, daemon, config…");
    sections.push((
        "a. versions, daemon, config".into(),
        section_basics(&cfg, &cfg_err, &mut status).await,
    ));
    let up = status.is_some();

    eprintln!("b. phone link…");
    sections.push((
        "b. phone link (Bluetooth RFCOMM)".into(),
        if up {
            daemon_steps(json!({"cmd": "selftest_link"})).await
        } else {
            vec![Step::skip("phone link", "daemon not reachable")]
        },
    ));

    eprintln!("c. hotspot (≤ ~90 s)…");
    sections.push((
        "c. hotspot".into(),
        if no_hotspot {
            vec![Step::skip("hotspot", "--no-hotspot")]
        } else if up {
            daemon_steps(json!({"cmd": "selftest_hotspot"})).await
        } else {
            vec![Step::skip("hotspot", "daemon not reachable")]
        },
    ));

    eprintln!("d. KDE Connect…");
    let want = cfg.as_ref().and_then(|c| c.kdeconnect_device.clone());
    sections.push(("d. KDE Connect".into(), section_kde(want.as_deref()).await));

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let report = render(
        &format!(
            "workstation-connect selftest on {host}, {} ({})",
            VERSION,
            logs::amz_date(secs)
        ),
        &sections,
    );
    println!("{report}");

    let key = match upload(&report).await {
        Ok(Some(k)) => {
            println!("uploaded: s3://wsconnect-logs/{k}");
            Some(k)
        }
        Ok(None) => {
            println!(
                "not uploaded: log upload off (no {})",
                logs::env_path().display()
            );
            None
        }
        Err(e) => {
            println!("upload failed: {e:#}");
            None
        }
    };

    if no_send {
        println!("--no-send: not handed to Rocky");
    } else {
        let where_ = key.as_deref().map_or("logs not uploaded".to_string(), |k| {
            format!("logs: s3://wsconnect-logs/{k}")
        });
        let text = format!(
            "workstation-connect selftest from {host}: please analyse and fix failures ({where_})\n\n{report}"
        );
        match send_to_rocky(&text).await {
            Ok(out) => {
                println!("{out}");
                match find_url(&out) {
                    Some(u) => println!("thread: {u}"),
                    None => println!("handed to Rocky (no thread URL in the output)"),
                }
            }
            Err(e) => {
                println!("not handed to Rocky: {e:#}");
                match &key {
                    Some(k) => {
                        println!("the report is still in the bucket: s3://wsconnect-logs/{k}")
                    }
                    None => println!("the report above was not uploaded either"),
                }
            }
        }
    }
    match failures(&sections) {
        0 => Ok(()),
        n => bail!("{n} step(s) failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_report() {
        let r = render(
            "t",
            &[(
                "a".into(),
                vec![
                    Step::new("x", Res::Pass, Some(12), "ok"),
                    Step::new("y", Res::Fail, None, "boom"),
                    Step::skip("z", "--no-hotspot"),
                ],
            )],
        );
        assert!(r.contains("PASS     12 ms  x: ok"), "{r}");
        assert!(r.contains("FAIL         -  y: boom"), "{r}");
        assert!(r.contains("SKIP         -  z: --no-hotspot"), "{r}");
        assert!(r.ends_with("summary: 1 pass, 1 fail, 1 skip\n"), "{r}");
    }

    #[test]
    fn steps_roundtrip_json() {
        let s = vec![Step::new("ping", Res::Pass, Some(80), "80 ms")];
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v[0]["res"], "PASS");
        assert_eq!(serde_json::from_value::<Vec<Step>>(v).unwrap(), s);
    }

    #[test]
    fn finds_thread_url() {
        assert_eq!(
            find_url("action: new\nthread: abc\nurl: https://rocky.example/t/abc\n"),
            Some("https://rocky.example/t/abc")
        );
        assert_eq!(find_url("nothing"), None);
    }
}
