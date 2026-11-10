package com.vphone.android

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.RectF
import android.graphics.drawable.GradientDrawable
import android.os.Handler
import android.os.Looper
import android.view.Gravity
import android.view.HapticFeedbackConstants
import android.view.MotionEvent
import android.view.View
import android.view.ViewConfiguration
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.GridLayout
import android.widget.LinearLayout
import android.widget.TextView
import kotlin.math.abs
import kotlin.math.hypot

/**
 * A floating button over the guest's screen, as iOS's AssistiveTouch: drag it anywhere and it
 * settles at the nearer side; tap it for the menu, hold it for Home. Where it sits is kept.
 */
@SuppressLint("ViewConstructor")
class FloatingButton(context: Context, private val onTap: () -> Unit, private val onHold: () -> Unit) : View(context) {
    private val density = resources.displayMetrics.density
    val size = (56 * density).toInt()
    private val margin = 8 * density
    private val slop = ViewConfiguration.get(context).scaledTouchSlop
    private val handler = Handler(Looper.getMainLooper())
    private val prefs = context.getSharedPreferences("ui", Context.MODE_PRIVATE)
    private val body = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = 0xCC1C1C1E.toInt() }
    private val ring = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = 0x66FFFFFF
        style = Paint.Style.STROKE
        strokeWidth = 2 * density
    }
    private val dot = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = 0xE6FFFFFF.toInt() }
    private val box = RectF()
    private var downX = 0f
    private var downY = 0f
    private var startX = 0f
    private var startY = 0f
    private var dragging = false
    private var held = false

    private val fade = Runnable { animate().alpha(IDLE_ALPHA).setDuration(400).start() }
    private val hold = Runnable {
        held = true
        performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
        onHold()
    }

    init {
        alpha = IDLE_ALPHA
        contentDescription = "VM controls"
    }

    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) = setMeasuredDimension(size, size)

    /** A drag that starts on the button must not become Android's Back swipe at the screen's edge. */
    override fun onLayout(changed: Boolean, left: Int, top: Int, right: Int, bottom: Int) {
        super.onLayout(changed, left, top, right, bottom)
        if (changed) systemGestureExclusionRects = listOf(android.graphics.Rect(0, 0, width, height))
    }

    override fun onDraw(canvas: Canvas) {
        val s = size.toFloat()
        box.set(0f, 0f, s, s)
        canvas.drawRoundRect(box, s * 0.3f, s * 0.3f, body)
        canvas.drawCircle(s / 2, s / 2, s * 0.32f, ring)
        canvas.drawCircle(s / 2, s / 2, s * 0.24f, ring)
        canvas.drawCircle(s / 2, s / 2, s * 0.16f, dot)
    }

    /** Back where it was left, inside a parent of this size. */
    fun place(parentWidth: Int, parentHeight: Int) {
        if (parentWidth == 0 || parentHeight == 0) return
        val right = prefs.getBoolean(KEY_RIGHT, true)
        val fraction = prefs.getFloat(KEY_Y, 0.6f)
        translationX = if (right) parentWidth - size - margin else margin
        translationY = clampY(fraction * parentHeight, parentHeight)
    }

    private fun clampY(y: Float, parentHeight: Int) = y.coerceIn(margin, (parentHeight - size - margin).coerceAtLeast(margin))

    @SuppressLint("ClickableViewAccessibility")
    override fun onTouchEvent(event: MotionEvent): Boolean {
        val parent = parent as? View ?: return false
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                handler.removeCallbacks(fade)
                animate().cancel()
                alpha = 1f
                downX = event.rawX
                downY = event.rawY
                startX = translationX
                startY = translationY
                dragging = false
                held = false
                handler.postDelayed(hold, ViewConfiguration.getLongPressTimeout().toLong())
            }
            MotionEvent.ACTION_MOVE -> {
                val dx = event.rawX - downX
                val dy = event.rawY - downY
                if (!dragging && !held && hypot(dx, dy) > slop) {
                    dragging = true
                    handler.removeCallbacks(hold)
                }
                if (dragging) {
                    translationX = (startX + dx).coerceIn(0f, (parent.width - size).toFloat())
                    translationY = clampY(startY + dy, parent.height)
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                handler.removeCallbacks(hold)
                if (dragging) {
                    settle(parent)
                } else if (!held && event.actionMasked == MotionEvent.ACTION_UP) {
                    performClick()
                }
                handler.postDelayed(fade, FADE_AFTER)
            }
        }
        return true
    }

    override fun performClick(): Boolean {
        super.performClick()
        onTap()
        return true
    }

    /** To the nearer side, remembered for the next time the VM's screen opens. */
    private fun settle(parent: View) {
        val right = translationX + size / 2 > parent.width / 2
        val x = if (right) parent.width - size - margin else margin
        animate().translationX(x).setDuration(if (abs(x - translationX) < 1) 0 else 180).start()
        prefs.edit()
            .putBoolean(KEY_RIGHT, right)
            .putFloat(KEY_Y, translationY / parent.height.coerceAtLeast(1))
            .apply()
    }

    private companion object {
        const val KEY_RIGHT = "floatRight"
        const val KEY_Y = "floatY"
        const val IDLE_ALPHA = 0.55f
        const val FADE_AFTER = 2500L
    }
}

/** The controls' menu: a dimmed screen with a panel of tiles, closed by a tap outside or Back. */
@SuppressLint("ViewConstructor")
class ControlsMenu(context: Context) : FrameLayout(context) {
    class Item(val icon: String, val label: String, val action: () -> Unit)

    private val density = resources.displayMetrics.density
    private val title = TextView(context).apply {
        setTextColor(Color.WHITE)
        textSize = 15f
        gravity = Gravity.CENTER
    }
    private val hint = TextView(context).apply {
        setTextColor(0xB3FFFFFF.toInt())
        textSize = 12f
        gravity = Gravity.CENTER
    }
    private val grid = GridLayout(context).apply { columnCount = 3 }

    val isOpen get() = visibility == VISIBLE

    init {
        setBackgroundColor(0x99000000.toInt())
        visibility = GONE
        isClickable = true
        setOnClickListener { close() }
        val pad = (20 * density).toInt()
        val panel = LinearLayout(context).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
            background = GradientDrawable().apply {
                cornerRadius = 28 * density
                setColor(0xF01C1C1E.toInt())
            }
            isClickable = true // a tap between tiles leaves it open
            addView(title)
            addView(grid, LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply {
                gravity = Gravity.CENTER_HORIZONTAL
                topMargin = (12 * density).toInt()
                bottomMargin = (8 * density).toInt()
            })
            addView(hint)
        }
        addView(panel, LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.CENTER))
    }

    fun setItems(items: List<Item>) {
        grid.removeAllViews()
        val side = (88 * density).toInt()
        for (item in items) {
            grid.addView(tile(item), GridLayout.LayoutParams().apply {
                width = side
                height = side
                setMargins(4, 4, 4, 4)
            })
        }
    }

    private fun tile(item: Item) = LinearLayout(context).apply {
        orientation = LinearLayout.VERTICAL
        gravity = Gravity.CENTER
        background = GradientDrawable().apply {
            cornerRadius = 18 * density
            setColor(0xFF2C2C2E.toInt())
        }
        isClickable = true
        isFocusable = true
        contentDescription = item.label
        setOnClickListener {
            close()
            item.action()
        }
        addView(TextView(context).apply {
            text = item.icon
            textSize = 24f
            gravity = Gravity.CENTER
            setTextColor(Color.WHITE)
        })
        addView(TextView(context).apply {
            text = item.label
            textSize = 12f
            gravity = Gravity.CENTER
            setTextColor(Color.WHITE)
        })
    }

    fun open(heading: String, note: String) {
        title.text = heading
        hint.text = note
        hint.visibility = if (note.isEmpty()) GONE else VISIBLE
        visibility = VISIBLE
        alpha = 0f
        animate().alpha(1f).setDuration(120).start()
    }

    fun setHeading(heading: String) {
        title.text = heading
    }

    fun close() {
        visibility = GONE
    }
}
