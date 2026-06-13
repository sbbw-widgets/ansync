package org.gameros.ansync

import android.accessibilityservice.AccessibilityService
import android.view.KeyEvent
import android.view.accessibility.AccessibilityEvent

/**
 * Reverse-input handler: receives gesture / key dispatch from the
 * paired host and replays them on this device.
 *
 * The host pushes `proto::InputMessage` packets over the QUIC
 * `Input` stream → companion service → this AccessibilityService.
 * Step 7c ships the lifecycle stub; Step 7e wires `dispatchGesture`
 * for touch + `performGlobalAction` for system navigation.
 */
class AnsyncAccessibilityService : AccessibilityService() {

    override fun onServiceConnected() {
        super.onServiceConnected()
        INSTANCE = this
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {
        // No-op: we are a write-only consumer of dispatchGesture +
        // performGlobalAction. canRetrieveWindowContent=false in the
        // service config rejects this stream of events outright.
    }

    override fun onInterrupt() {
        // Required override. Nothing to cancel here.
    }

    override fun onKeyEvent(event: KeyEvent?): Boolean {
        // Step 7e routes synthesised key events from the host into
        // here. For now, defer to the system handler.
        return false
    }

    override fun onUnbind(intent: android.content.Intent?): Boolean {
        INSTANCE = null
        return super.onUnbind(intent)
    }

    companion object {
        /**
         * Backing field for [current]. AccessibilityService instances
         * are owned by the system; we surface a static handle so the
         * companion service can call `dispatchGesture` on the live
         * one without inventing an IPC layer.
         */
        @Volatile
        private var INSTANCE: AnsyncAccessibilityService? = null

        fun current(): AnsyncAccessibilityService? = INSTANCE
    }
}
