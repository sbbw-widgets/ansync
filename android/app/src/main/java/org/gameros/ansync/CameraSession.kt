package org.gameros.ansync

import android.content.Context
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.params.OutputConfiguration
import android.hardware.camera2.params.SessionConfiguration
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.util.Range
import android.util.Size
import android.view.Surface
import java.nio.ByteBuffer
import java.util.concurrent.Executor
import kotlin.concurrent.thread

/**
 * One Camera2 → MediaCodec pipeline per session.
 *
 *   CameraDevice ─▶ CaptureSession ─▶ MediaCodec input Surface ─▶
 *     MediaCodec encoder (H.264 / H.265) ─▶ NativeBridge.nativeSendCameraChunk
 *
 * Lifecycle is phone-driven (post sender-initiates refactor). The
 * QSTile short-tap fires `ACTION_START_CAMERA` with a
 * [CameraLocalConfig] loaded from SharedPreferences; long-press
 * opens [CameraSettingsActivity] to edit the config first.
 *
 * The host never asks — it just accepts the incoming Camera stream
 * and reads the [CameraStreamInit] header off the first frame to
 * provision its v4l2loopback sink.
 */
class CameraSession(
    private val context: Context,
    private val config: CameraLocalConfig,
) {
    private val cameraManager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
    private var encoder: MediaCodec? = null
    private var inputSurface: Surface? = null
    private var camera: CameraDevice? = null
    private var captureSession: CameraCaptureSession? = null
    private var bgThread: HandlerThread? = null
    private var bgHandler: Handler? = null
    @Volatile private var running = false
    private var drainThread: Thread? = null

    fun start() {
        if (running) return
        val mime = when (config.codec) {
            CameraLocalConfig.Codec.H264 -> MediaFormat.MIMETYPE_VIDEO_AVC
            CameraLocalConfig.Codec.H265 -> MediaFormat.MIMETYPE_VIDEO_HEVC
        }

        // Pick a sensor-supported output size at least as large as
        // the requested target. Camera2 refuses arbitrary sizes; if
        // none matches we fall back to the closest available area.
        val pickedSize = pickOutputSize(config.cameraId, config.width, config.height)
        Log.i(TAG, "camera $cameraId picked size $pickedSize for target ${config.width}x${config.height}")

        // Announce the wire format to the host BEFORE the first
        // encoded frame. If this fails we abort — the daemon can't
        // decode without the CameraStreamInit header.
        val initOk = NativeBridge.nativeSendCameraStreamInit(
            pickedSize.width,
            pickedSize.height,
            config.fps,
            config.codec.tag,
            config.aspect.tag,
        )
        if (!initOk) {
            Log.e(TAG, "nativeSendCameraStreamInit failed; aborting")
            return
        }

        val format = MediaFormat.createVideoFormat(mime, pickedSize.width, pickedSize.height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, config.bitrateKbps * 1000)
            setInteger(MediaFormat.KEY_FRAME_RATE, config.fps)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, I_FRAME_INTERVAL_SEC)
            when (config.codec) {
                CameraLocalConfig.Codec.H264 -> setInteger(
                    MediaFormat.KEY_PROFILE,
                    MediaCodecInfo.CodecProfileLevel.AVCProfileBaseline,
                )
                CameraLocalConfig.Codec.H265 -> setInteger(
                    MediaFormat.KEY_PROFILE,
                    MediaCodecInfo.CodecProfileLevel.HEVCProfileMain,
                )
            }
        }
        val codec = MediaCodec.createEncoderByType(mime).apply {
            configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            inputSurface = createInputSurface()
            start()
        }
        encoder = codec

        bgThread = HandlerThread("ansync-camera-bg").also { it.start() }
        bgHandler = Handler(bgThread!!.looper)

        try {
            cameraManager.openCamera(config.cameraId, openCallback, bgHandler)
        } catch (e: SecurityException) {
            Log.e(TAG, "camera permission missing; aborting", e)
            stop()
            return
        } catch (e: Exception) {
            Log.e(TAG, "openCamera failed", e)
            stop()
            return
        }
        running = true
        drainThread = thread(name = "ansync-camera-drain") { drainLoop(codec) }
    }

    fun stop() {
        running = false
        try { captureSession?.close() } catch (e: Exception) { Log.w(TAG, "captureSession.close", e) }
        try { camera?.close() } catch (e: Exception) { Log.w(TAG, "camera.close", e) }
        try { encoder?.stop() } catch (e: Exception) { Log.w(TAG, "encoder.stop", e) }
        try { encoder?.release() } catch (e: Exception) { Log.w(TAG, "encoder.release", e) }
        try { inputSurface?.release() } catch (e: Exception) { Log.w(TAG, "inputSurface.release", e) }
        try { bgThread?.quitSafely() } catch (e: Exception) { Log.w(TAG, "bgThread.quitSafely", e) }
        drainThread?.join(TIMEOUT_DRAIN_JOIN_MS)
        captureSession = null
        camera = null
        encoder = null
        inputSurface = null
        bgThread = null
        bgHandler = null
        drainThread = null
        NativeBridge.nativeStopCameraStream()
    }

    private val cameraId get() = config.cameraId

    private val openCallback = object : CameraDevice.StateCallback() {
        override fun onOpened(device: CameraDevice) {
            camera = device
            val surface = inputSurface ?: run {
                Log.w(TAG, "no input surface on camera open")
                return
            }
            val executor = Executor { r -> bgHandler?.post(r) }
            val outputs = listOf(OutputConfiguration(surface))
            val sc = SessionConfiguration(
                SessionConfiguration.SESSION_REGULAR,
                outputs,
                executor,
                object : CameraCaptureSession.StateCallback() {
                    override fun onConfigured(session: CameraCaptureSession) {
                        captureSession = session
                        val req = device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                            addTarget(surface)
                            set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, Range(config.fps, config.fps))
                            if (config.stabilization) {
                                set(
                                    CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE,
                                    CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE_ON,
                                )
                            }
                        }.build()
                        try {
                            session.setRepeatingRequest(req, null, bgHandler)
                        } catch (e: Exception) {
                            Log.e(TAG, "setRepeatingRequest failed", e)
                        }
                    }
                    override fun onConfigureFailed(session: CameraCaptureSession) {
                        Log.e(TAG, "captureSession.onConfigureFailed")
                        stop()
                    }
                },
            )
            try {
                device.createCaptureSession(sc)
            } catch (e: Exception) {
                Log.e(TAG, "createCaptureSession failed", e)
                stop()
            }
        }

        override fun onDisconnected(device: CameraDevice) {
            Log.w(TAG, "camera disconnected")
            stop()
        }

        override fun onError(device: CameraDevice, error: Int) {
            Log.e(TAG, "camera error $error")
            stop()
        }
    }

    private fun pickOutputSize(cameraId: String, w: Int, h: Int): Size {
        val chars = cameraManager.getCameraCharacteristics(cameraId)
        val map = chars.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
            ?: return Size(w, h)
        val candidates = map.getOutputSizes(MediaCodec::class.java) ?: return Size(w, h)
        return candidates
            .filter { it.width >= w && it.height >= h }
            .minByOrNull { (it.width * it.height) - (w * h) }
            ?: candidates.maxByOrNull { it.width * it.height }
            ?: Size(w, h)
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
                    Log.i(TAG, "camera encoder output format: ${codec.outputFormat}")
                }
                index >= 0 -> {
                    val buf: ByteBuffer? = codec.getOutputBuffer(index)
                    if (buf == null) {
                        codec.releaseOutputBuffer(index, false)
                        continue
                    }
                    buf.position(bufferInfo.offset)
                    buf.limit(bufferInfo.offset + bufferInfo.size)
                    val bytes = ByteArray(bufferInfo.size)
                    buf.get(bytes)
                    val ok = NativeBridge.nativeSendCameraChunk(bytes, bufferInfo.presentationTimeUs)
                    if (!ok) {
                        Log.w(TAG, "nativeSendCameraChunk returned false; tearing down")
                        running = false
                    }
                    codec.releaseOutputBuffer(index, false)
                    if (bufferInfo.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) {
                        Log.i(TAG, "camera encoder reported EOS")
                        running = false
                    }
                }
            }
        }
    }

    companion object {
        private const val TAG = "ansync.camera"
        private const val I_FRAME_INTERVAL_SEC = 2
        private const val TIMEOUT_DRAIN_JOIN_MS = 1_000L
    }
}
