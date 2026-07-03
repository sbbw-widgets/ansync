package org.gameros.ansync

import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.os.HandlerThread
import android.util.Log
import androidx.core.app.NotificationCompat
import kotlin.concurrent.thread

/**
 * Always-on WiFi pair listener manager.
 *
 *   1. Starts the native pair listener at service `onCreate`
 *      (idempotent) and registers an mDNS advertisement under
 *      `_ansync-pair._tcp.` so the host's `ansyncctl pair` discovers
 *      this device without user interaction.
 *   2. Dedicates a worker thread to [NativeBridge.nativePollPairEvent].
 *      Each event becomes one of:
 *        - heads-up notif `"X wants to pair — PIN 123456"` on `REQUEST`,
 *        - silent toast / updated notif on `BAD` / `LOCK`,
 *        - persistence to [PairingReceiver.PREF_HOST_PUBKEY_HEX] +
 *          notif dismissal on `OK`.
 *   3. Releases everything on [stop].
 *
 * The user only ever sees the heads-up notif with the PIN — no
 * Activity, no QSTile, no manual flag. They read the PIN, type it on
 * the host CLI, and the listener completes the bootstrap.
 */
class WifiPairManager(private val ctx: Context) {

    private var pollThread: Thread? = null
    @Volatile private var pollRunning = false
    private var nsd: NsdManager? = null
    private var nsdListener: NsdManager.RegistrationListener? = null
    private var port: Long = -1

    fun start() {
        if (pollRunning) return
        ensureChannel()
        port = NativeBridge.nativeWifiPairListenerStart()
        if (port <= 0) {
            Log.w(TAG, "native listener start returned $port; pair over WiFi disabled")
            return
        }
        Log.i(TAG, "wifi pair listener up on :$port")
        registerMdns(port.toInt())
        pollRunning = true
        pollThread = thread(name = "ansync-pair-events", start = true) {
            runEventLoop()
        }
    }

    fun stop() {
        pollRunning = false
        pollThread?.interrupt()
        pollThread = null
        unregisterMdns()
        NativeBridge.nativeWifiPairListenerStop()
        port = -1
        dismissNotif()
    }

    /**
     * Hot loop on the dedicated worker thread. Blocks (up to 30 s per
     * call) on the native event channel; each non-null event is parsed
     * and dispatched. The loop survives transient nulls (timeouts) so
     * the listener thread stays alive across idle minutes.
     */
    private fun runEventLoop() {
        while (pollRunning) {
            val event = try {
                NativeBridge.nativePollPairEvent(30_000L)
            } catch (e: Throwable) {
                Log.w(TAG, "pollPairEvent threw", e)
                null
            } ?: continue
            handleEvent(event)
        }
    }

    private fun handleEvent(event: String) {
        val parts = event.split('|')
        val tag = parts.firstOrNull() ?: return
        when (tag) {
            "REQUEST" -> {
                if (parts.size < 4) return
                val pubkeyHex = parts[1]
                val hostName = parts[2]
                val pin = parts[3]
                Log.i(TAG, "pair REQUEST from '$hostName' pubkey=${pubkeyHex.take(8)}…")
                postPinNotif(hostName, pin)
            }
            "BAD" -> {
                if (parts.size < 3) return
                val remaining = parts[1].toIntOrNull() ?: 0
                val hostName = parts[2]
                Log.w(TAG, "pair BAD from '$hostName' remaining=$remaining")
                postBadNotif(hostName, remaining)
            }
            "LOCK" -> {
                val hostName = parts.getOrNull(1) ?: "host"
                Log.w(TAG, "pair LOCKOUT for '$hostName'")
                postLockNotif(hostName)
            }
            "OK" -> {
                if (parts.size < 3) return
                val pubkeyHex = parts[1]
                val hostName = parts[2]
                persistPaired(pubkeyHex, hostName)
                Log.i(TAG, "pair OK with '$hostName' pubkey=${pubkeyHex.take(8)}…")
                postOkNotif(hostName)
                // Start the foreground service so the freshly paired
                // host can be auto-dialed by HostDialer.
                AnsyncCompanionService.startSelf(ctx)
            }
            else -> Log.w(TAG, "unknown pair event tag '$tag'")
        }
    }

    private fun persistPaired(pubkeyHex: String, hostName: String) {
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putString(PairingReceiver.PREF_HOST_PUBKEY_HEX, pubkeyHex)
            .putString(PairingReceiver.PREF_HOST_NAME, hostName)
            .remove(PREF_HOST_ADDR)
            .apply()
    }

    private fun postPinNotif(hostName: String, pin: String) {
        val n = NotificationCompat.Builder(ctx, CHANNEL_ID)
            .setContentTitle("$hostName wants to pair")
            .setContentText("Enter PIN $pin on the PC")
            .setStyle(NotificationCompat.BigTextStyle()
                .bigText("Enter this PIN on the PC running ansyncctl:\n\n$pin"))
            .setSmallIcon(R.drawable.ic_ansync)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setCategory(NotificationCompat.CATEGORY_RECOMMENDATION)
            .setAutoCancel(false)
            .setOngoing(true)
            .build()
        notifMgr().notify(NOTIFICATION_ID, n)
    }

    private fun postBadNotif(hostName: String, remaining: Int) {
        val n = NotificationCompat.Builder(ctx, CHANNEL_ID)
            .setContentTitle("Wrong PIN from $hostName")
            .setContentText("$remaining attempt(s) left before lockout")
            .setSmallIcon(R.drawable.ic_warning)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setAutoCancel(true)
            .build()
        notifMgr().notify(NOTIFICATION_ID, n)
    }

    private fun postLockNotif(hostName: String) {
        val n = NotificationCompat.Builder(ctx, CHANNEL_ID)
            .setContentTitle("Pairing locked out")
            .setContentText("$hostName entered the wrong PIN 3 times")
            .setSmallIcon(R.drawable.ic_warning)
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .setAutoCancel(true)
            .build()
        notifMgr().notify(NOTIFICATION_ID, n)
    }

    private fun postOkNotif(hostName: String) {
        val n = NotificationCompat.Builder(ctx, CHANNEL_ID)
            .setContentTitle("Paired with $hostName")
            .setContentText("Tap to dismiss")
            .setSmallIcon(R.drawable.ic_ansync)
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .setAutoCancel(true)
            .build()
        notifMgr().notify(NOTIFICATION_ID, n)
    }

    private fun dismissNotif() {
        notifMgr().cancel(NOTIFICATION_ID)
    }

    private fun notifMgr(): NotificationManager =
        ctx.getSystemService(NotificationManager::class.java)

    private fun ensureChannel() {
        val mgr = notifMgr()
        if (mgr.getNotificationChannel(CHANNEL_ID) != null) return
        val ch = NotificationChannel(
            CHANNEL_ID,
            "ansync pair requests",
            NotificationManager.IMPORTANCE_HIGH,
        ).apply {
            description = "Shown when a host on the LAN asks to pair with this device"
        }
        mgr.createNotificationChannel(ch)
    }

    private fun registerMdns(port: Int) {
        val mgr = ctx.getSystemService(Context.NSD_SERVICE) as? NsdManager ?: return
        nsd = mgr
        val info = NsdServiceInfo().apply {
            serviceName = "ansync-${Build.MODEL.replace(' ', '-')}"
            serviceType = "_ansync-pair._tcp"
            this.port = port
            val pubkeyHex = NativeBridge.nativeOurPubkeyHex().orEmpty()
            setAttribute("id", pubkeyHex)
            setAttribute("name", "${Build.MANUFACTURER} ${Build.MODEL}")
        }
        val listener = object : NsdManager.RegistrationListener {
            override fun onRegistrationFailed(s: NsdServiceInfo?, code: Int) {
                Log.w(TAG, "mDNS register failed code=$code")
            }
            override fun onUnregistrationFailed(s: NsdServiceInfo?, code: Int) {}
            override fun onServiceRegistered(s: NsdServiceInfo?) {
                Log.i(TAG, "mDNS registered: ${s?.serviceName}")
            }
            override fun onServiceUnregistered(s: NsdServiceInfo?) {}
        }
        try {
            mgr.registerService(info, NsdManager.PROTOCOL_DNS_SD, listener)
            nsdListener = listener
        } catch (e: Exception) {
            Log.w(TAG, "mDNS register threw: $e")
        }
    }

    private fun unregisterMdns() {
        val listener = nsdListener ?: return
        nsdListener = null
        try {
            nsd?.unregisterService(listener)
        } catch (e: Exception) {
            Log.w(TAG, "mDNS unregister threw: $e")
        }
    }

    companion object {
        private const val TAG = "ansync.wifipair"
        private const val CHANNEL_ID = "ansync.pair"
        private const val NOTIFICATION_ID = 4243
    }
}
