package com.rutger.wsconnect.spike;

// Runs inside the Shizuku UserService (shell uid). Every call returns a human-readable result line.
interface ITetherControl {
    // type: 0 = Wi-Fi hotspot, 2 = Bluetooth tethering (TetheringManager.TETHERING_*)
    String start(int type) = 1;
    String stop(int type) = 2;
    String status() = 3;
    String whoami() = 4;
    void destroy() = 16777114; // Shizuku's reserved destroy transaction
}
