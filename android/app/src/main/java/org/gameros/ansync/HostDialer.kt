package org.gameros.ansync

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.os.Handler
import android.os.HandlerThread
import android.util.Log

/**
 * Keeps a live QUIC dial to the paired host. Watches the system
 * connectivity callbacks; whenever a Wi-Fi / Ethernet network comes
 * up we (re)try [HostDiscovery] until the host's mDNS record appears
 * with a matching pubkey, then call
 * [NativeBridge.nativeOpenConnection]. Dial failures back off
 * exponentially; mDNS handles the case where the host changed IP.
 *
 * This is what makes "phone unlocks → screen mirrors automatically"
 * work without the user opening anything. The class is owned by
 * [AnsyncCompanionService]; tear-down happens in `onDestroy`.
 */
/**
 * Coarse-grained status the dialer publishes to listeners. Drives the
 * persistent notification's content line (`Connected to X` / `Looking
 * for X` / `Waiting for Wi-Fi` / `No paired host`).
 *
 * The dialer cannot tell when the QUIC session drops post-connect on
 * its own (the peer might disappear without the kernel surfacing a
 * link drop). [Connected] is therefore best-effort — it's correct when
 * we transition through one of the events the dialer sees (network
 * up, dial result), and stale otherwise. The notification copy is
 * deliberately worded so a stale [Connected] still reads naturally.
 */
sealed interface HostStatus {
    /** No pubkey in SharedPreferences — pair hasn't happened. */
    data object NotPaired : HostStatus
    /** No Wi-Fi / Ethernet up — dialer is parked. */
    data object NoNetwork : HostStatus
    /** Network is up, dialer is browsing mDNS or retrying. */
    data class Searching(val hostName: String) : HostStatus
    /** Dial succeeded; QUIC session opened against [hostName]. */
    data class Connected(val hostName: String) : HostStatus
}

class HostDialer(private val ctx: Context) {

    private val cm = ctx.getSystemService(ConnectivityManager::class.java)
    private val handlerThread = HandlerThread("ansync-dialer").also { it.start() }
    private val handler = Handler(handlerThread.looper)
    private var discovery: HostDiscovery? = null
    private var callback: ConnectivityManager.NetworkCallback? = null
    @Volatile private var listener: ((HostStatus) -> Unit)? = null
    @Volatile private var lastStatus: HostStatus = HostStatus.NotPaired

    @Volatile private var connected = false
    @Volatile private var backoffMs = INITIAL_BACKOFF_MS

    /** Register a status listener. Most recent value is delivered
     *  immediately so the caller doesn't have to wait for the next
     *  transition. Set to `null` to unregister. */
    fun setListener(cb: ((HostStatus) -> Unit)?) {
        listener = cb
        cb?.invoke(lastStatus)
    }

    fun start() {
        if (callback != null) return
        emitInitialStatus()
        val req = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
            .addTransportType(NetworkCapabilities.TRANSPORT_ETHERNET)
            .build()
        val cb = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                Log.i(TAG, "network up: $network — kicking dialer")
                connected = false
                backoffMs = INITIAL_BACKOFF_MS
                publishSearchingIfPaired()
                handler.post(::dialOnce)
            }
            override fun onLost(network: Network) {
                Log.i(TAG, "network lost: $network — pausing dialer")
                connected = false
                publish(HostStatus.NoNetwork)
            }
        }
        callback = cb
        cm?.registerNetworkCallback(req, cb)
        // Initial kick — `onAvailable` only fires for *transitions*,
        // so if we're already on Wi-Fi at start-up we'd otherwise
        // sit idle until the user toggles airplane mode.
        handler.post(::dialOnce)
        // Liveness poll: the network callbacks fire on link drops but
        // NOT on a peer dying mid-session (daemon restart, QUIC idle
        // timeout). Ask the native side every few seconds whether the
        // QUIC session is still up; flip our local `connected` cache
        // + redial when it isn't.
        handler.postDelayed(livenessProbe, LIVENESS_INTERVAL_MS)
    }

    private val livenessProbe = object : Runnable {
        override fun run() {
            if (callback == null) return
            // Wrapped: an APK that predates the new `nativeIsConnected`
            // symbol would throw `UnsatisfiedLinkError` the first time
            // we poke it and kill the foreground service. Skip the
            // probe silently instead — the network-callback path still
            // covers link drops in that build.
            val native = try {
                NativeBridge.nativeIsConnected()
            } catch (t: Throwable) {
                Log.w(TAG, "nativeIsConnected unavailable; liveness probe disabled", t)
                return
            }
            if (connected && !native) {
                Log.i(TAG, "native reports session down — redialing")
                connected = false
                backoffMs = INITIAL_BACKOFF_MS
                publishSearchingIfPaired()
                handler.post(::dialOnce)
            }
            handler.postDelayed(this, LIVENESS_INTERVAL_MS)
        }
    }

    fun stop() {
        callback?.let { cm?.unregisterNetworkCallback(it) }
        callback = null
        discovery?.stop()
        discovery = null
        handler.removeCallbacksAndMessages(null)
        handlerThread.quitSafely()
        listener = null
    }

    private fun emitInitialStatus() {
        val initial = if (storedHostHex().isNullOrBlank()) {
            HostStatus.NotPaired
        } else {
            HostStatus.Searching(storedHostName())
        }
        publish(initial)
    }

    private fun publishSearchingIfPaired() {
        val hex = storedHostHex()
        if (hex.isNullOrBlank()) {
            publish(HostStatus.NotPaired)
        } else {
            publish(HostStatus.Searching(storedHostName()))
        }
    }

    private fun publish(status: HostStatus) {
        if (status == lastStatus) return
        lastStatus = status
        try {
            listener?.invoke(status)
        } catch (e: Throwable) {
            Log.w(TAG, "status listener threw", e)
        }
    }

    private fun storedHostHex(): String? =
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getString(PairingReceiver.PREF_HOST_PUBKEY_HEX, null)

    private fun storedHostName(): String =
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getString(PairingReceiver.PREF_HOST_NAME, null)
            .orEmpty()
            .ifBlank { "ansync host" }

    private fun dialOnce() {
        if (connected) return
        val hex = storedHostHex() ?: run {
            publish(HostStatus.NotPaired)
            return
        }
        publishSearchingIfPaired()
        // Start (or re-start) discovery; the callback fires the dial
        // once the paired host's mDNS record is seen.
        if (discovery == null) {
            val d = HostDiscovery(ctx)
            d.start { hosts ->
                tryDial(hosts, hex)
            }
            discovery = d
        }
        // Schedule a direct-dial fallback against the addresses we
        // persisted at pair time. Wi-Fi AP isolation / hotspot
        // subnets drop mDNS multicast, so without this we'd sit
        // forever on `discovery started` with nothing to match.
        handler.postDelayed({ tryDirectFallback(hex) }, FALLBACK_DELAY_MS)
    }

    private fun tryDial(hosts: List<DiscoveredHost>, hex: String) {
        Log.i(
            TAG,
            "discovery callback: ${hosts.size} hosts; looking for ${hex.take(16)}…",
        )
        hosts.forEachIndexed { i, h ->
            Log.i(
                TAG,
                "  [$i] ${h.name} @ ${h.address.hostAddress}:${h.port} pubkey=${h.pubkeyHex.take(16)}…",
            )
        }
        val match = hosts.firstOrNull { it.pubkeyHex == hex }
        if (match == null) {
            Log.w(TAG, "no host matches stored pubkey — dial deferred")
            return
        }
        if (connected) return
        Log.i(TAG, "match for $hex at ${match.address.hostAddress}:${match.port}")
        dial(match.address.hostAddress ?: "", match.port, hex, "mdns")
    }

    private fun tryDirectFallback(hex: String) {
        if (connected) return
        val raw = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getString(PREF_HOST_ADDR, null)
        if (raw.isNullOrBlank()) return
        for (entry in raw.split(',')) {
            if (connected) return
            // PREF_HOST_ADDR may contain IPv6 literals (multiple colons).
            // Split on the LAST `:` so `[fe80::1]:47215` and
            // `192.168.0.1:47215` both parse correctly.
            val sep = entry.lastIndexOf(':')
            if (sep <= 0) continue
            val ip = entry.substring(0, sep).trim().removeSurrounding("[", "]")
            val port = entry.substring(sep + 1).trim().toIntOrNull() ?: 0
            if (ip.isEmpty() || port == 0) continue
            val parsed = runCatching { java.net.InetAddress.getByName(ip) }.getOrNull()
            if (parsed == null || !HostDiscovery.isDialableAddress(parsed)) {
                Log.i(TAG, "skip direct-dial entry $ip: undialable")
                continue
            }
            Log.i(TAG, "direct-dial fallback: $ip:$port")
            if (dial(ip, port, hex, "direct")) return
        }
    }

    private fun dial(ip: String, port: Int, hex: String, kind: String): Boolean {
        val ok = NativeBridge.nativeOpenConnection(ip, port, hex)
        return if (ok) {
            connected = true
            backoffMs = INITIAL_BACKOFF_MS
            Log.i(TAG, "$kind dial ok ($ip:$port)")
            publish(HostStatus.Connected(storedHostName()))
            true
        } else {
            Log.w(TAG, "$kind dial failed; will retry in ${backoffMs}ms")
            publishSearchingIfPaired()
            handler.postDelayed(::dialOnce, backoffMs)
            backoffMs = (backoffMs * 2).coerceAtMost(MAX_BACKOFF_MS)
            false
        }
    }

    companion object {
        private const val TAG = "ansync.dial"
        private const val INITIAL_BACKOFF_MS = 1_000L
        private const val MAX_BACKOFF_MS = 60_000L
        /** mDNS gets ~5 s grace; after that we try direct addresses
         *  the host shared during cable pair. */
        private const val FALLBACK_DELAY_MS = 5_000L
        /** Liveness probe cadence. Low enough that a daemon restart
         *  reconnects within a couple of seconds, high enough that
         *  it costs nothing on idle. */
        private const val LIVENESS_INTERVAL_MS = 3_000L
    }
}
