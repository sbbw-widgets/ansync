package org.gameros.ansync

import android.content.Context

/**
 * Phone-owned camera capture parameters.
 *
 * Post sender-initiates refactor (2026-07-01) the phone picks its
 * encoding parameters from [CameraSettingsActivity] (long-press on
 * [tile.CameraTile]) or from the last saved values (short tap). The
 * host never sees a wire `StartCamera` control message any more —
 * these fields flow across as the `CameraStreamInit` header on the
 * first Camera frame.
 *
 * Codec / aspect tag values are shared with
 * `nativeSendCameraStreamInit` on the Rust side; changing one
 * requires the other in the same commit.
 */
data class CameraLocalConfig(
    val cameraId: String,
    val width: Int,
    val height: Int,
    val fps: Int,
    val bitrateKbps: Int,
    val codec: Codec,
    val aspect: Aspect,
    val stabilization: Boolean,
) {
    enum class Codec(val tag: Int) { H264(0), H265(1) }
    enum class Aspect(val tag: Int) { Crop(0), Letterbox(1), Stretch(2) }

    fun persist(ctx: Context) {
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().apply {
            putString(PREF_CAMERA_ID, cameraId)
            putInt(PREF_CAMERA_WIDTH, width)
            putInt(PREF_CAMERA_HEIGHT, height)
            putInt(PREF_CAMERA_FPS, fps)
            putInt(PREF_CAMERA_BITRATE, bitrateKbps)
            putInt(PREF_CAMERA_CODEC, codec.tag)
            putInt(PREF_CAMERA_ASPECT, aspect.tag)
            putBoolean(PREF_CAMERA_STAB, stabilization)
            apply()
        }
    }

    companion object {
        val DEFAULT = CameraLocalConfig(
            cameraId = "0",
            width = 1280,
            height = 720,
            fps = 30,
            bitrateKbps = 4_000,
            codec = Codec.H264,
            aspect = Aspect.Crop,
            stabilization = false,
        )

        fun load(ctx: Context): CameraLocalConfig {
            val p = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            return CameraLocalConfig(
                cameraId = p.getString(PREF_CAMERA_ID, DEFAULT.cameraId) ?: DEFAULT.cameraId,
                width = p.getInt(PREF_CAMERA_WIDTH, DEFAULT.width),
                height = p.getInt(PREF_CAMERA_HEIGHT, DEFAULT.height),
                fps = p.getInt(PREF_CAMERA_FPS, DEFAULT.fps),
                bitrateKbps = p.getInt(PREF_CAMERA_BITRATE, DEFAULT.bitrateKbps),
                codec = Codec.entries.firstOrNull { it.tag == p.getInt(PREF_CAMERA_CODEC, DEFAULT.codec.tag) }
                    ?: DEFAULT.codec,
                aspect = Aspect.entries.firstOrNull { it.tag == p.getInt(PREF_CAMERA_ASPECT, DEFAULT.aspect.tag) }
                    ?: DEFAULT.aspect,
                stabilization = p.getBoolean(PREF_CAMERA_STAB, DEFAULT.stabilization),
            )
        }
    }
}
