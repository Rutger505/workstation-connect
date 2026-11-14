//! CLI client for the daemon socket, plus the waybar module.

use crate::daemon::{Mode, socket_path};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub async fn request(cmd: &str) -> Result<String> {
    let mut s = UnixStream::connect(socket_path()).await.context("daemon not running (systemctl --user status wsconnect)")?;
    s.write_all(format!("{cmd}\n").as_bytes()).await?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).await?;
    Ok(line.trim().to_string())
}

/// Streams waybar JSON (return-type json) forever; survives daemon restarts.
pub async fn waybar() -> Result<()> {
    loop {
        if let Ok(mut s) = UnixStream::connect(socket_path()).await {
            s.write_all(b"watch\n").await?;
            let mut lines = BufReader::new(s).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    println!("{}", format_waybar(&v));
                }
            }
        }
        println!("{}", json!({"text": "󰄜 ?", "tooltip": "wsconnect daemon not running", "class": "down"}));
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn mode_of(v: &Value) -> Option<Mode> {
    serde_json::from_value(v.get("mode")?.clone()).ok()
}

pub fn format_waybar(v: &Value) -> Value {
    let mode = mode_of(v).unwrap_or(Mode::Idle);
    let phone = &v["phone"];
    let connected = phone["connected"].as_bool().unwrap_or(false);
    let state = &phone["state"];
    let battery = state["battery"].as_u64();
    let bars = state["signal"].as_u64().map(|s| ["▁", "▂", "▄", "▆", "█"][s.min(4) as usize]);

    let icon = match mode {
        Mode::Hotspot => "󰀂",
        Mode::Bluetooth => "󰂯",
        Mode::Activating => "󰑓",
        Mode::Offline | Mode::Cooldown => "󰖪",
        Mode::Paused => "󰏤",
        Mode::Idle => "󰄜",
    };
    let mut text = icon.to_string();
    if connected {
        if let Some(b) = battery {
            text.push_str(&format!(" {b}%"));
        }
        if let Some(bars) = bars {
            text.push_str(&format!(" {bars}"));
        }
    }

    let mut tip = vec![format!("mode: {}", serde_json::to_string(&mode).unwrap_or_default().trim_matches('"'))];
    tip.push(if connected { "phone: connected".into() } else { "phone: not in reach".into() });
    if connected && state["shizuku"] == false {
        tip.push("Shizuku not running on phone".into());
    }
    if let Some(s) = v["offline_secs"].as_u64() {
        tip.push(format!("offline for {s}s"));
    }
    if let Some(e) = v["error"].as_str() {
        tip.push(format!("error: {e}"));
    }
    tip.push("click: pause/resume".into());

    let class = match mode {
        Mode::Hotspot | Mode::Bluetooth => "tethered",
        Mode::Activating => "activating",
        Mode::Offline | Mode::Cooldown => "offline",
        Mode::Paused => "paused",
        Mode::Idle if !connected => "nophone",
        Mode::Idle => "idle",
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
        let w = format_waybar(&v);
        assert_eq!(w["text"], "󰀂 81% ▆");
        assert_eq!(w["class"], "tethered");
    }

    #[test]
    fn formats_no_phone() {
        let v = json!({"mode": "idle", "paused": false, "phone": {"connected": false, "state": null}});
        assert_eq!(format_waybar(&v)["class"], "nophone");
    }
}
