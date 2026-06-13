package org.gameros.ansync

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.graphics.Path
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.KeyEvent
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

    override fun onServiceConnected() {
        super.onServiceConnected()
        INSTANCE = this
        startPolling()
    }

    override fun onUnbind(intent: android.content.Intent?): Boolean {
        stopPolling()
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
            is WireInputMessage.KeyPress -> { /* Step 7e+ — system key replay via performGlobalAction map */ }
            is WireInputMessage.Stylus, is WireInputMessage.MouseMove,
            is WireInputMessage.MouseButton, is WireInputMessage.MouseWheel,
            is WireInputMessage.Gamepad -> {
                // Out-of-band on Android: the host shouldn't push these
                // back to the device. Drop silently.
            }
        }
    }

    private fun dispatchTouch(t: WireInputMessage.TouchSlot) {
        // Single-finger swipe: synthesise a 16 ms gesture from the
        // (x,y) point. The host already dedupes per-slot motion, so
        // multi-finger follow-up lands in Step 7e+ once we track
        // active slots into composite gestures.
        if (t.trackingId < 0) return
        val path = Path().apply { moveTo(t.x.toFloat(), t.y.toFloat()) }
        val stroke = GestureDescription.StrokeDescription(path, 0L, GESTURE_DURATION_MS)
        val gesture = GestureDescription.Builder().addStroke(stroke).build()
        dispatchGesture(gesture, null, null)
    }

    companion object {
        private const val TAG = "ansync.access"
        private const val RETRY_DELAY_MS = 500L
        private const val GESTURE_DURATION_MS = 16L

        @Volatile
        private var INSTANCE: AnsyncAccessibilityService? = null

        fun current(): AnsyncAccessibilityService? = INSTANCE
    }
}
