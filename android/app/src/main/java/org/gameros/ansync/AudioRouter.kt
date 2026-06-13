package org.gameros.ansync

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioRecord
import android.media.AudioTrack
import android.media.MediaRecorder
import android.util.Log
import kotlin.concurrent.thread

/**
 * Manages the bi-directional PCM bridge.
 *
 * Direction `DeviceToHost` (mic forwarding): `AudioRecord` reads
 * 48 kHz stereo S16 frames and pushes them via
 * `NativeBridge.nativeSendAudioChunk`. The host receives them on a
 * `CpalSink` and they play through the user's PipeWire default.
 *
 * Direction `HostToDevice`: a worker thread loops on
 * `nativePollAudioChunk` and writes the returned PCM to an
 * `AudioTrack` configured for music playback.
 *
 * `Both` runs both threads. Stopping unblocks each worker by closing
 * the native session — the JNI poll returns `null` and the loop
 * exits.
 *
 * Sample format is hardcoded to 48 kHz / stereo / S16LE because
 * that's what the host-side `daemon-core::handle_start_audio`
 * provisions; changing one side requires changing the other in the
 * same commit.
 */
class AudioRouter(private val direction: WireAudioControl.Direction) {
    @Volatile private var running = false
    private var captureThread: Thread? = null
    private var playbackThread: Thread? = null
    private var record: AudioRecord? = null
    private var track: AudioTrack? = null

    fun start() {
        if (running) return
        running = true
        if (direction == WireAudioControl.Direction.DeviceToHost
            || direction == WireAudioControl.Direction.Both
        ) {
            startCapture()
        }
        if (direction == WireAudioControl.Direction.HostToDevice
            || direction == WireAudioControl.Direction.Both
        ) {
            startPlayback()
        }
    }

    fun stop() {
        running = false
        try { record?.stop() } catch (e: Exception) { Log.w(TAG, "record.stop", e) }
        try { record?.release() } catch (e: Exception) { Log.w(TAG, "record.release", e) }
        try { track?.stop() } catch (e: Exception) { Log.w(TAG, "track.stop", e) }
        try { track?.release() } catch (e: Exception) { Log.w(TAG, "track.release", e) }
        captureThread?.join(TIMEOUT_JOIN_MS)
        playbackThread?.join(TIMEOUT_JOIN_MS)
        record = null
        track = null
        captureThread = null
        playbackThread = null
        NativeBridge.nativeStopAudioStream()
    }

    private fun startCapture() {
        val minBuf = AudioRecord.getMinBufferSize(
            SAMPLE_RATE,
            AudioFormat.CHANNEL_IN_STEREO,
            AudioFormat.ENCODING_PCM_16BIT,
        )
        val bufSize = (minBuf * 2).coerceAtLeast(MIN_BUFFER_BYTES)
        val r = try {
            AudioRecord(
                MediaRecorder.AudioSource.MIC,
                SAMPLE_RATE,
                AudioFormat.CHANNEL_IN_STEREO,
                AudioFormat.ENCODING_PCM_16BIT,
                bufSize,
            )
        } catch (e: SecurityException) {
            Log.e(TAG, "AudioRecord failed; missing RECORD_AUDIO permission?", e)
            return
        }
        record = r
        r.startRecording()
        captureThread = thread(name = "ansync-audio-capture") {
            val buf = ByteArray(CHUNK_BYTES)
            while (running) {
                val n = r.read(buf, 0, buf.size)
                if (n <= 0) continue
                val chunk = if (n == buf.size) buf else buf.copyOf(n)
                NativeBridge.nativeSendAudioChunk(chunk)
            }
        }
    }

    private fun startPlayback() {
        val minBuf = AudioTrack.getMinBufferSize(
            SAMPLE_RATE,
            AudioFormat.CHANNEL_OUT_STEREO,
            AudioFormat.ENCODING_PCM_16BIT,
        )
        val bufSize = (minBuf * 2).coerceAtLeast(MIN_BUFFER_BYTES)
        val attrs = AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_MEDIA)
            .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
            .build()
        val fmt = AudioFormat.Builder()
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
            .setSampleRate(SAMPLE_RATE)
            .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
            .build()
        val t = AudioTrack.Builder()
            .setAudioAttributes(attrs)
            .setAudioFormat(fmt)
            .setBufferSizeInBytes(bufSize)
            .setTransferMode(AudioTrack.MODE_STREAM)
            .build()
        track = t
        t.play()
        playbackThread = thread(name = "ansync-audio-playback") {
            while (running) {
                val chunk = NativeBridge.nativePollAudioChunk() ?: return@thread
                t.write(chunk, 0, chunk.size, AudioTrack.WRITE_BLOCKING)
            }
        }
    }

    companion object {
        private const val TAG = "ansync.audio"
        private const val SAMPLE_RATE = 48_000
        private const val MIN_BUFFER_BYTES = 16 * 1024
        private const val CHUNK_BYTES = 4 * 1024
        private const val TIMEOUT_JOIN_MS = 1_000L
    }
}

/**
 * Tag-binary audio-control wire mirrored from
 * `control_recv_loop::Message::Control(StartAudioRoute|StopAudioRoute)`
 * in `android/src/lib.rs`. Any change requires a matching diff in
 * that file in the same commit.
 *
 * Wire:
 *   tag 0 StartAudioRoute : u8 direction (0=HostToDevice, 1=DeviceToHost, 2=Both)
 *   tag 1 StopAudioRoute  : (no payload)
 */
sealed class WireAudioControl {
    data class StartAudioRoute(val direction: Direction) : WireAudioControl()
    object StopAudioRoute : WireAudioControl()

    enum class Direction { HostToDevice, DeviceToHost, Both }

    companion object {
        fun decode(bytes: ByteArray): WireAudioControl? {
            if (bytes.isEmpty()) return null
            return when (bytes[0].toInt() and 0xFF) {
                0 -> {
                    if (bytes.size < 2) return null
                    val dir = when (bytes[1].toInt() and 0xFF) {
                        0 -> Direction.HostToDevice
                        1 -> Direction.DeviceToHost
                        2 -> Direction.Both
                        else -> return null
                    }
                    StartAudioRoute(dir)
                }
                1 -> StopAudioRoute
                else -> null
            }
        }
    }
}
