package org.gameros.ansync

/**
 * JNI surface implemented by the `ansync_companion_native` cdylib.
 *
 * The native side owns the tokio runtime and the `quinn` QUIC client
 * — Kotlin only sees a thin RPC. All calls are safe to invoke from
 * any thread; the native side serialises via an internal mutex.
 *
 * Step 7d-1 ships the surface + stubs. 7d-2 wires the real QUIC dial
 * + video / input streams.
 */
object NativeBridge {
    init {
        System.loadLibrary("ansync_companion_native")
    }

    /** Initialise the tokio runtime + android_logger. Idempotent. */
    external fun nativeInit(): Boolean

    /**
     * Dial the host at `host:port`. Returns `true` on success.
     * Step 7d-1 just records the target; 7d-2 performs the handshake.
     */
    external fun nativeOpenConnection(host: String, port: Int): Boolean

    /**
     * Push one encoded H.264 access unit over the host's `Video`
     * stream. `chunk` is one MediaCodec output buffer; `ptsUs` is the
     * presentation timestamp in microseconds.
     */
    external fun nativeSendVideoChunk(chunk: ByteArray, ptsUs: Long): Boolean

    /**
     * Block (in native) until the next reverse-input `InputMessage`
     * arrives from the host, then return the postcard-encoded bytes.
     * Returns `null` on session teardown.
     */
    external fun nativePollInputMessage(): ByteArray?

    /** Tear the active session down. Safe to call when no session is open. */
    external fun nativeClose()
}
