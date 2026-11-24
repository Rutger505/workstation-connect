//! One-shot actions on the phone (menu, Hyprland binds) via kdeconnectd, plus scrcpy.

use crate::config::Config;
use crate::kde::{Kde, file_url};
use anyhow::{Context, Result, bail};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, info, warn};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Clipboard,
    SendFiles,
    Browse,
    Screen,
    Pause,
    /// Only in the menu while the daemon reports an update.
    Update,
}

impl Action {
    pub const MENU: [Action; 5] = [
        Action::SendFiles,
        Action::Clipboard,
        Action::Browse,
        Action::Screen,
        Action::Pause,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Action::SendFiles => "󰁜  Send files to phone",
            Action::Clipboard => "󰅌  Send clipboard to phone",
            Action::Browse => "󰉋  Browse phone files",
            Action::Screen => "󰍹  Phone screen (scrcpy)",
            Action::Pause => "󰏤  Pause / resume auto hotspot",
            Action::Update => "󰚰  Update workstation-connect",
        }
    }
}

async fn phone(kde: &Kde) -> Result<String> {
    let want = match Config::load() {
        Ok(c) => c.kdeconnect_device,
        Err(e) => {
            debug!("{e:#}: first paired phone");
            None
        }
    };
    kde.find_phone(want.as_deref()).await
}

pub async fn run(action: Action, files: Vec<std::path::PathBuf>) -> Result<()> {
    info!(?action, files = files.len(), "action");
    if action == Action::Pause {
        println!("{}", crate::ctl::request("toggle").await?);
        return Ok(());
    }
    if action == Action::Screen {
        return screen().await;
    }
    if action == Action::Update {
        return update_in_terminal();
    }
    let kde = Kde::connect().await?;
    let id = phone(&kde).await?;
    match action {
        Action::Clipboard => kde.send_clipboard(&id).await,
        Action::SendFiles => {
            let files = if files.is_empty() {
                pick_files().await?
            } else {
                files
            };
            if files.is_empty() {
                debug!("send files: nothing picked");
                return Ok(()); // picker cancelled
            }
            let urls = files
                .iter()
                .map(|f| file_url(f))
                .collect::<Result<Vec<_>>>()?;
            kde.share(&id, urls).await
        }
        Action::Browse => {
            let dir = kde.mount(&id).await?;
            debug!("xdg-open {dir}");
            Command::new("xdg-open")
                .arg(&dir)
                .spawn()
                .context("xdg-open")?;
            Ok(())
        }
        Action::Screen | Action::Pause | Action::Update => unreachable!(),
    }
}

/// One-time kdeconnectd setup for the phone: laptop notifications → phone, Bluetooth link (no shared
/// network needed), incoming files/photos folder. Safe to re-run.
pub async fn setup() -> Result<()> {
    info!("setup");
    let cfg = Config::load()
        .map_err(|e| warn!("{e:#}: setting up without config (share_dir not applied)"))
        .ok();
    let kde = Kde::connect().await?;
    let id = phone(&kde).await?;
    let p = kde.phone(&id).await?;
    println!("phone: {} ({id})", p.name);

    kde.set_plugin(&id, "kdeconnect_sendnotifications", true)
        .await?;
    println!("laptop notifications → phone: on");

    match kde
        .link_providers()
        .await?
        .into_iter()
        .find(|(p, _)| p == "bluetooth")
    {
        Some((_, true)) => println!("bluetooth link: on"),
        Some((_, false)) => {
            kde.set_link_provider("bluetooth", true).await?;
            println!("bluetooth link: turned on");
        }
        None => println!("bluetooth link: not built into this kdeconnect (needs qt6-connectivity)"),
    }

    if let Some(dir) = cfg.and_then(|c| c.share_dir) {
        let dir = expand_home(&dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let conf = crate::config::xdg_config()
            .join("kdeconnect")
            .join(&id)
            .join("kdeconnect_share");
        std::fs::create_dir_all(&conf)?;
        let file = conf.join("config");
        let old = std::fs::read_to_string(&file).unwrap_or_else(|e| {
            debug!("{}: {e}: starting empty", file.display());
            String::new()
        });
        std::fs::write(
            &file,
            crate::kde::set_ini(&old, "incoming_path", &dir.to_string_lossy()),
        )?;
        println!("files/photos from phone → {}", dir.display());
    } else {
        println!(
            "files/photos from phone → kdeconnect default (~/Downloads); set share_dir in config.toml to change"
        );
    }
    Ok(())
}

fn expand_home(p: &str) -> std::path::PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest)
        }
        None => p.into(),
    }
}

/// Launcher menu (walker/fuzzel/wofi/rofi in dmenu mode, or config `menu`).
pub async fn menu() -> Result<()> {
    let configured = match Config::load() {
        Ok(c) => c.menu,
        Err(e) => {
            debug!("{e:#}: detecting the menu program");
            None
        }
    };
    let cmd = match configured {
        Some(c) if !c.is_empty() => c,
        _ => detect_menu()?,
    };
    debug!(menu = ?cmd, "menu");
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {}", cmd[0]))?;
    let view = fresh_status().await;
    let info = info_line(crate::update::VERSION, &view);
    let mut actions = Action::MENU.to_vec();
    if offer_update(&view) {
        actions.push(Action::Update);
    }
    let input = std::iter::once(info.as_str())
        .chain(actions.iter().map(|a| a.label()))
        .collect::<Vec<_>>()
        .join("\n");
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(input.as_bytes()).await?;
    drop(stdin);
    let out = child.wait_with_output().await?;
    let picked = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if picked == info.trim() {
        debug!("menu: info line picked, nothing to do");
        return Ok(());
    }
    match actions.into_iter().find(|a| a.label() == picked) {
        Some(a) => run(a, vec![]).await,
        None => {
            debug!(status = %out.status, "menu: nothing picked");
            Ok(()) // cancelled
        }
    }
}

/// The menu waits this long for the daemon's update check, then opens without its result.
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

const INFO_ICON: &str = "󰋼";

/// Daemon status as the menu sees it when opening.
#[derive(Debug)]
enum MenuStatus {
    NoDaemon,
    /// The update check took longer than CHECK_TIMEOUT; last known status if the daemon gave one.
    CheckTimedOut(Option<serde_json::Value>),
    /// Status right after a fresh update check (or the current one when the daemon couldn't check).
    Checked(serde_json::Value),
}

impl MenuStatus {
    fn status(&self) -> Option<&serde_json::Value> {
        match self {
            MenuStatus::NoDaemon | MenuStatus::CheckTimedOut(None) => None,
            MenuStatus::CheckTimedOut(Some(v)) | MenuStatus::Checked(v) => Some(v),
        }
    }
}

async fn status_request(cmd: &str) -> Result<serde_json::Value> {
    let answer = crate::ctl::request(cmd).await?;
    serde_json::from_str(&answer).with_context(|| format!("daemon answer to {cmd}"))
}

/// Has the daemon check GitHub now (ctl `check`), at most CHECK_TIMEOUT.
async fn fresh_status() -> MenuStatus {
    let t = std::time::Instant::now();
    let view = match tokio::time::timeout(CHECK_TIMEOUT, status_request("check")).await {
        Ok(Ok(v)) => MenuStatus::Checked(v),
        Ok(Err(e)) => {
            // Daemons before `check` close the connection without an answer: plain status.
            debug!("menu: update check: {e:#}; asking for the plain status");
            match status_request("status").await {
                Ok(v) => MenuStatus::Checked(v),
                Err(e) => {
                    debug!("menu: no daemon status: {e:#}");
                    MenuStatus::NoDaemon
                }
            }
        }
        Err(_) => {
            warn!(
                "menu: update check gave no result in {} s, opening with the last known status",
                CHECK_TIMEOUT.as_secs()
            );
            let last = status_request("status")
                .await
                .map_err(|e| debug!("menu: no daemon status: {e:#}"))
                .ok();
            MenuStatus::CheckTimedOut(last)
        }
    };
    debug!(
        elapsed_ms = t.elapsed().as_millis() as u64,
        view = view_name(&view),
        "menu: daemon status"
    );
    view
}

fn view_name(v: &MenuStatus) -> &'static str {
    match v {
        MenuStatus::NoDaemon => "no daemon",
        MenuStatus::CheckTimedOut(_) => "check timed out",
        MenuStatus::Checked(_) => "checked",
    }
}

/// Update item in the menu: daemon reports an update, not while an auto-update runs (its lock
/// would refuse anyway).
fn offer_update(view: &MenuStatus) -> bool {
    view.status()
        .is_some_and(|v| v["update"].is_object() && v["update"]["state"] != "updating")
}

/// First menu line: installed version and update state. Picking it does nothing.
fn info_line(installed: &str, view: &MenuStatus) -> String {
    let mut line = format!("{INFO_ICON}  workstation-connect {installed}");
    let status = view.status();
    if let Some(running) = status.and_then(|v| v["version"].as_str())
        && running != installed
    {
        line.push_str(&format!(" (running daemon: {running})"));
    }
    let update = status.map(|v| &v["update"]).filter(|u| u.is_object());
    let latest = |u: &serde_json::Value| u["latest"].as_str().unwrap_or("?").to_string();
    let outcome = status.and_then(|v| v["update_check"]["outcome"].as_str());
    let state = match (view, update) {
        (MenuStatus::NoDaemon, _) => "daemon not running, update state unknown".to_string(),
        (_, Some(u)) if u["state"] == "updating" => {
            "newer version on GitHub, updating…".to_string()
        }
        (MenuStatus::CheckTimedOut(_), _) => format!(
            "update check took over {} s, result unknown",
            CHECK_TIMEOUT.as_secs()
        ),
        (_, _) if outcome == Some("failed") => "update check failed".to_string(),
        (_, Some(u)) if u["state"] == "failed" => {
            format!(
                "newer version on GitHub ({}), last update failed",
                latest(u)
            )
        }
        (_, Some(u)) => format!("newer version on GitHub ({})", latest(u)),
        (_, None) if outcome == Some("up_to_date") => "up to date".to_string(),
        (_, None) if status.is_some_and(|v| v["commit"] == "unknown") => {
            "update check off (build commit unknown)".to_string()
        }
        (_, None) => "update state unknown".to_string(),
    };
    line.push_str(" · ");
    line.push_str(&state);
    line
}

/// `workstation-connect update` in a terminal: xdg-terminal-exec, else $TERMINAL -e.
fn update_in_terminal() -> Result<()> {
    let args = ["workstation-connect", "update", "--pause"];
    let mut cmd = if which("xdg-terminal-exec") {
        std::process::Command::new("xdg-terminal-exec")
    } else {
        let term = std::env::var("TERMINAL")
            .ok()
            .filter(|t| !t.is_empty())
            .context("no terminal: install xdg-terminal-exec or set $TERMINAL")?;
        let mut c = std::process::Command::new(term);
        c.arg("-e");
        c
    };
    debug!(terminal = ?cmd.get_program(), "update in a terminal");
    cmd.args(args).spawn().context("start terminal")?;
    Ok(())
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

fn detect_menu() -> Result<Vec<String>> {
    let known: [&[&str]; 4] = [
        &["walker", "--dmenu"],
        &["fuzzel", "--dmenu"],
        &["wofi", "--dmenu"],
        &["rofi", "-dmenu"],
    ];
    known
        .iter()
        .find(|c| which(c[0]))
        .map(|c| c.iter().map(|s| s.to_string()).collect())
        .context("no menu program (walker/fuzzel/wofi/rofi); set `menu` in config.toml")
}

async fn pick_files() -> Result<Vec<std::path::PathBuf>> {
    if !which("zenity") {
        bail!("no file picker: install zenity, or run `workstation-connect send <files>`");
    }
    let out = Command::new("zenity")
        .args([
            "--file-selection",
            "--multiple",
            "--separator=\n",
            "--title=Send to phone",
        ])
        .output()
        .await?;
    // Exit code 1 = cancelled; only the count is logged (file names are private).
    let files: Vec<std::path::PathBuf> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(Into::into)
        .collect();
    debug!(status = %out.status, files = files.len(), "zenity file picker");
    Ok(files)
}

/// scrcpy over wireless debugging (already on for Shizuku). Uses an existing adb connection,
/// else connects to the phone found via mDNS (same network only; first time: `adb pair`).
async fn screen() -> Result<()> {
    if !which("scrcpy") || !which("adb") {
        bail!("install scrcpy + android-tools");
    }
    if adb_has_device().await? {
        debug!("screen: adb already has a device");
    } else {
        let out = Command::new("adb")
            .args(["mdns", "services"])
            .output()
            .await?;
        let services = String::from_utf8_lossy(&out.stdout);
        let target = parse_mdns_connect(&services).context(
            "phone not found via mDNS: wireless debugging on? same network? first time: adb pair <ip:port> <code>",
        )?;
        let out = Command::new("adb")
            .args(["connect", &target])
            .output()
            .await?;
        let msg = String::from_utf8_lossy(&out.stdout);
        if !msg.contains("connected") {
            bail!("adb connect {target}: {}", msg.trim());
        }
        info!("screen: adb connected to {target}");
    }
    info!("screen: starting scrcpy");
    let status = Command::new("scrcpy")
        .args(["--stay-awake", "--turn-screen-off"])
        .status()
        .await?;
    if !status.success() {
        bail!("scrcpy exited with {status}");
    }
    Ok(())
}

async fn adb_has_device() -> Result<bool> {
    let out = Command::new("adb").arg("devices").output().await?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .any(|l| l.trim_end().ends_with("\tdevice")))
}

/// `adb mdns services` line: "<name>\t_adb-tls-connect._tcp\t192.168.1.5:37123"
fn parse_mdns_connect(services: &str) -> Option<String> {
    services
        .lines()
        .filter(|l| l.contains("_adb-tls-connect._tcp"))
        .filter_map(|l| l.split_whitespace().last())
        .find(|addr| addr.contains(':'))
        .map(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn parses_mdns() {
        let s = "List of discovered mdns services\nadb-4A1B-xyz\t_adb-tls-pairing._tcp\t192.168.1.5:40001\nadb-4A1B-xyz\t_adb-tls-connect._tcp\t192.168.1.5:37123\n";
        assert_eq!(parse_mdns_connect(s).as_deref(), Some("192.168.1.5:37123"));
        assert_eq!(
            parse_mdns_connect("List of discovered mdns services\n"),
            None
        );
    }

    #[test]
    fn menu_labels_unique() {
        let mut l: Vec<_> = Action::MENU.iter().map(|a| a.label()).collect();
        l.push(Action::Update.label());
        l.sort();
        l.dedup();
        assert_eq!(l.len(), Action::MENU.len() + 1);
        assert!(
            l.iter().all(|a| !a.starts_with(INFO_ICON)),
            "info line looks like an action"
        );
    }

    fn checked(update: Value, check: Value) -> MenuStatus {
        MenuStatus::Checked(
            json!({"version": "v12", "commit": "0123456789012345678901234567890123456789",
            "update": update, "update_check": check}),
        )
    }

    #[test]
    fn info_line_states() {
        let info = |v: &MenuStatus| info_line("v12", v);
        let ok = json!({"at": 1, "outcome": "up_to_date", "latest": "abc1234", "error": null});
        let out = json!({"at": 1, "outcome": "outdated", "latest": "def5678", "error": null});
        let bad =
            json!({"at": 1, "outcome": "failed", "latest": null, "error": "git ls-remote: …"});
        let upd = |state: &str| json!({"installed": "v12", "latest": "def5678", "state": state});

        assert_eq!(
            info(&checked(Value::Null, ok.clone())),
            "󰋼  workstation-connect v12 · up to date"
        );
        assert_eq!(
            info(&checked(upd("available"), out.clone())),
            "󰋼  workstation-connect v12 · newer version on GitHub (def5678)"
        );
        assert!(
            info(&checked(upd("updating"), out.clone()))
                .ends_with("· newer version on GitHub, updating…")
        );
        assert!(info(&checked(upd("failed"), out)).ends_with("(def5678), last update failed"));
        assert!(info(&checked(Value::Null, bad.clone())).ends_with("· update check failed"));
        // Failed check with an older outdated result: the failure wins, update item stays.
        assert!(info(&checked(upd("available"), bad.clone())).ends_with("· update check failed"));
        assert!(offer_update(&checked(upd("available"), bad)));
        // Daemon before update_check, or no check yet.
        assert!(info(&checked(Value::Null, Value::Null)).ends_with("· update state unknown"));
        assert!(
            info(&MenuStatus::NoDaemon).ends_with("· daemon not running, update state unknown")
        );
        assert!(info(&MenuStatus::CheckTimedOut(None)).ends_with("took over 3 s, result unknown"));
        let unknown =
            MenuStatus::Checked(json!({"version": "unknown", "commit": "unknown", "update": null}));
        assert_eq!(
            info_line("unknown", &unknown),
            "󰋼  workstation-connect unknown · update check off (build commit unknown)"
        );
        let old_daemon =
            MenuStatus::Checked(json!({"version": "v11", "update": null, "update_check": ok}));
        assert!(info(&old_daemon).contains("v12 (running daemon: v11) · up to date"));
    }

    #[test]
    fn update_item_only_when_installable() {
        let upd = |state: &str| json!({"installed": "v12", "latest": "def5678", "state": state});
        assert!(offer_update(&checked(upd("available"), Value::Null)));
        assert!(offer_update(&checked(upd("failed"), Value::Null)));
        assert!(!offer_update(&checked(upd("updating"), Value::Null)));
        assert!(!offer_update(&checked(Value::Null, Value::Null)));
        assert!(!offer_update(&MenuStatus::NoDaemon));
        let last = json!({"update": upd("available")});
        assert!(offer_update(&MenuStatus::CheckTimedOut(Some(last))));
    }
}
