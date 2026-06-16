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
class HostDialer(private val ctx: Context) {

    private val cm = ctx.getSystemService(ConnectivityManager::class.java)
    private val handlerThread = HandlerThread("ansync-dialer").also { it.start() }
    private val handler = Handler(handlerThread.looper)
    private var discovery: HostDiscovery? = null
    private var callback: ConnectivityManager.NetworkCallback? = null

    @Volatile private var connected = false
    @Volatile private var backoffMs = INITIAL_BACKOFF_MS

    fun start() {
        if (callback != null) return
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
                handler.post(::dialOnce)
            }
            override fun onLost(network: Network) {
                Log.i(TAG, "network lost: $network — pausing dialer")
                connected = false
            }
        }
        callback = cb
        cm?.registerNetworkCallback(req, cb)
        // Initial kick — `onAvailable` only fires for *transitions*,
        // so if we're already on Wi-Fi at start-up we'd otherwise
        // sit idle until the user toggles airplane mode.
        handler.post(::dialOnce)
    }

    fun stop() {
        callback?.let { cm?.unregisterNetworkCallback(it) }
        callback = null
        discovery?.stop()
        discovery = null
        handler.removeCallbacksAndMessages(null)
        handlerThread.quitSafely()
    }

    private fun dialOnce() {
        if (connected) return
        val prefs = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val hex = prefs.getString(PairingReceiver.PREF_HOST_PUBKEY_HEX, null) ?: return
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
            val (ip, port) = entry.split(':', limit = 2).let {
                val ip = it.getOrNull(0)?.trim().orEmpty()
                val port = it.getOrNull(1)?.trim()?.toIntOrNull() ?: 0
                ip to port
            }
            if (ip.isEmpty() || port == 0) continue
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
            true
        } else {
            Log.w(TAG, "$kind dial failed; will retry in ${backoffMs}ms")
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
    }
}
