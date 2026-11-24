# workstation-connect

Phone ↔ laptop link for a Pixel 9 and an Arch/Hyprland laptop.

- Phone app: [Rutger505/workstation-connect-android](https://github.com/Rutger505/workstation-connect-android),
  a KDE Connect Android fork (GPL) with its own home screen, auto hotspot (Bluetooth RFCOMM server +
  Shizuku tethering) and a trackpad with volume keys as mouse buttons.
- `laptop/`: `workstation-connect`, Rust daemon (NetworkManager watch, Bluetooth trigger) + CLI + waybar module +
  action menu. Uses headless `kdeconnectd` over D-Bus as the engine for notifications, clipboard,
  files and pairing.
- `spike/`: phase 0 experiments (tethering via Shizuku, timing script).

## How it works

1. Laptop has no internet (no Wi-Fi, or Wi-Fi with captive portal/no internet). Wi-Fi scan: no saved
   network in range → ask the phone at once; else NetworkManager gets `offline_secs` (15 s).
2. Daemon sends "on" over Bluetooth; the phone starts its Wi-Fi hotspot. The daemon waits (≤10 s) for the
   phone's hotspot flag and joins `hotspot_connection` (rescans + retries, ~45 s total). No hotspot:
   phone off, 60 s cooldown, try again. Bluetooth tethering is gone (BT PAN on the MT7921 had 2-60 s
   RTT): the hotspot is the only way online, Bluetooth only carries on/off and state.
3. Hotspot connection drops: phone off, back to step 1.
4. Every 60 s the laptop checks for a saved Wi-Fi in range. Each one gets one try per session; if it
   has internet the phone is turned off, otherwise back to the hotspot.
5. Suspend or pause (waybar click): phone off. Laptop gone for 3 min (Bluetooth link lost): the phone turns
   itself off.

Protocol v5 (both sides: `Proto.kt` in the app, `proto.rs`): classic Bluetooth (BR/EDR) RFCOMM,
service UUID `6f2b0010-3c1d-4e8a-9f41-7a5d2c9e0b10`, SDP name "Workstation Connect", channel via SDP
(laptop does its own SDP lookup, cached until a connect fails; not BlueZ ConnectProfile, which
browses a device once per bluetoothd run and then misses a service added later).
The phone listens (secure, bonded devices only); the laptop pages the bonded phone and keeps the
link up while awake, reconnecting when it drops. Several laptops may be connected at once.
- Framing, both directions: `[len: u8][payload: len bytes]`; `len` 0 = keepalive, ignored.
- Laptop → phone, 1 byte command: `0` off, `1` on (hotspot), `3` laptop's Wi-Fi changed/dropped:
  drop KDE Connect LAN links to it (sent on every change away from a Wi-Fi connection; else the dead
  TCP link stays "connected" ~15 min), `5` ping (phone answers with a state frame). `2` and `4`
  retired, never reuse.
- Phone → laptop, state (>= 4 bytes, extra ignored): `[flags, battery %, signal 0-4|255, version]`
  (version 5), flags: 1 Shizuku ready, 4 hotspot on, 8 wanted, 16 last command failed (2 retired).
  Sent right after accept and on every change.

## Setup

**Phone**
1. Install Shizuku (Play Store), start it via wireless debugging (again after every reboot).
2. Install the app ([APK](https://cdn.rutgerpronk.com/artifacts/workstation-connect/workstation-connect.apk)),
   work through **Nog instellen**, pair the laptop, turn on **Internet delen → Automatisch**.
3. Hotspot settings: note SSID + password.

**Laptop**
```
sudo pacman -S kdeconnect   # engine; also: scrcpy android-tools zenity (optional actions)
laptop/kdeconnect/build     # patched kdeconnect, see "kdeconnect patch"; again after every kdeconnect update
systemctl --user start app-org.kde.kdeconnect.daemon@autostart.service   # not a bare kdeconnectd, see trackpad
cargo install --git https://github.com/Rutger505/workstation-connect workstation-connect
mkdir -p ~/.config/workstation-connect ~/.config/systemd/user
cp laptop/dist/config.example.toml ~/.config/workstation-connect/config.toml   # edit
cp laptop/dist/workstation-connect.service ~/.config/systemd/user/

# pair the phone once via normal Bluetooth (desktop Bluetooth menu, or phone: Settings → Connected
# devices → Pair new device; bluetoothctl pair <phone MAC> works too). No BlueZ config changes needed.

# hotspot connection (config: hotspot_connection, required): turn the hotspot on by hand once
# (app: "Nu aanzetten"), then
nmcli device wifi connect "<SSID>" password "<password>" name "Pixel 9 hotspot"
nmcli connection modify "Pixel 9 hotspot" connection.autoconnect no 802-11-wireless.hidden yes  # daemon sets hidden itself too: probes the SSID, no waiting for a scan
# Old configs: `bt_connection` and `hotspot` are obsolete, ignored (daemon logs a warning); remove
# them, and the old Bluetooth PAN connection: nmcli connection delete "Pixel 9 BT"

# trackpad: kdeconnectd injects input via the RemoteDesktop portal, which xdg-desktop-portal-hyprland
# lacks. Route it to luminous and whitelist only KDE Connect, so it skips luminous' permission
# dialog. First use asks once which screen the cursor lives on.
sudo pacman -S xdg-desktop-portal-luminous   # chaotic-aur
mkdir -p ~/.config/xdg-desktop-portal ~/.config/xdg-desktop-portal-luminous ~/.local/share/dbus-1/services
printf '[preferred]\ndefault=hyprland;gtk\norg.freedesktop.impl.portal.RemoteDesktop=luminous\n' \
  > ~/.config/xdg-desktop-portal/hyprland-portals.conf
echo '{"remote": ["org.kde.kdeconnect.daemon"]}' > ~/.config/xdg-desktop-portal-luminous/whitelist.json
# start kdeconnectd in its app unit so the portal knows its app ID, also when D-Bus activates it
printf '[D-BUS Service]\nName=org.kde.kdeconnect\nExec=/usr/bin/kdeconnectd\nSystemdService=app-org.kde.kdeconnect.daemon@autostart.service\n' \
  > ~/.local/share/dbus-1/services/org.kde.kdeconnect.service

workstation-connect setup   # once, after pairing in the app: laptop notifications → phone, Bluetooth link, share_dir,
                            # kdeconnectd logging drop-in (restarts kdeconnectd when it changed)
systemctl --user enable --now workstation-connect
journalctl --user -fu workstation-connect
```

**Waybar**
```jsonc
"custom/phone": {
  "exec": "workstation-connect waybar",
  "return-type": "json",
  "on-click": "workstation-connect toggle",
  "on-click-right": "workstation-connect menu"
}
```
Phone battery/signal come from the Bluetooth link when in reach, else from KDE Connect.
CSS classes: `idle`, `nophone`, `offline`, `activating` (also during a selftest), `tethered`, `paused`, `down`.
Outdated install (GitHub main has newer `laptop/` commits): daemon checks every 5 min, on every
network reconnect and when the menu opens, via `git ls-remote` (no API quota); the GitHub compare API only for a main sha it
hasn't seen yet (60 requests/h per IP). Text gets `󰚰`, tooltip shows `installed → latest`, class
becomes `["<class>", "outdated"]`.
- `auto_update = true` (default): installs at once, no click: `workstation-connect update` in the transient
  user unit `workstation-connect-update` (`systemd-run --user`, outside the daemon's cgroup, since the update
  restarts the daemon). Class adds `updating` meanwhile. Failed (daemon still alive afterwards):
  class adds `update-failed`, that commit is retried after 30 min (a newer commit at once; state in
  `~/.local/state/workstation-connect/update.failed`), menu gets **Update workstation-connect**. Log:
  `journalctl --user -u workstation-connect-update`.
- `auto_update = false`: only the marker; menu gets **Update workstation-connect**.
kdeconnectd hung (watchdog, below): text gets `󰄢`, class adds `kde-hung`, tooltip + menu info line
say so.
Phone not paired over classic Bluetooth (or bond lost): text gets `󰀦`, class `not-paired`; pair it
in the Bluetooth menu. Phone paged fine but its RFCOMM service didn't answer 3 tries in a row (app
not running / no Bluetooth permission): `󰀦`, class `service-missing`.
Connect sequence in the journal (`phone link: …`): connecting → SDP: RFCOMM channel N in X ms
(pages the phone; skipped with a cached channel) → RFCOMM connected in X ms → `phone connected:
first state X ms after RFCOMM connect`. Status says connected only after the
first state frame. Idle link: ping every 60 s (15 s to answer); after resume a ping with 5 s to
answer, else reconnect (every 1 s for 20 s, then 2/5/10/30/60 s backoff).

**Log upload** (private S3 bucket `wsconnect-logs` on `https://cdn.rutgerpronk.com`, SeaweedFS,
path-style, SigV4, region us-east-1): `workstation-connect logs-key <access> <secret>` writes the write-only
key to `~/.config/workstation-connect/logs.env` (mode 600, never in the repo; lines
`WSCONNECT_LOGS_ACCESS_KEY=…`, `WSCONNECT_LOGS_SECRET_KEY=…`), used from the next upload on, no
restart. No file: upload off (one info line).
- Object: `laptop/<hostname>/<YYYY-MM-DD>/<HHMMSS>-<reason>.log` (UTC), text/plain. Header (host,
  version, kernel, reason, time), then the user journal of `workstation-connect`, `workstation-connect-update` and
  `app-org.kde.kdeconnect.daemon@*` since the last successful upload (cursor in
  `~/.local/state/workstation-connect/upload.cursor`; first time: last hour), plus best effort the system
  journal of `bluetooth.service` and Bluetooth kernel lines (needs journal read access). Newest
  ~1 MB.
- Reasons: `kde-restart` (kdeconnectd restarted or restart failed: watchdog or Bluetooth adapter back, below), `start` (daemon start, so also after an update), `link-ok` / `link-fail` (phone link
  connected / 3 connects failed in a row outside the 20 s after resume), `hotspot-ok` /
  `hotspot-fail` (hotspot join outcome), `update` (auto-update failed), `periodic` (every 10 min
  while online, only with new lines), `selftest`. Triggers within 5 s become one upload.
- Failed upload: cursor stays, reason kept, retried at the next trigger or network reconnect.
- The daemon logs KDE Connect changes (reachable, link types, loaded plugins, D-Bus errors) and its
  own KDE Connect actions, so input/notification/clipboard problems show up there too.
- kdeconnectd itself logs almost nothing by default. `setup` writes
  `~/.config/systemd/user/app-org.kde.kdeconnect.daemon@.service.d/workstation-connect-logging.conf`
  (`Environment=QT_LOGGING_RULES=kdeconnect.core.debug=true`): discovery, link up/down, pairing,
  plugin loading. No `kdeconnect.plugin.*` (clipboard/notification content). Applies from the next
  kdeconnectd start; Qt debug lines are journal priority 7, the upload takes all priorities.

**kdeconnectd watchdog**: kdeconnectd can hang with its name still on D-Bus (main thread stuck,
every call times out, pairing silently fails). Every kdeconnectd call has a 30 s timeout. The daemon
probes it every 2 min (`selfId`, 5 s timeout; sooner, ≥30 s apart, when a KDE Connect call timed
out), also while paused. Not on the bus = fine (D-Bus activation starts it). 2 hung probes in a row:
logs its threads' kernel wait channels, `systemctl --user restart app-org.kde.kdeconnect.daemon@autostart.service`,
log upload; at most one restart per 10 min (skip logged once). Running outside that unit: no
restart, error in the status. Status `kde_health`: `state` = `unknown | ok | not_running | hung`,
`last_restart` (unix s), `restart_error`.

**Bluetooth adapter back**: kdeconnectd's Bluetooth backend looks for an adapter only at start
and resume, never again (`No local bluetooth adapter found`). On this laptop btusb is unloaded on
every suspend (`/usr/lib/systemd/system-sleep/btusb-workaround`, MT7921) and hci0 needs ~2.5 s
after resume, so kdeconnectd lost Bluetooth after every suspend. The daemon watches bluez (adapter
removed/added, Powered) and when the adapter comes back after a gap: 3 s settle, then restarts the
kdeconnectd unit (same status fields + `kde-restart` upload). Skipped when its unit isn't active
or its `bluetooth` link provider is off (pause); at most one per 30 s.

**Selftest**: `workstation-connect selftest [--no-hotspot] [--no-send]` prints PASS/FAIL/SKIP per step with
timings (exit code 1 on any FAIL):
- a. versions (CLI, daemon, phone app protocol), daemon reachable, up to date, config,
  `hotspot_connection` in NetworkManager, `logs.env`.
- b. phone link (in the daemon): connects now if needed (timed), ping round trip over RFCOMM, phone
  state (battery, signal, Shizuku, hotspot flags).
- c. hotspot (in the daemon, auto logic on hold, mode `selftest`, waybar class `activating`; ≤60 s +
  restore): phone on → hotspot flag → join `hotspot_connection` → GET
  `http://ping.archlinux.org/nm-check.txt` → phone off → previous Wi-Fi back (also on error/timeout).
  Already on the hotspot: left on.
- d. KDE Connect: kdeconnectd answers (hung: FAIL, rest skipped), logging drop-in, kdeconnectd on D-Bus, phone device, paired, reachable + link types, key plugins
  (mousepad, remotekeyboard, remotecontrol, notifications, sendnotifications, clipboard, ping),
  trackpad portal config, KDE Connect ping to the phone. D-Bus errors verbatim.
- Then: report + logs uploaded (reason `selftest`), report handed to Rocky with
  `someclaw-thread send new -` (dev-env); prints the thread URL. Fails: says so, report stays in
  the bucket.

Daemon socket `$XDG_RUNTIME_DIR/workstation-connect.sock`, one request line, one JSON answer line:
`status | pause | resume | toggle`, `check` (update check now, answers with the status once it's
done; at once while an auto-update runs), `watch` (status line on every change), or JSON
`{"cmd": "selftest_link"}`, `{"cmd": "selftest_hotspot"}` (→ `{"steps": [{name, res, ms, detail}]}`),
`{"cmd": "upload", "reason": "<reason>", "report": "…"}` (→ `{"key": …}`, null when upload off);
errors `{"error": "…"}`. Status has `version` (the running daemon's, `v<n>`) and `commit` (its full build commit),
`update` (only when outdated: `installed`, `latest`, `state` = `available | updating | failed`) and
`update_check` (last finished check: `at` unix seconds, `outcome` = `up_to_date | outdated | failed`,
`latest` short commit, `error`; null before the first check or with an unknown build commit),
`kde_health` (kdeconnectd watchdog, see above).

CLI: `workstation-connect status | pause | resume | toggle | menu | clip | send [files] | browse | screen | setup | update | logs-key | selftest`.
- `pause`: auto hotspot off and KDE Connect fully off (all link providers); `resume` restores them.
- `menu`: walker/fuzzel/wofi/rofi (dmenu mode), first found; override with `menu = [...]` in config.
  First line: installed version + update state from a fresh check (socket `check`, waits at most
  3 s, usually under 1 s: `git ls-remote`, compare API only for a new main commit), e.g.
  `workstation-connect v12 · up to date` / `· newer version on GitHub (abc1234)` / `· update check failed`
  / `· daemon not running, update state unknown`. Picking it does nothing.
- `screen`: scrcpy over wireless debugging (on anyway for Shizuku). Uses an existing adb connection,
  else finds the phone via mDNS (same network). First time: `adb pair <ip:port> <code>`.
- `browse`: mounts the phone over sftp (kdeconnectd) and opens it with xdg-open.
- `update`: clones/fetches main into `~/.cache/workstation-connect/src`, `cargo install --path … --force --locked --target-dir ~/.cache/workstation-connect/target` (kept build cache: only changed crates recompile), checks the installed `--build-commit` (hidden flag) matches the fetched commit, then restarts the daemon if active. One at a time (lock `~/.cache/workstation-connect/update.lock`). Menu runs it
  via `xdg-terminal-exec`, else `$TERMINAL -e`. `--version` shows `v<n>`: first-parent commits
  touching `laptop/` since the base in `laptop/build.rs` (v1 = first one after it). Without git
  history (shallow clone): `unknown`, and an unknown commit means no update check. Selftest and
  log uploads add the short commit: `v3 (abc1234)`. Updaters before v1 need the commit in
  `--version` (TRANSITIONAL: printed when the parent is `workstation-connect update`).
- Rename `wsconnect` → `workstation-connect` (TRANSITIONAL): the package also installs a `wsconnect`
  shim (`laptop/src/bin/wsconnect.rs`) that execs `workstation-connect`, so old updaters and
  `wsconnect.service` still work. At daemon start `laptop/src/migrate.rs` moves
  `~/.local/state/wsconnect`, drops `~/.cache/wsconnect`, installs + enables
  `workstation-connect.service`, removes `wsconnect.service` (handing over when running in it),
  switches the omarchy bar entry `rutger.wsconnect` → `rutger.workstation-connect` and then deletes
  the shim. Remove shim, migration and the old unit names once the laptop runs
  `workstation-connect.service`.

## Build

```
cd laptop && cargo test && cargo build --release
```
Phone: see `WSCONNECT.md` in the app repo.

## Phase 0 spike

`spike/hotspot`: minimal app that starts tethering via Shizuku and logs timings.
`spike/measure <nm-connection>`: time to link, time to internet, download speed.

**kdeconnect patch** (`laptop/kdeconnect/`): stock kdeconnectd runs a Bluetooth inquiry + SDP
paging every 30 s even while the phone is linked over BT; on the MT7921 that steals radio time
from the link (trackpad p90 ~0.5 s late). The patch skips discovery while any BT peer is linked
(fine for one phone; a second BT peer isn't found meanwhile). `build` rebuilds Arch's current
kdeconnect package with it (pkgrel `N.1`, Packager `workstation-connect …`), so Arch's next
kdeconnect release replaces it unpatched: the daemon warns at start and `selftest` fails
("kdeconnect BT patch") until `build` runs again. `DRY_RUN=1`: build to /tmp only.
