package org.gameros.ansync

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.net.wifi.WifiManager
import android.util.Log
import java.net.InetAddress

/**
 * Companion-side mDNS browse for `_ansync._udp.local.` services.
 * Mirrors the host's announcement format from `ansync_discovery`:
 *
 *   TXT records:
 *     id   = <ed25519 pubkey hex, 64 chars>
 *     name = <device name>
 *     caps = <u32 hex bitfield>
 *
 * Surfaces a `List<DiscoveredHost>` via callback so the MainActivity
 * Compose tree can recompose when entries appear / disappear.
 */
data class DiscoveredHost(
    val name: String,
    val address: InetAddress,
    val port: Int,
    val pubkeyHex: String,
)

class HostDiscovery(private val ctx: Context) {
    private val nsd: NsdManager = ctx.getSystemService(Context.NSD_SERVICE) as NsdManager
    private val wifi: WifiManager = ctx.applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
    private var listener: NsdManager.DiscoveryListener? = null
    private var multicastLock: WifiManager.MulticastLock? = null
    private val state: MutableMap<String, DiscoveredHost> = mutableMapOf()

    fun start(onChange: (List<DiscoveredHost>) -> Unit) {
        if (listener != null) return
        // mDNS multicast packets are dropped by the Wi-Fi stack
        // unless an app has an active MulticastLock. This is the
        // canonical Android pattern for any mDNS / Bonjour browser.
        multicastLock = wifi.createMulticastLock("ansync-discovery").apply {
            setReferenceCounted(false)
            acquire()
        }
        val l = object : NsdManager.DiscoveryListener {
            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                Log.w(TAG, "onStartDiscoveryFailed: $errorCode")
            }
            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {
                Log.w(TAG, "onStopDiscoveryFailed: $errorCode")
            }
            override fun onDiscoveryStarted(serviceType: String) {
                Log.i(TAG, "discovery started for $serviceType")
            }
            override fun onDiscoveryStopped(serviceType: String) {
                Log.i(TAG, "discovery stopped for $serviceType")
            }
            override fun onServiceFound(info: NsdServiceInfo) {
                nsd.resolveService(info, object : NsdManager.ResolveListener {
                    override fun onResolveFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
                        Log.w(TAG, "resolve failed for ${serviceInfo.serviceName}: $errorCode")
                    }
                    override fun onServiceResolved(resolved: NsdServiceInfo) {
                        val attrs = resolved.attributes ?: emptyMap()
                        val pubkey = attrs["id"]?.let { String(it) } ?: return
                        val name = attrs["name"]?.let { String(it) } ?: resolved.serviceName
                        val host = resolved.host ?: return
                        synchronized(state) {
                            state[pubkey] = DiscoveredHost(
                                name = name,
                                address = host,
                                port = resolved.port,
                                pubkeyHex = pubkey,
                            )
                            onChange(state.values.toList())
                        }
                    }
                })
            }
            override fun onServiceLost(info: NsdServiceInfo) {
                synchronized(state) {
                    val gone = state.entries.firstOrNull { it.value.name == info.serviceName }?.key
                    gone?.let { state.remove(it) }
                    onChange(state.values.toList())
                }
            }
        }
        nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, l)
        listener = l
    }

    fun stop() {
        listener?.let { runCatching { nsd.stopServiceDiscovery(it) } }
        listener = null
        multicastLock?.runCatching { release() }
        multicastLock = null
        synchronized(state) { state.clear() }
    }

    companion object {
        private const val TAG = "ansync.discovery"
        // The host announces under `_ansync._udp` per
        // ansync_discovery::ANNOUNCE_TYPE. NsdManager wants the
        // trailing dot.
        private const val SERVICE_TYPE = "_ansync._udp."
    }
}
