# workstation-connect

Phone ↔ laptop link for a Pixel 9 and an Arch/Hyprland laptop.

- Phone app: [Rutger505/workstation-connect-android](https://github.com/Rutger505/workstation-connect-android),
  a KDE Connect Android fork (GPL) with its own home screen, auto hotspot (Bluetooth RFCOMM server +
  Shizuku tethering) and a trackpad with volume keys as mouse buttons.
- `laptop/`: `wsconnect`, Rust daemon (NetworkManager watch, Bluetooth trigger) + CLI + waybar module +
  action menu. Uses headless `kdeconnectd` over D-Bus as the engine for notifications, clipboard,
  files and pairing.
- `spike/`: phase 0 experiments (tethering via Shizuku, timing script).

## How it works

1. Laptop has no internet (no Wi-Fi, or Wi-Fi with captive portal/no internet). Wi-Fi scan: no saved
   network in range → ask the phone at once; else NetworkManager gets `offline_secs` (15 s).
2. Daemon sends "on" over Bluetooth; the phone starts its Wi-Fi hotspot. The daemon waits (≤20 s) for the
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
systemctl --user start app-org.kde.kdeconnect.daemon@autostart.service   # not a bare kdeconnectd, see trackpad
cargo install --git https://github.com/Rutger505/workstation-connect wsconnect
mkdir -p ~/.config/workstation-connect ~/.config/systemd/user
cp laptop/dist/config.example.toml ~/.config/workstation-connect/config.toml   # edit
cp laptop/dist/wsconnect.service ~/.config/systemd/user/

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

wsconnect setup   # once, after pairing in the app: laptop notifications → phone, Bluetooth link, share_dir
systemctl --user enable --now wsconnect
journalctl --user -fu wsconnect
```

**Waybar**
```jsonc
"custom/phone": {
  "exec": "wsconnect waybar",
  "return-type": "json",
  "on-click": "wsconnect toggle",
  "on-click-right": "wsconnect menu"
}
```
Phone battery/signal come from the Bluetooth link when in reach, else from KDE Connect.
CSS classes: `idle`, `nophone`, `offline`, `activating`, `tethered`, `paused`, `down`.
Outdated install (GitHub main has newer `laptop/` commits; daemon checks every 15 min and on every network reconnect): text gets `󰚰`,
tooltip shows `installed → latest`, class becomes `["<class>", "outdated"]`, menu gets
**Update wsconnect**.
Phone not paired over classic Bluetooth (or bond lost): text gets `󰀦`, class `not-paired`; pair it
in the Bluetooth menu. Phone paged fine but its RFCOMM service didn't answer 3 tries in a row (app
not running / no Bluetooth permission): `󰀦`, class `service-missing`.
Connect sequence in the journal (`phone link: …`): connecting → SDP: RFCOMM channel N in X ms
(pages the phone; skipped with a cached channel) → RFCOMM connected in X ms → `phone connected:
first state X ms after RFCOMM connect`. Status says connected only after the
first state frame. Idle link: ping every 60 s (15 s to answer); after resume a ping with 5 s to
answer, else reconnect (every 1 s for 20 s, then 2/5/10/30/60 s backoff).

CLI: `wsconnect status | pause | resume | toggle | menu | clip | send [files] | browse | screen | setup | update`.
- `pause`: auto hotspot off and KDE Connect fully off (all link providers); `resume` restores them.
- `menu`: walker/fuzzel/wofi/rofi (dmenu mode), first found; override with `menu = [...]` in config.
- `screen`: scrcpy over wireless debugging (on anyway for Shizuku). Uses an existing adb connection,
  else finds the phone via mDNS (same network). First time: `adb pair <ip:port> <code>`.
- `browse`: mounts the phone over sftp (kdeconnectd) and opens it with xdg-open.
- `update`: clones/fetches main into `~/.cache/wsconnect/src`, `cargo install --path … --force --locked --target-dir ~/.cache/wsconnect/target` (kept build cache: only changed crates recompile), checks the installed `--version` matches the fetched commit, then restarts the daemon if active. Menu runs it
  via `xdg-terminal-exec`, else `$TERMINAL -e`. `--version` shows the build commit (`unknown`
  without git: no update check).

## Build

```
cd laptop && cargo test && cargo build --release
```
Phone: see `WSCONNECT.md` in the app repo.

## Phase 0 spike

`spike/hotspot`: minimal app that starts tethering via Shizuku and logs timings.
`spike/measure <nm-connection>`: time to link, time to internet, download speed.
