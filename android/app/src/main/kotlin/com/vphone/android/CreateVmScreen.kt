package com.vphone.android

import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.vphone.android.ui.CreateVmVitalsScreen
import com.vphone.android.ui.VphoneTheme
import org.json.JSONObject
import java.io.File
import java.util.Locale

object CreateVmNative {
    init {
        System.loadLibrary("vphone_native")
    }

    external fun vitals(): String
}

/** What the VM's process is doing during setup: its vitals and each thread's CPU use. */
class CreateVmScreen : ComponentActivity() {
    private val handler = Handler(Looper.getMainLooper())
    private var status by mutableStateOf("")
    private var threads by mutableStateOf("")

    private val poll = object : Runnable {
        override fun run() {
            refresh()
            handler.postDelayed(this, 1000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        setContent {
            VphoneTheme { CreateVmVitalsScreen(status, threads, onBack = ::finish) }
        }
    }

    override fun onStart() {
        super.onStart()
        handler.post(poll)
    }

    override fun onStop() {
        handler.removeCallbacks(poll)
        super.onStop()
    }

    private fun refresh() {
        val text = CreateVmNative.vitals()
        val v = if (text.isEmpty()) JSONObject() else runCatching { JSONObject(text) }.getOrDefault(JSONObject())
        status = if (!v.optBoolean("alive")) {
            "No VM is running at the moment."
        } else {
            buildString {
                append(
                    when {
                        v.optBoolean("paused") -> "Paused"
                        (v.optJSONArray("pcs")?.length() ?: 0) == 0 -> "Guest CPU: can't be read (QEMU's monitor isn't answering)"
                        v.optBoolean("guestMoving") -> "Guest is running"
                        else -> "Guest is NOT moving"
                    },
                )
                val pc = v.optString("guestPc")
                if (pc.isNotEmpty()) append("  ${v.optString("guestEl")} $pc")
                append(String.format(Locale.US, "\nCPU %.0f%%  RAM %d MB  written %d MB", v.optDouble("cpuPercent", 0.0), v.optLong("rssMb"), v.optLong("diskWrittenMb")))
                val usb = v.optString("usbStage")
                if (usb.isNotEmpty()) append("\nUSB: $usb")
            }
        }
        val list = runCatching { File(filesDir, "cv/vm-threads.txt").readText() }.getOrDefault("")
        threads = if (list.isEmpty()) "No thread data yet." else list
    }
}
