package com.vphone.android

import android.app.ForegroundServiceStartNotAllowedException
import android.app.Notification
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.graphics.drawable.Icon
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.os.IBinder
import android.os.PowerManager
import org.json.JSONObject
import java.util.concurrent.Executors
import kotlin.concurrent.thread

/** Runs one creation to its end: foreground, wake lock, network and thermal watching, notification. */
class CreateService : Service() {
    private var wakeLock: PowerManager.WakeLock? = null
    private var callback: ConnectivityManager.NetworkCallback? = null
    private var thermal: PowerManager.OnThermalStatusChangedListener? = null
    @Volatile private var running = false

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val name = intent?.getStringExtra(EXTRA_NAME) ?: Native.createPending()
        if (intent?.action == ACTION_PAUSE) {
            Native.createPause(name, intent.getBooleanExtra(EXTRA_PAUSED, true))
            return START_STICKY
        }
        // A system restart (no intent) continues a run that was in progress, not one the
        // person stopped or that failed.
        val resumable = intent != null || JSONObject(Native.createStatus(name)).let {
            it.optString("state") in RESUMABLE && !it.optBoolean("paused")
        }
        if (name.isEmpty() || !resumable) {
            stopSelf()
            return START_NOT_STICKY
        }
        if (running) return START_STICKY
        try {
            startForeground(NOTIFICATION, progressNotification(name, null), ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } catch (e: ForegroundServiceStartNotAllowedException) {
            // Restarted by the system while the app is in the background; ask the person to continue.
            notifyContinue(name)
            stopSelf()
            return START_NOT_STICKY
        }
        running = true
        CreateBridge.context = applicationContext
        acquireWakeLock()
        watchNetwork()
        watchThermal()
        thread(name = "create-run", isDaemon = false) { run(name) }
        thread(name = "create-notify") { notifyLoop(name) }
        return START_STICKY
    }

    private fun acquireWakeLock() {
        if (wakeLock != null) return
        wakeLock = getSystemService(PowerManager::class.java)
            .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "vphone:create")
            .apply { acquire() }
    }

    private fun watchNetwork() {
        val cm = getSystemService(ConnectivityManager::class.java)
        val cb = object : ConnectivityManager.NetworkCallback() {
            override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
                // Not VALIDATED: networks that fail Android's check can still reach Apple, and
                // the download retries on its own if they cannot.
                val up = caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
                val unmetered = caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED) ||
                    caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_TEMPORARILY_NOT_METERED)
                Native.createNetwork(up, unmetered)
            }
            override fun onLost(network: Network) = Native.createNetwork(false, false)
            override fun onUnavailable() = Native.createNetwork(false, false)
        }
        Native.createNetwork(false, false)
        cm.registerDefaultNetworkCallback(cb)
        callback = cb
    }

    private fun watchThermal() {
        val pm = getSystemService(PowerManager::class.java)
        val listener = PowerManager.OnThermalStatusChangedListener { Native.createThermal(it) }
        Native.createThermal(pm.currentThermalStatus)
        pm.addThermalStatusListener(Executors.newSingleThreadExecutor(), listener)
        thermal = listener
    }

    private fun run(name: String) {
        Native.createRun(name)
        running = false
        val status = runCatching { JSONObject(Native.createStatus(name)) }.getOrNull()
        postResult(name, status)
        stopForeground(STOP_FOREGROUND_REMOVE)
        release()
        stopSelf()
        // A fresh process for the next creation: a ":create" process holds onto nothing else.
        Runtime.getRuntime().exit(0)
    }

    private fun release() {
        wakeLock?.let { if (it.isHeld) it.release() }
        wakeLock = null
        callback?.let { runCatching { getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(it) } }
        callback = null
        thermal?.let { runCatching { getSystemService(PowerManager::class.java).removeThermalStatusListener(it) } }
        thermal = null
    }

    private fun notifyLoop(name: String) {
        val nm = getSystemService(NotificationManager::class.java)
        var last = ""
        while (running) {
            val status = runCatching { JSONObject(Native.createStatus(name)) }.getOrNull()
            // A paused creation does no work, so it does not keep the phone awake.
            if (status?.optBoolean("paused") == true) {
                wakeLock?.let { if (it.isHeld) it.release() }
                wakeLock = null
            } else {
                acquireWakeLock()
            }
            val notification = progressNotification(name, status)
            val key = status?.toString().orEmpty()
            if (key != last) {
                last = key
                nm.notify(NOTIFICATION, notification)
            }
            Thread.sleep(2000)
        }
    }

    private fun openActivity(requestCode: Int): PendingIntent = PendingIntent.getActivity(
        this, requestCode,
        Intent(this, CreateActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

    private fun progressNotification(name: String, status: JSONObject?): Notification {
        val paused = status?.optBoolean("paused") == true
        val text = if (paused) "Paused" else status?.optString("message").orEmpty().ifEmpty { "Starting" }
        val total = status?.optLong("totalBytes") ?: 0
        val done = status?.optLong("doneBytes") ?: 0
        val builder = Notification.Builder(this, App.CHANNEL_CREATE)
            .setSmallIcon(R.drawable.ic_phone)
            .setContentTitle("Creating $name")
            .setContentText(text)
            .setContentIntent(openActivity(0))
            .setOngoing(true)
            .addAction(
                Notification.Action.Builder(
                    Icon.createWithResource(this, R.drawable.ic_phone),
                    if (paused) "Resume" else "Pause",
                    PendingIntent.getService(
                        this, 2,
                        Intent(this, CreateService::class.java).setAction(ACTION_PAUSE)
                            .putExtra(EXTRA_NAME, name).putExtra(EXTRA_PAUSED, !paused),
                        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
                    ),
                ).build(),
            )
        if (paused) builder.setProgress(0, 0, false)
        else if (total > 0) builder.setProgress(1000, (1000 * done / total).toInt(), false)
        else builder.setProgress(0, 0, true)
        return builder.build()
    }

    private fun postResult(name: String, status: JSONObject?) {
        val state = status?.optString("state").orEmpty()
        val (title, body) = when (state) {
            "done" -> "$name is ready" to "The VM is created. Open the app to launch it."
            "failed" -> "Creating $name stopped" to (status?.optString("error")?.ifEmpty { null } ?: "See the log")
            "cancelled" -> "Creating $name cancelled" to "Open the app to resume or delete it."
            else -> return
        }
        getSystemService(NotificationManager::class.java).notify(
            NOTIFICATION + 1,
            Notification.Builder(this, App.CHANNEL_CREATE_DONE)
                .setSmallIcon(R.drawable.ic_phone)
                .setContentTitle(title)
                .setContentText(body)
                .setStyle(Notification.BigTextStyle().bigText(body))
                .setContentIntent(openActivity(1))
                .setAutoCancel(true)
                .build(),
        )
    }

    private fun notifyContinue(name: String) {
        getSystemService(NotificationManager::class.java).notify(
            NOTIFICATION + 1,
            Notification.Builder(this, App.CHANNEL_CREATE_DONE)
                .setSmallIcon(R.drawable.ic_phone)
                .setContentTitle("Tap to continue creating $name")
                .setContentText("The phone restarted while the VM was being made.")
                .setContentIntent(openActivity(1))
                .setAutoCancel(true)
                .build(),
        )
    }

    override fun onDestroy() {
        release()
        super.onDestroy()
    }

    companion object {
        private const val NOTIFICATION = 2
        private const val EXTRA_NAME = "name"
        private const val EXTRA_PAUSED = "paused"
        private const val ACTION_PAUSE = "com.vphone.android.PAUSE"
        private val RESUMABLE = setOf("running", "interrupted")

        fun start(context: Context, name: String) {
            context.startForegroundService(Intent(context, CreateService::class.java).putExtra(EXTRA_NAME, name))
        }

        fun resumeAfterBoot(context: Context, name: String) {
            runCatching { start(context, name) }
        }
    }
}
