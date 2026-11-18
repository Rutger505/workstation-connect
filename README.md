# workstation-connect

Phone ↔ laptop link for a Pixel 9 and an Arch/Hyprland laptop.

- Phone app: [Rutger505/workstation-connect-android](https://github.com/Rutger505/workstation-connect-android),
  a KDE Connect Android fork (GPL) with its own home screen, auto hotspot (BLE GATT server +
  Shizuku tethering) and a trackpad with volume keys as mouse buttons.
- `laptop/`: `wsconnect`, Rust daemon (NetworkManager watch, BLE trigger) + CLI + waybar module +
  action menu. Uses headless `kdeconnectd` over D-Bus as the engine for notifications, clipboard,
  files and pairing.
- `spike/`: phase 0 experiments (tethering via Shizuku, timing script).

## How it works

1. Laptop has no internet for 10 s (no Wi-Fi, or Wi-Fi with captive portal/no internet).
2. Daemon sends "on" over BLE. Phone starts Bluetooth tethering **and** the hotspot.
3. Laptop joins Bluetooth tethering first: internet within seconds, but slow (~1-2 Mbps).
4. As soon as the hotspot connection is up, traffic moves there and Bluetooth tethering goes off.
   Open connections (Discord, Spotify, websockets) reconnect once.
5. Every 60 s the laptop checks for a saved Wi-Fi in range. Each one gets one try per session; if it
   has internet the phone is turned off, otherwise back to the hotspot.
6. Suspend or pause (waybar click): phone off. Laptop gone for 3 min (BLE lost): the phone turns
   itself off.

BLE protocol (both sides: `Proto.kt` in the app, `proto.rs`), service `6f2b0001-3c1d-4e8a-9f41-7a5d2c9e0b10`:
- `…0002` control, write, encrypted: `0` off, `1` on, `2` Bluetooth tethering off, `3` laptop's
  Wi-Fi changed/dropped: drop KDE Connect LAN links to it (sent on every change away from a Wi-Fi
  connection, only to version >= 2; else the dead TCP link stays "connected" ~15 min)
- `…0003` state, read/notify, encrypted: `[flags, battery %, signal 0-4|255, version]` (version 2),
  flags: 1 Shizuku ready, 2 BT tethered, 4 hotspot on, 8 wanted, 16 last command failed

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

# BlueZ >= 5.84: the daemon uses the LE bearer API (org.bluez.Bearer.LE1), which is experimental
sudo sed -i '/^\[General\]/,/^\[/ s/^#\?Experimental = .*/Experimental = true/' /etc/bluetooth/main.conf
# no GATT cache: a stale one hid our service on a bonded phone for a day (BlueZ missed Service Changed)
sudo sed -i '/^\[GATT\]/,/^\[/ s/^#\?Cache = .*/Cache = no/' /etc/bluetooth/main.conf
sudo systemctl restart bluetooth

# pair once (phone: Settings → Connected devices → Pair new device)
bluetoothctl pair <phone MAC> && bluetoothctl trust <phone MAC>
# That bond is BR/EDR (tethering). The BLE link needs an LE bond too: the daemon pairs over LE by
# itself on first start (journal shows a code, accept it on the phone).

# Bluetooth tethering connection
nmcli connection add type bluetooth con-name "Pixel 9 BT" bluetooth.bdaddr <phone MAC> \
  bluetooth.type panu connection.autoconnect no

# hotspot connection: turn the hotspot on by hand once (app: "Nu aanzetten"), then
nmcli device wifi connect "<SSID>" password "<password>" name "Pixel 9 hotspot"
nmcli connection modify "Pixel 9 hotspot" connection.autoconnect no

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

wsconnect setup   # once, after pairing in the app: laptop notifications → phone, Bluetooth link, share_dir;
                  # fixes /etc/bluetooth/main.conf with sudo (Experimental = true, [GATT] Cache = no)
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
Phone battery/signal come from BLE when in reach, else from KDE Connect.
CSS classes: `idle`, `nophone`, `offline`, `activating`, `tethered`, `paused`, `down`.
Outdated install (GitHub main has newer `laptop/` commits; daemon checks every 15 min and on every network reconnect): text gets `󰚰`,
tooltip shows `installed → latest`, class becomes `["<class>", "outdated"]`, menu gets
**Update wsconnect**.
Phone in reach over LE but our GATT service missing 3 tries in a row (app's service stopped or stale
BlueZ cache): text gets `󰀦`, class gets `service-missing`. `wsconnect setup` fixes main.conf.

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
