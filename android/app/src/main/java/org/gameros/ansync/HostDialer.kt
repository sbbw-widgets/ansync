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
 * Keeps a live ZUDP dial to the paired host.
 *
 * Reconnect is event-driven — no retry loop:
 *  - [ConnectivityManager.NetworkCallback.onAvailable] fires when Wi-Fi / Ethernet comes up.
 *  - [livenessProbe] detects that zudp keepalives stopped (daemon restart, idle timeout, etc.)
 *    and kicks a fresh dial immediately.
 *  - [HostDiscovery] polls zudp Probe/Beacon results every 1.5 s; as soon as the daemon's
 *    beacon is seen again [tryDial] reconnects without any additional timer.
 *
 * There is deliberately no exponential back-off: that would only add latency when the
 * daemon comes back or when the network is restored — both cases are already surfaced
 * by one of the three event sources above.
 */
sealed interface HostStatus {
    data object NotPaired : HostStatus
    data object NoNetwork : HostStatus
    data class Searching(val hostName: String) : HostStatus
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
                handler.removeCallbacksAndMessages(null)
                connected = false
                publishSearchingIfPaired()
                handler.post(::dialOnce)
                handler.postDelayed(livenessProbe, LIVENESS_INTERVAL_MS)
            }
            override fun onLost(network: Network) {
                Log.i(TAG, "network lost: $network — pausing dialer")
                handler.removeCallbacksAndMessages(null)
                connected = false
                discovery?.stop()
                discovery = null
                handler.postDelayed(livenessProbe, LIVENESS_INTERVAL_MS)
                publish(HostStatus.NoNetwork)
            }
        }
        callback = cb
        cm?.registerNetworkCallback(req, cb)
        // Initial kick for the case where we're already on Wi-Fi at start-up
        // (onAvailable only fires for *transitions*).
        handler.post(::dialOnce)
        handler.postDelayed(livenessProbe, LIVENESS_INTERVAL_MS)
    }

    private val livenessProbe = object : Runnable {
        override fun run() {
            if (callback == null) return
            val native = try {
                NativeBridge.nativeIsConnected()
            } catch (t: Throwable) {
                Log.w(TAG, "nativeIsConnected unavailable; liveness probe disabled", t)
                return
            }
            if (connected && !native) {
                Log.i(TAG, "native reports session down — redialing")
                connected = false
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
        if (discovery == null) {
            val d = HostDiscovery()
            d.start { hosts -> tryDial(hosts, hex) }
            discovery = d
        }
        // Direct-dial fallback: use the addresses persisted at pair time.
        // Covers AP isolation / VPN / hotspot where Probe/Beacon doesn't reach.
        handler.postDelayed({ tryDirectFallback(hex) }, FALLBACK_DELAY_MS)
    }

    private fun tryDial(hosts: List<DiscoveredHost>, hex: String) {
        Log.i(TAG, "discovery callback: ${hosts.size} hosts; looking for ${hex.take(16)}…")
        hosts.forEachIndexed { i, h ->
            Log.i(TAG, "  [$i] ${h.name} @ ${h.address.hostAddress}:${h.port} pubkey=${h.pubkeyHex.take(16)}…")
        }
        val match = hosts.firstOrNull { it.pubkeyHex == hex }
        if (match == null) {
            Log.w(TAG, "no host matches stored pubkey — dial deferred")
            return
        }
        if (connected) return
        Log.i(TAG, "match for $hex at ${match.address.hostAddress}:${match.port}")
        dial(match.address.hostAddress ?: "", match.port, hex, "discovery")
    }

    private fun tryDirectFallback(hex: String) {
        if (connected) return
        val raw = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getString(PREF_HOST_ADDR, null)
        if (raw.isNullOrBlank()) return
        for (entry in raw.split(',')) {
            if (connected) return
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
            Log.i(TAG, "$kind dial ok ($ip:$port)")
            publish(HostStatus.Connected(storedHostName()))
            true
        } else {
            // No retry scheduled here — reconnection is driven by:
            //   • HostDiscovery polling every 1.5 s (daemon beacon reappears)
            //   • onAvailable (network transition)
            //   • livenessProbe (zudp keepalives detect dead session)
            Log.w(TAG, "$kind dial failed ($ip:$port)")
            publishSearchingIfPaired()
            false
        }
    }

    companion object {
        private const val TAG = "ansync.dial"
        private const val FALLBACK_DELAY_MS = 5_000L
        private const val LIVENESS_INTERVAL_MS = 3_000L
    }
}
