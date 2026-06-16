package org.gameros.ansync

import android.content.ClipData
import android.content.ClipboardManager
import android.content.ContentValues
import android.content.Context
import android.provider.MediaStore
import android.util.Log
import java.nio.ByteBuffer
import java.nio.ByteOrder
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
    private var textThread: Thread? = null
    private var blobThread: Thread? = null

    fun start() {
        if (running) return
        running = true
        textThread = thread(name = "ansync-clipboard-text") {
            val mgr = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            while (running) {
                val text = NativeBridge.nativePollClipboardText()
                if (text == null) {
                    // Session not yet wired or peer dropped — back off
                    // + retry rather than killing the bridge.
                    try { Thread.sleep(500) } catch (_: InterruptedException) {}
                    continue
                }
                val clip = ClipData.newPlainText("ansync", text)
                try {
                    mgr.setPrimaryClip(clip)
                } catch (e: Exception) {
                    Log.w(TAG, "setPrimaryClip text threw", e)
                }
            }
        }
        blobThread = thread(name = "ansync-clipboard-blob") {
            val mgr = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            while (running) {
                val blob = NativeBridge.nativePollClipboardBlob()
                if (blob == null) {
                    try { Thread.sleep(500) } catch (_: InterruptedException) {}
                    continue
                }
                val (mime, data) = decodeBlob(blob) ?: continue
                if (!mime.startsWith("image/")) {
                    Log.w(TAG, "ignoring non-image blob mime=$mime")
                    continue
                }
                try {
                    publishImage(mgr, mime, data)
                } catch (e: Exception) {
                    Log.w(TAG, "publishImage threw mime=$mime", e)
                }
            }
        }
    }

    fun stop() {
        running = false
        textThread?.join(TIMEOUT_JOIN_MS)
        blobThread?.join(TIMEOUT_JOIN_MS)
        textThread = null
        blobThread = null
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
        val item = clip.getItemAt(0)
        val uri = item.uri
        val mime = clip.description.getMimeType(0)
        if (uri != null && mime != null && mime.startsWith("image/")) {
            val data = try {
                context.contentResolver.openInputStream(uri)?.use { it.readBytes() }
            } catch (e: Exception) {
                Log.w(TAG, "openInputStream threw uri=$uri", e)
                null
            } ?: return false
            return NativeBridge.nativeSendClipboardBlob(mime, data)
        }
        val text = item.coerceToText(context)?.toString() ?: return false
        return NativeBridge.nativeSendClipboardText(text)
    }

    private fun decodeBlob(buf: ByteArray): Pair<String, ByteArray>? {
        if (buf.size < 4) return null
        val mimeLen = ByteBuffer.wrap(buf, 0, 4).order(ByteOrder.LITTLE_ENDIAN).int
        if (mimeLen < 0 || 4 + mimeLen > buf.size) return null
        val mime = String(buf, 4, mimeLen, Charsets.UTF_8)
        val data = buf.copyOfRange(4 + mimeLen, buf.size)
        return mime to data
    }

    private fun publishImage(mgr: ClipboardManager, mime: String, data: ByteArray) {
        val ext = when (mime) {
            "image/png" -> "png"
            "image/jpeg", "image/jpg" -> "jpg"
            "image/webp" -> "webp"
            "image/gif" -> "gif"
            else -> "bin"
        }
        val values = ContentValues().apply {
            put(MediaStore.Images.Media.DISPLAY_NAME, "ansync-${System.currentTimeMillis()}.$ext")
            put(MediaStore.Images.Media.MIME_TYPE, mime)
        }
        val resolver = context.contentResolver
        val uri = resolver.insert(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, values)
            ?: run {
                Log.w(TAG, "MediaStore.insert returned null mime=$mime")
                return
            }
        resolver.openOutputStream(uri)?.use { it.write(data) }
        val clip = ClipData.newUri(resolver, "ansync", uri)
        mgr.setPrimaryClip(clip)
    }

    companion object {
        private const val TAG = "ansync.clip"
        private const val TIMEOUT_JOIN_MS = 1_000L
    }
}
