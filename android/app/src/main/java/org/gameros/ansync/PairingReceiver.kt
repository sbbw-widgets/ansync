package org.gameros.ansync

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log
import kotlin.concurrent.thread

/**
 * Cable-pairing entry point invoked by the host via:
 *
 *   adb shell am broadcast \
 *     -a org.gameros.ansync.action.PAIR \
 *     --ei port <PORT> \
 *     -n org.gameros.ansync/.PairingReceiver
 *
 * The host has already run `adb reverse tcp:PORT tcp:PORT`, so a
 * TCP dial to `127.0.0.1:PORT` from inside the companion lands on
 * the host's pairing listener. The cable is the security guarantee
 * — no in-app prompt; we just dial, exchange Ed25519 pubkeys, and
 * persist the host's pubkey for future connects.
 */
class PairingReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != ACTION_PAIR) return
        val port = intent.getIntExtra("port", -1)
        if (port <= 0 || port > 65_535) {
            Log.w(TAG, "PAIR intent missing valid port extra")
            return
        }
        val name = intent.getStringExtra("name") ?: Build.MODEL ?: "Android"
        val pending = goAsync()
        thread(name = "ansync-pair") {
            try {
                // Native side requires nativeInit() before pairing.
                NativeBridge.nativeInit(context.filesDir.absolutePath)
                val response = NativeBridge.nativePairOverCable(port, name)
                if (response == null) {
                    Log.w(TAG, "pairing failed: nativePairOverCable returned null")
                    return@thread
                }
                val (hex, hostName) = response.split('|', limit = 2)
                    .let { it[0] to (it.getOrNull(1) ?: "host") }
                persistPairedHost(context, hex, hostName)
                Log.i(TAG, "paired with host '$hostName' pubkey=${hex.take(8)}…")
            } catch (e: Exception) {
                Log.w(TAG, "pairing threw", e)
            } finally {
                pending.finish()
            }
        }
    }

    private fun persistPairedHost(context: Context, hex: String, name: String) {
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putString(PREF_HOST_PUBKEY_HEX, hex)
            .putString(PREF_HOST_NAME, name)
            .apply()
    }

    companion object {
        private const val TAG = "ansync.pair"
        const val ACTION_PAIR = "org.gameros.ansync.action.PAIR"
        const val PREF_HOST_PUBKEY_HEX = "host_pubkey_hex"
        const val PREF_HOST_NAME = "host_name"
    }
}
