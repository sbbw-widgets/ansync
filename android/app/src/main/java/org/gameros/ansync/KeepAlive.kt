package org.gameros.ansync

import android.content.Context
import android.net.wifi.WifiManager
import android.os.Build
import android.os.PowerManager
import android.util.Log

/**
 * Process-wide "stay alive" handles the companion service holds for
 * the duration of its lifetime.
 *
 * Android's defaults push idle apps toward two failure modes:
 *
 *   * **Doze**: screen off + device stationary for ~30 min defers
 *     background CPU + network. Even a foreground service feels the
 *     Wi-Fi radio go to power-save, killing QUIC keep-alive within a
 *     few minutes. Battery-whitelisting the app fixes this, but the
 *     user has to grant the exemption explicitly.
 *
 *   * **WifiManager idle**: the Wi-Fi chip can independently drop to
 *     low-power scan mode after a few seconds of socket inactivity,
 *     even when CPU is awake. A high-perf `WifiLock` keeps the radio
 *     in the full-power state.
 *
 * Holding these locks for the whole foreground-service lifetime is
 * what scrcpy-style "the phone is always reachable" UX requires.
 * Battery cost is in line with any other persistent network app
 * (Tailscale, RustDesk, etc.) — measurable but small versus the
 * value of "PC sees the phone instantly".
 */
class KeepAlive(private val ctx: Context) {
    private var wifiLock: WifiManager.WifiLock? = null

    fun acquire() {
        if (wifiLock != null) return
        val wm = ctx.applicationContext.getSystemService(WifiManager::class.java) ?: return
        val mode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            // Q+ introduced a lower-latency tier that's better suited
            // to interactive sessions (mirror, input). Fall through to
            // HIGH_PERF on older releases.
            WifiManager.WIFI_MODE_FULL_LOW_LATENCY
        } else {
            @Suppress("DEPRECATION")
            WifiManager.WIFI_MODE_FULL_HIGH_PERF
        }
        val lock = wm.createWifiLock(mode, TAG)
        lock.setReferenceCounted(false)
        try {
            lock.acquire()
            wifiLock = lock
            Log.i(TAG, "wifi lock acquired (mode=$mode)")
        } catch (t: Throwable) {
            Log.w(TAG, "wifi lock acquire failed", t)
        }
    }

    fun release() {
        wifiLock?.let {
            if (it.isHeld) {
                try { it.release() } catch (_: Throwable) {}
            }
        }
        wifiLock = null
    }

    companion object {
        private const val TAG = "ansync.keepalive"

        /**
         * `true` when the user has granted the doze whitelist. The
         * setup wizard polls this — once it flips true the step is
         * removed and the persistent setup notif disappears.
         */
        fun isBatteryWhitelisted(ctx: Context): Boolean {
            val pm = ctx.getSystemService(PowerManager::class.java) ?: return false
            return pm.isIgnoringBatteryOptimizations(ctx.packageName)
        }
    }
}
