package com.vphone.android

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.graphics.Color
import android.graphics.PixelFormat
import android.graphics.Rect
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.view.Gravity
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.WindowManager
import android.widget.Button
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import android.window.OnBackInvokedDispatcher
import org.json.JSONObject

/**
 * The guest's screen, full-screen in portrait. Home, Lock and the rest are behind a floating
 * button, or a bar underneath for those who choose it; Back opens the same menu.
 */
class VmActivity : Activity() {
    private val handler = Handler(Looper.getMainLooper())
    private lateinit var area: FrameLayout
    private lateinit var surface: SurfaceView
    private lateinit var status: TextView
    private lateinit var endPanel: ScrollView
    private lateinit var endText: TextView
    private lateinit var bar: LinearLayout
    private lateinit var floating: FloatingButton
    private lateinit var menu: ControlsMenu
    private lateinit var root: FrameLayout
    private var guestWidth = 0
    private var guestHeight = 0
    private var trackedPointer = -1

    private val poll = object : Runnable {
        override fun run() {
            refresh()
            handler.postDelayed(this, 500)
        }
    }

    private val surfaceCallback = object : SurfaceHolder.Callback {
        override fun surfaceCreated(holder: SurfaceHolder) = Native.setSurface(holder.surface)
        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) =
            Native.setSurface(holder.surface)
        override fun surfaceDestroyed(holder: SurfaceHolder) = Native.setSurface(null)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.attributes = window.attributes.apply {
            layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
        }
        if (getSystemService(PowerManager::class.java).isSustainedPerformanceModeSupported) {
            window.setSustainedPerformanceMode(true)
        }

        surface = SurfaceView(this).apply {
            holder.setFormat(PixelFormat.RGBA_8888)
            holder.addCallback(surfaceCallback)
            setOnTouchListener { _, event -> onTouch(event) }
        }
        status = TextView(this).apply {
            setTextColor(Color.WHITE)
            setBackgroundColor(0xB0000000.toInt())
            setPadding(24, 12, 24, 12)
            gravity = Gravity.CENTER
            setOnClickListener { VmLogActivity.open(this@VmActivity) }
        }
        endText = TextView(this).apply {
            setTextColor(Color.WHITE)
            setPadding(48, 96, 48, 24)
        }
        endPanel = ScrollView(this).apply {
            setBackgroundColor(Color.BLACK)
            visibility = View.GONE
            addView(LinearLayout(this@VmActivity).apply {
                orientation = LinearLayout.VERTICAL
                addView(endText)
                addView(button("Show the log") { VmLogActivity.open(this@VmActivity, App.state().optString("logPath")) })
                addView(button("Back to VMs") { leave() })
            })
        }
        area = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(surface, FrameLayout.LayoutParams(0, 0))
            addView(status, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
            addView(endPanel, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
            addOnLayoutChangeListener { _, _, _, _, _, _, _, _, _ -> layoutImage() }
        }
        bar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setBackgroundColor(Color.BLACK)
            val weight = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.MATCH_PARENT, 1f)
            addView(button("Home") { Native.home() }, weight)
            addView(button("Lock") { Native.lock() }, weight)
            addView(button("More") { openMenu() }, weight)
        }
        floating = FloatingButton(this, onTap = { openMenu() }, onHold = { Native.home() })
        menu = ControlsMenu(this)
        val screen = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.BLACK)
            addView(area, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
            addView(bar, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, (56 * resources.displayMetrics.density).toInt()))
        }
        root = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(screen, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
            addView(floating, FrameLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT))
            addView(menu, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
            addOnLayoutChangeListener { _, l, t, r, b, ol, ot, or, ob ->
                if (r - l != or - ol || b - t != ob - ot) floating.place(r - l, b - t)
            }
        }
        setContentView(root)
        applyControls()

        onBackInvokedDispatcher.registerOnBackInvokedCallback(OnBackInvokedDispatcher.PRIORITY_DEFAULT) { onBack() }
    }

    private val prefs get() = getSharedPreferences("ui", MODE_PRIVATE)
    private val useBar get() = prefs.getString(KEY_CONTROLS, "float") == "bar"

    private fun applyControls() {
        bar.visibility = if (useBar) View.VISIBLE else View.GONE
        floating.visibility = if (useBar || endPanel.visibility == View.VISIBLE) View.GONE else View.VISIBLE
    }

    private fun heading(): String {
        val state = App.state()
        val phase = state.optString("phase")
        val started = state.optLong("startedMs")
        val elapsed = if (started > 0 && phase in UP) " · ${VmLogActivity.clock((System.currentTimeMillis() - started) / 1000)}" else ""
        return "${state.optString("vm")}: ${VmService.phaseText(phase)}$elapsed"
    }

    private fun openMenu() {
        val bar = useBar
        menu.setItems(listOf(
            ControlsMenu.Item("🏠", "Home") { Native.home() },
            ControlsMenu.Item("🔒", "Lock") { Native.lock() },
            ControlsMenu.Item("📄", "Logs") { VmLogActivity.open(this) },
            ControlsMenu.Item("↩️", "VM list") { toVmList() },
            ControlsMenu.Item("⏹️", "Shut down") { askStop() },
            ControlsMenu.Item(if (bar) "🔘" else "▁", if (bar) "Use button" else "Use bar") {
                prefs.edit().putString(KEY_CONTROLS, if (bar) "float" else "bar").apply()
                applyControls()
            },
        ))
        menu.open(heading(), if (bar) "" else "Hold the button for Home. Drag it anywhere.")
    }

    /** The VM keeps running; the list's "Return to" brings this screen back. */
    private fun toVmList() {
        startActivity(Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP))
        finish()
    }

    private fun button(label: String, action: () -> Unit) = Button(this).apply {
        text = label
        isAllCaps = false
        setOnClickListener { action() }
    }

    override fun onStart() {
        super.onStart()
        handler.post(poll)
    }

    override fun onStop() {
        handler.removeCallbacks(poll)
        super.onStop()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) {
            window.insetsController?.apply {
                systemBarsBehavior = WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
                hide(WindowInsets.Type.systemBars())
            }
        }
    }

    private fun refresh() {
        val state: JSONObject = App.state()
        val width = state.optInt("width")
        val height = state.optInt("height")
        if (width > 0 && (width != guestWidth || height != guestHeight)) {
            guestWidth = width
            guestHeight = height
            surface.holder.setFixedSize(width, height)
            layoutImage()
        }
        Native.takeNotice().let { if (it.isNotEmpty()) Toast.makeText(this, it, Toast.LENGTH_LONG).show() }

        val phase = state.getString("phase")
        val message = state.optString("message")
        val bootError = VmService.lastBootError
        val ended = phase in setOf("exited", "failed") || (phase == "idle" && bootError != null)
        if (menu.isOpen) menu.setHeading(heading())
        if (ended) {
            status.visibility = View.GONE
            menu.close()
            if (endPanel.visibility != View.VISIBLE) {
                endPanel.visibility = View.VISIBLE
                applyControls()
            }
            endText.text = when {
                bootError != null -> bootError
                phase == "failed" -> "The VM could not start.\n\n$message"
                message.isNotEmpty() -> "The VM stopped.\n\n$message"
                else -> "The VM stopped."
            }
            return
        }
        if (endPanel.visibility == View.VISIBLE) {
            endPanel.visibility = View.GONE
            applyControls()
        }
        if (phase == "running" && message.isEmpty()) {
            status.visibility = View.GONE
        } else {
            status.visibility = View.VISIBLE
            val started = state.optLong("startedMs")
            val elapsed = if (started > 0 && phase in UP) " · ${VmLogActivity.clock((System.currentTimeMillis() - started) / 1000)}" else ""
            val text = if (message.isEmpty()) VmService.phaseText(phase) else "${VmService.phaseText(phase)}: $message"
            status.text = "$text$elapsed  ·  tap for the log"
        }
    }

    private fun layoutImage() {
        if (guestWidth == 0 || area.width == 0) return
        val rect = Native.layoutImage(area.width, area.height)
        surface.layoutParams = FrameLayout.LayoutParams(rect[2], rect[3]).apply {
            leftMargin = rect[0]
            topMargin = rect[1]
        }
        // iOS's edge swipes reach the guest instead of Android's Back, as far as Android allows.
        val edge = (32 * resources.displayMetrics.density).toInt()
        val band = (200 * resources.displayMetrics.density).toInt().coerceAtMost(rect[3])
        val top = (rect[3] - band) / 2
        surface.systemGestureExclusionRects = listOf(
            Rect(0, top, edge, top + band),
            Rect(rect[2] - edge, top, rect[2], top + band),
        )
    }

    private fun onTouch(event: MotionEvent): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                trackedPointer = event.getPointerId(0)
                Native.touch(ACTION_DOWN, event.getX(0), event.getY(0))
            }
            MotionEvent.ACTION_MOVE -> {
                val index = event.findPointerIndex(trackedPointer)
                if (index >= 0) {
                    for (h in 0 until event.historySize) {
                        Native.touch(ACTION_MOVE, event.getHistoricalX(index, h), event.getHistoricalY(index, h))
                    }
                    Native.touch(ACTION_MOVE, event.getX(index), event.getY(index))
                }
            }
            MotionEvent.ACTION_POINTER_UP -> {
                if (event.getPointerId(event.actionIndex) == trackedPointer) {
                    Native.touch(ACTION_UP, event.getX(event.actionIndex), event.getY(event.actionIndex))
                    trackedPointer = -1
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                if (trackedPointer != -1) {
                    Native.touch(ACTION_UP, event.x, event.y)
                    trackedPointer = -1
                }
            }
        }
        return true
    }

    private fun askStop() {
        AlertDialog.Builder(this)
            .setTitle("Shut down the VM?")
            .setMessage("iOS shuts down as it does on a phone.")
            .setPositiveButton("Shut down") { _, _ -> Native.stop() }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Back opens the menu, whose tiles leave or shut down: never one swipe that ends the VM. */
    private fun onBack() {
        when {
            endPanel.visibility == View.VISIBLE -> leave()
            menu.isOpen -> menu.close()
            else -> openMenu()
        }
    }

    private fun leave() {
        if (App.state().optBoolean("needsRestart")) App.restart(this) else finish()
    }

    private companion object {
        const val KEY_CONTROLS = "controls"
        val UP = setOf("booting", "unlocking", "running", "stopping")
        const val ACTION_DOWN = 0
        const val ACTION_UP = 1
        const val ACTION_MOVE = 2
    }
}
