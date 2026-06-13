package org.gameros.ansync

/**
 * JNI surface implemented by the `ansync_companion_native` cdylib.
 *
 * The native side owns the tokio runtime + the `quinn` QUIC client.
 * Kotlin only sees a thin RPC. All calls are safe to invoke from any
 * thread; the native side serialises via an internal mutex.
 */
object NativeBridge {
    init {
        System.loadLibrary("ansync_companion_native")
    }

    /**
     * Initialise the tokio runtime, set up android_logger, and load
     * (or generate + persist) the long-term Ed25519 identity inside
     * `filesDir/identity.key`. Idempotent.
     */
    external fun nativeInit(filesDir: String): Boolean

    /**
     * Return this companion's Ed25519 public key as 64 lowercase hex
     * chars, or `null` if `nativeInit` has not run. Surfaced in the
     * pairing UI so the user can verify the fingerprint shown on the
     * host matches.
     */
    external fun nativeOurPubkeyHex(): String?

    /**
     * Dial the host at `host:port` and bring up the Video + Input
     * streams. `daemonPubkeyHex` is the 64-char hex of the daemon's
     * Ed25519 public key learned at pairing time — used for cert
     * pinning. Returns `true` on success.
     */
    external fun nativeOpenConnection(host: String, port: Int, daemonPubkeyHex: String): Boolean

    /**
     * Push one encoded H.264 access unit over the host's `Video`
     * stream. `chunk` is one MediaCodec output buffer; `ptsUs` is the
     * presentation timestamp in microseconds. Returns `false` if the
     * stream is no longer healthy — caller should tear the session
     * down.
     */
    external fun nativeSendVideoChunk(chunk: ByteArray, ptsUs: Long): Boolean

    /**
     * Block (in native) until the next reverse-input `InputMessage`
     * arrives from the host, then return the postcard-encoded bytes.
     * Returns `null` on session teardown. The AccessibilityService
     * decodes the bytes and replays them via `dispatchGesture`
     * (Step 7e).
     */
    external fun nativePollInputMessage(): ByteArray?

    /**
     * Block (in native) until the next remote `FsOpMessage` arrives
     * from the host, then return the tag-binary encoding (see
     * `FsOpCodec`). Returns `null` on session teardown.
     */
    external fun nativePollFsRequest(): ByteArray?

    /**
     * Submit the tag-binary reply for the most recent request returned
     * by `nativePollFsRequest`. Sequential per Fs stream — callers
     * MUST poll and reply in strict alternation.
     */
    external fun nativeFsReply(reply: ByteArray): Boolean

    /**
     * Drive the cable pairing flow against `127.0.0.1:port`. The host
     * has already configured an `adb reverse`. Returns
     * `"<host_pubkey_hex>|<host_name>"` on success and `null` on
     * failure. No user prompt — the cable is the security guarantee.
     */
    external fun nativePairOverCable(port: Int, companionName: String): String?

    /**
     * Push one device→host `InputMessage` (tag-binary encoded —
     * mirror of `WireInputMessage.encode()`). Lazy-opens the
     * outbound Input stream on first call.
     */
    external fun nativeSendInputMessage(blob: ByteArray): Boolean

    /**
     * Block (in native) until the next camera control message arrives
     * from the host (StartCamera / StopCamera), then return the
     * tag-binary encoding (see `WireCameraControl`). Returns `null`
     * on session teardown.
     */
    external fun nativePollCameraControl(): ByteArray?

    /**
     * Push one encoded camera frame (H.264 / H.265 access unit) over
     * the outbound Camera stream. Lazy-opens the stream on first
     * call. Returns `false` if the stream is unhealthy — caller
     * should tear the encoder down.
     */
    external fun nativeSendCameraChunk(chunk: ByteArray, ptsUs: Long): Boolean

    /** Close the outbound camera stream. Idempotent. */
    external fun nativeStopCameraStream(): Boolean

    /** Tear the active session down. Safe to call when no session is open. */
    external fun nativeClose()
}
