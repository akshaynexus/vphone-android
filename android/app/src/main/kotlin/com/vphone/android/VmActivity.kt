package com.vphone.android

import android.app.GameManager
import android.app.GameState
import android.content.pm.ApplicationInfo
import android.content.Intent
import android.graphics.Color
import android.graphics.PixelFormat
import android.graphics.Rect
import android.graphics.drawable.GradientDrawable
import android.media.AudioManager
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import android.view.Surface
import android.view.Gravity
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import android.window.OnBackInvokedDispatcher
import androidx.activity.ComponentActivity
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.List
import androidx.compose.material.icons.filled.Home
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.platform.ComposeView
import com.vphone.android.ui.AppDialog
import com.vphone.android.ui.ControlItem
import com.vphone.android.ui.DialogAction
import com.vphone.android.ui.DialogHost
import com.vphone.android.ui.DialogSpec
import com.vphone.android.ui.VIcons
import com.vphone.android.ui.VmBottomBar
import com.vphone.android.ui.VmControlsMenu
import com.vphone.android.ui.VmEndPanel
import com.vphone.android.ui.VphoneTheme
import org.json.JSONObject

/**
 * The guest's screen, full-screen in portrait. Home, Lock and the rest are behind a floating
 * button, or a bar underneath for those who choose it; Back opens the same menu.
 *
 * The guest's picture is a SurfaceView in plain framework layouts, and its touches go straight
 * from that view to the native side. What is drawn with Compose is beside or over it only when it
 * is wanted: the bar sits under the picture, and [chrome] (the menu, the end of a run, a dialog)
 * is in the layout only while one of them shows.
 */
class VmActivity : ComponentActivity() {
    private val handler = Handler(Looper.getMainLooper())
    private lateinit var area: FrameLayout
    private lateinit var surface: SurfaceView
    private lateinit var status: TextView
    private lateinit var bar: ComposeView
    private lateinit var floating: FloatingButton
    private lateinit var chrome: ComposeView
    private lateinit var root: FrameLayout
    private val dialogs = DialogHost()
    private var menuOpen by mutableStateOf(false)
    private var menuHeading by mutableStateOf("")
    private var menuHint = ""
    private var menuItems: List<ControlItem> = emptyList()
    /** The run is over: its last words are shown in place of the picture. */
    private var ended by mutableStateOf(false)
    private var endTitle by mutableStateOf("")
    private var endMessage by mutableStateOf("")
    private var guestWidth = 0
    private var guestHeight = 0
    private var trackedPointer = -1

    private val poll = object : Runnable {
        override fun run() {
            applySpeed()
            refresh()
            handler.postDelayed(this, 500)
        }
    }

    private val surfaceCallback = object : SurfaceHolder.Callback {
        override fun surfaceCreated(holder: SurfaceHolder) {
            askFrameRate(holder.surface)
            Native.setSurface(holder.surface)
        }
        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
            askFrameRate(holder.surface)
            Native.setSurface(holder.surface)
        }
        override fun surfaceDestroyed(holder: SurfaceHolder) = Native.setSurface(null)
    }

    /** The VM's performance setting and refresh rate as last applied to the window. */
    private var appliedSpeed = ""

    /**
     * The VM's settings that are the window's to ask for, applied when they are first known and
     * when they change: the refresh rate, and Android's sustained-performance mode, which holds
     * the CPU's clocks down to what the phone can keep up for hours and so is on for Balanced
     * alone.
     */
    private fun applySpeed() {
        val state = App.state()
        val performance = state.optString("performance")
        val hz = state.optInt("refreshHz")
        val speed = "$performance@$hz"
        if (performance.isEmpty() || speed == appliedSpeed) return
        appliedSpeed = speed
        tellGameMode(true)
        if (getSystemService(PowerManager::class.java).isSustainedPerformanceModeSupported) {
            window.setSustainedPerformanceMode(performance == "balanced")
        }
        if (hz > 0) {
            val mode = display?.supportedModes
                ?.filter { it.physicalWidth == display?.mode?.physicalWidth && it.physicalHeight == display?.mode?.physicalHeight }
                ?.filter { it.refreshRate >= hz - 1f }
                ?.minByOrNull { it.refreshRate }
            window.attributes = window.attributes.apply {
                preferredRefreshRate = hz.toFloat()
                if (mode != null) preferredDisplayModeId = mode.modeId
            }
            Log.i("vphone", "screen: asked for $hz Hz" +
                (mode?.let { ", display mode ${it.modeId} at ${it.refreshRate} Hz" } ?: ", no display mode of this size reaches it") +
                "; modes ${display?.supportedModes?.joinToString { "${it.physicalWidth}x${it.physicalHeight}@${it.refreshRate.toInt()}" }}")
            if (::surface.isInitialized && surface.holder.surface?.isValid == true) askFrameRate(surface.holder.surface)
        }
    }

    /**
     * Tells Android the VM is on screen and must not be interrupted, which a phone's game mode
     * takes as leave to raise the clocks, and writes into the VM's log how the phone treats the
     * app: whether it counts as a game, and in which game mode. A phone that holds an ordinary
     * app's clocks down lets a game in its performance mode have more.
     */
    private fun tellGameMode(playing: Boolean) {
        val games = getSystemService(GameManager::class.java) ?: return
        try {
            games.setGameState(GameState(false, if (playing) GameState.MODE_GAMEPLAY_UNINTERRUPTIBLE else GameState.MODE_NONE))
            if (!playing) return
            val mode = when (val m = games.gameMode) {
                GameManager.GAME_MODE_UNSUPPORTED -> "unsupported (the phone does not treat the app as a game)"
                GameManager.GAME_MODE_STANDARD -> "standard"
                GameManager.GAME_MODE_PERFORMANCE -> "performance"
                GameManager.GAME_MODE_BATTERY -> "battery saver"
                GameManager.GAME_MODE_CUSTOM -> "custom"
                else -> "$m"
            }
            val game = applicationInfo.category == ApplicationInfo.CATEGORY_GAME
            Native.logLine("game mode: $mode; the app is ${if (game) "" else "not "}in Android's game category; " +
                "told the system the VM is on screen and not to be interrupted")
        } catch (e: RuntimeException) {
            Native.logLine("game mode: the system refused ($e)")
        }
    }

    /** Asks the compositor to show the guest's screen at the VM's refresh rate. */
    private fun askFrameRate(target: Surface) {
        val hz = App.state().optInt("refreshHz")
        if (hz <= 0 || !target.isValid) return
        try {
            target.setFrameRate(hz.toFloat(), Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE, Surface.CHANGE_FRAME_RATE_ALWAYS)
        } catch (e: RuntimeException) {
            Log.w("vphone", "screen: the surface refused $hz Hz: $e")
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        // The VM's sound is media: the volume keys move that volume here, playing or not.
        volumeControlStream = AudioManager.STREAM_MUSIC
        window.attributes = window.attributes.apply {
            layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
        }
        applySpeed()

        surface = SurfaceView(this).apply {
            holder.setFormat(PixelFormat.RGBA_8888)
            holder.addCallback(surfaceCallback)
            setOnTouchListener { _, event -> onTouch(event) }
        }
        val dp = resources.displayMetrics.density
        // The phone's own colours, dark: this screen is black around the guest's picture.
        val scheme = dynamicDarkColorScheme(this)
        status = TextView(this).apply {
            setTextColor(scheme.onSurface.toArgb())
            background = GradientDrawable().apply {
                cornerRadius = 24 * dp
                setColor(scheme.surfaceContainerHigh.copy(alpha = 0.85f).toArgb())
            }
            setPadding((16 * dp).toInt(), (10 * dp).toInt(), (16 * dp).toInt(), (10 * dp).toInt())
            minHeight = (48 * dp).toInt()
            gravity = Gravity.CENTER
            setOnClickListener { VmLogActivity.open(this@VmActivity) }
        }
        area = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(surface, FrameLayout.LayoutParams(0, 0))
            addView(
                status,
                FrameLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.TOP or Gravity.CENTER_HORIZONTAL)
                    .apply { topMargin = (8 * dp).toInt() },
            )
            addOnLayoutChangeListener { _, _, _, _, _, _, _, _, _ -> layoutImage() }
        }
        bar = ComposeView(this).apply {
            setContent {
                VphoneTheme(darkTheme = true) {
                    VmBottomBar(onHome = { Native.home() }, onLock = { Native.lock() }, onMore = { openMenu() })
                }
            }
        }
        floating = FloatingButton(
            this,
            container = scheme.primaryContainer.toArgb(),
            content = scheme.onPrimaryContainer.toArgb(),
            onTap = { openMenu() },
            onHold = { Native.home() },
        )
        chrome = ComposeView(this).apply {
            visibility = View.GONE
            setContent {
                VphoneTheme(darkTheme = true) {
                    if (ended) {
                        VmEndPanel(
                            title = endTitle,
                            message = endMessage,
                            onLog = { VmLogActivity.open(this@VmActivity, App.state().optString("logPath")) },
                            onLeave = { leave() },
                            onCopy = { App.copy(this@VmActivity, endMessage) },
                        )
                    } else if (menuOpen) {
                        VmControlsMenu(menuHeading, menuHint, menuItems, onDismiss = { closeMenu() })
                    }
                    dialogs.current?.let { spec ->
                        AppDialog(
                            spec,
                            onCopy = { App.copy(this@VmActivity, it) },
                            onDismiss = {
                                dialogs.current = null
                                applyControls()
                            },
                        )
                    }
                }
            }
        }
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
            addView(chrome, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
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

    /**
     * Which controls are in the layout. [chrome] is gone whenever it has nothing to show, so that
     * no view of it is between a finger and the guest's picture.
     */
    private fun applyControls() {
        bar.visibility = if (useBar) View.VISIBLE else View.GONE
        floating.visibility = if (useBar || ended) View.GONE else View.VISIBLE
        chrome.visibility = if (menuOpen || ended || dialogs.current != null) View.VISIBLE else View.GONE
    }

    private fun closeMenu() {
        menuOpen = false
        applyControls()
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
        menuItems = listOf(
            ControlItem(Icons.Filled.Home, "Home") { Native.home() },
            ControlItem(Icons.Filled.Lock, "Lock") { Native.lock() },
            ControlItem(VIcons.Log, "Logs") { VmLogActivity.open(this) },
            ControlItem(Icons.AutoMirrored.Filled.List, "VM list") { toVmList() },
            ControlItem(VIcons.Power, "Shut down", danger = true) { askStop() },
            ControlItem(if (bar) VIcons.Circle else VIcons.Bar, if (bar) "Use button" else "Use bar") {
                prefs.edit().putString(KEY_CONTROLS, if (bar) "float" else "bar").apply()
                applyControls()
            },
        )
        menuHeading = heading()
        menuHint = if (bar) "" else "Hold the button for Home. Drag it anywhere."
        menuOpen = true
        applyControls()
    }

    /** The VM keeps running; the list's "Return to" brings this screen back. */
    private fun toVmList() {
        startActivity(Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP))
        finish()
    }

    override fun onStart() {
        super.onStart()
        handler.post(poll)
    }

    override fun onStop() {
        tellGameMode(false)
        // Said again, with the mode the phone has by then, when the screen comes back.
        appliedSpeed = ""
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
        val over = phase in setOf("exited", "failed") || (phase == "idle" && bootError != null)
        if (menuOpen) menuHeading = heading()
        if (over) {
            status.visibility = View.GONE
            endTitle = if (bootError != null || phase == "failed") "The VM could not start" else "The VM stopped"
            endMessage = bootError ?: message
            if (!ended || menuOpen) {
                ended = true
                menuOpen = false
                applyControls()
            }
            return
        }
        if (ended) {
            ended = false
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

    /**
     * The finger's newest place only: Android batches the touchscreen's samples between two
     * frames into one event, and the older ones in it are places the guest no longer needs
     * (until 0.6.1 each was a report for its touchscreen driver). The native side holds moves
     * to one per refresh of the guest's display; a press and a release go at once.
     */
    private fun onTouch(event: MotionEvent): Boolean {
        val at = event.eventTimeNanos
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                trackedPointer = event.getPointerId(0)
                Native.touch(ACTION_DOWN, event.getX(0), event.getY(0), at)
            }
            MotionEvent.ACTION_MOVE -> {
                val index = event.findPointerIndex(trackedPointer)
                if (index >= 0) Native.touch(ACTION_MOVE, event.getX(index), event.getY(index), at)
            }
            MotionEvent.ACTION_POINTER_UP -> {
                if (event.getPointerId(event.actionIndex) == trackedPointer) {
                    Native.touch(ACTION_UP, event.getX(event.actionIndex), event.getY(event.actionIndex), at)
                    trackedPointer = -1
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                if (trackedPointer != -1) {
                    Native.touch(ACTION_UP, event.x, event.y, at)
                    trackedPointer = -1
                }
            }
        }
        return true
    }

    private fun askStop() {
        dialogs.show(
            DialogSpec(
                "Shut down the VM?",
                "iOS shuts down as it does on a phone.",
                confirm = DialogAction("Shut down") { Native.stop() },
                dismiss = DialogAction("Cancel"),
            ),
        )
        applyControls()
    }

    /** Back opens the menu, whose tiles leave or shut down: never one swipe that ends the VM. */
    private fun onBack() {
        when {
            ended -> leave()
            menuOpen -> closeMenu()
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
