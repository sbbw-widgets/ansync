package org.gameros.ansync

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat

/**
 * Foreground service hosting the companion's persistent workers:
 *   - QUIC client to the paired host (Step 7d)
 *   - MediaProjection capture loop + MediaCodec H.264 encoder (Step 7d)
 *   - Audio routing (Step 11)
 *
 * Step 7c ships the lifecycle skeleton: register a notification
 * channel, raise the foreground notification, idle. Step 7d wires
 * the real worker threads inside `onStartCommand`.
 */
class AnsyncCompanionService : Service() {
    override fun onCreate() {
        super.onCreate()
        ensureChannel(this)
        NativeBridge.nativeInit()
    }

    override fun onDestroy() {
        NativeBridge.nativeClose()
        super.onDestroy()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val notification = buildNotification(this)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION,
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        const val CHANNEL_ID = "ansync.companion"
        const val NOTIFICATION_ID = 1

        private fun ensureChannel(ctx: Context) {
            val mgr = ctx.getSystemService(NotificationManager::class.java) ?: return
            if (mgr.getNotificationChannel(CHANNEL_ID) != null) return
            val ch = NotificationChannel(
                CHANNEL_ID,
                "ansync companion",
                NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = "Persistent capture + transport for the paired host"
            }
            mgr.createNotificationChannel(ch)
        }

        private fun buildNotification(ctx: Context): Notification =
            NotificationCompat.Builder(ctx, CHANNEL_ID)
                .setContentTitle("ansync connected")
                .setContentText("Capture + remote input active")
                .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
                .setOngoing(true)
                .build()
    }
}
