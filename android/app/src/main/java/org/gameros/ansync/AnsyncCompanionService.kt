package org.gameros.ansync

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
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
 * Lifecycle: started by `PairingReceiver` immediately after a
 * successful cable pair, by `BootReceiver` on device boot if a host
 * pubkey is persisted, or by any QSTile / popup activity that needs
 * the service running. There is no launcher icon — the user never
 * opens an app for ansync. MediaProjection grants come through
 * `GrantScreenCaptureActivity` which delivers the resulting Intent
 * here via [ACTION_START_CAPTURE]; first-launch permission grants
 * are surfaced via [SetupNotif] — a persistent heads-up that walks
 * the user through each pending grant from the shade, one tap at a
 * time. No full-screen wizard.
 */
class AnsyncCompanionService : Service() {

    private var projection: MediaProjection? = null
    private var capture: CaptureSession? = null
    private var camera: CameraSession? = null
    private var cameraPollThread: HandlerThread? = null
    private var cameraPollHandler: Handler? = null
    @Volatile private var cameraPollRunning = false
    private var audio: AudioRouter? = null
    private var audioPollThread: HandlerThread? = null
    private var audioPollHandler: Handler? = null
    @Volatile private var audioPollRunning = false
    private var clipboard: ClipboardBridge? = null
    private var dialer: HostDialer? = null
    private var wifiPair: WifiPairManager? = null
    private var mediaSession: AudioMediaSession? = null
    private var hostNamePoller: HandlerThread? = null
    private var hostNameHandler: Handler? = null
    @Volatile private var hostNamePollRunning = false
    @Volatile private var hostStatus: HostStatus = HostStatus.NotPaired
    private var capturePollThread: HandlerThread? = null
    private var capturePollHandler: Handler? = null
    @Volatile private var capturePollRunning = false
    private var urlPollThread: HandlerThread? = null
    @Volatile private var urlPollRunning = false
    private var receivedFilePollThread: HandlerThread? = null
    @Volatile private var receivedFilePollRunning = false
    private var screenReceiver: BroadcastReceiver? = null
    private var keepAlive: KeepAlive? = null
    /** Names of currently-held streams (`"capture"`, `"camera"`, `"audio"`).
     *  Drives [KeepAlive] refcount so the CPU wake-lock only stays up
     *  while at least one media path is alive. */
    private val activeStreams = mutableSetOf<String>()
    private var cpuWakePrefReceiver: BroadcastReceiver? = null

    override fun onCreate() {
        super.onCreate()
        ensureChannel(this)
        // Acquire the Wi-Fi lock immediately so the radio stays in
        // full-power mode for the entire service lifetime. Cheap to
        // hold + the only thing that prevents idle-doze from dropping
        // QUIC keep-alive pings between us and the host. Battery
        // whitelist is the other half — covered by `SetupStep.BatteryWhitelist`.
        keepAlive = KeepAlive(this).also { it.acquire() }
        NativeBridge.nativeInit(filesDir.absolutePath)
        NativeBridge.nativeSetDeviceName("${Build.MANUFACTURER} ${Build.MODEL}")
        val storedHostPubkey = getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getString(PairingReceiver.PREF_HOST_PUBKEY_HEX, null)
        Log.i(
            TAG,
            "stored host pubkey = ${storedHostPubkey?.take(16) ?: "<missing>"}…",
        )
        // Guided setup: post a persistent heads-up notif with the
        // next pending grant. The user walks through them from the
        // shade — no full-screen wizard. SetupStepActivity calls
        // back via ACTION_REFRESH_SETUP after each grant resolves.
        SetupNotif.refresh(this)
        startCameraControlPoller()
        startAudioControlPoller()
        clipboard = ClipboardBridge(this).also { it.start() }
        dialer = HostDialer(this).also {
            it.setListener { status ->
                hostStatus = status
                Handler(mainLooper).post { refreshNotification() }
            }
            it.start()
        }
        wifiPair = WifiPairManager(this).also { it.start() }
        startCaptureControlPoller()
        startHostNamePoller()
        startUrlPoller()
        startReceivedFilePoller()
        registerScreenWakeReceiver()
        registerCpuWakePrefReceiver()
    }

    /** Pull the latest host name learned from the inbound Hello frame
     *  every 5 s and persist it as [PairingReceiver.PREF_HOST_NAME] so
     *  the dialer / notif reflect what the host *currently* calls
     *  itself, not the stale value captured at pair time. Without this
     *  worker the name is stuck at whatever was sent during the
     *  bootstrap envelope — typically still right, but goes stale if
     *  the user renames their machine. */
    private fun startHostNamePoller() {
        if (hostNamePoller != null) return
        val ht = HandlerThread("ansync-hostname").also { it.start() }
        hostNamePoller = ht
        hostNameHandler = Handler(ht.looper)
        hostNamePollRunning = true
        hostNameHandler?.post(object : Runnable {
            override fun run() {
                while (hostNamePollRunning) {
                    val fresh = try {
                        NativeBridge.nativePollHostName()
                    } catch (e: Throwable) {
                        Log.w(TAG, "pollHostName threw", e)
                        null
                    }
                    if (!fresh.isNullOrBlank()) {
                        val prefs = getSharedPreferences(PREFS, Context.MODE_PRIVATE)
                        val current = prefs.getString(PairingReceiver.PREF_HOST_NAME, null)
                        if (current != fresh) {
                            prefs.edit()
                                .putString(PairingReceiver.PREF_HOST_NAME, fresh)
                                .apply()
                            Log.i(TAG, "host name refreshed: $fresh")
                        }
                    }
                    try { Thread.sleep(5_000) } catch (_: InterruptedException) { return }
                }
            }
        })
    }

    /**
     * Watch for `ACTION_SCREEN_ON` / `ACTION_USER_PRESENT` so the
     * capture pipeline can resync after the device wakes. While the
     * screen is off VirtualDisplay stops feeding the encoder's input
     * Surface, so no buffers come out; when the screen wakes the
     * encoder restarts on a stale GOP and the host's NV12 decoder is
     * left rendering the last frame from before sleep until the next
     * scheduled IDR (default 5 s with `KEY_I_FRAME_INTERVAL`).
     *
     * Empirically a single `setParameters(REQUEST_SYNC_FRAME)` on
     * SCREEN_ON misses the recovery on Lenovo / Samsung skins because
     * VirtualDisplay takes several hundred milliseconds to resume
     * feeding the input Surface after wake. We post three retries
     * (0, 250, 750 ms) to bracket the resume window.
     */
    private fun registerScreenWakeReceiver() {
        if (screenReceiver != null) return
        val r = object : BroadcastReceiver() {
            override fun onReceive(context: Context?, intent: Intent?) {
                val action = intent?.action ?: return
                Log.i(TAG, "screen receiver fired: $action")
                if (action == Intent.ACTION_SCREEN_ON
                    || action == Intent.ACTION_USER_PRESENT
                ) {
                    val handler = Handler(mainLooper)
                    longArrayOf(0L, 250L, 750L, 1500L).forEach { delay ->
                        handler.postDelayed({
                            val active = capture
                            if (active != null) {
                                Log.i(TAG, "screen wake — key frame retry at ${delay}ms")
                                active.requestKeyFrame()
                            }
                        }, delay)
                    }
                }
            }
        }
        val filter = IntentFilter().apply {
            addAction(Intent.ACTION_SCREEN_ON)
            addAction(Intent.ACTION_USER_PRESENT)
        }
        // Android 14+ requires an explicit export flag for receivers
        // registered at runtime. SCREEN_ON/USER_PRESENT are system
        // broadcasts so RECEIVER_NOT_EXPORTED is the right choice —
        // we never want to be the target of an external app's intent
        // here. Older releases ignore the flag.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            registerReceiver(r, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            registerReceiver(r, filter)
        }
        screenReceiver = r
        Log.i(TAG, "screen wake receiver registered")
    }

    /** Track a long-running media stream and forward the start/stop
     *  edges to [KeepAlive] so the optional `PARTIAL_WAKE_LOCK` stays
     *  scoped to actual activity. Idempotent — duplicate starts/stops
     *  for the same `key` are no-ops. */
    private fun markStream(key: String, active: Boolean) {
        val changed = if (active) activeStreams.add(key) else activeStreams.remove(key)
        if (!changed) return
        if (active) keepAlive?.streamStarted() else keepAlive?.streamStopped()
    }

    /** Receiver for `org.gameros.ansync.action.SET_CPU_WAKE_LOCK` with
     *  boolean extra `enabled`. Lets the user (via `adb shell am
     *  broadcast`, host D-Bus bridge, or a future settings UI) flip
     *  the CPU wake-lock policy without restarting the service. */
    private fun registerCpuWakePrefReceiver() {
        if (cpuWakePrefReceiver != null) return
        val r = object : BroadcastReceiver() {
            override fun onReceive(context: Context?, intent: Intent?) {
                val enabled = intent?.getBooleanExtra(EXTRA_CPU_WAKE_LOCK_ENABLED, false) ?: return
                getSharedPreferences(PREFS, Context.MODE_PRIVATE)
                    .edit()
                    .putBoolean(PREF_CPU_WAKE_LOCK, enabled)
                    .apply()
                keepAlive?.refreshCpuLockPolicy()
                Log.i(TAG, "cpu wake lock pref set to $enabled")
            }
        }
        val filter = IntentFilter(ACTION_SET_CPU_WAKE_LOCK)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            registerReceiver(r, filter, Context.RECEIVER_EXPORTED)
        } else {
            registerReceiver(r, filter)
        }
        cpuWakePrefReceiver = r
    }

    /** Watches the host's Control stream for
     *  `RequestScreenCapture` / `StopScreenCapture`. On `request` we
     *  pop the grant notif (`requestCaptureFromUser`); on `stop` we
     *  tear the running session down. */
    /**
     * Drain inbound URL pushes from the native side and pop a
     * "tap to open" consent notification per URL. The user must
     * confirm before the device actually fires `ACTION_VIEW` — the
     * peer is trusted but a compromised peer would otherwise be able
     * to open arbitrary intents (Linux side opens via `xdg-open`
     * directly per the threat model).
     */
    private fun startUrlPoller() {
        if (urlPollThread != null) return
        val ht = HandlerThread("ansync-url-in").also { it.start() }
        urlPollThread = ht
        urlPollRunning = true
        Handler(ht.looper).post(object : Runnable {
            override fun run() {
                while (urlPollRunning) {
                    val url = NativeBridge.nativePollIncomingUrl()
                    if (url == null) {
                        try { Thread.sleep(500) } catch (_: InterruptedException) {}
                        continue
                    }
                    Handler(mainLooper).post { postUrlConsentNotif(url) }
                }
            }
        })
    }

    /**
     * Drain inbound file completions and post a "tap to open" notif
     * pointing at the saved file via [FileProvider]. Also runs a
     * `MediaScannerConnection.scanFile` so the file appears under
     * Files / gallery without a manual rescan.
     */
    private fun startReceivedFilePoller() {
        if (receivedFilePollThread != null) return
        val ht = HandlerThread("ansync-files-in").also { it.start() }
        receivedFilePollThread = ht
        receivedFilePollRunning = true
        Handler(ht.looper).post(object : Runnable {
            override fun run() {
                while (receivedFilePollRunning) {
                    val path = NativeBridge.nativePollReceivedFile()
                    if (path == null) {
                        try { Thread.sleep(500) } catch (_: InterruptedException) {}
                        continue
                    }
                    Handler(mainLooper).post { postReceivedFileNotif(path) }
                }
            }
        })
    }

    private fun postUrlConsentNotif(url: String) {
        val openIntent = Intent(Intent.ACTION_VIEW, Uri.parse(url))
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        } else {
            PendingIntent.FLAG_UPDATE_CURRENT
        }
        val pi = PendingIntent.getActivity(this, url.hashCode(), openIntent, flags)
        val n = NotificationCompat.Builder(this, GRANT_CHANNEL_ID)
            .setContentTitle("Open link from host?")
            .setContentText(url)
            .setSmallIcon(android.R.drawable.ic_menu_share)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setContentIntent(pi)
            .setAutoCancel(true)
            .build()
        getSystemService(NotificationManager::class.java)
            ?.notify(URL_NOTIF_ID_BASE + (url.hashCode() and 0x7fff), n)
    }

    private fun postReceivedFileNotif(path: String) {
        val file = java.io.File(path)
        // FileProvider would be the production-grade path; for MVP
        // tap-to-open just points the system at the file URI. Most
        // file managers + galleries handle it. We register with the
        // media scanner regardless so the file shows up everywhere.
        try {
            android.media.MediaScannerConnection.scanFile(
                this,
                arrayOf(path),
                null,
                null,
            )
        } catch (t: Throwable) {
            Log.w(TAG, "MediaScanner scan threw", t)
        }
        val viewIntent = Intent(Intent.ACTION_VIEW)
            .setData(Uri.fromFile(file))
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        } else {
            PendingIntent.FLAG_UPDATE_CURRENT
        }
        val pi = PendingIntent.getActivity(this, path.hashCode(), viewIntent, flags)
        val n = NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("File received")
            .setContentText(file.name)
            .setSmallIcon(android.R.drawable.stat_sys_download_done)
            .setContentIntent(pi)
            .setAutoCancel(true)
            .build()
        getSystemService(NotificationManager::class.java)
            ?.notify(FILE_NOTIF_ID_BASE + (path.hashCode() and 0x7fff), n)
    }

    private fun startCaptureControlPoller() {
        if (capturePollThread != null) return
        val ht = HandlerThread("ansync-cap-ctrl").also { it.start() }
        capturePollThread = ht
        capturePollHandler = Handler(ht.looper)
        capturePollRunning = true
        capturePollHandler?.post(object : Runnable {
            override fun run() {
                while (capturePollRunning) {
                    val blob = NativeBridge.nativePollCaptureControl()
                    if (blob == null) {
                        try { Thread.sleep(500) } catch (_: InterruptedException) {}
                        continue
                    }
                    if (blob.isEmpty()) continue
                    when (blob[0].toInt()) {
                        0 -> Handler(mainLooper).post {
                            // If a session already exists the host
                            // probably just wants a fresh IDR (e.g.
                            // viewer reattached, decoder reset).
                            // Skipping the projection re-prompt avoids
                            // a surprise dialog mid-session.
                            val active = capture
                            if (active != null) {
                                Log.i(TAG, "RequestScreenCapture w/ active session — key frame instead")
                                active.requestKeyFrame()
                            } else {
                                requestCaptureFromUser()
                            }
                        }
                        1 -> Handler(mainLooper).post { stopCapture() }
                        else -> Log.w(TAG, "unknown capture-ctrl tag ${blob[0]}")
                    }
                }
            }
        })
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
                    val blob = NativeBridge.nativePollAudioControl()
                    if (blob == null) {
                        try { Thread.sleep(500) } catch (_: InterruptedException) {}
                        continue
                    }
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
        // Microphone foreground type required when capturing from the
        // device; speaker-only direction stays under dataSync.
        if (msg.direction != WireAudioControl.Direction.HostToDevice) {
            promoteForegroundType(
                ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
                    or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
            )
        }
        audio = AudioRouter(msg.direction).also { it.start() }
        markStream("audio", true)
        val ms = mediaSession ?: AudioMediaSession(this).also { mediaSession = it }
        ms.start(msg.direction)
        Log.i(TAG, "audio route started ${msg.direction}")
        refreshNotification()
    }

    private fun handleStopAudio() {
        audio?.stop()
        audio = null
        markStream("audio", false)
        mediaSession?.release()
        mediaSession = null
        Log.i(TAG, "audio route stopped")
        refreshNotification()
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
                    val blob = NativeBridge.nativePollCameraControl()
                    if (blob == null) {
                        try { Thread.sleep(500) } catch (_: InterruptedException) {}
                        continue
                    }
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
        promoteForegroundType(
            ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
                or ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA
        )
        camera = CameraSession(this, cfg).also { it.start() }
        markStream("camera", true)
        Log.i(TAG, "camera session started for ${cfg.cameraId} (${cfg.width}x${cfg.height}@${cfg.fps})")
        refreshNotification()
    }

    private fun handleStopCamera() {
        camera?.stop()
        camera = null
        markStream("camera", false)
        Log.i(TAG, "camera session stopped")
        refreshNotification()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Default to dataSync — Android 14+ rejects mediaProjection /
        // camera / microphone foreground starts unless the relevant
        // privileged token is already held. We elevate the foreground
        // type from `startCaptureWithProjection` / `AudioRouter` once a
        // real session begins.
        promoteForegroundType(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)

        intent?.let { handleIntent(it) }

        return START_STICKY
    }

    /** Cached so action callbacks can refresh the foreground notif
     *  without re-deriving from scratch. */
    private var currentForegroundType: Int = ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC

    /**
     * Re-issue `startForeground` with a new type bitmask. Android
     * tolerates promotion in either direction as long as the new mask
     * intersects what we declared in the manifest. Used to widen the
     * service to `mediaProjection|camera|microphone` once the
     * corresponding privileged token / permission is granted.
     */
    private fun promoteForegroundType(type: Int) {
        currentForegroundType = type
        val notification = buildNotification(this)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIFICATION_ID, notification, type)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    /**
     * Re-render the persistent notification in place. Call from any
     * code path that flips a stream on/off so the action row stays in
     * sync. Cheaper than `startForeground` — only mutates the visible
     * notification, doesn't reassert the FG type bitmask.
     */
    private fun refreshNotification() {
        val notification = buildNotification(this)
        getSystemService(NotificationManager::class.java)
            ?.notify(NOTIFICATION_ID, notification)
    }

    private fun handleIntent(intent: Intent) {
        when (intent.action) {
            ACTION_REQUEST_CAPTURE -> requestCaptureFromUser()
            ACTION_REFRESH_SETUP -> SetupNotif.refresh(this)
            ACTION_START_MIC_SHARE -> startAudioFromTile(WireAudioControl.Direction.DeviceToHost)
            ACTION_STOP_MIC_SHARE -> stopAudioFromTile(WireAudioControl.Direction.DeviceToHost)
            ACTION_START_AUDIO_SINK -> startAudioFromTile(WireAudioControl.Direction.HostToDevice)
            ACTION_STOP_AUDIO_SINK -> stopAudioFromTile(WireAudioControl.Direction.HostToDevice)
            ACTION_STOP_CAMERA -> handleStopCamera()
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
                // Android 14+ rejects `getMediaProjection` unless the
                // service is already foreground with the
                // MEDIA_PROJECTION type bit set. Promote BEFORE we
                // try to acquire the projection — getting the order
                // wrong throws SecurityException and the user has to
                // re-grant.
                promoteForegroundType(
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
                        or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
                )
                // `MediaCodec.createEncoderByType` + `configure` +
                // `createInputSurface` take 1-2 s on midrange devices.
                // The service runs on the process main looper, which
                // is shared with `GrantScreenCaptureActivity`'s UI
                // thread — blocking it here triggers an Input
                // dispatching ANR on the activity (Waited 5000ms for
                // FocusEvent). Offload to a worker; the foreground
                // promotion above is already in effect, so Android
                // won't kill us mid-init.
                kotlin.concurrent.thread(name = "ansync-capture-init") {
                    val manager =
                        getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
                    val proj = manager.getMediaProjection(resultCode, data) ?: run {
                        Log.w(TAG, "MediaProjectionManager.getMediaProjection returned null (denied?)")
                        Handler(mainLooper).post {
                            promoteForegroundType(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
                        }
                        return@thread
                    }
                    // Android 14+ throws IllegalStateException from
                    // `createVirtualDisplay` unless a callback is
                    // registered first ("Must register a callback
                    // before starting capture, to manage resources
                    // in response to MediaProjection states").
                    proj.registerCallback(object : MediaProjection.Callback() {
                        override fun onStop() {
                            Log.i(TAG, "MediaProjection.onStop — tearing capture down")
                            Handler(mainLooper).post { stopCapture() }
                        }
                    }, Handler(mainLooper))
                    val session = CaptureSession(this, proj, CaptureConfig()).also { it.start() }
                    Handler(mainLooper).post {
                        projection = proj
                        capture = session
                        markStream("capture", true)
                        setTileState(PREF_MIRROR_ACTIVE, true)
                        getSystemService(NotificationManager::class.java)
                            ?.cancel(GRANT_NOTIFICATION_ID)
                        Log.i(TAG, "capture started")
                        refreshNotification()
                    }
                }
            }
            ACTION_STOP_CAPTURE -> stopCapture()
        }
    }

    private fun stopCapture() {
        // Reentrant: MediaProjection.Callback.onStop and the host's
        // StopScreenCapture both target this method. Pull the field
        // values out into locals + null the field first so a second
        // concurrent call sees nothing to do.
        val captureLocal = capture
        capture = null
        markStream("capture", false)
        val projectionLocal = projection
        projection = null
        try {
            captureLocal?.stop()
        } catch (e: Exception) {
            Log.w(TAG, "capture.stop threw", e)
        }
        try {
            projectionLocal?.stop()
        } catch (e: Exception) {
            Log.w(TAG, "projection.stop threw", e)
        }
        setTileState(PREF_MIRROR_ACTIVE, false)
        try {
            refreshNotification()
        } catch (e: Exception) {
            Log.w(TAG, "refreshNotification threw during stop", e)
        }
    }

    /** QSTile-driven audio start. Re-uses [AudioRouter] but skips the
     *  control-message handshake — the user already opted in by
     *  tapping the tile. The host sees the new stream open the same
     *  way it does for a `Device.StartAudioRoute` D-Bus call. */
    private fun startAudioFromTile(direction: WireAudioControl.Direction) {
        val existing = audio
        val effective: WireAudioControl.Direction
        if (existing != null) {
            // Merge directions: if the current router is `DeviceToHost`
            // and the user enables sink → upgrade to `Both`; vice
            // versa. Otherwise no-op.
            val merged = mergeDirections(existing.direction, direction)
            effective = merged
            if (merged != existing.direction) {
                existing.stop()
                audio = AudioRouter(merged).also { it.start() }
            }
        } else {
            audio = AudioRouter(direction).also { it.start() }
            effective = direction
        }
        markStream("audio", true)
        val ms = mediaSession ?: AudioMediaSession(this).also { mediaSession = it }
        ms.start(effective)
        refreshNotification()
        when (direction) {
            WireAudioControl.Direction.DeviceToHost -> {
                setTileState(PREF_MIC_ACTIVE, true)
                promoteForegroundType(
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
                        or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
                )
            }
            WireAudioControl.Direction.HostToDevice -> {
                setTileState(PREF_AUDIO_OUT_ACTIVE, true)
            }
            WireAudioControl.Direction.Both -> {
                setTileState(PREF_MIC_ACTIVE, true)
                setTileState(PREF_AUDIO_OUT_ACTIVE, true)
                promoteForegroundType(
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
                        or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
                )
            }
        }
    }

    /** Inverse: peel one direction off the active router. */
    private fun stopAudioFromTile(direction: WireAudioControl.Direction) {
        val existing = audio ?: return
        val remaining = removeDirection(existing.direction, direction)
        existing.stop()
        audio = remaining?.let { AudioRouter(it).also { r -> r.start() } }
        if (audio == null) markStream("audio", false)
        if (remaining == null) {
            mediaSession?.release()
            mediaSession = null
        } else {
            mediaSession?.start(remaining)
        }
        refreshNotification()
        when (direction) {
            WireAudioControl.Direction.DeviceToHost -> setTileState(PREF_MIC_ACTIVE, false)
            WireAudioControl.Direction.HostToDevice -> setTileState(PREF_AUDIO_OUT_ACTIVE, false)
            WireAudioControl.Direction.Both -> {
                setTileState(PREF_MIC_ACTIVE, false)
                setTileState(PREF_AUDIO_OUT_ACTIVE, false)
            }
        }
    }

    private fun mergeDirections(
        current: WireAudioControl.Direction,
        add: WireAudioControl.Direction,
    ): WireAudioControl.Direction = when {
        current == add -> current
        current == WireAudioControl.Direction.Both || add == WireAudioControl.Direction.Both ->
            WireAudioControl.Direction.Both
        else -> WireAudioControl.Direction.Both
    }

    private fun removeDirection(
        current: WireAudioControl.Direction,
        remove: WireAudioControl.Direction,
    ): WireAudioControl.Direction? = when {
        current == remove -> null
        current == WireAudioControl.Direction.Both && remove == WireAudioControl.Direction.DeviceToHost ->
            WireAudioControl.Direction.HostToDevice
        current == WireAudioControl.Direction.Both && remove == WireAudioControl.Direction.HostToDevice ->
            WireAudioControl.Direction.DeviceToHost
        else -> current
    }

    private fun setTileState(key: String, active: Boolean) {
        getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putBoolean(key, active)
            .apply()
    }

    /**
     * Fired when the host requests a mirror but the user hasn't
     * granted MediaProjection for this session yet. We can't pop the
     * picker from a Service — Android requires an Activity surface —
     * so we post a high-priority ongoing notification that, when
     * tapped, launches [GrantScreenCaptureActivity]. The activity
     * runs the picker and starts capture on RESULT_OK.
     */
    private fun requestCaptureFromUser() {
        if (capture != null) return
        val intent = Intent(this, GrantScreenCaptureActivity::class.java).apply {
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK)
        }
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        } else {
            PendingIntent.FLAG_UPDATE_CURRENT
        }
        val pending = PendingIntent.getActivity(this, 0, intent, flags)
        val n = NotificationCompat.Builder(this, GRANT_CHANNEL_ID)
            .setContentTitle("ansync wants to mirror your screen")
            .setContentText("Tap to grant — the host is waiting")
            .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setContentIntent(pending)
            .setAutoCancel(true)
            .build()
        val mgr = getSystemService(NotificationManager::class.java)
        mgr?.notify(GRANT_NOTIFICATION_ID, n)
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
        mediaSession?.release()
        mediaSession = null
        audioPollThread?.quitSafely()
        audioPollThread = null
        audioPollHandler = null
        clipboard?.stop()
        clipboard = null
        dialer?.stop()
        dialer = null
        wifiPair?.stop()
        wifiPair = null
        capturePollRunning = false
        capturePollThread?.quitSafely()
        capturePollThread = null
        capturePollHandler = null
        urlPollRunning = false
        urlPollThread?.quitSafely()
        urlPollThread = null
        receivedFilePollRunning = false
        receivedFilePollThread?.quitSafely()
        receivedFilePollThread = null
        hostNamePollRunning = false
        hostNamePoller?.quitSafely()
        hostNamePoller = null
        hostNameHandler = null
        stopCapture()
        screenReceiver?.let {
            try { unregisterReceiver(it) } catch (e: IllegalArgumentException) {
                Log.w(TAG, "unregisterReceiver screen wake threw", e)
            }
        }
        screenReceiver = null
        cpuWakePrefReceiver?.let {
            try { unregisterReceiver(it) } catch (e: IllegalArgumentException) {
                Log.w(TAG, "unregisterReceiver cpu wake pref threw", e)
            }
        }
        cpuWakePrefReceiver = null
        activeStreams.clear()
        keepAlive?.release()
        keepAlive = null
        NativeBridge.nativeClose()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val TAG = "ansync.svc"
        const val CHANNEL_ID = "ansync.companion"
        const val NOTIFICATION_ID = 1

        /** High-importance channel used only for "host wants X, tap to grant" prompts. */
        const val GRANT_CHANNEL_ID = "ansync.grant"
        const val GRANT_NOTIFICATION_ID = 2
        /** Base id for inbound URL prompts ("Open link from host?"). The
         *  full id mixes in the URL hash so concurrent links don't
         *  overwrite each other. */
        const val URL_NOTIF_ID_BASE = 10_000
        /** Base id for inbound file completions. Mixed with the path
         *  hash for the same reason as URLs. */
        const val FILE_NOTIF_ID_BASE = 20_000

        /** MediaProjection result delivered by [GrantScreenCaptureActivity]. */
        const val ACTION_START_CAPTURE = "org.gameros.ansync.action.START_CAPTURE"
        const val ACTION_STOP_CAPTURE  = "org.gameros.ansync.action.STOP_CAPTURE"
        const val EXTRA_RESULT_CODE    = "org.gameros.ansync.extra.RESULT_CODE"
        const val EXTRA_RESULT_DATA    = "org.gameros.ansync.extra.RESULT_DATA"

        /** Sent by host control-stream (U5) or QSTile when the user wants to grant capture. */
        const val ACTION_REQUEST_CAPTURE = "org.gameros.ansync.action.REQUEST_CAPTURE"

        /** Sent by [SetupStepActivity] after any grant step resolves so the
         *  service can re-evaluate which step (if any) is still pending. */
        const val ACTION_REFRESH_SETUP = "org.gameros.ansync.action.REFRESH_SETUP"

        /** QSTile triggers — start/stop audio routes directly without
         *  waiting for the host's control-stream handshake. */
        const val ACTION_START_MIC_SHARE = "org.gameros.ansync.action.START_MIC_SHARE"
        const val ACTION_STOP_MIC_SHARE = "org.gameros.ansync.action.STOP_MIC_SHARE"
        const val ACTION_START_AUDIO_SINK = "org.gameros.ansync.action.START_AUDIO_SINK"
        const val ACTION_STOP_AUDIO_SINK = "org.gameros.ansync.action.STOP_AUDIO_SINK"

        /** Camera lifecycle stop (notification action button). */
        const val ACTION_STOP_CAMERA = "org.gameros.ansync.action.STOP_CAMERA"

        /** Flip [PREF_CPU_WAKE_LOCK] at runtime.
         *  `adb shell am broadcast -a org.gameros.ansync.action.SET_CPU_WAKE_LOCK --ez enabled true`
         *  toggles the optional `PARTIAL_WAKE_LOCK` policy. */
        const val ACTION_SET_CPU_WAKE_LOCK = "org.gameros.ansync.action.SET_CPU_WAKE_LOCK"
        const val EXTRA_CPU_WAKE_LOCK_ENABLED = "enabled"

        /**
         * Start the foreground companion service idempotently. Used by
         * [WifiPairManager] after a successful pair so the freshly
         * paired host is immediately reachable by [HostDialer]; also
         * convenient for any other path that wants to wake the service
         * without picking the right `startForegroundService` overload
         * inline.
         */
        fun startSelf(ctx: Context) {
            val svc = Intent(ctx, AnsyncCompanionService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                ctx.startForegroundService(svc)
            } else {
                ctx.startService(svc)
            }
        }

        private fun ensureChannel(ctx: Context) {
            val mgr = ctx.getSystemService(NotificationManager::class.java) ?: return
            if (mgr.getNotificationChannel(CHANNEL_ID) == null) {
                val ch = NotificationChannel(
                    CHANNEL_ID,
                    "ansync companion",
                    NotificationManager.IMPORTANCE_LOW,
                ).apply {
                    description = "Persistent capture + transport for the paired host"
                }
                mgr.createNotificationChannel(ch)
            }
            if (mgr.getNotificationChannel(GRANT_CHANNEL_ID) == null) {
                val ch = NotificationChannel(
                    GRANT_CHANNEL_ID,
                    "ansync grant prompts",
                    NotificationManager.IMPORTANCE_HIGH,
                ).apply {
                    description = "Heads-up when the host needs you to grant a one-shot permission"
                }
                mgr.createNotificationChannel(ch)
            }
        }

        private fun buildNotification(svc: AnsyncCompanionService): Notification {
            val ctx: Context = svc
            val builder = NotificationCompat.Builder(ctx, CHANNEL_ID)
                .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
                .setOngoing(true)
                .setShowWhen(false)
            val active = mutableListOf<String>()
            val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
            } else {
                PendingIntent.FLAG_UPDATE_CURRENT
            }
            if (svc.capture != null) {
                active.add("mirror")
                val stop = Intent(ctx, AnsyncCompanionService::class.java)
                    .setAction(ACTION_STOP_CAPTURE)
                val pi = PendingIntent.getService(ctx, 10, stop, flags)
                builder.addAction(android.R.drawable.ic_media_pause, "Stop mirror", pi)
            }
            val audioRouter = svc.audio
            if (audioRouter != null) {
                val dir = audioRouter.direction
                if (dir == WireAudioControl.Direction.DeviceToHost
                    || dir == WireAudioControl.Direction.Both
                ) {
                    active.add("mic")
                    val stop = Intent(ctx, AnsyncCompanionService::class.java)
                        .setAction(ACTION_STOP_MIC_SHARE)
                    val pi = PendingIntent.getService(ctx, 11, stop, flags)
                    builder.addAction(android.R.drawable.ic_media_pause, "Stop mic share", pi)
                }
                if (dir == WireAudioControl.Direction.HostToDevice
                    || dir == WireAudioControl.Direction.Both
                ) {
                    active.add("PC audio")
                    val stop = Intent(ctx, AnsyncCompanionService::class.java)
                        .setAction(ACTION_STOP_AUDIO_SINK)
                    val pi = PendingIntent.getService(ctx, 12, stop, flags)
                    builder.addAction(android.R.drawable.ic_media_pause, "Stop PC audio", pi)
                }
            }
            if (svc.camera != null) {
                active.add("camera")
                val stop = Intent(ctx, AnsyncCompanionService::class.java)
                    .setAction(ACTION_STOP_CAMERA)
                val pi = PendingIntent.getService(ctx, 13, stop, flags)
                builder.addAction(android.R.drawable.ic_media_pause, "Stop camera", pi)
            }
            val streamLine = if (active.isEmpty()) {
                "Idle — paired host can request streams"
            } else {
                "Active: " + active.joinToString(", ")
            }
            val statusLine = when (val s = svc.hostStatus) {
                is HostStatus.NotPaired -> "Not paired"
                is HostStatus.NoNetwork -> "Waiting for Wi-Fi"
                is HostStatus.Searching -> "Looking for ${s.hostName.ifBlank { "host" }}…"
                is HostStatus.Connected -> "Connected to ${s.hostName.ifBlank { "host" }}"
            }
            builder.setContentTitle("ansync companion")
                .setContentText(statusLine)
                .setStyle(
                    NotificationCompat.BigTextStyle()
                        .bigText("$statusLine\n$streamLine"),
                )
            return builder.build()
        }
    }
}
