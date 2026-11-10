package com.vphone.android

import android.app.Activity
import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.widget.Toast
import org.json.JSONObject

class App : Application() {
    override fun onCreate() {
        super.onCreate()
        val external = getExternalFilesDir(null) ?: filesDir
        Native.setup(filesDir.path, cacheDir.path, external.path, applicationInfo.nativeLibraryDir)
        // The APK of an update this app now runs, or of an older one, is no longer needed.
        if (Application.getProcessName() == packageName) {
            Updater.cleanup(this)
            // A packed VM is kept a day, for a share that is still reading it.
            Native.shareCleanup(VmService.shareDir(this).path)
        }
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

        fun copy(activity: Activity, text: String) {
            activity.getSystemService(ClipboardManager::class.java)
                .setPrimaryClip(ClipData.newPlainText("vphone", text))
            Toast.makeText(activity, "Copied", Toast.LENGTH_SHORT).show()
        }
    }
}
