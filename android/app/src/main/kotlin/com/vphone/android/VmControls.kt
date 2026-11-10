package com.vphone.android

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Canvas
import android.graphics.Matrix
import android.graphics.Paint
import android.graphics.Path
import android.os.Handler
import android.os.Looper
import android.view.HapticFeedbackConstants
import android.view.MotionEvent
import android.view.View
import android.view.ViewConfiguration
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.MaterialShapes
import androidx.graphics.shapes.toPath
import kotlin.math.abs
import kotlin.math.hypot

/**
 * A floating button over the guest's screen, as iOS's AssistiveTouch: drag it anywhere and it
 * settles at the nearer side; tap it for the menu, hold it for Home. Where it sits is kept.
 *
 * It is a plain View drawn by hand, in one of Material 3 Expressive's shapes and the theme's
 * colours ([container] and [content]): nothing of it lies over the guest's picture but itself.
 */
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@SuppressLint("ViewConstructor")
class FloatingButton(
    context: Context,
    private val container: Int,
    private val content: Int,
    private val onTap: () -> Unit,
    private val onHold: () -> Unit,
) : View(context) {
    private val density = resources.displayMetrics.density
    val size = (56 * density).toInt()
    private val margin = 8 * density
    private val slop = ViewConfiguration.get(context).scaledTouchSlop
    private val handler = Handler(Looper.getMainLooper())
    private val prefs = context.getSharedPreferences("ui", Context.MODE_PRIVATE)
    private val body = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = container }
    private val ring = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = content
        style = Paint.Style.STROKE
        strokeWidth = 2.5f * density
    }
    private val dot = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = content }
    /** Material's nine-sided cookie, which its polygons give in a unit square. */
    private val shape = Path().also { path ->
        MaterialShapes.Cookie9Sided.toPath(path)
        path.transform(Matrix().apply { setScale(size.toFloat(), size.toFloat()) })
    }
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
        canvas.drawPath(shape, body)
        canvas.drawCircle(s / 2, s / 2, s * 0.25f, ring)
        canvas.drawCircle(s / 2, s / 2, s * 0.13f, dot)
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
        const val IDLE_ALPHA = 0.6f
        const val FADE_AFTER = 2500L
    }
}
