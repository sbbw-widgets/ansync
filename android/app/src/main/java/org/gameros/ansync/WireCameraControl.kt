package org.gameros.ansync

import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Tag-binary camera-control wire mirrored from Rust
 * `control_recv_loop` in `android/src/lib.rs`. Any change requires a
 * matching diff in that file in the same commit.
 *
 * Wire (little-endian throughout):
 *   tag 0 StartCamera : u32 len | UTF-8 camera_id | u32 w | u32 h |
 *                       u8 fps | u32 bitrate_kbps |
 *                       u8 codec(0=H264,1=H265) |
 *                       u8 aspect(0=Crop,1=Letterbox,2=Stretch) |
 *                       u8 stabilization
 *   tag 1 StopCamera  : (no payload)
 */
sealed class WireCameraControl {
    data class StartCamera(
        val cameraId: String,
        val width: Int,
        val height: Int,
        val fps: Int,
        val bitrateKbps: Int,
        val codec: Codec,
        val aspect: Aspect,
        val stabilization: Boolean,
    ) : WireCameraControl()

    object StopCamera : WireCameraControl()

    enum class Codec { H264, H265 }
    enum class Aspect { Crop, Letterbox, Stretch }

    companion object {
        fun decode(bytes: ByteArray): WireCameraControl? {
            if (bytes.isEmpty()) return null
            val buf = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN)
            return when (buf.get().toInt() and 0xFF) {
                0 -> {
                    val idLen = buf.int
                    val idBytes = ByteArray(idLen)
                    buf.get(idBytes)
                    val cameraId = String(idBytes, Charsets.UTF_8)
                    val width = buf.int
                    val height = buf.int
                    val fps = buf.get().toInt() and 0xFF
                    val bitrateKbps = buf.int
                    val codec = if ((buf.get().toInt() and 0xFF) == 1) Codec.H265 else Codec.H264
                    val aspect = when (buf.get().toInt() and 0xFF) {
                        1 -> Aspect.Letterbox
                        2 -> Aspect.Stretch
                        else -> Aspect.Crop
                    }
                    val stab = (buf.get().toInt() and 0xFF) != 0
                    StartCamera(cameraId, width, height, fps, bitrateKbps, codec, aspect, stab)
                }
                1 -> StopCamera
                else -> null
            }
        }
    }
}
