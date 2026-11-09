package com.vphone.android

import android.app.Activity
import android.content.ComponentName
import android.content.Intent
import android.content.ServiceConnection
import android.graphics.Bitmap
import android.graphics.Color
import android.graphics.PixelFormat
import android.os.Binder
import android.os.Bundle
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.Parcel
import android.view.Gravity
import android.view.PixelCopy
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager
import android.widget.Button
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.Switch
import android.widget.TextView
import org.json.JSONObject
import java.util.Locale

/** The Rust library's calls for this screen: the vitals file and the per-VM switch. */
object CreateVmNative {
    init {
        System.loadLibrary("vphone_native")
    }

    external fun vitals(): String
    external fun screenSetting(name: String): String
    external fun setScreenOn(name: String, on: Boolean): Boolean
}

/**
 * The VM's screen while it is being created: a live view of the guest, read-only, with how the
 * guest is doing. The Surface goes across processes to [CreateVmService], where QEMU draws to it.
 */
@Suppress("DEPRECATION") // android.widget.Switch: a plain toggle, as this screen has no theme of its own.
class CreateVmScreen : Activity() {
    private val handler = Handler(Looper.getMainLooper())
    private lateinit var surfaceView: SurfaceView
    private lateinit var status: TextView
    private lateinit var noPicture: TextView
    private lateinit var switch: Switch
    private lateinit var note: TextView
    private var vmName = ""
    private var bound = false
    private var service: IBinder? = null
    private var currentSurface: Surface? = null
    private val token = Binder()
    private var picture = false
    private var settingSwitch = false

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName?, binder: IBinder?) {
            service = binder
            currentSurface?.let { sendSurface(it) }
        }

        // The VM process ended: the next boot's process brings the binder back.
        override fun onServiceDisconnected(name: ComponentName?) {
            service = null
        }

        override fun onBindingDied(name: ComponentName?) {
            service = null
            unbind()
            bind()
        }

        override fun onNullBinding(name: ComponentName?) {
            service = null
        }
    }

    private val surfaceCallback = object : SurfaceHolder.Callback {
        override fun surfaceCreated(holder: SurfaceHolder) {
            currentSurface = holder.surface
            sendSurface(holder.surface)
        }

        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
            send(CreateVmService.TX_SURFACE_CHANGED)
        }

        override fun surfaceDestroyed(holder: SurfaceHolder) {
            currentSurface = null
            // Synchronous: the renderer must have let go of the Surface when this returns.
            send(CreateVmService.TX_CLEAR)
        }
    }

    private val poll = object : Runnable {
        override fun run() {
            refresh()
            handler.postDelayed(this, 1000)
        }
    }

    private val pixels = object : Runnable {
        override fun run() {
            checkPicture()
            handler.postDelayed(this, 2000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        vmName = intent.getStringExtra(EXTRA_VM).orEmpty()
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        val dp = resources.displayMetrics.density
        surfaceView = SurfaceView(this).apply {
            holder.setFormat(PixelFormat.RGBA_8888)
            holder.addCallback(surfaceCallback)
        }
        status = TextView(this).apply {
            setTextColor(Color.WHITE)
            setBackgroundColor(0xC0000000.toInt())
            setPadding((12 * dp).toInt(), (8 * dp).toInt(), (12 * dp).toInt(), (8 * dp).toInt())
            textSize = 13f
        }
        noPicture = TextView(this).apply {
            setTextColor(Color.WHITE)
            gravity = Gravity.CENTER
            setPadding((32 * dp).toInt(), 0, (32 * dp).toInt(), 0)
            textSize = 16f
            visibility = View.GONE
        }
        note = TextView(this).apply {
            setTextColor(0xFFCCCCCC.toInt())
            textSize = 12f
        }
        switch = Switch(this).apply {
            text = "Show the VM's screen during setup"
            setTextColor(Color.WHITE)
            setOnCheckedChangeListener { _, checked ->
                if (!settingSwitch) {
                    CreateVmNative.setScreenOn(vmName, checked)
                    refreshSetting()
                }
            }
        }
        val close = Button(this).apply {
            text = "Back to progress"
            isAllCaps = false
            setOnClickListener { finish() }
        }
        val bar = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(0xC0000000.toInt())
            setPadding((12 * dp).toInt(), (8 * dp).toInt(), (12 * dp).toInt(), (8 * dp).toInt())
            addView(switch)
            addView(note)
            addView(close)
        }
        val root = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(surfaceView, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
            addView(noPicture, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
            addView(status, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.TOP))
            addView(bar, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.BOTTOM))
        }
        setContentView(root)
        App.fitSystemBars(root)
    }

    override fun onStart() {
        super.onStart()
        bind()
        refreshSetting()
        handler.post(poll)
        handler.post(pixels)
    }

    override fun onStop() {
        handler.removeCallbacks(poll)
        handler.removeCallbacks(pixels)
        // A Surface that is still up goes back before the binding does.
        if (currentSurface != null) send(CreateVmService.TX_CLEAR)
        unbind()
        super.onStop()
    }

    /** Without BIND_AUTO_CREATE: this view must not start a VM process, only meet one. */
    private fun bind() {
        if (!bound) {
            bound = bindService(Intent(this, CreateVmService::class.java), connection, 0)
        }
    }

    private fun unbind() {
        if (bound) {
            bound = false
            service = null
            runCatching { unbindService(connection) }
        }
    }

    private fun sendSurface(surface: Surface) {
        val binder = service ?: return
        transact(binder, CreateVmService.TX_SET_SURFACE) { data ->
            data.writeStrongBinder(token)
            data.writeInt(1)
            surface.writeToParcel(data, 0)
        }
    }

    private fun send(code: Int) {
        val binder = service ?: return
        transact(binder, code) {}
    }

    private fun transact(binder: IBinder, code: Int, fill: (Parcel) -> Unit) {
        val data = Parcel.obtain()
        val reply = Parcel.obtain()
        try {
            data.writeInterfaceToken(CreateVmService.DESCRIPTOR)
            fill(data)
            binder.transact(code, data, reply, 0)
            reply.readException()
        } catch (e: Exception) {
            // The VM process went away mid-call; its next one gets the Surface on connect.
            service = null
        } finally {
            reply.recycle()
            data.recycle()
        }
    }

    /** Whether the guest has drawn anything: a frame in the Surface with something in it. */
    private fun checkPicture() {
        if (currentSurface?.isValid != true) {
            picture = false
            return
        }
        val bitmap = Bitmap.createBitmap(32, 72, Bitmap.Config.ARGB_8888)
        runCatching {
            PixelCopy.request(surfaceView, bitmap, { result ->
                picture = result == PixelCopy.SUCCESS && hasContent(bitmap)
                bitmap.recycle()
            }, handler)
        }.onFailure { picture = false }
    }

    private fun hasContent(bitmap: Bitmap): Boolean {
        val pixels = IntArray(bitmap.width * bitmap.height)
        bitmap.getPixels(pixels, 0, bitmap.width, 0, 0, bitmap.width, bitmap.height)
        return pixels.any { p -> Color.red(p) > 8 || Color.green(p) > 8 || Color.blue(p) > 8 }
    }

    private fun refreshSetting() {
        val setting = CreateVmNative.screenSetting(vmName)
        settingSwitch = true
        switch.isChecked = setting != "off"
        settingSwitch = false
        note.text = when {
            setting == "off" -> "Off for this VM. Takes effect from the next boot."
            setting.startsWith("headless:") ->
                "Turned off after a boot with the display failed:\n${setting.removePrefix("headless:")}\nTurn it on again to retry from the next boot."
            else -> "On. If a boot with the display fails, the step is redone without it automatically."
        }
    }

    private fun refresh() {
        val text = CreateVmNative.vitals()
        val v = if (text.isEmpty()) JSONObject() else runCatching { JSONObject(text) }.getOrDefault(JSONObject())
        val alive = v.optBoolean("alive")
        status.text = if (!alive) {
            "No VM is running at the moment."
        } else {
            buildString {
                append(if (v.optBoolean("paused")) "Paused" else if (v.optBoolean("guestMoving")) "Guest is running" else "Guest is NOT moving")
                val pc = v.optString("guestPc")
                if (pc.isNotEmpty()) append("  ${v.optString("guestEl")} $pc")
                append(String.format(Locale.US, "\nCPU %.0f%%  RAM %d MB  written %d MB", v.optDouble("cpuPercent", 0.0), v.optLong("rssMb"), v.optLong("diskWrittenMb")))
                val usb = v.optString("usbStage")
                if (usb.isNotEmpty()) append("\nUSB: $usb")
            }
        }
        val display = v.optString("display")
        val message = when {
            !alive -> "No picture: no VM is running right now. The next boot will show here."
            display == "off" -> "No picture: the screen is switched off for this VM."
            display == "headless" -> "No picture: this boot runs without a display.\n${v.optString("displayNote")}"
            display != "on" -> "No picture: this boot has no display."
            currentSurface == null -> ""
            !picture -> "No picture: the guest has not drawn anything yet.\n\nEarly boot and much of a restore show nothing even when all is well. The line above shows whether the guest is running."
            else -> ""
        }
        noPicture.text = message
        noPicture.visibility = if (message.isEmpty()) View.GONE else View.VISIBLE
    }

    companion object {
        const val EXTRA_VM = "vm"
    }
}
