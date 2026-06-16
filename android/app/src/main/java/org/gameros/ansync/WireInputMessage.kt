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

    fun encode(): ByteArray {
        val buf = mutableListOf<Byte>()
        fun u8(v: Int) = buf.add(v.toByte())
        fun i32(v: Int) {
            val b = ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(v).array()
            b.forEach { buf.add(it) }
        }
        fun u32(v: Int) = i32(v)
        fun u16(v: Int) {
            val b = ByteBuffer.allocate(2).order(ByteOrder.LITTLE_ENDIAN).putShort(v.toShort()).array()
            b.forEach { buf.add(it) }
        }
        fun i16(v: Short) {
            val b = ByteBuffer.allocate(2).order(ByteOrder.LITTLE_ENDIAN).putShort(v).array()
            b.forEach { buf.add(it) }
        }
        when (this) {
            is KeyPress -> { u8(0); i32(keycode); u8(if (pressed) 1 else 0) }
            is MouseMove -> { u8(1); i32(dx); i32(dy) }
            is MouseButton -> { u8(2); u8(button.toInt() and 0xFF); u8(if (pressed) 1 else 0) }
            is MouseWheel -> { u8(3); i32(dx); i32(dy) }
            is TouchSlot -> {
                u8(4)
                u8(slot.toInt() and 0xFF)
                i32(x)
                i32(y)
                u16(pressure)
                i32(trackingId)
            }
            is Stylus -> {
                u8(5)
                i32(x)
                i32(y)
                u16(pressure)
                i16(tiltX)
                i16(tiltY)
                u8(btn.toInt() and 0xFF)
            }
            is Gamepad -> {
                u8(6)
                u32(buttons)
                i16(lx)
                i16(ly)
                i16(rx)
                i16(ry)
                u8(lt.toInt() and 0xFF)
                u8(rt.toInt() and 0xFF)
            }
        }
        return buf.toByteArray()
    }

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
