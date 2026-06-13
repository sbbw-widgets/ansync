package org.gameros.ansync

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.util.Log
import kotlin.concurrent.thread

/**
 * Drains the host→device clipboard channel into the Android
 * `ClipboardManager`. Started by `AnsyncCompanionService`; stops
 * when the JNI poll returns `null` (session torn down).
 *
 * Privacy: every paste lands in the Android global clipboard, so
 * the user's other apps will see it. The host side already gates
 * outbound clipboard on `Permission::ClipboardOut`, so a peer can't
 * push without an explicit "Allow" earlier.
 */
class ClipboardBridge(private val context: Context) {
    @Volatile private var running = false
    private var thread: Thread? = null

    fun start() {
        if (running) return
        running = true
        thread = thread(name = "ansync-clipboard") {
            val mgr = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            while (running) {
                val text = NativeBridge.nativePollClipboardText() ?: return@thread
                val clip = ClipData.newPlainText("ansync", text)
                try {
                    mgr.setPrimaryClip(clip)
                } catch (e: Exception) {
                    Log.w(TAG, "setPrimaryClip threw", e)
                }
            }
        }
    }

    fun stop() {
        running = false
        thread?.join(TIMEOUT_JOIN_MS)
        thread = null
    }

    /**
     * Push the device's current clipboard text to the host. Returns
     * `false` if the clipboard is empty or non-text — callers should
     * surface that to the UI rather than retry.
     */
    fun pushToHost(): Boolean {
        val mgr = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        val clip = mgr.primaryClip ?: return false
        if (clip.itemCount == 0) return false
        val text = clip.getItemAt(0).coerceToText(context)?.toString() ?: return false
        return NativeBridge.nativeSendClipboardText(text)
    }

    companion object {
        private const val TAG = "ansync.clip"
        private const val TIMEOUT_JOIN_MS = 1_000L
    }
}
