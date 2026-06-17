package org.gameros.ansync

import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.PixelFormat
import android.os.Build
import android.provider.Settings
import android.util.Log
import android.view.Gravity
import android.view.View
import android.view.WindowManager

/**
 * Floating circular cursor drawn on top of every window. Gives the
 * remote operator real-time visual feedback of where the next
 * AccessibilityService gesture will land — the gesture itself is
 * still buffered + dispatched on a 60 Hz cadence, but the overlay
 * follows the host pointer immediately.
 *
 * Backed by `TYPE_ACCESSIBILITY_OVERLAY` so it appears even on top
 * of secure surfaces the AccessibilityService can already see.
 * Requires `SYSTEM_ALERT_WINDOW` permission; absence of that grant
 * degrades gracefully — [show] no-ops and the gesture pipeline keeps
 * working without visual feedback.
 *
 * Thread-affinity: all `WindowManager` operations are routed to the
 * main looper via `post`, so callers from arbitrary threads (the
 * AccessibilityService poller, the host's input stream loop) don't
 * need to synchronise themselves.
 */
class CursorOverlay(private val context: Context) {

    private val wm = context.getSystemService(Context.WINDOW_SERVICE) as? WindowManager
    private val mainHandler = android.os.Handler(context.mainLooper)
    private var view: CursorView? = null
    private var params: WindowManager.LayoutParams? = null
    @Volatile private var visible = false

    fun canShow(): Boolean = Settings.canDrawOverlays(context)

    /** Bring the cursor up at (x, y). Subsequent calls are no-op. */
    fun show(x: Float, y: Float) {
        if (visible) {
            move(x, y)
            return
        }
        if (!canShow()) {
            Log.w(TAG, "SYSTEM_ALERT_WINDOW not granted; cursor overlay disabled")
            return
        }
        mainHandler.post {
            if (visible) return@post
            val v = CursorView(context)
            val p = WindowManager.LayoutParams(
                CURSOR_SIZE_PX,
                CURSOR_SIZE_PX,
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY
                } else {
                    @Suppress("DEPRECATION")
                    WindowManager.LayoutParams.TYPE_SYSTEM_ALERT
                },
                WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE
                    or WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE
                    or WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
                PixelFormat.TRANSLUCENT,
            ).apply {
                gravity = Gravity.TOP or Gravity.START
                this.x = (x - CURSOR_SIZE_PX / 2f).toInt()
                this.y = (y - CURSOR_SIZE_PX / 2f).toInt()
            }
            try {
                wm?.addView(v, p)
                view = v
                params = p
                visible = true
            } catch (e: Exception) {
                Log.w(TAG, "addView failed", e)
            }
        }
    }

    fun move(x: Float, y: Float) {
        if (!visible) {
            // First move doubles as a show.
            show(x, y)
            return
        }
        mainHandler.post {
            val v = view ?: return@post
            val p = params ?: return@post
            p.x = (x - CURSOR_SIZE_PX / 2f).toInt()
            p.y = (y - CURSOR_SIZE_PX / 2f).toInt()
            try {
                wm?.updateViewLayout(v, p)
            } catch (e: Exception) {
                Log.w(TAG, "updateViewLayout failed", e)
            }
        }
    }

    /** Tear down the overlay; safe to call when not shown. */
    fun hide() {
        if (!visible) return
        mainHandler.post {
            val v = view ?: return@post
            try {
                wm?.removeViewImmediate(v)
            } catch (e: Exception) {
                Log.w(TAG, "removeViewImmediate failed", e)
            }
            view = null
            params = null
            visible = false
        }
    }

    /** Flip the cursor between idle / pressed colour. Cheap visual
     *  cue that the operator's mouse button is held. */
    fun setPressed(pressed: Boolean) {
        mainHandler.post {
            view?.setPressed(pressed)
        }
    }

    /**
     * Tiny self-rendered View. We don't pull in a drawable resource
     * so the asset doesn't have to ship in the apk; a 32-px filled
     * circle with a contrasting ring is enough to be legible against
     * any background.
     */
    private class CursorView(context: Context) : View(context) {
        private var pressed = false
        private val fillPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.FILL
            color = COLOR_FILL_IDLE
        }
        private val ringPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = 3f
            color = COLOR_RING
        }

        override fun setPressed(p: Boolean) {
            if (pressed == p) return
            pressed = p
            fillPaint.color = if (p) COLOR_FILL_PRESSED else COLOR_FILL_IDLE
            invalidate()
        }

        override fun onDraw(canvas: Canvas) {
            val cx = width / 2f
            val cy = height / 2f
            val rOuter = (width.coerceAtMost(height) / 2f) - 2f
            canvas.drawCircle(cx, cy, rOuter, fillPaint)
            canvas.drawCircle(cx, cy, rOuter, ringPaint)
        }

        companion object {
            // Translucent-on-purpose: opaque cursors hide what's under
            // them which is exactly the content the user is trying to
            // click. ~50 % alpha keeps the target visible.
            private const val COLOR_FILL_IDLE = 0x60_FFFFFF.toInt()
            private const val COLOR_FILL_PRESSED = 0xA0_FF4040.toInt()
            private const val COLOR_RING = 0xFF_202020.toInt()
        }
    }

    companion object {
        private const val TAG = "ansync.cursor"
        // 32 dp on a typical mdpi-ish density. We don't bother
        // scaling per-display because the cursor is visual feedback,
        // not a precise input target.
        private const val CURSOR_SIZE_PX = 40
    }
}
