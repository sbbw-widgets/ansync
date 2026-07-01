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
    private var audio: AudioRouter? = null
    private var audioPollThread: HandlerThread? = null
    private var audioPollHandler: Handler? = null
    @Volatile private var audioPollRunning = false
    private var clipboard: ClipboardBridge? = null
    private var dialer: HostDialer? = null
    private var wifiPair: WifiPairManager? = null
    private var mediaSession: AudioMediaSession? = null
    private var mirrorMediaSession: MirrorMediaSession? = null
    private var hostNamePoller: HandlerThread? = null
    private var hostNameHandler: Handler? = null
    @Volatile private var hostNamePollRunning = false
    @Volatile private var hostStatus: HostStatus = HostStatus.NotPaired
    private var urlPollThread: HandlerThread? = null
    @Volatile private var urlPollRunning = false
    private var receivedFilePollThread: HandlerThread? = null
    @Volatile private var receivedFilePollRunning = false
    private var progressPollThread: HandlerThread? = null
    @Volatile private var progressPollRunning = false
    /** Last percentage notif'd per `batchId`, so chunk-rate callbacks
     *  don't repost the same percent. */
    private val lastProgressPct = mutableMapOf<Long, Int>()
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
        startHostNamePoller()
        startUrlPoller()
        startReceivedFilePoller()
        startTransferProgressPoller()
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
    /**
     * Drain per-chunk transfer progress events emitted by the native
     * batch sender. Single low-priority notif per `batchId` (sender)
     * or `transferId` (receive), updated in-place via
     * `setOnlyAlertOnce(true)` + `setProgress()`. Throttled to 1% to
     * keep the shade calm.
     */
    private fun startTransferProgressPoller() {
        if (progressPollThread != null) return
        val ht = HandlerThread("ansync-progress").also { it.start() }
        progressPollThread = ht
        progressPollRunning = true
        Handler(ht.looper).post(object : Runnable {
            override fun run() {
                while (progressPollRunning) {
                    val blob = try {
                        NativeBridge.nativePollTransferProgress()
                    } catch (e: Throwable) {
                        Log.w(TAG, "pollTransferProgress threw", e)
                        null
                    }
                    if (blob == null) {
                        try { Thread.sleep(250) } catch (_: InterruptedException) { return }
                        continue
                    }
                    val ev = WireProgress.decode(blob) ?: continue
                    handleProgressEvent(ev)
                }
            }
        })
    }

    private fun handleProgressEvent(ev: WireProgress) {
        val notifId = when (ev.direction) {
            WireProgress.Direction.Send -> PROGRESS_NOTIF_BASE + (ev.batchId.hashCode() and 0x7fff)
            WireProgress.Direction.Receive ->
                PROGRESS_NOTIF_BASE + (ev.transferId.hashCode() and 0x7fff)
        }
        val mgr = getSystemService(NotificationManager::class.java) ?: return

        val isFinal = ev.bytes == ev.total && ev.total > 0L
        when (ev.direction) {
            WireProgress.Direction.Send -> {
                val key = ev.batchId
                val pct = ev.batchPercent()
                val last = lastProgressPct[key] ?: -1
                if (!isFinal && pct == last) return
                lastProgressPct[key] = pct
                val filesDone = ev.batchFilesDone
                val title = if (ev.batchFiles > 1) {
                    val current = (filesDone + 1).coerceAtMost(ev.batchFiles)
                    "Sending $current of ${ev.batchFiles} to PC"
                } else {
                    "Sending to PC"
                }
                val text = "${ev.name} · $pct%"
                val builder = NotificationCompat.Builder(this, CHANNEL_ID)
                    .setSmallIcon(android.R.drawable.stat_sys_upload)
                    .setContentTitle(title)
                    .setContentText(text)
                    .setOnlyAlertOnce(true)
                    .setOngoing(true)
                    .setPriority(NotificationCompat.PRIORITY_LOW)
                    .setProgress(100, pct, false)
                mgr.notify(notifId, builder.build())
                if (isFinal && filesDone + 1 >= ev.batchFiles) {
                    // Final summary collapses the progress entry into a
                    // tap-dismissible toast.
                    mgr.cancel(notifId)
                    val total = ev.batchFiles
                    val summary = if (total > 1) "Sent $total files to PC" else "Sent ${ev.name}"
                    val done = NotificationCompat.Builder(this, CHANNEL_ID)
                        .setSmallIcon(android.R.drawable.stat_sys_upload_done)
                        .setContentTitle(summary)
                        .setOnlyAlertOnce(true)
                        .setAutoCancel(true)
                        .setPriority(NotificationCompat.PRIORITY_LOW)
                        .build()
                    mgr.notify(notifId, done)
                    lastProgressPct.remove(key)
                }
            }
            WireProgress.Direction.Receive -> {
                val key = ev.transferId
                val pct = if (ev.total <= 0L) 100 else
                    ((ev.bytes * 100L) / ev.total).coerceIn(0L, 100L).toInt()
                val last = lastProgressPct[key] ?: -1
                if (!isFinal && pct == last) return
                lastProgressPct[key] = pct
                val title = "Receiving ${ev.name}"
                val text = "$pct%"
                val builder = NotificationCompat.Builder(this, CHANNEL_ID)
                    .setSmallIcon(android.R.drawable.stat_sys_download)
                    .setContentTitle(title)
                    .setContentText(text)
                    .setOnlyAlertOnce(true)
                    .setOngoing(true)
                    .setPriority(NotificationCompat.PRIORITY_LOW)
                    .setProgress(100, pct, false)
                mgr.notify(notifId, builder.build())
                if (isFinal) {
                    // postReceivedFileNotif (driven by the
                    // received-file poller) replaces the entry once
                    // the path lands in the channel — clear the
                    // progress placeholder now.
                    mgr.cancel(notifId)
                    lastProgressPct.remove(key)
                }
            }
        }
    }

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
                    Handler(mainLooper).post { inboundCoalescer.record(path) }
                }
            }
        })
    }

    /**
     * Coalesce arrivals from the paired host within a 2 s TTL so a
     * burst (5-file multi-share) collapses into a single
     * "Received N files" notif instead of 5 stacked entries. The
     * host is implicit — only one host is ever paired today; multi-
     * host work (N8) will key on the sender's pubkey instead.
     */
    private inner class InboundCoalescer(private val windowMs: Long = 2_000L) {
        private val main = Handler(mainLooper)
        private val pending = ArrayList<String>()
        private val flush = Runnable { flushNow() }

        fun record(path: String) {
            main.post {
                pending.add(path)
                main.removeCallbacks(flush)
                main.postDelayed(flush, windowMs)
            }
        }

        private fun flushNow() {
            val paths = ArrayList(pending)
            pending.clear()
            if (paths.isEmpty()) return
            try {
                android.media.MediaScannerConnection.scanFile(
                    this@AnsyncCompanionService,
                    paths.toTypedArray(),
                    null,
                    null,
                )
            } catch (t: Throwable) {
                Log.w(TAG, "MediaScanner batch scan threw", t)
            }
            val host = getSharedPreferences(PREFS, Context.MODE_PRIVATE)
                .getString(PairingReceiver.PREF_HOST_NAME, null) ?: "PC"
            val mgr = getSystemService(NotificationManager::class.java) ?: return
            val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
            } else {
                PendingIntent.FLAG_UPDATE_CURRENT
            }
            if (paths.size == 1) {
                val path = paths[0]
                val file = java.io.File(path)
                val viewIntent = Intent(Intent.ACTION_VIEW)
                    .setData(Uri.fromFile(file))
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                val pi = PendingIntent.getActivity(
                    this@AnsyncCompanionService,
                    path.hashCode(),
                    viewIntent,
                    flags,
                )
                val n = NotificationCompat.Builder(this@AnsyncCompanionService, CHANNEL_ID)
                    .setContentTitle("File received from $host")
                    .setContentText(file.name)
                    .setSmallIcon(android.R.drawable.stat_sys_download_done)
                    .setContentIntent(pi)
                    .setAutoCancel(true)
                    .build()
                mgr.notify(FILE_NOTIF_ID_BASE + (path.hashCode() and 0x7fff), n)
            } else {
                val first = java.io.File(paths[0])
                val parent = first.parentFile ?: first
                val viewIntent = Intent(Intent.ACTION_VIEW)
                    .setDataAndType(Uri.fromFile(parent), "resource/folder")
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                val pi = PendingIntent.getActivity(
                    this@AnsyncCompanionService,
                    parent.absolutePath.hashCode(),
                    viewIntent,
                    flags,
                )
                val sample = paths.take(3).joinToString(", ") { java.io.File(it).name } +
                    if (paths.size > 3) ", …" else ""
                val n = NotificationCompat.Builder(this@AnsyncCompanionService, CHANNEL_ID)
                    .setContentTitle("Received ${paths.size} files from $host")
                    .setContentText(sample)
                    .setSmallIcon(android.R.drawable.stat_sys_download_done)
                    .setContentIntent(pi)
                    .setAutoCancel(true)
                    .build()
                mgr.notify(FILE_NOTIF_ID_BASE + (parent.absolutePath.hashCode() and 0x7fff), n)
            }
        }
    }

    private val inboundCoalescer by lazy { InboundCoalescer() }

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
                    when (WireAudioControl.decode(blob)) {
                        WireAudioControl.StartAudioSink -> handleStartAudioSink()
                        WireAudioControl.StopAudioSink -> handleStopAudioSink()
                        null -> Log.w(TAG, "bad audio control blob")
                    }
                }
            }
        })
    }

    /** Host announced `ControlMessage::StartAudioSink` — arm the
     *  playback AudioTrack + notif with Stop action. Direction is
     *  always [WireAudioControl.Direction.HostToDevice]. */
    private fun handleStartAudioSink() {
        audio?.stop()
        audio = AudioRouter(WireAudioControl.Direction.HostToDevice).also { it.start() }
        markStream("audio", true)
        val ms = mediaSession ?: AudioMediaSession(this).also { mediaSession = it }
        ms.start(WireAudioControl.Direction.HostToDevice)
        Log.i(TAG, "audio sink armed")
        refreshNotification()
    }

    /** Host announced `ControlMessage::StopAudioSink` — tear the
     *  AudioTrack + notif down. No upstream signal needed: the host
     *  already stopped pumping before sending this. */
    private fun handleStopAudioSink() {
        audio?.stop()
        audio = null
        markStream("audio", false)
        mediaSession?.release()
        mediaSession = null
        Log.i(TAG, "audio sink stopped by host")
        refreshNotification()
    }

    /** User tapped the "Stop PC audio" notif action. Tear the local
     *  AudioTrack down AND tell the host to stop pumping so the
     *  encoder + capture-source shut off on that side too. */
    private fun stopAudioSinkFromNotif() {
        handleStopAudioSink()
        // Best-effort: if the QUIC session is gone the call returns
        // false — that's fine, the host already lost the stream.
        NativeBridge.nativeSendStopAudioSink()
    }

    /** Fired by [tile.CameraTile] short-tap. Loads the persisted
     *  [CameraLocalConfig] (or defaults on first run) and spawns a
     *  [CameraSession]. Config is picked BY the phone — the host
     *  never dictates. */
    private fun handleStartCamera() {
        if (camera != null) {
            Log.i(TAG, "camera already running; ignoring start")
            return
        }
        promoteForegroundType(
            ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
                or ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA
        )
        val cfg = CameraLocalConfig.load(this)
        camera = CameraSession(this, cfg).also { it.start() }
        markStream("camera", true)
        Log.i(TAG, "camera session started ${cfg.cameraId} ${cfg.width}x${cfg.height}@${cfg.fps}")
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
            // Audio sink is host-initiated (D-Bus). The Stop action on
            // the notif tears the local AudioTrack down AND tells the
            // PC to stop pumping (receiver-can-stop).
            ACTION_STOP_AUDIO_SINK -> stopAudioSinkFromNotif()
            ACTION_START_CAMERA -> handleStartCamera()
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
                        startMirrorMediaSession()
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
        mirrorMediaSession?.release()
        mirrorMediaSession = null
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

    /** Bring up [MirrorMediaSession] tied to the current capture
     *  session. Title is rendered against the last host name we
     *  learned (Hello frame → SharedPreferences). Idempotent. */
    private fun startMirrorMediaSession() {
        if (mirrorMediaSession != null) return
        val host = getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getString(PairingReceiver.PREF_HOST_NAME, null) ?: "PC"
        mirrorMediaSession = MirrorMediaSession(this, host).also { it.start() }
    }

    /** QSTile-driven mic share start. The user tapped [MicShareTile];
     *  the host has no say — it will see the stream open when the
     *  first Opus packet arrives.
     *
     *  Direction is passed for symmetry with [stopAudioFromTile] but
     *  callers pass [WireAudioControl.Direction.DeviceToHost] only.
     *  Simultaneous host-initiated audio sink runs on a separate
     *  [AudioRouter] wired by `handleStartAudioSink`. */
    private fun startAudioFromTile(direction: WireAudioControl.Direction) {
        val existing = audio
        if (existing != null) {
            if (existing.direction == direction) return
            existing.stop()
        }
        audio = AudioRouter(direction).also { it.start() }
        markStream("audio", true)
        val ms = mediaSession ?: AudioMediaSession(this).also { mediaSession = it }
        ms.start(direction)
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
                Log.w(TAG, "startAudioFromTile(HostToDevice) — audio sink is host-initiated, ignoring")
            }
        }
    }

    /** Inverse: tear the router down if the direction matches. */
    private fun stopAudioFromTile(direction: WireAudioControl.Direction) {
        val existing = audio ?: return
        if (existing.direction != direction) return
        existing.stop()
        audio = null
        markStream("audio", false)
        mediaSession?.release()
        mediaSession = null
        refreshNotification()
        when (direction) {
            WireAudioControl.Direction.DeviceToHost -> setTileState(PREF_MIC_ACTIVE, false)
            WireAudioControl.Direction.HostToDevice -> {
                // no-op: audio sink Stop comes through
                // stopAudioSinkFromNotif, not the tile-driven path.
            }
        }
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
        camera?.stop()
        camera = null
        audioPollRunning = false
        audio?.stop()
        audio = null
        mediaSession?.release()
        mediaSession = null
        mirrorMediaSession?.release()
        mirrorMediaSession = null
        audioPollThread?.quitSafely()
        audioPollThread = null
        audioPollHandler = null
        clipboard?.stop()
        clipboard = null
        dialer?.stop()
        dialer = null
        wifiPair?.stop()
        wifiPair = null
        urlPollRunning = false
        urlPollThread?.quitSafely()
        urlPollThread = null
        receivedFilePollRunning = false
        receivedFilePollThread?.quitSafely()
        receivedFilePollThread = null
        progressPollRunning = false
        progressPollThread?.quitSafely()
        progressPollThread = null
        lastProgressPct.clear()
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
        /** Base id for in-flight transfer progress notifs. Mixed with
         *  the batch id (sender) or transfer id (receive). */
        const val PROGRESS_NOTIF_BASE = 30_000

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

        /** QSTile triggers — mic share (phone → PC) starts/stops
         *  directly from [MicShareTile]. Audio sink (PC → phone) has
         *  no start tile (host-initiated via D-Bus); only STOP exists,
         *  fired from the notification action. */
        const val ACTION_START_MIC_SHARE = "org.gameros.ansync.action.START_MIC_SHARE"
        const val ACTION_STOP_MIC_SHARE = "org.gameros.ansync.action.STOP_MIC_SHARE"
        const val ACTION_STOP_AUDIO_SINK = "org.gameros.ansync.action.STOP_AUDIO_SINK"

        /** Camera lifecycle (QSTile short-tap start, notification /
         *  QSTile toggle-off stop). */
        const val ACTION_START_CAMERA = "org.gameros.ansync.action.START_CAMERA"
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
                when (audioRouter.direction) {
                    WireAudioControl.Direction.DeviceToHost -> {
                        active.add("mic")
                        val stop = Intent(ctx, AnsyncCompanionService::class.java)
                            .setAction(ACTION_STOP_MIC_SHARE)
                        val pi = PendingIntent.getService(ctx, 11, stop, flags)
                        builder.addAction(android.R.drawable.ic_media_pause, "Stop mic share", pi)
                    }
                    WireAudioControl.Direction.HostToDevice -> {
                        active.add("PC audio")
                        val stop = Intent(ctx, AnsyncCompanionService::class.java)
                            .setAction(ACTION_STOP_AUDIO_SINK)
                        val pi = PendingIntent.getService(ctx, 12, stop, flags)
                        builder.addAction(android.R.drawable.ic_media_pause, "Stop PC audio", pi)
                    }
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
