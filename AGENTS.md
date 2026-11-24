# workstation-connect

Phone ↔ laptop link: Pixel 9 ↔ Arch/Hyprland laptop. Auto hotspot when the laptop is offline, KDE Connect (notifications, clipboard, files, trackpad) under Rutger's own UI.
README = how it works, **link protocol** (single source, the app repo points here), setup, waybar, log upload, selftest, socket, CLI.
Phone app: Rutger505/workstation-connect-android (KDE Connect fork, its own AGENTS.md).

## Rules

- **Git**: commit + push straight to `main`, no branches/PRs (except big renames). Public repo, MIT.
- `laptop/`: Rust `workstation-connect` (daemon, waybar, menu, clip/send/browse/screen, setup, update, selftest). bluer 0.17 + zbus 5 + nmcli + ureq. `spike/`: phase 0 experiments.
- Check: `cd laptop && cargo test && cargo clippy` (clean), `cargo build --release`.
- **Logging**: every change ships with `tracing` lines (setup `logging.rs`, config `log_level`/RUST_LOG): state changes `info`, decisions/skips/timings `debug`, failures `warn`/`error` with context (`{e:#}`), no silently swallowed errors, changes not repeats in loops. Never secrets or content (clipboard, notifications, keys).
- **Protocol** change: README section + `proto.rs` + app's `Proto.kt` together; bump `VERSION` in `Proto.kt` (phone reports it). Cmds 2/4 and flag 2 retired: never reuse.
- `build.rs` embeds the git commit; the update check only counts `laptop/` changes.
- KDE Connect via D-Bus in `laptop/src/kde.rs`; API = kdeconnect-kde `core/*.h`, `plugins/*plugin.h` (Q_SCRIPTABLE/Q_PROPERTY).
- Rename `wsconnect` → `workstation-connect` is TRANSITIONAL (shim `src/bin/wsconnect.rs`, `src/migrate.rs`): remove once the laptop runs `workstation-connect.service` (README CLI section).
- Laptop install for real: dev-env optional group `workstation-connect` (`install/optional/workstation-connect.sh`). Pairing, nmcli hotspot connection, `workstation-connect setup` stay manual.

## Design decisions (Rutger)

- Own UI on everything; KDE Connect (headless kdeconnectd / the fork) only as engine. Reused as is: scrcpy (phone screen), Firefox Sync (tabs).
- Trigger: no known Wi-Fi in range OR Wi-Fi without internet (captive portal). Off on known Wi-Fi with internet, suspend, pause.
- Must work without a shared LAN (eduroam client isolation, train without Wi-Fi). Bluetooth = control channel only, never data.
- **Hotspot only** (2026-11-23): BT PAN on the laptop's MT7921 unusable (phone-gateway RTT 2-60 s, Wi-Fi off 2-6 s, kernel NETDEV WATCHDOG tx timeouts on the PAN iface). Want BT tethering back: git history (c150355 era) + newer kernel/firmware or a USB BT dongle.
- Switch networks exactly once, never flap: each saved Wi-Fi one try per session, `ignore_ssids` for train Wi-Fi. Open connections (Discord, websockets) drop + reconnect: accepted.
- Out of scope: typing via phone (scrcpy does it), answering calls, ring phone/laptop, own laptop notification popups.

## Pitfalls

- **Classic BT RFCOMM**, not BLE (v4 and older were BLE GATT; phone LOW_POWER adverts missed by MT7921 after resume). Laptop pages the bonded phone; phone only listens.
- Own SDP over L2CAP PSM 1 (`sdp.rs`) + direct RFCOMM connect, security Medium. Never BlueZ ConnectProfile: browses once per bluetoothd run, then `NotAvailable` forever.
- bluer 0.17 `connect` resolves while still paging (first send `ENOTCONN` <1 ms): keep `btsock::settled` after every bluer connect.
- Every bluetoothd D-Bus call needs a timeout (`phone.rs` `CALL`): after S4 resume bluetoothd can stop answering; bluer then waits 120 s, zbus forever.
- bluetoothd 5.87 segfaulted (SEGV_ACCERR) repeatedly while the daemon ran (2026-11-15); trigger unknown, not reported upstream.
- Never hold a tokio `watch` `borrow()` guard across `send`/`send_replace` on the same channel: deadlock (00509d3).
- Never `cargo install --git` with a kept `--target-dir`: git sources aren't content-fingerprinted, it reinstalls the old build. `update` clones + `cargo install --path`.
- `NetChanged` (cmd 3) exists because a dead TCP KDE Connect link stays "connected" ~15 min after the laptop leaves a Wi-Fi.
- udev renames `bnep0` (e.g. `enp5s0f3u4`): never match a PAN iface by name.
- Captive-portal Wi-Fi is downed first, else it keeps the default route.
- Hotspot connection is set `802-11-wireless.hidden yes` by the daemon: NM probes the SSID instead of waiting for a scan.
- Trackpad on Hyprland: xdph has no RemoteDesktop portal → luminous for RemoteDesktop only, whitelist only `org.kde.kdeconnect.daemon` (never `remote_permission_check = false`); kdeconnectd must run in `app-org.kde.kdeconnect.daemon@autostart.service` (D-Bus override), never bare/exec-once, else empty app ID and a hanging session. Setup in README.

## Hardware status

- Laptop: Acer A715-42G, MT7921 (Wi-Fi + BT). Also tested on Rutger's desktop (Omarchy, Realtek USB BT).
- Protocol v5 RFCOMM link: untested on hardware since the `btsock::settled` fix (e166f98). Auto hotspot flow untested since BT tethering was dropped.
