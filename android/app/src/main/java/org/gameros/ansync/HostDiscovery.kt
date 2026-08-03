package org.gameros.ansync

import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import java.net.InetAddress

/**
 * Companion-side device discovery using zudp Probe/Beacon frames.
 *
 * The host daemon announces via `zudp::Discovery::advertise` on port 7701.
 * The native side scans via `zudp::Discovery::scan_stream` and accumulates
 * results in a static slot. We poll that slot via [NativeBridge.nativePollDiscovery]
 * on a background thread and surface results as [DiscoveredHost] objects.
 *
 * Surfaces a `List<DiscoveredHost>` via callback so the auto-reconnect
 * worker can redial as soon as the paired host appears on the LAN.
 */
data class DiscoveredHost(
    val name: String,
    val address: InetAddress,
    val port: Int,
    val pubkeyHex: String,
)

class HostDiscovery {
    private val handlerThread = HandlerThread("ansync-discovery").also { it.start() }
    private val handler = Handler(handlerThread.looper)
    @Volatile private var running = false
    @Volatile private var onChange: ((List<DiscoveredHost>) -> Unit)? = null

    fun start(onChange: (List<DiscoveredHost>) -> Unit) {
        if (running) return
        this.onChange = onChange
        running = true
        NativeBridge.nativeStartDiscoveryScan()
        handler.post(pollRunnable)
        Log.i(TAG, "zudp discovery scan started")
    }

    fun stop() {
        running = false
        handler.removeCallbacks(pollRunnable)
        handlerThread.quitSafely()
        NativeBridge.nativeStopDiscoveryScan()
        onChange = null
        Log.i(TAG, "zudp discovery scan stopped")
    }

    private val pollRunnable = object : Runnable {
        override fun run() {
            if (!running) return
            val raw = try {
                NativeBridge.nativePollDiscovery()
            } catch (t: Throwable) {
                Log.w(TAG, "nativePollDiscovery threw", t)
                ""
            }
            val hosts = mutableListOf<DiscoveredHost>()
            for (line in raw.split('\n')) {
                if (line.isBlank()) continue
                val parts = line.split('|')
                if (parts.size < 4) continue
                val name = parts[0]
                val pubkeyHex = parts[1]
                val ip = parts[2]
                val port = parts[3].toIntOrNull() ?: continue
                val addr = runCatching { InetAddress.getByName(ip) }.getOrNull() ?: continue
                if (!isDialableAddress(addr)) continue
                hosts += DiscoveredHost(name = name, address = addr, port = port, pubkeyHex = pubkeyHex)
            }
            if (hosts.isNotEmpty()) {
                try { onChange?.invoke(hosts) } catch (t: Throwable) {
                    Log.w(TAG, "onChange threw", t)
                }
            }
            if (running) handler.postDelayed(this, POLL_INTERVAL_MS)
        }
    }

    companion object {
        private const val TAG = "ansync.discovery"
        private const val POLL_INTERVAL_MS = 1_500L

        internal fun isDialableAddress(addr: InetAddress): Boolean {
            if (addr.isLoopbackAddress || addr.isAnyLocalAddress) return false
            if (addr.isMulticastAddress) return false
            if (addr is java.net.Inet6Address && addr.isLinkLocalAddress) return false
            return true
        }
    }
}
