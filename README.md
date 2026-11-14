# workstation-connect

Phone ↔ laptop link for a Pixel 9 and an Arch/Hyprland laptop. Status: phase 0.

## Phase 0: can the phone start tethering without root?

`spike/hotspot`: tiny Android app. Shizuku runs a helper as the shell user, which may call
`TetheringManager.startTethering` (Wi-Fi hotspot = type 0, Bluetooth tethering = type 2).
The app logs the result and how long each start took.

`spike/measure <nm-connection>`: laptop side. Brings the connection up and prints time to link,
time to working internet and download speed over that interface.

### Test

1. Phone: install Shizuku (Play Store), start it via wireless debugging.
2. Phone: install the spike APK, tap **Shizuku-toestemming**, allow.
3. Tap **Hotspot aan**, **BT-tethering aan**, **Combi**, **Status**. Then **Log kopiëren**.
4. Laptop, Wi-Fi hotspot: connect once by hand so NM saves it, then `spike/measure "<hotspot SSID>"`.
5. Laptop, Bluetooth: pair once (`bluetoothctl pair/trust <phone MAC>`), then
   `spike/measure "<phone name> Network"`.

### Build

```
cd spike/hotspot
export ANDROID_HOME=~/.local/share/android-sdk JAVA_HOME=/usr/lib/jvm/java-17-openjdk
echo "sdk.dir=$ANDROID_HOME" > local.properties
./gradlew assembleDebug --no-daemon
```
