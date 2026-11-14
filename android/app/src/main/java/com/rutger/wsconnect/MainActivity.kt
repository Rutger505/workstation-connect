package com.rutger.wsconnect

import android.Manifest
import android.app.Activity
import android.bluetooth.BluetoothManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Typeface
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.provider.Settings
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast

/** Setup + debug screen. Real UI comes with the KDE Connect fork (phase 2). */
class MainActivity : Activity() {

    private val main = Handler(Looper.getMainLooper())
    private lateinit var status: TextView
    private lateinit var log: TextView
    private lateinit var scroll: ScrollView
    private val refresh: () -> Unit = { main.post(::render) }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(24, 96, 24, 24) }
        fun row(vararg buttons: Pair<String, () -> Unit>) = root.addView(LinearLayout(this).apply {
            buttons.forEach { (label, action) ->
                addView(Button(context).apply { text = label; isAllCaps = false; setOnClickListener { action() } },
                    LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
            }
        })

        status = TextView(this).apply { typeface = Typeface.MONOSPACE; textSize = 13f }
        root.addView(status)
        row("Rechten" to ::requestPermissions, "Shizuku" to Tether::requestPermission, "Batterij" to ::batteryExemption)
        row("Start" to ::startLink, "Stop" to ::stopLink)
        row("Test aan" to Tether::on, "Test BT uit" to Tether::btOff, "Test uit" to Tether::off)
        row("Log kopiëren" to ::copyLog, "Log wissen" to AppLog::clear)

        log = TextView(this).apply { typeface = Typeface.MONOSPACE; textSize = 11f; setTextIsSelectable(true) }
        scroll = ScrollView(this).apply { addView(log) }
        root.addView(scroll, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        setContentView(root)
    }

    override fun onResume() {
        super.onResume()
        AppLog.listen(refresh)
        Tether.listen(refresh)
        render()
    }

    override fun onPause() {
        AppLog.unlisten(refresh)
        Tether.unlisten(refresh)
        super.onPause()
    }

    private fun render() {
        val adapter = getSystemService(BluetoothManager::class.java).adapter
        fun yn(b: Boolean) = if (b) "ja" else "NEE"
        status.text = buildString {
            appendLine("${BuildConfig.VERSION_NAME}")
            appendLine("rechten:       ${yn(missingPermissions().isEmpty())}")
            appendLine("batterij-uitz: ${yn(getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(packageName))}")
            appendLine("bluetooth:     ${yn(adapter?.isEnabled == true)}  ${adapter?.name ?: ""}")
            appendLine("shizuku:       draait ${yn(Tether.shizukuRunning)}, toestemming ${yn(Tether.shizukuAllowed)}, helper ${yn(Tether.ready)}")
            appendLine("service:       ${yn(LinkService.running)} (autostart ${yn(App.enabled(this@MainActivity))})")
            append("tethering:     gewenst ${yn(Tether.wanted)}, bt ${yn(Tether.btTethered)}, hotspot ${yn(Tether.hotspotOn)}, fout ${yn(Tether.lastError)}")
        }
        log.text = AppLog.text()
        scroll.post { scroll.fullScroll(ScrollView.FOCUS_DOWN) }
    }

    private fun missingPermissions() = PERMISSIONS.filter { checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }

    private fun requestPermissions() {
        val missing = missingPermissions()
        if (missing.isEmpty()) toast("Alle rechten zijn er") else requestPermissions(missing.toTypedArray(), 1)
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) = render()

    private fun batteryExemption() {
        if (getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(packageName)) return toast("Al vrijgesteld")
        @Suppress("BatteryLife") // a foreground BLE link must survive Doze
        startActivity(Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:$packageName")))
    }

    private fun startLink() {
        if (missingPermissions().isNotEmpty()) return toast("Eerst Rechten")
        App.setEnabled(this, true)
        startForegroundService(Intent(this, LinkService::class.java))
        main.postDelayed(::render, 500)
    }

    private fun stopLink() {
        App.setEnabled(this, false)
        stopService(Intent(this, LinkService::class.java))
        main.postDelayed(::render, 500)
    }

    private fun copyLog() {
        getSystemService(ClipboardManager::class.java)
            .setPrimaryClip(ClipData.newPlainText("wsconnect log", status.text.toString() + "\n\n" + AppLog.text()))
        toast("Log gekopieerd")
    }

    private fun toast(msg: String) = Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()

    private companion object {
        val PERMISSIONS = listOf(
            Manifest.permission.BLUETOOTH_CONNECT,
            Manifest.permission.BLUETOOTH_ADVERTISE,
            Manifest.permission.POST_NOTIFICATIONS,
        )
    }
}
