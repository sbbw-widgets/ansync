package org.gameros.ansync

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log

/**
 * Re-launches [AnsyncCompanionService] after the device boots or the
 * package gets updated. Skips when no host pubkey is persisted — a
 * fresh install with no prior pair has no peer to talk to, so we
 * stay dormant until the user runs `ansyncctl pair` and
 * [PairingReceiver] kicks us off.
 *
 * Triggered by:
 *   * [Intent.ACTION_BOOT_COMPLETED] — device cold start
 *   * [Intent.ACTION_MY_PACKAGE_REPLACED] — companion APK upgrade
 *   * [Intent.ACTION_LOCKED_BOOT_COMPLETED] — direct-boot path, fires
 *     before user unlock; we still start so the service can dial as
 *     soon as the network comes up
 */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val action = intent.action ?: return
        if (action != Intent.ACTION_BOOT_COMPLETED
            && action != Intent.ACTION_LOCKED_BOOT_COMPLETED
            && action != Intent.ACTION_MY_PACKAGE_REPLACED
        ) {
            return
        }
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val hex = prefs.getString(PairingReceiver.PREF_HOST_PUBKEY_HEX, null)
        if (hex.isNullOrEmpty()) {
            Log.i(TAG, "boot but no paired host — staying dormant")
            return
        }
        Log.i(TAG, "boot ($action): re-launching companion service for $hex")
        val svc = Intent(context, AnsyncCompanionService::class.java)
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(svc)
            } else {
                context.startService(svc)
            }
        } catch (e: Exception) {
            Log.w(TAG, "service start failed: $e")
        }
    }

    companion object {
        private const val TAG = "ansync.boot"
    }
}
