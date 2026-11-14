package com.rutger.wsconnect.spike

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.ComponentName
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.graphics.Typeface
import android.os.Bundle
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import rikka.shizuku.Shizuku
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.concurrent.Executors

/** Phase 0 spike: can Shizuku (shell uid) start Wi-Fi hotspot / Bluetooth tethering on this phone, and how fast? */
class MainActivity : Activity() {

    private val main = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor()
    private lateinit var log: TextView
    private lateinit var scroll: ScrollView
    @Volatile private var control: ITetherControl? = null

    private val args = Shizuku.UserServiceArgs(ComponentName(BuildConfig.APPLICATION_ID, TetherControlService::class.java.name))
        .daemon(false)
        .processNameSuffix("tether")
        .debuggable(BuildConfig.DEBUG)
        .version(BuildConfig.VERSION_CODE)

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName?, binder: IBinder?) {
            if (binder == null || !binder.pingBinder()) { append("service: lege binder"); return }
            control = ITetherControl.Stub.asInterface(binder)
            run("whoami") { it.whoami() }
        }
        override fun onServiceDisconnected(name: ComponentName?) {
            control = null
            append("service: verbinding weg")
        }
    }

    private val permissionListener = Shizuku.OnRequestPermissionResultListener { _, result ->
        append("Shizuku-toestemming: ${if (result == PackageManager.PERMISSION_GRANTED) "gegeven" else "geweigerd"}")
        if (result == PackageManager.PERMISSION_GRANTED) bind()
    }

    private val binderListener = Shizuku.OnBinderReceivedListener { append("Shizuku draait (v${Shizuku.getVersion()}, uid ${Shizuku.getUid()})"); bindIfAllowed() }
    private val deadListener = Shizuku.OnBinderDeadListener { control = null; append("Shizuku gestopt") }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(24, 96, 24, 24) }
        fun row(vararg buttons: Pair<String, () -> Unit>) = root.addView(LinearLayout(this).apply {
            buttons.forEach { (label, action) ->
                addView(Button(context).apply { text = label; isAllCaps = false; setOnClickListener { action() } },
                    LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
            }
        })

        row("Shizuku-toestemming" to ::requestPermission, "Status" to { run("status") { it.status() } })
        row("Hotspot aan" to { run("hotspot aan") { it.start(WIFI) } }, "Hotspot uit" to { run("hotspot uit") { it.stop(WIFI) } })
        row("BT-tethering aan" to { run("bt aan") { it.start(BT) } }, "BT-tethering uit" to { run("bt uit") { it.stop(BT) } })
        row("Combi: BT → hotspot" to ::combo, "Alles uit" to { run("alles uit") { it.stop(BT) + "\n" + it.stop(WIFI) } })
        row("Log kopiëren" to ::copyLog, "Log wissen" to { log.text = "" })

        log = TextView(this).apply { typeface = Typeface.MONOSPACE; textSize = 11f; setTextIsSelectable(true) }
        scroll = ScrollView(this).apply { addView(log) }
        root.addView(scroll, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        setContentView(root)

        append("${BuildConfig.APPLICATION_ID} ${BuildConfig.VERSION_NAME}")
        Shizuku.addRequestPermissionResultListener(permissionListener)
        Shizuku.addBinderReceivedListenerSticky(binderListener)
        Shizuku.addBinderDeadListener(deadListener)
        if (!Shizuku.pingBinder()) append("Shizuku draait niet: start hem in de Shizuku-app (draadloze foutopsporing)")
    }

    override fun onDestroy() {
        Shizuku.removeRequestPermissionResultListener(permissionListener)
        Shizuku.removeBinderReceivedListener(binderListener)
        Shizuku.removeBinderDeadListener(deadListener)
        if (control != null) runCatching { Shizuku.unbindUserService(args, connection, true) }
        super.onDestroy()
    }

    private fun bindIfAllowed() {
        if (Shizuku.checkSelfPermission() == PackageManager.PERMISSION_GRANTED) bind()
        else append("Nog geen Shizuku-toestemming: tik op Shizuku-toestemming")
    }

    private fun bind() = runCatching { Shizuku.bindUserService(args, connection) }
        .onFailure { append("bindUserService: ${it.javaClass.simpleName}: ${it.message}") }

    private fun requestPermission() {
        when {
            !Shizuku.pingBinder() -> append("Shizuku draait niet")
            Shizuku.checkSelfPermission() == PackageManager.PERMISSION_GRANTED -> { append("toestemming is er al"); bind() }
            else -> Shizuku.requestPermission(1)
        }
    }

    private fun combo() = run("combi") { c ->
        val t0 = System.nanoTime()
        val bt = c.start(BT)
        val tBt = (System.nanoTime() - t0) / 1_000_000
        val wifi = c.start(WIFI)
        val tAll = (System.nanoTime() - t0) / 1_000_000
        "$bt\n$wifi\nBT klaar na $tBt ms, hotspot klaar na $tAll ms (vanaf tik)"
    }

    private fun run(label: String, block: (ITetherControl) -> String) {
        val c = control ?: return append("$label: geen service (Shizuku draait niet of geen toestemming)")
        append("$label …")
        worker.execute {
            val out = runCatching { block(c) }.getOrElse { "FAIL ${it.javaClass.simpleName}: ${it.message}" }
            append("$label → $out")
        }
    }

    private fun append(line: String) { main.post {
        val ts = SimpleDateFormat("HH:mm:ss.SSS", Locale.ROOT).format(Date())
        log.append("$ts $line\n")
        scroll.post { scroll.fullScroll(ScrollView.FOCUS_DOWN) }
    } }

    private fun copyLog() {
        getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("spike log", log.text))
        Toast.makeText(this, "Log gekopieerd", Toast.LENGTH_SHORT).show()
    }

    private companion object {
        const val WIFI = 0
        const val BT = 2
    }
}
