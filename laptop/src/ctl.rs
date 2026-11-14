//! CLI client for the daemon socket, plus the waybar module.

use crate::config::Config;
use crate::daemon::{Mode, socket_path};
use crate::kde::{Kde, Phone};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::watch;

pub async fn request(cmd: &str) -> Result<String> {
    let mut s = UnixStream::connect(socket_path())
        .await
        .context("daemon not running (systemctl --user status wsconnect)")?;
    s.write_all(format!("{cmd}\n").as_bytes()).await?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).await?;
    Ok(line.trim().to_string())
}

/// What the waybar module knows about KDE Connect.
#[derive(Debug, Clone, PartialEq)]
pub enum KdeView {
    Unknown,
    Error(String),
    Phone(Phone),
}

async fn kde_view(kde: &mut Option<Kde>, want: Option<&str>) -> KdeView {
    if kde.is_none() {
        match Kde::connect().await {
            Ok(k) => *kde = Some(k),
            Err(e) => return KdeView::Error(e.to_string()),
        }
    }
    let k = kde.as_ref().expect("set above");
    let id = match k.find_phone(want).await {
        Ok(id) => id,
        Err(e) => return KdeView::Error(e.to_string()),
    };
    match k.phone(&id).await {
        Ok(p) => KdeView::Phone(p),
        Err(e) => KdeView::Error(e.to_string()),
    }
}

/// Streams waybar JSON (return-type json) forever: daemon status (BLE/hotspot) merged with
/// KDE Connect (polled every 5 s). Survives daemon and kdeconnectd restarts.
pub async fn waybar() -> Result<()> {
    let (daemon_tx, mut daemon_rx) = watch::channel::<Option<Value>>(None);
    tokio::spawn(async move {
        loop {
            if let Ok(mut s) = UnixStream::connect(socket_path()).await
                && s.write_all(b"watch\n").await.is_ok()
            {
                let mut lines = BufReader::new(s).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        daemon_tx.send_replace(Some(v));
                    }
                }
            }
            daemon_tx.send_replace(None);
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    let (kde_tx, mut kde_rx) = watch::channel(KdeView::Unknown);
    tokio::spawn(async move {
        let want = Config::load().ok().and_then(|c| c.kdeconnect_device);
        let mut kde = None;
        loop {
            let view = kde_view(&mut kde, want.as_deref()).await;
            if matches!(view, KdeView::Error(_)) {
                kde = None; // reconnect next time (bus or kdeconnectd restarted)
            }
            kde_tx.send_if_modified(|old| {
                if *old != view {
                    *old = view;
                    true
                } else {
                    false
                }
            });
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    let mut last = Value::Null;
    loop {
        let out = format_waybar(
            daemon_rx.borrow_and_update().as_ref(),
            &kde_rx.borrow_and_update(),
        );
        if out != last {
            println!("{out}");
            last = out;
        }
        tokio::select! {
            r = daemon_rx.changed() => r?,
            r = kde_rx.changed() => r?,
        }
    }
}

fn mode_of(v: &Value) -> Option<Mode> {
    serde_json::from_value(v.get("mode")?.clone()).ok()
}

const BARS: [&str; 5] = ["▁", "▂", "▄", "▆", "█"];

pub fn format_waybar(daemon: Option<&Value>, kde: &KdeView) -> Value {
    let mode = daemon.and_then(mode_of);
    let phone = daemon.map(|v| &v["phone"]);
    let ble = phone
        .and_then(|p| p["connected"].as_bool())
        .unwrap_or(false);
    let state = phone.map(|p| &p["state"]);
    let kp = match kde {
        KdeView::Phone(p) if p.reachable => Some(p),
        _ => None,
    };

    // BLE state is freshest when connected; KDE Connect covers the rest of the time.
    let battery = state
        .filter(|_| ble)
        .and_then(|s| s["battery"].as_u64())
        .map(|b| b as i64)
        .or_else(|| kp.and_then(|p| p.battery).map(i64::from));
    let signal = state
        .filter(|_| ble)
        .and_then(|s| s["signal"].as_u64())
        .filter(|s| *s <= 4)
        .map(|s| s as i64)
        .or_else(|| kp.and_then(|p| p.signal).map(i64::from));

    let icon = match mode {
        None => "󰄜",
        Some(Mode::Hotspot) => "󰀂",
        Some(Mode::Bluetooth) => "󰂯",
        Some(Mode::Activating) => "󰑓",
        Some(Mode::Offline | Mode::Cooldown) => "󰖪",
        Some(Mode::Paused) => "󰏤",
        Some(Mode::Idle) if kp.is_some() || ble => "󰄜",
        Some(Mode::Idle) => "󰥐",
    };
    let mut text = icon.to_string();
    if let Some(b) = battery {
        text.push_str(&format!(" {b}%"));
    }
    if let Some(s) = signal {
        text.push_str(&format!(" {}", BARS[s.clamp(0, 4) as usize]));
    }

    let mut tip = Vec::new();
    match mode {
        Some(m) => tip.push(format!(
            "mode: {}",
            serde_json::to_string(&m)
                .unwrap_or_default()
                .trim_matches('"')
        )),
        None => tip.push("wsconnect daemon not running (auto hotspot off)".into()),
    }
    if daemon.is_some() {
        tip.push(if ble {
            "bluetooth: phone in reach".into()
        } else {
            "bluetooth: phone not in reach".into()
        });
    }
    if ble && state.is_some_and(|s| s["shizuku"] == false) {
        tip.push("Shizuku not running on phone".into());
    }
    match kde {
        KdeView::Phone(p) if p.reachable => {
            let mut l = format!("KDE Connect: {} connected", p.name);
            if p.charging {
                l.push_str(", charging");
            }
            if let Some(n) = &p.network {
                l.push_str(&format!(", {n}"));
            }
            tip.push(l);
        }
        KdeView::Phone(p) => tip.push(format!("KDE Connect: {} not reachable", p.name)),
        KdeView::Error(e) => tip.push(format!("KDE Connect: {e}")),
        KdeView::Unknown => {}
    }
    if let Some(s) = daemon.and_then(|v| v["offline_secs"].as_u64()) {
        tip.push(format!("offline for {s}s"));
    }
    if let Some(e) = daemon.and_then(|v| v["error"].as_str()) {
        tip.push(format!("error: {e}"));
    }
    tip.push("click: pause/resume · right-click: menu".into());

    let class = match mode {
        None => "down",
        Some(Mode::Hotspot | Mode::Bluetooth) => "tethered",
        Some(Mode::Activating) => "activating",
        Some(Mode::Offline | Mode::Cooldown) => "offline",
        Some(Mode::Paused) => "paused",
        Some(Mode::Idle) if !ble && kp.is_none() => "nophone",
        Some(Mode::Idle) => "idle",
    };
    json!({"text": text, "tooltip": tip.join("\n"), "class": class})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_hotspot() {
        let v = json!({"mode": "hotspot", "paused": false, "offline_secs": null, "error": null,
            "phone": {"connected": true, "state": {"shizuku": true, "bt_tether": false, "hotspot": true,
            "wanted": true, "error": false, "battery": 81, "signal": 3}}});
        let w = format_waybar(Some(&v), &KdeView::Unknown);
        assert_eq!(w["text"], "󰀂 81% ▆");
        assert_eq!(w["class"], "tethered");
    }

    fn kde_phone(reachable: bool) -> KdeView {
        KdeView::Phone(Phone {
            id: "x".into(),
            name: "Pixel 9".into(),
            reachable,
            battery: Some(55),
            charging: true,
            network: Some("5G".into()),
            signal: Some(2),
        })
    }

    #[test]
    fn falls_back_to_kde_connect() {
        let v =
            json!({"mode": "idle", "paused": false, "phone": {"connected": false, "state": null}});
        let w = format_waybar(Some(&v), &kde_phone(true));
        assert_eq!(w["text"], "󰄜 55% ▄");
        assert_eq!(w["class"], "idle");
        assert!(
            w["tooltip"]
                .as_str()
                .unwrap()
                .contains("Pixel 9 connected, charging, 5G")
        );
    }

    #[test]
    fn daemon_down_still_shows_kde() {
        let w = format_waybar(None, &kde_phone(true));
        assert_eq!(w["class"], "down");
        assert_eq!(w["text"], "󰄜 55% ▄");
        assert_eq!(format_waybar(None, &kde_phone(false))["text"], "󰄜");
    }

    #[test]
    fn formats_no_phone() {
        let v =
            json!({"mode": "idle", "paused": false, "phone": {"connected": false, "state": null}});
        assert_eq!(
            format_waybar(Some(&v), &KdeView::Unknown)["class"],
            "nophone"
        );
        assert_eq!(
            format_waybar(Some(&v), &kde_phone(false))["class"],
            "nophone"
        );
    }
}
