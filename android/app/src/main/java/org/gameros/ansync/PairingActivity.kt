package org.gameros.ansync

import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.widget.Toast
import androidx.activity.ComponentActivity
import kotlin.concurrent.thread

/**
 * Translucent activity that owns the cable-pairing bootstrap.
 *
 * Why an Activity instead of letting [PairingReceiver] do the work:
 *   * BroadcastReceivers are killed after ~10 s (~60 s with
 *     `goAsync`). The native pair handshake waits up to 60 s for the
 *     host TCP listener to accept; that overlap triggers an ANR.
 *   * Activities have a foreground-eligible context so
 *     `startForegroundService` works without
 *     `ForegroundServiceStartNotAllowedException`.
 *   * `am broadcast` only opens a ~10 s background-activity-start
 *     window. Launching us synchronously from the receiver uses that
 *     window before it closes.
 *
 * Intent extras (set by [PairingReceiver]):
 *   * [EXTRA_PORT] (int)        — TCP port the host has already
 *                                 `adb reverse`d.
 *   * [EXTRA_HOST_NAME] (string) — display name announced by the host.
 */
class PairingActivity : ComponentActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val port = intent.getIntExtra(EXTRA_PORT, -1)
        val hostName = intent.getStringExtra(EXTRA_HOST_NAME) ?: "host"
        if (port <= 0 || port > 65_535) {
            Log.w(TAG, "missing/invalid port extra")
            finish()
            return
        }
        val companionName = "${Build.MANUFACTURER} ${Build.MODEL}"
        Toast.makeText(this, "Pairing with $hostName…", Toast.LENGTH_SHORT).show()

        thread(name = "ansync-pair-activity") {
            try {
                NativeBridge.nativeInit(filesDir.absolutePath)
                NativeBridge.nativeSetDeviceName(companionName)
                val response = NativeBridge.nativePairOverCable(port, companionName)
                if (response == null) {
                    Log.w(TAG, "nativePairOverCable returned null")
                    finishOnMain()
                    return@thread
                }
                val parts = response.split('|', limit = 3)
                val hex = parts[0]
                val resolvedHost = parts.getOrNull(1) ?: hostName
                val endpoints = parts.getOrNull(2).orEmpty()
                persistPairedHost(hex, resolvedHost, endpoints)
                Log.i(
                    TAG,
                    "paired with host '$resolvedHost' pubkey=${hex.take(8)}… endpoints=[$endpoints]",
                )
                Handler(Looper.getMainLooper()).post {
                    Toast.makeText(
                        this, "Paired with $resolvedHost", Toast.LENGTH_SHORT
                    ).show()
                    startCompanionServiceAndNext()
                }
            } catch (e: Exception) {
                Log.w(TAG, "pair threw", e)
                finishOnMain()
            }
        }
    }

    private fun persistPairedHost(hex: String, name: String, endpoints: String) {
        getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putString(PairingReceiver.PREF_HOST_PUBKEY_HEX, hex)
            .putString(PairingReceiver.PREF_HOST_NAME, name)
            .putString(PREF_HOST_ADDR, endpoints)
            .apply()
    }

    /**
     * Start the companion service from this Activity context
     * (foreground-eligible — works around Android 14+ BAL block on
     * Receivers). The service `onCreate` posts the persistent
     * [SetupNotif] which guides the user through any pending grants
     * one at a time from the notification shade. No further Activity
     * chaining: the user picks whether/when to walk the setup notif.
     */
    private fun startCompanionServiceAndNext() {
        val svc = Intent(this, AnsyncCompanionService::class.java)
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                startForegroundService(svc)
            } else {
                startService(svc)
            }
        } catch (e: Exception) {
            Log.w(TAG, "service start failed: $e")
        }
        finish()
    }

    private fun finishOnMain() {
        Handler(Looper.getMainLooper()).post { finish() }
    }

    companion object {
        private const val TAG = "ansync.pair"
        const val EXTRA_PORT = "org.gameros.ansync.extra.PAIR_PORT"
        const val EXTRA_HOST_NAME = "org.gameros.ansync.extra.PAIR_HOST_NAME"
    }
}
