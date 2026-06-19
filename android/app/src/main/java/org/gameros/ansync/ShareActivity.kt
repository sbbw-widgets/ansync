package org.gameros.ansync

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.OpenableColumns
import android.util.Log
import android.util.Patterns
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.result.contract.ActivityResultContracts
import java.io.File
import java.io.FileOutputStream
import kotlin.concurrent.thread

/**
 * Receives external share intents (`ACTION_SEND` /
 * `ACTION_SEND_MULTIPLE`) and dispatches the payload to the paired
 * host via [NativeBridge].
 *
 * The activity is translucent — it shows nothing on its own; results
 * are surfaced through toasts so the share-sheet flow feels native to
 * any sender. Future work (N8: multi-host companion) will gain a
 * picker; today there is exactly one paired host, so we just send.
 *
 * URLs: detected with [Patterns.WEB_URL] on `EXTRA_TEXT`. Non-URL
 * text is dropped with a "use clipboard sync instead" toast — text
 * is already covered by `ClipboardBridge` and bridging the two
 * surfaces was out of scope for the share feature.
 *
 * Files: each `EXTRA_STREAM` URI is copied into a per-share temp
 * file under `cacheDir/share/` because the Rust side ([send_file]
 * in `ansync-files`) reads from a filesystem path. The temp file is
 * deleted after the native call returns.
 *
 * Launching with no intent extras (the QSTile path) opens an
 * `ACTION_GET_CONTENT` chooser so the user can pick any file
 * without leaving Quick Settings.
 */
class ShareActivity : ComponentActivity() {

    private val pickContent = registerForActivityResult(
        ActivityResultContracts.GetContent()
    ) { uri: Uri? ->
        if (uri == null) {
            finish()
            return@registerForActivityResult
        }
        dispatchFile(uri)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val incoming = intent
        when (incoming?.action) {
            Intent.ACTION_SEND -> handleSend(incoming)
            Intent.ACTION_SEND_MULTIPLE -> handleSendMultiple(incoming)
            else -> {
                // QSTile launch → pop an arbitrary-file picker. We
                // can't pre-pick a host because we currently know one.
                pickContent.launch("*/*")
            }
        }
    }

    private fun handleSend(intent: Intent) {
        val text = intent.getStringExtra(Intent.EXTRA_TEXT)
        if (text != null && Patterns.WEB_URL.matcher(text).matches()) {
            dispatchUrl(text)
            return
        }
        if (text != null) {
            toastAndFinish(
                "Use clipboard sync for non-URL text — share only handles files + links.",
            )
            return
        }
        val uri: Uri? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableExtra(Intent.EXTRA_STREAM)
        }
        if (uri == null) {
            toastAndFinish("Share: nothing to send.")
            return
        }
        dispatchFile(uri)
    }

    private fun handleSendMultiple(intent: Intent) {
        val uris: ArrayList<Uri>? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM)
        }
        if (uris.isNullOrEmpty()) {
            toastAndFinish("Share: nothing to send.")
            return
        }
        thread(name = "ansync-share-multi") {
            var sent = 0
            for (uri in uris) {
                if (sendOne(uri)) sent += 1
            }
            uiToast("Share: sent $sent/${uris.size} files to $hostLabel.")
            runOnUiThread { finish() }
        }
    }

    private fun dispatchUrl(url: String) {
        thread(name = "ansync-share-url") {
            val ok = try {
                NativeBridge.nativeSendUrl(url)
            } catch (t: Throwable) {
                Log.w(TAG, "nativeSendUrl threw", t)
                false
            }
            uiToast(
                if (ok) "Share: link sent to $hostLabel."
                else "Share: send failed — host offline?",
            )
            runOnUiThread { finish() }
        }
    }

    private fun dispatchFile(uri: Uri) {
        thread(name = "ansync-share-file") {
            val ok = sendOne(uri)
            uiToast(
                if (ok) "Share: file sent to $hostLabel."
                else "Share: send failed — host offline?",
            )
            runOnUiThread { finish() }
        }
    }

    /**
     * Copy [uri] into a cache file and hand the path to native.
     * Cleans the temp file regardless of outcome. Returns whether
     * the native side reported success.
     */
    private fun sendOne(uri: Uri): Boolean {
        val name = queryDisplayName(uri) ?: "shared-${System.currentTimeMillis()}"
        val cacheRoot = File(cacheDir, "share").apply { mkdirs() }
        val tmp = File(cacheRoot, "${System.nanoTime()}-$name")
        try {
            contentResolver.openInputStream(uri)?.use { input ->
                FileOutputStream(tmp).use { output ->
                    input.copyTo(output)
                }
            } ?: return false
        } catch (t: Throwable) {
            Log.w(TAG, "share: copy uri to cache failed", t)
            return false
        }
        return try {
            NativeBridge.nativeSendFile(tmp.absolutePath)
        } catch (t: Throwable) {
            Log.w(TAG, "share: nativeSendFile threw", t)
            false
        } finally {
            tmp.delete()
        }
    }

    private fun queryDisplayName(uri: Uri): String? {
        if (uri.scheme == "file") return uri.lastPathSegment
        return try {
            contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
                ?.use { c ->
                    if (c.moveToFirst()) {
                        val idx = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                        if (idx >= 0) c.getString(idx) else null
                    } else null
                }
        } catch (t: Throwable) {
            Log.w(TAG, "queryDisplayName threw", t)
            null
        }
    }

    private val hostLabel: String
        get() = getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getString(PairingReceiver.PREF_HOST_NAME, null) ?: "host"

    private fun uiToast(text: String) {
        runOnUiThread {
            Toast.makeText(this, text, Toast.LENGTH_SHORT).show()
        }
    }

    private fun toastAndFinish(text: String) {
        Toast.makeText(this, text, Toast.LENGTH_SHORT).show()
        finish()
    }

    companion object {
        private const val TAG = "ansync.share"
    }
}
