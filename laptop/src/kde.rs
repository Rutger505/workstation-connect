//! KDE Connect engine: talks to kdeconnectd on the session bus. kdeconnectd does the protocol
//! (pairing, encryption, notifications, clipboard, files); this is only our own UI on top.
//! API: kdeconnect-kde core/daemon.h, core/device.h, plugins/*/…plugin.h (Q_SCRIPTABLE / Q_PROPERTY).

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::time::Duration;
use tracing::{debug, info, trace};
use zbus::{Connection, Proxy};

const DEST: &str = "org.kde.kdeconnect";
const ROOT: &str = "/modules/kdeconnect";
/// Every kdeconnectd call (zbus never times out by itself; kdeconnectd can hang for days with its
/// name still on the bus). Generous: `mountAndWait` waits for the phone.
const CALL: Duration = Duration::from_secs(30);
/// Health probe: a healthy kdeconnectd answers `selfId` in milliseconds.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// The user unit kdeconnectd must run in (xdg autostart + D-Bus activation override, README).
pub const UNIT: &str = "app-org.kde.kdeconnect.daemon@autostart.service";
/// systemctl calls (restart of a hung kdeconnectd: SIGTERM ends it, its main thread isn't needed).
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(30);

/// Qt logging rules for kdeconnectd: `kdeconnect.core` debug = discovery, link up/down, pairing,
/// plugin loading (checked against kdeconnect-kde core/: no packet bodies, those lines are
/// commented out upstream). Plugin categories (`kdeconnect.plugin.*`) stay off: clipboard and
/// notification content.
pub const QT_LOGGING_RULES: &str = "kdeconnect.core.debug=true";
/// Drop-in for the template, so it applies to the generated `@autostart` instance.
const DROPIN_DIR: &str = "systemd/user/app-org.kde.kdeconnect.daemon@.service.d";
const DROPIN_FILE: &str = "workstation-connect-logging.conf";

/// kdeconnectd's state as seen by a probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    /// Name not on the session bus: not started yet; D-Bus activation starts it on first use.
    NotRunning,
    /// On the bus but no answer within [`PROBE_TIMEOUT`].
    Hung,
}

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

/// Everything about the phone's KDE Connect device worth logging / testing.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub paired: bool,
    pub reachable: bool,
    /// Active link types, e.g. ["LanLinkProvider"].
    pub providers: Vec<String>,
    pub connection: String,
    /// Loaded plugin ids, sorted, e.g. "kdeconnect_mousepad".
    pub plugins: Vec<String>,
    /// D-Bus errors reading the above, verbatim (older kdeconnectd lacks some properties).
    pub errors: Vec<String>,
}

/// Plugins worth calling out: (id, what it's for).
pub const KEY_PLUGINS: [(&str, &str); 7] = [
    ("kdeconnect_mousepad", "trackpad/mouse input"),
    ("kdeconnect_remotekeyboard", "keyboard input"),
    ("kdeconnect_remotecontrol", "remote control"),
    ("kdeconnect_notifications", "phone notifications"),
    (
        "kdeconnect_sendnotifications",
        "laptop notifications to phone",
    ),
    ("kdeconnect_clipboard", "clipboard"),
    ("kdeconnect_ping", "ping"),
];

pub struct Kde {
    conn: Connection,
}

impl Kde {
    pub async fn connect() -> Result<Self> {
        let conn = zbus::connection::Builder::session()
            .context("session bus")?
            .method_timeout(CALL)
            .build()
            .await
            .context("session bus")?;
        Ok(Self { conn })
    }

    /// Cheap liveness check: is the name owned, and does kdeconnectd's main loop answer
    /// (`selfId`, dispatched on its main thread like every real call)? Err: the probe itself
    /// failed (bus trouble, error reply), not a verdict on kdeconnectd.
    pub async fn health(&self) -> Result<Health> {
        let t = std::time::Instant::now();
        let bus = zbus::fdo::DBusProxy::new(&self.conn)
            .await
            .context("dbus proxy")?;
        let name = zbus::names::BusName::try_from(DEST).expect("valid bus name");
        let owned = tokio::time::timeout(PROBE_TIMEOUT, bus.name_has_owner(name))
            .await
            .context("dbus NameHasOwner: no answer from the bus")?
            .context("dbus NameHasOwner")?;
        if !owned {
            debug!("kde connect: health: kdeconnectd not on the bus");
            return Ok(Health::NotRunning);
        }
        let daemon = self.proxy(ROOT.into(), "org.kde.kdeconnect.daemon").await?;
        match tokio::time::timeout(PROBE_TIMEOUT, daemon.call::<_, _, String>("selfId", &())).await
        {
            Ok(Ok(_)) => {
                trace!(
                    elapsed_ms = t.elapsed().as_millis() as u64,
                    "kde connect: health ok"
                );
                Ok(Health::Ok)
            }
            Ok(Err(e)) => Err(e).context("kdeconnectd selfId"),
            Err(_) => {
                debug!(
                    timeout_s = PROBE_TIMEOUT.as_secs(),
                    "kde connect: health: kdeconnectd on the bus but selfId unanswered"
                );
                Ok(Health::Hung)
            }
        }
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

    /// What the kdeconnectd process is stuck on, for the log before a restart: pid and per
    /// thread name, state and kernel wait channel (e.g. `kdeconnectd S futex_wait_queue`).
    pub async fn hung_details(&self) -> String {
        let pid = async {
            let bus = zbus::fdo::DBusProxy::new(&self.conn).await?;
            let name = zbus::names::BusName::try_from(DEST).expect("valid bus name");
            Ok::<u32, zbus::Error>(bus.get_connection_unix_process_id(name).await?)
        };
        let pid = match tokio::time::timeout(PROBE_TIMEOUT, pid).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => return format!("pid unknown: {e}"),
            Err(_) => return "pid unknown: no answer from the bus".into(),
        };
        let read = |p: std::path::PathBuf| {
            std::fs::read_to_string(p)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "?".into())
        };
        let mut threads = Vec::new();
        match std::fs::read_dir(format!("/proc/{pid}/task")) {
            Ok(dir) => {
                for t in dir.flatten() {
                    let state = read(t.path().join("stat"));
                    // stat: "tid (comm) S ...": state is the field after the closing paren.
                    let state = state
                        .rsplit_once(')')
                        .and_then(|(_, rest)| rest.split_whitespace().next())
                        .unwrap_or("?")
                        .to_string();
                    threads.push(format!(
                        "{} {state} {}",
                        read(t.path().join("comm")),
                        read(t.path().join("wchan"))
                    ));
                }
            }
            Err(e) => return format!("pid {pid}: threads unreadable: {e}"),
        }
        threads.sort();
        format!("pid {pid}, threads: {}", threads.join(", "))
    }

    /// Paired device ids.
    pub async fn paired(&self) -> Result<Vec<String>> {
        let daemon = self.proxy(ROOT.into(), "org.kde.kdeconnect.daemon").await?;
        let r: Result<Vec<String>> = daemon
            .call("devices", &(false, true))
            .await
            .map_err(Into::into);
        r.map_err(|e| {
            let hint = if timed_out(&e) {
                "kdeconnectd not answering (hung? the daemon restarts it)"
            } else {
                "kdeconnectd not running? (kdeconnectd / kdeconnect-indicator)"
            };
            e.context(hint)
        })
    }

    /// Reachable but unpaired devices as "name (id)": tells "phone not visible" apart from
    /// "visible, pairing missing" (e.g. after a fresh app install with a new device id).
    async fn unpaired_reachable(&self) -> Result<Vec<String>> {
        let daemon = self.proxy(ROOT.into(), "org.kde.kdeconnect.daemon").await?;
        let ids: Vec<String> = daemon
            .call("devices", &(true, false))
            .await
            .context("kdeconnectd devices")?;
        let mut out = Vec::new();
        for id in ids {
            let dev = self.device(&id, None, "org.kde.kdeconnect.device").await?;
            if dev.get_property::<bool>("isPaired").await.unwrap_or(false) {
                continue;
            }
            let name: String = dev.get_property("name").await.unwrap_or_default();
            out.push(format!("{name} ({id})"));
        }
        Ok(out)
    }

    /// The phone: `want` (device id or name) if set, else the first paired smartphone/tablet.
    pub async fn find_phone(&self, want: Option<&str>) -> Result<String> {
        let ids = self.paired().await?;
        for id in &ids {
            let dev = self.device(id, None, "org.kde.kdeconnect.device").await?;
            let name: String = dev.get_property("name").await.unwrap_or_else(|e| {
                debug!("kde connect: {id}: name unreadable: {e}");
                String::new()
            });
            let kind: String = dev.get_property("type").await.unwrap_or_else(|e| {
                debug!("kde connect: {id}: type unreadable: {e}");
                String::new()
            });
            let hit = match want {
                Some(w) => w == id || w == name,
                None => kind == "smartphone" || kind == "phone" || kind == "tablet",
            };
            trace!(%id, %name, %kind, hit, "kde connect: paired device");
            if hit {
                return Ok(id.clone());
            }
        }
        debug!(paired = ids.len(), want = ?want, "kde connect: no matching device");
        match want {
            Some(w) => bail!("no paired KDE Connect device '{w}'"),
            None => {
                let seen = match self.unpaired_reachable().await {
                    Ok(v) if v.is_empty() => {
                        "phone not visible to KDE Connect either (same network? app running?)"
                            .into()
                    }
                    Ok(v) => format!("visible but unpaired: {}", v.join(", ")),
                    Err(e) => {
                        format!("unpaired devices unknown: {e:#}")
                    }
                };
                debug!(%seen, "kde connect: no paired phone");
                bail!("no paired phone in KDE Connect: pair it from the phone app first; {seen}")
            }
        }
    }

    pub async fn phone(&self, id: &str) -> Result<Phone> {
        let dev = self.device(id, None, "org.kde.kdeconnect.device").await?;
        let reachable: bool = dev.get_property("isReachable").await?;
        let name: String = dev.get_property("name").await?;
        let (mut battery, mut charging, mut network, mut signal) = (None, false, None, None);
        if reachable {
            // Plugin objects only exist while the plugin is loaded: missing = unknown, not an error.
            // Polled every few seconds (waybar): trace only.
            match self
                .device(id, Some("battery"), "org.kde.kdeconnect.device.battery")
                .await
            {
                Ok(b) => {
                    battery = b
                        .get_property::<i32>("charge")
                        .await
                        .map_err(|e| trace!("kde connect: battery charge: {e}"))
                        .ok()
                        .filter(|c| *c >= 0);
                    charging = b
                        .get_property("isCharging")
                        .await
                        .map_err(|e| trace!("kde connect: battery isCharging: {e}"))
                        .unwrap_or(false);
                }
                Err(e) => trace!("kde connect: battery plugin not loaded: {e:#}"),
            }
            match self
                .device(
                    id,
                    Some("connectivity_report"),
                    "org.kde.kdeconnect.device.connectivity_report",
                )
                .await
            {
                Ok(c) => {
                    network = c
                        .get_property::<String>("cellularNetworkType")
                        .await
                        .map_err(|e| trace!("kde connect: cellularNetworkType: {e}"))
                        .ok()
                        .filter(|s| !s.is_empty());
                    signal = c
                        .get_property::<i32>("cellularNetworkStrength")
                        .await
                        .map_err(|e| trace!("kde connect: cellularNetworkStrength: {e}"))
                        .ok()
                        .filter(|s| *s >= 0);
                }
                Err(e) => trace!("kde connect: connectivity_report plugin not loaded: {e:#}"),
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

    pub async fn device_info(&self, id: &str) -> Result<DeviceInfo> {
        let dev = self.device(id, None, "org.kde.kdeconnect.device").await?;
        let mut errors = Vec::new();
        let mut note = |what: &str, e: zbus::Error| errors.push(format!("{what}: {e}"));
        let name: String = dev.get_property("name").await?;
        let reachable: bool = dev.get_property("isReachable").await?;
        let paired = dev
            .get_property::<bool>("isPaired")
            .await
            .unwrap_or_else(|e| {
                note("isPaired", e);
                false
            });
        let providers = dev
            .get_property::<Vec<String>>("activeProviderNames")
            .await
            .unwrap_or_else(|e| {
                note("activeProviderNames", e);
                vec![]
            });
        let connection = dev
            .get_property::<String>("connectionDescription")
            .await
            .unwrap_or_else(|e| {
                note("connectionDescription", e);
                String::new()
            });
        let mut plugins = dev
            .call::<_, _, Vec<String>>("loadedPlugins", &())
            .await
            .unwrap_or_else(|e| {
                note("loadedPlugins", e);
                vec![]
            });
        plugins.sort();
        Ok(DeviceInfo {
            id: id.into(),
            name,
            paired,
            reachable,
            providers,
            connection,
            plugins,
            errors,
        })
    }

    /// KDE Connect ping: a notification on the phone.
    pub async fn ping(&self, id: &str, message: &str) -> Result<()> {
        let p = self
            .device(id, Some("ping"), "org.kde.kdeconnect.device.ping")
            .await?;
        p.call::<_, _, ()>("sendPing", &(message,))
            .await
            .context("ping plugin sendPing")?;
        info!(device = id, "kde connect: ping sent");
        Ok(())
    }

    /// Content never logged (privacy).
    pub async fn send_clipboard(&self, id: &str) -> Result<()> {
        let p = self
            .device(id, Some("clipboard"), "org.kde.kdeconnect.device.clipboard")
            .await?;
        p.call::<_, _, ()>("sendClipboard", &()).await?;
        info!(device = id, "kde connect: clipboard sent");
        Ok(())
    }

    /// Files as file:// URLs, or http(s) links (opened on the phone). Only the count is logged.
    pub async fn share(&self, id: &str, urls: Vec<String>) -> Result<()> {
        let p = self
            .device(id, Some("share"), "org.kde.kdeconnect.device.share")
            .await?;
        let n = urls.len();
        p.call::<_, _, ()>("shareUrls", &(urls,)).await?;
        info!(device = id, files = n, "kde connect: shared");
        Ok(())
    }

    /// Mounts the phone over sftp; returns the mount point.
    pub async fn mount(&self, id: &str) -> Result<String> {
        let p = self
            .device(id, Some("sftp"), "org.kde.kdeconnect.device.sftp")
            .await?;
        let t = std::time::Instant::now();
        let ok: bool = p.call("mountAndWait", &()).await?;
        if !ok {
            let err: String = p.call("getMountError", &()).await.unwrap_or_else(|e| {
                debug!("kde connect: getMountError: {e}");
                String::new()
            });
            bail!("mount failed: {err} (allow file access in the phone app)");
        }
        let dir: String = p.call("mountPoint", &()).await?;
        info!(
            device = id,
            elapsed_ms = t.elapsed().as_millis() as u64,
            "kde connect: mounted at {dir}"
        );
        Ok(dir)
    }
}

impl Kde {
    pub async fn set_plugin(&self, id: &str, plugin: &str, on: bool) -> Result<()> {
        let dev = self.device(id, None, "org.kde.kdeconnect.device").await?;
        dev.call::<_, _, ()>("setPluginEnabled", &(plugin, on))
            .await?;
        info!(device = id, plugin, on, "kde connect: plugin set");
        Ok(())
    }

    /// Link providers as (id, enabled), e.g. ("bluetooth", true).
    pub async fn link_providers(&self) -> Result<Vec<(String, bool)>> {
        let daemon = self.proxy(ROOT.into(), "org.kde.kdeconnect.daemon").await?;
        let lines: Vec<String> = daemon.call("linkProviders", &()).await?;
        Ok(lines.iter().filter_map(|l| parse_provider(l)).collect())
    }

    pub async fn set_link_provider(&self, provider: &str, on: bool) -> Result<()> {
        let daemon = self.proxy(ROOT.into(), "org.kde.kdeconnect.daemon").await?;
        daemon
            .call::<_, _, ()>("setLinkProviderState", &(provider, on))
            .await?;
        debug!(provider, on, "kde connect: link provider set");
        Ok(())
    }
}

/// A kdeconnectd call ran into the [`CALL`] timeout (kdeconnectd possibly hung).
pub fn timed_out(e: &anyhow::Error) -> bool {
    e.chain().any(|c| match c.downcast_ref::<zbus::Error>() {
        Some(zbus::Error::InputOutput(io)) => io.kind() == std::io::ErrorKind::TimedOut,
        _ => false,
    })
}

async fn systemctl(args: &[&str]) -> Result<(bool, String)> {
    let out = tokio::time::timeout(
        SYSTEMCTL_TIMEOUT,
        tokio::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .with_context(|| {
        format!(
            "systemctl --user {}: no result in {} s",
            args.join(" "),
            SYSTEMCTL_TIMEOUT.as_secs()
        )
    })?
    .with_context(|| format!("systemctl --user {}", args.join(" ")))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok((out.status.success(), text.trim().to_string()))
}

/// Is kdeconnectd's unit active? Hung kdeconnectd outside it (bare `kdeconnectd`) can't be
/// restarted through it: starting the unit would only add a second instance.
pub async fn unit_active() -> Result<bool> {
    Ok(systemctl(&["is-active", "--quiet", UNIT]).await?.0)
}

/// Restarts kdeconnectd's unit (it then reconnects to the phone by itself).
pub async fn restart() -> Result<()> {
    let t = std::time::Instant::now();
    let (ok, out) = systemctl(&["restart", UNIT]).await?;
    if !ok {
        bail!("systemctl --user restart {UNIT}: {out}");
    }
    info!(
        unit = UNIT,
        elapsed_ms = t.elapsed().as_millis() as u64,
        "kde connect: kdeconnectd restarted"
    );
    Ok(())
}

pub fn logging_dropin_path() -> std::path::PathBuf {
    crate::config::xdg_config()
        .join(DROPIN_DIR)
        .join(DROPIN_FILE)
}

pub fn logging_dropin() -> String {
    format!(
        "# Written by `workstation-connect setup`: kdeconnectd connection/pairing lifecycle in the\n\
         # journal (uploaded with the laptop logs). kdeconnect.core only: no packet or plugin content.\n\
         [Service]\n\
         Environment=QT_LOGGING_RULES={QT_LOGGING_RULES}\n"
    )
}

/// Writes the kdeconnectd logging drop-in and reloads systemd if it changed. Returns whether it
/// changed (kdeconnectd picks it up on its next start).
pub async fn ensure_logging_dropin() -> Result<bool> {
    let path = logging_dropin_path();
    let want = logging_dropin();
    match std::fs::read_to_string(&path) {
        Ok(have) if have == want => {
            debug!(path = %path.display(), "kde connect: logging drop-in up to date");
            return Ok(false);
        }
        Ok(_) => debug!(path = %path.display(), "kde connect: logging drop-in outdated"),
        Err(e) => debug!(path = %path.display(), "kde connect: logging drop-in: {e}"),
    }
    let dir = path.parent().expect("has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(&path, want).with_context(|| format!("write {}", path.display()))?;
    info!(path = %path.display(), "kde connect: logging drop-in written");
    let (ok, out) = systemctl(&["daemon-reload"]).await?;
    if !ok {
        bail!("systemctl --user daemon-reload: {out}");
    }
    Ok(true)
}

/// Pause = KDE Connect fully off too: disables every enabled link provider and remembers which
/// ones (file in XDG_RUNTIME_DIR, survives daemon restarts); resume re-enables exactly those.
pub async fn pause_sync(on: bool) -> Result<()> {
    let state = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("workstation-connect-paused-providers");
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
            debug!("kde connect: pause: no link provider enabled, keeping the remembered list");
            return Ok(()); // already paused (or nothing on): keep the remembered list
        }
        std::fs::write(&state, enabled.join("\n"))?;
        info!("kde connect: pause: disabling link providers {enabled:?}");
        for id in &enabled {
            kde.set_link_provider(id, false).await?;
        }
    } else {
        let text = match std::fs::read_to_string(&state) {
            Ok(t) => t,
            Err(e) => {
                debug!(
                    "kde connect: resume: nothing to re-enable ({}: {e})",
                    state.display()
                );
                return Ok(());
            }
        };
        info!(
            "kde connect: resume: enabling link providers {:?}",
            text.lines().collect::<Vec<_>>()
        );
        for id in text.lines().filter(|l| !l.is_empty()) {
            kde.set_link_provider(id, true).await?;
        }
        std::fs::remove_file(&state)?;
    }
    Ok(())
}

/// Log lines for a change of the phone's KDE Connect device (all of it when `prev` is None).
pub fn describe_change(prev: Option<&DeviceInfo>, new: &DeviceInfo) -> Vec<String> {
    let Some(p) = prev.filter(|p| p.id == new.id) else {
        let mut l = vec![format!(
            "kde connect: {} ({}) paired={} reachable={} links={:?} ({}) plugins={:?}",
            new.name, new.id, new.paired, new.reachable, new.providers, new.connection, new.plugins
        )];
        l.extend(new.errors.iter().map(|e| format!("kde connect: D-Bus {e}")));
        return l;
    };
    let mut l = Vec::new();
    if p.paired != new.paired {
        l.push(format!("kde connect: {} paired={}", new.name, new.paired));
    }
    if p.reachable != new.reachable
        || p.providers != new.providers
        || p.connection != new.connection
    {
        l.push(format!(
            "kde connect: {} reachable={} links={:?} ({})",
            new.name, new.reachable, new.providers, new.connection
        ));
    }
    let added: Vec<_> = new
        .plugins
        .iter()
        .filter(|x| !p.plugins.contains(x))
        .collect();
    let removed: Vec<_> = p
        .plugins
        .iter()
        .filter(|x| !new.plugins.contains(x))
        .collect();
    if !added.is_empty() || !removed.is_empty() {
        l.push(format!(
            "kde connect: {} plugins loaded {added:?}, unloaded {removed:?}",
            new.name
        ));
    }
    l.extend(
        new.errors
            .iter()
            .filter(|e| !p.errors.contains(e))
            .map(|e| format!("kde connect: D-Bus {e}")),
    );
    l
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

    fn info() -> DeviceInfo {
        DeviceInfo {
            id: "abc".into(),
            name: "Pixel 9".into(),
            paired: true,
            reachable: true,
            providers: vec!["LanLinkProvider".into()],
            connection: "Connected via LAN".into(),
            plugins: vec!["kdeconnect_mousepad".into(), "kdeconnect_ping".into()],
            errors: vec![],
        }
    }

    #[test]
    fn describes_kde_changes() {
        let a = info();
        assert_eq!(describe_change(None, &a).len(), 1);
        assert!(describe_change(Some(&a), &a).is_empty());
        let mut b = a.clone();
        b.reachable = false;
        b.providers.clear();
        b.plugins = vec!["kdeconnect_ping".into(), "kdeconnect_clipboard".into()];
        let l = describe_change(Some(&a), &b);
        assert_eq!(l.len(), 2, "{l:?}");
        assert!(l[0].contains("reachable=false links=[]"), "{l:?}");
        assert!(
            l[1].contains("loaded [\"kdeconnect_clipboard\"], unloaded [\"kdeconnect_mousepad\"]"),
            "{l:?}"
        );
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
