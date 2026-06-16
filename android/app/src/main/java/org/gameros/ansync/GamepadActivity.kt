package org.gameros.ansync

import android.os.Bundle
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp

/**
 * Pass-through surface for a physical gamepad attached to the
 * companion device (USB-C dongle, Bluetooth controller, Razer Kishi,
 * etc.). The Activity intercepts every [KeyEvent] / [MotionEvent]
 * sourced from a joystick / gamepad and forwards it as a
 * `Gamepad { buttons, lx, ly, rx, ry, lt, rt }` packet to the host's
 * uinput `Gamepad` device.
 *
 * Layout assumptions (mirrored against
 * `ansync_input::uinput::Gamepad::GP_BTN_LIST`):
 *
 *   bit 0  Button South  (Xbox A)
 *   bit 1  Button East   (Xbox B)
 *   bit 2  Button North  (Xbox Y)
 *   bit 3  Button West   (Xbox X)
 *   bit 4  TL (L1)
 *   bit 5  TR (R1)
 *   bit 6  Select
 *   bit 7  Start
 *   bit 8  Mode
 *   bit 9  Thumb L
 *   bit 10 Thumb R
 *
 * DPAD buttons / hat axes are not surfaced by the current wire
 * protocol — the host gamepad has no slot for them — and are dropped
 * silently with a status hint. Analogue triggers (`AXIS_LTRIGGER` /
 * `AXIS_RTRIGGER`) become `lt` / `rt` in the 0..255 range.
 */
class GamepadActivity : ComponentActivity() {

    private var buttons: Int = 0
    private var lx: Short = 0
    private var ly: Short = 0
    private var rx: Short = 0
    private var ry: Short = 0
    private var lt: Byte = 0
    private var rt: Byte = 0
    private var statusState = mutableStateOf("waiting for gamepad input…")

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                GamepadScreen(status = statusState.value)
            }
        }
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (!event.isGamepadKey()) return super.dispatchKeyEvent(event)
        val bit = KeycodeMap.toGamepadButtonBit(event.keyCode)
        if (bit != null) {
            val mask = 1 shl bit
            buttons = if (event.action == KeyEvent.ACTION_DOWN) {
                buttons or mask
            } else {
                buttons and mask.inv()
            }
            flush()
            return true
        }
        // L2 / R2 keyevents (some controllers post them as buttons
        // rather than triggers) map to the analogue trigger fields.
        when (event.keyCode) {
            KeyEvent.KEYCODE_BUTTON_L2 -> {
                lt = if (event.action == KeyEvent.ACTION_DOWN) 0xFF.toByte() else 0
                flush(); return true
            }
            KeyEvent.KEYCODE_BUTTON_R2 -> {
                rt = if (event.action == KeyEvent.ACTION_DOWN) 0xFF.toByte() else 0
                flush(); return true
            }
        }
        return super.dispatchKeyEvent(event)
    }

    override fun dispatchGenericMotionEvent(event: MotionEvent): Boolean {
        if ((event.source and InputDevice.SOURCE_JOYSTICK) != InputDevice.SOURCE_JOYSTICK) {
            return super.dispatchGenericMotionEvent(event)
        }
        if (event.actionMasked != MotionEvent.ACTION_MOVE) {
            return super.dispatchGenericMotionEvent(event)
        }
        lx = axisToShort(event, MotionEvent.AXIS_X)
        ly = axisToShort(event, MotionEvent.AXIS_Y)
        rx = axisToShort(event, MotionEvent.AXIS_Z)
        ry = axisToShort(event, MotionEvent.AXIS_RZ)
        lt = triggerToByte(event, MotionEvent.AXIS_LTRIGGER, MotionEvent.AXIS_BRAKE)
        rt = triggerToByte(event, MotionEvent.AXIS_RTRIGGER, MotionEvent.AXIS_GAS)
        flush()
        return true
    }

    private fun flush() {
        val ok = NativeBridge.nativeSendInputMessage(
            WireInputMessage.Gamepad(
                buttons = buttons,
                lx = lx,
                ly = ly,
                rx = rx,
                ry = ry,
                lt = lt,
                rt = rt,
            ).encode()
        )
        statusState.value = if (ok) {
            "buttons=0x${Integer.toHexString(buttons)}  L=($lx,$ly)  R=($rx,$ry)  LT=${lt.toInt() and 0xFF}  RT=${rt.toInt() and 0xFF}"
        } else {
            "send failed (no active session?)"
        }
    }
}

private fun KeyEvent.isGamepadKey(): Boolean {
    val gp = source and InputDevice.SOURCE_GAMEPAD
    val js = source and InputDevice.SOURCE_JOYSTICK
    return gp == InputDevice.SOURCE_GAMEPAD || js == InputDevice.SOURCE_JOYSTICK
}

private fun axisToShort(event: MotionEvent, axis: Int): Short {
    val raw = event.getAxisValue(axis).coerceIn(-1f, 1f)
    return (raw * Short.MAX_VALUE.toFloat()).toInt()
        .coerceIn(Short.MIN_VALUE.toInt(), Short.MAX_VALUE.toInt())
        .toShort()
}

private fun triggerToByte(event: MotionEvent, primary: Int, fallback: Int): Byte {
    var v = event.getAxisValue(primary)
    if (v == 0f) v = event.getAxisValue(fallback)
    val clamped = (v.coerceIn(0f, 1f) * 255f).toInt().coerceIn(0, 255)
    return clamped.toByte()
}

@Composable
private fun GamepadScreen(status: String) {
    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color(0xFF101418)),
    ) {
        Text(
            text = "ansync gamepad bridge\n\n" +
                "Connect a controller and press any button.\n" +
                "Stick / trigger axes forward in real time.\n\n" +
                status,
            color = Color.White,
            modifier = Modifier.align(Alignment.Center).padding(24.dp),
            style = MaterialTheme.typography.bodyMedium,
        )
    }
}
