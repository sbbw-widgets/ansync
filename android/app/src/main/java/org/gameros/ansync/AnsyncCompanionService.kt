package org.gameros.ansync

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat

/**
 * Foreground service hosting the companion's persistent workers:
 *   - QUIC client to the paired host (`NativeBridge`)
 *   - MediaProjection capture loop + MediaCodec H.264 encoder
 *     (`CaptureSession`)
 *   - Audio routing (Step 11)
 *
 * Lifecycle: started by `MainActivity` once the user grants
 * MediaProjection. Receives the `resultCode` + `data` Intent from
 * `MediaProjectionManager.createScreenCaptureIntent` via the
 * `EXTRA_*` keys below.
 */
class AnsyncCompanionService : Service() {

    private var projection: MediaProjection? = null
    private var capture: CaptureSession? = null
    private var fsServer: AnsyncFsServer? = null
    private var camera: CameraSession? = null
    private var cameraPollThread: HandlerThread? = null
    private var cameraPollHandler: Handler? = null
    @Volatile private var cameraPollRunning = false
    private var audio: AudioRouter? = null
    private var audioPollThread: HandlerThread? = null
    private var audioPollHandler: Handler? = null
    @Volatile private var audioPollRunning = false
    private var clipboard: ClipboardBridge? = null

    override fun onCreate() {
        super.onCreate()
        ensureChannel(this)
        NativeBridge.nativeInit(filesDir.absolutePath)
        maybeStartFsServer()
        startCameraControlPoller()
        startAudioControlPoller()
        clipboard = ClipboardBridge(this).also { it.start() }
    }

    private fun startAudioControlPoller() {
        if (audioPollThread != null) return
        val ht = HandlerThread("ansync-aud-ctrl").also { it.start() }
        audioPollThread = ht
        audioPollHandler = Handler(ht.looper)
        audioPollRunning = true
        audioPollHandler?.post(object : Runnable {
            override fun run() {
                while (audioPollRunning) {
                    val blob = NativeBridge.nativePollAudioControl() ?: return
                    when (val msg = WireAudioControl.decode(blob)) {
                        is WireAudioControl.StartAudioRoute -> handleStartAudio(msg)
                        WireAudioControl.StopAudioRoute -> handleStopAudio()
                        null -> Log.w(TAG, "bad audio control blob")
                    }
                }
            }
        })
    }

    private fun handleStartAudio(msg: WireAudioControl.StartAudioRoute) {
        audio?.stop()
        audio = AudioRouter(msg.direction).also { it.start() }
        Log.i(TAG, "audio route started ${msg.direction}")
    }

    private fun handleStopAudio() {
        audio?.stop()
        audio = null
        Log.i(TAG, "audio route stopped")
    }

    private fun startCameraControlPoller() {
        if (cameraPollThread != null) return
        val ht = HandlerThread("ansync-cam-ctrl").also { it.start() }
        cameraPollThread = ht
        cameraPollHandler = Handler(ht.looper)
        cameraPollRunning = true
        cameraPollHandler?.post(object : Runnable {
            override fun run() {
                while (cameraPollRunning) {
                    val blob = NativeBridge.nativePollCameraControl() ?: return
                    when (val msg = WireCameraControl.decode(blob)) {
                        is WireCameraControl.StartCamera -> handleStartCamera(msg)
                        WireCameraControl.StopCamera -> handleStopCamera()
                        null -> Log.w(TAG, "bad camera control blob")
                    }
                }
            }
        })
    }

    private fun handleStartCamera(cfg: WireCameraControl.StartCamera) {
        if (camera != null) {
            Log.i(TAG, "camera already running; tearing down before re-bootstrap")
            camera?.stop()
            camera = null
        }
        camera = CameraSession(this, cfg).also { it.start() }
        Log.i(TAG, "camera session started for ${cfg.cameraId} (${cfg.width}x${cfg.height}@${cfg.fps})")
    }

    private fun handleStopCamera() {
        camera?.stop()
        camera = null
        Log.i(TAG, "camera session stopped")
    }

    private fun maybeStartFsServer() {
        val prefs = getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val uriStr = prefs.getString(PREF_TREE_URI, null) ?: return
        val uri = Uri.parse(uriStr)
        fsServer = AnsyncFsServer(this, uri).also { it.start() }
        Log.i(TAG, "fs server started against $uri")
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

        intent?.let { handleIntent(it) }

        return START_STICKY
    }

    private fun handleIntent(intent: Intent) {
        when (intent.action) {
            ACTION_START_CAPTURE -> {
                val resultCode = intent.getIntExtra(EXTRA_RESULT_CODE, 0)
                val data = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                    intent.getParcelableExtra(EXTRA_RESULT_DATA, Intent::class.java)
                } else {
                    @Suppress("DEPRECATION")
                    intent.getParcelableExtra<Intent>(EXTRA_RESULT_DATA)
                } ?: run {
                    Log.w(TAG, "ACTION_START_CAPTURE without result data")
                    return
                }
                if (capture != null) {
                    Log.i(TAG, "capture already running; ignoring start")
                    return
                }
                val manager = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
                val proj = manager.getMediaProjection(resultCode, data)
                projection = proj
                capture = CaptureSession(proj, CaptureConfig()).also { it.start() }
                Log.i(TAG, "capture started")
            }
            ACTION_STOP_CAPTURE -> stopCapture()
        }
    }

    private fun stopCapture() {
        capture?.stop()
        capture = null
        projection?.stop()
        projection = null
    }

    override fun onDestroy() {
        cameraPollRunning = false
        camera?.stop()
        camera = null
        cameraPollThread?.quitSafely()
        cameraPollThread = null
        cameraPollHandler = null
        audioPollRunning = false
        audio?.stop()
        audio = null
        audioPollThread?.quitSafely()
        audioPollThread = null
        audioPollHandler = null
        clipboard?.stop()
        clipboard = null
        stopCapture()
        fsServer?.stop()
        fsServer = null
        NativeBridge.nativeClose()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val TAG = "ansync.svc"
        const val CHANNEL_ID = "ansync.companion"
        const val NOTIFICATION_ID = 1

        const val ACTION_START_CAPTURE = "org.gameros.ansync.action.START_CAPTURE"
        const val ACTION_STOP_CAPTURE  = "org.gameros.ansync.action.STOP_CAPTURE"
        const val EXTRA_RESULT_CODE    = "org.gameros.ansync.extra.RESULT_CODE"
        const val EXTRA_RESULT_DATA    = "org.gameros.ansync.extra.RESULT_DATA"

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
