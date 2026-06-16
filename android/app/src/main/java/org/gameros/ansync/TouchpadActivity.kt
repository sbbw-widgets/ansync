package org.gameros.ansync

import android.content.Context
import android.os.Bundle
import android.os.SystemClock
import android.text.InputType
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.View
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputConnectionWrapper
import android.view.inputmethod.InputMethodManager
import android.widget.EditText
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
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
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInteropFilter
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.viewinterop.AndroidView
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp

/**
 * Full-screen device→host input surface. Routes every interaction
 * over the QUIC `Input` stream the companion already keeps open per
 * peer:
 *
 *  ┌─ Touch / mouse pad ────────────────────────────────────────────┐
 *  │ • 1-finger drag         → `MouseMove { dx, dy }` (no button)    │
 *  │ • 1-finger tap          → `MouseButton { 1, down/up }`          │
 *  │ • long press            → `MouseButton { 2, down/up }`          │
 *  │ • double-tap + hold     → `MouseButton { 1, down }` + drag      │
 *  │ • 2-finger drag         → `MouseWheel { dx, dy }`               │
 *  │ • 2-finger tap          → `MouseButton { 3, down/up }`          │
 *  │ • stylus events         → `Stylus { x, y, pressure, tilt, btn }`│
 *  └────────────────────────────────────────────────────────────────┘
 *
 *  ┌─ Keyboard ──────────────────────────────────────────────────────┐
 *  │ • Hardware KeyEvent  → `KeyPress { keycode, pressed }` via the  │
 *  │   activity-level `dispatchKeyEvent` (USB / BT keyboards).       │
 *  │ • Soft IME           → an offscreen `EditText` whose            │
 *  │   `InputConnection` intercepts `commitText`,                    │
 *  │   `deleteSurroundingText` and `sendKeyEvent` 1-to-1 — no shared │
 *  │   text buffer, so IME composition / autocomplete cannot         │
 *  │   manufacture phantom deletes the way `BasicTextField`+         │
 *  │   `onValueChange` did.                                          │
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
     * Hardware keyboard events arrive here before the focused
     * [EditText] sees them — forward attached BT / USB key events
     * straight to the host and consume.
     * Gamepad-source events are forwarded to the default handler so
     * [GamepadActivity] can claim them when launched instead.
     */
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if ((event.source and android.view.InputDevice.SOURCE_GAMEPAD) ==
            android.view.InputDevice.SOURCE_GAMEPAD
        ) {
            return super.dispatchKeyEvent(event)
        }
        // Skip events that already came from the soft IME — those
        // arrive via the InputConnection path below, not here.
        if (event.deviceId == KeyEvent.KEYCODE_UNKNOWN) {
            return super.dispatchKeyEvent(event)
        }
        val evdev = KeycodeMap.toEvdev(event.keyCode) ?: return super.dispatchKeyEvent(event)
        if (event.action == KeyEvent.ACTION_DOWN || event.action == KeyEvent.ACTION_UP) {
            sendKey(evdev, event.action == KeyEvent.ACTION_DOWN)
            return true
        }
        return super.dispatchKeyEvent(event)
    }
}

private const val LONG_PRESS_MS = 450L
private const val TAP_SLOP_PX = 12f
private const val TAP_MAX_MS = 200L
private const val DOUBLE_TAP_MS = 300L
private const val STYLUS_ABS_MAX = 32767
private const val STYLUS_PRESSURE_MAX = 8191

@OptIn(ExperimentalComposeUiApi::class)
@Composable
private fun TouchpadScreen() {
    var status by remember { mutableStateOf("touchpad ready") }
    var canvasSize by remember { mutableStateOf(IntSize.Zero) }
    var imeOpen by remember { mutableStateOf(false) }
    var editTextRef by remember { mutableStateOf<HostKeyboardEditText?>(null) }

    LaunchedEffect(imeOpen, editTextRef) {
        val et = editTextRef ?: return@LaunchedEffect
        val imm = et.context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        if (imeOpen) {
            et.requestFocus()
            imm.showSoftInput(et, InputMethodManager.SHOW_IMPLICIT)
        } else {
            imm.hideSoftInputFromWindow(et.windowToken, 0)
            et.clearFocus()
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

        // Offscreen IME sink. The InputConnection wrapper installed
        // by [HostKeyboardEditText] forwards every commit / delete /
        // raw-key event straight to the host and *never* writes the
        // typed text back into the EditText buffer — so IME
        // composition state (live word previews, autocorrect, etc.)
        // cannot manufacture phantom deletes when it rewrites the
        // text under us.
        AndroidView(
            factory = { ctx ->
                HostKeyboardEditText(ctx).also { editTextRef = it }
            },
            modifier = Modifier.size(1.dp),
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
                    "double-tap + hold → left button drag\n" +
                    "2-finger drag → wheel  •  2-finger tap → middle\n" +
                    "stylus → pen events  •  Show keyboard → type to host",
                color = Color.White,
                modifier = Modifier.align(Alignment.TopStart).padding(16.dp),
                style = MaterialTheme.typography.bodyMedium,
            )
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
    var lastUpAt: Long = 0L,
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
            val now = SystemClock.uptimeMillis()
            gesture.downAt = now
            gesture.startX = event.x
            gesture.startY = event.y
            gesture.lastX = event.x
            gesture.lastY = event.y
            gesture.rightHeld = false
            gesture.twoFingerActive = false
            gesture.twoFingerMoved = false
            // Double-tap-and-hold: if the previous tap released
            // within the double-tap window, the new touch starts
            // with the left button held — user can now drag the
            // selection / window / scrollbar.
            gesture.leftHeld = (now - gesture.lastUpAt) < DOUBLE_TAP_MS
            if (gesture.leftHeld) sendButton(button = 1, pressed = true)
            if (gesture.leftHeld) "drag-mode" else "pointer-down"
        }
        MotionEvent.ACTION_POINTER_DOWN -> {
            // Second finger landed — promote the gesture to a
            // two-finger scroll/middle-click stream. A drag-mode
            // single-finger gesture in progress is cancelled (button
            // release) before flipping to scroll.
            if (gesture.leftHeld) {
                sendButton(button = 1, pressed = false)
                gesture.leftHeld = false
            }
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
                    // Long-press right-click: stationary finger
                    // past the threshold upgrades to button 2 held.
                    // Drag mode already has button 1 held from
                    // ACTION_DOWN; plain drag emits *just* MouseMove
                    // with no implicit button press.
                    if (!gesture.leftHeld && !gesture.rightHeld &&
                        elapsed > LONG_PRESS_MS && moved < TAP_SLOP_PX
                    ) {
                        sendButton(button = 2, pressed = true)
                        gesture.rightHeld = true
                    }
                    NativeBridge.nativeSendInputMessage(
                        WireInputMessage.MouseMove(dx = dx, dy = dy).encode()
                    )
                    gesture.lastX = event.x
                    gesture.lastY = event.y
                }
                if (gesture.leftHeld) "drag" else if (gesture.rightHeld) "right-drag" else "move"
            }
        }
        MotionEvent.ACTION_POINTER_UP -> "two-finger ending"
        MotionEvent.ACTION_UP -> {
            val now = SystemClock.uptimeMillis()
            val elapsed = now - gesture.downAt
            val moved = kotlin.math.hypot(
                (event.x - gesture.startX).toDouble(),
                (event.y - gesture.startY).toDouble(),
            )
            when {
                gesture.twoFingerActive -> {
                    if (!gesture.twoFingerMoved && elapsed < TAP_MAX_MS) {
                        sendButton(button = 3, pressed = true)
                        sendButton(button = 3, pressed = false)
                    }
                }
                gesture.rightHeld -> sendButton(button = 2, pressed = false)
                gesture.leftHeld -> sendButton(button = 1, pressed = false)
                elapsed < TAP_MAX_MS && moved < TAP_SLOP_PX -> {
                    sendButton(button = 1, pressed = true)
                    sendButton(button = 1, pressed = false)
                    gesture.lastUpAt = now
                }
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

// ── Soft IME sink ────────────────────────────────────────────────────

/**
 * Offscreen [EditText] whose [InputConnection] is hijacked so that
 * every IME event — committed text, surrounding-text deletes, raw
 * key events — is forwarded straight to the host as a sequence of
 * `KeyPress` events without ever mutating the local text buffer.
 *
 * The buffer-less design is deliberate: a shared `value` between
 * Compose state and the IME (the original `BasicTextField` approach)
 * gives the IME freedom to rewrite the composition (autocorrect /
 * predictive replacement) and surfaces those rewrites as misleading
 * length deltas, which the previous diff-based emitter translated
 * into spurious backspaces — at worst wiping the host's text field
 * end-to-end.
 */
internal class HostKeyboardEditText(ctx: Context) : EditText(ctx) {
    init {
        inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        setBackgroundResource(0)
        setTextColor(0)
        isFocusable = true
        isFocusableInTouchMode = true
        importantForAutofill = View.IMPORTANT_FOR_AUTOFILL_NO
    }

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        val base = super.onCreateInputConnection(outAttrs)
        return HostKeyboardInputConnection(base, true)
    }
}

private class HostKeyboardInputConnection(
    base: InputConnection,
    mutable: Boolean,
) : InputConnectionWrapper(base, mutable) {

    override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
        text?.forEach { sendCharAsKey(it) }
        return true
    }

    /**
     * IMEs use `setComposingText` for the in-progress word preview
     * (the underlined word above the keyboard while you're typing).
     * We do not surface that to the host — only the eventual
     * `finishComposingText` / `commitText` does. Returning `true`
     * with no buffer mutation makes the IME think the call landed
     * so it stops retrying.
     */
    override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean = true
    override fun finishComposingText(): Boolean = true
    override fun setComposingRegion(start: Int, end: Int): Boolean = true

    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
        repeat(beforeLength) {
            sendKey(14, true)
            sendKey(14, false)
        }
        repeat(afterLength) {
            sendKey(111, true)
            sendKey(111, false)
        }
        return true
    }

    override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int): Boolean =
        deleteSurroundingText(beforeLength, afterLength)

    override fun sendKeyEvent(event: KeyEvent): Boolean {
        if (event.action == KeyEvent.ACTION_DOWN || event.action == KeyEvent.ACTION_UP) {
            KeycodeMap.toEvdev(event.keyCode)?.let { evdev ->
                sendKey(evdev, event.action == KeyEvent.ACTION_DOWN)
                return true
            }
        }
        return super.sendKeyEvent(event)
    }

    override fun performEditorAction(editorAction: Int): Boolean {
        // IME "send" / "done" / "go" actions: synthesise an Enter.
        sendKey(28, true)
        sendKey(28, false)
        return true
    }
}

/**
 * Translate a Unicode `Char` into one or more evdev key presses.
 * Capital ASCII letters and standard shifted punctuation synthesise
 * a left-shift held around the base key. Non-ASCII glyphs are
 * dropped — the wire only carries evdev keycodes and the host uinput
 * keyboard cannot type composed text directly; use the clipboard
 * path for non-ASCII strings.
 */
private fun sendCharAsKey(c: Char) {
    val (evdev, shifted) = when (c) {
        '\n' -> 28 to false
        '\t' -> 15 to false
        ' ' -> 57 to false
        in 'a'..'z' -> KeycodeMap.toEvdev(KeyEvent.KEYCODE_A + (c - 'a'))!! to false
        in 'A'..'Z' -> KeycodeMap.toEvdev(KeyEvent.KEYCODE_A + (c - 'A'))!! to true
        in '0'..'9' -> KeycodeMap.toEvdev(KeyEvent.KEYCODE_0 + (c - '0'))!! to false
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
