package org.gameros.ansync

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.graphics.BitmapFactory
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.media.MediaMetadata
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.os.Build
import android.support.v4.media.session.MediaSessionCompat
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.media.app.NotificationCompat as MediaStyle

/**
 * MediaSession-backed widget for the [AudioRouter] route.
 *
 * Wraps a `MediaSession` so the active audio route shows up:
 *   * Lock-screen media area (Android picks the active session
 *     automatically when its `PlaybackState` is `PLAYING`).
 *   * BT headset / AVRCP play / pause / stop keys (we register for
 *     `FLAG_HANDLES_MEDIA_BUTTONS | FLAG_HANDLES_TRANSPORT_CONTROLS`).
 *   * Hardware media keys on USB / Bluetooth keyboards.
 *   * Wear OS / Android Auto, which auto-mirror the active session.
 *
 * Also requests `AUDIOFOCUS_GAIN` so an incoming call or another app
 * with higher focus (alarm, navigation prompt) pauses the share, and
 * resumes when focus returns.
 *
 * Lifecycle is tied to a single audio route: each new
 * `StartAudioRoute` from the host calls [start]; `StopAudioRoute` /
 * stream teardown call [release]. Idempotent.
 */
class AudioMediaSession(private val svc: AnsyncCompanionService) {

    private var session: MediaSession? = null
    private var focusRequest: AudioFocusRequest? = null
    private var direction: WireAudioControl.Direction = WireAudioControl.Direction.Both
    private var pausedByFocus: Boolean = false

    private val focusListener = AudioManager.OnAudioFocusChangeListener { change ->
        when (change) {
            AudioManager.AUDIOFOCUS_LOSS -> {
                Log.i(TAG, "audio focus lost permanently — stopping share")
                stopRouteViaService()
            }
            AudioManager.AUDIOFOCUS_LOSS_TRANSIENT,
            AudioManager.AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK -> {
                Log.i(TAG, "audio focus lost transiently — pausing")
                pausedByFocus = true
                updatePlaybackState(PlaybackState.STATE_PAUSED)
            }
            AudioManager.AUDIOFOCUS_GAIN -> {
                if (pausedByFocus) {
                    Log.i(TAG, "audio focus regained — resuming")
                    pausedByFocus = false
                    updatePlaybackState(PlaybackState.STATE_PLAYING)
                }
            }
        }
    }

    /** Fire whichever STOP intents apply to the current direction.
     *  Routed through startService so the existing onStartCommand
     *  dispatch handles the actual teardown (avoids duplicating
     *  AudioRouter shutdown logic here). */
    private fun stopRouteViaService() {
        val intents = when (direction) {
            WireAudioControl.Direction.DeviceToHost ->
                listOf(AnsyncCompanionService.ACTION_STOP_MIC_SHARE)
            WireAudioControl.Direction.HostToDevice ->
                listOf(AnsyncCompanionService.ACTION_STOP_AUDIO_SINK)
            WireAudioControl.Direction.Both -> listOf(
                AnsyncCompanionService.ACTION_STOP_MIC_SHARE,
                AnsyncCompanionService.ACTION_STOP_AUDIO_SINK,
            )
        }
        for (action in intents) {
            val i = Intent(svc, AnsyncCompanionService::class.java).setAction(action)
            try {
                svc.startService(i)
            } catch (e: Exception) {
                Log.w(TAG, "startService($action) threw", e)
            }
        }
    }

    fun start(direction: WireAudioControl.Direction) {
        this.direction = direction
        ensureChannel()
        val mediaSession = session ?: MediaSession(svc, TAG).apply {
            setFlags(
                MediaSession.FLAG_HANDLES_MEDIA_BUTTONS
                    or MediaSession.FLAG_HANDLES_TRANSPORT_CONTROLS,
            )
            setCallback(object : MediaSession.Callback() {
                override fun onPlay() {
                    pausedByFocus = false
                    updatePlaybackState(PlaybackState.STATE_PLAYING)
                }
                override fun onPause() {
                    pausedByFocus = false
                    updatePlaybackState(PlaybackState.STATE_PAUSED)
                }
                override fun onStop() {
                    // Hardware "stop" key → tear the route down via the
                    // same path as the QSTile / notif action.
                    stopRouteViaService()
                }
            })
            isActive = true
        }
        session = mediaSession
        requestAudioFocus()
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
        abandonAudioFocus()
        val mgr = svc.getSystemService(NotificationManager::class.java)
        try {
            mgr?.cancel(NOTIFICATION_ID)
        } catch (e: Exception) {
            Log.w(TAG, "cancel media notif threw", e)
        }
    }

    fun token(): MediaSession.Token? = session?.sessionToken

    private fun updateMetadata() {
        val title = when (direction) {
            WireAudioControl.Direction.DeviceToHost -> "Sharing mic with PC"
            WireAudioControl.Direction.HostToDevice -> "Playing PC audio"
            WireAudioControl.Direction.Both -> "Two-way audio with PC"
        }
        val md = MediaMetadata.Builder()
            .putString(MediaMetadata.METADATA_KEY_TITLE, title)
            .putString(MediaMetadata.METADATA_KEY_ARTIST, "ansync companion")
            .build()
        session?.setMetadata(md)
    }

    private fun updatePlaybackState(state: Int) {
        val actions = PlaybackState.ACTION_PLAY or
            PlaybackState.ACTION_PAUSE or
            PlaybackState.ACTION_STOP or
            PlaybackState.ACTION_PLAY_PAUSE
        val ps = PlaybackState.Builder()
            .setActions(actions)
            .setState(state, PlaybackState.PLAYBACK_POSITION_UNKNOWN, 1.0f)
            .build()
        session?.setPlaybackState(ps)
        postNotification()
    }

    private fun requestAudioFocus() {
        val am = svc.getSystemService(Context.AUDIO_SERVICE) as? AudioManager ?: return
        if (focusRequest != null) return
        val attrs = AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_MEDIA)
            .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
            .build()
        val req = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
            .setAudioAttributes(attrs)
            .setOnAudioFocusChangeListener(focusListener)
            .setWillPauseWhenDucked(true)
            .build()
        focusRequest = req
        val result = am.requestAudioFocus(req)
        if (result != AudioManager.AUDIOFOCUS_REQUEST_GRANTED) {
            Log.w(TAG, "audio focus request not granted: $result")
        }
    }

    private fun abandonAudioFocus() {
        val am = svc.getSystemService(Context.AUDIO_SERVICE) as? AudioManager ?: return
        focusRequest?.let { am.abandonAudioFocusRequest(it) }
        focusRequest = null
    }

    private fun postNotification() {
        val sess = session ?: return
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        } else {
            PendingIntent.FLAG_UPDATE_CURRENT
        }
        val title = when (direction) {
            WireAudioControl.Direction.DeviceToHost -> "Mic → PC"
            WireAudioControl.Direction.HostToDevice -> "PC audio → phone"
            WireAudioControl.Direction.Both -> "Two-way audio"
        }
        val builder = NotificationCompat.Builder(svc, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.ic_btn_speak_now)
            .setContentTitle(title)
            .setContentText("ansync companion")
            .setOngoing(true)
            .setShowWhen(false)
            .setVisibility(NotificationCompat.VISIBILITY_PUBLIC)
            .setLargeIcon(
                BitmapFactory.decodeResource(
                    svc.resources,
                    android.R.drawable.ic_btn_speak_now,
                ),
            )

        // `androidx.media.app.NotificationCompat.MediaStyle` ships with
        // the legacy AndroidX-media surface and only accepts the
        // `MediaSessionCompat.Token` shape. Framework `MediaSession`
        // exposes its native `Token` which `MediaSessionCompat.Token
        // .fromToken(...)` wraps for free — no `MediaSessionCompat`
        // instance needed.
        val compatToken = MediaSessionCompat.Token.fromToken(sess.sessionToken)
        val mediaStyle = MediaStyle.MediaStyle()
            .setMediaSession(compatToken)
        // Show single action button compactly on lock screen.
        mediaStyle.setShowActionsInCompactView(0)

        // The persistent companion notif still owns "Stop X" actions
        // — the media notif's stop action funnels through the same
        // service Intent so behaviour stays consistent.
        val stopMicIntent = Intent(svc, AnsyncCompanionService::class.java)
            .setAction(AnsyncCompanionService.ACTION_STOP_MIC_SHARE)
        val stopSinkIntent = Intent(svc, AnsyncCompanionService::class.java)
            .setAction(AnsyncCompanionService.ACTION_STOP_AUDIO_SINK)

        when (direction) {
            WireAudioControl.Direction.DeviceToHost -> {
                builder.addAction(
                    android.R.drawable.ic_media_pause,
                    "Stop mic",
                    PendingIntent.getService(svc, 30, stopMicIntent, flags),
                )
            }
            WireAudioControl.Direction.HostToDevice -> {
                builder.addAction(
                    android.R.drawable.ic_media_pause,
                    "Stop PC audio",
                    PendingIntent.getService(svc, 31, stopSinkIntent, flags),
                )
            }
            WireAudioControl.Direction.Both -> {
                builder.addAction(
                    android.R.drawable.ic_media_pause,
                    "Stop mic",
                    PendingIntent.getService(svc, 30, stopMicIntent, flags),
                )
                builder.addAction(
                    android.R.drawable.ic_media_pause,
                    "Stop PC audio",
                    PendingIntent.getService(svc, 31, stopSinkIntent, flags),
                )
                mediaStyle.setShowActionsInCompactView(0, 1)
            }
        }
        builder.setStyle(mediaStyle)
        try {
            svc.getSystemService(NotificationManager::class.java)
                ?.notify(NOTIFICATION_ID, builder.build())
        } catch (e: Exception) {
            Log.w(TAG, "media notif post threw", e)
        }
    }

    private fun ensureChannel() {
        val mgr = svc.getSystemService(NotificationManager::class.java) ?: return
        if (mgr.getNotificationChannel(CHANNEL_ID) != null) return
        // LOW importance so the media notif doesn't sound / pop —
        // it's a stateful widget, not an alert.
        val ch = NotificationChannel(
            CHANNEL_ID,
            "ansync audio route",
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = "MediaSession controls for active mic / PC-audio routes"
            setSound(null, null)
            enableVibration(false)
        }
        mgr.createNotificationChannel(ch)
    }

    companion object {
        private const val TAG = "ansync.media"
        const val CHANNEL_ID = "ansync.media"
        const val NOTIFICATION_ID = 5
    }
}
