package org.gameros.ansync

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.content.Context
import android.graphics.Path
import android.os.Handler
import android.os.HandlerThread
import android.util.DisplayMetrics
import android.util.Log
import android.view.KeyEvent
import android.view.WindowManager
import android.view.accessibility.AccessibilityEvent

/**
 * Reverse-input handler: receives `proto::InputMessage` packets from
 * the paired host (via the QUIC `Input` stream → native cdylib →
 * `NativeBridge.nativePollInputMessage`) and replays them on this
 * device using `dispatchGesture` for touch + `performGlobalAction`
 * for system navigation.
 *
 * The poll loop runs on a dedicated thread so blocking calls into
 * the native bridge never stall the AccessibilityService's main
 * thread. The loop exits cleanly when `nativePollInputMessage`
 * returns null (session torn down) or when the service is unbound.
 */
class AnsyncAccessibilityService : AccessibilityService() {

    private var pollThread: HandlerThread? = null
    private var pollHandler: Handler? = null
    @Volatile private var running = false
    private var cursor: CursorOverlay? = null

    override fun onServiceConnected() {
        super.onServiceConnected()
        INSTANCE = this
        cursor = CursorOverlay(this)
        startPolling()
    }

    override fun onUnbind(intent: android.content.Intent?): Boolean {
        stopPolling()
        cursor?.hide()
        cursor = null
        INSTANCE = null
        return super.onUnbind(intent)
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {
        // No-op: we are a write-only consumer of dispatchGesture +
        // performGlobalAction. canRetrieveWindowContent=false in the
        // service config makes this stream cheap to ignore.
    }

    override fun onInterrupt() {
        // Required override.
    }

    override fun onKeyEvent(event: KeyEvent?): Boolean = false

    private fun startPolling() {
        if (running) return
        running = true
        val t = HandlerThread("ansync-input-poll").also { it.start() }
        pollThread = t
        pollHandler = Handler(t.looper).also { it.post(pollRunnable) }
    }

    private fun stopPolling() {
        running = false
        pollHandler = null
        pollThread?.quitSafely()
        pollThread = null
    }

    private val pollRunnable = object : Runnable {
        override fun run() {
            if (!running) return
            val bytes = NativeBridge.nativePollInputMessage()
            if (bytes == null) {
                // Session ended; nap briefly and retry — the
                // companion service will re-open once paired.
                pollHandler?.postDelayed(this, RETRY_DELAY_MS)
                return
            }
            try {
                val msg = WireInputMessage.decode(bytes)
                replay(msg)
            } catch (e: Exception) {
                Log.w(TAG, "decode/replay failed", e)
            }
            pollHandler?.post(this)
        }
    }

    private fun replay(msg: WireInputMessage) {
        when (msg) {
            is WireInputMessage.TouchSlot -> dispatchTouch(msg)
            is WireInputMessage.KeyPress -> dispatchKey(msg)
            is WireInputMessage.Text -> dispatchText(msg.text)
            is WireInputMessage.Stylus, is WireInputMessage.MouseMove,
            is WireInputMessage.MouseButton, is WireInputMessage.MouseWheel,
            is WireInputMessage.Gamepad -> {
                // Out-of-band on Android: the host shouldn't push these
                // back to the device. Drop silently.
            }
        }
    }

    /**
     * Insert `text` at the focused EditText via
     * `AccessibilityNodeInfo.ACTION_SET_TEXT`. Non-rooted Android
     * doesn't let an app inject raw key events for arbitrary
     * characters, but `ACTION_SET_TEXT` on the focused node is the
     * documented escape hatch — provided the service config has
     * `canRetrieveWindowContent=true` (we flipped it for this).
     *
     * Behaviour: appends `text` to whatever the field already
     * contains, then puts the caret after the inserted text.
     * Bounded use case: typing a short string into a focused input.
     * Won't handle backspace, arrow navigation, or richtext.
     */
    private fun dispatchText(text: String) {
        if (text.isEmpty()) return
        val node = findFocus(android.view.accessibility.AccessibilityNodeInfo.FOCUS_INPUT)
        if (node == null) {
            Log.d(TAG, "Text injection: no focused input")
            return
        }
        try {
            val existing = node.text?.toString() ?: ""
            val combined = existing + text
            val args = android.os.Bundle().apply {
                putCharSequence(
                    android.view.accessibility.AccessibilityNodeInfo
                        .ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE,
                    combined,
                )
            }
            val ok = node.performAction(
                android.view.accessibility.AccessibilityNodeInfo.ACTION_SET_TEXT,
                args,
            )
            if (!ok) {
                Log.w(TAG, "ACTION_SET_TEXT rejected by focused node")
                return
            }
            // Move caret to the end of the new combined text.
            val selArgs = android.os.Bundle().apply {
                putInt(
                    android.view.accessibility.AccessibilityNodeInfo
                        .ACTION_ARGUMENT_SELECTION_START_INT,
                    combined.length,
                )
                putInt(
                    android.view.accessibility.AccessibilityNodeInfo
                        .ACTION_ARGUMENT_SELECTION_END_INT,
                    combined.length,
                )
            }
            node.performAction(
                android.view.accessibility.AccessibilityNodeInfo.ACTION_SET_SELECTION,
                selArgs,
            )
        } finally {
            node.recycle()
        }
    }

    /**
     * System-key replay. The host sends evdev `KEY_*` codes; we map a
     * curated set to AccessibilityService global actions and ignore
     * the rest. Arbitrary character input (`KEY_A` … `KEY_Z`,
     * punctuation, digits) can only be injected by an IME on
     * non-rooted Android, so we drop them with a debug log instead of
     * faking jittery taps on the on-screen keyboard.
     *
     * Acts on key-down only — performing the same global action twice
     * (down + up) would fire `BACK` etc twice per press.
     */
    private fun dispatchKey(k: WireInputMessage.KeyPress) {
        if (!k.pressed) return
        // Codes that map to a global action regardless of OS level.
        val baseAction: Int? = when (k.keycode.toInt()) {
            1 /* KEY_ESC */, 158 /* KEY_BACK */ -> GLOBAL_ACTION_BACK
            102 /* KEY_HOME */, 125 /* KEY_LEFTMETA */, 126 /* KEY_RIGHTMETA */ ->
                GLOBAL_ACTION_HOME
            580 /* KEY_APPSELECT */, 0x244 /* KEY_TASK_SWITCH */ ->
                GLOBAL_ACTION_RECENTS
            116 /* KEY_POWER */ -> GLOBAL_ACTION_POWER_DIALOG
            else -> null
        }
        if (baseAction != null) {
            performGlobalAction(baseAction)
            return
        }
        // API 33+ ships DPAD globals so arrow keys + Enter can drive
        // focus traversal on TV / leanback-style content.
        if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.TIRAMISU) {
            val dpad = when (k.keycode.toInt()) {
                103 /* KEY_UP */ -> GLOBAL_ACTION_DPAD_UP
                108 /* KEY_DOWN */ -> GLOBAL_ACTION_DPAD_DOWN
                105 /* KEY_LEFT */ -> GLOBAL_ACTION_DPAD_LEFT
                106 /* KEY_RIGHT */ -> GLOBAL_ACTION_DPAD_RIGHT
                28 /* KEY_ENTER */ -> GLOBAL_ACTION_DPAD_CENTER
                else -> null
            }
            if (dpad != null) {
                performGlobalAction(dpad)
                return
            }
        }
        Log.d(TAG, "ignored unmapped key: ${k.keycode}")
    }

    // Touch pipeline:
    //
    //   * Cursor overlay tracks the host pointer in real time so the
    //     operator sees their PC mouse move with zero perceived
    //     latency.
    //
    //   * The actual `dispatchGesture` is BUFFERED. We collect every
    //     (x, y, tMs) sample we receive while the finger is "down"
    //     and dispatch ONE atomic gesture on release. Yes, this means
    //     the receiving app sees no touch motion until the user lifts
    //     — but the gesture that fires is a real continuous drag
    //     (single ACTION_DOWN → many ACTION_MOVE → ACTION_UP), which
    //     is what apps actually need. The sequential / continueStroke
    //     chain experiments turned every move into a fresh tap on
    //     Lenovo's accessibility skin, which is much worse.
    //
    // Visual feedback (cursor) + correct-but-delayed gesture gives
    // the closest UX to scrcpy that's possible without root.
    private data class TouchPoint(val x: Float, val y: Float, val tMs: Long)
    private val touchPoints = mutableListOf<TouchPoint>()
    private var touchStartTime: Long = 0L

    private fun dispatchTouch(t: WireInputMessage.TouchSlot) {
        val release = t.trackingId < 0
        val (sx, sy) = scaleToDisplay(t.x.toFloat(), t.y.toFloat())
        val x = sx.coerceAtLeast(0f)
        val y = sy.coerceAtLeast(0f)
        val now = android.os.SystemClock.uptimeMillis()

        // Cursor always tracks the host pointer in real time,
        // regardless of whether a gesture is in flight.
        cursor?.let {
            if (release) {
                it.setPressed(false)
                it.hide()
            } else {
                it.move(x, y)
                it.setPressed(true)
            }
        }

        if (release) {
            if (touchPoints.isEmpty()) return
            buildAndDispatch()
            touchPoints.clear()
            return
        }
        if (touchPoints.isEmpty()) {
            touchStartTime = now
        }
        // Drop sub-pixel duplicates so the path doesn't blow up on
        // micro-jitter from the host's pointer normalisation.
        val last = touchPoints.lastOrNull()
        if (last != null && kotlin.math.abs(last.x - x) < 1f && kotlin.math.abs(last.y - y) < 1f) {
            return
        }
        touchPoints.add(TouchPoint(x, y, now - touchStartTime))
    }

    private fun buildAndDispatch() {
        val pts = touchPoints
        if (pts.isEmpty()) return
        val first = pts.first()
        val path = Path().apply { moveTo(first.x, first.y) }
        if (pts.size == 1) {
            // Pure tap: 1-pixel nudge so the path has non-zero
            // geometry (Android's gesture validator rejects degenerate
            // paths outright on some OEM builds).
            path.lineTo(first.x + 1f, first.y)
        } else {
            for (i in 1 until pts.size) {
                val p = pts[i]
                val prev = pts[i - 1]
                val px = if (p.x == prev.x && p.y == prev.y) p.x + 1f else p.x
                path.lineTo(px, p.y)
            }
        }
        val duration = if (pts.size == 1) {
            TAP_DURATION_MS
        } else {
            (pts.last().tMs - first.tMs).coerceAtLeast(MIN_DRAG_DURATION_MS)
        }
        try {
            val stroke = GestureDescription.StrokeDescription(path, 0L, duration)
            dispatchGesture(
                GestureDescription.Builder().addStroke(stroke).build(),
                null,
                null,
            )
        } catch (e: IllegalArgumentException) {
            Log.w(TAG, "dispatchGesture rejected path (pts=${pts.size})", e)
        }
    }

    /**
     * Translate `(frameX, frameY)` from the host's capture pixel
     * grid to the device's real display pixel grid. Reads the
     * actual capture size from [CaptureSession.lastCaptureWidth] /
     * [CaptureSession.lastCaptureHeight] each call so a mid-session
     * `start()` with a different aspect doesn't strand stale
     * dimensions here. `DisplayMetrics` is cached because re-asking
     * WindowManager every tick is expensive.
     */
    private fun scaleToDisplay(frameX: Float, frameY: Float): Pair<Float, Float> {
        val metrics = displayMetrics ?: run {
            val wm = getSystemService(Context.WINDOW_SERVICE) as? WindowManager
            val dm = DisplayMetrics().also {
                @Suppress("DEPRECATION")
                wm?.defaultDisplay?.getRealMetrics(it)
            }
            displayMetrics = dm
            dm
        }
        val captureW = CaptureSession.lastCaptureWidth.coerceAtLeast(1)
        val captureH = CaptureSession.lastCaptureHeight.coerceAtLeast(1)
        val sx = frameX * metrics.widthPixels.toFloat() / captureW.toFloat()
        val sy = frameY * metrics.heightPixels.toFloat() / captureH.toFloat()
        return sx to sy
    }

    private var displayMetrics: DisplayMetrics? = null

    companion object {
        private const val TAG = "ansync.access"
        private const val RETRY_DELAY_MS = 500L
        // Pure-tap duration. Long enough for touch consumers to
        // register the press without it being mistaken for a long
        // press.
        private const val TAP_DURATION_MS = 60L
        // Floor on drag duration so a very fast flick still produces
        // an interpolatable swipe instead of a sub-frame no-op.
        private const val MIN_DRAG_DURATION_MS = 30L

        @Volatile
        private var INSTANCE: AnsyncAccessibilityService? = null

        fun current(): AnsyncAccessibilityService? = INSTANCE
    }
}
