package org.gameros.ansync

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.graphics.BitmapFactory
import android.media.MediaMetadata
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.os.Build
import android.support.v4.media.session.MediaSessionCompat
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.media.app.NotificationCompat as MediaStyle

/**
 * MediaSession-backed widget for the active mirror (screen-share)
 * session.
 *
 * Mirrors the pattern set by [AudioMediaSession]: a stateful
 * `MediaSession` flagged `PLAYING` so Android surfaces it in the
 * lock-screen media area, in the notif shade media section, and on
 * any connected smartwatch / car system. The single button is
 * "Stop", which funnels through `AnsyncCompanionService` so the
 * teardown path matches the QSTile + persistent-notif equivalents.
 *
 * The widget is *additive* to the existing persistent companion
 * notification — power users keep the dense per-stream action grid;
 * casual users get a one-tap stop from the lock screen without
 * pulling the shade.
 *
 * No `AudioFocusRequest` here: mirror is not audio output. Headset
 * play / pause / stop keys still route through us though, since
 * `FLAG_HANDLES_MEDIA_BUTTONS` is on — the user explicitly chose to
 * mirror, hardware "stop" should win.
 */
class MirrorMediaSession(
    private val svc: AnsyncCompanionService,
    private val hostLabel: String,
) {

    private var session: MediaSession? = null

    fun start() {
        ensureChannel()
        val mediaSession = session ?: MediaSession(svc, TAG).apply {
            setFlags(
                MediaSession.FLAG_HANDLES_MEDIA_BUTTONS
                    or MediaSession.FLAG_HANDLES_TRANSPORT_CONTROLS,
            )
            setCallback(object : MediaSession.Callback() {
                override fun onStop() {
                    stopMirrorViaService()
                }
                override fun onPause() {
                    // Headset "pause" → treat as stop. Mirror has no
                    // pause primitive on Android — MediaProjection is
                    // all-or-nothing — so a half-stop would be lying
                    // to the user.
                    stopMirrorViaService()
                }
            })
            isActive = true
        }
        session = mediaSession
        updateMetadata()
        updatePlaybackState(PlaybackState.STATE_PLAYING)
        postNotification()
    }

    /** Tear the session + notif down. Safe to call when nothing is running. */
    fun release() {
        try {
            session?.isActive = false
            session?.release()
        } catch (e: Exception) {
            Log.w(TAG, "MediaSession release threw", e)
        }
        session = null
        try {
            svc.getSystemService(NotificationManager::class.java)
                ?.cancel(NOTIFICATION_ID)
        } catch (e: Exception) {
            Log.w(TAG, "cancel mirror media notif threw", e)
        }
    }

    private fun stopMirrorViaService() {
        val i = Intent(svc, AnsyncCompanionService::class.java)
            .setAction(AnsyncCompanionService.ACTION_STOP_CAPTURE)
        try {
            svc.startService(i)
        } catch (e: Exception) {
            Log.w(TAG, "startService(STOP_CAPTURE) threw", e)
        }
    }

    private fun updateMetadata() {
        val md = MediaMetadata.Builder()
            .putString(MediaMetadata.METADATA_KEY_TITLE, "Mirroring to $hostLabel")
            .putString(MediaMetadata.METADATA_KEY_ARTIST, "ansync companion")
            .build()
        session?.setMetadata(md)
    }

    private fun updatePlaybackState(state: Int) {
        // Mirror doesn't have a real "pause" but we register the
        // intent anyway so headset key dispatch resolves consistently
        // (`onPause` collapses to `onStop` above).
        val actions = PlaybackState.ACTION_STOP or
            PlaybackState.ACTION_PLAY_PAUSE
        val ps = PlaybackState.Builder()
            .setActions(actions)
            .setState(state, PlaybackState.PLAYBACK_POSITION_UNKNOWN, 1.0f)
            .build()
        session?.setPlaybackState(ps)
        postNotification()
    }

    private fun postNotification() {
        val sess = session ?: return
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        } else {
            PendingIntent.FLAG_UPDATE_CURRENT
        }
        val builder = NotificationCompat.Builder(svc, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
            .setContentTitle("Mirroring to $hostLabel")
            .setContentText("ansync companion")
            .setOngoing(true)
            .setShowWhen(false)
            .setVisibility(NotificationCompat.VISIBILITY_PUBLIC)
            .setLargeIcon(
                BitmapFactory.decodeResource(
                    svc.resources,
                    android.R.drawable.stat_sys_data_bluetooth,
                ),
            )

        val compatToken = MediaSessionCompat.Token.fromToken(sess.sessionToken)
        val mediaStyle = MediaStyle.MediaStyle()
            .setMediaSession(compatToken)
            .setShowActionsInCompactView(0)

        val stopIntent = Intent(svc, AnsyncCompanionService::class.java)
            .setAction(AnsyncCompanionService.ACTION_STOP_CAPTURE)
        builder.addAction(
            android.R.drawable.ic_media_pause,
            "Stop",
            PendingIntent.getService(svc, 40, stopIntent, flags),
        )
        builder.setStyle(mediaStyle)
        try {
            svc.getSystemService(NotificationManager::class.java)
                ?.notify(NOTIFICATION_ID, builder.build())
        } catch (e: Exception) {
            Log.w(TAG, "mirror media notif post threw", e)
        }
    }

    private fun ensureChannel() {
        val mgr = svc.getSystemService(NotificationManager::class.java) ?: return
        if (mgr.getNotificationChannel(CHANNEL_ID) != null) return
        val ch = NotificationChannel(
            CHANNEL_ID,
            "ansync mirror",
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = "MediaSession controls for the active screen mirror session"
            setSound(null, null)
            enableVibration(false)
        }
        mgr.createNotificationChannel(ch)
    }

    companion object {
        private const val TAG = "ansync.media.mirror"
        const val CHANNEL_ID = "ansync.media.mirror"
        const val NOTIFICATION_ID = 6
    }
}
