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
     * Stash the human-readable device name (typically
     * `"${Build.MANUFACTURER} ${Build.MODEL}"`). The native side
     * forwards it to the host inside every Hello frame so the
     * daemon's `PeerStore.name` stays in sync with what the user
     * renamed the device to in Android Settings.
     */
    external fun nativeSetDeviceName(name: String): Boolean

    /**
     * Latest host name learned from the inbound Hello frame, or
     * `null` until the first session post-handshake completes.
     * Surfaced on the paired-host card so it shows
     * `gethostname(2)` instead of a pubkey prefix.
     */
    external fun nativePollHostName(): String?

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
     * Block (in native) until the host sends a screen-capture
     * control: `RequestScreenCapture` (single byte 0x00) or
     * `StopScreenCapture` (single byte 0x01). Returns `null` on
     * session teardown.
     */
    external fun nativePollCaptureControl(): ByteArray?

    /**
     * Block until the host sends a file-access control:
     * `RequestFileAccess` (0x00) or `ReleaseFileAccess` (0x01).
     * The service decides whether to silently start the FS server
     * (URI already picked) or pop a notif asking the user to pick
     * a folder.
     */
    external fun nativePollFileControl(): ByteArray?

    /**
     * Push one encoded camera frame (H.264 / H.265 access unit) over
     * the outbound Camera stream. Lazy-opens the stream on first
     * call. Returns `false` if the stream is unhealthy — caller
     * should tear the encoder down.
     */
    external fun nativeSendCameraChunk(chunk: ByteArray, ptsUs: Long): Boolean

    /** Close the outbound camera stream. Idempotent. */
    external fun nativeStopCameraStream(): Boolean

    /**
     * Block (in native) until the next audio control message arrives
     * (StartAudioRoute / StopAudioRoute). Wire layout:
     *   tag 0 StartAudioRoute : u8 direction(0=HostToDevice,1=DeviceToHost,2=Both)
     *   tag 1 StopAudioRoute  : (no payload)
     */
    external fun nativePollAudioControl(): ByteArray?

    /**
     * Block until the next host→device PCM chunk arrives over the
     * inbound `StreamKind::Audio`. Raw 48 kHz / stereo / S16LE bytes.
     */
    external fun nativePollAudioChunk(): ByteArray?

    /**
     * Push a device→host PCM chunk (raw 48 kHz / stereo / S16LE).
     * Lazy-opens the outbound Audio stream + sends the
     * AudioStreamInit header on first call.
     */
    external fun nativeSendAudioChunk(chunk: ByteArray): Boolean

    /** Close the outbound Audio stream. Idempotent. */
    external fun nativeStopAudioStream(): Boolean

    /**
     * Block until the next inbound clipboard text arrives from the
     * host. Returns `null` on session teardown. Blob clipboard
     * payloads are dropped natively — Step 12 surfaces text only.
     */
    external fun nativePollClipboardText(): String?

    /**
     * Push the device's current clipboard text to the host. Opens a
     * one-shot `StreamKind::Clipboard` per call (the host writes the
     * content to Wayland and the stream closes). Cheap — clipboard
     * messages are tiny.
     */
    external fun nativeSendClipboardText(text: String): Boolean

    /**
     * Block (in native) until the next inbound clipboard blob arrives
     * from the host. Returns `null` on session teardown. Layout:
     *   `[mime_len u32 LE | mime utf8 | data]`.
     */
    external fun nativePollClipboardBlob(): ByteArray?

    /**
     * Push a binary clipboard payload (e.g. `image/png`) to the host.
     * Wraps in `ClipboardMessage::Blob`. The host gates on
     * `Permission::ClipboardOut` and writes to Wayland via the
     * matching MIME type.
     */
    external fun nativeSendClipboardBlob(mime: String, data: ByteArray): Boolean

    /**
     * Forward a `NotificationListenerService.onNotificationPosted`
     * event to the host. Lazy-opens the outbound
     * `StreamKind::Notifications` stream on first call.
     */
    external fun nativeSendNotificationPosted(
        id: Long,
        app: String,
        title: String,
        body: String,
    ): Boolean

    /**
     * Forward a `NotificationListenerService.onNotificationRemoved`
     * event to the host using the same `id` the post used.
     */
    external fun nativeSendNotificationRemoved(id: Long): Boolean

    /** Tear the active session down. Safe to call when no session is open. */
    external fun nativeClose()

    /**
     * `true` while the QUIC session against the host is still up.
     * Flips back to `false` the moment the native accept loop sees
     * the connection closed (daemon restart, idle timeout, network
     * drop). [HostDialer] polls this so it can redial without
     * waiting for the OS to surface a network transition.
     */
    external fun nativeIsConnected(): Boolean

    /**
     * Start the always-on WiFi pair listener. Idempotent — subsequent
     * calls return the already-bound port. Returns the listener port
     * (`> 0`) on success, or `-1` on bind failure. The companion
     * service registers an mDNS advert with this port so hosts can
     * discover the device with no user interaction.
     */
    external fun nativeWifiPairListenerStart(): Long

    /**
     * Block (up to `timeoutMs`) waiting for the next pair protocol
     * event from the always-on listener. Returns the event's wire
     * encoding or `null` on timeout. Tag prefixes (separated by `|`):
     *
     *   * `REQUEST|<host_pubkey_hex>|<host_name>|<pin>` — host has
     *     sent BootstrapHello; PIN is now safe to display.
     *   * `BAD|<remaining>|<host_name>` — PIN MAC mismatch; the
     *     listener will accept further attempts until `remaining`
     *     reaches zero.
     *   * `LOCK|<host_name>` — 3-strike lockout for the current PIN.
     *   * `OK|<host_pubkey_hex>|<host_name>` — pair complete; persist
     *     to SharedPreferences and dismiss the heads-up notif.
     */
    external fun nativePollPairEvent(timeoutMs: Long): String?

    /** Stop the always-on WiFi pair listener. Idempotent. */
    external fun nativeWifiPairListenerStop()
}
