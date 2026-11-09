package com.vphone.android

import android.app.ActivityManager
import android.app.ApplicationExitInfo
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.os.IBinder

/** What the Rust orchestrator needs from Android: a process for QEMU, and why it died. */
object CreateBridge {
    @Volatile var context: Context? = null
    @Volatile private var connection: ServiceConnection? = null

    @JvmStatic
    fun startVm(): String {
        val ctx = context ?: return "the creation service is not running"
        releaseVm()
        val conn = object : ServiceConnection {
            override fun onServiceConnected(name: ComponentName?, service: IBinder?) {}
            override fun onServiceDisconnected(name: ComponentName?) {}
        }
        val intent = Intent(ctx, CreateVmService::class.java)
        val bound = ctx.bindService(intent, conn, Context.BIND_AUTO_CREATE or Context.BIND_IMPORTANT)
        if (!bound) return "Android refused to start the VM process"
        connection = conn
        return ""
    }

    @JvmStatic
    fun releaseVm() {
        val conn = connection ?: return
        connection = null
        runCatching { context?.unbindService(conn) }
    }

    /** The readable parts of Android's native crash record: the signal, abort text and stack. */
    private fun crashDetails(info: ApplicationExitInfo): String {
        val bytes = runCatching { info.traceInputStream?.use { it.readBytes() } }.getOrNull() ?: return ""
        val lines = Regex("[\\x20-\\x7e]{6,}")
            .findAll(String(bytes, Charsets.ISO_8859_1))
            .map { it.value }
            .filter { it.contains(".so") || it.contains("SIG") || it.contains("panic") || it.contains("abort", true) }
            .distinct()
            .take(30)
            .joinToString("\n")
        return if (lines.isEmpty()) "" else "\nCrash record:\n$lines"
    }

    @JvmStatic
    fun vmExitReason(): String {
        val ctx = context ?: return ""
        val am = ctx.getSystemService(ActivityManager::class.java)
        val info = am.getHistoricalProcessExitReasons(ctx.packageName, 0, 8)
            .firstOrNull { it.processName.endsWith(":createvm") } ?: return ""
        val why = when (info.reason) {
            ApplicationExitInfo.REASON_LOW_MEMORY -> "Android ran out of memory and stopped the VM."
            ApplicationExitInfo.REASON_CRASH_NATIVE -> "QEMU crashed (signal ${info.status})."
            ApplicationExitInfo.REASON_SIGNALED -> "The VM process was killed by signal ${info.status}."
            ApplicationExitInfo.REASON_EXIT_SELF -> "QEMU exited with status ${info.status}."
            ApplicationExitInfo.REASON_EXCESSIVE_RESOURCE_USAGE -> "Android stopped the VM for excessive resource use."
            else -> "Android ended the VM process (reason ${info.reason})."
        }
        val base = if (info.description.isNullOrEmpty()) why else "$why (${info.description})"
        return if (info.reason == ApplicationExitInfo.REASON_CRASH_NATIVE) base + crashDetails(info) else base
    }
}
