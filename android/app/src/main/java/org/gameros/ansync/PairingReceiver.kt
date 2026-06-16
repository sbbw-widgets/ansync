package org.gameros.ansync

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log
import androidx.core.app.NotificationCompat

/**
 * Cable-pairing entry point invoked by the host via:
 *
 *   adb shell am broadcast \
 *     -a org.gameros.ansync.action.PAIR \
 *     --ei port <PORT> \
 *     --es name <host_name> \
 *     -n org.gameros.ansync/.PairingReceiver
 *
 * Android 14+ blocks Background Activity Launch from Receivers
 * (`goo.gle/android-bal`) regardless of OEM. The fix is a
 * heads-up notification whose `contentIntent` PendingIntent carries
 * the activity launch — the user's tap on the notification counts as
 * consent and bypasses BAL. UX cost: one tap.
 */
class PairingReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != ACTION_PAIR) return
        val port = intent.getIntExtra("port", -1)
        if (port <= 0 || port > 65_535) {
            Log.w(TAG, "PAIR intent missing valid port extra")
            return
        }
        val hostName = intent.getStringExtra("name") ?: "host"
        Log.i(TAG, "pair request from host '$hostName' on port $port — posting notif")
        postPairNotification(context, port, hostName)
    }

    private fun postPairNotification(context: Context, port: Int, hostName: String) {
        ensureChannel(context)
        val launch = Intent(context, PairingActivity::class.java).apply {
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK)
            putExtra(PairingActivity.EXTRA_PORT, port)
            putExtra(PairingActivity.EXTRA_HOST_NAME, hostName)
        }
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        } else {
            PendingIntent.FLAG_UPDATE_CURRENT
        }
        val pi = PendingIntent.getActivity(context, port, launch, flags)
        val n = NotificationCompat.Builder(context, CHANNEL_ID)
            .setContentTitle("Pair with $hostName?")
            .setContentText("Tap to finish pairing the ansync host.")
            .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setCategory(NotificationCompat.CATEGORY_RECOMMENDATION)
            .setContentIntent(pi)
            .setFullScreenIntent(pi, true)
            .setAutoCancel(true)
            .build()
        val mgr = context.getSystemService(NotificationManager::class.java) ?: return
        mgr.notify(NOTIFICATION_ID, n)
    }

    private fun ensureChannel(context: Context) {
        val mgr = context.getSystemService(NotificationManager::class.java) ?: return
        if (mgr.getNotificationChannel(CHANNEL_ID) != null) return
        val ch = NotificationChannel(
            CHANNEL_ID,
            "ansync pair requests",
            NotificationManager.IMPORTANCE_HIGH,
        ).apply {
            description = "Heads-up when the host asks to pair with this device"
        }
        mgr.createNotificationChannel(ch)
    }

    companion object {
        private const val TAG = "ansync.pair"
        const val ACTION_PAIR = "org.gameros.ansync.action.PAIR"
        const val PREF_HOST_PUBKEY_HEX = "host_pubkey_hex"
        const val PREF_HOST_NAME = "host_name"
        private const val CHANNEL_ID = "ansync.pair"
        private const val NOTIFICATION_ID = 4242
    }
}
