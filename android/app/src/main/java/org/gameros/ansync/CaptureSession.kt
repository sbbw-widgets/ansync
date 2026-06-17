package org.gameros.ansync

import android.content.Context
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Bundle
import android.view.WindowManager
import android.media.projection.MediaProjection
import android.util.DisplayMetrics
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import kotlin.concurrent.thread

/**
 * Owns one MediaProjection-driven capture loop:
 *
 *   MediaProjection ─▶ VirtualDisplay ─▶ Surface ─▶ MediaCodec (H.264) ─▶ NativeBridge.nativeSendVideoChunk
 *
 * The encoder runs in a dedicated drain thread to keep the
 * service's main thread free for binder traffic. Stopping the
 * session releases everything in reverse order.
 *
 * Pacing + bitrate / fps come from the configured [CaptureConfig].
 * Defaults mirror what scrcpy ships: 8 Mbps target bitrate at 60 fps
 * 1080p, key-frame interval of 5 s (the host requests a key frame
 * explicitly when a new viewer attaches in Step 7d-4).
 */
class CaptureSession(
    private val context: Context,
    private val projection: MediaProjection,
    private val config: CaptureConfig,
) {
    private var encoder: MediaCodec? = null
    private var virtualDisplay: VirtualDisplay? = null
    private var inputSurface: Surface? = null
    @Volatile private var running = false
    private var drainThread: Thread? = null

    fun start() {
        if (running) return
        // Capture at the device display's native aspect ratio,
        // capped to `config.maxWidth`. This avoids the pillarbox
        // bands that `VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR` adds when
        // the encoder size doesn't match the display aspect — those
        // bands threw off the host's `scaleToDisplay` math at the
        // screen edges, so taps landed ~150 px off in corners.
        val (capW, capH, dpi) = resolveCaptureDims()
        val mimeType = MediaFormat.MIMETYPE_VIDEO_AVC
        val format = MediaFormat.createVideoFormat(mimeType, capW, capH).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, config.bitrateKbps * 1000)
            setInteger(MediaFormat.KEY_FRAME_RATE, config.fps)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, config.iFrameIntervalSec)
            // Match the host's H.264 facade: Baseline profile is what
            // every Android device encodes losslessly and is what the
            // openh264 SW fallback decodes for free.
            setInteger(MediaFormat.KEY_PROFILE, MediaCodecInfo.CodecProfileLevel.AVCProfileBaseline)
        }
        val codec = MediaCodec.createEncoderByType(mimeType).apply {
            configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            inputSurface = createInputSurface()
            start()
        }
        encoder = codec

        virtualDisplay = projection.createVirtualDisplay(
            VIRTUAL_DISPLAY_NAME,
            capW,
            capH,
            dpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            inputSurface,
            null,
            null,
        )

        running = true
        // Publish the actual capture dimensions so input replay can
        // map coordinates correctly. Reads from `lastCapture*` are
        // racey but harmless: stale read just means one off-screen
        // tap until the next replay tick refreshes.
        lastCaptureWidth = capW
        lastCaptureHeight = capH
        Log.i(TAG, "capture started at ${capW}x${capH} @ ${dpi}dpi")
        drainThread = thread(name = "ansync-encoder-drain") { drainLoop(codec) }
    }

    /**
     * Read the device's real display metrics and derive an encoder-
     * friendly size that preserves the source aspect. Caps width at
     * `config.maxWidth` so we don't try to push 2944-wide native
     * panels through the Baseline encoder at full resolution.
     * Returns `Triple(width, height, densityDpi)` — both width and
     * height are forced even so the H.264 encoder accepts them.
     */
    private fun resolveCaptureDims(): Triple<Int, Int, Int> {
        val wm = context.getSystemService(Context.WINDOW_SERVICE) as? WindowManager
        val dm = DisplayMetrics().also {
            @Suppress("DEPRECATION")
            wm?.defaultDisplay?.getRealMetrics(it)
        }
        val nativeW = dm.widthPixels.coerceAtLeast(1)
        val nativeH = dm.heightPixels.coerceAtLeast(1)
        val nativeDpi = dm.densityDpi.coerceAtLeast(120)
        val (w, h) = if (nativeW > config.maxWidth) {
            val scale = config.maxWidth.toFloat() / nativeW.toFloat()
            (config.maxWidth.toEven()) to (nativeH * scale).toInt().toEven()
        } else {
            nativeW.toEven() to nativeH.toEven()
        }
        return Triple(w, h, nativeDpi)
    }

    private fun Int.toEven(): Int = if (this % 2 == 0) this else this - 1

    /**
     * Force the encoder to emit a SYNC frame (IDR / keyframe) on its
     * next dequeue. Used after the screen wakes from off: while the
     * display was off VirtualDisplay stops driving the input Surface,
     * so the encoder produces nothing; the host's H.264 decoder is
     * then sitting on stale reference frames. Asking for a fresh IDR
     * resyncs both sides with a single self-contained frame.
     *
     * Safe to call even if the encoder is not running (just a no-op).
     */
    fun requestKeyFrame() {
        val enc = encoder ?: return
        try {
            enc.setParameters(Bundle().apply {
                putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0)
            })
            Log.i(TAG, "key frame requested")
        } catch (e: IllegalStateException) {
            Log.w(TAG, "requestKeyFrame: encoder in bad state", e)
        }
    }

    fun stop() {
        // Teardown order matters:
        //   1. `running = false` signals the drain loop to exit at the
        //      next dequeue boundary.
        //   2. Release VirtualDisplay so the encoder's input Surface
        //      stops receiving buffers. Without this `encoder.stop()`
        //      can block waiting for an in-flight frame.
        //   3. Join the drain thread BEFORE calling `encoder.stop()`.
        //      `stop()` blocks until pending output is drained, but
        //      if the drain thread is mid-`nativeSendVideoChunk` on a
        //      QUIC stream that's been closed the JNI call returns
        //      Err synchronously and the loop exits cleanly.
        //   4. Now safe to stop + release the encoder + Surface.
        // Every step is wrapped in try/catch because any one of them
        // can throw `IllegalStateException` if the caller hit stop()
        // twice (MediaProjection.Callback.onStop races with our own
        // ACTION_STOP_CAPTURE) — we want the *first* call to win and
        // the second to be a silent no-op, not a crash.
        running = false
        try {
            virtualDisplay?.release()
        } catch (e: Exception) {
            Log.w(TAG, "virtualDisplay.release threw", e)
        }
        try {
            drainThread?.join(TIMEOUT_DRAIN_JOIN_MS)
        } catch (e: InterruptedException) {
            Thread.currentThread().interrupt()
            Log.w(TAG, "drainThread.join interrupted", e)
        }
        try {
            encoder?.stop()
        } catch (e: Exception) {
            Log.w(TAG, "encoder.stop threw", e)
        }
        try {
            encoder?.release()
        } catch (e: Exception) {
            Log.w(TAG, "encoder.release threw", e)
        }
        try {
            inputSurface?.release()
        } catch (e: Exception) {
            Log.w(TAG, "inputSurface.release threw", e)
        }
        encoder = null
        virtualDisplay = null
        inputSurface = null
        drainThread = null
    }

    private fun drainLoop(codec: MediaCodec) {
        val bufferInfo = MediaCodec.BufferInfo()
        val dequeueTimeoutUs = 10_000L
        while (running) {
            val index = try {
                codec.dequeueOutputBuffer(bufferInfo, dequeueTimeoutUs)
            } catch (e: IllegalStateException) {
                Log.w(TAG, "dequeueOutputBuffer threw; exiting drain", e)
                return
            }
            when {
                index == MediaCodec.INFO_TRY_AGAIN_LATER -> { /* spin */ }
                index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    val newFormat = codec.outputFormat
                    Log.i(TAG, "encoder output format: $newFormat")
                }
                index >= 0 -> {
                    val buf: ByteBuffer = codec.getOutputBuffer(index) ?: run {
                        codec.releaseOutputBuffer(index, false)
                        return@run null
                    } ?: continue
                    buf.position(bufferInfo.offset)
                    buf.limit(bufferInfo.offset + bufferInfo.size)
                    val bytes = ByteArray(bufferInfo.size)
                    buf.get(bytes)
                    val ok = NativeBridge.nativeSendVideoChunk(bytes, bufferInfo.presentationTimeUs)
                    if (!ok) {
                        Log.w(TAG, "nativeSendVideoChunk returned false; tearing down session")
                        running = false
                    }
                    codec.releaseOutputBuffer(index, false)
                    if (bufferInfo.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) {
                        Log.i(TAG, "encoder reported EOS")
                        running = false
                    }
                }
            }
        }
    }

    companion object {
        private const val TAG = "ansync.capture"
        private const val VIRTUAL_DISPLAY_NAME = "ansync-mirror"
        private const val TIMEOUT_DRAIN_JOIN_MS = 1_000L

        /**
         * Last-published capture dimensions. Other companion components
         * (notably [AnsyncAccessibilityService]) read these to map host
         * coordinates back into the device's display pixel grid. Volatile
         * because writes happen on the capture-init thread and reads can
         * land on the AccessibilityService poller thread.
         */
        @Volatile var lastCaptureWidth: Int = 1920
            private set
        @Volatile var lastCaptureHeight: Int = 1080
            private set
    }
}

/**
 * Resolution / bitrate / fps for the capture session. Defaults
 * mirror common scrcpy presets and are conservative — the daemon
 * picks something snappier via the StartScreen control message in
 * Step 7d-4.
 */
data class CaptureConfig(
    /** Upper bound on the encoded width. Native display panels can
     *  exceed this (e.g. 2944 px on the Lenovo P12); we downscale so
     *  the Baseline encoder doesn't churn at 4K. Height is always
     *  derived from the native aspect to avoid pillarboxing. */
    val maxWidth: Int = 1920,
    val bitrateKbps: Int = 8_000,
    val fps: Int = 60,
    val iFrameIntervalSec: Int = 5,
)
