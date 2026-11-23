//! One-shot actions on the phone (menu, Hyprland binds) via kdeconnectd, plus scrcpy.

use crate::config::Config;
use crate::kde::{Kde, file_url};
use anyhow::{Context, Result, bail};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

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
    let want = Config::load().ok().and_then(|c| c.kdeconnect_device);
    kde.find_phone(want.as_deref()).await
}

pub async fn run(action: Action, files: Vec<std::path::PathBuf>) -> Result<()> {
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
    let cfg = Config::load().ok();
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
        let old = std::fs::read_to_string(&file).unwrap_or_default();
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
    let cmd = match Config::load().ok().and_then(|c| c.menu) {
        Some(c) if !c.is_empty() => c,
        _ => detect_menu()?,
    };
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {}", cmd[0]))?;
    let outdated = crate::ctl::request("status")
        .await
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        // Not while an auto-update runs (its lock would refuse anyway).
        .is_some_and(|v| v["update"].is_object() && v["update"]["state"] != "updating");
    let mut actions = Action::MENU.to_vec();
    if outdated {
        actions.push(Action::Update);
    }
    let input = actions
        .iter()
        .map(|a| a.label())
        .collect::<Vec<_>>()
        .join("\n");
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(input.as_bytes()).await?;
    drop(stdin);
    let out = child.wait_with_output().await?;
    let picked = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match actions.into_iter().find(|a| a.label() == picked) {
        Some(a) => run(a, vec![]).await,
        None => Ok(()), // cancelled
    }
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
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(Into::into)
        .collect())
}

/// scrcpy over wireless debugging (already on for Shizuku). Uses an existing adb connection,
/// else connects to the phone found via mDNS (same network only; first time: `adb pair`).
async fn screen() -> Result<()> {
    if !which("scrcpy") || !which("adb") {
        bail!("install scrcpy + android-tools");
    }
    if !adb_has_device().await? {
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
    }
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
    }
}
