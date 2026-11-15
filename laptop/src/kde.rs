//! KDE Connect engine: talks to kdeconnectd on the session bus. kdeconnectd does the protocol
//! (pairing, encryption, notifications, clipboard, files); this is only our own UI on top.
//! API: kdeconnect-kde core/daemon.h, core/device.h, plugins/*/…plugin.h (Q_SCRIPTABLE / Q_PROPERTY).

use anyhow::{Context, Result, bail};
use serde::Serialize;
use zbus::{Connection, Proxy};

const DEST: &str = "org.kde.kdeconnect";
const ROOT: &str = "/modules/kdeconnect";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Phone {
    pub id: String,
    pub name: String,
    pub reachable: bool,
    pub battery: Option<i32>,
    pub charging: bool,
    /// e.g. "5G", "LTE"
    pub network: Option<String>,
    /// 0-4
    pub signal: Option<i32>,
}

pub struct Kde {
    conn: Connection,
}

impl Kde {
    pub async fn connect() -> Result<Self> {
        Ok(Self {
            conn: Connection::session().await.context("session bus")?,
        })
    }

    async fn proxy(&self, path: String, iface: &'static str) -> Result<Proxy<'static>> {
        Ok(Proxy::new(&self.conn, DEST, path, iface).await?)
    }

    async fn device(
        &self,
        id: &str,
        plugin: Option<&str>,
        iface: &'static str,
    ) -> Result<Proxy<'static>> {
        let mut path = format!("{ROOT}/devices/{id}");
        if let Some(p) = plugin {
            path.push('/');
            path.push_str(p);
        }
        self.proxy(path, iface).await
    }

    /// Paired device ids.
    pub async fn paired(&self) -> Result<Vec<String>> {
        let daemon = self.proxy(ROOT.into(), "org.kde.kdeconnect.daemon").await?;
        daemon
            .call("devices", &(false, true))
            .await
            .context("kdeconnectd not running? (kdeconnectd / kdeconnect-indicator)")
    }

    /// The phone: `want` (device id or name) if set, else the first paired smartphone/tablet.
    pub async fn find_phone(&self, want: Option<&str>) -> Result<String> {
        let ids = self.paired().await?;
        for id in &ids {
            let dev = self.device(id, None, "org.kde.kdeconnect.device").await?;
            let name: String = dev.get_property("name").await.unwrap_or_default();
            let kind: String = dev.get_property("type").await.unwrap_or_default();
            let hit = match want {
                Some(w) => w == id || w == name,
                None => kind == "smartphone" || kind == "phone" || kind == "tablet",
            };
            if hit {
                return Ok(id.clone());
            }
        }
        match want {
            Some(w) => bail!("no paired KDE Connect device '{w}'"),
            None => bail!("no paired phone in KDE Connect: pair it from the phone app first"),
        }
    }

    pub async fn phone(&self, id: &str) -> Result<Phone> {
        let dev = self.device(id, None, "org.kde.kdeconnect.device").await?;
        let reachable: bool = dev.get_property("isReachable").await?;
        let name: String = dev.get_property("name").await?;
        let (mut battery, mut charging, mut network, mut signal) = (None, false, None, None);
        if reachable {
            // Plugin objects only exist while the plugin is loaded: missing = unknown, not an error.
            if let Ok(b) = self
                .device(id, Some("battery"), "org.kde.kdeconnect.device.battery")
                .await
            {
                battery = b
                    .get_property::<i32>("charge")
                    .await
                    .ok()
                    .filter(|c| *c >= 0);
                charging = b.get_property("isCharging").await.unwrap_or(false);
            }
            if let Ok(c) = self
                .device(
                    id,
                    Some("connectivity_report"),
                    "org.kde.kdeconnect.device.connectivity_report",
                )
                .await
            {
                network = c
                    .get_property::<String>("cellularNetworkType")
                    .await
                    .ok()
                    .filter(|s| !s.is_empty());
                signal = c
                    .get_property::<i32>("cellularNetworkStrength")
                    .await
                    .ok()
                    .filter(|s| *s >= 0);
            }
        }
        Ok(Phone {
            id: id.into(),
            name,
            reachable,
            battery,
            charging,
            network,
            signal,
        })
    }

    pub async fn send_clipboard(&self, id: &str) -> Result<()> {
        let p = self
            .device(id, Some("clipboard"), "org.kde.kdeconnect.device.clipboard")
            .await?;
        Ok(p.call("sendClipboard", &()).await?)
    }

    /// Files as file:// URLs, or http(s) links (opened on the phone).
    pub async fn share(&self, id: &str, urls: Vec<String>) -> Result<()> {
        let p = self
            .device(id, Some("share"), "org.kde.kdeconnect.device.share")
            .await?;
        Ok(p.call("shareUrls", &(urls,)).await?)
    }

    /// Mounts the phone over sftp; returns the mount point.
    pub async fn mount(&self, id: &str) -> Result<String> {
        let p = self
            .device(id, Some("sftp"), "org.kde.kdeconnect.device.sftp")
            .await?;
        let ok: bool = p.call("mountAndWait", &()).await?;
        if !ok {
            let err: String = p.call("getMountError", &()).await.unwrap_or_default();
            bail!("mount failed: {err} (allow file access in the phone app)");
        }
        Ok(p.call("mountPoint", &()).await?)
    }
}

impl Kde {
    pub async fn set_plugin(&self, id: &str, plugin: &str, on: bool) -> Result<()> {
        let dev = self.device(id, None, "org.kde.kdeconnect.device").await?;
        Ok(dev.call("setPluginEnabled", &(plugin, on)).await?)
    }

    /// Link providers as (id, enabled), e.g. ("bluetooth", true).
    pub async fn link_providers(&self) -> Result<Vec<(String, bool)>> {
        let daemon = self.proxy(ROOT.into(), "org.kde.kdeconnect.daemon").await?;
        let lines: Vec<String> = daemon.call("linkProviders", &()).await?;
        Ok(lines.iter().filter_map(|l| parse_provider(l)).collect())
    }

    pub async fn set_link_provider(&self, provider: &str, on: bool) -> Result<()> {
        let daemon = self.proxy(ROOT.into(), "org.kde.kdeconnect.daemon").await?;
        Ok(daemon.call("setLinkProviderState", &(provider, on)).await?)
    }
}

/// Pause = KDE Connect fully off too: disables every enabled link provider and remembers which
/// ones (file in XDG_RUNTIME_DIR, survives daemon restarts); resume re-enables exactly those.
pub async fn pause_sync(on: bool) -> Result<()> {
    let state = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("wsconnect-paused-providers");
    let kde = Kde::connect().await?;
    if on {
        let enabled: Vec<String> = kde
            .link_providers()
            .await?
            .into_iter()
            .filter(|(_, e)| *e)
            .map(|(id, _)| id)
            .collect();
        if enabled.is_empty() {
            return Ok(()); // already paused (or nothing on): keep the remembered list
        }
        std::fs::write(&state, enabled.join("\n"))?;
        for id in &enabled {
            kde.set_link_provider(id, false).await?;
        }
    } else {
        let Ok(text) = std::fs::read_to_string(&state) else {
            return Ok(());
        };
        for id in text.lines().filter(|l| !l.is_empty()) {
            kde.set_link_provider(id, true).await?;
        }
        std::fs::remove_file(&state)?;
    }
    Ok(())
}

/// "Bluetooth|bluetooth|enabled" → ("bluetooth", true)
fn parse_provider(line: &str) -> Option<(String, bool)> {
    let mut parts = line.split('|');
    let (_name, id, state) = (parts.next()?, parts.next()?, parts.next()?);
    Some((id.to_string(), state == "enabled"))
}

/// Sets `key=value` in the [General] section of a QSettings INI file (kdeconnectd plugin config).
pub fn set_ini(text: &str, key: &str, value: &str) -> String {
    let mut out = Vec::new();
    let (mut in_general, mut seen_general, mut done) = (false, false, false);
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if in_general && !done {
                out.push(format!("{key}={value}"));
                done = true;
            }
            in_general = t == "[General]";
            seen_general |= in_general;
        } else if in_general && t.split('=').next().map(str::trim) == Some(key) {
            out.push(format!("{key}={value}"));
            done = true;
            continue;
        }
        out.push(line.to_string());
    }
    if !done {
        if !seen_general {
            out.push("[General]".into());
        }
        out.push(format!("{key}={value}"));
    }
    out.join("\n") + "\n"
}

/// file:// URL for a local path (absolute), percent-encoding what URLs can't hold.
pub fn file_url(path: &std::path::Path) -> Result<String> {
    let abs = std::path::absolute(path)?;
    let mut out = String::from("file://");
    for b in abs.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_providers() {
        assert_eq!(
            parse_provider("Bluetooth|bluetooth|disabled"),
            Some(("bluetooth".into(), false))
        );
        assert_eq!(parse_provider("garbage"), None);
    }

    #[test]
    fn edits_ini() {
        assert_eq!(
            set_ini("", "incoming_path", "/x"),
            "[General]\nincoming_path=/x\n"
        );
        assert_eq!(
            set_ini(
                "[General]\nincoming_path=/old\nfoo=1\n",
                "incoming_path",
                "/x"
            ),
            "[General]\nincoming_path=/x\nfoo=1\n"
        );
        assert_eq!(
            set_ini("[General]\nfoo=1\n[Other]\na=b\n", "incoming_path", "/x"),
            "[General]\nfoo=1\nincoming_path=/x\n[Other]\na=b\n"
        );
    }

    #[test]
    fn encodes_file_urls() {
        assert_eq!(
            file_url(std::path::Path::new("/tmp/a b#é.txt")).unwrap(),
            "file:///tmp/a%20b%23%C3%A9.txt"
        );
    }
}
