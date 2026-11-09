package com.vphone.android

import android.app.Activity
import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Intent
import android.widget.TextView
import org.json.JSONObject

class App : Application() {
    override fun onCreate() {
        super.onCreate()
        val external = getExternalFilesDir(null) ?: filesDir
        Native.setup(filesDir.path, cacheDir.path, external.path, applicationInfo.nativeLibraryDir)
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel(CHANNEL, "Virtual machine", NotificationManager.IMPORTANCE_LOW),
        )
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel(CHANNEL_CREATE, "VM creation progress", NotificationManager.IMPORTANCE_LOW),
        )
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel(CHANNEL_CREATE_DONE, "VM creation finished or stopped", NotificationManager.IMPORTANCE_DEFAULT),
        )
    }

    companion object {
        const val CHANNEL = "vm"
        const val CHANNEL_CREATE = "create"
        const val CHANNEL_CREATE_DONE = "create-done"

        fun state() = JSONObject(Native.state())

        /** QEMU cannot start twice in one process: the next VM gets a new one. */
        fun restart(activity: Activity) {
            activity.startActivity(
                Intent(activity, MainActivity::class.java)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK),
            )
            Runtime.getRuntime().exit(0)
        }

        fun showLog(activity: Activity, path: String) {
            val text = TextView(activity).apply {
                typeface = android.graphics.Typeface.MONOSPACE
                textSize = 10f
                setTextIsSelectable(true)
                setPadding(24, 24, 24, 24)
                text = Native.logTail(path, 32768).ifEmpty { "(the log is empty)" }
            }
            android.app.AlertDialog.Builder(activity)
                .setTitle(path.substringAfterLast('/'))
                .setView(android.widget.ScrollView(activity).apply { addView(text) })
                .setPositiveButton("Close", null)
                .show()
        }
    }
}
