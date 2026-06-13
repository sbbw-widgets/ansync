package org.gameros.ansync

import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Kotlin mirror of the flat tag+payload binary format emitted by
 * `encode_for_kotlin` in `android/src/lib.rs`. Multi-byte fields are
 * little-endian.
 *
 * Schema lives in two places by necessity (Rust + Kotlin); changes
 * MUST land in both files in the same commit.
 */
sealed class WireInputMessage {
    data class KeyPress(val keycode: Int, val pressed: Boolean) : WireInputMessage()
    data class MouseMove(val dx: Int, val dy: Int) : WireInputMessage()
    data class MouseButton(val button: Byte, val pressed: Boolean) : WireInputMessage()
    data class MouseWheel(val dx: Int, val dy: Int) : WireInputMessage()
    data class TouchSlot(
        val slot: Byte,
        val x: Int,
        val y: Int,
        val pressure: Int,
        val trackingId: Int,
    ) : WireInputMessage()
    data class Stylus(
        val x: Int,
        val y: Int,
        val pressure: Int,
        val tiltX: Short,
        val tiltY: Short,
        val btn: Byte,
    ) : WireInputMessage()
    data class Gamepad(
        val buttons: Int,
        val lx: Short,
        val ly: Short,
        val rx: Short,
        val ry: Short,
        val lt: Byte,
        val rt: Byte,
    ) : WireInputMessage()

    companion object {
        fun decode(bytes: ByteArray): WireInputMessage {
            require(bytes.isNotEmpty()) { "empty payload" }
            val buf = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN)
            return when (val tag = buf.get().toInt() and 0xFF) {
                0 -> KeyPress(buf.int, buf.get() != 0.toByte())
                1 -> MouseMove(buf.int, buf.int)
                2 -> MouseButton(buf.get(), buf.get() != 0.toByte())
                3 -> MouseWheel(buf.int, buf.int)
                4 -> TouchSlot(
                    slot = buf.get(),
                    x = buf.int,
                    y = buf.int,
                    pressure = buf.short.toInt() and 0xFFFF,
                    trackingId = buf.int,
                )
                5 -> Stylus(
                    x = buf.int,
                    y = buf.int,
                    pressure = buf.short.toInt() and 0xFFFF,
                    tiltX = buf.short,
                    tiltY = buf.short,
                    btn = buf.get(),
                )
                6 -> Gamepad(
                    buttons = buf.int,
                    lx = buf.short,
                    ly = buf.short,
                    rx = buf.short,
                    ry = buf.short,
                    lt = buf.get(),
                    rt = buf.get(),
                )
                else -> throw IllegalArgumentException("unknown WireInputMessage tag $tag")
            }
        }
    }
}
