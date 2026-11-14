# workstation-connect

Phone ↔ laptop link for a Pixel 9 and an Arch/Hyprland laptop.

- `android/`: phone app. BLE GATT server in a foreground service; turns Bluetooth tethering and the
  Wi-Fi hotspot on/off via Shizuku; reports battery, signal and tethering state.
- `laptop/`: `wsconnect`, Rust daemon + CLI + waybar module.
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

BLE protocol (both sides: `Proto.kt`, `proto.rs`), service `6f2b0001-3c1d-4e8a-9f41-7a5d2c9e0b10`:
- `…0002` control, write, encrypted: `0` off, `1` on, `2` Bluetooth tethering off
- `…0003` state, read/notify, encrypted: `[flags, battery %, signal 0-4|255, version]`,
  flags: 1 Shizuku ready, 2 BT tethered, 4 hotspot on, 8 wanted, 16 last command failed

## Setup

**Phone**
1. Install Shizuku (Play Store), start it via wireless debugging (again after every reboot).
2. Install the app, tap **Rechten**, **Shizuku**, **Batterij**, then **Start**.
3. Hotspot settings: note SSID + password.

**Laptop**
```
cargo install --path laptop
mkdir -p ~/.config/workstation-connect ~/.config/systemd/user
cp laptop/dist/config.example.toml ~/.config/workstation-connect/config.toml   # edit
cp laptop/dist/wsconnect.service ~/.config/systemd/user/

# pair once (phone: Settings → Connected devices → Pair new device)
bluetoothctl pair <phone MAC> && bluetoothctl trust <phone MAC>

# Bluetooth tethering connection
nmcli connection add type bluetooth con-name "Pixel 9 BT" bluetooth.bdaddr <phone MAC> \
  bluetooth.type panu connection.autoconnect no

# hotspot connection: turn the hotspot on by hand once (app: "Test aan"), then
nmcli device wifi connect "<SSID>" password "<password>" name "Pixel 9 hotspot"
nmcli connection modify "Pixel 9 hotspot" connection.autoconnect no

systemctl --user enable --now wsconnect
journalctl --user -fu wsconnect
```

**Waybar**
```jsonc
"custom/phone": {
  "exec": "wsconnect waybar",
  "return-type": "json",
  "on-click": "wsconnect toggle"
}
```
CSS classes: `idle`, `nophone`, `offline`, `activating`, `tethered`, `paused`, `down`.

CLI: `wsconnect status | pause | resume | toggle`.

## Build

```
# phone
cd android
export ANDROID_HOME=~/.local/share/android-sdk JAVA_HOME=/usr/lib/jvm/java-17-openjdk
echo "sdk.dir=$ANDROID_HOME" > local.properties
./gradlew assembleDebug --no-daemon

# laptop
cd laptop && cargo test && cargo build --release
```

## Phase 0 spike

`spike/hotspot`: minimal app that starts tethering via Shizuku and logs timings.
`spike/measure <nm-connection>`: time to link, time to internet, download speed.
