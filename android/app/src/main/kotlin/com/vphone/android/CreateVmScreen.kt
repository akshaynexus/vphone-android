package com.vphone.android

import android.app.Activity
import android.graphics.Color
import android.graphics.Typeface
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.WindowManager
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
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
class CreateVmScreen : Activity() {
    private val handler = Handler(Looper.getMainLooper())
    private lateinit var status: TextView
    private lateinit var threads: TextView

    private val poll = object : Runnable {
        override fun run() {
            refresh()
            handler.postDelayed(this, 1000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        val dp = resources.displayMetrics.density
        status = TextView(this).apply {
            setTextColor(Color.WHITE)
            textSize = 15f
            setTextIsSelectable(true)
        }
        threads = TextView(this).apply {
            setTextColor(0xFFCCCCCC.toInt())
            typeface = Typeface.MONOSPACE
            textSize = 12f
            setTextIsSelectable(true)
            setPadding(0, (16 * dp).toInt(), 0, (16 * dp).toInt())
        }
        val note = TextView(this).apply {
            setTextColor(0xFFCCCCCC.toInt())
            textSize = 12f
            text = "Setup has no live picture yet. Each thread's history and native stacks are in Live log under VM."
        }
        val close = Button(this).apply {
            text = "Back to progress"
            isAllCaps = false
            setOnClickListener { finish() }
        }
        val pad = (16 * dp).toInt()
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
            addView(status)
            addView(threads)
            addView(note)
            addView(close)
        }
        val root = ScrollView(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(column)
        }
        setContentView(root)
        App.fitSystemBars(root)
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
        status.text = if (!v.optBoolean("alive")) {
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
        threads.text = if (list.isEmpty()) "No thread data yet." else list
    }
}
