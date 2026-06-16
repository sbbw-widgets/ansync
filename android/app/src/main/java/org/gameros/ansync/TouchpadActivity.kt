package org.gameros.ansync

import android.os.Bundle
import android.os.SystemClock
import android.view.KeyEvent
import android.view.MotionEvent
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.ExperimentalComposeUiApi
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInteropFilter
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * Full-screen device→host input surface. Routes every interaction
 * over the QUIC `Input` stream the companion already keeps open per
 * peer:
 *
 *  ┌─ Touch / mouse pad ────────────────────────────────────────────┐
 *  │ • 1-finger drag  → `MouseMove { dx, dy }`                       │
 *  │ • 1-finger tap   → `MouseButton { 1, press / release }`         │
 *  │ • long press     → `MouseButton { 2, press / release }`         │
 *  │ • 2-finger drag  → `MouseWheel { dx, dy }`                      │
 *  │ • 2-finger tap   → `MouseButton { 3, press / release }`         │
 *  │ • stylus events  → `Stylus { x, y, pressure, tiltX, tiltY, btn }`│
 *  │   (TOOL_TYPE_STYLUS, x/y scaled to 0..32767 host ABS range)     │
 *  └────────────────────────────────────────────────────────────────┘
 *
 *  ┌─ Keyboard ──────────────────────────────────────────────────────┐
 *  │ • Hardware KeyEvent  → `KeyPress { keycode, pressed }` via the  │
 *  │   activity-level `dispatchKeyEvent` (covers attached USB / BT   │
 *  │   keyboards out of the box).                                    │
 *  │ • Soft IME          → invisible `BasicTextField`; each char     │
 *  │   the IME commits is synthesised to a press/release sequence    │
 *  │   (with auto-shift for capital ASCII letters).                  │
 *  └────────────────────────────────────────────────────────────────┘
 */
class TouchpadActivity : ComponentActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                TouchpadScreen()
            }
        }
    }

    /**
     * Catches hardware keyboard events (attached BT / USB keyboards).
     * Gamepad-source key events are forwarded to the default handler
     * so the dedicated [GamepadActivity] can claim them when launched
     * instead — this activity is the *mouse + keyboard* surface.
     */
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if ((event.source and android.view.InputDevice.SOURCE_GAMEPAD) ==
            android.view.InputDevice.SOURCE_GAMEPAD
        ) {
            return super.dispatchKeyEvent(event)
        }
        val evdev = KeycodeMap.toEvdev(event.keyCode) ?: return super.dispatchKeyEvent(event)
        val pressed = event.action == KeyEvent.ACTION_DOWN
        if (event.action == KeyEvent.ACTION_UP || event.action == KeyEvent.ACTION_DOWN) {
            sendKey(evdev, pressed)
            return true
        }
        return super.dispatchKeyEvent(event)
    }
}

private const val LONG_PRESS_MS = 450L
private const val TAP_SLOP_PX = 12f
private const val TAP_MAX_MS = 200L
private const val STYLUS_ABS_MAX = 32767
private const val STYLUS_PRESSURE_MAX = 8191

@OptIn(ExperimentalComposeUiApi::class)
@Composable
private fun TouchpadScreen() {
    var status by remember { mutableStateOf("touchpad ready") }
    var canvasSize by remember { mutableStateOf(IntSize.Zero) }
    var imeText by remember { mutableStateOf("") }
    var imeOpen by remember { mutableStateOf(false) }
    val focusRequester = remember { FocusRequester() }
    val ime = LocalSoftwareKeyboardController.current

    LaunchedEffect(imeOpen) {
        if (imeOpen) {
            focusRequester.requestFocus()
            ime?.show()
        } else {
            ime?.hide()
        }
    }

    Column(modifier = Modifier.fillMaxSize().background(Color(0xFF101418))) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(8.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Button(onClick = { imeOpen = !imeOpen }) {
                Text(if (imeOpen) "Hide keyboard" else "Show keyboard")
            }
            Text(
                text = status,
                color = Color.White,
                modifier = Modifier.padding(start = 8.dp).align(Alignment.CenterVertically),
                style = MaterialTheme.typography.bodySmall,
            )
        }

        // Invisible-size BasicTextField — owns the focus when the IME
        // is open and is the only sink the soft keyboard's
        // `commitText` calls actually feed into. The mouse/touch box
        // below stays the visual surface.
        BasicTextField(
            value = imeText,
            onValueChange = { new ->
                onImeTextChanged(old = imeText, new = new)
                // Reset the buffer to keep memory bounded; we only
                // care about the *delta* in this tick, not the
                // accumulated string.
                imeText = if (new.length > 256) "" else new
            },
            singleLine = false,
            textStyle = TextStyle(color = Color.Transparent, fontSize = 1.sp),
            modifier = Modifier
                .size(1.dp)
                .focusRequester(focusRequester),
        )

        Box(
            modifier = Modifier
                .fillMaxSize()
                .background(Color(0xFF101418))
                .onSizeChanged { canvasSize = it }
                .pointerInteropFilter { event ->
                    val update = handlePointerEvent(event, canvasSize)
                    if (update != null) status = update
                    true
                },
        ) {
            Text(
                text = "drag → cursor  •  tap → click  •  long press → right\n" +
                    "2-finger drag → wheel  •  2-finger tap → middle\n" +
                    "stylus → pen events  •  Show keyboard → type to host",
                color = Color.White,
                modifier = Modifier.align(Alignment.TopStart).padding(16.dp),
                style = MaterialTheme.typography.bodyMedium,
            )
            Spacer(modifier = Modifier.height(8.dp))
        }
    }
}

// ── Pointer state machine ────────────────────────────────────────────

private data class Gesture(
    var downAt: Long = 0L,
    var startX: Float = 0f,
    var startY: Float = 0f,
    var lastX: Float = 0f,
    var lastY: Float = 0f,
    var rightHeld: Boolean = false,
    var leftHeld: Boolean = false,
    var twoFingerActive: Boolean = false,
    var twoFingerLastY: Float = 0f,
    var twoFingerLastX: Float = 0f,
    var twoFingerMoved: Boolean = false,
)

private val gesture = Gesture()

private fun handlePointerEvent(event: MotionEvent, canvas: IntSize): String? {
    // Stylus events take their own absolute-coord path; the
    // host-side uinput Stylus device is a separate evdev node.
    if (event.getToolType(0) == MotionEvent.TOOL_TYPE_STYLUS) {
        return handleStylus(event, canvas)
    }

    return when (event.actionMasked) {
        MotionEvent.ACTION_DOWN -> {
            gesture.downAt = SystemClock.uptimeMillis()
            gesture.startX = event.x
            gesture.startY = event.y
            gesture.lastX = event.x
            gesture.lastY = event.y
            gesture.rightHeld = false
            gesture.leftHeld = false
            gesture.twoFingerActive = false
            gesture.twoFingerMoved = false
            "pointer-down"
        }
        MotionEvent.ACTION_POINTER_DOWN -> {
            // Second finger landed — promote the gesture to a
            // two-finger scroll/middle-click stream and revoke any
            // single-finger left-click that may have already been
            // dispatched (we never tap-click before MOVE/UP).
            gesture.twoFingerActive = true
            gesture.twoFingerLastX = event.x
            gesture.twoFingerLastY = event.y
            gesture.twoFingerMoved = false
            "two-finger active"
        }
        MotionEvent.ACTION_MOVE -> {
            if (gesture.twoFingerActive) {
                val dx = (event.x - gesture.twoFingerLastX).toInt()
                val dy = (event.y - gesture.twoFingerLastY).toInt()
                if (dx != 0 || dy != 0) {
                    // Y-up = wheel-up = positive dy in evdev REL_WHEEL.
                    val wheelY = (-dy) / 8
                    val wheelX = dx / 8
                    if (wheelY != 0 || wheelX != 0) {
                        gesture.twoFingerMoved = true
                        NativeBridge.nativeSendInputMessage(
                            WireInputMessage.MouseWheel(dx = wheelX, dy = wheelY).encode()
                        )
                    }
                    gesture.twoFingerLastX = event.x
                    gesture.twoFingerLastY = event.y
                }
                "wheel"
            } else {
                val dx = (event.x - gesture.lastX).toInt()
                val dy = (event.y - gesture.lastY).toInt()
                if (dx != 0 || dy != 0) {
                    val moved = kotlin.math.hypot(
                        (event.x - gesture.startX).toDouble(),
                        (event.y - gesture.startY).toDouble(),
                    )
                    val elapsed = SystemClock.uptimeMillis() - gesture.downAt
                    if (!gesture.leftHeld && !gesture.rightHeld) {
                        if (elapsed > LONG_PRESS_MS && moved < TAP_SLOP_PX) {
                            // Stationary press past the long-press
                            // window upgrades to a right-click drag.
                            sendButton(button = 2, pressed = true)
                            gesture.rightHeld = true
                        } else if (moved > TAP_SLOP_PX) {
                            // Real drag → start holding left button.
                            sendButton(button = 1, pressed = true)
                            gesture.leftHeld = true
                        }
                    }
                    NativeBridge.nativeSendInputMessage(
                        WireInputMessage.MouseMove(dx = dx, dy = dy).encode()
                    )
                    gesture.lastX = event.x
                    gesture.lastY = event.y
                }
                "drag"
            }
        }
        MotionEvent.ACTION_POINTER_UP -> {
            // Trailing finger lifted while the leading one is still
            // down. Keep the leading state alive but flag that any
            // pending single-finger tap should be skipped on UP.
            "two-finger ending"
        }
        MotionEvent.ACTION_UP -> {
            val elapsed = SystemClock.uptimeMillis() - gesture.downAt
            val moved = kotlin.math.hypot(
                (event.x - gesture.startX).toDouble(),
                (event.y - gesture.startY).toDouble(),
            )
            if (gesture.twoFingerActive) {
                if (!gesture.twoFingerMoved && elapsed < TAP_MAX_MS) {
                    sendButton(button = 3, pressed = true)
                    sendButton(button = 3, pressed = false)
                }
            } else if (gesture.rightHeld) {
                sendButton(button = 2, pressed = false)
            } else if (gesture.leftHeld) {
                sendButton(button = 1, pressed = false)
            } else if (elapsed < TAP_MAX_MS && moved < TAP_SLOP_PX) {
                sendButton(button = 1, pressed = true)
                sendButton(button = 1, pressed = false)
            }
            gesture.leftHeld = false
            gesture.rightHeld = false
            gesture.twoFingerActive = false
            "pointer-up"
        }
        MotionEvent.ACTION_CANCEL -> {
            if (gesture.leftHeld) sendButton(button = 1, pressed = false)
            if (gesture.rightHeld) sendButton(button = 2, pressed = false)
            gesture.leftHeld = false
            gesture.rightHeld = false
            gesture.twoFingerActive = false
            "cancelled"
        }
        else -> null
    }
}

private fun handleStylus(event: MotionEvent, canvas: IntSize): String {
    val absX = if (canvas.width > 0) {
        (event.x.coerceIn(0f, canvas.width.toFloat()) * STYLUS_ABS_MAX / canvas.width).toInt()
    } else 0
    val absY = if (canvas.height > 0) {
        (event.y.coerceIn(0f, canvas.height.toFloat()) * STYLUS_ABS_MAX / canvas.height).toInt()
    } else 0
    val pressure = when (event.actionMasked) {
        MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> 0
        else -> (event.pressure.coerceIn(0f, 1f) * STYLUS_PRESSURE_MAX).toInt()
            .coerceIn(0, STYLUS_PRESSURE_MAX)
    }
    // Android exposes a single tilt magnitude (0..π/2) and an
    // orientation azimuth; project to tiltX / tiltY in degrees.
    val tilt = event.getAxisValue(MotionEvent.AXIS_TILT)
    val orient = event.orientation
    val degs = (tilt * 180.0 / Math.PI).toFloat()
    val tiltX = (degs * kotlin.math.cos(orient.toDouble())).toInt()
        .coerceIn(-90, 90).toShort()
    val tiltY = (degs * kotlin.math.sin(orient.toDouble())).toInt()
        .coerceIn(-90, 90).toShort()
    val btnState = event.buttonState
    var btn = 0
    if ((btnState and MotionEvent.BUTTON_STYLUS_PRIMARY) != 0) btn = btn or 0x1
    if ((btnState and MotionEvent.BUTTON_STYLUS_SECONDARY) != 0) btn = btn or 0x2
    NativeBridge.nativeSendInputMessage(
        WireInputMessage.Stylus(
            x = absX,
            y = absY,
            pressure = pressure,
            tiltX = tiltX,
            tiltY = tiltY,
            btn = btn.toByte(),
        ).encode()
    )
    return "stylus p=$pressure"
}

private fun sendButton(button: Int, pressed: Boolean) {
    NativeBridge.nativeSendInputMessage(
        WireInputMessage.MouseButton(button = button.toByte(), pressed = pressed).encode()
    )
}

internal fun sendKey(evdev: Int, pressed: Boolean) {
    NativeBridge.nativeSendInputMessage(
        WireInputMessage.KeyPress(keycode = evdev, pressed = pressed).encode()
    )
}

// ── Soft IME → KeyPress synthesis ────────────────────────────────────

private fun onImeTextChanged(old: String, new: String) {
    val oldLen = old.length
    val newLen = new.length
    when {
        newLen > oldLen -> {
            // Characters appended (covers both single-char commits
            // and IME "paste" / autocomplete bursts).
            val added = new.substring(oldLen)
            for (c in added) {
                sendCharAsKey(c)
            }
        }
        newLen < oldLen -> {
            // Characters deleted — emit BACKSPACE per removed char.
            val removed = oldLen - newLen
            repeat(removed) {
                sendKey(14, true)
                sendKey(14, false)
            }
        }
    }
}

/**
 * Translate a Unicode `Char` into one or more evdev key presses.
 * Capital ASCII letters and the standard shifted punctuation glyphs
 * synthesise a left-shift held around the base key. Other Unicode
 * points are silently dropped — the wire only carries evdev keycodes
 * and the host uinput keyboard cannot type composed text directly;
 * use the clipboard path for non-ASCII strings.
 */
private fun sendCharAsKey(c: Char) {
    val (evdev, shifted) = when (c) {
        '\n' -> 28 to false
        '\t' -> 15 to false
        ' ' -> 57 to false
        in 'a'..'z' -> KeycodeMap.toEvdev(android.view.KeyEvent.KEYCODE_A + (c - 'a'))!! to false
        in 'A'..'Z' -> KeycodeMap.toEvdev(android.view.KeyEvent.KEYCODE_A + (c - 'A'))!! to true
        in '0'..'9' -> KeycodeMap.toEvdev(android.view.KeyEvent.KEYCODE_0 + (c - '0'))!! to false
        '-' -> 12 to false
        '_' -> 12 to true
        '=' -> 13 to false
        '+' -> 13 to true
        '[' -> 26 to false
        '{' -> 26 to true
        ']' -> 27 to false
        '}' -> 27 to true
        '\\' -> 43 to false
        '|' -> 43 to true
        ';' -> 39 to false
        ':' -> 39 to true
        '\'' -> 40 to false
        '"' -> 40 to true
        '`' -> 41 to false
        '~' -> 41 to true
        ',' -> 51 to false
        '<' -> 51 to true
        '.' -> 52 to false
        '>' -> 52 to true
        '/' -> 53 to false
        '?' -> 53 to true
        '!' -> 2 to true
        '@' -> 3 to true
        '#' -> 4 to true
        '$' -> 5 to true
        '%' -> 6 to true
        '^' -> 7 to true
        '&' -> 8 to true
        '*' -> 9 to true
        '(' -> 10 to true
        ')' -> 11 to true
        else -> return
    }
    if (shifted) {
        sendKey(42, true)
        sendKey(evdev, true)
        sendKey(evdev, false)
        sendKey(42, false)
    } else {
        sendKey(evdev, true)
        sendKey(evdev, false)
    }
}
