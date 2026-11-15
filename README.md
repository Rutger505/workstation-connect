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
- `…0002` control, write, encrypted: `0` off, `1` on, `2` Bluetooth tethering off
- `…0003` state, read/notify, encrypted: `[flags, battery %, signal 0-4|255, version]`,
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
kdeconnectd &               # or autostart it (Hyprland: exec-once = kdeconnectd)
cargo install --git https://github.com/Rutger505/workstation-connect wsconnect
mkdir -p ~/.config/workstation-connect ~/.config/systemd/user
cp laptop/dist/config.example.toml ~/.config/workstation-connect/config.toml   # edit
cp laptop/dist/wsconnect.service ~/.config/systemd/user/

# BlueZ >= 5.84: the daemon uses the LE bearer API (org.bluez.Bearer.LE1), which is experimental
sudo sed -i 's/^#\?Experimental = .*/Experimental = true/' /etc/bluetooth/main.conf
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
Phone battery/signal come from BLE when in reach, else from KDE Connect.
CSS classes: `idle`, `nophone`, `offline`, `activating`, `tethered`, `paused`, `down`.

CLI: `wsconnect status | pause | resume | toggle | menu | ring | clip | send [files] | browse | screen | setup`.
- `pause`: auto hotspot off and KDE Connect fully off (all link providers); `resume` restores them.
- `menu`: walker/fuzzel/wofi/rofi (dmenu mode), first found; override with `menu = [...]` in config.
- `screen`: scrcpy over wireless debugging (on anyway for Shizuku). Uses an existing adb connection,
  else finds the phone via mDNS (same network). First time: `adb pair <ip:port> <code>`.
- `browse`: mounts the phone over sftp (kdeconnectd) and opens it with xdg-open.

## Build

```
cd laptop && cargo test && cargo build --release
```
Phone: see `WSCONNECT.md` in the app repo.

## Phase 0 spike

`spike/hotspot`: minimal app that starts tethering via Shizuku and logs timings.
`spike/measure <nm-connection>`: time to link, time to internet, download speed.
