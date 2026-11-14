package com.rutger.wsconnect

import android.app.Application
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

class App : Application() {
    override fun onCreate() {
        super.onCreate()
        // Shizuku's UserService process also creates the Application; only wire up the main process.
        if (getProcessName() == packageName) Tether.init(this)
    }

    companion object {
        private const val PREFS = "wsconnect"
        fun enabled(c: Context) = c.getSharedPreferences(PREFS, MODE_PRIVATE).getBoolean("enabled", false)
        fun setEnabled(c: Context, on: Boolean) =
            c.getSharedPreferences(PREFS, MODE_PRIVATE).edit().putBoolean("enabled", on).apply()
    }
}

/** Restarts the link after a reboot. Shizuku won't run until wireless debugging is started again. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action == Intent.ACTION_BOOT_COMPLETED && App.enabled(context)) {
            context.startForegroundService(Intent(context, LinkService::class.java))
        }
    }
}
